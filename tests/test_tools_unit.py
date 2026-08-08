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


def test_write_sheet_gate_inspects_formulas_so_one_rendering_empty_still_trips_it(monkeypatch):
    # Under UNFORMATTED_VALUE a formula evaluating to "" comes back as "", _count_nonempty reads the
    # target as empty, the gate does not fire, and a confirm-less write destroys the formula.
    # Reading FORMULA is what closes that: formula *text* is never empty.
    svc = _sheets_svc(existing_values=[['=IF(A9=1,"","x")']])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.write_sheet(SID, "Data", [["new"]], confirm=False)
    kwargs = svc.spreadsheets.return_value.values.return_value.get.call_args.kwargs
    assert kwargs["valueRenderOption"] == "FORMULA"
    assert out["status"] == "confirmation_required"
    assert out["impact"]["overwrites_nonempty_cells"] == 1
    assert not svc.spreadsheets.return_value.values.return_value.update.called


def test_write_sheet_dry_run_shows_a_formula_cell_as_its_formula(monkeypatch):
    # The same read feeds `before`, so a caller deciding whether to overwrite sees the formula it
    # would destroy rather than the value that formula happened to produce.
    svc = _sheets_svc(existing_values=[["=SUM(B:B)"]])
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    out = sheets_mod.write_sheet(SID, "Data", [["x"]], dry_run=True)
    assert out["before"] == [["=SUM(B:B)"]]
    assert out["overwrites_nonempty_cells"] == 1


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


