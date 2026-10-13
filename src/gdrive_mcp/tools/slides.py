"""Google Slides tools: read, create, add/duplicate/move/delete slides, rewrite text, speaker notes.

Every tool references its target presentation via `item` (a URL or ID). Content is addressed by
the stable `objectId`s that `read_presentation` returns, or by a slide's 1-based number — never by
a character offset.
"""

from __future__ import annotations

import re
from collections.abc import Iterator

from gdrive_mcp.chunking import DEFAULT_MAX_CHARS, paginate
from gdrive_mcp.clients import slides
from gdrive_mcp.errors import api_errors
from gdrive_mcp.guard import preview_response
from gdrive_mcp.ids import parse_ref

_PREVIEW_CHARS = 200

# createSlide's predefinedLayout values, minus PREDEFINED_LAYOUT_UNSPECIFIED.
_PREDEFINED_LAYOUTS = (
    "BLANK", "CAPTION_ONLY", "TITLE", "TITLE_AND_BODY", "TITLE_AND_TWO_COLUMNS", "TITLE_ONLY",
    "SECTION_HEADER", "SECTION_TITLE_AND_DESCRIPTION", "ONE_COLUMN_TEXT", "MAIN_POINT", "BIG_NUMBER",
)
# Slides rejects an assignee that is not a well-formed address, and cannot resolve a name to one.
_EMAIL = re.compile(r"^[^@\s,;<>]+@[^@\s,;<>]+\.[^@\s,;<>]+$")
_COMMENT_MAX_BYTES = 2048  # insertComment's documented limit, in UTF-8 code units
_TITLE_TYPES = ("TITLE", "CENTERED_TITLE")
_BODY_TYPES = ("BODY", "SUBTITLE")


def _clip(s: str) -> str:
    return s if len(s) <= _PREVIEW_CHARS else s[:_PREVIEW_CHARS] + "…"


def _text_of(text: dict | None) -> str:
    """Plain text of a shape's or table cell's `text`, minus the implicit trailing newline."""
    runs = "".join(
        te["textRun"]["content"] for te in (text or {}).get("textElements", []) if "textRun" in te
    )
    return runs[:-1] if runs.endswith("\n") else runs


def _walk(elements: list) -> Iterator[dict]:
    """Every page element, descending into groups."""
    for el in elements:
        yield el
        yield from _walk(el.get("elementGroup", {}).get("children", []))


def _placeholder_type(el: dict) -> str | None:
    return el.get("shape", {}).get("placeholder", {}).get("type")


def _table_cells(el: dict) -> Iterator[tuple[int, int, str]]:
    for r, row in enumerate(el["table"].get("tableRows", [])):
        for c, cell in enumerate(row.get("tableCells", [])):
            yield r, c, _text_of(cell.get("text"))


def _slide_title(slide: dict) -> str:
    for el in _walk(slide.get("pageElements", [])):
        if _placeholder_type(el) in _TITLE_TYPES:
            return _text_of(el["shape"].get("text"))
    return ""


def _notes(slide: dict) -> tuple[str | None, str]:
    """(speaker-notes shape id, speaker-notes text) for a slide."""
    page = slide.get("slideProperties", {}).get("notesPage", {})
    oid = page.get("notesProperties", {}).get("speakerNotesObjectId")
    for el in _walk(page.get("pageElements", [])):
        if el.get("objectId") == oid:
            return oid, _text_of(el.get("shape", {}).get("text"))
    return oid, ""  # the shape is only materialised once text is inserted into it


def _slide_text(slide: dict) -> str:
    """All of a slide's visible text (shapes and table cells), for previews."""
    parts: list[str] = []
    for el in _walk(slide.get("pageElements", [])):
        if "table" in el:
            parts.extend(t for _, _, t in _table_cells(el) if t)
        elif t := _text_of(el.get("shape", {}).get("text")):
            parts.append(t)
    return "\n".join(parts)


