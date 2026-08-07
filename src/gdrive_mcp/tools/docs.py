"""Google Docs tools: read text/markdown, extract images, create, append/insert (opt-in
markdown formatting), comments.

Every tool references its target Doc/file via `item` (a URL or ID).
"""

from __future__ import annotations

import re
from urllib.parse import urlparse

from googleapiclient.errors import HttpError
from mcp.server.fastmcp import Image

from gdrive_mcp.chunking import DEFAULT_MAX_CHARS, paginate
from gdrive_mcp.clients import authed_session, docs, drive
from gdrive_mcp.errors import api_errors
from gdrive_mcp.ids import parse_ref, parse_tab
from gdrive_mcp.md import (
    ParsedMarkdown,
    has_table,
    parse_cell,
    parse_markdown,
    render_markdown_preview,
    render_table_markdown,
    split_blocks,
    style_requests,
    u16len,
)

_HEADING = {f"HEADING_{i}": "#" * i for i in range(1, 7)}

# Only fetch image contentUris from Google-owned hosts — the fetch carries the user's Drive
# OAuth token (AuthorizedSession), so it must never reach a host a document could point at.
_ALLOWED_IMAGE_HOST_SUFFIXES = (".googleusercontent.com", ".google.com", ".googleapis.com", ".gstatic.com")


def _is_google_host(url: str) -> bool:
    try:
        host = (urlparse(url).hostname or "").lower()
    except ValueError:
        return False
    return any(host == s.lstrip(".") or host.endswith(s) for s in _ALLOWED_IMAGE_HOST_SUFFIXES)


_HEX_RE = re.compile(r"^#?([0-9a-fA-F]{6})$")


def _hex_to_rgb(color: str) -> dict:
    """'#3366CC' (or '3366CC') -> Docs rgbColor with 0..1 float channels."""
    m = _HEX_RE.match(color.strip())
    if not m:
        raise RuntimeError(f"color must be a 6-digit hex string like '#3366CC', got {color!r}")
    h = m.group(1)
    return {"red": int(h[0:2], 16) / 255, "green": int(h[2:4], 16) / 255, "blue": int(h[4:6], 16) / 255}


def _rgb_to_hex(rgb: dict) -> str:
    return "#%02x%02x%02x" % (round(rgb.get("red", 0.0) * 255), round(rgb.get("green", 0.0) * 255), round(rgb.get("blue", 0.0) * 255))


def _colored_runs(content: list) -> list[dict]:
    """Text runs carrying an explicit foreground color, as {text, color} (hex)."""
    out: list[dict] = []
    for el in content:
        for pe in el.get("paragraph", {}).get("elements", []):
            tr = pe.get("textRun")
            if not tr:
                continue
            fg = tr.get("textStyle", {}).get("foregroundColor", {}).get("color", {}).get("rgbColor")
            if fg is not None:
                out.append({"text": tr.get("content", "").rstrip("\n"), "color": _rgb_to_hex(fg)})
    return out


def _color_request(start: int, length: int, tab_id: str | None, rgb: dict) -> dict:
    """An updateTextStyle request coloring [start, start+length) — applied after the insert."""
    rng: dict = {"startIndex": start, "endIndex": start + length}
    if tab_id:
        rng["tabId"] = tab_id
    return {
        "updateTextStyle": {
            "range": rng,
            "textStyle": {"foregroundColor": {"color": {"rgbColor": rgb}}},
            "fields": "foregroundColor",
        }
    }


def _para_text(para: dict) -> str:
    return "".join(el.get("textRun", {}).get("content", "") for el in para.get("elements", []))


def _content_to_markdown(content: list) -> tuple[str, list[dict]]:
    lines: list[str] = []
    outline: list[dict] = []
    for el in content:
        para = el.get("paragraph")
        if para is not None:
            text = _para_text(para).rstrip("\n")
            style = para.get("paragraphStyle", {}).get("namedStyleType", "")
            if style in _HEADING and text:
                lines.append(f"{_HEADING[style]} {text}")
                outline.append({"level": int(style[-1]), "text": text})
            else:
                lines.append(text)
            continue
        table = el.get("table")
        if table is not None:
            for ri, row in enumerate(table.get("tableRows", [])):
                cells = []
                for cell in row.get("tableCells", []):
                    ctext = "".join(
                        _para_text(c["paragraph"])
                        for c in cell.get("content", [])
                        if "paragraph" in c
                    )
                    # escape '|' so cell text stays one field and re-parses (see md.split_row)
                    cells.append(ctext.strip().replace("\n", " ").replace("|", "\\|"))
                lines.append("| " + " | ".join(cells) + " |")
                if ri == 0:  # GFM delimiter row (column-matched) so the table re-parses on write
                    lines.append("| " + " | ".join(["---"] * len(cells)) + " |")
    return "\n".join(lines), outline


