import asyncio
import inspect
import types

import pytest

from gdrive_mcp.gating import (
    _model_requires_verification,
    calling_model,
    gate_required,
    gated,
    require_verification,
)


def _fake_ctx(action: str, approved: bool, meta_model: str | None = None):
    async def elicit(message, schema):
        return types.SimpleNamespace(action=action, data=types.SimpleNamespace(approved=approved))

    rc = types.SimpleNamespace(meta=types.SimpleNamespace(model=meta_model)) if meta_model else None
    return types.SimpleNamespace(request_context=rc, elicit=elicit)


def _ctx_meta(model: str | None):
    rc = types.SimpleNamespace(meta=types.SimpleNamespace(model=model)) if model else None
    return types.SimpleNamespace(request_context=rc)


@pytest.fixture(autouse=True)
def _clear_gate_env(monkeypatch):
    monkeypatch.delenv("GDRIVE_MCP_CALLING_MODEL", raising=False)
    monkeypatch.delenv("GDRIVE_MCP_REQUIRE_VERIFICATION", raising=False)
    monkeypatch.delenv("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", raising=False)


def _configure_model_gate(monkeypatch):
    monkeypatch.setenv(
        "GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS",
        "verification-required, restricted-family",
    )


def test_model_requires_verification_uses_configured_patterns(monkeypatch):
    _configure_model_gate(monkeypatch)
    assert _model_requires_verification("vendor-verification-required-v2")
    assert _model_requires_verification("RESTRICTED_FAMILY_LATEST")
    assert not _model_requires_verification("standard-model")
    assert not _model_requires_verification(None)


def test_calling_model_prefers_meta_then_env(monkeypatch):
    monkeypatch.setenv("GDRIVE_MCP_CALLING_MODEL", "operator-pinned-model")
    ctx = types.SimpleNamespace(
        request_context=types.SimpleNamespace(meta=types.SimpleNamespace(model="request-model"))
    )
    assert calling_model(ctx) == "request-model"  # _meta wins
    assert calling_model(None) == "operator-pinned-model"  # falls back to env


def test_no_gate_without_configured_patterns():
    asyncio.run(require_verification(None, "read_document", {}))  # must not raise


def test_configured_model_accept_passes(monkeypatch):
    _configure_model_gate(monkeypatch)
    monkeypatch.setenv("GDRIVE_MCP_CALLING_MODEL", "verification-required-model")
    asyncio.run(require_verification(_fake_ctx("accept", True), "read_document", {"item": "x"}))


def test_configured_model_decline_blocks(monkeypatch):
    _configure_model_gate(monkeypatch)
    monkeypatch.setenv("GDRIVE_MCP_CALLING_MODEL", "verification-required-model")
    with pytest.raises(RuntimeError):
        asyncio.run(require_verification(_fake_ctx("decline", False), "read_document", {"item": "x"}))


def test_configured_model_accept_but_not_approved_blocks(monkeypatch):
    _configure_model_gate(monkeypatch)
    monkeypatch.setenv("GDRIVE_MCP_CALLING_MODEL", "verification-required-model")
    with pytest.raises(RuntimeError):
        asyncio.run(require_verification(_fake_ctx("accept", False), "read_document", {"item": "x"}))


def test_configured_model_no_context_fails_closed(monkeypatch):
    _configure_model_gate(monkeypatch)
    monkeypatch.setenv("GDRIVE_MCP_CALLING_MODEL", "verification-required-model")
    with pytest.raises(RuntimeError):
        asyncio.run(require_verification(None, "read_document", {}))


def test_gated_injects_ctx_and_runs_when_ungated(monkeypatch):
    monkeypatch.delenv("GDRIVE_MCP_CALLING_MODEL", raising=False)
    seen = []

    def tool(item: str) -> dict:
        seen.append(item)
        return {"ok": item}

    wrapped = gated(tool)
    sig = inspect.signature(wrapped)
    assert "ctx" in sig.parameters and "item" in sig.parameters  # ctx injected for FastMCP
    out = asyncio.run(wrapped(item="x", ctx=None))
    assert out == {"ok": "x"} and seen == ["x"]


def test_gated_blocks_configured_model_without_ctx(monkeypatch):
    _configure_model_gate(monkeypatch)
    monkeypatch.setenv("GDRIVE_MCP_CALLING_MODEL", "verification-required-model")

    def tool(item: str) -> dict:
        return {"ok": item}

    with pytest.raises(RuntimeError):
        asyncio.run(gated(tool)(item="x", ctx=None))


def test_meta_cannot_downgrade_env_pin(monkeypatch):
    # A nonmatching request model cannot turn off a match from the operator-controlled pin.
    _configure_model_gate(monkeypatch)
    monkeypatch.setenv("GDRIVE_MCP_CALLING_MODEL", "verification-required-model")
    assert gate_required(_ctx_meta("standard-model")) is True
    # cannot slip through without approval
    with pytest.raises(RuntimeError):
        asyncio.run(require_verification(None, "read_document", {}))
    # Proceeds only with approval, despite the nonmatching request model.
    asyncio.run(
        require_verification(
            _fake_ctx("accept", True, meta_model="standard-model"),
            "read_document",
            {"item": "x"},
        )
    )


def test_matching_meta_model_triggers_without_pin(monkeypatch):
    _configure_model_gate(monkeypatch)
    assert gate_required(_ctx_meta("restricted-family-latest")) is True


def test_always_override_gates_any_model(monkeypatch):
    monkeypatch.setenv("GDRIVE_MCP_REQUIRE_VERIFICATION", "always")
    assert gate_required(_ctx_meta("standard-model")) is True
    with pytest.raises(RuntimeError):
        asyncio.run(
            require_verification(
                _fake_ctx("decline", False, meta_model="standard-model"),
                "read_sheet",
                {"item": "x"},
            )
        )


def test_no_gate_when_model_does_not_match(monkeypatch):
    _configure_model_gate(monkeypatch)
    assert gate_required(_ctx_meta("standard-model")) is False
    assert gate_required(None) is False