def _render_element(el: dict) -> str:
    oid = el["objectId"]
    if "table" in el:
        rows = el["table"].get("tableRows", [])
        head = f"- [TABLE] id={oid} {el['table'].get('rows', len(rows))}x{el['table'].get('columns', 0)}"
        lines = [head]
        for r, row in enumerate(rows):
            cells = (_text_of(c.get("text")).replace("\n", " ") for c in row.get("tableCells", []))
            lines.append(f"  r{r}: " + " | ".join(cells))
        return "\n".join(lines)
    if "shape" in el:
        label = _placeholder_type(el) or el["shape"].get("shapeType", "SHAPE")
        text = _text_of(el["shape"].get("text")).replace("\n", "\n    ")
        return f"- [{label}] id={oid}: {text or '(empty)'}"
    if "elementGroup" in el:
        return f"- [GROUP] id={oid} ({len(el['elementGroup'].get('children', []))} children)"
    kind = next((k for k in ("image", "video", "line", "sheetsChart", "wordArt") if k in el), "ELEMENT")
    return f"- [{kind.upper()}] id={oid}"


def _render_thread(thread: dict) -> str:
    head = thread.get("headPost", {})
    who = head.get("author", {}).get("displayName") or "unknown"
    assignee = head.get("assigneeEmail")
    line = f"- [{thread.get('status', 'OPEN')}] id={thread['commentId']} {who}"
    line += (f" -> assigned to {assignee}" if assignee else "") + f": {head.get('content', '')}"
    for reply in thread.get("replies", []):
        if not reply.get("deleted"):
            line += f"\n    reply from {reply.get('author', {}).get('displayName') or 'unknown'}: {reply.get('content', '')}"
    return line


def _live_threads(pres: dict) -> list[dict]:
    return [t for t in pres.get("comments", []) if not t.get("headPost", {}).get("deleted")]


def _slide_threads(slide: dict, threads: list[dict]) -> list[dict]:
    """Threads anchored to this slide or to an element on it (matched by Slides' own anchor ids)."""
    anchors = {a["anchorId"] for a in slide.get("commentAnchors", [])}
    return [t for t in threads if t.get("anchorId") in anchors]


def _render_slide(number: int, slide: dict, layout_names: dict, threads: list[dict]) -> str:
    layout = layout_names.get(slide.get("slideProperties", {}).get("layoutObjectId"))
    head = f"## Slide {number} · id={slide['objectId']}" + (f" · layout={layout}" if layout else "")
    lines = [head, *(_render_element(el) for el in slide.get("pageElements", []))]
    _, notes = _notes(slide)
    if notes:
        lines.append("Notes: " + notes.replace("\n", "\n    "))
    mine = _slide_threads(slide, threads)
    if mine:
        lines.append("Comments:")
        lines.extend("  " + _render_thread(t).replace("\n", "\n  ") for t in mine)
    return "\n".join(lines)


def _get(item: str, comments: bool = False) -> tuple[str, dict]:
    pid = parse_ref(item).id
    # Comments are omitted unless asked for. DEFAULT_FOR_CURRENT_ACCESS includes them for editors
    # and omits them for view-only users, where INCLUDED would be refused with a 403.
    extra = {"commentsViewMode": "COMMENTS_VIEW_MODE_DEFAULT_FOR_CURRENT_ACCESS"} if comments else {}
    return pid, slides().presentations().get(presentationId=pid, **extra).execute()


def _pick_slide(slide_list: list, ref: str) -> tuple[int, dict]:
    """Resolve a slide by objectId, else by 1-based number. Returns (0-based index, slide)."""
    ref = ref.strip()
    for i, s in enumerate(slide_list):
        if s["objectId"] == ref:
            return i, s
    if ref.isdigit():
        n = int(ref)
        if 1 <= n <= len(slide_list):
            return n - 1, slide_list[n - 1]
        raise RuntimeError(f"slide {n} is out of range; this presentation has {len(slide_list)} slide(s)")
    raise RuntimeError(f"no slide with id {ref!r} (pass a 1-based slide number or an id from read_presentation)")


def _write_control(pres: dict) -> dict:
    """Pin the write to the revision it was resolved against, so a concurrent edit rejects it."""
    rev = pres.get("revisionId")
    return {"writeControl": {"requiredRevisionId": rev}} if rev else {}


def _batch(pid: str, requests: list, pres: dict | None = None) -> dict:
    body = {"requests": requests, **(_write_control(pres) if pres else {})}
    return slides().presentations().batchUpdate(presentationId=pid, body=body).execute()


