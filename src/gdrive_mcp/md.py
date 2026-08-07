"""Markdown -> Google Docs styling: parse a small dialect into plain text + batchUpdate requests.

The Docs write tools render this opt-in (markdown=true). Dialect, kept deliberately small:

  - headings:   '# ' .. '###### ' at line start -> HEADING_1..6
  - bullets:    '- ' / '* ' / '+ ' at line start -> bulleted list
  - numbered:   '1. ' / '1) ' at line start -> numbered list
  - nesting:    two spaces or one tab of indent per list level (list text is inserted with
                leading tabs, which createParagraphBullets consumes to set the nesting level)
  - inline:     **bold**, __bold__, *italic*, _italic_, ***bold italic***, <u>underline</u>

There is no escape syntax: text that looks like markup gets styled (the plain, non-markdown
write path stores text verbatim). All offsets are UTF-16 code units — the unit the Docs API's
Range/Location indexes count (Python len() undercounts astral-plane chars like emoji).
"""

from __future__ import annotations

import re
from dataclasses import dataclass


def u16len(s: str) -> int:
    """Length in UTF-16 code units, the unit of Docs API indexes."""
    return len(s.encode("utf-16-le")) // 2


_HEADING_RE = re.compile(r"^(#{1,6}) (.*)$")
_BULLET_RE = re.compile(r"^(?P<indent>[\t ]*)[-*+] (?P<content>.*)$")
_NUMBER_RE = re.compile(r"^(?P<indent>[\t ]*)\d{1,9}[.)] (?P<content>.*)$")

# Alternation order matters: longer delimiters first so '***' isn't eaten as '*' + '**'.
# Openers must not be followed by whitespace (closers not preceded by it) so 'a * b' stays
# literal, and '_' emphasis requires non-word boundaries so snake_case stays literal.
_INLINE_RE = re.compile(
    r"\*\*\*(?!\s)(?P<bi>.+?)(?<!\s)\*\*\*"
    r"|\*\*(?!\s)(?P<b>.+?)(?<!\s)\*\*"
    r"|\*(?!\s)(?P<i>[^*]+?)(?<!\s)\*"
    r"|(?<!\w)__(?!\s)(?P<b2>.+?)(?<!\s)__(?!\w)"
    r"|(?<!\w)_(?!\s)(?P<i2>[^_]+?)(?<!\s)_(?!\w)"
    r"|<u>(?P<u>.+?)</u>",
)

_GROUP_STYLES = {
    "bi": ("bold", "italic"),
    "b": ("bold",),
    "b2": ("bold",),
    "i": ("italic",),
    "i2": ("italic",),
    "u": ("underline",),
}

_BULLET_PRESETS = {"bullet": "BULLET_DISC_CIRCLE_SQUARE", "number": "NUMBERED_DECIMAL_ALPHA_ROMAN"}

_MAX_NESTING = 8  # Docs lists support nesting levels 0..8


@dataclass(frozen=True)
class ParsedMarkdown:
    text: str  # plain text to insert: markers stripped, list nesting encoded as leading tabs
    spans: list[tuple[int, int, tuple[str, ...]]]  # inline style spans (start, end, style names)
    headings: list[tuple[int, int, int]]  # (start, end, level), one per heading line
    normal_runs: list[tuple[int, int]]  # contiguous plain-line runs
    nonlist_runs: list[tuple[int, int]]  # contiguous non-list-line runs (headings included)
    list_runs: list[tuple[int, int, str]]  # contiguous list runs, kind 'bullet' | 'number'
    list_items: int

    @property
    def has_blocks(self) -> bool:
        """Whether any paragraph-level construct (heading or list line) is present."""
        return bool(self.headings or self.list_runs)


