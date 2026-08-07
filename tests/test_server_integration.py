"""Integration tests over the ACTUAL registered path — build_server()'s mcp.tool()(gated(fn))
composition driven through an in-memory MCP client. Covers what unit tests (raw functions /
toy stubs) miss: annotations, extra='forbid', and the gate firing on the shipping path."""

import asyncio

import pytest
from mcp.shared.memory import create_connected_server_and_client_session as connect
from mcp.types import ElicitResult

from gdrive_mcp.server import build_server


def _elicit(action: str, approved: bool = False):
    async def cb(context, params):
        return ElicitResult(action=action, content={"approved": approved} if action == "accept" else None)

    return cb


@pytest.fixture(autouse=True)
def _isolate(tmp_path, monkeypatch):
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(tmp_path / "files"))
    monkeypatch.delenv("GDRIVE_MCP_CALLING_MODEL", raising=False)
    monkeypatch.delenv("GDRIVE_MCP_REQUIRE_VERIFICATION", raising=False)


async def _list():
    async with connect(build_server()) as session:
        return await session.list_tools()


async def _call(tool, args):
    async with connect(build_server()) as session:
        return await session.call_tool(tool, args)


def _text(result) -> str:
    return " ".join(getattr(c, "text", "") for c in result.content)


def test_registered_tools_and_annotations():
    listed = asyncio.run(_list())
    by_name = {t.name: t for t in listed.tools}
    assert len(by_name) == 29
    assert by_name["read_sheet"].annotations.readOnlyHint is True
    assert by_name["delete_rows"].annotations.destructiveHint is True
    assert by_name["append_text"].annotations.destructiveHint is False
    assert by_name["create_document"].annotations.destructiveHint is False
    assert by_name["insert_table"].annotations.destructiveHint is False  # additive, not destructive
    assert by_name["format_cells"].annotations.readOnlyHint is False
    assert "ctx" not in (by_name["read_sheet"].inputSchema.get("properties") or {})


def test_locator_edit_tools_are_registered_as_destructive():
    by_name = {t.name: t for t in asyncio.run(_list()).tools}
    for name in ("delete_text", "replace_text"):
        assert by_name[name].annotations.destructiveHint is True
        assert by_name[name].annotations.readOnlyHint is not True
        # the gate wrapper injects ctx; it must not surface in the agent-facing schema
        assert "ctx" not in (by_name[name].inputSchema.get("properties") or {})


def test_locator_params_are_exposed_on_the_insert_tools():
    by_name = {t.name: t for t in asyncio.run(_list()).tools}
    for name in ("insert_text", "insert_table"):
        props = by_name[name].inputSchema.get("properties") or {}
        assert {"after", "before", "index"} <= set(props)


def test_unknown_kwarg_rejected_on_new_tools():
    res = asyncio.run(_call("delete_text", {"item": "A" * 30, "match": "x", "bogus": 1}))
    assert res.isError
    assert "bogus" in _text(res) or "Extra" in _text(res) or "unexpected" in _text(res)


def test_unknown_kwarg_rejected_on_registered_path():
    res = asyncio.run(_call("resolve_link", {"item": "A" * 30, "bogus": 1}))
    assert res.isError
    assert "bogus" in _text(res) or "Extra" in _text(res) or "unexpected" in _text(res)


def test_gate_fires_fail_closed_on_registered_path(monkeypatch):
    monkeypatch.setenv("GDRIVE_MCP_REQUIRE_VERIFICATION", "always")
    res = asyncio.run(_call("resolve_link", {"item": "A" * 30}))
    assert res.isError
    assert "verification" in _text(res)


async def _call_cb(tool, args, cb):
    async with connect(build_server(), elicitation_callback=cb) as session:
        return await session.call_tool(tool, args)


def test_gate_decline_blocks_on_registered_path(monkeypatch):
    monkeypatch.setenv("GDRIVE_MCP_REQUIRE_VERIFICATION", "always")
    res = asyncio.run(_call_cb("resolve_link", {"item": "A" * 30}, _elicit("decline")))
    assert res.isError and "not approved" in _text(res)


def test_gate_approve_proceeds_on_registered_path(monkeypatch):
    # After approval the gate passes and the tool actually runs — so any error is a downstream
    # Google/auth error, NOT a verification block. That proves approve->proceed on the real path.
    monkeypatch.setenv("GDRIVE_MCP_REQUIRE_VERIFICATION", "always")
    res = asyncio.run(_call_cb("resolve_link", {"item": "A" * 30}, _elicit("accept", approved=True)))
    text = _text(res)
    assert "verification" not in text and "not approved" not in text
