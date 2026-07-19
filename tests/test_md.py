"""Markdown dialect parser + Docs request builder (offsets are UTF-16 code units)."""

import pytest

from gdrive_mcp.md import parse_markdown, style_requests, u16len


def test_u16len_counts_utf16_units():
    assert u16len("abc") == 3
    assert u16len("😀") == 2  # astral-plane char: 2 UTF-16 units, len() says 1


def test_heading_and_body():
    p = parse_markdown("# Title\nbody")
    assert p.text == "Title\nbody"
    assert p.headings == [(0, 6, 1)]  # includes the trailing newline
    assert p.normal_runs == [(6, 10)]
    assert p.nonlist_runs == [(0, 10)]  # heading + body merge into one non-list run
    assert p.list_runs == [] and p.has_blocks


def test_heading_levels():
    p = parse_markdown("### Deep")
    assert p.headings == [(0, 4, 3)]
    assert parse_markdown("####### seven").headings == []  # 7 '#'s is not a heading


def test_bullets_nest_via_tabs():
    p = parse_markdown("- a\n  - b\n- c")
    assert p.text == "a\n\tb\nc"  # two-space indent -> one leading tab
    assert p.list_runs == [(0, 6, "bullet")]  # one contiguous run despite nesting
    assert p.list_items == 3 and p.normal_runs == [] and p.has_blocks


def test_numbered_list_and_mixed_runs_split():
    p = parse_markdown("1. x\n2) y")
    assert p.text == "x\ny" and p.list_runs == [(0, 3, "number")]
    mixed = parse_markdown("- a\n1. b")
    assert mixed.list_runs == [(0, 2, "bullet"), (2, 3, "number")]


def test_inline_styles_and_offsets():
    p = parse_markdown("**b** *i* <u>u</u>")
    assert p.text == "b i u"
    assert p.spans == [(0, 1, ("bold",)), (2, 3, ("italic",)), (4, 5, ("underline",))]
    assert not p.has_blocks  # inline-only markdown has no block constructs


def test_inline_nesting_and_bold_italic():
    p = parse_markdown("**b *i* b**")
    assert p.text == "b i b"
    assert p.spans == [(0, 5, ("bold",)), (2, 3, ("italic",))]  # overlapping outer + inner
    assert parse_markdown("***x***").spans == [(0, 1, ("bold", "italic"))]
    assert parse_markdown("__b__ and _i_").spans == [(0, 1, ("bold",)), (6, 7, ("italic",))]


def test_inline_offsets_are_utf16():
    p = parse_markdown("😀 **b**")
    assert p.text == "😀 b"
    assert p.spans == [(3, 4, ("bold",))]  # emoji occupies units 0-1


def test_markup_lookalikes_stay_literal():
    assert parse_markdown("snake_case_name").spans == []  # intraword '_' is not emphasis
    assert parse_markdown("2 * 3 * 4").spans == []  # space-padded '*' is not emphasis
    assert parse_markdown("**unclosed").text == "**unclosed"
    assert parse_markdown("*text*").list_runs == []  # no space after '*': emphasis, not bullet
    assert parse_markdown("* text").list_runs == [(0, 4, "bullet")]


def test_trailing_newline_makes_no_empty_run():
    p = parse_markdown("- a\n")
    assert p.text == "a\n"
    assert p.list_runs == [(0, 2, "bullet")]
    assert p.normal_runs == [] and p.nonlist_runs == []  # empty final line is skipped


def test_blank_line_splits_list_runs():
    p = parse_markdown("- a\n\n- b")
    assert p.text == "a\n\nb"
    assert p.list_runs == [(0, 2, "bullet"), (3, 4, "bullet")]
    assert p.normal_runs == [(2, 3)]  # the blank paragraph still gets normalized


def test_style_requests_order_offsets_and_tab():
    reqs = style_requests(parse_markdown("- a\n\n- b"), base=10, tab_id="t.1")
    kinds = [next(iter(r)) for r in reqs]
    # styles first, bullet creation last (it consumes nesting tabs, shifting later indexes)
    assert kinds == [
        "updateParagraphStyle",
        "deleteParagraphBullets",
        "createParagraphBullets",
        "createParagraphBullets",
    ]
    assert reqs[0]["updateParagraphStyle"]["range"] == {"startIndex": 12, "endIndex": 13, "tabId": "t.1"}
    creates = [r["createParagraphBullets"] for r in reqs[2:]]
    assert [c["range"]["startIndex"] for c in creates] == [13, 10]  # bottom-up
    assert creates[0]["bulletPreset"] == "BULLET_DISC_CIRCLE_SQUARE"


def test_style_requests_inline_only_never_touches_paragraphs():
    reqs = style_requests(parse_markdown("**b**"), base=5, tab_id=None)
    assert [next(iter(r)) for r in reqs] == ["updateTextStyle"]
    st = reqs[0]["updateTextStyle"]
    assert st["range"] == {"startIndex": 5, "endIndex": 6}
    assert st["textStyle"] == {"bold": True} and st["fields"] == "bold"


def test_style_requests_numbered_preset_and_heading_fields():
    reqs = style_requests(parse_markdown("# H\n1. one"), base=1, tab_id=None)
    by_kind = {next(iter(r)): r for r in reqs}
    para = by_kind["updateParagraphStyle"]["updateParagraphStyle"]
    assert para["paragraphStyle"] == {"namedStyleType": "HEADING_1"}
    assert para["fields"] == "namedStyleType"
    assert by_kind["createParagraphBullets"]["createParagraphBullets"]["bulletPreset"] == (
        "NUMBERED_DECIMAL_ALPHA_ROMAN"
    )


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-q"]))