def _content_to_text(content: list) -> str:
    return "\n".join(
        _para_text(el["paragraph"]).rstrip("\n") for el in content if "paragraph" in el
    )


def _flatten_tabs(tabs: list) -> list[tuple[str, str, list, dict, dict]]:
    """(tab_id, title, body_content, inline_objects, positioned_objects) for every tab,
    depth-first incl. children."""
    out: list[tuple[str, str, list, dict, dict]] = []
    for t in tabs:
        props = t.get("tabProperties", {})
        dtab = t.get("documentTab", {})
        out.append(
            (
                props.get("tabId"),
                props.get("title"),
                dtab.get("body", {}).get("content", []),
                dtab.get("inlineObjects", {}),
                dtab.get("positionedObjects", {}),
            )
        )
        out.extend(_flatten_tabs(t.get("childTabs", [])))
    return out


def _selected_tabs(all_tabs: list, tab_id: str | None) -> list:
    if not tab_id:
        return all_tabs
    selected = [t for t in all_tabs if t[0] == tab_id]
    if not selected:
        raise RuntimeError(f"tab {tab_id!r} not found; available: {[(t[0], t[1]) for t in all_tabs]}")
    return selected


def _resolve_write_tab(doc: dict, tab_id: str | None) -> tuple[str | None, list]:
    """Pick a write target: (tab_id, body_content) for a tabbed doc, or (None, top-level body).

    Defaults to the first tab when the doc has tabs but none was requested. A batchUpdate on a
    tabbed doc must carry the tab id in location.tabId, so writes always resolve one.
    """
    all_tabs = _flatten_tabs(doc.get("tabs", []))
    if not all_tabs:
        return None, doc.get("body", {}).get("content", [])
    tid, _title, body, *_ = _selected_tabs(all_tabs, tab_id)[0]
    return tid, body


def _embedded_image_uri(obj: dict, props_key: str) -> str | None:
    emb = obj.get(props_key, {}).get("embeddedObject", {})
    return emb.get("imageProperties", {}).get("contentUri")


def _image_uris(content: list, inline_objects: dict, positioned_objects: dict) -> list[str]:
    """contentUris of embedded images in `content`, in document order.

    Covers both inline images and positioned (floating/wrapped) images; a positioned object is
    anchored to the start of its paragraph, so it is emitted before that paragraph's inline
    images.
    """
    uris: list[str] = []
    for el in content:
        para = el.get("paragraph", {})
        for oid in para.get("positionedObjectIds", []):
            uri = _embedded_image_uri(positioned_objects.get(oid, {}), "positionedObjectProperties")
            if uri:
                uris.append(uri)
        for pe in para.get("elements", []):
            oid = pe.get("inlineObjectElement", {}).get("inlineObjectId")
            if not oid:
                continue
            uri = _embedded_image_uri(inline_objects.get(oid, {}), "inlineObjectProperties")
            if uri:
                uris.append(uri)
    return uris