def _text_requests(object_id: str, existing: str, text: str, cell: dict | None = None) -> list[dict]:
    """Replace everything in a shape/cell. deleteText is skipped on empty text, which Slides rejects."""
    where = {"objectId": object_id, **({"cellLocation": cell} if cell else {})}
    requests: list[dict] = []
    if existing:
        requests.append({"deleteText": {**where, "textRange": {"type": "ALL"}}})
    if text:
        requests.append({"insertText": {**where, "insertionIndex": 0, "text": text}})
    return requests


@api_errors
def read_presentation(
    item: str,
    slide: str | None = None,
    chunk: int = 0,
    max_chars: int = DEFAULT_MAX_CHARS,
) -> dict:
    """Read a Google Slides presentation (item = URL or ID) as text, one bounded chunk at a time.

    Every slide is rendered with its `id`, layout, each page element (its placeholder/shape type,
    `id` and text; tables cell by cell), speaker notes and comment threads (status, author,
    assignee, replies; ones not tied to a slide appear last under "Unanchored comments"). Pass `slide` (a 1-based number such as
    "3", or a slide id) to read just that slide. Returns the requested chunk (0-based) plus title,
    `revision_id`, `slide_count`, an `outline` of every slide (number, id, title), the deck's
    `layouts` (id and name, usable as add_slide's `layout`) and paging metadata (total_chunks,
    total_chars, has_more). Set max_chars<=0 to get everything.

    Use the element `id`s with set_shape_text, and slide numbers or ids with the other slide tools.
    Slide numbers shift when slides are added, moved or deleted; ids do not.
    """
    pid, pres = _get(item, comments=True)
    slide_list = pres.get("slides", [])
    layouts = [
        {"object_id": lay["objectId"], "name": lay.get("layoutProperties", {}).get("displayName")}
        for lay in pres.get("layouts", [])
    ]
    layout_names = {lay["object_id"]: lay["name"] for lay in layouts}
    if slide is not None:
        i, picked = _pick_slide(slide_list, slide)
        selected = [(i + 1, picked)]
    else:
        selected = list(enumerate(slide_list, start=1))
    threads = _live_threads(pres)
    content = "\n\n".join(_render_slide(n, s, layout_names, threads) for n, s in selected)
    if slide is None:
        # Comments made through the Drive API are not tied to a slide; keep them visible anyway.
        placed = {t["commentId"] for _, s in selected for t in _slide_threads(s, threads)}
        loose = [t for t in threads if t["commentId"] not in placed]
        if loose:
            content += "\n\n## Unanchored comments\n" + "\n".join(_render_thread(t) for t in loose)
    return {
        "presentation_id": pid,
        "title": pres.get("title"),
        # Pass-through for callers that want to reason about staleness; write tools pin it themselves.
        "revision_id": pres.get("revisionId"),
        "slide_count": len(slide_list),
        "outline": [
            {"number": n, "object_id": s["objectId"], "title": _slide_title(s)}
            for n, s in enumerate(slide_list, start=1)
        ],
        "layouts": layouts,
        **paginate(content, chunk, max_chars),
    }


@api_errors
def create_presentation(title: str) -> dict:
    """Create a new Google Slides presentation (in My Drive root) with the given title.

    Slides starts it with one slide; its id is returned as `first_slide_id`. Add more with
    add_slide. (For documents use create_document, for spreadsheets create_spreadsheet.)
    """
    pres = slides().presentations().create(body={"title": title}).execute()
    pid = pres["presentationId"]
    first = (pres.get("slides") or [{}])[0].get("objectId")
    return {
        "id": pid,
        "url": f"https://docs.google.com/presentation/d/{pid}/edit",
        "title": pres.get("title"),
        "first_slide_id": first,
    }


def _resolve_layout(pres: dict, layout: str) -> dict:
    """A LayoutReference for a predefined layout name, or for a layout of this deck by id/name."""
    if layout.upper() in _PREDEFINED_LAYOUTS:
        return {"predefinedLayout": layout.upper()}
    wanted = layout.strip().lower()
    for lay in pres.get("layouts", []):
        name = lay.get("layoutProperties", {}).get("displayName") or ""
        if lay["objectId"] == layout.strip() or name.lower() == wanted:
            return {"layoutId": lay["objectId"]}
    raise RuntimeError(
        f"unknown layout {layout!r}: use one of {', '.join(_PREDEFINED_LAYOUTS)} or a layout id/name "
        f"from read_presentation's `layouts`"
    )


