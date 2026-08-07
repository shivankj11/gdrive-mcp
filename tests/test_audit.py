import json

import pytest

from gdrive_mcp import audit

_SAFE = {"item", "replace_id", "parent", "start_row", "count", "to", "value_input"}
_SYNTHETIC_SPREADSHEET_ID = "TEST_SPREADSHEET_ID_0000001"


@pytest.fixture
def audit_log(tmp_path, monkeypatch):
    p = tmp_path / "audit.log"
    monkeypatch.setenv("GDRIVE_MCP_AUDIT_LOG", str(p))
    return p


def test_records_only_safe_keys_and_no_content(audit_log):
    audit.record(
        "write_sheet",
        {
            "item": f"https://docs.google.com/spreadsheets/d/{_SYNTHETIC_SPREADSHEET_ID}/edit",
            "tab": "Confidential Title",          # free text (potentially sensitive) — must NOT be logged
            "rows": [["secret-cell-A", "secret-cell-B"]],  # content — must NOT be logged
            "value_input": "RAW",
        },
        "ok",
        user="u@example.com",
    )
    entry = json.loads(audit_log.read_text().strip())
    assert entry["tool"] == "write_sheet" and entry["outcome"] == "ok" and entry["user"] == "u@example.com"
    assert set(entry["args"]) <= _SAFE
    assert entry["args"]["item"] == _SYNTHETIC_SPREADSHEET_ID  # ref reduced to opaque id
    assert "tab" not in entry["args"] and "rows" not in entry["args"]
    blob = audit_log.read_text()
    assert "Confidential Title" not in blob and "secret-cell-A" not in blob


def test_locator_args_are_never_logged(audit_log):
    # Locators and replacement text are document content by definition, so they are sensitive.
    # _SAFE_ARG_KEYS is an allowlist, which excludes them by default — pinned here so widening
    # the allowlist can't quietly start writing document text into the audit log.
    audit.record(
        "replace_text",
        {
            "item": "B" * 30,
            "match": "secret-match-text",
            "section": "Confidential Heading",
            "replacement": "secret-replacement-text",
            "after": "secret-anchor-text",
        },
        "ok",
    )
    entry = json.loads(audit_log.read_text().strip())
    assert set(entry["args"]) <= _SAFE
    blob = audit_log.read_text()
    for secret in (
        "secret-match-text",
        "Confidential Heading",
        "secret-replacement-text",
        "secret-anchor-text",
    ):
        assert secret not in blob


def test_unparseable_ref_is_masked(audit_log):
    audit.record("read_document", {"item": "free text not a ref"}, "ok")
    assert json.loads(audit_log.read_text().strip())["args"]["item"] == "<unparseable>"


def test_error_outcome_and_scalars(audit_log):
    audit.record("delete_rows", {"item": "A" * 30, "count": 3}, "error:ValueError")
    entry = json.loads(audit_log.read_text().strip())
    assert entry["outcome"] == "error:ValueError" and entry["args"]["count"] == 3


def test_never_raises_on_unwritable_path(tmp_path, monkeypatch):
    afile = tmp_path / "afile"
    afile.write_text("x")  # parent-is-a-file -> mkdir/open fail, must be swallowed
    monkeypatch.setenv("GDRIVE_MCP_AUDIT_LOG", str(afile / "sub" / "audit.log"))
    audit.record("read_sheet", {"item": "A" * 30}, "ok")  # must not raise