def test_replace_text_dry_run_neither_writes_nor_gates(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.replace_text("A" * 30, "beta", match="alpha", dry_run=True)
    assert out["dry_run"] is True and "status" not in out  # dry-run explores; confirm gates
    assert out["replaces_text"] == ["alpha"] and out["with_text"] == "beta"
    assert not svc.documents.return_value.batchUpdate.called


def test_replace_text_all_occurrences_pair_each_delete_with_its_own_insert(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.replace_text("A" * 30, "beta", match="alpha", occurrence=0, confirm=True)
    assert svc.documents.return_value.batchUpdate.call_count == 1  # one batch, never a partial rewrite
    reqs = _reqs(svc)
    # paired per range, not grouped into all-deletes-then-all-inserts
    assert [next(iter(r)) for r in reqs] == [
        "deleteContentRange", "insertText", "deleteContentRange", "insertText",
    ]
    deleted = [r["deleteContentRange"]["range"]["startIndex"] for r in reqs if "deleteContentRange" in r]
    inserted = [r["insertText"]["location"]["index"] for r in reqs if "insertText" in r]
    assert deleted == [17, 6]  # bottom-up keeps the earlier range valid
    assert inserted == deleted  # each insert rewrites the range deleted just before it


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


def test_replace_text_pins_the_resolved_revision(monkeypatch):
    svc = _locator_svc(_doc_body())
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    docs_mod.replace_text("A" * 30, "beta", match="alpha", confirm=True)
    assert _body_arg(svc)["writeControl"] == {"requiredRevisionId": "rev-1"}

    norev = _locator_svc(_doc_body(), revision=None)
    monkeypatch.setattr(docs_mod, "docs", lambda: norev)
    docs_mod.replace_text("A" * 30, "beta", match="alpha", confirm=True)
    assert "writeControl" not in _body_arg(norev)  # requiredRevisionId=None would be rejected outright


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


# ---- discovery: resolve_link / get_metadata ---------------------------------------------

def _drive_get_svc(meta: dict):
    svc = MagicMock()
    svc.files.return_value.get.return_value.execute.return_value = meta
    return svc


def test_resolve_link_slims_the_response_and_sends_the_parsed_id(monkeypatch):
    svc = _drive_get_svc({
        "id": "F1",
        "name": "Q3 Plan",
        "mimeType": "application/vnd.google-apps.document",
        "modifiedTime": "2026-01-02T03:04:05Z",
        "size": "1234",
        "webViewLink": "https://docs.google.com/document/d/F1/edit",
        "owners": [{"displayName": "A", "emailAddress": "a@example.com"}],
        "parents": ["P1"],
    })
    monkeypatch.setattr(disc, "drive", lambda: svc)
    out = disc.resolve_link(f"https://docs.google.com/document/d/{'D' * 30}/edit")
    assert out["id"] == "F1" and out["name"] == "Q3 Plan" and out["kind"] == "document"
    assert out["modified"] == "2026-01-02T03:04:05Z" and out["size"] == "1234"
    assert out["web_view_link"] == "https://docs.google.com/document/d/F1/edit"
    assert "owners" not in out and "parents" not in out  # the verbose fields are dropped
    kwargs = svc.files.return_value.get.call_args.kwargs
    assert kwargs["fileId"] == "D" * 30  # the id parsed out of the URL, not the URL
    assert kwargs["supportsAllDrives"] is True  # so shared-drive items resolve too
    assert "owners(displayName,emailAddress)" in kwargs["fields"]  # mask survives its line break


def test_resolve_link_kinds_only_the_mime_types_it_knows(monkeypatch):
    for mime, kind in [
        ("application/vnd.google-apps.spreadsheet", "spreadsheet"),
        ("application/vnd.google-apps.folder", "folder"),
        ("application/pdf", "file"),  # anything else is just a file
    ]:
        svc = _drive_get_svc({"id": "X", "mimeType": mime})
        monkeypatch.setattr(disc, "drive", lambda: svc)
        assert disc.resolve_link("A" * 30)["kind"] == kind


def test_get_metadata_returns_drives_response_verbatim(monkeypatch):
    raw = {
        "id": "F1", "name": "Q3 Plan", "mimeType": "application/pdf",
        "owners": [{"displayName": "A", "emailAddress": "a@example.com"}], "parents": ["P1"],
        "createdTime": "2026-01-01T00:00:00Z", "shared": True, "trashed": False,
    }
    svc = _drive_get_svc(raw)
    monkeypatch.setattr(disc, "drive", lambda: svc)
    assert disc.get_metadata("A" * 30) == raw  # unlike resolve_link, nothing is slimmed away
    kwargs = svc.files.return_value.get.call_args.kwargs
    assert kwargs["fields"].endswith(", createdTime, description, shared, trashed")
    assert kwargs["supportsAllDrives"] is True


# ---- comments ---------------------------------------------------------------------------

def _comments_svc(listed: dict | None = None, created: dict | None = None):
    svc = MagicMock()
    svc.comments.return_value.list.return_value.execute.return_value = listed if listed is not None else {}
    svc.comments.return_value.create.return_value.execute.return_value = created or {"id": "cmt-1"}
    return svc


def test_read_comments_clamps_page_size_and_surfaces_the_next_token(monkeypatch):
    svc = _comments_svc(listed={"comments": [{"id": "c1"}], "nextPageToken": "TOK"})
    monkeypatch.setattr(docs_mod, "drive", lambda: svc)
    out = docs_mod.read_comments("A" * 30, page_size=5000, page_token="prev")
    assert out["file_id"] == "A" * 30 and out["comments"] == [{"id": "c1"}]
    assert out["has_more"] is True and out["next_page_token"] == "TOK"
    kwargs = svc.comments.return_value.list.call_args.kwargs
    assert kwargs["pageSize"] == 100 and kwargs["pageToken"] == "prev"  # Drive caps comments at 100
    assert "replies(content,author/displayName)" in kwargs["fields"]  # replies ride along with the page


def test_read_comments_last_page_reports_no_more(monkeypatch):
    svc = _comments_svc()  # no comments, no token
    monkeypatch.setattr(docs_mod, "drive", lambda: svc)
    out = docs_mod.read_comments("A" * 30, page_size=0)
    assert out["comments"] == [] and out["has_more"] is False and out["next_page_token"] is None
    assert svc.comments.return_value.list.call_args.kwargs["pageSize"] == 1  # 0 would be rejected


def test_add_comment_posts_the_content_and_returns_the_new_id(monkeypatch):
    svc = _comments_svc(created={"id": "cmt-1", "content": "hi"})
    monkeypatch.setattr(docs_mod, "drive", lambda: svc)
    assert docs_mod.add_comment("A" * 30, "hi") == {"file_id": "A" * 30, "comment_id": "cmt-1"}
    kwargs = svc.comments.return_value.create.call_args.kwargs
    assert kwargs["fileId"] == "A" * 30 and kwargs["body"] == {"content": "hi"}


# ---- image extraction --------------------------------------------------------------------

def _inline_image(uri: str) -> dict:
    return {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": uri}}}}


def _positioned_image(uri: str) -> dict:
    return {"positionedObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": uri}}}}