@api_errors
def add_slide(
    item: str,
    layout: str = "TITLE_AND_BODY",
    title: str | None = None,
    body: str | None = None,
    position: int | None = None,
    dry_run: bool = False,
) -> dict:
    """Add a slide to a Google Slides presentation (item = URL or ID), optionally filling its text.

    `layout` is a predefined layout (BLANK, CAPTION_ONLY, TITLE, TITLE_AND_BODY,
    TITLE_AND_TWO_COLUMNS, TITLE_ONLY, SECTION_HEADER, SECTION_TITLE_AND_DESCRIPTION,
    ONE_COLUMN_TEXT, MAIN_POINT, BIG_NUMBER) or the id or name of one of the deck's own layouts
    (listed by read_presentation). `title` goes into the slide's title placeholder and `body` into
    its body (or subtitle) placeholder; a layout without that placeholder is reported under
    `unfilled` rather than failing after the slide was created. `position` is the 1-based number
    the new slide takes (default: appended at the end). dry_run=true reports the resolved layout
    and position without writing.
    """
    if position is not None and position < 1:
        raise RuntimeError("position is a 1-based slide number, so it must be >= 1")
    pid = parse_ref(item).id
    svc = slides().presentations()
    needs_deck = layout.upper() not in _PREDEFINED_LAYOUTS
    pres = svc.get(presentationId=pid).execute() if needs_deck else {}
    reference = _resolve_layout(pres, layout)
    create: dict = {"slideLayoutReference": reference}
    if position is not None:
        create["insertionIndex"] = position - 1
    if dry_run:
        return {
            "dry_run": True,
            "action": "add_slide",
            "layout": reference,
            "position": position,
            "title": title,
            "body": body,
        }

    reply = _batch(pid, [{"createSlide": create}])
    slide_id = reply["replies"][0]["createSlide"]["objectId"]
    result: dict = {"presentation_id": pid, "slide_id": slide_id, "layout": reference}
    if position is not None:
        result["position"] = position
    if title is None and body is None:
        return result

    page = svc.pages().get(presentationId=pid, pageObjectId=slide_id).execute()
    placeholders = [el for el in _walk(page.get("pageElements", [])) if "shape" in el]
    wanted = (("title", title, _TITLE_TYPES), ("body", body, _BODY_TYPES))
    requests: list[dict] = []
    filled: list[str] = []
    unfilled: list[str] = []
    for name, text, types in wanted:
        if text is None:
            continue
        target = next((el for el in placeholders if _placeholder_type(el) in types), None)
        if target is None:
            unfilled.append(name)
            continue
        requests.extend(_text_requests(target["objectId"], "", text))
        filled.append(name)
    if requests:
        _batch(pid, requests)
    result["filled"] = filled
    if unfilled:
        result["unfilled"] = unfilled
    return result


@api_errors
def duplicate_slide(item: str, slide: str, dry_run: bool = False) -> dict:
    """Duplicate a slide in a Google Slides presentation (item = URL or ID).

    `slide` is a 1-based number (e.g. "3") or a slide id from read_presentation. The copy is placed
    immediately after the original; its id is returned as `new_slide_id`. dry_run=true reports
    which slide would be copied without writing.
    """
    pid, pres = _get(item)
    index, picked = _pick_slide(pres.get("slides", []), slide)
    if dry_run:
        return {
            "dry_run": True,
            "action": "duplicate_slide",
            "slide_number": index + 1,
            "slide_id": picked["objectId"],
            "title": _slide_title(picked),
        }
    reply = _batch(pid, [{"duplicateObject": {"objectId": picked["objectId"]}}])
    return {
        "presentation_id": pid,
        "duplicated": picked["objectId"],
        "new_slide_id": reply["replies"][0]["duplicateObject"]["objectId"],
        "new_slide_number": index + 2,
    }