@api_errors
def read_document(
    item: str,
    output_format: str = "markdown",
    chunk: int = 0,
    max_chars: int = DEFAULT_MAX_CHARS,
    tab: str | None = None,
    include_colors: bool = False,
) -> dict:
    """Read a Google Doc (item = URL or ID) as 'markdown' (default) or 'text', one bounded chunk at a time.

    Multi-tab docs: by default every tab is read (each prefixed with its title as a heading);
    pass a `tab` id, or an `item` URL containing `tab=t.xxxx`, to read only that tab. Returns the
    requested chunk (0-based) plus title, the doc's `tabs`, the outline, and paging metadata
    (total_chunks, total_chars, has_more). Set max_chars<=0 to get the whole document. Set
    include_colors=true to also return `colored_runs` (text spans with an explicit foreground color).
    """
    did = parse_ref(item).id
    tab_id = tab or parse_tab(item)
    doc = docs().documents().get(documentId=did, includeTabsContent=True).execute()
    all_tabs = _flatten_tabs(doc.get("tabs", []))

    outline: list[dict] = []
    colored: list[dict] = []
    if all_tabs:
        selected = _selected_tabs(all_tabs, tab_id)
        parts: list[str] = []
        for tid, title, body, *_ in selected:
            if len(selected) > 1:
                parts.append(title if output_format == "text" else f"# {title}")
                outline.append({"level": 1, "text": title, "tab": tid})
            if output_format == "text":
                parts.append(_content_to_text(body))
            else:
                md, tab_outline = _content_to_markdown(body)
                parts.append(md)
                outline.extend({**e, "tab": tid} for e in tab_outline)
            if include_colors:
                colored.extend(_colored_runs(body))
        content = "\n".join(parts)
    else:
        body = doc.get("body", {}).get("content", [])
        content, outline = (_content_to_text(body), []) if output_format == "text" else _content_to_markdown(body)
        if include_colors:
            colored.extend(_colored_runs(body))

    result = {
        "document_id": did,
        "title": doc.get("title"),
        "tabs": [{"id": tid, "title": title} for tid, title, *_ in all_tabs],
        "tab_read": tab_id or ("all" if len(all_tabs) > 1 else None),
        "outline": outline,
        **paginate(content, chunk, max_chars),
    }
    if include_colors:
        result["colored_runs"] = colored
    return result


@api_errors
def extract_images(item: str, tab: str | None = None) -> list:
    """Extract embedded images from a Google Doc (item = URL or ID) as viewable images, in order.

    Returns both inline images and positioned (floating/wrapped) images, each emitted at the
    paragraph it is anchored to. Multi-tab docs: all tabs by default; pass a `tab` id or an
    `item` URL with `tab=t.xxxx`.
    """
    did = parse_ref(item).id
    tab_id = tab or parse_tab(item)
    doc = docs().documents().get(documentId=did, includeTabsContent=True).execute()
    all_tabs = _flatten_tabs(doc.get("tabs", []))
    if all_tabs:
        uris = [
            uri
            for _tid, _title, body, inline, positioned in _selected_tabs(all_tabs, tab_id)
            for uri in _image_uris(body, inline, positioned)
        ]
    else:
        uris = _image_uris(
            doc.get("body", {}).get("content", []),
            doc.get("inlineObjects", {}),
            doc.get("positionedObjects", {}),
        )

    session = authed_session()
    images = []
    for uri in uris:
        if not _is_google_host(uri):
            continue
        r = session.get(uri, allow_redirects=False, timeout=30)
        if r.is_redirect or r.is_permanent_redirect:
            continue
        r.raise_for_status()
        fmt = r.headers.get("content-type", "image/png").split("/")[-1].split(";")[0]
        images.append(Image(data=r.content, format=fmt))
    return images


def _md_summary(parsed: ParsedMarkdown) -> dict:
    return {
        "headings": len(parsed.headings),
        "list_items": parsed.list_items,
        "styled_spans": len(parsed.spans),
    }


# ---- table writes (shared by insert_table and the markdown pipe-table path) --------------

_MAX_TABLE_COLS = 20
_MAX_TABLE_CELLS = 10_000


def _cell_text(v: object) -> str:
    return "" if v is None else str(v)


def _validate_rows(rows: object) -> tuple[int, int]:
    """Validate a rectangular, sanely-sized grid; return (n_rows, n_cols) or raise RuntimeError."""
    if not isinstance(rows, list) or not rows:
        raise RuntimeError("rows must be a non-empty list of row lists")
    widths = []
    for i, row in enumerate(rows):
        if not isinstance(row, list):
            raise RuntimeError(f"row {i} must be a list of cell values")
        widths.append(len(row))
    n_cols = widths[0]
    if n_cols < 1:
        raise RuntimeError("rows must have at least one column")
    if len(set(widths)) != 1:
        raise RuntimeError(f"all rows must have the same number of columns; got row widths {widths}")
    n_rows = len(rows)
    if n_cols > _MAX_TABLE_COLS:
        raise RuntimeError(f"too many columns ({n_cols}); max {_MAX_TABLE_COLS}")
    if n_rows * n_cols > _MAX_TABLE_CELLS:
        raise RuntimeError(f"table too large ({n_rows}x{n_cols} = {n_rows * n_cols} cells); max {_MAX_TABLE_CELLS}")
    return n_rows, n_cols


