"""Google Sheets tools: read (incl. formulas), write, append, format, create, clear, delete.

Every tool references its target spreadsheet via `item` (a URL or ID).
"""

from __future__ import annotations

import csv
from typing import Any, Literal

from gdrive_mcp.a1 import build_range, parse_range, quote_tab
from gdrive_mcp.clients import sheets
from gdrive_mcp.errors import api_errors
from gdrive_mcp.guard import preview_response
from gdrive_mcp.ids import parse_ref
from gdrive_mcp.localfs import safe_write_path

_RENDER = {
    "FORMATTED": "FORMATTED_VALUE",
    "UNFORMATTED": "UNFORMATTED_VALUE",
    "FORMULA": "FORMULA",
}

_VALUE_INPUT = {"RAW", "USER_ENTERED"}


def _input_option(value_input: str) -> str:
    """RAW (default) stores cell text literally; USER_ENTERED parses it like the UI (so a leading
    '=' becomes a live formula). Default RAW so agent-supplied text can't inject formulas."""
    vi = value_input.upper()
    if vi not in _VALUE_INPUT:
        raise RuntimeError(f"value_input must be RAW or USER_ENTERED, got {value_input!r}")
    return vi


def _sheet_titles(svc, sid: str) -> dict[str, int]:
    meta = (
        svc.spreadsheets()
        .get(spreadsheetId=sid, fields="sheets.properties(sheetId,title)")
        .execute()
    )
    return {s["properties"]["title"]: s["properties"]["sheetId"] for s in meta.get("sheets", [])}


def _records(values: list[list]) -> dict:
    if not values:
        return {"headers": [], "rows": [], "records": []}
    headers = [str(h) for h in values[0]]
    rows = values[1:]
    records = [dict(zip(headers, row + [None] * (len(headers) - len(row)))) for row in rows]
    return {"headers": headers, "rows": rows, "records": records}


def _count_nonempty(values: list[list]) -> int:
    return sum(1 for row in values for cell in row if cell not in ("", None))


@api_errors
def read_sheet(
    item: str,
    tab: str | None = None,
    max_rows: int = 5,
    max_cols: int = 5,
    a1_range: str | None = None,
    header_row: bool = True,
    values: str = "UNFORMATTED",
) -> dict:
    """Preview a spreadsheet (item = URL or ID): the first max_rows x max_cols (default 5x5) of each tab.

    A quick, context-safe peek. For the complete data use read_full_sheet (which saves it to a local
    file). Pass an explicit a1_range to read exactly that range instead of the preview window.
    values: UNFORMATTED | FORMATTED | FORMULA (FORMULA returns cell formulas).
    """
    sid = parse_ref(item).id
    svc = sheets()
    vr = _RENDER.get(values.upper(), "UNFORMATTED_VALUE")
    if a1_range:
        ranges = [a1_range]
    elif tab:
        ranges = [build_range(tab, "A1", max_rows, max_cols)]
    else:
        ranges = [build_range(t, "A1", max_rows, max_cols) for t in _sheet_titles(svc, sid)]
    out = []
    for rng in ranges:
        resp = svc.spreadsheets().values().get(spreadsheetId=sid, range=rng, valueRenderOption=vr).execute()
        vals = resp.get("values", [])
        entry = {"range": resp.get("range", rng)}
        if header_row:
            entry.update(_records(vals))
        else:
            entry["rows"] = vals
        out.append(entry)
    return {"spreadsheet_id": sid, "preview": True, "tabs": out}


@api_errors
def read_full_sheet(
    item: str, tab: str | None = None, dest_path: str | None = None, values: str = "UNFORMATTED"
) -> dict:
    """Read an entire sheet tab, save it to a local CSV, and return the path + a 5x5 preview.

    Reads the full tab (item = URL or ID; default first tab) via the Sheets API, writes it to
    dest_path (resolved inside the sandbox files dir; a sanitized default name otherwise), and
    returns total_rows/total_cols plus the same first-5-rows/cols preview as read_sheet. Use this
    instead of read_sheet when you need the complete data without flooding the context.
    """
    sid = parse_ref(item).id
    svc = sheets()
    if not tab:
        tab = next(iter(_sheet_titles(svc, sid)), None)
        if tab is None:
            raise RuntimeError("spreadsheet has no tabs")
    vr = _RENDER.get(values.upper(), "UNFORMATTED_VALUE")
    full = (
        svc.spreadsheets()
        .values()
        .get(spreadsheetId=sid, range=quote_tab(tab), valueRenderOption=vr)
        .execute()
        .get("values", [])
    )
    path = safe_write_path(dest_path, f"gdrive-{sid}-{tab}.csv")
    with path.open("w", newline="") as f:
        csv.writer(f).writerows(full)
    path.chmod(0o600)
    window = [row[:5] for row in full[:5]]
    return {
        "tab": tab,
        "path": str(path),
        "total_rows": len(full),
        "total_cols": max((len(r) for r in full), default=0),
        "preview": _records(window),
    }


