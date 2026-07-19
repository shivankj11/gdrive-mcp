"""Tool-logic tests with a stubbed Google service.

These verify the invariants this codebase owns — the confirm-before-destructive gate
(no mutation without confirm) and the exact API requests we build — without any live
Google API. Live end-to-end coverage is a separate, credentialed run.
"""

from unittest.mock import MagicMock

import pytest

from gdrive_mcp.tools import discovery as disc
from gdrive_mcp.tools import docs as docs_mod
from gdrive_mcp.tools import files as files_mod
from gdrive_mcp.tools import sheets as sheets_mod
from gdrive_mcp.tools.docs import (
    _content_to_markdown,
    _flatten_tabs,
    _image_uris,
    _resolve_write_tab,
)

SID = "A" * 30  # bare IDs must be >= 20 chars to parse


def _sheets_svc(existing_values=None, titles=None):
    svc = MagicMock()
    values = svc.spreadsheets.return_value.values.return_value
    values.get.return_value.execute.return_value = {"range": "Data!A1", "values": existing_values or []}
    values.update.return_value.execute.return_value = {"updatedRange": "Data!A1:B2", "updatedCells": 4}
    values.append.return_value.execute.return_value = {"updates": {"updatedRange": "Data!A5", "updatedRows": 1}}
    svc.spreadsheets.return_value.get.return_value.execute.return_value = {
        "sheets": [{"properties": {"sheetId": sid, "title": t}} for t, sid in (titles or {}).items()]
    }
    return svc


# ---- confirm-before-destructive gate -------------------------------------------------

def test_write_sheet_gates_on_overwrite(monkeypatch):
    svc = _sheets_svc(existing_values=[["a", "b"]])  # target non-empty
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.write_sheet(SID, "Data", [["x", "y"]], confirm=False)
    assert out["status"] == "confirmation_required"
    assert out["impact"]["overwrites_nonempty_cells"] == 2
    assert not svc.spreadsheets.return_value.values.return_value.update.called


def test_write_sheet_executes_with_confirm(monkeypatch):
    svc = _sheets_svc(existing_values=[["a", "b"]])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.write_sheet(SID, "Data", [["x", "y"]], confirm=True)
    assert out["updated_cells"] == 4
    kwargs = svc.spreadsheets.return_value.values.return_value.update.call_args.kwargs
    assert kwargs["range"] == "'Data'!A1:B1"  # 1 row x 2 cols
    assert kwargs["valueInputOption"] == "RAW"  # default is RAW (no formula injection)
    assert kwargs["body"] == {"values": [["x", "y"]]}


def test_write_sheet_user_entered_opt_in(monkeypatch):
    svc = _sheets_svc(existing_values=[])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    sheets_mod.write_sheet(SID, "Data", [["=A1*2"]], value_input="USER_ENTERED")
    kwargs = svc.spreadsheets.return_value.values.return_value.update.call_args.kwargs
    assert kwargs["valueInputOption"] == "USER_ENTERED"


def test_write_sheet_no_gate_when_target_empty(monkeypatch):
    svc = _sheets_svc(existing_values=[])  # empty target -> not destructive
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.write_sheet(SID, "Data", [["x"]], confirm=False)
    assert out.get("updated_cells") == 4
    assert svc.spreadsheets.return_value.values.return_value.update.called


def test_clear_range_gate(monkeypatch):
    svc = _sheets_svc(existing_values=[["x", "y"]])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.clear_range(SID, "Data!A1:B2", confirm=False)
    assert out["status"] == "confirmation_required"
    assert out["impact"]["nonempty_cells_cleared"] == 2
    assert not svc.spreadsheets.return_value.values.return_value.clear.called
    sheets_mod.clear_range(SID, "Data!A1:B2", confirm=True)
    assert svc.spreadsheets.return_value.values.return_value.clear.called


def test_delete_rows_gate_and_range(monkeypatch):
    svc = _sheets_svc(existing_values=[["r"]], titles={"Data": 123})
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.delete_rows(SID, "Data", start_row=3, count=2, confirm=False)
    assert out["status"] == "confirmation_required"
    assert not svc.spreadsheets.return_value.batchUpdate.called
    sheets_mod.delete_rows(SID, "Data", start_row=3, count=2, confirm=True)
    body = svc.spreadsheets.return_value.batchUpdate.call_args.kwargs["body"]
    rng = body["requests"][0]["deleteDimension"]["range"]
    assert rng == {"sheetId": 123, "dimension": "ROWS", "startIndex": 2, "endIndex": 4}