def _image_session(by_uri: dict):
    """Stub of the OAuth-carrying AuthorizedSession: one canned response per URI."""
    def get(uri, **_kwargs):
        content, content_type = by_uri[uri]
        resp = MagicMock()
        resp.is_redirect = resp.is_permanent_redirect = False
        resp.headers = {"content-type": content_type}
        resp.content = content
        return resp

    session = MagicMock()
    session.get.side_effect = get
    return session


def test_extract_images_fetches_only_google_hosts_in_document_order(monkeypatch):
    doc = {
        "body": {"content": [{"paragraph": {
            "positionedObjectIds": ["po1"],
            "elements": [
                {"inlineObjectElement": {"inlineObjectId": "evil"}},
                {"inlineObjectElement": {"inlineObjectId": "io1"}},
            ],
        }}]},
        "inlineObjects": {
            "evil": _inline_image("https://evil.com/x.png"),
            "io1": _inline_image("https://lh3.googleusercontent.com/inline"),
        },
        "positionedObjects": {"po1": _positioned_image("https://lh3.googleusercontent.com/anchored")},
    }
    svc = MagicMock()
    svc.documents.return_value.get.return_value.execute.return_value = doc
    session = _image_session({
        "https://evil.com/x.png": (b"pwned", "image/png"),
        "https://lh3.googleusercontent.com/anchored": (b"anchored", "image/png"),
        "https://lh3.googleusercontent.com/inline": (b"inline", "image/jpeg; charset=x"),
    })
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    monkeypatch.setattr(docs_mod, "authed_session", lambda: session)
    images = docs_mod.extract_images("A" * 30)
    # anchored image first (it is anchored to the paragraph's start), then the paragraph's inline one
    assert [(i.data, i._mime_type) for i in images] == [(b"anchored", "image/png"), (b"inline", "image/jpeg")]
    # the fetch carries the user's Drive token, so the non-Google host is never contacted at all
    assert [c.args[0] for c in session.get.call_args_list] == [
        "https://lh3.googleusercontent.com/anchored",
        "https://lh3.googleusercontent.com/inline",
    ]
    assert session.get.call_args.kwargs["allow_redirects"] is False  # a redirect could carry it off-host


# ---- spreadsheet and tab creation --------------------------------------------------------

def test_create_spreadsheet_names_its_tabs_only_when_asked(monkeypatch):
    svc = MagicMock()
    svc.spreadsheets.return_value.create.return_value.execute.return_value = {
        "spreadsheetId": "NEW", "spreadsheetUrl": "https://example/NEW"
    }
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    assert sheets_mod.create_spreadsheet("T", tabs=["a", "b"]) == {"id": "NEW", "url": "https://example/NEW"}
    create = svc.spreadsheets.return_value.create
    assert create.call_args.kwargs["body"] == {
        "properties": {"title": "T"},
        "sheets": [{"properties": {"title": "a"}}, {"properties": {"title": "b"}}],
    }
    sheets_mod.create_spreadsheet("T")  # no tabs -> no sheets key, so Sheets makes its default one
    assert create.call_args.kwargs["body"] == {"properties": {"title": "T"}}


def test_add_tab_sends_an_index_only_when_one_was_given(monkeypatch):
    svc = MagicMock()
    svc.spreadsheets.return_value.batchUpdate.return_value.execute.return_value = {
        "replies": [{"addSheet": {"properties": {"sheetId": 7, "title": "New", "index": 2}}}]
    }
    monkeypatch.setattr(sheets_mod, "sheets", lambda: svc)
    assert sheets_mod.add_tab(SID, "New", index=2) == {"sheet_id": 7, "title": "New", "index": 2}
    batch = svc.spreadsheets.return_value.batchUpdate
    assert batch.call_args.kwargs["spreadsheetId"] == SID
    assert batch.call_args.kwargs["body"]["requests"][0]["addSheet"]["properties"] == {
        "title": "New", "index": 2,
    }
    sheets_mod.add_tab(SID, "New")  # no index -> appended, not forced to position 0
    assert batch.call_args.kwargs["body"]["requests"][0]["addSheet"]["properties"] == {"title": "New"}


# ---- file reads: text extraction, download, export ---------------------------------------

