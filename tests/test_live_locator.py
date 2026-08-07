"""Live, credentialed checks L1-L10 from LOCATOR_WRITES_VERIFICATION.md §7.

These are the claims a stubbed service cannot falsify: that Docs indexes really are UTF-16, that
requiredRevisionId really rejects a stale write, that the final-newline clamp is load-bearing, and
that locator anchors really do satisfy insertTable's paragraph-boundary rule.

Skipped unless GDRIVE_MCP_LIVE=1. Creates ONE scratch Doc in the authenticated account's My Drive
and trashes it at the end. Content is synthetic — never point this at a real document.

    GDRIVE_MCP_LIVE=1 uv run pytest tests/test_live_locator.py -v
"""

import os

import pytest
from googleapiclient.errors import HttpError

from gdrive_mcp import locate
from gdrive_mcp.clients import docs, drive
from gdrive_mcp.tools import docs as docs_mod

pytestmark = pytest.mark.skipif(
    not os.environ.get("GDRIVE_MCP_LIVE"), reason="live run: set GDRIVE_MCP_LIVE=1"
)

# Distinctive synthetic tokens so every assertion targets an unambiguous span.
EMOJI_LINE = "pre a\U0001F600b NEEDLE post"
TRIPLE_LINE = "ZAP one ZAP two ZAP three"
BOLD_LINE = "hello **world** tail"


@pytest.fixture(scope="module")
def scratch():
    """One scratch Doc for the whole module; trashed on teardown."""
    made = docs_mod.create_document("gdrive-mcp locator live check (scratch)")
    did = made["id"]
    body = "\n".join([
        "# Alpha",
        "alpha body text",
        "## Beta",
        "beta body text",
        "# Gamma",
        EMOJI_LINE,
        TRIPLE_LINE,
        BOLD_LINE,
        "anchor paragraph",
    ])
    docs_mod.append_text(did, body, markdown=True)
    yield did
    drive().files().update(fileId=did, body={"trashed": True}).execute()


def _doc(did: str) -> dict:
    return docs().documents().get(documentId=did, includeTabsContent=True).execute()


def _body(did: str) -> list:
    # Empirical: a Doc created through the API comes back under `tabs`, with NO top-level `body`
    # key at all. _resolve_write_tab is what production uses to paper over both shapes.
    _tab_id, body = docs_mod._resolve_write_tab(_doc(did), None)
    return body


def _text(did: str) -> str:
    return docs_mod.read_document(did, output_format="text", max_chars=0)["content"]


# ---- L1 / L2: revision plumbing ---------------------------------------------------------

def test_L1_get_returns_a_revision_id(scratch):
    assert _doc(scratch).get("revisionId")
    assert docs_mod.read_document(scratch)["revision_id"]


def test_L2_stale_revision_is_rejected(scratch):
    stale = _doc(scratch)["revisionId"]
    # out-of-band edit: bumps the revision behind the resolver's back
    docs().documents().batchUpdate(
        documentId=scratch,
        body={"requests": [{"insertText": {"location": {"index": 1}, "text": "X"}}]},
    ).execute()
    before = _text(scratch)
    with pytest.raises(HttpError) as exc:
        docs().documents().batchUpdate(
            documentId=scratch,
            body={
                "requests": [{"insertText": {"location": {"index": 1}, "text": "SHOULD-NOT-LAND"}}],
                "writeControl": {"requiredRevisionId": stale},
            },
        ).execute()
    assert exc.value.resp.status in (400, 409)
    assert "SHOULD-NOT-LAND" not in _text(scratch)
    assert _text(scratch) == before  # the rejected batch changed nothing

    docs_mod.delete_text(scratch, match="X", confirm=True)  # undo the out-of-band edit


# ---- L3: the UTF-16 claim ----------------------------------------------------------------