def _loc(index: int, tab_id: str | None) -> dict:
    loc: dict = {"index": index}
    if tab_id:
        loc["tabId"] = tab_id
    return loc


def _doc_range(start: int, end: int, tab_id: str | None) -> dict:
    r: dict = {"startIndex": start, "endIndex": end}
    if tab_id:
        r["tabId"] = tab_id
    return r


def _table_starts(body: list) -> set:
    """startIndexes of every table element in a body — the pre-insert snapshot for table selection."""
    return {el["startIndex"] for el in body if "table" in el and "startIndex" in el}


def _find_new_table(body: list, pre: set) -> dict:
    """The just-inserted table: the table element whose startIndex is not in the pre-insert snapshot.

    Deterministic regardless of pre-existing tables below the insert point (which shift down and
    would otherwise also satisfy `start >= insert_index`). insertTable adds a newline before the
    table, so its start is `requested_index + 1` — never rely on `start == requested_index`.
    """
    fresh = [el for el in body if "table" in el and el.get("startIndex") not in pre]
    if not fresh:
        raise RuntimeError("could not locate the inserted table after re-fetch")
    return min(fresh, key=lambda el: el["startIndex"])


def _table_cell_indexes(table_el: dict) -> list[tuple[int, int, int]]:
    """(row, col, first-paragraph startIndex) in row-major order for an empty table element."""
    out: list[tuple[int, int, int]] = []
    for r, row in enumerate(table_el["table"].get("tableRows", [])):
        for c, cell in enumerate(row.get("tableCells", [])):
            first_para = next(x for x in cell.get("content", []) if "paragraph" in x)
            out.append((r, c, first_para["startIndex"]))
    return out


def _fill_requests(cells: list, tid: str | None, content_fn) -> tuple[list, int]:
    """Fill requests for the empty cells, forward with a running offset so a single batch is correct.

    `content_fn(r, c) -> (plain_text, spans)`; empty cells are skipped (empty insertText is rejected).
    Each insertText shifts later indexes, so `at = empty_idx + offset` accounts for prior inserts;
    style ranges are computed post-shift for their own cell.
    """
    reqs: list = []
    offset = 0
    filled = 0
    for r, c, empty_idx in cells:
        plain, spans = content_fn(r, c)
        if not plain:
            continue
        at = empty_idx + offset
        reqs.append({"insertText": {"location": _loc(at, tid), "text": plain}})
        for s, e, styles in spans:
            reqs.append({
                "updateTextStyle": {
                    "range": _doc_range(at + s, at + e, tid),
                    "textStyle": {name: True for name in styles},
                    "fields": ",".join(styles),
                }
            })
        offset += u16len(plain)
        filled += 1
    return reqs, filled


def _fill_new_table(svc, did: str, tid: str | None, pre: set, rows: list, content_fn) -> int:
    """Re-fetch, locate the table not in `pre`, and fill it. Rolls back the empty table if the
    fill batch fails (the two-phase write is non-atomic), then re-raises. Returns cells filled."""
    doc = svc.documents().get(documentId=did, includeTabsContent=True).execute()
    _tid, body = _resolve_write_tab(doc, tid)
    table_el = _find_new_table(body, pre)
    cells = _table_cell_indexes(table_el)
    fill_reqs, filled = _fill_requests(cells, tid, content_fn)
    if fill_reqs:
        try:
            svc.documents().batchUpdate(documentId=did, body={"requests": fill_reqs}).execute()
        except HttpError:
            rng = _doc_range(table_el["startIndex"], table_el["endIndex"], tid)
            try:  # best-effort: remove the orphaned empty table before surfacing the error
                svc.documents().batchUpdate(
                    documentId=did, body={"requests": [{"deleteContentRange": {"range": rng}}]}
                ).execute()
            except HttpError:
                pass
            raise
    return filled


