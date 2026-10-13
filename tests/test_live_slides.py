"""Live, credentialed checks SL1-SL14 for the Slides tools.

Skipped unless GDRIVE_MCP_LIVE=1 and GDRIVE_MCP_LIVE_FOLDER names a Drive folder you may write to.
The test creates one scratch presentation *inside that folder* and deletes it in ``finally``; the
id is printed first so a failed cleanup is recoverable by hand. Comments only ever assign to or mention
the signed-in user's own address, so nobody else is emailed. It does not call
`create_presentation`, which always writes to the My Drive root rather than the sandbox folder.

    GDRIVE_MCP_LIVE=1 GDRIVE_MCP_LIVE_FOLDER=<folder id> uv run pytest tests/test_live_slides.py -v -s
"""

import os

import pytest

from gdrive_mcp.clients import authed_user_email, drive, slides
from gdrive_mcp.tools import slides as slides_mod

live_only = pytest.mark.skipif(
    not (os.environ.get("GDRIVE_MCP_LIVE") and os.environ.get("GDRIVE_MCP_LIVE_FOLDER")),
    reason="live run: set GDRIVE_MCP_LIVE=1 and GDRIVE_MCP_LIVE_FOLDER=<folder id>",
)


def _deck(pid: str) -> dict:
    return slides().presentations().get(presentationId=pid).execute()


def _slide_text(deck: dict, number: int) -> str:
    return slides_mod._slide_text(deck["slides"][number - 1])


def _layout_text(deck: dict) -> str:
    return "\n".join(
        t
        for page in [*deck.get("layouts", []), *deck.get("masters", [])]
        for el in slides_mod._walk(page.get("pageElements", []))
        if (t := slides_mod._text_of(el.get("shape", {}).get("text")))
    )