def test_append_rows_is_not_gated(monkeypatch):
    svc = _sheets_svc()
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.append_rows(SID, "Data", [["z"]])
    assert out["appended_rows"] == 1
    kwargs = svc.spreadsheets.return_value.values.return_value.append.call_args.kwargs
    assert kwargs["insertDataOption"] == "INSERT_ROWS"
    assert kwargs["valueInputOption"] == "RAW"


def test_move_file_gate_and_parents(monkeypatch):
    svc = MagicMock()
    svc.files.return_value.get.return_value.execute.return_value = {"id": "F", "name": "n", "parents": ["OLD"]}
    svc.files.return_value.update.return_value.execute.return_value = {"id": "F", "name": "n", "parents": ["NEW"]}
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    dest = "B" * 30
    out = files_mod.move_file("A" * 30, dest, confirm=False)
    assert out["status"] == "confirmation_required"
    assert not svc.files.return_value.update.called
    files_mod.move_file("A" * 30, dest, confirm=True)
    kwargs = svc.files.return_value.update.call_args.kwargs
    assert kwargs["addParents"] == dest
    assert kwargs["removeParents"] == "OLD"


def test_rename_file_gate(monkeypatch):
    svc = MagicMock()
    svc.files.return_value.get.return_value.execute.return_value = {"id": "F", "name": "old"}
    svc.files.return_value.update.return_value.execute.return_value = {"id": "F", "name": "new"}
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    out = files_mod.rename_file("A" * 30, "new", confirm=False)
    assert out["status"] == "confirmation_required"
    assert not svc.files.return_value.update.called
    files_mod.rename_file("A" * 30, "new", confirm=True)
    assert svc.files.return_value.update.called


def test_upload_replace_gate(monkeypatch):
    svc = MagicMock()
    svc.files.return_value.get.return_value.execute.return_value = {
        "id": "R", "name": "old", "mimeType": "text/plain", "modifiedTime": "t"
    }
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    out = files_mod.upload_file("newname", content="hi", replace_id="C" * 30, confirm=False)
    assert out["status"] == "confirmation_required"
    assert not svc.files.return_value.update.called


# ---- read-side structuring & query building ------------------------------------------

def test_read_full_sheet_saves_csv_and_previews(monkeypatch, tmp_path):
    full = [["h1", "h2"]] + [[str(i), str(i * 2)] for i in range(1, 9)]  # 9 rows x 2 cols
    svc = _sheets_svc(existing_values=full, titles={"Data": 1})
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(tmp_path))
    out = sheets_mod.read_full_sheet(SID, tab="Data", dest_path="out.csv")
    assert out["total_rows"] == 9 and out["total_cols"] == 2
    assert out["preview"]["records"][0] == {"h1": "1", "h2": "2"}
    assert len(out["preview"]["records"]) <= 4  # 5-row window incl header -> <=4 data rows
    assert (tmp_path / "out.csv").read_text().startswith("h1,h2")


def test_is_google_host():
    from gdrive_mcp.tools.docs import _is_google_host

    assert _is_google_host("https://lh3.googleusercontent.com/AbC")
    assert _is_google_host("https://docs.google.com/x")
    assert not _is_google_host("https://evil.com/x")
    assert not _is_google_host("https://googleusercontent.com.evil.com/x")  # suffix-spoof


def test_read_sheet_builds_records(monkeypatch):
    svc = _sheets_svc(existing_values=[["h1", "h2"], ["1", "2"], ["3"]])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    tab = sheets_mod.read_sheet(SID, tab="Data")["tabs"][0]
    assert tab["headers"] == ["h1", "h2"]
    assert tab["records"][0] == {"h1": "1", "h2": "2"}
    assert tab["records"][1] == {"h1": "3", "h2": None}  # short row padded


