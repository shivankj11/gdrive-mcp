"""Google Docs tools: read text/markdown, extract images, create, append/insert (opt-in
markdown formatting), comments.

Every tool references its target Doc/file via `item` (a URL or ID).
"""

from __future__ import annotations

import re
from urllib.parse import urlparse

from mcp.server.fastmcp import Image

from gdrive_mcp.chunking import DEFAULT_MAX_CHARS, paginate
from gdrive_mcp.clients import authed_session, docs, drive
from gdrive_mcp.errors import api_errors
from gdrive_mcp.ids import parse_ref, parse_tab
from gdrive_mcp.md import ParsedMarkdown, parse_markdown, style_requests, u16len

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
            for row in table.get("tableRows", []):
                cells = []
                for cell in row.get("tableCells", []):
                    ctext = "".join(
                        _para_text(c["paragraph"])
                        for c in cell.get("content", [])
                        if "paragraph" in c
                    )
                    cells.append(ctext.strip().replace("\n", " "))
                lines.append("| " + " | ".join(cells) + " |")
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
    **bold**, *italic*, <u>underline</u>. No escape syntax — text that looks like markup gets
    styled; leave markdown unset to write literally. Block content (headings/lists) starts on
    its own paragraph rather than merging into the doc's last line. Multi-tab docs: appends to
    the given `tab` (id, or an `item` URL with `tab=t.xxxx`), else the first tab. `color` (hex
    like '#3366CC') colors the inserted text; leave it unset for plain text. dry_run=true
    returns a predicted before/after of the tab's tail without writing.
    """
    did = parse_ref(item).id
    doc = docs().documents().get(documentId=did, includeTabsContent=True).execute()
    tid, body = _resolve_write_tab(doc, tab or parse_tab(item))
    rgb = _hex_to_rgb(color) if color is not None else None  # validate before any dry-run return
    parsed = parse_markdown(text) if markdown else None
    insert = parsed.text if parsed else text
    start = body[-1]["endIndex"] - 1
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
    location: dict = {"index": start}
    if tid:
        location["tabId"] = tid
    requests: list[dict] = [{"insertText": {"location": location, "text": insert}}]
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
    **bold**, *italic*, <u>underline</u>); note heading/list styles apply to the whole
    paragraphs the insertion touches, so insert block markdown at a paragraph boundary.
    Multi-tab docs: pass a `tab` (id, or an `item` URL with `tab=t.xxxx`), else the first tab.
    `color` (hex like '#3366CC') colors the inserted text; leave it unset for plain text.
    dry_run=true reports what would be inserted and where without writing. (`index` is a Docs
    structural offset, so the exact merged text isn't reconstructed client-side.)
    """
    did = parse_ref(item).id
    doc = docs().documents().get(documentId=did, includeTabsContent=True).execute()
    tid, _body = _resolve_write_tab(doc, tab or parse_tab(item))
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
    location: dict = {"index": index}
    if tid:
        location["tabId"] = tid
    requests: list[dict] = [{"insertText": {"location": location, "text": insert}}]
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
    numbered lists, **bold**, *italic*, <u>underline</u>. (For spreadsheets use
    create_spreadsheet.)
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
    read_comments,
    add_comment,
)