def _insert_one_table(svc, did: str, tid: str | None, anchor: int, rows: list) -> int:
    """Insert+fill one table (markdown path): snapshot, insert empty grid, fill with inline styling."""
    doc = svc.documents().get(documentId=did, includeTabsContent=True).execute()
    _tid, body = _resolve_write_tab(doc, tid)
    pre = _table_starts(body)
    n_rows, n_cols = len(rows), len(rows[0])
    svc.documents().batchUpdate(
        documentId=did,
        body={"requests": [{"insertTable": {"rows": n_rows, "columns": n_cols, "location": _loc(anchor, tid)}}]},
    ).execute()

    def content(r: int, c: int):
        return parse_cell(_cell_text(rows[r][c]))

    return _fill_new_table(svc, did, tid, pre, rows, content)


def _insert_markdown_segments(svc, did: str, tid: str | None, anchor: int, segments: list, lead_newline: bool) -> None:
    """Insert mixed text/table segments bottom-up at a fixed anchor.

    Processing last->first means each newly inserted segment sits above the already-placed ones, so
    we never re-reference an already-placed segment's indexes (no cross-segment offset math). Text
    segments reuse the prose renderer with a TRAILING newline and insertion index == style base (do
    not adopt append_text's prepend/base+1). Table segments delegate to _insert_one_table (whose
    insertTable auto-inserts its own leading newline, so tables need no lead_newline handling).
    """
    for i in range(len(segments) - 1, -1, -1):
        kind, payload = segments[i]
        if kind == "table":
            _insert_one_table(svc, did, tid, anchor, payload)
            continue
        prefix = "\n" if (i == 0 and lead_newline) else ""
        parsed = parse_markdown(payload)
        if not parsed.text.strip() and not prefix:
            continue  # skip an empty text run (empty insertText is rejected)
        base = anchor + len(prefix)
        requests = [{"insertText": {"location": _loc(anchor, tid), "text": prefix + parsed.text + "\n"}}]
        requests.extend(style_requests(parsed, base, tid))
        svc.documents().batchUpdate(documentId=did, body={"requests": requests}).execute()


def _table_segment_count(segments: list) -> int:
    return sum(1 for kind, _ in segments if kind == "table")


@api_errors
def append_text(
    item: str,
    text: str,
    tab: str | None = None,
    color: str | None = None,
    markdown: bool = False,
    dry_run: bool = False,
) -> dict:
    """Append text to the end of a Google Doc (item = URL or ID).

    markdown=true renders a small dialect instead of storing text verbatim: '#'..'######'
    headings, '-'/'*' bullets, '1.' numbered lists (nest with two spaces or a tab per level),
    **bold**, *italic*, <u>underline</u>, and GFM pipe tables (a header row immediately followed
    by a '| --- | --- |' delimiter row; write a literal pipe inside a cell as '\\|'). No other
    escape syntax — text that looks like markup gets styled; leave markdown unset to write
    literally. Block content (headings/lists/tables) starts on its own paragraph rather than
    merging into the doc's last line. Multi-tab docs: appends to the given `tab` (id, or an
    `item` URL with `tab=t.xxxx`), else the first tab. `color` (hex like '#3366CC') colors the
    inserted text (ignored for tables); leave it unset for plain text. dry_run=true returns a
    predicted before/after of the tab's tail without writing.
    """
    did = parse_ref(item).id
    doc = docs().documents().get(documentId=did, includeTabsContent=True).execute()
    tid, body = _resolve_write_tab(doc, tab or parse_tab(item))
    start = body[-1]["endIndex"] - 1

    if markdown:
        segments = split_blocks(text)
        if has_table(segments):  # structural (table) content -> segmented write path
            n_tables = _table_segment_count(segments)
            if dry_run:
                return {"dry_run": True, "action": "append_text", "tab": tid, "at_index": start,
                        "tables": n_tables, "preview": render_markdown_preview(segments)}
            tail_nonempty = bool(_para_text(body[-1].get("paragraph", {})).rstrip("\n"))
            _insert_markdown_segments(docs(), did, tid, start, segments, lead_newline=tail_nonempty)
            return {"document_id": did, "tab": tid, "inserted_at": start, "tables": n_tables}

    rgb = _hex_to_rgb(color) if color is not None else None  # validate before any dry-run return
    parsed = parse_markdown(text) if markdown else None
    insert = parsed.text if parsed else text
    base = start
    if parsed and parsed.has_blocks and _para_text(body[-1].get("paragraph", {})).rstrip("\n"):
        insert = "\n" + insert  # blocks start on a fresh paragraph, not inside the tail one
        base = start + 1
    if dry_run:
        tail = _content_to_text(body)[-200:]
        out = {
            "dry_run": True,
            "action": "append_text",
            "tab": tid,
            "at_index": start,
            "chars": len(insert),
            "color": color,
            "before_tail": tail,
            "after_tail": tail + insert,
        }
        if parsed:
            out["markdown"] = _md_summary(parsed)
        return out
    requests: list[dict] = [{"insertText": {"location": _loc(start, tid), "text": insert}}]
    if rgb is not None:
        requests.append(_color_request(start, u16len(insert), tid, rgb))
    if parsed:
        requests.extend(style_requests(parsed, base, tid))
    docs().documents().batchUpdate(documentId=did, body={"requests": requests}).execute()
    result = {"document_id": did, "tab": tid, "inserted_at": start, "chars": len(insert), "color": color}
    if parsed:
        result["markdown"] = _md_summary(parsed)
    return result


