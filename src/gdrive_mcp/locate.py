"""Locators -> Docs index ranges: resolve human-meaningful anchors to API offsets.

The Docs API's Range/Location indexes are UTF-16 code units in a per-tab index space, and the
markdown a caller reads back is lossy (heading prefixes, a synthesized table delimiter row,
escaped pipes, stripped trailing newlines). A caller therefore can never compute a valid index
from what it read. These helpers resolve a locator against the API's *own* element offsets
instead, so no offset is ever derived from rendered text.

Two locator forms:
  - match:   a literal substring, resolved per-paragraph (including paragraphs inside table
             cells). A needle spanning a paragraph break never matches — the same limit the
             API's own replaceAllText has, and what keeps a range from straddling a table cell
             boundary (which deleteContentRange rejects).
  - section: a heading's text, resolved to that heading plus everything under it, up to the
             next heading of the same or higher level.

Pure index math over already-fetched body content: no service calls, no I/O.
"""

from __future__ import annotations

from gdrive_mcp.md import u16len

# (offset into the paragraph's flat text, document index of the run starting there) — both UTF-16.
_Run = tuple[int, int]

_HEADING_LEVELS = {f"HEADING_{i}": i for i in range(1, 7)}


def _walk_paragraphs(content: list):
    """Every paragraph element in document order, descending into table cells."""
    for el in content:
        if "paragraph" in el:
            yield el
        elif "table" in el:
            for row in el["table"].get("tableRows", []):
                for cell in row.get("tableCells", []):
                    yield from _walk_paragraphs(cell.get("content", []))


def _para_runs(el: dict) -> tuple[str, list[_Run]]:
    """A paragraph's flat text plus the breakpoints mapping flat offsets to document indexes.

    Each text run records its own startIndex, so elements that occupy index space without
    contributing text (inline images, footnote references) never desync the mapping.
    """
    parts: list[str] = []
    runs: list[_Run] = []
    off = 0
    for pe in el.get("paragraph", {}).get("elements", []):
        tr = pe.get("textRun")
        if tr is None:
            continue
        content = tr.get("content", "")
        runs.append((off, pe["startIndex"]))
        parts.append(content)
        off += u16len(content)
    return "".join(parts), runs


def _to_doc_index(runs: list[_Run], u16_off: int) -> int:
    """Map a UTF-16 offset in a paragraph's flat text back to a document index."""
    base_off, base_idx = runs[0]
    for off, idx in runs:
        if off > u16_off:
            break
        base_off, base_idx = off, idx
    return base_idx + (u16_off - base_off)


def find_matches(body: list, needle: str) -> list[tuple[int, int]]:
    """Every occurrence of `needle` as (start, end) document ranges, in document order.

    Matching is per-paragraph and case-sensitive. Case-insensitive matching is deliberately not
    offered: folding can change a string's length (`'İ'.lower()` is two characters), which
    would shift every offset computed from the folded text.
    """
    if not needle:
        raise RuntimeError("match must be a non-empty string")
    out: list[tuple[int, int]] = []
    for el in _walk_paragraphs(body):
        text, runs = _para_runs(el)
        if not runs:
            continue
        pos = text.find(needle)
        while pos != -1:
            end = pos + len(needle)
            # u16len(text[:n]) converts a code-point offset from str.find into the UTF-16
            # offset the Docs index space counts — they diverge on astral-plane characters.
            out.append((_to_doc_index(runs, u16len(text[:pos])), _to_doc_index(runs, u16len(text[:end]))))
            pos = text.find(needle, end)
    return out


def _heading_level(el: dict) -> int | None:
    style = el.get("paragraph", {}).get("paragraphStyle", {}).get("namedStyleType", "")
    return _HEADING_LEVELS.get(style)


def body_end(body: list) -> int:
    """The last index content may occupy: the body's final newline can never be deleted."""
    return body[-1]["endIndex"] - 1 if body else 1