@api_errors
def move_slide(item: str, slide: str, position: int, dry_run: bool = False) -> dict:
    """Move a slide to a new position in a Google Slides presentation (item = URL or ID).

    `slide` is a 1-based number (e.g. "3") or a slide id from read_presentation; `position` is the
    1-based number the slide has once moved. Nothing is lost, so this is not gated. dry_run=true
    reports the move without writing.
    """
    pid, pres = _get(item)
    slide_list = pres.get("slides", [])
    current, picked = _pick_slide(slide_list, slide)
    if not 1 <= position <= len(slide_list):
        raise RuntimeError(f"position {position} is out of range; this presentation has {len(slide_list)} slide(s)")
    target = position - 1
    moved = {"slide_id": picked["objectId"], "from": current + 1, "to": position}
    if target == current:
        return {"dry_run": True, **moved, "moved": False} if dry_run else {**moved, "moved": False}
    if dry_run:
        return {"dry_run": True, "action": "move_slide", **moved}
    # updateSlidesPosition's index is in the arrangement *before* the move, so a slide moving later
    # must be inserted before the slide that ends up after it — one past its final 0-based index.
    insertion = target + 1 if target > current else target
    _batch(
        pid,
        [{"updateSlidesPosition": {"slideObjectIds": [picked["objectId"]], "insertionIndex": insertion}}],
        pres,
    )
    return {"presentation_id": pid, **moved, "moved": True}


@api_errors
def delete_slide(item: str, slide: str, confirm: bool = False, dry_run: bool = False) -> dict:
    """Delete a slide from a Google Slides presentation (item = URL or ID).

    `slide` is a 1-based number (e.g. "3") or a slide id from read_presentation. dry_run=true
    reports exactly what would be removed without writing. This tool is destructive: without
    confirm=true it returns a preview of the slide (its title, element count and text) and
    deletes nothing.
    """
    pid, pres = _get(item)
    index, picked = _pick_slide(pres.get("slides", []), slide)
    impact = {
        "slide_number": index + 1,
        "slide_id": picked["objectId"],
        "title": _slide_title(picked),
        "elements": sum(1 for _ in _walk(picked.get("pageElements", []))),
        "deletes_text": _clip(_slide_text(picked)),
    }
    if dry_run:
        return {"dry_run": True, "action": "delete_slide", **impact}
    if not confirm:
        return preview_response("delete_slide", impact)
    _batch(pid, [{"deleteObject": {"objectId": picked["objectId"]}}], pres)
    return {"presentation_id": pid, "deleted": True, **impact}


@api_errors
def replace_slide_text(
    item: str,
    match: str,
    replacement: str,
    slide: str | None = None,
    confirm: bool = False,
    dry_run: bool = False,
) -> dict:
    """Replace text on the slides of a Google Slides presentation (item = URL or ID).

    `match` is a literal, case-sensitive substring within a single paragraph; every occurrence in
    slide shapes and table cells is replaced. Pass `slide` (a 1-based number such as "3", or a
    slide id) to limit it to one slide, else every slide is searched. Layouts, masters and speaker
    notes are never touched (use set_speaker_notes for those). To rewrite a whole shape rather
    than a phrase, use set_shape_text.

    dry_run=true reports the occurrences per slide without writing. This tool is destructive:
    without confirm=true it returns that preview and writes nothing. An unmatched `match` raises
    rather than silently doing nothing.
    """
    if not match:
        raise RuntimeError("match must be a non-empty string")
    if "\n" in match:
        raise RuntimeError("match cannot span paragraphs; replace one paragraph's text at a time")
    pid, pres = _get(item)
    slide_list = pres.get("slides", [])
    if slide is not None:
        i, picked = _pick_slide(slide_list, slide)
        scanned = [(i + 1, picked)]
    else:
        scanned = list(enumerate(slide_list, start=1))

    per_slide: list[dict] = []
    for number, s in scanned:
        count = 0
        for el in _walk(s.get("pageElements", [])):
            if "table" in el:
                count += sum(t.count(match) for _, _, t in _table_cells(el))
            else:
                count += _text_of(el.get("shape", {}).get("text")).count(match)
        if count:
            per_slide.append({"slide_number": number, "slide_id": s["objectId"], "occurrences": count})
    if not per_slide:
        raise RuntimeError(f"no match for {match!r} on the searched slide(s) (matching is case-sensitive)")
    impact = {
        "target": match,
        "replaces_with": _clip(replacement),
        "occurrences": sum(p["occurrences"] for p in per_slide),
        "slides": per_slide,
    }
    if dry_run:
        return {"dry_run": True, "action": "replace_slide_text", **impact}
    if not confirm:
        return preview_response("replace_slide_text", impact)

    # Always name the pages: with none, replaceAllText also rewrites layouts, masters and notes.
    request = {
        "replaceAllText": {
            "containsText": {"text": match, "matchCase": True},
            "replaceText": replacement,
            "pageObjectIds": [s["objectId"] for _, s in scanned],
        }
    }
    reply = _batch(pid, [request], pres)
    changed = reply["replies"][0].get("replaceAllText", {}).get("occurrencesChanged", 0)
    return {"presentation_id": pid, "replaced": True, **impact, "occurrences_changed": changed}