def _indent_level(indent: str) -> int:
    tabs = indent.count("\t")
    return min(tabs + (len(indent) - tabs) // 2, _MAX_NESTING)


def _parse_inline(src: str) -> tuple[str, list[tuple[int, int, tuple[str, ...]]]]:
    """One line's inline markup -> (plain text, spans). Nested emphasis recurses; the outer
    span and each inner span become separate (overlapping) style requests."""
    parts: list[str] = []
    spans: list[tuple[int, int, tuple[str, ...]]] = []
    out = pos = 0
    for m in _INLINE_RE.finditer(src):
        literal = src[pos : m.start()]
        parts.append(literal)
        out += u16len(literal)
        group = next(g for g in _GROUP_STYLES if m.group(g) is not None)
        inner_plain, inner_spans = _parse_inline(m.group(group))
        spans.append((out, out + u16len(inner_plain), _GROUP_STYLES[group]))
        spans.extend((out + s, out + e, st) for s, e, st in inner_spans)
        parts.append(inner_plain)
        out += u16len(inner_plain)
        pos = m.end()
    parts.append(src[pos:])
    return "".join(parts), spans


def _runs(metas: list[tuple[int, int, str]], key) -> list[tuple[int, int, str]]:
    """Merge consecutive lines whose key(kind) is truthy (and equal) into (start, end, key) runs."""
    out: list[tuple[int, int, str]] = []
    for s, e, kind in metas:
        k = key(kind)
        if not k or s == e:  # s == e: an empty final line — nothing to style
            continue
        if out and out[-1][1] == s and out[-1][2] == k:
            out[-1] = (out[-1][0], e, k)
        else:
            out.append((s, e, k))
    return out


def parse_markdown(md: str) -> ParsedMarkdown:
    """Parse the dialect into insertable plain text plus style metadata (UTF-16 offsets).

    Line ranges include their trailing newline (except the final line, whose paragraph is
    closed by whatever follows the insertion point), so runs over blank lines stay contiguous
    and empty paragraphs still get styled.
    """
    parts: list[str] = []
    spans: list[tuple[int, int, tuple[str, ...]]] = []
    metas: list[tuple[int, int, str]] = []  # (start, end incl. trailing newline, kind)
    list_items = 0
    off = 0
    lines = md.split("\n")
    for n, raw in enumerate(lines):
        kind, prefix, content = "normal", "", raw
        if m := _HEADING_RE.match(raw):
            kind, content = f"h{len(m.group(1))}", m.group(2)
        elif m := _BULLET_RE.match(raw):
            kind, content = "bullet", m.group("content")
            prefix = "\t" * _indent_level(m.group("indent"))
        elif m := _NUMBER_RE.match(raw):
            kind, content = "number", m.group("content")
            prefix = "\t" * _indent_level(m.group("indent"))
        if kind in _BULLET_PRESETS:
            list_items += 1
        plain, inline = _parse_inline(content)
        start, text_start = off, off + len(prefix)  # tabs are one UTF-16 unit each
        spans.extend((text_start + s, text_start + e, st) for s, e, st in inline)
        end_of_text = text_start + u16len(plain)
        off = end_of_text if n == len(lines) - 1 else end_of_text + 1  # +1: the '\n' separator
        metas.append((start, off, kind))
        parts.append(prefix + plain)

    return ParsedMarkdown(
        text="\n".join(parts),
        spans=spans,
        headings=[(s, e, int(k[1])) for s, e, k in metas if k.startswith("h") and s < e],
        normal_runs=[(s, e) for s, e, _ in _runs(metas, lambda k: k == "normal" and "normal")],
        nonlist_runs=[(s, e) for s, e, _ in _runs(metas, lambda k: k not in _BULLET_PRESETS and "nonlist")],
        list_runs=_runs(metas, lambda k: k if k in _BULLET_PRESETS else None),
        list_items=list_items,
    )


def style_requests(parsed: ParsedMarkdown, base: int, tab_id: str | None) -> list[dict]:
    """The batchUpdate styling requests for parsed markdown inserted at index `base`.

    Paragraph-level styling only happens when the markdown has block constructs; inline-only
    markdown never restyles the paragraphs it lands in. Request order is load-bearing: style
    updates first (they never move indexes), createParagraphBullets last and bottom-up,
    because it consumes the leading nesting tabs, shifting every index past the consumed run.
    """

    def rng(s: int, e: int) -> dict:
        r: dict = {"startIndex": base + s, "endIndex": base + e}
        if tab_id:
            r["tabId"] = tab_id
        return r

    reqs: list[dict] = []
    for s, e, styles in parsed.spans:
        reqs.append(
            {
                "updateTextStyle": {
                    "range": rng(s, e),
                    "textStyle": {name: True for name in styles},
                    "fields": ",".join(styles),
                }
            }
        )
    if parsed.has_blocks:
        for s, e in parsed.normal_runs:
            reqs.append(_para_style(rng(s, e), "NORMAL_TEXT"))
        for s, e, level in parsed.headings:
            reqs.append(_para_style(rng(s, e), f"HEADING_{level}"))
        # Inserted paragraphs inherit the insertion point's list membership; strip it so
        # non-list markdown lines don't continue a pre-existing bulleted/numbered list.
        for s, e in parsed.nonlist_runs:
            reqs.append({"deleteParagraphBullets": {"range": rng(s, e)}})
    for s, e, kind in sorted(parsed.list_runs, reverse=True):
        reqs.append(
            {"createParagraphBullets": {"range": rng(s, e), "bulletPreset": _BULLET_PRESETS[kind]}}
        )
    return reqs


def _para_style(rng: dict, named_style: str) -> dict:
    return {
        "updateParagraphStyle": {
            "range": rng,
            "paragraphStyle": {"namedStyleType": named_style},
            "fields": "namedStyleType",
        }
    }


# ---- pipe tables (Option B) --------------------------------------------------
# Markdown is split into ordered text/table segments. A table is a pipe row immediately
# followed by a delimiter row (GFM) — requiring the delimiter keeps a lone '| a | b |' line
# literal text, so table parsing never changes existing table-free markdown writes. '\|' is the
# one escape the dialect honors, and only inside table cells, so a cell can carry a literal '|'
# (this closes the read->write loop with the reader, which emits '\|' for pipes in cell text).

_DELIM_CELL_RE = re.compile(r"^:?-+:?$")
_UNESCAPED_PIPE_RE = re.compile(r"(?<!\\)\|")


def _has_pipe(line: str) -> bool:
    return _UNESCAPED_PIPE_RE.search(line) is not None


def split_row(line: str) -> list[str]:
    """A pipe-table row -> cells: split on unescaped '|', drop the bounding empties, unescape '\\|'."""
    parts = _UNESCAPED_PIPE_RE.split(line.strip())
    if parts and parts[0].strip() == "":
        parts = parts[1:]
    if parts and parts[-1].strip() == "":
        parts = parts[:-1]
    return [p.strip().replace("\\|", "|") for p in parts]


def _is_delimiter(line: str) -> bool:
    if not _has_pipe(line):
        return False
    cells = split_row(line)
    return bool(cells) and all(_DELIM_CELL_RE.match(c) for c in cells)


def split_blocks(md: str) -> list[tuple[str, object]]:
    """Split markdown into ordered ('text', str) and ('table', rows) segments.

    `rows` is list[list[str]] of raw cell source (inline markup preserved for later styling);
    body rows are padded/truncated to the header's column count. A single-row table (header +
    delimiter, no body rows) yields exactly one row. Text runs between tables are joined with
    newlines; empty runs are still emitted and skipped by the renderer.
    """
    lines = md.split("\n")
    n = len(lines)
    segments: list[tuple[str, object]] = []
    buf: list[str] = []
    i = 0
    while i < n:
        if _has_pipe(lines[i]) and i + 1 < n and _is_delimiter(lines[i + 1]):
            if buf:
                segments.append(("text", "\n".join(buf)))
                buf = []
            header = split_row(lines[i])
            ncols = len(header)
            rows = [header]
            i += 2  # consume header + delimiter
            while i < n and _has_pipe(lines[i]) and not _is_delimiter(lines[i]):
                cells = split_row(lines[i])
                if len(cells) < ncols:
                    cells += [""] * (ncols - len(cells))
                elif len(cells) > ncols:
                    cells = cells[:ncols]
                rows.append(cells)
                i += 1
            segments.append(("table", rows))
        else:
            buf.append(lines[i])
            i += 1
    if buf:
        segments.append(("text", "\n".join(buf)))
    return segments


def has_table(segments: list[tuple[str, object]]) -> bool:
    return any(kind == "table" for kind, _ in segments)


def parse_cell(src: str) -> tuple[str, list[tuple[int, int, tuple[str, ...]]]]:
    """A table cell's source -> (plain text, inline style spans) — same inline dialect as prose."""
    return _parse_inline(src)


def escape_cell(value: object) -> str:
    """Render one cell value for markdown output: flatten newlines, escape '|' so it stays one cell."""
    return ("" if value is None else str(value)).replace("\n", " ").replace("|", "\\|")


def render_table_markdown(rows: list[list[object]]) -> str:
    """Rows -> a GFM pipe table (header + column-matched delimiter + body), cells escaped."""
    ncols = len(rows[0]) if rows else 0
    out: list[str] = []
    for r, row in enumerate(rows):
        out.append("| " + " | ".join(escape_cell(c) for c in row) + " |")
        if r == 0:
            out.append("| " + " | ".join(["---"] * ncols) + " |")
    return "\n".join(out)


def render_markdown_preview(segments: list[tuple[str, object]]) -> str:
    """A plain-text preview of segmented markdown (tables shown as pipe rows) for dry-run."""
    parts: list[str] = []
    for kind, payload in segments:
        if kind == "table":
            parts.append(render_table_markdown(payload))
        else:
            parts.append(parse_markdown(payload).text)
    return "\n".join(p for p in parts if p)
