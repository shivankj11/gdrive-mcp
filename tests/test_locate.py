"""Resolver tests: locator -> Docs index ranges, over synthetic body content.

Pure index math, no Google service and no mocks (the style of test_a1.py). This is where the
correctness burden sits: an offset bug here is silent data loss at the delete/replace tools,
and it is the only layer where the arithmetic is falsifiable without a live document.
"""

import pytest

from gdrive_mcp.locate import (
    body_end,
    find_matches,
    find_section,
    paragraph_bounds,
    resolve,
    text_in_range,
)


def para(start: int, *runs: str, style: str | None = None) -> dict:
    """A paragraph element with one text run per string, indexed the way the API indexes them."""
    elements, idx = [], start
    for content in runs:
        elements.append({"startIndex": idx, "textRun": {"content": content}})
        idx += len(content.encode("utf-16-le")) // 2
    p: dict = {"elements": elements}
    if style:
        p["paragraphStyle"] = {"namedStyleType": style}
    return {"startIndex": start, "endIndex": idx, "paragraph": p}


def table(start: int, *cell_texts: str) -> dict:
    """A one-row table whose cells each hold a single paragraph."""
    cells, idx = [], start + 3  # table/row/cell structural offsets precede the first paragraph
    for t in cell_texts:
        cells.append({"content": [para(idx, t)]})
        idx += len(t) + 2
    return {"startIndex": start, "endIndex": idx, "table": {"tableRows": [{"tableCells": cells}]}}


# ---- offset mapping ------------------------------------------------------------------

def test_single_run_match_uses_the_runs_own_start_index():
    body = [para(1, "hello world\n")]
    assert find_matches(body, "world") == [(7, 12)]


def test_astral_char_before_needle_resolves_in_utf16_units():
    # '😀' is one code point but TWO UTF-16 units, which is what Docs indexes count. 'a😀b ' is
    # 5 units, so 'needle' starts at 1+5=6. Counting code points gives 5 — off by one, and the
    # delete would eat the preceding space and leave a trailing 'e'.
    body = [para(1, "a😀b needle\n")]
    assert find_matches(body, "needle") == [(6, 12)]
    assert text_in_range(body, 6, 12) == "needle"


def test_match_spanning_two_text_runs_in_one_paragraph():
    body = [para(1, "hel", "lo world\n")]
    assert find_matches(body, "lo wo") == [(4, 9)]


def test_non_text_element_between_runs_does_not_desync_mapping():
    # An inline image occupies one index but contributes no text; the second run's own
    # startIndex accounts for it, so offsets after the image stay correct.
    body = [{
        "startIndex": 1, "endIndex": 14,
        "paragraph": {"elements": [
            {"startIndex": 1, "textRun": {"content": "ab"}},
            {"startIndex": 3, "inlineObjectElement": {"inlineObjectId": "kix.1"}},
            {"startIndex": 4, "textRun": {"content": "cd target\n"}},
        ]},
    }]
    assert find_matches(body, "target") == [(7, 13)]


def test_needle_spanning_a_paragraph_break_never_matches():
    body = [para(1, "first\n"), para(7, "second\n")]
    assert find_matches(body, "first\nsecond") == []
    assert find_matches(body, "st\nse") == []


def test_occurrences_in_document_order_and_selection():
    body = [para(1, "x and x\n"), para(9, "x again\n")]
    hits = find_matches(body, "x")
    assert hits == [(1, 2), (7, 8), (9, 10)]
    assert resolve(body, "x", None, 2)[0] == [(7, 8)]
    assert resolve(body, "x", None, 0)[0] == hits  # 0 = every occurrence

    with pytest.raises(RuntimeError, match="occurs 3 time"):
        resolve(body, "x", None, 4)
    with pytest.raises(RuntimeError, match="no match"):
        resolve(body, "absent", None, 1)
    with pytest.raises(RuntimeError, match="occurrence must be"):
        resolve(body, "x", None, -1)


def test_resolve_requires_exactly_one_locator():
    body = [para(1, "hi\n")]
    with pytest.raises(RuntimeError, match="exactly one"):
        resolve(body, None, None, 1)
    with pytest.raises(RuntimeError, match="exactly one"):
        resolve(body, "hi", "Heading", 1)


def test_occurrence_with_a_section_is_rejected_not_ignored():
    # A heading resolves to a single span, so an occurrence the caller believed in would
    # otherwise be silently dropped.
    with pytest.raises(RuntimeError, match="occurrence applies to match"):
        resolve(_sectioned(), None, "Intro", 2)


# ---- traversal -----------------------------------------------------------------------