def test_L3_astral_char_before_match_resolves_correctly(scratch):
    assert EMOJI_LINE in _text(scratch)
    out = docs_mod.delete_text(scratch, match="NEEDLE", confirm=True)
    assert out["chars"] == 6
    after = _text(scratch)
    # exactly the needle went; the emoji and both neighbours survive intact
    assert "pre a\U0001F600b  post" in after
    assert "NEEDLE" not in after


# ---- L4: descending multi-range delete ---------------------------------------------------

def test_L4_all_occurrences_delete_in_one_batch(scratch):
    out = docs_mod.delete_text(scratch, match="ZAP ", occurrence=0, confirm=True)
    assert out["occurrences"] == 3
    after = _text(scratch)
    assert "ZAP" not in after
    assert "one two three" in after  # surrounding text intact, no double-shift damage


# ---- L10: match spanning two text runs ---------------------------------------------------

def test_L10_match_across_a_bold_boundary(scratch):
    assert "hello world tail" in _text(scratch)
    docs_mod.delete_text(scratch, match="lo wor", confirm=True)  # spans plain -> bold runs
    assert "helld tail" in _text(scratch)


# ---- L9 / L5: table anchoring and in-cell edits -------------------------------------------

def test_L9_locator_anchor_satisfies_insert_table(scratch):
    out = docs_mod.insert_table(scratch, [["CELLONE", "CELLTWO"]], after="anchor paragraph")
    assert out["cells_filled"] == 2
    assert "CELLONE" in docs_mod.read_document(scratch, max_chars=0)["content"]


def test_L5_delete_inside_a_table_cell_keeps_the_table(scratch):
    before = sum(1 for el in _body(scratch) if "table" in el)
    docs_mod.delete_text(scratch, match="ONE", confirm=True)
    tables = [el for el in _body(scratch) if "table" in el]
    assert len(tables) == before
    rows = tables[-1]["table"]["tableRows"]
    assert len(rows) == 1 and len(rows[0]["tableCells"]) == 2  # structure survived
    md = docs_mod.read_document(scratch, max_chars=0)["content"]
    assert "CELL" in md and "CELLONE" not in md


# ---- L7: the final-newline clamp ----------------------------------------------------------

def test_L7_unclamped_range_through_final_newline_is_rejected(scratch):
    body = _body(scratch)
    unclamped = body[-1]["endIndex"]
    with pytest.raises(HttpError) as exc:
        docs().documents().batchUpdate(
            documentId=scratch,
            body={"requests": [{"deleteContentRange": {
                "range": {"startIndex": unclamped - 2, "endIndex": unclamped}}}]},
        ).execute()
    assert exc.value.resp.status == 400
    # The clamp is exactly one below the rejected bound, so body_end() is the last legal index.
    # That the clamped side is *accepted* is shown by every other delete in this module, which
    # all resolve through body_end and succeed.
    assert locate.body_end(body) == unclamped - 1


# ---- L6 / L8: sections and atomic replace --------------------------------------------------

def test_L8_replace_with_markdown_lands_styles_on_new_text(scratch):
    docs_mod.replace_text(scratch, "**BOLDED**", match="beta body text", markdown=True, confirm=True)
    body = _body(scratch)
    runs = [
        pe["textRun"]
        for el in body if "paragraph" in el
        for pe in el["paragraph"]["elements"] if "textRun" in pe
    ]
    hit = next(r for r in runs if "BOLDED" in r["content"])
    assert hit["textStyle"].get("bold") is True
    assert "beta body text" not in _text(scratch)


def test_L6_section_delete_leaves_no_orphan_paragraph(scratch):
    before = _text(scratch)
    assert "Beta" in before
    docs_mod.delete_text(scratch, section="Beta", confirm=True)
    after = _text(scratch)
    assert "Beta" not in after and "BOLDED" not in after  # heading AND its content
    assert "\n\n" not in after.strip()  # no blank paragraph left behind
    assert "Gamma" in after  # the following section is untouched
