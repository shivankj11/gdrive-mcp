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
    # Elements carry startIndex/endIndex exactly as the API returns them: the outline's write
    # anchors come from those offsets, so a fixture without them would not exercise the real shape.
    content = [
        {"startIndex": 1, "endIndex": 7,
         "paragraph": {"paragraphStyle": {"namedStyleType": "HEADING_1"},
                       "elements": [{"startIndex": 1, "textRun": {"content": "Title\n"}}]}},
        {"startIndex": 7, "endIndex": 17,
         "paragraph": {"elements": [{"startIndex": 7, "textRun": {"content": "body text\n"}}]}},
        {"table": {"tableRows": [
            {"tableCells": [
                {"content": [{"paragraph": {"elements": [{"textRun": {"content": "a | x"}}]}}]},
                {"content": [{"paragraph": {"elements": [{"textRun": {"content": "b"}}]}}]},
            ]},
            {"tableCells": [
                {"content": [{"paragraph": {"elements": [{"textRun": {"content": "1"}}]}}]},
                {"content": [{"paragraph": {"elements": [{"textRun": {"content": "2"}}]}}]},
            ]},
        ]}},
    ]
    md, outline = _content_to_markdown(content)
    assert "# Title" in md
    assert "body text" in md
    # GFM: header row, a column-matched delimiter row, then the body row; '|' in a cell is escaped
    assert "| a \\| x | b |\n| --- | --- |\n| 1 | 2 |" in md
    # start/end make each outline entry a usable write anchor, not just a table of contents
    assert outline == [{"level": 1, "text": "Title", "start": 1, "end": 7}]


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


# ---- locator-anchored edits: delete_text / replace_text --------------------------------

def _para(start: int, *runs: str, style: str | None = None) -> dict:
    elements, idx = [], start
    for content in runs:
        elements.append({"startIndex": idx, "textRun": {"content": content}})
        idx += len(content.encode("utf-16-le")) // 2
    p: dict = {"elements": elements}
    if style:
        p["paragraphStyle"] = {"namedStyleType": style}
    return {"startIndex": start, "endIndex": idx, "paragraph": p}


def _locator_svc(body_content: list, revision: str | None = "rev-1", tabbed: bool = True):
    svc = MagicMock()
    doc: dict = {"revisionId": revision} if revision else {}
    if tabbed:
        doc["tabs"] = [{"tabProperties": {"tabId": "t.0", "title": "T"},
                        "documentTab": {"body": {"content": body_content}}}]
    else:
        doc["body"] = {"content": body_content}
    svc.documents.return_value.get.return_value.execute.return_value = doc
    return svc


def _doc_body():
    return [_para(1, "keep alpha keep\n"), _para(17, "alpha again\n")]


def _reqs(svc) -> list:
    return svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]


def _body_arg(svc) -> dict:
    return svc.documents.return_value.batchUpdate.call_args.kwargs["body"]