def test_search_files_surfaces_pagination_signals(monkeypatch):
    svc = MagicMock()
    svc.files.return_value.list.return_value.execute.return_value = {
        "files": [], "nextPageToken": "TOK", "incompleteSearch": True,
    }
    monkeypatch.setattr(disc, "drive", lambda: svc)
    out = disc.search_files(name_contains="x", page_token="prev")
    assert out["has_more"] is True and out["next_page_token"] == "TOK" and out["incomplete_search"] is True
    kwargs = svc.files.return_value.list.call_args.kwargs
    assert kwargs["pageToken"] == "prev" and "nextPageToken" in kwargs["fields"]


def test_list_folder_no_more_pages(monkeypatch):
    svc = MagicMock()
    svc.files.return_value.list.return_value.execute.return_value = {"files": []}  # no token
    monkeypatch.setattr(disc, "drive", lambda: svc)
    out = disc.list_folder("A" * 30)
    assert out["has_more"] is False and out["next_page_token"] is None


def test_search_query_and_escaping(monkeypatch):
    svc = MagicMock()
    svc.files.return_value.list.return_value.execute.return_value = {"files": []}
    monkeypatch.setattr(disc, "drive", lambda: svc)
    disc.search_files(name_contains="O'Brien", mime_type="application/pdf")
    q = svc.files.return_value.list.call_args.kwargs["q"]
    assert "trashed = false" in q
    assert "name contains 'O\\'Brien'" in q
    assert "mimeType = 'application/pdf'" in q


def test_content_to_markdown_headings_and_table():
    content = [
        {"paragraph": {"paragraphStyle": {"namedStyleType": "HEADING_1"},
                       "elements": [{"textRun": {"content": "Title\n"}}]}},
        {"paragraph": {"elements": [{"textRun": {"content": "body text\n"}}]}},
        {"table": {"tableRows": [
            {"tableCells": [
                {"content": [{"paragraph": {"elements": [{"textRun": {"content": "a"}}]}}]},
                {"content": [{"paragraph": {"elements": [{"textRun": {"content": "b"}}]}}]},
            ]}
        ]}},
    ]
    md, outline = _content_to_markdown(content)
    assert "# Title" in md
    assert "body text" in md
    assert "| a | b |" in md
    assert outline == [{"level": 1, "text": "Title"}]


def test_flatten_tabs_depth_first_with_children():
    tabs = [
        {"tabProperties": {"tabId": "t.1", "title": "Parent"},
         "documentTab": {"body": {"content": ["p-body"]}},
         "childTabs": [
             {"tabProperties": {"tabId": "t.1a", "title": "Child"},
              "documentTab": {"body": {"content": ["c-body"]}}},
         ]},
        {"tabProperties": {"tabId": "t.2", "title": "Second"},
         "documentTab": {"body": {"content": ["s-body"]}}},
    ]
    flat = _flatten_tabs(tabs)
    assert [(tid, title) for tid, title, *_ in flat] == [
        ("t.1", "Parent"), ("t.1a", "Child"), ("t.2", "Second")
    ]


def test_image_uris_in_document_order():
    content = [
        {"paragraph": {"elements": [
            {"inlineObjectElement": {"inlineObjectId": "io1"}},
            {"textRun": {"content": "between"}},
            {"inlineObjectElement": {"inlineObjectId": "io2"}},
        ]}},
    ]
    inline = {
        "io1": {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "u1"}}}},
        "io2": {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "u2"}}}},
    }
    assert _image_uris(content, inline, {}) == ["u1", "u2"]