@api_errors
def write_sheet(
    item: str,
    tab: str,
    rows: list[list[Any]],
    start_cell: str = "A1",
    value_input: Literal["RAW", "USER_ENTERED"] = "RAW",
    confirm: bool = False,
    dry_run: bool = False,
) -> dict:
    """Write rows to a tab of a spreadsheet (item), starting at start_cell (default A1).

    value_input RAW (default) stores cell text literally; pass 'USER_ENTERED' to interpret typed
    values/formulas (a leading '=' becomes a live formula). Overwriting existing non-empty cells
    requires confirm=true. dry_run=true returns a predicted before/after (client-side) without
    writing; formula cells are shown literally (Google's recalculation is not simulated).
    """
    sid = parse_ref(item).id
    svc = sheets()
    vio = _input_option(value_input)  # validate before any (dry-run) preview so it's consistent
    nrows = len(rows)
    ncols = max((len(r) for r in rows), default=0)
    target = build_range(tab, start_cell, nrows, ncols)
    existing = (
        svc.spreadsheets()
        .values()
        .get(spreadsheetId=sid, range=target, valueRenderOption="UNFORMATTED_VALUE")
        .execute()
        .get("values", [])
    )
    nonempty = _count_nonempty(existing)
    if dry_run:
        return {
            "dry_run": True,
            "action": "write_sheet",
            "target_range": target,
            "before": existing,
            "after": rows,
            "overwrites_nonempty_cells": nonempty,
            "formulas_not_recalculated": any(
                isinstance(c, str) and c.startswith("=") for row in rows for c in row
            ),
        }
    if nonempty and not confirm:
        return preview_response(
            "write_sheet",
            {"target_range": target, "rows_to_write": nrows, "overwrites_nonempty_cells": nonempty},
        )
    resp = (
        svc.spreadsheets()
        .values()
        .update(
            spreadsheetId=sid,
            range=target,
            valueInputOption=vio,
            body={"values": rows},
        )
        .execute()
    )
    return {"updated_range": resp.get("updatedRange"), "updated_cells": resp.get("updatedCells")}


@api_errors
def append_rows(
    item: str,
    tab: str,
    rows: list[list[Any]],
    value_input: Literal["RAW", "USER_ENTERED"] = "RAW",
    dry_run: bool = False,
) -> dict:
    """Append rows after the last row of a tab of a spreadsheet (item). Non-destructive.

    value_input RAW (default) stores cell text literally; 'USER_ENTERED' interprets formulas/typed
    values. dry_run=true returns where the rows would land (predicted) without appending.
    """
    sid = parse_ref(item).id
    vio = _input_option(value_input)  # validate before the dry-run branch for consistency
    if dry_run:
        current = (
            sheets()
            .spreadsheets()
            .values()
            .get(spreadsheetId=sid, range=quote_tab(tab), valueRenderOption="UNFORMATTED_VALUE")
            .execute()
            .get("values", [])
        )
        return {
            "dry_run": True,
            "action": "append_rows",
            "at_row": len(current) + 1,
            "after": rows,
            "appends_rows": len(rows),
        }
    resp = (
        sheets()
        .spreadsheets()
        .values()
        .append(
            spreadsheetId=sid,
            range=quote_tab(tab),
            valueInputOption=vio,
            insertDataOption="INSERT_ROWS",
            body={"values": rows},
        )
        .execute()
    )
    upd = resp.get("updates", {})
    return {"updated_range": upd.get("updatedRange"), "appended_rows": upd.get("updatedRows")}


@api_errors
def format_cells(
    item: str,
    a1_range: str,
    bold: bool | None = None,
    italic: bool | None = None,
    underline: bool | None = None,
    dry_run: bool = False,
) -> dict:
    """Set text formatting (bold/italic/underline) on a bounded A1 range of a spreadsheet (item).

    a1_range like 'Data!A1:C10' or a single cell 'Data!B2' (no tab prefix targets the first
    tab; open-ended ranges like 'A:C' are rejected). Each flag is tri-state: true applies it,
    false removes it, unset leaves that property as-is. Formatting only — cell values are
    untouched. dry_run=true returns the resolved target without writing. (Headings, bullets,
    and rich text are Docs concepts: see append_text/create_document with markdown=true.)
    """
    sid = parse_ref(item).id
    applied = {
        k: v for k, v in {"bold": bold, "italic": italic, "underline": underline}.items() if v is not None
    }
    if not applied:
        raise RuntimeError("nothing to format: pass at least one of bold/italic/underline")
    tab, (c0, r0, c1, r1) = parse_range(a1_range)
    svc = sheets()
    titles = _sheet_titles(svc, sid)
    if tab is None:
        tab = next(iter(titles), None)
        if tab is None:
            raise RuntimeError("spreadsheet has no tabs")
    if tab not in titles:
        raise RuntimeError(f"tab {tab!r} not found; available: {list(titles)}")
    cells = (r1 - r0) * (c1 - c0)
    if dry_run:
        return {
            "dry_run": True,
            "action": "format_cells",
            "tab": tab,
            "range": a1_range,
            "cells": cells,
            "applies": applied,
        }
    svc.spreadsheets().batchUpdate(
        spreadsheetId=sid,
        body={
            "requests": [
                {
                    "repeatCell": {
                        "range": {
                            "sheetId": titles[tab],
                            "startRowIndex": r0,
                            "endRowIndex": r1,
                            "startColumnIndex": c0,
                            "endColumnIndex": c1,
                        },
                        "cell": {"userEnteredFormat": {"textFormat": applied}},
                        "fields": ",".join(f"userEnteredFormat.textFormat.{k}" for k in sorted(applied)),
                    }
                }
            ]
        },
    ).execute()
    return {"tab": tab, "formatted_range": a1_range, "cells": cells, "applied": applied}