# The documented inline/spill threshold, spelled out rather than read from files_mod: taking it
# from the module would resize these payloads along with it and hide a change to the limit itself.
_FIVE_MIB = 5 * 1024 * 1024


def _files_svc(mime: str, name: str = "f", raw: bytes = b"", exported: bytes = b""):
    svc = MagicMock()
    svc.files.return_value.get.return_value.execute.return_value = {
        "id": "F", "name": name, "mimeType": mime, "size": str(len(raw))
    }
    svc.files.return_value.get_media.return_value.execute.return_value = raw
    svc.files.return_value.export.return_value.execute.return_value = exported
    return svc


def _two_page_pdf() -> bytes:
    """A two-page PDF built by hand (with a correct xref) so the page count is a real one."""
    def obj(num: int, body: str) -> str:
        return f"{num} 0 obj\n{body}\nendobj\n"

    def page(num: int, contents: int) -> str:
        return obj(num, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents "
                        f"{contents} 0 R /Resources << /Font << /F1 7 0 R >> >> >>")

    def stream(num: int, word: str) -> str:
        text = f"BT /F1 24 Tf 20 100 Td ({word}) Tj ET\n"
        return obj(num, f"<< /Length {len(text)} >>\nstream\n{text}endstream")

    objects = [
        obj(1, "<< /Type /Catalog /Pages 2 0 R >>"),
        obj(2, "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>"),
        page(3, 4), stream(4, "Hello"), page(5, 6), stream(6, "World"),
        obj(7, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"),
    ]
    out, offsets = "%PDF-1.4\n", []
    for body in objects:
        offsets.append(len(out))
        out += body
    startxref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n"
    out += "".join(f"{off:010} 00000 n \n" for off in offsets)
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n"
    return out.encode()


def test_read_file_as_text_exports_google_native_files(monkeypatch):
    svc = _files_svc("application/vnd.google-apps.document", name="Notes", exported=b"para one\n\npara two")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    out = files_mod.read_file_as_text("A" * 30)
    assert out["name"] == "Notes" and out["mime_type"] == "application/vnd.google-apps.document"
    assert out["content"] == "para one\n\npara two" and out["total_chunks"] == 1 and out["has_more"] is False
    assert svc.files.return_value.export.call_args.kwargs["mimeType"] == "text/plain"
    assert not svc.files.return_value.get_media.called  # a native file has no bytes to download
    assert svc.files.return_value.get.call_args.kwargs["supportsAllDrives"] is True

    sheet = _files_svc("application/vnd.google-apps.spreadsheet", exported=b"a,b\n1,2")
    monkeypatch.setattr(files_mod, "drive", lambda: sheet)
    files_mod.read_file_as_text("A" * 30)
    assert sheet.files.return_value.export.call_args.kwargs["mimeType"] == "text/csv"  # not text/plain


def test_read_file_as_text_extracts_pdf_text_and_page_count(monkeypatch):
    svc = _files_svc("application/pdf", name="report.pdf", raw=_two_page_pdf())
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    out = files_mod.read_file_as_text("A" * 30)
    assert out["pages"] == 2
    assert "Hello" in out["content"] and "World" in out["content"]  # every page, not just the first
    assert not svc.files.return_value.export.called  # a PDF is downloaded, not exported


def test_read_file_as_text_curates_a_corrupt_pdf_instead_of_leaking_pypdf(monkeypatch):
    # api_errors only curates HttpError, so a truncated PDF used to escape as a raw
    # pypdf.errors.PdfStreamError — an exception type the agent cannot act on and which says
    # nothing about the *file* being at fault rather than the request. Rust pins the same message.
    svc = _files_svc("application/pdf", name="broken.pdf", raw=b"%PDF-1.4 truncated")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    with pytest.raises(RuntimeError, match="^could not extract text from PDF: "):
        files_mod.read_file_as_text("A" * 30)


def test_read_file_as_text_pages_a_plain_text_file(monkeypatch):
    svc = _files_svc("text/plain", name="long.txt", raw=b"aaaa\n\nbbbb\n\ncccc\xff")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    out = files_mod.read_file_as_text("A" * 30, chunk=1, max_chars=5)
    assert out["content"] == "bbbb" and out["chunk_index"] == 1
    assert out["total_chunks"] == 3 and out["total_chars"] == 17 and out["has_more"] is True
    whole = files_mod.read_file_as_text("A" * 30, max_chars=0)  # <=0 disables chunking
    assert whole["total_chunks"] == 1 and whole["content"].endswith("cccc�")  # undecodable byte replaced
    assert svc.files.return_value.get_media.call_args.kwargs["supportsAllDrives"] is True


def test_read_file_as_text_refuses_a_binary_mime(monkeypatch):
    svc = _files_svc("image/png", name="shot.png", raw=b"\x89PNG")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    with pytest.raises(RuntimeError, match="not text-extractable"):
        files_mod.read_file_as_text("A" * 30)


def test_download_file_returns_a_small_file_inline(monkeypatch):
    svc = _files_svc("image/png", name="shot.png", raw=b"\x89PNG")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    assert files_mod.download_file("A" * 30) == {
        "name": "shot.png", "mime_type": "image/png", "bytes": 4, "base64": "iVBORw==",
    }
    assert svc.files.return_value.get_media.call_args.kwargs["supportsAllDrives"] is True


def test_download_file_spills_over_the_inline_limit(monkeypatch, tmp_path):
    assert files_mod._INLINE_MAX == _FIVE_MIB  # the "<5MB → inline" the docstring promises
    big = b"x" * (_FIVE_MIB + 1)
    svc = _files_svc("application/zip", name="big.zip", raw=big)
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(tmp_path))
    out = files_mod.download_file("A" * 30)
    # base64 of 5MB+ would flood the agent's context, so it goes to the sandbox with a note instead
    assert "base64" not in out and out["bytes"] == len(big)
    assert out["note"] == f"{len(big)} bytes exceeded {_FIVE_MIB} inline limit; written to file"
    assert out["path"].endswith("big.zip") and (tmp_path / "big.zip").read_bytes() == big


