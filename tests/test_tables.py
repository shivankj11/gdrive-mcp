"""Table writes: the markdown pipe-table splitter (B) and the insert_table tool (A).

Covers the invariants surfaced by the adversarial plan review: deterministic table selection
after re-fetch (no wrong-table fill), rollback of an orphaned empty table on fill failure, the
bad-index 400 remap, forward running-offset fill math, empty-cell skip, escaped-pipe round-trip,
and the segmented renderer's index base.
"""

from unittest.mock import MagicMock

import pytest
from googleapiclient.errors import HttpError

from gdrive_mcp import md
from gdrive_mcp.tools import docs as docs_mod

SID = "A" * 30


def _tab_doc(content):
    return {"tabs": [{"tabProperties": {"tabId": "t.0", "title": "T"},
                      "documentTab": {"body": {"content": content}}}]}


def _http_error(status):
    return HttpError(MagicMock(status=status), b"{}")


# ---- md.split_blocks (pure) ----------------------------------------------------------

def test_split_blocks_text_only():
    assert md.split_blocks("# H\n- a") == [("text", "# H\n- a")]
    assert not md.has_table(md.split_blocks("# H\n- a"))


def test_split_blocks_lone_pipe_line_stays_text():
    # a pipe row with NO following delimiter row is literal text (backward-compatible)
    segs = md.split_blocks("| a | b |")
    assert segs == [("text", "| a | b |")]


def test_split_blocks_basic_table():
    segs = md.split_blocks("| Name | Role |\n| --- | --- |\n| Ada | Eng |")
    assert segs == [("table", [["Name", "Role"], ["Ada", "Eng"]])]


def test_split_blocks_single_row_table():
    # header + delimiter + no body rows -> a 1-row table (not zero-row)
    segs = md.split_blocks("| a | b |\n| --- | --- |")
    assert segs == [("table", [["a", "b"]])]


def test_split_blocks_interleaved_and_delimiter_variants():
    src = "intro\n| A | B |\n| :-- | --: |\n| 1 | 2 |\nouttro"
    assert md.split_blocks(src) == [
        ("text", "intro"),
        ("table", [["A", "B"], ["1", "2"]]),
        ("text", "outtro"),
    ]


def test_split_blocks_pads_and_truncates_body_rows():
    segs = md.split_blocks("| a | b | c |\n| --- | --- | --- |\n| 1 |\n| 1 | 2 | 3 | 4 |")
    assert segs == [("table", [["a", "b", "c"], ["1", "", ""], ["1", "2", "3"]])]


def test_split_blocks_keeps_cell_markup_for_parse_cell():
    # split_blocks hands back RAW cell source — inline markup is NOT parsed there, because each
    # cell is styled later against its own insertion index. md.parse_cell does that half, and its
    # spans are offsets into that one cell's text (UTF-16 units, like every Docs index).
    segs = md.split_blocks("| **b** | x _i_ |\n| --- | --- |\n| a**b** | 😀_i_ |")
    assert segs == [("table", [["**b**", "x _i_"], ["a**b**", "😀_i_"]])]
    (_kind, rows), = segs
    assert md.parse_cell(rows[0][0]) == ("b", [(0, 1, ("bold",))])
    assert md.parse_cell(rows[0][1]) == ("x i", [(2, 3, ("italic",))])
    assert md.parse_cell(rows[1][0]) == ("ab", [(1, 2, ("bold",))])
    assert md.parse_cell(rows[1][1]) == ("😀i", [(2, 3, ("italic",))])  # emoji = 2 units, not 1
    # the same dialect as prose, not a table-only subset: identical text and spans
    assert md.parse_cell("***bi*** <u>u</u>") == (
        md.parse_markdown("***bi*** <u>u</u>").text, md.parse_markdown("***bi*** <u>u</u>").spans
    )


def test_split_row_honors_escaped_pipe():
    # '\|' is not a delimiter; it decodes to a literal '|' inside the cell
    assert md.split_row(r"| a \| b | c |") == ["a | b", "c"]