@api_errors
def insert_text(
    item: str,
    text: str,
    index: int,
    tab: str | None = None,
    color: str | None = None,
    markdown: bool = False,
    dry_run: bool = False,
) -> dict:
    """Insert text at a character index in a Google Doc (item = URL or ID).

    markdown=true renders the same dialect as append_text (headings, bullets, numbered lists,
    **bold**, *italic*, <u>underline</u>, and GFM pipe tables); note heading/list styles apply to
    the whole paragraphs the insertion touches, so insert block markdown at a paragraph boundary.
    Multi-tab docs: pass a `tab` (id, or an `item` URL with `tab=t.xxxx`), else the first tab.
    `color` (hex like '#3366CC') colors the inserted text (ignored for tables); leave it unset for
    plain text. dry_run=true reports what would be inserted and where without writing. (`index` is
    a Docs structural offset, so the exact merged text isn't reconstructed client-side.)
    """
    did = parse_ref(item).id
    doc = docs().documents().get(documentId=did, includeTabsContent=True).execute()
    tid, _body = _resolve_write_tab(doc, tab or parse_tab(item))

    if markdown:
        segments = split_blocks(text)
        if has_table(segments):  # structural (table) content -> segmented write path
            n_tables = _table_segment_count(segments)
            if dry_run:
                return {"dry_run": True, "action": "insert_text", "tab": tid, "at_index": index,
                        "tables": n_tables, "preview": render_markdown_preview(segments)}
            _insert_markdown_segments(docs(), did, tid, index, segments, lead_newline=False)
            return {"document_id": did, "tab": tid, "inserted_at": index, "tables": n_tables}

    rgb = _hex_to_rgb(color) if color is not None else None  # validate before any dry-run return
    parsed = parse_markdown(text) if markdown else None
    insert = parsed.text if parsed else text
    if dry_run:
        out = {
            "dry_run": True,
            "action": "insert_text",
            "tab": tid,
            "at_index": index,
            "chars": len(insert),
            "color": color,
            "would_insert": insert,
        }
        if parsed:
            out["markdown"] = _md_summary(parsed)
        return out
    requests: list[dict] = [{"insertText": {"location": _loc(index, tid), "text": insert}}]
    if rgb is not None:
        requests.append(_color_request(index, u16len(insert), tid, rgb))
    if parsed:
        requests.extend(style_requests(parsed, index, tid))
    docs().documents().batchUpdate(documentId=did, body={"requests": requests}).execute()
    result = {"document_id": did, "tab": tid, "inserted_at": index, "chars": len(insert), "color": color}
    if parsed:
        result["markdown"] = _md_summary(parsed)
    return result


@api_errors
def create_document(title: str, text: str | None = None, markdown: bool = False) -> dict:
    """Create a new Google Doc (in My Drive root), optionally with initial text content.

    markdown=true renders `text` with the same dialect as append_text: headings, bullets,
    numbered lists, **bold**, *italic*, <u>underline</u>, and GFM pipe tables. (For spreadsheets
    use create_spreadsheet.)
    """
    svc = docs()
    doc = svc.documents().create(body={"title": title}, fields="documentId,title").execute()
    did = doc["documentId"]
    result = {
        "id": did,
        "url": f"https://docs.google.com/document/d/{did}/edit",
        "title": doc.get("title"),
    }
    if text:
        segments = split_blocks(text) if markdown else None
        if segments is not None and has_table(segments):  # structural (table) content
            _insert_markdown_segments(svc, did, None, 1, segments, lead_newline=False)
            result["tables"] = _table_segment_count(segments)
            return result
        parsed = parse_markdown(text) if markdown else None
        insert = parsed.text if parsed else text
        requests: list[dict] = [{"insertText": {"location": {"index": 1}, "text": insert}}]
        if parsed:
            requests.extend(style_requests(parsed, 1, None))
        svc.documents().batchUpdate(documentId=did, body={"requests": requests}).execute()
        result["chars"] = len(insert)
        if parsed:
            result["markdown"] = _md_summary(parsed)
    return result