def _find_element(slide_list: list, object_id: str) -> tuple[int, dict]:
    for number, s in enumerate(slide_list, start=1):
        for el in _walk(s.get("pageElements", [])):
            if el["objectId"] == object_id:
                return number, el
    raise RuntimeError(f"no page element with id {object_id!r} on any slide (ids come from read_presentation)")


@api_errors
def set_shape_text(
    item: str,
    object_id: str,
    text: str,
    row: int | None = None,
    column: int | None = None,
    confirm: bool = False,
    dry_run: bool = False,
) -> dict:
    """Set the entire text of one shape, placeholder or table cell on a slide (item = URL or ID).

    `object_id` is an element id from read_presentation (a title, body, text box or table). For a
    table pass its 0-based `row` and `column` as well; for anything else leave them out. The
    element's existing text is replaced by `text` (pass "" to clear it); a "\\n" in `text` starts a
    new paragraph. To change just a phrase use replace_slide_text; for speaker notes use
    set_speaker_notes.

    dry_run=true reports the before and after without writing. Replacing existing text is
    destructive: without confirm=true it returns a preview of the text it would overwrite and
    writes nothing. Setting text on an empty shape needs no confirmation.
    """
    pid, pres = _get(item)
    number, el = _find_element(pres.get("slides", []), object_id)
    cell = None
    if "table" in el:
        if row is None or column is None:
            raise RuntimeError(f"{object_id!r} is a table: pass its 0-based row and column")
        cells = {(r, c): t for r, c, t in _table_cells(el)}
        if (row, column) not in cells:
            raise RuntimeError(f"table {object_id!r} has no cell at row {row}, column {column}")
        existing = cells[(row, column)]
        cell = {"rowIndex": row, "columnIndex": column}
    elif "shape" in el:
        if row is not None or column is not None:
            raise RuntimeError(f"{object_id!r} is not a table, so row and column do not apply")
        existing = _text_of(el["shape"].get("text"))
    else:
        raise RuntimeError(f"{object_id!r} holds no text (it is an image, line, video, chart or group)")

    impact = {
        "slide_number": number,
        "object_id": object_id,
        "cell": cell,
        "overwrites_text": _clip(existing),
        "new_text": _clip(text),
    }
    if existing == text:
        return {"presentation_id": pid, "unchanged": True, **impact}
    if dry_run:
        return {"dry_run": True, "action": "set_shape_text", **impact}
    if existing and not confirm:
        return preview_response("set_shape_text", impact)
    _batch(pid, _text_requests(object_id, existing, text, cell), pres)
    return {"presentation_id": pid, "updated": True, **impact}


@api_errors
def set_speaker_notes(item: str, slide: str, text: str, confirm: bool = False, dry_run: bool = False) -> dict:
    """Set a slide's speaker notes in a Google Slides presentation (item = URL or ID).

    `slide` is a 1-based number (e.g. "3") or a slide id from read_presentation. The existing notes
    are replaced by `text` (pass "" to clear them); a "\\n" in `text` starts a new paragraph.

    dry_run=true reports the before and after without writing. Replacing existing notes is
    destructive: without confirm=true it returns a preview of the notes it would overwrite and
    writes nothing. Setting notes on a slide that has none needs no confirmation.
    """
    pid, pres = _get(item)
    index, picked = _pick_slide(pres.get("slides", []), slide)
    notes_id, existing = _notes(picked)
    if notes_id is None:
        raise RuntimeError(f"slide {index + 1} has no speaker-notes shape to write to")
    impact = {
        "slide_number": index + 1,
        "slide_id": picked["objectId"],
        "overwrites_notes": _clip(existing),
        "new_notes": _clip(text),
    }
    if existing == text:
        return {"presentation_id": pid, "unchanged": True, **impact}
    if dry_run:
        return {"dry_run": True, "action": "set_speaker_notes", **impact}
    if existing and not confirm:
        return preview_response("set_speaker_notes", impact)
    _batch(pid, _text_requests(notes_id, existing, text), pres)
    return {"presentation_id": pid, "updated": True, **impact}