def test_escaped_pipe_round_trips_through_reader_format():
    # reader escapes '|' as '\|'; the splitter decodes it back to the same single cell
    emitted = md.render_table_markdown([["a | b", "c"]])
    header = emitted.splitlines()[0]
    assert header == r"| a \| b | c |"
    assert md.split_row(header) == ["a | b", "c"]


def test_render_table_markdown_shape():
    out = md.render_table_markdown([["H1", "H2"], ["x", "y"]])
    assert out == "| H1 | H2 |\n| --- | --- |\n| x | y |"


# ---- insert_table (A) ----------------------------------------------------------------

def _insert_table_svc(get_returns):
    svc = MagicMock()
    svc.documents.return_value.get.return_value.execute.side_effect = list(get_returns)
    return svc


def test_insert_table_validation_rejects_bad_shapes(monkeypatch):
    svc = MagicMock()
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    for bad in ([], [[]], [["a", "b"], ["c"]], "notalist"):
        with pytest.raises(RuntimeError):
            docs_mod.insert_table(SID, bad)
    # too large
    with pytest.raises(RuntimeError, match="too large"):
        docs_mod.insert_table(SID, [["x"] * 20 for _ in range(600)])
    assert not svc.documents.return_value.get.called  # validation precedes any API call


def test_insert_table_dry_run_previews_without_writing(monkeypatch):
    svc = _insert_table_svc([_tab_doc([{"paragraph": {"elements": []}, "endIndex": 2}])])
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.insert_table(SID, [["a", "b"], ["c", "d"]], dry_run=True)
    assert out["dry_run"] and out["rows"] == 2 and out["cols"] == 2
    assert out["preview"] == "| a | b |\n| --- | --- |\n| c | d |"
    assert not svc.documents.return_value.batchUpdate.called


def test_insert_table_fills_with_running_offset_and_header_bold(monkeypatch):
    doc1 = _tab_doc([{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 7}])
    table_el = {"startIndex": 5, "endIndex": 30, "table": {"tableRows": [
        {"tableCells": [{"content": [{"startIndex": 8, "paragraph": {}}]},
                        {"content": [{"startIndex": 10, "paragraph": {}}]}]},
        {"tableCells": [{"content": [{"startIndex": 13, "paragraph": {}}]},
                        {"content": [{"startIndex": 15, "paragraph": {}}]}]},
    ]}}
    doc2 = _tab_doc([{"paragraph": {}, "endIndex": 7}, table_el])
    svc = _insert_table_svc([doc1, doc2])
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)

    out = docs_mod.insert_table(SID, [["A", "B"], ["C", "D"]], header=True)
    assert out["cells_filled"] == 4 and out["inserted_at"] == 6 and out["header"] is True

    calls = svc.documents.return_value.batchUpdate.call_args_list
    assert calls[0].kwargs["body"]["requests"] == [
        {"insertTable": {"rows": 2, "columns": 2, "location": {"index": 6, "tabId": "t.0"}}}
    ]
    fill = calls[1].kwargs["body"]["requests"]
    inserts = [r["insertText"] for r in fill if "insertText" in r]
    # forward running-offset: A@8, B@10+1, C@13+2, D@15+3
    assert inserts == [
        {"location": {"index": 8, "tabId": "t.0"}, "text": "A"},
        {"location": {"index": 11, "tabId": "t.0"}, "text": "B"},
        {"location": {"index": 15, "tabId": "t.0"}, "text": "C"},
        {"location": {"index": 18, "tabId": "t.0"}, "text": "D"},
    ]
    bolds = [r["updateTextStyle"]["range"] for r in fill if "updateTextStyle" in r]
    assert bolds == [  # only row 0
        {"startIndex": 8, "endIndex": 9, "tabId": "t.0"},
        {"startIndex": 11, "endIndex": 12, "tabId": "t.0"},
    ]