def test_match_inside_a_table_cell_resolves_within_that_cell():
    cell_table = table(1, "alpha", "beta")
    body = [cell_table, para(cell_table["endIndex"], "tail\n")]
    (start, end), = find_matches(body, "beta")
    inner = cell_table["table"]["tableRows"][0]["tableCells"][1]["content"][0]
    assert (start, end) == (inner["startIndex"], inner["startIndex"] + 4)
    # wholly inside one cell: deleteContentRange rejects a range that straddles a cell boundary
    assert inner["startIndex"] <= start < end <= inner["endIndex"]


def test_resolver_descends_into_tables_while_content_to_text_skips_them():
    # Deliberate asymmetry: read_document(output_format='text') omits table text
    # (docs._content_to_text), but a locator can still target it. Asserted so it stays a
    # decision rather than drifting into an accident.
    from gdrive_mcp.tools.docs import _content_to_text

    body = [table(1, "in-cell")]
    assert _content_to_text(body) == ""
    assert find_matches(body, "in-cell") != []


# ---- sections ------------------------------------------------------------------------

def _sectioned() -> list:
    return [
        para(1, "Intro\n", style="HEADING_1"),
        para(7, "intro body\n"),
        para(18, "Sub\n", style="HEADING_2"),
        para(22, "sub body\n"),
        para(31, "Next\n", style="HEADING_1"),
        para(36, "next body\n"),
    ]


def test_section_ends_at_next_same_level_heading():
    body = _sectioned()
    assert find_section(body, "Intro") == (1, 31)  # spans the nested H2, stops at the next H1


def test_section_ends_at_next_higher_level_heading():
    body = _sectioned()
    assert find_section(body, "Sub") == (18, 31)  # H2 stops at the following H1


def test_section_runs_to_end_of_tab_when_last():
    body = _sectioned()
    assert find_section(body, "Next") == (31, body_end(body))


def test_section_is_clamped_to_body_end():
    body = _sectioned()
    _start, end = find_section(body, "Next")
    assert end == body[-1]["endIndex"] - 1  # the body's final newline is undeletable


def test_section_matches_the_heading_not_prose_with_the_same_text():
    body = [para(1, "Results\n"), para(9, "Results\n", style="HEADING_1"), para(17, "body\n")]
    assert find_section(body, "Results") == (9, body_end(body))


def test_missing_heading_error_lists_available_headings():
    with pytest.raises(RuntimeError, match="Intro"):
        find_section(_sectioned(), "Nope")


# ---- case sensitivity ------------------------------------------------------------------

def test_matching_is_case_sensitive_and_offers_no_folding_option():
    # Folding is deliberately absent: 'İ'.lower() is two characters, so any offset computed
    # from folded text would be shifted. Pinned here so it is not added without handling that.
    import inspect

    from gdrive_mcp.tools import docs as docs_mod

    body = [para(1, "Needle\n")]
    assert find_matches(body, "needle") == []
    assert find_matches(body, "Needle") == [(1, 7)]
    for tool in (docs_mod.delete_text, docs_mod.replace_text):
        assert "match_case" not in inspect.signature(tool).parameters


def test_empty_needle_is_rejected():
    with pytest.raises(RuntimeError, match="non-empty"):
        find_matches([para(1, "hi\n")], "")


# ---- slicing & boundaries ----------------------------------------------------------------

def test_text_in_range_slices_across_runs_and_astral_chars():
    body = [para(1, "ab😀", "cd\n")]
    assert text_in_range(body, 1, 7) == "ab😀cd"
    assert text_in_range(body, 3, 5) == "😀"


def test_paragraph_bounds_snaps_to_the_containing_paragraph():
    body = [para(1, "first\n"), para(7, "second\n")]
    assert paragraph_bounds(body, 9) == (7, 14)
    assert paragraph_bounds(body, 2) == (1, 7)


def test_paragraph_bounds_reports_no_boundary_inside_a_table():
    # Cell paragraphs are not top-level, so there is no boundary to snap to. Returning None
    # (rather than the raw index) is what lets the caller refuse instead of anchoring mid-cell.
    cell_table = table(1, "inside")
    body = [cell_table, para(cell_table["endIndex"], "after\n")]
    (start, _end), = find_matches(body, "inside")
    assert paragraph_bounds(body, start) is None


def test_section_ending_at_body_end_never_includes_the_final_newline():
    # A heading that is the last element has endIndex == body_end + 1; an unclamped max() would
    # return that and the API would reject the delete.
    body = [para(1, "Only\n", style="HEADING_1")]
    start, end = find_section(body, "Only")
    assert (start, end) == (1, body_end(body))
    assert end < body[-1]["endIndex"]


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-q"]))
