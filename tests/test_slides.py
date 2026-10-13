"""Slides tool tests with a stubbed Slides service: the confirm gates and the exact requests built."""

from unittest.mock import MagicMock

import pytest

from gdrive_mcp.tools import slides as sl

PID = "P" * 30


def _text(content: str) -> dict:
    return {"textElements": [{"textRun": {"content": content}}]}


def _shape(oid: str, text: str, placeholder: str | None = None) -> dict:
    shape: dict = {"text": _text(text + "\n" if text else "\n")}
    if placeholder:
        shape["placeholder"] = {"type": placeholder}
    return {"objectId": oid, "shape": shape}


def _table(oid: str, grid: list[list[str]]) -> dict:
    rows = [{"tableCells": [{"text": _text(c + "\n" if c else "\n")} for c in row]} for row in grid]
    return {"objectId": oid, "table": {"rows": len(grid), "columns": len(grid[0]), "tableRows": rows}}


def _slide(oid: str, elements: list, notes: str | None = None, notes_id: str = "n_shape") -> dict:
    notes_els = [_shape(notes_id, notes)] if notes is not None else []
    return {
        "objectId": oid,
        "pageElements": elements,
        "slideProperties": {
            "layoutObjectId": "lay1",
            "notesPage": {"pageElements": notes_els, "notesProperties": {"speakerNotesObjectId": notes_id}},
        },
    }


def _deck() -> dict:
    return {
        "title": "Q3 review",
        "revisionId": "rev-1",
        "layouts": [{"objectId": "lay1", "layoutProperties": {"displayName": "Title and body"}}],
        "slides": [
            _slide("s1", [_shape("t1", "Roadmap", "TITLE"), _shape("b1", "Ship Acme v2\nAcme tests", "BODY")], notes="Say hi"),
            _slide("s2", [_shape("t2", "Risks", "TITLE"), _table("tab2", [["Acme", "Owner"], ["Perf", ""]])]),
            _slide("s3", [_shape("t3", "Next steps", "TITLE"), _shape("e3", "")]),
        ],
    }


@pytest.fixture
def svc(monkeypatch):
    s = MagicMock()
    s.presentations.return_value.get.return_value.execute.return_value = _deck()
    monkeypatch.setattr(sl, "slides", lambda: s)
    return s


def _requests(svc) -> list:
    return svc.presentations.return_value.batchUpdate.call_args.kwargs["body"]["requests"]


def _wrote(svc) -> bool:
    return svc.presentations.return_value.batchUpdate.called


# ---- read ---------------------------------------------------------------------------------

def test_read_presentation_renders_ids_tables_notes_and_an_outline(svc):
    out = sl.read_presentation(PID)
    assert out["slide_count"] == 3
    assert out["outline"][1] == {"number": 2, "object_id": "s2", "title": "Risks"}
    assert out["layouts"] == [{"object_id": "lay1", "name": "Title and body"}]
    assert out["revision_id"] == "rev-1"
    c = out["content"]
    assert "## Slide 1 · id=s1 · layout=Title and body" in c
    assert "- [BODY] id=b1: Ship Acme v2\n    Acme tests" in c
    assert "- [TABLE] id=tab2 2x2" in c and "r1: Perf | " in c
    assert "- [SHAPE] id=e3: (empty)" in c
    assert "Notes: Say hi" in c


def test_read_presentation_single_slide_by_number_or_id(svc):
    by_number = sl.read_presentation(PID, slide="2")["content"]
    by_id = sl.read_presentation(PID, slide="s2")["content"]
    assert by_number == by_id
    assert "Slide 2" in by_number and "Slide 1" not in by_number


def test_unknown_or_out_of_range_slide_is_refused(svc):
    with pytest.raises(RuntimeError, match="out of range"):
        sl.read_presentation(PID, slide="9")
    with pytest.raises(RuntimeError, match="no slide with id"):
        sl.read_presentation(PID, slide="nope")


# ---- replace_slide_text -------------------------------------------------------------------

def test_replace_slide_text_counts_shapes_and_table_cells_per_slide_and_gates(svc):
    out = sl.replace_slide_text(PID, "Acme", "Globex")
    assert out["status"] == "confirmation_required"
    assert out["impact"]["occurrences"] == 3
    assert out["impact"]["slides"] == [
        {"slide_number": 1, "slide_id": "s1", "occurrences": 2},
        {"slide_number": 2, "slide_id": "s2", "occurrences": 1},
    ]
    assert not _wrote(svc)