def test_insert_table_skips_empty_cells(monkeypatch):
    doc1 = _tab_doc([{"paragraph": {}, "endIndex": 2}])
    table_el = {"startIndex": 1, "endIndex": 20, "table": {"tableRows": [
        {"tableCells": [{"content": [{"startIndex": 3, "paragraph": {}}]},
                        {"content": [{"startIndex": 5, "paragraph": {}}]}]},
    ]}}
    svc = _insert_table_svc([doc1, _tab_doc([{"paragraph": {}, "endIndex": 2}, table_el])])
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.insert_table(SID, [["", "y"]])
    assert out["cells_filled"] == 1
    inserts = [r["insertText"] for r in svc.documents.return_value.batchUpdate.call_args_list[1].kwargs["body"]["requests"]
               if "insertText" in r]
    assert inserts == [{"location": {"index": 5, "tabId": "t.0"}, "text": "y"}]  # empty cell skipped


def test_insert_table_selects_new_table_not_preexisting(monkeypatch):
    # A pre-existing table (start 100) is present before AND after the insert; the fill must target
    # the NEW table (start 5), never the old one. Guards the wrong-table data-corruption bug.
    old = {"startIndex": 100, "endIndex": 130, "table": {"tableRows": [
        {"tableCells": [{"content": [{"startIndex": 101, "paragraph": {}}]}]}]}}
    doc1 = _tab_doc([{"paragraph": {}, "endIndex": 7}, old])
    new = {"startIndex": 5, "endIndex": 12, "table": {"tableRows": [
        {"tableCells": [{"content": [{"startIndex": 6, "paragraph": {}}]},
                        {"content": [{"startIndex": 8, "paragraph": {}}]}]}]}}
    doc2 = _tab_doc([{"paragraph": {}, "endIndex": 7}, new, old])
    svc = _insert_table_svc([doc1, doc2])
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)

    docs_mod.insert_table(SID, [["x", "y"]], index=5)
    fill = svc.documents.return_value.batchUpdate.call_args_list[1].kwargs["body"]["requests"]
    inserts = [r["insertText"]["location"]["index"] for r in fill if "insertText" in r]
    assert inserts == [6, 9]  # new table's cells (6, 8+1), NOT the pre-existing table's 101


def test_insert_table_rolls_back_orphan_on_fill_failure(monkeypatch):
    doc1 = _tab_doc([{"paragraph": {}, "endIndex": 7}])
    table_el = {"startIndex": 5, "endIndex": 12, "table": {"tableRows": [
        {"tableCells": [{"content": [{"startIndex": 6, "paragraph": {}}]}]}]}}
    svc = _insert_table_svc([doc1, _tab_doc([{"paragraph": {}, "endIndex": 7}, table_el])])
    # batch1 (insertTable) ok, batch2 (fill) fails, batch3 (rollback delete) ok
    svc.documents.return_value.batchUpdate.return_value.execute.side_effect = [{}, _http_error(500), {}]
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)

    with pytest.raises(RuntimeError):
        docs_mod.insert_table(SID, [["x"]])
    calls = svc.documents.return_value.batchUpdate.call_args_list
    assert calls[2].kwargs["body"]["requests"] == [
        {"deleteContentRange": {"range": {"startIndex": 5, "endIndex": 12, "tabId": "t.0"}}}
    ]


def test_insert_table_bad_index_400_is_remapped(monkeypatch):
    svc = _insert_table_svc([_tab_doc([{"paragraph": {}, "endIndex": 7}])])
    svc.documents.return_value.batchUpdate.return_value.execute.side_effect = [_http_error(400)]
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    with pytest.raises(RuntimeError, match="paragraph boundary"):
        docs_mod.insert_table(SID, [["x"]], index=999)


# ---- Option B segmented renderer -----------------------------------------------------

def test_segment_text_heading_range_starts_at_anchor():
    # a heading text-segment's paragraph range must start at the anchor (not anchor+1) — the
    # trailing-newline design keeps insertion index == style base
    svc = MagicMock()
    docs_mod._insert_markdown_segments(svc, "D", "t.0", 10, [("text", "# H")], lead_newline=False)
    reqs = svc.documents.return_value.batchUpdate.call_args.kwargs["body"]["requests"]
    assert reqs[0]["insertText"] == {"location": {"index": 10, "tabId": "t.0"}, "text": "H\n"}
    assert reqs[1]["updateParagraphStyle"]["range"] == {"startIndex": 10, "endIndex": 11, "tabId": "t.0"}