def test_download_file_with_a_dest_path_spills_even_a_tiny_file(monkeypatch, tmp_path):
    svc = _files_svc("application/zip", name="small.zip", raw=b"pk")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(tmp_path))
    out = files_mod.download_file("A" * 30, dest_path="sub/out.zip")
    assert "base64" not in out and out["note"] is None  # an explicit destination needs no explaining
    spilled = tmp_path / "sub" / "out.zip"
    assert spilled.read_bytes() == b"pk"
    assert spilled.stat().st_mode & 0o777 == 0o600  # spilled bytes may be sensitive: owner-only


def test_download_file_refuses_a_google_native_file(monkeypatch):
    svc = _files_svc("application/vnd.google-apps.document", name="Notes")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    with pytest.raises(RuntimeError, match="use export_file or read_file_as_text"):
        files_mod.download_file("A" * 30)
    assert not svc.files.return_value.get_media.called  # get_media on a native file 403s


def test_export_file_returns_a_small_export_inline(monkeypatch):
    svc = _files_svc("application/vnd.google-apps.document", exported=b"%PDF")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    assert files_mod.export_file("A" * 30, "PDF") == {"format": "PDF", "bytes": 4, "base64": "JVBERg=="}
    kwargs = svc.files.return_value.export.call_args.kwargs
    assert kwargs["mimeType"] == "application/pdf"  # the format is matched case-insensitively
    assert "supportsAllDrives" not in kwargs  # Drive's export method has no such parameter


def test_export_file_rejects_an_unsupported_format(monkeypatch):
    svc = _files_svc("application/vnd.google-apps.document")
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    with pytest.raises(RuntimeError, match=r"unsupported export format 'epub'; choose from \['csv', 'docx'"):
        files_mod.export_file("A" * 30, "epub")
    assert not svc.files.return_value.export.called  # refused before any API call


def test_export_file_spills_a_large_export_under_the_file_id(monkeypatch, tmp_path):
    svc = _files_svc("application/vnd.google-apps.spreadsheet", exported=b"x" * (_FIVE_MIB + 1))
    monkeypatch.setattr(files_mod, "drive", lambda: svc)
    monkeypatch.setenv("GDRIVE_MCP_FILES_DIR", str(tmp_path))
    out = files_mod.export_file("A" * 30, "CSV")
    assert "base64" not in out and out["format"] == "CSV"
    # the default name comes from the file id and the lowercased format, not the caller's spelling
    assert out["path"].endswith(f"{'A' * 30}.csv")
    assert (tmp_path / f"{'A' * 30}.csv").stat().st_size == _FIVE_MIB + 1


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-q"]))