def test_replace_slide_text_names_the_slides_so_layouts_masters_and_notes_are_untouched(svc):
    svc.presentations.return_value.batchUpdate.return_value.execute.return_value = {
        "replies": [{"replaceAllText": {"occurrencesChanged": 3}}]
    }
    out = sl.replace_slide_text(PID, "Acme", "Globex", confirm=True)
    req = _requests(svc)[0]["replaceAllText"]
    assert req["pageObjectIds"] == ["s1", "s2", "s3"]  # omitted => the API also rewrites layouts/masters
    assert req["containsText"] == {"text": "Acme", "matchCase": True}
    assert req["replaceText"] == "Globex"
    assert out["occurrences_changed"] == 3
    assert svc.presentations.return_value.batchUpdate.call_args.kwargs["body"]["writeControl"] == {
        "requiredRevisionId": "rev-1"
    }


def test_replace_slide_text_scoped_to_one_slide_sends_only_that_page(svc):
    svc.presentations.return_value.batchUpdate.return_value.execute.return_value = {"replies": [{}]}
    sl.replace_slide_text(PID, "Acme", "Globex", slide="2", confirm=True)
    assert _requests(svc)[0]["replaceAllText"]["pageObjectIds"] == ["s2"]


def test_replace_slide_text_dry_run_neither_writes_nor_gates(svc):
    out = sl.replace_slide_text(PID, "Acme", "Globex", dry_run=True)
    assert out["dry_run"] is True and out["occurrences"] == 3
    assert not _wrote(svc)


def test_replace_slide_text_unmatched_or_multiparagraph_match_raises_before_writing(svc):
    with pytest.raises(RuntimeError, match="no match"):
        sl.replace_slide_text(PID, "acme", "x", confirm=True)  # case-sensitive
    with pytest.raises(RuntimeError, match="span paragraphs"):
        sl.replace_slide_text(PID, "a\nb", "x", confirm=True)
    with pytest.raises(RuntimeError, match="non-empty"):
        sl.replace_slide_text(PID, "", "x", confirm=True)
    assert not _wrote(svc)


# ---- set_shape_text -----------------------------------------------------------------------

def test_set_shape_text_overwrite_gates_then_deletes_all_and_inserts(svc):
    out = sl.set_shape_text(PID, "t1", "Plan")
    assert out["status"] == "confirmation_required"
    assert out["impact"]["overwrites_text"] == "Roadmap"
    assert not _wrote(svc)
    sl.set_shape_text(PID, "t1", "Plan", confirm=True)
    assert _requests(svc) == [
        {"deleteText": {"objectId": "t1", "textRange": {"type": "ALL"}}},
        {"insertText": {"objectId": "t1", "insertionIndex": 0, "text": "Plan"}},
    ]


def test_set_shape_text_on_an_empty_shape_needs_no_confirm_and_sends_no_delete(svc):
    # deleteText on empty text is rejected by Slides, so it must not be sent.
    out = sl.set_shape_text(PID, "e3", "Hello")
    assert out["updated"] is True
    assert _requests(svc) == [{"insertText": {"objectId": "e3", "insertionIndex": 0, "text": "Hello"}}]


def test_set_shape_text_clearing_sends_only_a_delete(svc):
    sl.set_shape_text(PID, "t1", "", confirm=True)
    assert _requests(svc) == [{"deleteText": {"objectId": "t1", "textRange": {"type": "ALL"}}}]


def test_set_shape_text_same_text_is_a_noop(svc):
    out = sl.set_shape_text(PID, "t1", "Roadmap")
    assert out["unchanged"] is True and not _wrote(svc)


def test_set_shape_text_targets_a_table_cell_by_row_and_column(svc):
    sl.set_shape_text(PID, "tab2", "Sam", row=1, column=1)  # empty cell: no confirm needed
    assert _requests(svc) == [
        {"insertText": {"objectId": "tab2", "cellLocation": {"rowIndex": 1, "columnIndex": 1},
                        "insertionIndex": 0, "text": "Sam"}}
    ]


def test_set_shape_text_validates_the_target(svc):
    with pytest.raises(RuntimeError, match="is a table"):
        sl.set_shape_text(PID, "tab2", "x")
    with pytest.raises(RuntimeError, match="no cell at row 5"):
        sl.set_shape_text(PID, "tab2", "x", row=5, column=0)
    with pytest.raises(RuntimeError, match="not a table"):
        sl.set_shape_text(PID, "t1", "x", row=0, column=0)
    with pytest.raises(RuntimeError, match="no page element"):
        sl.set_shape_text(PID, "ghost", "x")
    assert not _wrote(svc)