def test_segment_empty_text_is_skipped():
    svc = MagicMock()
    docs_mod._insert_markdown_segments(svc, "D", None, 5, [("text", "   ")], lead_newline=False)
    assert not svc.documents.return_value.batchUpdate.called


def test_append_text_markdown_table_uses_segmented_path(monkeypatch):
    para = {"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}
    new_table = {"startIndex": 3, "endIndex": 15, "table": {"tableRows": [
        {"tableCells": [{"content": [{"startIndex": 4, "paragraph": {}}]},
                        {"content": [{"startIndex": 6, "paragraph": {}}]}]}]}}
    svc = _insert_table_svc([
        _tab_doc([para]),               # append: resolve tab + start
        _tab_doc([para]),               # _insert_one_table: pre-snapshot (no tables)
        _tab_doc([para, new_table]),    # _fill_new_table: post-insert re-fetch
    ])
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)

    out = docs_mod.append_text(SID, "| a | b |\n| --- | --- |", markdown=True)
    assert out["tables"] == 1 and out["tab"] == "t.0" and out["inserted_at"] == 3
    calls = svc.documents.return_value.batchUpdate.call_args_list
    assert calls[0].kwargs["body"]["requests"][0]["insertTable"] == {
        "rows": 1, "columns": 2, "location": {"index": 3, "tabId": "t.0"}
    }
    inserts = [r["insertText"] for r in calls[1].kwargs["body"]["requests"] if "insertText" in r]
    assert inserts == [
        {"location": {"index": 4, "tabId": "t.0"}, "text": "a"},
        {"location": {"index": 7, "tabId": "t.0"}, "text": "b"},  # b@6 + offset 1
    ]


def test_append_text_markdown_table_styles_cells_at_their_own_offsets(monkeypatch):
    # A cell's inline markup becomes styled spans based on THAT cell's insertion index (post-shift),
    # not the table start and not a running text offset: '| **b** | x _i_ |' bolds only the first
    # cell's single char and italicises only the 'i' at the second cell's offset 2.
    para = {"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}
    new_table = {"startIndex": 3, "endIndex": 15, "table": {"tableRows": [
        {"tableCells": [{"content": [{"startIndex": 4, "paragraph": {}}]},
                        {"content": [{"startIndex": 6, "paragraph": {}}]}]}]}}
    svc = _insert_table_svc([_tab_doc([para]), _tab_doc([para]), _tab_doc([para, new_table])])
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)

    docs_mod.append_text(SID, "| **b** | x _i_ |\n| --- | --- |", markdown=True)
    fill = svc.documents.return_value.batchUpdate.call_args_list[1].kwargs["body"]["requests"]
    assert fill == [
        {"insertText": {"location": {"index": 4, "tabId": "t.0"}, "text": "b"}},
        {"updateTextStyle": {"range": {"startIndex": 4, "endIndex": 5, "tabId": "t.0"},
                             "textStyle": {"bold": True}, "fields": "bold"}},
        # 'x i' at 6+1 (one char already inserted above); the italic span sits at 7+2, inside it
        {"insertText": {"location": {"index": 7, "tabId": "t.0"}, "text": "x i"}},
        {"updateTextStyle": {"range": {"startIndex": 9, "endIndex": 10, "tabId": "t.0"},
                             "textStyle": {"italic": True}, "fields": "italic"}},
    ]


def test_append_text_markdown_table_dry_run_does_not_write(monkeypatch):
    para = {"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}
    svc = _insert_table_svc([_tab_doc([para])])
    monkeypatch.setattr(docs_mod, "docs", lambda: svc)
    out = docs_mod.append_text(SID, "| a | b |\n| --- | --- |\n| 1 | 2 |", markdown=True, dry_run=True)
    assert out["dry_run"] and out["tables"] == 1
    assert out["preview"] == "| a | b |\n| --- | --- |\n| 1 | 2 |"
    assert not svc.documents.return_value.batchUpdate.called