def _email(value: str, arg: str) -> str:
    email = value.strip()
    if not _EMAIL.match(email):
        raise RuntimeError(
            f"{arg} must be an email address such as 'sam@company.com': a name like '@Sam' cannot be "
            f"resolved to an account (got {value!r})"
        )
    return email


@api_errors
def add_slide_comment(
    item: str,
    slide: str,
    content: str,
    assignee: str | None = None,
    mention: str | None = None,
    element: str | None = None,
    confirm: bool = False,
    dry_run: bool = False,
) -> dict:
    """Add a comment to a slide of a Google Slides presentation (item = URL or ID), optionally assigned.

    `slide` is a 1-based number (e.g. "3") or a slide id from read_presentation; pass `element` (an
    element id from read_presentation) to pin the comment to that element instead of the whole
    slide. `assignee` is the email address to assign the comment to, the same as ticking "Assign
    to" in the editor; they are also @mentioned. `mention` is an email address to @mention without
    assigning. Both must be email addresses: a name like "@Sam" cannot be resolved to an account.
    `content` is plain text of at most 2048 bytes; the @mentions are added to the start of it
    unless already present.

    Assigning or mentioning someone notifies them by email, so that is outward-facing: without
    confirm=true the tool returns a preview of the comment and who would be notified, and posts
    nothing. A plain comment with neither needs no confirmation. dry_run=true previews without
    writing. Read comments back with read_presentation.
    """
    if not content or not content.strip():
        raise RuntimeError("content must be a non-empty string")
    assignee = _email(assignee, "assignee") if assignee is not None else None
    mention = _email(mention, "mention") if mention is not None else None
    pid, pres = _get(item)
    index, picked = _pick_slide(pres.get("slides", []), slide)
    target = picked["objectId"]
    if element is not None:
        if not any(el["objectId"] == element for el in _walk(picked.get("pageElements", []))):
            raise RuntimeError(f"no page element with id {element!r} on slide {index + 1} (ids come from read_presentation)")
        target = element

    people = list(dict.fromkeys(e for e in (assignee, mention) if e))
    text = content
    missing = [e for e in people if f"+{e}" not in content and f"@{e}" not in content]
    if missing:
        text = " ".join(f"+{e}" for e in missing) + " " + content
    if len(text.encode("utf-8")) > _COMMENT_MAX_BYTES:
        raise RuntimeError(f"the comment (with its @mentions) exceeds {_COMMENT_MAX_BYTES} bytes")

    impact = {
        "slide_number": index + 1,
        "slide_id": picked["objectId"],
        "element": element,
        "comment": _clip(text),
        "assigns": assignee,
        "mentions": people,
        "notifies_by_email": people,
    }
    if dry_run:
        return {"dry_run": True, "action": "add_slide_comment", **impact}
    if people and not confirm:
        return preview_response("add_slide_comment", impact)

    request: dict = {"content": text, "objectId": target}
    if assignee:
        request["assigneeEmailAddress"] = assignee
    reply = _batch(pid, [{"insertComment": request}])
    thread = reply["replies"][0]["insertComment"]["commentThread"]
    assigned_to = thread.get("headPost", {}).get("assigneeEmail")
    return {
        "presentation_id": pid,
        "posted": True,
        "comment_id": thread["commentId"],
        # What Slides reports back, not what was asked for.
        "assigned_to": assigned_to,
        **impact,
    }


_TOOLS = (
    read_presentation,
    create_presentation,
    add_slide,
    duplicate_slide,
    move_slide,
    delete_slide,
    replace_slide_text,
    set_shape_text,
    set_speaker_notes,
    add_slide_comment,
)