def test_image_uris_includes_positioned_objects():
    content = [
        {"paragraph": {
            "positionedObjectIds": ["po1"],
            "elements": [{"inlineObjectElement": {"inlineObjectId": "io1"}}],
        }},
        {"paragraph": {"positionedObjectIds": ["po2", "po-imageless"], "elements": []}},
    ]
    inline = {
        "io1": {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "iu1"}}}},
    }
    positioned = {
        "po1": {"positionedObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "pu1"}}}},
        "po2": {"positionedObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "pu2"}}}},
        "po-imageless": {"positionedObjectProperties": {"embeddedObject": {}}},  # e.g. a drawing
    }
    # anchored (positioned) images come before the paragraph's inline images
    assert _image_uris(content, inline, positioned) == ["pu1", "iu1", "pu2"]


def test_flatten_tabs_carries_positioned_objects():
    tabs = [{
        "tabProperties": {"tabId": "t.1", "title": "T"},
        "documentTab": {
            "body": {"content": ["b"]},
            "inlineObjects": {"io": {}},
            "positionedObjects": {"po": {}},
        },
    }]
    (_tid, _title, _body, inline, positioned), = _flatten_tabs(tabs)
    assert inline == {"io": {}} and positioned == {"po": {}}


def test_resolve_write_tab():
    legacy = {"body": {"content": ["b"]}}
    assert _resolve_write_tab(legacy, None) == (None, ["b"])

    tabbed = {"tabs": [
        {"tabProperties": {"tabId": "t.1", "title": "A"}, "documentTab": {"body": {"content": ["a"]}}},
        {"tabProperties": {"tabId": "t.2", "title": "B"}, "documentTab": {"body": {"content": ["b"]}}},
    ]}
    assert _resolve_write_tab(tabbed, None)[0] == "t.1"  # defaults to first tab
    assert _resolve_write_tab(tabbed, "t.2") == ("t.2", ["b"])
    with pytest.raises(RuntimeError):
        _resolve_write_tab(tabbed, "t.bogus")


# ---- dry-run (predict, never mutate) -------------------------------------------------

def _docs_svc(body_content: list):
    svc = MagicMock()
    svc.documents.return_value.get.return_value.execute.return_value = {
        "tabs": [{"tabProperties": {"tabId": "t.0", "title": "T"},
                  "documentTab": {"body": {"content": body_content}}}]
    }
    return svc


def test_write_sheet_dry_run_predicts_and_does_not_write(monkeypatch):
    svc = _sheets_svc(existing_values=[["a", "b"]])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.write_sheet(SID, "Data", [["=A1*2", "y"]], dry_run=True)
    assert out["dry_run"] is True
    assert out["before"] == [["a", "b"]] and out["after"] == [["=A1*2", "y"]]
    assert out["formulas_not_recalculated"] is True
    assert not svc.spreadsheets.return_value.values.return_value.update.called


def test_clear_range_dry_run_does_not_clear(monkeypatch):
    svc = _sheets_svc(existing_values=[["x", "y"]])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.clear_range(SID, "Data!A1:B1", dry_run=True)
    assert out["dry_run"] and out["before"] == [["x", "y"]] and out["after"] == []
    assert not svc.spreadsheets.return_value.values.return_value.clear.called


def test_delete_rows_dry_run_does_not_delete(monkeypatch):
    svc = _sheets_svc(existing_values=[["r"]], titles={"Data": 1})
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.delete_rows(SID, "Data", start_row=2, count=3, dry_run=True)
    assert out["dry_run"] and out["deletes_rows"] == 3 and out["rows"] == "2..4"
    assert not svc.spreadsheets.return_value.batchUpdate.called


def test_append_rows_dry_run_does_not_append(monkeypatch):
    svc = _sheets_svc(existing_values=[["h"], ["1"], ["2"]])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.append_rows(SID, "Data", [["9"]], dry_run=True)
    assert out["dry_run"] and out["at_row"] == 4 and out["appends_rows"] == 1
    assert not svc.spreadsheets.return_value.values.return_value.append.called


def test_append_text_dry_run_does_not_write(monkeypatch):
    body = [{"paragraph": {"elements": [{"textRun": {"content": "hello\n"}}]}, "endIndex": 7}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.append_text("A" * 30, " world", dry_run=True)
    assert out["dry_run"] and out["tab"] == "t.0"
    assert out["before_tail"] == "hello" and out["after_tail"] == "hello world"
    assert not svc.documents.return_value.batchUpdate.called


def test_insert_text_dry_run_does_not_write(monkeypatch):
    body = [{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.insert_text("A" * 30, "X", index=1, dry_run=True)
    assert out["dry_run"] and out["would_insert"] == "X" and out["at_index"] == 1
    assert not svc.documents.return_value.batchUpdate.called


# ---- markdown writes (opt-in) ---------------------------------------------------------

def test_append_text_markdown_blocks_start_fresh_paragraph(monkeypatch):
    body = [{"paragraph": {"elements": [{"textRun": {"content": "hello\n"}}]}, "endIndex": 7}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.append_text("A" * 30, "# Title", markdown=True)
    reqs = svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]
    assert [next(iter(r)) for r in reqs] == ["insertText", "updateParagraphStyle", "deleteParagraphBullets"]
    # tail paragraph is non-empty -> block content is pushed onto its own paragraph
    assert reqs[0]["insertText"] == {"location": {"index": 6, "tabId": "t.0"}, "text": "\nTitle"}
    heading = reqs[1]["updateParagraphStyle"]
    assert heading["range"] == {"startIndex": 7, "endIndex": 12, "tabId": "t.0"}
    assert heading["paragraphStyle"] == {"namedStyleType": "HEADING_1"}
    assert out["markdown"] == {"headings": 1, "list_items": 0, "styled_spans": 0}


def test_append_text_markdown_inline_only_merges_into_tail(monkeypatch):
    body = [{"paragraph": {"elements": [{"textRun": {"content": "hello\n"}}]}, "endIndex": 7}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.append_text("A" * 30, "**hi**", markdown=True)
    reqs = svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]
    assert [next(iter(r)) for r in reqs] == ["insertText", "updateTextStyle"]  # no paragraph restyling
    assert reqs[0]["insertText"]["text"] == "hi"  # markers stripped, no forced newline
    style = reqs[1]["updateTextStyle"]
    assert style["range"] == {"startIndex": 6, "endIndex": 8, "tabId": "t.0"}
    assert style["textStyle"] == {"bold": True} and style["fields"] == "bold"


def test_append_text_markdown_no_fresh_paragraph_when_tail_empty(monkeypatch):
    body = [{"paragraph": {"elements": [{"textRun": {"content": "\n"}}]}, "endIndex": 2}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.append_text("A" * 30, "# T", markdown=True)
    reqs = svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]
    assert reqs[0]["insertText"]["text"] == "T"  # tail paragraph already empty
    assert reqs[1]["updateParagraphStyle"]["range"] == {"startIndex": 1, "endIndex": 2, "tabId": "t.0"}


def test_insert_text_markdown_builds_bullets_at_index(monkeypatch):
    body = [{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.insert_text("A" * 30, "- a\n- b", index=5, markdown=True)
    reqs = svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]
    assert reqs[0]["insertText"] == {"location": {"index": 5, "tabId": "t.0"}, "text": "a\nb"}
    bullets = reqs[-1]["createParagraphBullets"]
    assert bullets["range"] == {"startIndex": 5, "endIndex": 8, "tabId": "t.0"}
    assert bullets["bulletPreset"] == "BULLET_DISC_CIRCLE_SQUARE"


def test_append_text_markdown_dry_run_does_not_write(monkeypatch):
    body = [{"paragraph": {"elements": [{"textRun": {"content": "hello\n"}}]}, "endIndex": 7}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.append_text("A" * 30, "- a\n- b", markdown=True, dry_run=True)
    assert out["dry_run"] and out["markdown"]["list_items"] == 2
    assert out["after_tail"] == "hello\na\nb"  # markers stripped in the prediction
    assert not svc.documents.return_value.batchUpdate.called


def test_create_document_plain_and_empty(monkeypatch):
    svc = MagicMock()
    svc.documents.return_value.create.return_value.execute.return_value = {"documentId": "D1", "title": "T"}
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.create_document("T")
    assert out["id"] == "D1" and "docs.google.com/document/d/D1" in out["url"]
    assert not svc.documents.return_value.batchUpdate.called  # no initial text -> no write
    assert svc.documents.return_value.create.call_args.kwargs["body"] == {"title": "T"}


def test_create_document_with_markdown_content(monkeypatch):
    svc = MagicMock()
    svc.documents.return_value.create.return_value.execute.return_value = {"documentId": "D1", "title": "T"}
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.create_document("T", text="# H\n- a", markdown=True)
    reqs = svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]
    assert reqs[0]["insertText"] == {"location": {"index": 1}, "text": "H\na"}
    assert [next(iter(r)) for r in reqs][-1] == "createParagraphBullets"
    assert out["markdown"] == {"headings": 1, "list_items": 1, "styled_spans": 0}


# ---- sheets cell formatting ------------------------------------------------------------

def test_format_cells_builds_repeat_cell(monkeypatch):
    svc = _sheets_svc(titles={"Data": 123})
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.format_cells(SID, "Data!A2:B3", bold=True, underline=False)
    rc = svc.spreadsheets.return_value.batchUpdate.call_args.kwargs["body"]["requests"][0]["repeatCell"]
    assert rc["range"] == {
        "sheetId": 123, "startRowIndex": 1, "endRowIndex": 3, "startColumnIndex": 0, "endColumnIndex": 2,
    }
    assert rc["cell"] == {"userEnteredFormat": {"textFormat": {"bold": True, "underline": False}}}
    # fields mask names only the passed flags, so unset properties are left untouched
    assert rc["fields"] == "userEnteredFormat.textFormat.bold,userEnteredFormat.textFormat.underline"
    assert out["cells"] == 4 and out["applied"] == {"bold": True, "underline": False}


def test_format_cells_defaults_to_first_tab(monkeypatch):
    svc = _sheets_svc(titles={"Data": 123})
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.format_cells(SID, "B2", italic=True)
    assert out["tab"] == "Data" and out["cells"] == 1


def test_format_cells_requires_a_flag_and_known_tab(monkeypatch):
    svc = _sheets_svc(titles={"Data": 123})
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    with pytest.raises(RuntimeError, match="at least one"):
        sheets_mod.format_cells(SID, "A1:B2")
    with pytest.raises(RuntimeError, match="not found"):
        sheets_mod.format_cells(SID, "Bogus!A1:B2", bold=True)
    assert not svc.spreadsheets.return_value.batchUpdate.called


def test_format_cells_dry_run_does_not_write(monkeypatch):
    svc = _sheets_svc(titles={"Data": 123})
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.format_cells(SID, "Data!A1:C2", bold=True, dry_run=True)
    assert out["dry_run"] and out["cells"] == 6 and out["applies"] == {"bold": True}
    assert not svc.spreadsheets.return_value.batchUpdate.called


# ---- colored text (opt-in) -----------------------------------------------------------

def test_hex_color_helpers_roundtrip_and_validation():
    from gdrive_mcp.tools.docs import _hex_to_rgb, _rgb_to_hex

    assert _hex_to_rgb("#FF0000") == {"red": 1.0, "green": 0.0, "blue": 0.0}
    assert _hex_to_rgb("00ff00")["green"] == 1.0  # leading # optional
    assert _rgb_to_hex({"red": 1.0}) == "#ff0000"  # missing channels default to 0
    with pytest.raises(RuntimeError):
        _hex_to_rgb("blue")


def test_colored_runs_extracts_only_colored():
    from gdrive_mcp.tools.docs import _colored_runs

    content = [{"paragraph": {"elements": [
        {"textRun": {"content": "red\n", "textStyle": {"foregroundColor": {"color": {"rgbColor": {"red": 1.0}}}}}},
        {"textRun": {"content": "plain", "textStyle": {}}},
    ]}}]
    assert _colored_runs(content) == [{"text": "red", "color": "#ff0000"}]


def test_append_text_colors_only_when_requested(monkeypatch):
    body = [{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)

    docs_mod.append_text("A" * 30, "X")  # no color -> plain insert only
    reqs = svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]
    assert [next(iter(r)) for r in reqs] == ["insertText"]

    docs_mod.append_text("A" * 30, "X", color="#FF0000")  # explicit color -> insert + style
    reqs = svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]
    assert [next(iter(r)) for r in reqs] == ["insertText", "updateTextStyle"]
    style = reqs[1]["updateTextStyle"]
    assert style["textStyle"]["foregroundColor"]["color"]["rgbColor"] == {"red": 1.0, "green": 0.0, "blue": 0.0}
    assert style["range"]["tabId"] == "t.0"


def test_read_document_include_colors(monkeypatch):
    body = [{"paragraph": {"elements": [
        {"textRun": {"content": "hello\n", "textStyle": {"foregroundColor": {"color": {"rgbColor": {"blue": 1.0}}}}}}
    ]}}]
    svc = _docs_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    assert docs_mod.read_document("A" * 30, include_colors=True)["colored_runs"] == [{"text": "hello", "color": "#0000ff"}]
    assert "colored_runs" not in docs_mod.read_document("A" * 30)  # off by default


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-q"]))