def find_section(body: list, heading: str) -> tuple[int, int]:
    """(start, end) covering a heading paragraph and everything beneath it.

    Ends at the next heading of the same or higher level, else at the end of the tab. Only
    top-level elements are scanned — a heading inside a table cell is not a section. The range
    includes the heading's own trailing newline so deleting a section leaves no empty paragraph.
    """
    levels: list[tuple[int, int, int, int]] = []  # (position, level, startIndex, endIndex)
    for i, el in enumerate(body):
        level = _heading_level(el)
        if level is not None:
            levels.append((i, level, el["startIndex"], el["endIndex"]))

    hit = next((x for x in levels if _para_text(body[x[0]]).rstrip("\n") == heading), None)
    if hit is None:
        available = [_para_text(body[i]).rstrip("\n") for i, *_ in levels]
        raise RuntimeError(f"no heading titled {heading!r}; headings in this tab: {available}")

    pos, level, start, end = hit
    following = next((x for x in levels if x[0] > pos and x[1] <= level), None)
    stop = following[2] if following else body_end(body)
    # Clamp last: a heading that IS the final element has endIndex == body_end + 1, and a range
    # running through the body's final newline is rejected by deleteContentRange.
    return start, min(max(end, stop), body_end(body))


def _para_text(el: dict) -> str:
    return "".join(pe.get("textRun", {}).get("content", "") for pe in el.get("paragraph", {}).get("elements", []))


def resolve(body: list, match: str | None, section: str | None, occurrence: int) -> tuple[list[tuple[int, int]], str]:
    """Resolve a locator to (ranges, description). Exactly one of match/section is required.

    `occurrence` is 1-based over a `match`; 0 means every occurrence. It does not apply to
    `section` (heading titles resolve to a single span).
    """
    if (match is None) == (section is None):
        raise RuntimeError("pass exactly one of match= or section=")

    if section is not None:
        if occurrence != 1:
            raise RuntimeError("occurrence applies to match=, not section= (a heading resolves to one span)")
        return [find_section(body, section)], f"section {section!r}"

    hits = find_matches(body, match)
    if not hits:
        raise RuntimeError(f"no match for {match!r} in this tab (matching is case-sensitive and cannot span paragraphs)")
    if occurrence == 0:
        return hits, f"all {len(hits)} occurrences of {match!r}"
    if occurrence < 0:
        raise RuntimeError(f"occurrence must be >= 0 (0 means all); got {occurrence}")
    if occurrence > len(hits):
        raise RuntimeError(f"occurrence {occurrence} is out of range: {match!r} occurs {len(hits)} time(s)")
    return [hits[occurrence - 1]], f"occurrence {occurrence} of {match!r}"


def text_in_range(body: list, start: int, end: int) -> str:
    """The document text covered by [start, end) — the payload a delete/replace preview shows.

    Intersects per text run rather than per paragraph, so index jumps caused by non-text
    elements inside a paragraph don't skew the slice.
    """
    out: list[str] = []
    for el in _walk_paragraphs(body):
        text, runs = _para_runs(el)
        total = u16len(text)
        for i, (off, idx) in enumerate(runs):
            run_end_off = runs[i + 1][0] if i + 1 < len(runs) else total
            lo, hi = max(idx, start), min(idx + (run_end_off - off), end)
            if lo < hi:
                out.append(_slice_u16(text, off + (lo - idx), off + (hi - idx)))
    return "".join(out)


def _slice_u16(s: str, lo: int, hi: int) -> str:
    """Slice by UTF-16 offsets rather than code points (they differ past the BMP)."""
    return s.encode("utf-16-le")[lo * 2 : hi * 2].decode("utf-16-le", errors="ignore")


def paragraph_bounds(body: list, index: int) -> tuple[int, int] | None:
    """(start, end) of the top-level paragraph containing `index`, or None if there isn't one.

    insertTable rejects a location that is not at a paragraph boundary, so block-level inserts
    snap to one instead of landing mid-paragraph. Returns None when `index` falls inside a table
    (cell paragraphs are not top-level), which callers must treat as an error rather than
    silently anchoring mid-cell.
    """
    for el in body:
        if "paragraph" in el and el["startIndex"] <= index < el["endIndex"]:
            return el["startIndex"], el["endIndex"]
    return None