def test_set_shape_text_dry_run_does_not_write(svc):
    out = sl.set_shape_text(PID, "t1", "Plan", dry_run=True)
    assert out["dry_run"] is True and not _wrote(svc)


# ---- speaker notes ------------------------------------------------------------------------

def test_set_speaker_notes_gates_when_notes_exist_and_targets_the_notes_shape(svc):
    assert sl.set_speaker_notes(PID, "1", "New")["status"] == "confirmation_required"
    assert not _wrote(svc)
    sl.set_speaker_notes(PID, "1", "New", confirm=True)
    assert _requests(svc) == [
        {"deleteText": {"objectId": "n_shape", "textRange": {"type": "ALL"}}},
        {"insertText": {"objectId": "n_shape", "insertionIndex": 0, "text": "New"}},
    ]


def test_set_speaker_notes_on_a_slide_without_notes_inserts_without_confirm(svc):
    out = sl.set_speaker_notes(PID, "3", "First notes")
    assert out["updated"] is True
    assert _requests(svc) == [{"insertText": {"objectId": "n_shape", "insertionIndex": 0, "text": "First notes"}}]


# ---- delete / duplicate / move ------------------------------------------------------------

def test_delete_slide_gates_and_previews_then_deletes_the_page(svc):
    out = sl.delete_slide(PID, "2")
    assert out["status"] == "confirmation_required"
    assert out["impact"]["title"] == "Risks" and out["impact"]["elements"] == 2
    assert "Acme" in out["impact"]["deletes_text"]
    assert not _wrote(svc)
    sl.delete_slide(PID, "2", confirm=True)
    assert _requests(svc) == [{"deleteObject": {"objectId": "s2"}}]


def test_delete_slide_dry_run_does_not_write(svc):
    assert sl.delete_slide(PID, "2", dry_run=True)["dry_run"] is True
    assert not _wrote(svc)


def test_duplicate_slide_returns_the_new_id_placed_after_the_original(svc):
    svc.presentations.return_value.batchUpdate.return_value.execute.return_value = {
        "replies": [{"duplicateObject": {"objectId": "s1_copy"}}]
    }
    out = sl.duplicate_slide(PID, "s1")
    assert _requests(svc) == [{"duplicateObject": {"objectId": "s1"}}]
    assert out["new_slide_id"] == "s1_copy" and out["new_slide_number"] == 2


def test_duplicate_and_move_dry_runs_report_the_plan_without_writing(svc):
    dup = sl.duplicate_slide(PID, "2", dry_run=True)
    assert dup["dry_run"] is True and dup["slide_id"] == "s2" and dup["title"] == "Risks"
    mv = sl.move_slide(PID, "3", 1, dry_run=True)
    assert mv["dry_run"] is True and (mv["from"], mv["to"]) == (3, 1)
    assert not _wrote(svc)


@pytest.mark.parametrize(
    "slide, position, insertion",
    [
        ("1", 3, 3),  # moving later: index is in the pre-move arrangement, so one past the target
        ("3", 1, 0),  # moving earlier: the target index itself
        ("1", 2, 2),
        ("2", 1, 0),
    ],
)
def test_move_slide_translates_the_final_position_to_a_pre_move_insertion_index(svc, slide, position, insertion):
    sl.move_slide(PID, slide, position)
    (req,) = _requests(svc)
    assert req["updateSlidesPosition"]["insertionIndex"] == insertion


def test_move_slide_to_its_own_position_is_a_noop_and_range_is_checked(svc):
    assert sl.move_slide(PID, "2", 2)["moved"] is False
    assert not _wrote(svc)
    with pytest.raises(RuntimeError, match="out of range"):
        sl.move_slide(PID, "1", 4)


# ---- create / add_slide -------------------------------------------------------------------

def test_create_presentation_returns_the_first_slide_id(svc):
    svc.presentations.return_value.create.return_value.execute.return_value = {
        "presentationId": "NEW", "title": "T", "slides": [{"objectId": "p"}]
    }
    out = sl.create_presentation("T")
    assert svc.presentations.return_value.create.call_args.kwargs["body"] == {"title": "T"}
    assert out["url"] == "https://docs.google.com/presentation/d/NEW/edit" and out["first_slide_id"] == "p"


def _add_slide_svc(svc, page_elements):
    svc.presentations.return_value.batchUpdate.return_value.execute.return_value = {
        "replies": [{"createSlide": {"objectId": "newslide"}}]
    }
    svc.presentations.return_value.pages.return_value.get.return_value.execute.return_value = {
        "pageElements": page_elements
    }