def test_delete_text_gates_without_confirm(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.delete_text("A" * 30, match="alpha")
    assert out["status"] == "confirmation_required"
    assert out["impact"]["occurrences"] == 1 and out["impact"]["deletes_text"] == ["alpha"]
    assert not svc.documents.return_value.batchUpdate.called


def test_delete_text_executes_with_confirm(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.delete_text("A" * 30, match="alpha", confirm=True)
    assert out["deleted"] is True and out["chars"] == 5
    assert _reqs(svc) == [{"deleteContentRange": {"range": {"startIndex": 6, "endIndex": 11, "tabId": "t.0"}}}]


def test_delete_text_dry_run_neither_writes_nor_gates(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.delete_text("A" * 30, match="alpha", dry_run=True)
    assert out["dry_run"] is True and "status" not in out  # dry-run explores; confirm gates
    assert out["deletes_text"] == ["alpha"]
    assert not svc.documents.return_value.batchUpdate.called


def test_delete_text_all_occurrences_emit_descending_ranges(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.delete_text("A" * 30, match="alpha", occurrence=0, confirm=True)
    starts = [r["deleteContentRange"]["range"]["startIndex"] for r in _reqs(svc)]
    assert starts == sorted(starts, reverse=True)  # bottom-up keeps earlier ranges valid
    assert starts == [17, 6]


def test_locator_writes_pin_the_resolved_revision(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.delete_text("A" * 30, match="alpha", confirm=True)
    assert _body_arg(svc)["writeControl"] == {"requiredRevisionId": "rev-1"}


def test_untabbed_doc_emits_no_tab_id(monkeypatch):
    svc = _locator_svc(_doc_body(), tabbed=False)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.delete_text("A" * 30, match="alpha", confirm=True)
    assert _reqs(svc)[0]["deleteContentRange"]["range"] == {"startIndex": 6, "endIndex": 11}


def test_delete_text_section_removes_heading_and_body(monkeypatch):
    body = [_para(1, "Intro\n", style="HEADING_1"), _para(7, "body\n"), _para(12, "Next\n", style="HEADING_1")]
    svc = _locator_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.delete_text("A" * 30, section="Intro", confirm=True)
    assert out["deletes_text"] == ["Intro\nbody\n"]  # heading + its content, trailing newline included
    assert _reqs(svc)[0]["deleteContentRange"]["range"] == {"startIndex": 1, "endIndex": 12, "tabId": "t.0"}


def test_delete_text_requires_exactly_one_locator(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    with pytest.raises(RuntimeError, match="exactly one"):
        docs_mod.delete_text("A" * 30, confirm=True)
    assert not svc.documents.return_value.batchUpdate.called


def test_replace_text_gates_then_replaces_in_one_batch(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    gated_out = docs_mod.replace_text("A" * 30, "beta", match="alpha")
    assert gated_out["status"] == "confirmation_required"
    assert gated_out["impact"]["replaces_text"] == ["alpha"] and gated_out["impact"]["with_text"] == "beta"
    assert not svc.documents.return_value.batchUpdate.called

    docs_mod.replace_text("A" * 30, "beta", match="alpha", confirm=True)
    assert svc.documents.return_value.batchUpdate.call_count == 1  # never a delete-then-insert window
    assert [next(iter(r)) for r in _reqs(svc)] == ["deleteContentRange", "insertText"]
    assert _reqs(svc)[1]["insertText"] == {"location": {"index": 6, "tabId": "t.0"}, "text": "beta"}


def test_replace_text_markdown_styles_land_on_the_new_text(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.replace_text("A" * 30, "**bold**", match="alpha", markdown=True, confirm=True)
    kinds = [next(iter(r)) for r in _reqs(svc)]
    assert kinds == ["deleteContentRange", "insertText", "updateTextStyle"]
    assert _reqs(svc)[2]["updateTextStyle"]["range"] == {"startIndex": 6, "endIndex": 10, "tabId": "t.0"}


def test_replace_text_rejects_pipe_tables(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    with pytest.raises(RuntimeError, match="insert_table"):
        docs_mod.replace_text("A" * 30, "| a | b |\n| --- | --- |", match="alpha", markdown=True, confirm=True)
    assert not svc.documents.return_value.batchUpdate.called


def test_replace_text_with_empty_string_only_deletes(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.replace_text("A" * 30, "", match="alpha", confirm=True)
    assert [next(iter(r)) for r in _reqs(svc)] == ["deleteContentRange"]  # empty insertText is rejected by the API


# ---- locator anchors on the insert tools ------------------------------------------------

def test_insert_text_after_anchors_at_match_end(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.insert_text("A" * 30, "!", after="alpha")
    assert out["inserted_at"] == 11
    assert _reqs(svc)[0]["insertText"]["location"]["index"] == 11


def test_insert_text_before_anchors_at_match_start(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    assert docs_mod.insert_text("A" * 30, "!", before="alpha")["inserted_at"] == 6


def test_insert_text_block_markdown_snaps_to_paragraph_boundary(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    # 'alpha' sits mid-paragraph; a heading anchored there would restyle the host paragraph,
    # so block content snaps to the end of the containing paragraph instead.
    assert docs_mod.insert_text("A" * 30, "# H", after="alpha", markdown=True)["inserted_at"] == 17


def test_insert_text_rejects_multiple_or_missing_anchors(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    with pytest.raises(RuntimeError, match="only one of"):
        docs_mod.insert_text("A" * 30, "x", index=1, after="alpha")
    with pytest.raises(RuntimeError, match="pass one of"):
        docs_mod.insert_text("A" * 30, "x")
    assert not svc.documents.return_value.batchUpdate.called


def test_insert_text_unmatched_locator_raises_before_writing(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    with pytest.raises(RuntimeError, match="no match"):
        docs_mod.insert_text("A" * 30, "x", after="absent")
    assert not svc.documents.return_value.batchUpdate.called


def test_block_anchor_inside_a_table_cell_is_refused(monkeypatch):
    cell = {"content": [_para(4, "in-cell")]}
    body = [{"startIndex": 1, "endIndex": 13, "table": {"tableRows": [{"tableCells": [cell]}]}},
            _para(13, "outside\n")]
    svc = _locator_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    with pytest.raises(RuntimeError, match="inside a table cell"):
        docs_mod.insert_table("A" * 30, [["x"]], after="in-cell")
    with pytest.raises(RuntimeError, match="inside a table cell"):
        docs_mod.insert_text("A" * 30, "# H", after="in-cell", markdown=True)
    # inline text has no boundary requirement, so it still anchors inside the cell
    assert docs_mod.insert_text("A" * 30, "!", after="in-cell")["inserted_at"] == 11


def test_delete_text_section_at_end_of_doc_stops_before_final_newline(monkeypatch):
    body = [_para(1, "Only\n", style="HEADING_1"), _para(6, "tail\n")]
    svc = _locator_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.delete_text("A" * 30, section="Only", confirm=True)
    rng = _reqs(svc)[0]["deleteContentRange"]["range"]
    assert rng["endIndex"] == body[-1]["endIndex"] - 1  # the body's final newline is undeletable


def test_write_omits_write_control_when_the_api_returns_no_revision(monkeypatch):
    svc = _locator_svc(_doc_body(), revision=None)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.delete_text("A" * 30, match="alpha", confirm=True)
    # sending requiredRevisionId=None would be rejected outright; omit the key instead
    assert "writeControl" not in _body_arg(svc)


def test_insert_table_after_resolves_to_a_paragraph_boundary(monkeypatch):
    body = _doc_body()
    svc = _locator_svc(body)
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.insert_table("A" * 30, [["a"]], after="alpha", dry_run=True)
    # end of the paragraph holding the match -> a boundary insertTable accepts, so the
    # mid-paragraph 400 remap path is never reached
    assert out["at_index"] == 17
    assert out["at_index"] in {el["startIndex"] for el in body} | {docs_mod.locate.body_end(body)}


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-q"]))