@live_only
def test_slides_tools_hold_against_the_real_slides_api():
    folder = os.environ["GDRIVE_MCP_LIVE_FOLDER"]
    made = (
        drive()
        .files()
        .create(
            body={
                "name": "gdrive-mcp slides live check (auto-deleted)",
                "mimeType": "application/vnd.google-apps.presentation",
                "parents": [folder],
            },
            fields="id",
        )
        .execute()
    )
    pid = made["id"]
    print(f"scratch presentation: {pid}")

    try:
        # SL1: read returns the starting slide with addressable element ids.
        first = slides_mod.read_presentation(pid)
        assert first["slide_count"] == 1
        s1 = first["outline"][0]["object_id"]
        els = [e for e in slides_mod._walk(_deck(pid)["slides"][0]["pageElements"]) if "shape" in e]
        assert els, "SL1: starting slide has no shapes"
        target = els[0]["objectId"]
        assert f"id={target}" in first["content"]

        # SL2: setting text on an empty shape needs no confirm and lands.
        slides_mod.set_shape_text(pid, target, "Acme roadmap")
        assert "Acme roadmap" in _slide_text(_deck(pid), 1)

        # SL3: overwriting is gated, the gate writes nothing, confirm=true writes.
        gate = slides_mod.set_shape_text(pid, target, "Globex roadmap")
        assert gate["status"] == "confirmation_required"
        assert "Acme roadmap" in _slide_text(_deck(pid), 1), "SL3: unconfirmed overwrite wrote"
        slides_mod.set_shape_text(pid, target, "Globex roadmap", confirm=True)
        text = _slide_text(_deck(pid), 1)
        assert "Globex roadmap" in text and "Acme roadmap" not in text

        # SL4: add_slide at position 2 fills the title and body placeholders.
        added = slides_mod.add_slide(pid, "TITLE_AND_BODY", title="Acme risks", body="Acme perf", position=2)
        assert added["filled"] == ["title", "body"], added
        deck = _deck(pid)
        assert deck["slides"][1]["objectId"] == added["slide_id"]
        assert "Acme risks" in _slide_text(deck, 2) and "Acme perf" in _slide_text(deck, 2)

        # SL5: replace_slide_text previews, then changes exactly what it counted, and never
        # touches layouts or masters (which replaceAllText would, if not given page ids).
        before_layouts = _layout_text(deck)
        preview = slides_mod.replace_slide_text(pid, "Acme", "Initech")
        assert preview["status"] == "confirmation_required" and preview["impact"]["occurrences"] == 2
        done = slides_mod.replace_slide_text(pid, "Acme", "Initech", confirm=True)
        assert done["occurrences_changed"] == preview["impact"]["occurrences"]
        deck = _deck(pid)
        assert "Acme" not in _slide_text(deck, 2) and "Initech risks" in _slide_text(deck, 2)
        assert _layout_text(deck) == before_layouts, "SL5: replace_slide_text altered layouts/masters"

        # SL6: speaker notes insert freely, then overwrite only with confirm.
        slides_mod.set_speaker_notes(pid, "1", "first notes")
        assert slides_mod.set_speaker_notes(pid, "1", "second notes")["status"] == "confirmation_required"
        slides_mod.set_speaker_notes(pid, "1", "second notes", confirm=True)
        assert "Notes: second notes" in slides_mod.read_presentation(pid, slide="1")["content"]

        # SL7: duplicate_slide puts the copy immediately after the original.
        dup = slides_mod.duplicate_slide(pid, "1")
        deck = _deck(pid)
        assert [s["objectId"] for s in deck["slides"]][:2] == [s1, dup["new_slide_id"]]
        assert _slide_text(deck, 2) == _slide_text(deck, 1)

        # SL8: move_slide lands the slide at the requested final position, in both directions.
        order = [s["objectId"] for s in _deck(pid)["slides"]]  # [s1, copy, added]
        slides_mod.move_slide(pid, order[0], 3)
        assert [s["objectId"] for s in _deck(pid)["slides"]] == [order[1], order[2], order[0]], "SL8: later"
        slides_mod.move_slide(pid, order[0], 1)
        assert [s["objectId"] for s in _deck(pid)["slides"]] == order, "SL8: earlier"

        # SL9: delete_slide is gated and then removes exactly that slide.
        assert slides_mod.delete_slide(pid, dup["new_slide_id"])["status"] == "confirmation_required"
        assert len(_deck(pid)["slides"]) == 3, "SL9: unconfirmed delete wrote"
        slides_mod.delete_slide(pid, dup["new_slide_id"], confirm=True)
        assert [s["objectId"] for s in _deck(pid)["slides"]] == [order[0], order[2]]

        # SL10: a table cell is addressable by row/column.
        slides().presentations().batchUpdate(
            presentationId=pid,
            body={"requests": [{"createTable": {"objectId": "live_table_1", "rows": 2, "columns": 2,
                                                "elementProperties": {"pageObjectId": order[0]}}}]},
        ).execute()
        slides_mod.set_shape_text(pid, "live_table_1", "cell text", row=1, column=0)
        cells = {(r, c): t for r, c, t in slides_mod._table_cells(
            next(e for e in slides_mod._walk(_deck(pid)["slides"][0]["pageElements"]) if e["objectId"] == "live_table_1"))}
        assert cells[(1, 0)] == "cell text" and cells[(0, 0)] == ""

        # SL11: a plain comment needs no confirm and shows up under its slide on read-back.
        slides_mod.add_slide_comment(pid, "1", "plain live comment")
        assert "plain live comment" in slides_mod.read_presentation(pid, slide="1")["content"]

        # SL12: assigning is gated (posts nothing), then lands as a real assignment that Slides
        # and Drive both report, on a comment pinned to the slide.
        me = authed_user_email()
        gate = slides_mod.add_slide_comment(pid, "1", "assigned live comment", assignee=me)
        assert gate["status"] == "confirmation_required"
        assert "assigned live comment" not in slides_mod.read_presentation(pid, slide="1")["content"], "SL12: gate wrote"
        posted = slides_mod.add_slide_comment(pid, "1", "assigned live comment", assignee=me, confirm=True)
        assert posted["assigned_to"] == me, posted
        shown = slides_mod.read_presentation(pid, slide="1")["content"]
        assert f"-> assigned to {me}: +{me} assigned live comment" in shown
        drive_view = {
            c["id"]: c
            for c in drive().comments().list(fileId=pid, fields="comments(id,assigneeEmailAddress,mentionedEmailAddresses)").execute()["comments"]
        }
        assert drive_view[posted["comment_id"]]["assigneeEmailAddress"] == me
        assert drive_view[posted["comment_id"]]["mentionedEmailAddresses"] == [me]

        # SL13: a comment pinned to one element is listed under that slide.
        slides_mod.add_slide_comment(pid, "1", "element live comment", element=target)
        assert "element live comment" in slides_mod.read_presentation(pid, slide="1")["content"]

        # SL14: a name instead of an email is refused and nothing is posted.
        before = slides_mod.read_presentation(pid)["content"].count("id=")
        with pytest.raises(RuntimeError, match="email address"):
            slides_mod.add_slide_comment(pid, "1", "x", assignee="@Sam", confirm=True)
        assert slides_mod.read_presentation(pid)["content"].count("id=") == before
    finally:
        drive().files().delete(fileId=pid).execute()
        print(f"deleted scratch presentation {pid}")