def test_add_slide_predefined_layout_at_a_position_fills_title_and_body_placeholders(svc):
    _add_slide_svc(svc, [_shape("nt", "", "TITLE"), _shape("nb", "", "BODY")])
    out = sl.add_slide(PID, "title_and_body", title="Hi", body="There", position=2)
    first = svc.presentations.return_value.batchUpdate.call_args_list[0].kwargs["body"]["requests"]
    assert first == [{"createSlide": {"slideLayoutReference": {"predefinedLayout": "TITLE_AND_BODY"}, "insertionIndex": 1}}]
    assert _requests(svc) == [
        {"insertText": {"objectId": "nt", "insertionIndex": 0, "text": "Hi"}},
        {"insertText": {"objectId": "nb", "insertionIndex": 0, "text": "There"}},
    ]
    assert out["slide_id"] == "newslide" and out["filled"] == ["title", "body"] and "unfilled" not in out
    # a predefined layout needs no read of the deck first
    assert not svc.presentations.return_value.get.called


def test_add_slide_reports_unfilled_instead_of_failing_after_the_slide_was_created(svc):
    _add_slide_svc(svc, [_shape("nt", "", "TITLE")])  # TITLE_ONLY has no body placeholder
    out = sl.add_slide(PID, "TITLE_ONLY", title="Hi", body="Lost")
    assert out["filled"] == ["title"] and out["unfilled"] == ["body"]


def test_add_slide_accepts_a_deck_layout_by_name_and_rejects_unknown_ones(svc):
    _add_slide_svc(svc, [])
    sl.add_slide(PID, "title and body")
    first = svc.presentations.return_value.batchUpdate.call_args_list[0].kwargs["body"]["requests"]
    assert first[0]["createSlide"]["slideLayoutReference"] == {"layoutId": "lay1"}
    with pytest.raises(RuntimeError, match="unknown layout"):
        sl.add_slide(PID, "Nonexistent")


def test_add_slide_dry_run_and_position_validation(svc):
    out = sl.add_slide(PID, "BLANK", dry_run=True)
    assert out["dry_run"] is True and not _wrote(svc)
    with pytest.raises(RuntimeError, match=">= 1"):
        sl.add_slide(PID, "BLANK", position=0)


# ---- comments -----------------------------------------------------------------------------

SAM = "sam@company.com"
KAI = "kai@company.com"


def _thread(cid, content, anchor, assignee=None, status="OPEN", replies=(), deleted=False):
    head = {"content": content, "author": {"displayName": "Pat"}, "deleted": deleted}
    if assignee:
        head["assigneeEmail"] = assignee
    return {"commentId": cid, "anchorId": anchor, "status": status, "headPost": head, "replies": list(replies)}


def _posted(svc, assignee=None):
    head = {"assigneeEmail": assignee} if assignee else {}
    svc.presentations.return_value.batchUpdate.return_value.execute.return_value = {
        "replies": [{"insertComment": {"commentThread": {"commentId": "c_new", "headPost": head}}}]
    }


def test_a_plain_comment_needs_no_confirm_and_targets_the_slide(svc):
    _posted(svc)
    out = sl.add_slide_comment(PID, "2", "Looks good")
    assert _requests(svc) == [{"insertComment": {"content": "Looks good", "objectId": "s2"}}]
    assert out["posted"] is True and out["comment_id"] == "c_new" and out["assigned_to"] is None


def test_assigning_gates_and_previews_who_would_be_emailed_without_posting(svc):
    out = sl.add_slide_comment(PID, "1", "Please review", assignee=SAM)
    assert out["status"] == "confirmation_required"
    assert out["impact"]["notifies_by_email"] == [SAM] and out["impact"]["assigns"] == SAM
    assert not _wrote(svc)


def test_confirmed_assignment_sends_the_assignee_and_mentions_them_in_the_text(svc):
    _posted(svc, assignee=SAM)
    out = sl.add_slide_comment(PID, "1", "Please review", assignee=SAM, confirm=True)
    assert _requests(svc) == [
        {"insertComment": {"content": f"+{SAM} Please review", "objectId": "s1", "assigneeEmailAddress": SAM}}
    ]
    assert out["assigned_to"] == SAM


def test_assigned_to_reports_what_slides_returned_not_what_was_requested(svc):
    _posted(svc)  # the API echoes no assignee
    out = sl.add_slide_comment(PID, "1", "x", assignee=SAM, confirm=True)
    assert out["assigns"] == SAM and out["assigned_to"] is None


