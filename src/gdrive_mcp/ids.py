"""Parse a Google Drive/Docs/Sheets URL or bare ID into an id + a kind hint."""

from __future__ import annotations

import re
from dataclasses import dataclass

_ID = r"([a-zA-Z0-9_-]+)"

_PATTERNS = [
    (re.compile(rf"docs\.google\.com/document/d/{_ID}"), "document"),
    (re.compile(rf"docs\.google\.com/spreadsheets/d/{_ID}"), "spreadsheet"),
    (re.compile(rf"docs\.google\.com/presentation/d/{_ID}"), "file"),
    (re.compile(rf"drive\.google\.com/drive/folders/{_ID}"), "folder"),
    (re.compile(rf"drive\.google\.com/file/d/{_ID}"), "file"),
    (re.compile(rf"[?&]id={_ID}"), "file"),
]

_BARE_ID = re.compile(r"^[a-zA-Z0-9_-]{20,}$")


@dataclass(frozen=True)
class Ref:
    id: str
    kind: str  # document | spreadsheet | folder | file | unknown


def parse_ref(url_or_id: str) -> Ref:
    s = (url_or_id or "").strip()
    for pattern, kind in _PATTERNS:
        m = pattern.search(s)
        if m:
            return Ref(m.group(1), kind)
    if _BARE_ID.match(s):
        return Ref(s, "unknown")
    raise ValueError(f"could not parse a Drive ID from: {url_or_id!r}")


_TAB = re.compile(r"[?&#]tab=(t\.[A-Za-z0-9]+)")


def parse_tab(url_or_id: str) -> str | None:
    """Extract a Google Docs tab id (e.g. t.70s4vio5pxrm) from a doc URL, if present."""
    m = _TAB.search(url_or_id or "")
    return m.group(1) if m else None