@api_errors
def insert_table(
    item: str,
    rows: list,
    index: int | None = None,
    tab: str | None = None,
    header: bool = False,
    dry_run: bool = False,
) -> dict:
    """Insert a table into a Google Doc (item = URL or ID), filled from `rows`.

    `rows` is a list of equal-length row lists (e.g. [["Name","Role"],["Ada","Eng"]]); empty cells
    are left blank. header=true bolds the first row. Appends at the end of the target tab by
    default; pass `index` to place it — `index` is a Docs STRUCTURAL offset at a paragraph boundary
    (not a character count from read_document), so prefer appending (omit index) unless you have a
    boundary offset. Multi-tab docs: pass a `tab` id (or an `item` URL with `tab=t.xxxx`), else the
    first tab. dry_run=true previews the table without writing.
    """
    n_rows, n_cols = _validate_rows(rows)
    did = parse_ref(item).id
    doc = docs().documents().get(documentId=did, includeTabsContent=True).execute()
    tid, body = _resolve_write_tab(doc, tab or parse_tab(item))
    start = body[-1]["endIndex"] - 1 if index is None else index

    if dry_run:
        return {
            "dry_run": True,
            "action": "insert_table",
            "tab": tid,
            "at_index": start,
            "rows": n_rows,
            "cols": n_cols,
            "header": header,
            "preview": render_table_markdown(rows),
        }

    svc = docs()
    pre = _table_starts(body)
    try:
        svc.documents().batchUpdate(
            documentId=did,
            body={"requests": [{"insertTable": {"rows": n_rows, "columns": n_cols, "location": _loc(start, tid)}}]},
        ).execute()
    except HttpError as exc:
        if index is not None and getattr(exc.resp, "status", None) == 400:
            raise RuntimeError(
                f"could not insert a table at index {index}: index must be a Docs structural offset at "
                f"a paragraph boundary (not a character count from read_document). Omit index to append."
            ) from exc
        raise

    def content(r: int, c: int):
        text = _cell_text(rows[r][c])
        spans = [(0, u16len(text), ("bold",))] if (header and r == 0 and text) else []
        return text, spans

    filled = _fill_new_table(svc, did, tid, pre, rows, content)
    return {
        "document_id": did,
        "tab": tid,
        "inserted_at": start,
        "rows": n_rows,
        "cols": n_cols,
        "cells_filled": filled,
        "header": header,
    }


@api_errors
def read_comments(item: str, page_size: int = 100, page_token: str | None = None) -> dict:
    """List comments on a Doc or file (item = URL or ID).

    Returns one page; when `has_more` is true, pass the returned `next_page_token` to continue.
    """
    fid = parse_ref(item).id
    resp = (
        drive()
        .comments()
        .list(
            fileId=fid,
            pageSize=max(1, min(page_size, 100)),
            pageToken=page_token,
            fields=(
                "nextPageToken, comments(id,author/displayName,content,resolved,createdTime,"
                "replies(content,author/displayName))"
            ),
        )
        .execute()
    )
    token = resp.get("nextPageToken")
    return {"file_id": fid, "comments": resp.get("comments", []), "has_more": bool(token), "next_page_token": token}


@api_errors
def add_comment(item: str, content: str) -> dict:
    """Add an (unanchored) comment to a Doc or file (item = URL or ID)."""
    fid = parse_ref(item).id
    c = drive().comments().create(fileId=fid, body={"content": content}, fields="id,content").execute()
    return {"file_id": fid, "comment_id": c["id"]}


_TOOLS = (
    read_document,
    extract_images,
    create_document,
    append_text,
    insert_text,
    insert_table,
    read_comments,
    add_comment,
)
