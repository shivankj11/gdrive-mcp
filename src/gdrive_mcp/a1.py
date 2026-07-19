"""A1-notation helpers for the Sheets tools."""

from __future__ import annotations

import re

_CELL = re.compile(r"([A-Za-z]+)(\d+)")


def col_to_letter(index0: int) -> str:
    """0-based column index -> letters (0 -> 'A', 26 -> 'AA')."""
    if index0 < 0:
        raise ValueError("column index must be >= 0")
    letters = ""
    n = index0 + 1
    while n:
        n, rem = divmod(n - 1, 26)
        letters = chr(ord("A") + rem) + letters
    return letters


def letter_to_col(letters: str) -> int:
    """Column letters -> 0-based index ('A' -> 0, 'AA' -> 26)."""
    n = 0
    for ch in letters.upper():
        if not ("A" <= ch <= "Z"):
            raise ValueError(f"invalid column letters: {letters!r}")
        n = n * 26 + (ord(ch) - ord("A") + 1)
    return n - 1


def parse_cell(cell: str) -> tuple[int, int]:
    """'B3' -> (col0=1, row0=2)."""
    m = _CELL.fullmatch(cell.strip())
    if not m:
        raise ValueError(f"invalid A1 cell: {cell!r}")
    return letter_to_col(m.group(1)), int(m.group(2)) - 1


_TAB_QUOTED = re.compile(r"^'((?:[^']|'')*)'!(.*)$")


def split_range(a1: str) -> tuple[str | None, str]:
    """Split 'Tab!A1:B2' into (tab, 'A1:B2'), unquoting a quoted tab name; (None, a1) if no tab."""
    a1 = a1.strip()
    m = _TAB_QUOTED.match(a1)
    if m:
        return m.group(1).replace("''", "'"), m.group(2)
    tab, sep, rest = a1.partition("!")
    return (tab, rest) if sep else (None, a1)


def parse_range(a1: str) -> tuple[str | None, tuple[int, int, int, int]]:
    """'Tab!A2:C10' -> (tab, (start_col0, start_row0, end_col0, end_row0)), 0-based half-open.

    Bounded ranges only ('A2:C10', or a single cell 'B4'); open-ended ranges like 'A:C' are
    rejected (parse_cell requires an explicit row number).
    """
    tab, cells = split_range(a1)
    start, _, end = cells.partition(":")
    c0, r0 = parse_cell(start)
    c1, r1 = parse_cell(end) if end else (c0, r0)
    if c1 < c0 or r1 < r0:
        raise ValueError(f"range end must not precede its start: {a1!r}")
    return tab, (c0, r0, c1 + 1, r1 + 1)


def quote_tab(tab: str) -> str:
    """Quote a sheet/tab name for A1 notation, doubling embedded single quotes.

    Google Sheets escapes a `'` inside a quoted name by doubling it, so a tab like `John's Data`
    must render as `'John''s Data'` — without this the inner quote terminates the name early and
    the range is invalid or mis-targeted.
    """
    return "'" + tab.replace("'", "''") + "'"


def build_range(tab: str, start_cell: str, nrows: int, ncols: int) -> str:
    """Full A1 range covering an nrows x ncols block anchored at start_cell on tab."""
    c0, r0 = parse_cell(start_cell)
    end = f"{col_to_letter(c0 + max(ncols, 1) - 1)}{r0 + max(nrows, 1)}"
    prefix = f"{quote_tab(tab)}!" if tab else ""
    return f"{prefix}{start_cell.upper()}:{end}"