@api_errors
def create_spreadsheet(title: str, tabs: list[str] | None = None) -> dict:
    """Create a new spreadsheet with optional named tabs."""
    body: dict = {"properties": {"title": title}}
    if tabs:
        body["sheets"] = [{"properties": {"title": t}} for t in tabs]
    ss = (
        sheets()
        .spreadsheets()
        .create(body=body, fields="spreadsheetId,spreadsheetUrl")
        .execute()
    )
    return {"id": ss["spreadsheetId"], "url": ss["spreadsheetUrl"]}


@api_errors
def add_tab(item: str, title: str, index: int | None = None) -> dict:
    """Add a new tab (sheet) to an existing spreadsheet (item)."""
    sid = parse_ref(item).id
    props: dict = {"title": title}
    if index is not None:
        props["index"] = index
    resp = (
        sheets()
        .spreadsheets()
        .batchUpdate(spreadsheetId=sid, body={"requests": [{"addSheet": {"properties": props}}]})
        .execute()
    )
    p = resp["replies"][0]["addSheet"]["properties"]
    return {"sheet_id": p["sheetId"], "title": p["title"], "index": p.get("index")}


@api_errors
def clear_range(item: str, a1_range: str, confirm: bool = False, dry_run: bool = False) -> dict:
    """Clear all values in an A1 range of a spreadsheet (item), e.g. 'Data!A2:C10'. Requires confirm=true.

    dry_run=true returns the values that would be cleared (before) without clearing.
    """
    sid = parse_ref(item).id
    svc = sheets()
    existing = (
        svc.spreadsheets()
        .values()
        .get(spreadsheetId=sid, range=a1_range, valueRenderOption="UNFORMATTED_VALUE")
        .execute()
        .get("values", [])
    )
    nonempty = _count_nonempty(existing)
    if dry_run:
        return {
            "dry_run": True,
            "action": "clear_range",
            "range": a1_range,
            "before": existing,
            "after": [],
            "nonempty_cells_cleared": nonempty,
        }
    if not confirm:
        return preview_response("clear_range", {"range": a1_range, "nonempty_cells_cleared": nonempty})
    svc.spreadsheets().values().clear(spreadsheetId=sid, range=a1_range, body={}).execute()
    return {"cleared_range": a1_range, "cleared_nonempty_cells": nonempty}


@api_errors
def delete_rows(
    item: str, tab: str, start_row: int, count: int = 1, confirm: bool = False, dry_run: bool = False
) -> dict:
    """Delete `count` rows starting at start_row (1-based) from a tab of a spreadsheet (item). Requires confirm=true.

    dry_run=true returns the rows that would be deleted without deleting.
    """
    sid = parse_ref(item).id
    svc = sheets()
    titles = _sheet_titles(svc, sid)
    if tab not in titles:
        raise RuntimeError(f"tab {tab!r} not found; available: {list(titles)}")
    doomed = (
        svc.spreadsheets()
        .values()
        .get(
            spreadsheetId=sid,
            range=f"{quote_tab(tab)}!{start_row}:{start_row + count - 1}",
            valueRenderOption="UNFORMATTED_VALUE",
        )
        .execute()
        .get("values", [])
    )
    if dry_run:
        return {
            "dry_run": True,
            "action": "delete_rows",
            "tab": tab,
            "rows": f"{start_row}..{start_row + count - 1}",
            "would_delete": doomed,
            "deletes_rows": count,
        }
    if not confirm:
        return preview_response(
            "delete_rows",
            {"tab": tab, "start_row": start_row, "count": count, "sample": doomed[:5]},
        )
    svc.spreadsheets().batchUpdate(
        spreadsheetId=sid,
        body={
            "requests": [
                {
                    "deleteDimension": {
                        "range": {
                            "sheetId": titles[tab],
                            "dimension": "ROWS",
                            "startIndex": start_row - 1,
                            "endIndex": start_row - 1 + count,
                        }
                    }
                }
            ]
        },
    ).execute()
    return {"tab": tab, "deleted_rows": count}


_TOOLS = (
    read_sheet,
    read_full_sheet,
    write_sheet,
    append_rows,
    format_cells,
    create_spreadsheet,
    add_tab,
    clear_range,
    delete_rows,
)