def test_a_mention_alone_notifies_and_gates_but_does_not_assign(svc):
    assert sl.add_slide_comment(PID, "1", "fyi", mention=KAI)["status"] == "confirmation_required"
    _posted(svc)
    sl.add_slide_comment(PID, "1", "fyi", mention=KAI, confirm=True)
    (req,) = _requests(svc)
    assert req["insertComment"]["content"] == f"+{KAI} fyi"
    assert "assigneeEmailAddress" not in req["insertComment"]


def test_assignee_and_a_second_mention_are_each_mentioned_once(svc):
    _posted(svc, assignee=SAM)
    sl.add_slide_comment(PID, "1", f"+{SAM} see this", assignee=SAM, mention=KAI, confirm=True)
    assert _requests(svc)[0]["insertComment"]["content"] == f"+{KAI} +{SAM} see this"


def test_a_name_instead_of_an_email_is_refused_before_any_io(svc):
    for bad in ("@Sam", "Sam", "sam@", "sam@company", "a b@c.com"):
        with pytest.raises(RuntimeError, match="email address"):
            sl.add_slide_comment(PID, "1", "x", assignee=bad, confirm=True)
    with pytest.raises(RuntimeError, match="email address"):
        sl.add_slide_comment(PID, "1", "x", mention="@Kai")
    assert not svc.presentations.return_value.get.called and not _wrote(svc)


def test_element_pins_the_comment_to_that_element_and_must_be_on_the_slide(svc):
    _posted(svc)
    sl.add_slide_comment(PID, "1", "check this", element="b1")
    assert _requests(svc)[0]["insertComment"]["objectId"] == "b1"
    with pytest.raises(RuntimeError, match="no page element"):
        sl.add_slide_comment(PID, "1", "x", element="t2")  # exists, but on slide 2


def test_comment_dry_run_previews_without_writing_or_gating(svc):
    out = sl.add_slide_comment(PID, "1", "x", assignee=SAM, dry_run=True)
    assert out["dry_run"] is True and out["assigns"] == SAM and not _wrote(svc)


def test_empty_or_oversize_comments_are_refused_counting_the_added_mentions(svc):
    with pytest.raises(RuntimeError, match="non-empty"):
        sl.add_slide_comment(PID, "1", "   ")
    with pytest.raises(RuntimeError, match="2048 bytes"):
        sl.add_slide_comment(PID, "1", "é" * 1030, assignee=SAM, confirm=True)  # 2060 bytes + mention
    assert not _wrote(svc)


def test_read_presentation_shows_threads_on_their_slide_and_unanchored_ones_last(svc):
    deck = _deck()
    deck["slides"][0]["commentAnchors"] = [{"anchorId": "A1"}, {"anchorId": "A2"}]
    deck["comments"] = [
        _thread("c1", "Tighten this", "A1", assignee=SAM, replies=[{"content": "Done", "author": {"displayName": "Lee"}}]),
        _thread("c2", "On a shape", "A2", status="RESOLVED"),
        _thread("c3", "Drive-API comment", None),
        _thread("c4", "gone", "A1", deleted=True),
    ]
    svc.presentations.return_value.get.return_value.execute.return_value = deck
    content = sl.read_presentation(PID)["content"]
    slide1 = content.split("## Slide 2")[0]
    assert f"- [OPEN] id=c1 Pat -> assigned to {SAM}: Tighten this" in slide1
    assert "reply from Lee: Done" in slide1 and "- [RESOLVED] id=c2" in slide1
    assert "gone" not in content
    assert content.rstrip().endswith("## Unanchored comments\n- [OPEN] id=c3 Pat: Drive-API comment")
    # a single-slide read shows only that slide's threads, not the unanchored ones
    only = sl.read_presentation(PID, slide="1")["content"]
    assert "c1" in only and "Unanchored" not in only


def test_only_read_presentation_asks_for_comments_and_it_does_so_in_the_view_only_safe_mode(svc):
    # Comments are omitted from presentations.get unless requested, and INCLUDED is a 403 for a
    # user who cannot view comments; DEFAULT_FOR_CURRENT_ACCESS is the mode that degrades safely.
    get = svc.presentations.return_value.get
    sl.read_presentation(PID)
    assert get.call_args.kwargs["commentsViewMode"] == "COMMENTS_VIEW_MODE_DEFAULT_FOR_CURRENT_ACCESS"
    get.reset_mock()
    sl.delete_slide(PID, "1")
    assert "commentsViewMode" not in get.call_args.kwargs
