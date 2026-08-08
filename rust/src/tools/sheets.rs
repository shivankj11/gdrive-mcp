//! Google Sheets tools: read (incl. formulas), write, append, format, create, clear, delete.
//!
//! Every tool references its target spreadsheet via `item` (a URL or ID).

use std::path::Path;

use serde_json::{json, Map, Value};

use super::{ToolDef, ToolOutput};
use crate::a1::{build_range, parse_range, quote_tab};
use crate::args::Args;
use crate::clients::GoogleApi;
use crate::config::chmod;
use crate::error::{Result, ToolError};
use crate::guard::preview_response;
use crate::ids::parse_ref;
use crate::localfs::safe_write_path;
use crate::md::cell_text;

/// `values` -> the Sheets `valueRenderOption`. An unrecognised name falls back to
/// UNFORMATTED_VALUE, exactly as the Python `_RENDER.get(..., "UNFORMATTED_VALUE")` did.
fn render_option(values: &str) -> &'static str {
    match values.to_uppercase().as_str() {
        "FORMATTED" => "FORMATTED_VALUE",
        "FORMULA" => "FORMULA",
        _ => "UNFORMATTED_VALUE",
    }
}

/// RAW (default) stores cell text literally; USER_ENTERED parses it like the UI (so a leading
/// '=' becomes a live formula). Default RAW so agent-supplied text can't inject formulas.
///
/// Case-sensitive, matching the `Literal["RAW", "USER_ENTERED"]` annotation: pydantic validated
/// that before the body ran, so the `.upper()` in the Python was unreachable and `"user_entered"`
/// was rejected. Accepting it here would let a spelling the Python server refused turn on formula
/// interpretation.
fn input_option(value_input: &str) -> Result<String> {
    match value_input {
        "RAW" | "USER_ENTERED" => Ok(value_input.to_string()),
        _ => Err(ToolError::msg(format!("value_input must be RAW or USER_ENTERED, got '{value_input}'"))),
    }
}

/// (title, sheetId) for every tab, in the order the API reports them — "the first tab" and the
/// preview order both depend on that order, so this is a Vec rather than a hash map.
async fn sheet_titles(api: &dyn GoogleApi, sid: &str) -> Result<Vec<(String, i64)>> {
    let meta = api.sheets_get(sid, "sheets.properties(sheetId,title)").await?;
    let Some(sheets) = meta.get("sheets").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(sheets
        .iter()
        .filter_map(|s| {
            let props = s.get("properties")?;
            let title = props.get("title")?.as_str()?;
            // A tab with no reported sheetId is the id-0 first tab; dropping it instead would
            // silently shift which tab counts as "the first one".
            let id = props.get("sheetId").and_then(Value::as_i64).unwrap_or(0);
            Some((title.to_string(), id))
        })
        .collect())
}

/// Python's `list(titles)` repr, for the "tab not found" message: `['Data', 'Notes']`.
fn titles_repr(titles: &[(String, i64)]) -> String {
    let names: Vec<String> = titles.iter().map(|(t, _)| format!("'{t}'")).collect();
    format!("[{}]", names.join(", "))
}

fn tab_not_found(tab: &str, titles: &[(String, i64)]) -> ToolError {
    ToolError::msg(format!("tab '{tab}' not found; available: {}", titles_repr(titles)))
}

/// A values response's `values` as a (possibly ragged) grid of raw cell values.
fn grid(resp: &Value) -> Vec<Vec<Value>> {
    resp.get("values")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(|row| row.as_array().cloned().unwrap_or_default()).collect())
        .unwrap_or_default()
}

/// `json!({...})` as a `Map`; the literals this is applied to are always objects.
fn json_map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// headers / rows / records for a grid whose first row is the header row.
fn records(values: &[Vec<Value>]) -> Map<String, Value> {
    let Some(header_row) = values.first() else {
        return json_map(json!({"headers": [], "rows": [], "records": []}));
    };
    let headers: Vec<String> = header_row.iter().map(cell_text).collect();
    let rows = &values[1..];
    let records: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut rec = Map::new();
            // A short row is padded with nulls out to the header width, and `zip` drops any cell
            // past the last header — so a record holds exactly one entry per distinct header.
            for (i, header) in headers.iter().enumerate() {
                rec.insert(header.clone(), row.get(i).cloned().unwrap_or(Value::Null));
            }
            Value::Object(rec)
        })
        .collect();
    json_map(json!({"headers": headers, "rows": rows, "records": records}))
}

/// Python's `cell not in ("", None)`: only the empty string and null are "empty", so 0 and
/// false still count as data a write would destroy.
fn count_nonempty(values: &[Vec<Value>]) -> usize {
    values.iter().flatten().filter(|cell| !cell.is_null() && cell.as_str() != Some("")).count()
}

/// Spill a grid to CSV the way `csv.writer(f).writerows(rows)` did.
fn write_csv(path: &Path, rows: &[Vec<Value>]) -> Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::File::create(path)?;
    let mut i = 0;
    while i < rows.len() {
        // A wholly-empty sheet row (Sheets omits trailing empties, so a blank row comes back as
        // `[]`) is written by this crate as `""`, to keep an empty record distinguishable from a
        // blank line. Python's csv.writer just terminates it, so emit the terminator directly and
        // keep the spill byte-identical.
        if rows[i].is_empty() {
            file.write_all(b"\r\n")?;
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && !rows[i].is_empty() {
            i += 1;
        }
        let mut writer = csv::WriterBuilder::new()
            // Sheets returns ragged rows (trailing empties are omitted), which Python's csv.writer
            // wrote happily but this crate rejects unless told the record width may vary.
            .flexible(true)
            // Python's csv module terminates records with \r\n by default.
            .terminator(csv::Terminator::CRLF)
            .from_writer(&mut file);
        for row in &rows[start..i] {
            writer.write_record(row.iter().map(cell_text)).map_err(csv_error)?;
        }
        writer.flush()?;
    }
    Ok(())
}

fn csv_error(e: csv::Error) -> ToolError {
    let rendered = e.to_string();
    match e.into_kind() {
        // A failed local write reads the same as any other filesystem error.
        csv::ErrorKind::Io(io) => io.into(),
        _ => ToolError::msg(rendered),
    }
}

/// `d.get(key)` on a response object: the value, or null when the API omitted it.
fn field(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

async fn read_sheet(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let sid = parse_ref(&args.req_str("item")?)?.id;
    let tab = args.opt_str("tab")?;
    let max_rows = args.i64_or("max_rows", 5)?;
    let max_cols = args.i64_or("max_cols", 5)?;
    let a1_range = args.opt_str("a1_range")?;
    let header_row = args.bool_or("header_row", true)?;
    let vr = render_option(&args.str_or("values", "UNFORMATTED")?);

    // Both branches key off truthiness, so an empty string means "not given".
    let ranges: Vec<String> = match (a1_range.filter(|r| !r.is_empty()), tab.filter(|t| !t.is_empty())) {
        (Some(range), _) => vec![range],
        (None, Some(tab)) => vec![build_range(&tab, "A1", max_rows, max_cols)?],
        (None, None) => sheet_titles(api, &sid)
            .await?
            .iter()
            .map(|(title, _)| build_range(title, "A1", max_rows, max_cols))
            .collect::<Result<Vec<_>>>()?,
    };

    let mut out: Vec<Value> = Vec::new();
    for rng in &ranges {
        let resp = api.sheets_values_get(&sid, rng, vr).await?;
        let vals = grid(&resp);
        let mut entry = Map::new();
        entry.insert("range".into(), resp.get("range").cloned().unwrap_or_else(|| json!(rng)));
        if header_row {
            entry.extend(records(&vals));
        } else {
            entry.insert("rows".into(), json!(vals));
        }
        out.push(Value::Object(entry));
    }
    Ok(json!({"spreadsheet_id": sid, "preview": true, "tabs": out}).into())
}

async fn read_full_sheet(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let sid = parse_ref(&args.req_str("item")?)?.id;
    let mut tab = args.opt_str("tab")?.filter(|t| !t.is_empty());
    if tab.is_none() {
        tab = sheet_titles(api, &sid).await?.into_iter().next().map(|(title, _)| title);
    }
    let Some(tab) = tab else {
        return Err(ToolError::msg("spreadsheet has no tabs"));
    };
    let vr = render_option(&args.str_or("values", "UNFORMATTED")?);
    let full = grid(&api.sheets_values_get(&sid, &quote_tab(&tab), vr).await?);

    let path = safe_write_path(args.opt_str("dest_path")?.as_deref(), &format!("gdrive-{sid}-{tab}.csv"))?;
    write_csv(&path, &full)?;
    // The spill can hold sensitive data, so it becomes owner-only as soon as it exists on disk.
    chmod(&path, 0o600);

    let window: Vec<Vec<Value>> =
        full.iter().take(5).map(|row| row.iter().take(5).cloned().collect()).collect();
    Ok(json!({
        "tab": tab,
        "path": path.display().to_string(),
        "total_rows": full.len(),
        "total_cols": full.iter().map(Vec::len).max().unwrap_or(0),
        "preview": Value::Object(records(&window)),
    })
    .into())
}

async fn write_sheet(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let sid = parse_ref(&args.req_str("item")?)?.id;
    let tab = args.req_str("tab")?;
    let rows = args.req_rows("rows")?;
    let start_cell = args.str_or("start_cell", "A1")?;
    let value_input = args.str_or("value_input", "RAW")?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let vio = input_option(&value_input)?; // validate before any (dry-run) preview so it's consistent
    let nrows = rows.len() as i64;
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0) as i64;
    let target = build_range(&tab, &start_cell, nrows, ncols)?;
    // FORMULA, not UNFORMATTED_VALUE: a cell whose formula evaluates to the empty string
    // (`=IF(x,"",y)`, a lookup that misses) renders as "" under UNFORMATTED_VALUE, so
    // `count_nonempty` read it as empty, the gate did not fire, and a confirm-less write destroyed
    // the formula. Formula *text* is never empty, so the gate fires.
    let existing = grid(&api.sheets_values_get(&sid, &target, "FORMULA").await?);
    let nonempty = count_nonempty(&existing);

    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "action": "write_sheet",
            "target_range": target,
            "before": existing,
            "after": rows,
            "overwrites_nonempty_cells": nonempty,
            "formulas_not_recalculated": rows
                .iter()
                .flatten()
                .any(|c| c.as_str().is_some_and(|s| s.starts_with('='))),
        })
        .into());
    }
    if nonempty > 0 && !confirm {
        return Ok(preview_response(
            "write_sheet",
            json!({
                "target_range": target,
                "rows_to_write": nrows,
                "overwrites_nonempty_cells": nonempty,
            }),
        )
        .into());
    }
    let resp = api.sheets_values_update(&sid, &target, &vio, &json!(rows)).await?;
    Ok(json!({
        "updated_range": field(&resp, "updatedRange"),
        "updated_cells": field(&resp, "updatedCells"),
    })
    .into())
}

async fn append_rows(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let sid = parse_ref(&args.req_str("item")?)?.id;
    let tab = args.req_str("tab")?;
    let rows = args.req_rows("rows")?;
    let value_input = args.str_or("value_input", "RAW")?;
    let dry_run = args.bool_or("dry_run", false)?;

    let vio = input_option(&value_input)?; // validate before the dry-run branch for consistency
    if dry_run {
        let current = grid(&api.sheets_values_get(&sid, &quote_tab(&tab), "UNFORMATTED_VALUE").await?);
        return Ok(json!({
            "dry_run": true,
            "action": "append_rows",
            "at_row": current.len() + 1,
            "after": rows,
            "appends_rows": rows.len(),
        })
        .into());
    }
    let resp = api.sheets_values_append(&sid, &quote_tab(&tab), &vio, &json!(rows)).await?;
    let upd = resp.get("updates").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "updated_range": field(&upd, "updatedRange"),
        "appended_rows": field(&upd, "updatedRows"),
    })
    .into())
}

async fn format_cells(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let sid = parse_ref(&args.req_str("item")?)?.id;
    let a1_range = args.req_str("a1_range")?;
    let dry_run = args.bool_or("dry_run", false)?;

    // Tri-state: an unset flag never reaches the request, so that property keeps whatever it was.
    let mut applied = Map::new();
    for (name, flag) in [
        ("bold", args.opt_bool("bold")?),
        ("italic", args.opt_bool("italic")?),
        ("underline", args.opt_bool("underline")?),
    ] {
        if let Some(value) = flag {
            applied.insert(name.into(), Value::Bool(value));
        }
    }
    if applied.is_empty() {
        return Err(ToolError::msg("nothing to format: pass at least one of bold/italic/underline"));
    }

    let (parsed_tab, (c0, r0, c1, r1)) = parse_range(&a1_range)?;
    let titles = sheet_titles(api, &sid).await?;
    let tab = match parsed_tab {
        Some(t) => t,
        None => match titles.first() {
            Some((title, _)) => title.clone(),
            None => return Err(ToolError::msg("spreadsheet has no tabs")),
        },
    };
    let Some(sheet_id) = titles.iter().find(|(t, _)| *t == tab).map(|(_, id)| *id) else {
        return Err(tab_not_found(&tab, &titles));
    };
    // parse_range yields a half-open, non-inverted box; saturating arithmetic keeps an absurd
    // row/column number from overflowing on the way to a cell count.
    let cells = r1.saturating_sub(r0).saturating_mul(c1.saturating_sub(c0));

    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "action": "format_cells",
            "tab": tab,
            "range": a1_range,
            "cells": cells,
            "applies": applied,
        })
        .into());
    }
    // The fields mask names only the flags that were passed, so properties the caller left unset
    // are not reset to their defaults.
    let mut masked: Vec<&str> = applied.keys().map(String::as_str).collect();
    masked.sort_unstable();
    let fields =
        masked.iter().map(|k| format!("userEnteredFormat.textFormat.{k}")).collect::<Vec<_>>().join(",");
    api.sheets_batch_update(
        &sid,
        &json!({
            "requests": [{
                "repeatCell": {
                    "range": {
                        "sheetId": sheet_id,
                        "startRowIndex": r0,
                        "endRowIndex": r1,
                        "startColumnIndex": c0,
                        "endColumnIndex": c1,
                    },
                    "cell": {"userEnteredFormat": {"textFormat": applied}},
                    "fields": fields,
                }
            }]
        }),
    )
    .await?;
    Ok(json!({
        "tab": tab,
        "formatted_range": a1_range,
        "cells": cells,
        "applied": applied,
    })
    .into())
}

async fn create_spreadsheet(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let title = args.req_str("title")?;
    let tabs = args.opt_str_list("tabs")?;
    let mut body = json!({"properties": {"title": title}});
    if let (Some(obj), Some(tabs)) = (body.as_object_mut(), tabs.filter(|t| !t.is_empty())) {
        let sheets: Vec<Value> = tabs.iter().map(|t| json!({"properties": {"title": t}})).collect();
        obj.insert("sheets".into(), Value::Array(sheets));
    }
    let ss = api.sheets_create(&body, "spreadsheetId,spreadsheetUrl").await?;
    Ok(json!({"id": field(&ss, "spreadsheetId"), "url": field(&ss, "spreadsheetUrl")}).into())
}

async fn add_tab(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let sid = parse_ref(&args.req_str("item")?)?.id;
    let title = args.req_str("title")?;
    let index = args.opt_i64("index")?;
    let mut props = json!({"title": title});
    if let (Some(obj), Some(index)) = (props.as_object_mut(), index) {
        obj.insert("index".into(), json!(index));
    }
    let resp =
        api.sheets_batch_update(&sid, &json!({"requests": [{"addSheet": {"properties": props}}]})).await?;
    let p = resp.pointer("/replies/0/addSheet/properties").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "sheet_id": field(&p, "sheetId"),
        "title": field(&p, "title"),
        "index": field(&p, "index"),
    })
    .into())
}

async fn clear_range(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let sid = parse_ref(&args.req_str("item")?)?.id;
    let a1_range = args.req_str("a1_range")?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let existing = grid(&api.sheets_values_get(&sid, &a1_range, "UNFORMATTED_VALUE").await?);
    let nonempty = count_nonempty(&existing);
    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "action": "clear_range",
            "range": a1_range,
            "before": existing,
            "after": [],
            "nonempty_cells_cleared": nonempty,
        })
        .into());
    }
    if !confirm {
        return Ok(preview_response(
            "clear_range",
            json!({"range": a1_range, "nonempty_cells_cleared": nonempty}),
        )
        .into());
    }
    api.sheets_values_clear(&sid, &a1_range).await?;
    Ok(json!({"cleared_range": a1_range, "cleared_nonempty_cells": nonempty}).into())
}

async fn delete_rows(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let sid = parse_ref(&args.req_str("item")?)?.id;
    let tab = args.req_str("tab")?;
    let start_row = args
        .opt_i64("start_row")?
        .ok_or_else(|| ToolError::msg("delete_rows: missing required argument 'start_row'"))?;
    let count = args.i64_or("count", 1)?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let titles = sheet_titles(api, &sid).await?;
    let Some(sheet_id) = titles.iter().find(|(t, _)| *t == tab).map(|(_, id)| *id) else {
        return Err(tab_not_found(&tab, &titles));
    };
    // start_row/count come straight from tool input, so every step saturates rather than
    // overflowing on an absurd value.
    let last_row = start_row.saturating_add(count).saturating_sub(1);
    let doomed = grid(
        &api.sheets_values_get(
            &sid,
            &format!("{}!{start_row}:{last_row}", quote_tab(&tab)),
            "UNFORMATTED_VALUE",
        )
        .await?,
    );
    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "action": "delete_rows",
            "tab": tab,
            "rows": format!("{start_row}..{last_row}"),
            "would_delete": doomed,
            "deletes_rows": count,
        })
        .into());
    }
    if !confirm {
        let sample: Vec<&Vec<Value>> = doomed.iter().take(5).collect();
        return Ok(preview_response(
            "delete_rows",
            json!({"tab": tab, "start_row": start_row, "count": count, "sample": sample}),
        )
        .into());
    }
    let start_index = start_row.saturating_sub(1);
    api.sheets_batch_update(
        &sid,
        &json!({
            "requests": [{
                "deleteDimension": {
                    "range": {
                        "sheetId": sheet_id,
                        "dimension": "ROWS",
                        "startIndex": start_index,
                        "endIndex": start_index.saturating_add(count),
                    }
                }
            }]
        }),
    )
    .await?;
    Ok(json!({"tab": tab, "deleted_rows": count}).into())
}

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "read_sheet",
            "Preview a spreadsheet (item = URL or ID): the first max_rows x max_cols (default 5x5) of each tab.\n\
             \n\
             A quick, context-safe peek. For the complete data use read_full_sheet (which saves it to a local\n\
             file). Pass an explicit a1_range to read exactly that range instead of the preview window.\n\
             values: UNFORMATTED | FORMATTED | FORMULA (FORMULA returns cell formulas).",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "tab": {"type": "string"},
                    "max_rows": {"type": "integer", "default": 5},
                    "max_cols": {"type": "integer", "default": 5},
                    "a1_range": {"type": "string"},
                    "header_row": {"type": "boolean", "default": true},
                    "values": {"type": "string", "default": "UNFORMATTED"},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "read_full_sheet",
            "Read an entire sheet tab, save it to a local CSV, and return the path + a 5x5 preview.\n\
             \n\
             Reads the full tab (item = URL or ID; default first tab) via the Sheets API, writes it to\n\
             dest_path (resolved inside the sandbox files dir; a sanitized default name otherwise), and\n\
             returns total_rows/total_cols plus the same first-5-rows/cols preview as read_sheet. Use this\n\
             instead of read_sheet when you need the complete data without flooding the context.",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "tab": {"type": "string"},
                    "dest_path": {"type": "string"},
                    "values": {"type": "string", "default": "UNFORMATTED"},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "write_sheet",
            "Write rows to a tab of a spreadsheet (item), starting at start_cell (default A1).\n\
             \n\
             value_input RAW (default) stores cell text literally; pass 'USER_ENTERED' to interpret typed\n\
             values/formulas (a leading '=' becomes a live formula). Overwriting existing non-empty cells\n\
             requires confirm=true; the target is inspected as formulas, so a cell holding a formula counts\n\
             as non-empty even when it currently evaluates to an empty string. dry_run=true returns a\n\
             predicted before/after (client-side) without writing; `before` shows a formula cell as its\n\
             formula text, and Google's recalculation of `after` is not simulated.",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "tab": {"type": "string"},
                    "rows": {"type": "array", "items": {"type": "array"}},
                    "start_cell": {"type": "string", "default": "A1"},
                    "value_input": {
                        "type": "string",
                        "enum": ["RAW", "USER_ENTERED"],
                        "default": "RAW",
                    },
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "tab", "rows"],
            }),
        ),
        ToolDef::new(
            "append_rows",
            "Append rows after the last row of a tab of a spreadsheet (item). Non-destructive.\n\
             \n\
             value_input RAW (default) stores cell text literally; 'USER_ENTERED' interprets formulas/typed\n\
             values. dry_run=true returns where the rows would land (predicted) without appending.",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "tab": {"type": "string"},
                    "rows": {"type": "array", "items": {"type": "array"}},
                    "value_input": {
                        "type": "string",
                        "enum": ["RAW", "USER_ENTERED"],
                        "default": "RAW",
                    },
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "tab", "rows"],
            }),
        ),
        ToolDef::new(
            "format_cells",
            "Set text formatting (bold/italic/underline) on a bounded A1 range of a spreadsheet (item).\n\
             \n\
             a1_range like 'Data!A1:C10' or a single cell 'Data!B2' (no tab prefix targets the first\n\
             tab; open-ended ranges like 'A:C' are rejected). Each flag is tri-state: true applies it,\n\
             false removes it, unset leaves that property as-is. Formatting only — cell values are\n\
             untouched. dry_run=true returns the resolved target without writing. (Headings, bullets,\n\
             and rich text are Docs concepts: see append_text/create_document with markdown=true.)",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "a1_range": {"type": "string"},
                    "bold": {"type": "boolean"},
                    "italic": {"type": "boolean"},
                    "underline": {"type": "boolean"},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "a1_range"],
            }),
        ),
        ToolDef::new(
            "create_spreadsheet",
            "Create a new spreadsheet with optional named tabs.",
            json!({
                "properties": {
                    "title": {"type": "string"},
                    "tabs": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["title"],
            }),
        ),
        ToolDef::new(
            "add_tab",
            "Add a new tab (sheet) to an existing spreadsheet (item).",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "title": {"type": "string"},
                    "index": {"type": "integer"},
                },
                "required": ["item", "title"],
            }),
        ),
        ToolDef::new(
            "clear_range",
            "Clear all values in an A1 range of a spreadsheet (item), e.g. 'Data!A2:C10'. Requires confirm=true.\n\
             \n\
             dry_run=true returns the values that would be cleared (before) without clearing.",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "a1_range": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "a1_range"],
            }),
        ),
        ToolDef::new(
            "delete_rows",
            "Delete `count` rows starting at start_row (1-based) from a tab of a spreadsheet (item). Requires confirm=true.\n\
             \n\
             dry_run=true returns the rows that would be deleted without deleting.",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "tab": {"type": "string"},
                    "start_row": {"type": "integer"},
                    "count": {"type": "integer", "default": 1},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "tab", "start_row"],
            }),
        ),
    ]
}

pub async fn dispatch(name: &str, api: &dyn GoogleApi, args: &Args) -> Option<Result<ToolOutput>> {
    Some(match name {
        "read_sheet" => read_sheet(api, args).await,
        "read_full_sheet" => read_full_sheet(api, args).await,
        "write_sheet" => write_sheet(api, args).await,
        "append_rows" => append_rows(api, args).await,
        "format_cells" => format_cells(api, args).await,
        "create_spreadsheet" => create_spreadsheet(api, args).await,
        "add_tab" => add_tab(api, args).await,
        "clear_range" => clear_range(api, args).await,
        "delete_rows" => delete_rows(api, args).await,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::FakeApi;
    use std::path::PathBuf;

    /// Bare IDs must be >= 20 chars to parse.
    const SID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn def(name: &str) -> ToolDef {
        defs().into_iter().find(|d| d.name == name).expect("tool is declared here")
    }

    async fn run(api: &FakeApi, name: &str, arguments: Value) -> Result<Value> {
        let d = def(name);
        let Value::Object(map) = arguments else { panic!("arguments must be an object") };
        let args = Args::new(name, Some(map), &d.params()).unwrap();
        match dispatch(name, api, &args).await.expect("sheets owns this tool")? {
            ToolOutput::Json(json) => Ok(json),
            other => panic!("expected JSON, got {other:?}"),
        }
    }

    /// The pytest `_sheets_svc` fixture. A MagicMock replayed one canned answer for every call;
    /// this fake queues per call, so stock enough copies for the repeat invocations the
    /// confirm-gate tests make.
    fn fake_sheets(existing_values: Value, titles: &[(&str, i64)]) -> FakeApi {
        let api = FakeApi::new();
        let meta = json!({
            "sheets": titles
                .iter()
                .map(|(t, id)| json!({"properties": {"sheetId": id, "title": t}}))
                .collect::<Vec<Value>>()
        });
        for _ in 0..4 {
            api.on("sheets_get", meta.clone());
            api.on("sheets_values_get", json!({"range": "Data!A1", "values": existing_values}));
            api.on("sheets_values_update", json!({"updatedRange": "Data!A1:B2", "updatedCells": 4}));
            api.on("sheets_values_append", json!({"updates": {"updatedRange": "Data!A5", "updatedRows": 1}}));
        }
        api
    }

    use crate::ENV_LOCK as FILES_DIR_LOCK;

    struct Sandbox {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        root: PathBuf,
        previous: Option<String>,
    }

    impl Sandbox {
        fn open() -> Sandbox {
            let guard = FILES_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().unwrap();
            // Canonicalised because localfs resolves symlinks, and macOS's /tmp is one.
            let root = dir.path().canonicalize().unwrap().join("files");
            let previous = std::env::var("GDRIVE_MCP_FILES_DIR").ok();
            // A path that does not exist yet, so files_root() creates and marks it as ours.
            unsafe { std::env::set_var("GDRIVE_MCP_FILES_DIR", &root) };
            Sandbox { _guard: guard, _dir: dir, root, previous }
        }

        /// Every file in the temp dir that holds the sandbox but *outside* the sandbox root —
        /// i.e. exactly where an escaping `dest_path` would deposit a spill.
        fn escaped_files(&self) -> Vec<PathBuf> {
            fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
                for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, found);
                    } else {
                        found.push(path);
                    }
                }
            }
            let mut found = Vec::new();
            walk(self.root.parent().expect("the sandbox root sits inside the temp dir"), &mut found);
            found.retain(|p| !p.starts_with(&self.root));
            found
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var("GDRIVE_MCP_FILES_DIR", v) },
                None => unsafe { std::env::remove_var("GDRIVE_MCP_FILES_DIR") },
            }
        }
    }

    // ---- confirm-before-destructive gate -------------------------------------------------

    #[tokio::test]
    async fn write_sheet_without_confirm_previews_the_overwrite_and_writes_nothing() {
        let api = fake_sheets(json!([["a", "b"]]), &[]); // target non-empty
        let out = run(&api, "write_sheet", json!({"item": SID, "tab": "Data", "rows": [["x", "y"]]}))
            .await
            .unwrap();
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"]["overwrites_nonempty_cells"], 2);
        assert_eq!(api.call_count("sheets_values_update"), 0);
    }

    #[tokio::test]
    async fn the_gate_inspects_the_target_as_formulas_so_one_rendering_empty_still_trips_it() {
        // Under UNFORMATTED_VALUE a formula evaluating to "" comes back as "", `count_nonempty`
        // reads the target as empty, the gate does not fire, and a confirm-less write destroys the
        // formula. Reading FORMULA is what closes that: formula *text* is never empty.
        let api = fake_sheets(json!([["=IF(A9=1,\"\",\"x\")"]]), &[]);
        let out =
            run(&api, "write_sheet", json!({"item": SID, "tab": "Data", "rows": [["new"]]})).await.unwrap();
        assert_eq!(api.last("sheets_values_get")["render"], "FORMULA");
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"]["overwrites_nonempty_cells"], 1);
        assert_eq!(api.call_count("sheets_values_update"), 0);
    }

    #[tokio::test]
    async fn a_write_dry_run_shows_a_formula_cell_as_its_formula() {
        // The same read feeds `before`, so the caller deciding whether to overwrite sees the
        // formula it would destroy rather than the value that formula happened to produce.
        let api = fake_sheets(json!([["=SUM(B:B)"]]), &[]);
        let out =
            run(&api, "write_sheet", json!({"item": SID, "tab": "Data", "rows": [["x"]], "dry_run": true}))
                .await
                .unwrap();
        assert_eq!(out["before"], json!([["=SUM(B:B)"]]));
        assert_eq!(out["overwrites_nonempty_cells"], 1);
    }

    #[tokio::test]
    async fn write_sheet_with_confirm_updates_exactly_the_block_the_rows_span() {
        let api = fake_sheets(json!([["a", "b"]]), &[]);
        let out = run(
            &api,
            "write_sheet",
            json!({"item": SID, "tab": "Data", "rows": [["x", "y"]], "confirm": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["updated_cells"], 4);
        let call = api.last("sheets_values_update");
        assert_eq!(call["range"], "'Data'!A1:B1"); // 1 row x 2 cols
        assert_eq!(call["value_input"], "RAW"); // default is RAW (no formula injection)
        assert_eq!(call["values"], json!([["x", "y"]]));
    }

    #[tokio::test]
    async fn interpreting_a_leading_equals_as_a_formula_is_opt_in() {
        let api = fake_sheets(json!([]), &[]);
        run(
            &api,
            "write_sheet",
            json!({"item": SID, "tab": "Data", "rows": [["=A1*2"]], "value_input": "USER_ENTERED"}),
        )
        .await
        .unwrap();
        assert_eq!(api.last("sheets_values_update")["value_input"], "USER_ENTERED");
    }

    #[tokio::test]
    async fn an_empty_target_range_is_written_without_confirmation() {
        let api = fake_sheets(json!([]), &[]); // empty target -> not destructive
        let out =
            run(&api, "write_sheet", json!({"item": SID, "tab": "Data", "rows": [["x"]]})).await.unwrap();
        assert_eq!(out["updated_cells"], 4);
        assert_eq!(api.call_count("sheets_values_update"), 1);
    }

    #[tokio::test]
    async fn clear_range_clears_only_once_confirmed() {
        let api = fake_sheets(json!([["x", "y"]]), &[]);
        let out = run(&api, "clear_range", json!({"item": SID, "a1_range": "Data!A1:B2"})).await.unwrap();
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"]["nonempty_cells_cleared"], 2);
        assert_eq!(api.call_count("sheets_values_clear"), 0);

        run(&api, "clear_range", json!({"item": SID, "a1_range": "Data!A1:B2", "confirm": true}))
            .await
            .unwrap();
        assert_eq!(api.call_count("sheets_values_clear"), 1);
    }

    #[tokio::test]
    async fn delete_rows_deletes_a_zero_based_half_open_dimension_range_once_confirmed() {
        let api = fake_sheets(json!([["r"]]), &[("Data", 123)]);
        let out = run(&api, "delete_rows", json!({"item": SID, "tab": "Data", "start_row": 3, "count": 2}))
            .await
            .unwrap();
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(api.call_count("sheets_batch_update"), 0);

        run(
            &api,
            "delete_rows",
            json!({"item": SID, "tab": "Data", "start_row": 3, "count": 2, "confirm": true}),
        )
        .await
        .unwrap();
        assert_eq!(
            api.last("sheets_batch_update")["body"]["requests"][0]["deleteDimension"]["range"],
            json!({"sheetId": 123, "dimension": "ROWS", "startIndex": 2, "endIndex": 4})
        );
    }

    #[tokio::test]
    async fn deleting_from_an_unknown_tab_is_refused_before_the_confirm_gate() {
        // The tab lookup runs BEFORE the gate, so a regression that resolved an unknown tab to some
        // other sheetId would delete rows from the wrong tab — and `confirm=true` would not save
        // you, because the caller confirmed a tab that was never targeted. Asserted at the
        // confirmed call for exactly that reason.
        let api = fake_sheets(json!([["r"]]), &[("Data", 123)]);
        let err = run(
            &api,
            "delete_rows",
            json!({"item": SID, "tab": "Bogus", "start_row": 1, "count": 1, "confirm": true}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "tab 'Bogus' not found; available: ['Data']");
        assert_eq!(api.call_count("sheets_batch_update"), 0);
        // ...and nothing was even read, so the refusal cannot be mistaken for an empty result.
        assert_eq!(api.call_count("sheets_values_get"), 0);
    }

    #[tokio::test]
    async fn a_spill_forwards_its_render_option_to_the_api() {
        // Nothing else pins this: a spill that silently took UNFORMATTED regardless of `values`
        // would hand back computed numbers where the caller asked for the formulas behind them.
        let _sandbox = Sandbox::open();
        for (requested, expected) in
            [("FORMULA", "FORMULA"), ("FORMATTED", "FORMATTED_VALUE"), ("nonsense", "UNFORMATTED_VALUE")]
        {
            let api = fake_sheets(json!([["=A1*2"]]), &[("Data", 1)]);
            run(&api, "read_full_sheet", json!({"item": SID, "values": requested, "dest_path": "r.csv"}))
                .await
                .unwrap();
            assert_eq!(api.last("sheets_values_get")["render"], expected, "values={requested}");
        }
    }

    #[tokio::test]
    async fn append_rows_is_never_gated_and_defaults_to_raw_input() {
        let api = fake_sheets(json!([]), &[]);
        let out =
            run(&api, "append_rows", json!({"item": SID, "tab": "Data", "rows": [["z"]]})).await.unwrap();
        assert_eq!(out["appended_rows"], 1);
        let call = api.last("sheets_values_append");
        assert_eq!(call["value_input"], "RAW");
        assert_eq!(call["range"], "'Data'"); // the whole tab; the API picks the first free row
    }

    // ---- read-side structuring -----------------------------------------------------------

    #[tokio::test]
    async fn read_full_sheet_spills_a_csv_and_previews_only_the_first_five_rows() {
        let _sandbox = Sandbox::open();
        let mut full = vec![json!(["h1", "h2"])];
        for i in 1..=8 {
            full.push(json!([i.to_string(), (i * 2).to_string()]));
        }
        let api = fake_sheets(Value::Array(full), &[("Data", 1)]); // 9 rows x 2 cols
        let out = run(&api, "read_full_sheet", json!({"item": SID, "tab": "Data", "dest_path": "out.csv"}))
            .await
            .unwrap();
        assert_eq!(out["total_rows"], 9);
        assert_eq!(out["total_cols"], 2);
        assert_eq!(out["preview"]["records"][0], json!({"h1": "1", "h2": "2"}));
        // 5-row window incl. header -> at most 4 data rows
        assert!(out["preview"]["records"].as_array().unwrap().len() <= 4);
        let body = std::fs::read_to_string(out["path"].as_str().unwrap()).unwrap();
        assert!(body.starts_with("h1,h2"), "{body}");
    }

    #[tokio::test]
    async fn a_short_row_is_padded_with_nulls_out_to_the_header_width() {
        let api = fake_sheets(json!([["h1", "h2"], ["1", "2"], ["3"]]), &[]);
        let out = run(&api, "read_sheet", json!({"item": SID, "tab": "Data"})).await.unwrap();
        let tab = &out["tabs"][0];
        assert_eq!(tab["headers"], json!(["h1", "h2"]));
        assert_eq!(tab["records"][0], json!({"h1": "1", "h2": "2"}));
        assert_eq!(tab["records"][1], json!({"h1": "3", "h2": null}));
    }

    #[tokio::test]
    async fn a_row_wider_than_the_header_loses_its_extra_cells_from_the_record_only() {
        let api = fake_sheets(json!([["h1"], ["1", "spill"]]), &[]);
        let out = run(&api, "read_sheet", json!({"item": SID, "tab": "Data"})).await.unwrap();
        assert_eq!(out["tabs"][0]["records"][0], json!({"h1": "1"}));
        assert_eq!(out["tabs"][0]["rows"], json!([["1", "spill"]]));
    }

    #[tokio::test]
    async fn header_row_false_returns_the_raw_grid_with_no_records() {
        let api = fake_sheets(json!([["h1", "h2"], ["1", "2"]]), &[]);
        let out =
            run(&api, "read_sheet", json!({"item": SID, "tab": "Data", "header_row": false})).await.unwrap();
        let tab = &out["tabs"][0];
        assert_eq!(tab["rows"], json!([["h1", "h2"], ["1", "2"]]));
        assert!(tab.get("records").is_none());
    }

    #[tokio::test]
    async fn a_tabless_read_previews_every_tab_in_the_order_the_api_reports_them() {
        let api = fake_sheets(json!([]), &[("Second", 2), ("First", 1)]);
        let out = run(&api, "read_sheet", json!({"item": SID})).await.unwrap();
        let ranges: Vec<Value> =
            api.calls_to("sheets_values_get").iter().map(|c| c["range"].clone()).collect();
        assert_eq!(ranges, vec![json!("'Second'!A1:E5"), json!("'First'!A1:E5")]);
        assert_eq!(out["tabs"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_default_tab_is_the_first_one_the_api_lists_not_the_alphabetical_first() {
        let _sandbox = Sandbox::open();
        let api = fake_sheets(json!([["x"]]), &[("Second", 2), ("First", 1)]);
        let out = run(&api, "read_full_sheet", json!({"item": SID})).await.unwrap();
        assert_eq!(out["tab"], "Second");
        assert_eq!(api.last("sheets_values_get")["range"], "'Second'");
    }

    #[tokio::test]
    async fn reading_a_spreadsheet_with_no_tabs_at_all_says_so() {
        let api = fake_sheets(json!([]), &[]);
        let err = run(&api, "read_full_sheet", json!({"item": SID})).await.unwrap_err();
        assert_eq!(err.to_string(), "spreadsheet has no tabs");
    }

    #[tokio::test]
    async fn the_spilled_csv_renders_cells_the_way_pythons_csv_writer_did() {
        let _sandbox = Sandbox::open();
        let api = fake_sheets(json!([[null, true, 3, "a,b"], ["say \"hi\""]]), &[("Data", 1)]);
        let out = run(&api, "read_full_sheet", json!({"item": SID, "dest_path": "out.csv"})).await.unwrap();
        let body = std::fs::read_to_string(out["path"].as_str().unwrap()).unwrap();
        assert_eq!(body, ",True,3,\"a,b\"\r\n\"say \"\"hi\"\"\"\r\n");
        // The two rows differ in width, which Sheets emits routinely.
        assert_eq!(out["total_cols"], 4);
    }

    #[tokio::test]
    async fn a_blank_sheet_row_spills_as_a_blank_line_not_an_empty_field() {
        let _sandbox = Sandbox::open();
        let api = fake_sheets(json!([["a", "b"], [], ["c", "d"]]), &[("Data", 1)]);
        let out = run(&api, "read_full_sheet", json!({"item": SID, "dest_path": "gap.csv"})).await.unwrap();
        let body = std::fs::read_to_string(out["path"].as_str().unwrap()).unwrap();
        assert_eq!(body, "a,b\r\n\r\nc,d\r\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_spilled_csv_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let _sandbox = Sandbox::open();
        let api = fake_sheets(json!([["mrn", "dx"], ["123", "flu"]]), &[("Data", 1)]);
        let out = run(&api, "read_full_sheet", json!({"item": SID, "dest_path": "phi.csv"})).await.unwrap();
        let mode = std::fs::metadata(out["path"].as_str().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600); // the spill can hold sensitive data, so no other local account may read it
    }

    #[tokio::test]
    async fn a_spill_dest_path_escaping_the_sandbox_is_rejected() {
        let sandbox = Sandbox::open();
        // The absolute escape aims at the temp dir holding the sandbox, so a containment
        // regression leaves an observable CSV next to the sandbox rather than in /etc.
        let absolute = sandbox.root.parent().unwrap().join("loot.csv");
        for escape in [absolute.to_str().unwrap(), "../loot.csv", "sub/../../loot.csv"] {
            let api = fake_sheets(json!([["mrn", "dx"]]), &[("Data", 1)]);
            let err =
                run(&api, "read_full_sheet", json!({"item": SID, "dest_path": escape})).await.unwrap_err();
            assert!(err.to_string().contains("must stay within the files dir"), "{escape}: {err}");
        }
        assert_eq!(sandbox.escaped_files(), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn the_preview_window_is_max_rows_by_max_cols_anchored_at_a1() {
        let api = fake_sheets(json!([["h1", "h2"]]), &[]);
        run(&api, "read_sheet", json!({"item": SID, "tab": "Data"})).await.unwrap();
        assert_eq!(api.last("sheets_values_get")["range"], "'Data'!A1:E5"); // 5x5 by default
        run(&api, "read_sheet", json!({"item": SID, "tab": "Data", "max_rows": 3, "max_cols": 2}))
            .await
            .unwrap();
        assert_eq!(api.last("sheets_values_get")["range"], "'Data'!A1:B3");
    }

    #[tokio::test]
    async fn an_explicit_a1_range_is_read_verbatim_and_outranks_the_tab() {
        let api = fake_sheets(json!([["x"]]), &[("Data", 1)]);
        let out = run(
            &api,
            "read_sheet",
            json!({"item": SID, "tab": "Other", "a1_range": "Data!B2:C9", "max_rows": 1, "max_cols": 1}),
        )
        .await
        .unwrap();
        // Passed through as spelled — neither re-quoted nor cropped to the window — and the tab
        // list is never fetched, so a range naming a tab is readable on its own.
        assert_eq!(api.last("sheets_values_get")["range"], "Data!B2:C9");
        assert_eq!(api.call_count("sheets_get"), 0);
        assert_eq!(out["tabs"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn only_an_explicit_formula_render_returns_formulas() {
        let api = fake_sheets(json!([]), &[]);
        run(&api, "read_sheet", json!({"item": SID, "tab": "Data", "values": "formula"})).await.unwrap();
        assert_eq!(api.last("sheets_values_get")["render"], "FORMULA");
        run(&api, "read_sheet", json!({"item": SID, "tab": "Data", "values": "nonsense"})).await.unwrap();
        assert_eq!(api.last("sheets_values_get")["render"], "UNFORMATTED_VALUE");
    }

    // ---- dry runs --------------------------------------------------------------------------

    #[tokio::test]
    async fn a_write_dry_run_predicts_before_and_after_without_writing() {
        let api = fake_sheets(json!([["a", "b"]]), &[]);
        let out = run(
            &api,
            "write_sheet",
            json!({"item": SID, "tab": "Data", "rows": [["=A1*2", "y"]], "dry_run": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["before"], json!([["a", "b"]]));
        assert_eq!(out["after"], json!([["=A1*2", "y"]]));
        assert_eq!(out["formulas_not_recalculated"], true);
        assert_eq!(api.call_count("sheets_values_update"), 0);
    }

    #[tokio::test]
    async fn a_clear_dry_run_reports_what_would_go_without_clearing() {
        let api = fake_sheets(json!([["x", "y"]]), &[]);
        let out = run(&api, "clear_range", json!({"item": SID, "a1_range": "Data!A1:B1", "dry_run": true}))
            .await
            .unwrap();
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["before"], json!([["x", "y"]]));
        assert_eq!(out["after"], json!([]));
        assert_eq!(api.call_count("sheets_values_clear"), 0);
    }

    #[tokio::test]
    async fn a_delete_dry_run_names_the_inclusive_row_span_without_deleting() {
        let api = fake_sheets(json!([["r"]]), &[("Data", 1)]);
        let out = run(
            &api,
            "delete_rows",
            json!({"item": SID, "tab": "Data", "start_row": 2, "count": 3, "dry_run": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["deletes_rows"], 3);
        assert_eq!(out["rows"], "2..4");
        assert_eq!(api.last("sheets_values_get")["range"], "'Data'!2:4");
        assert_eq!(api.call_count("sheets_batch_update"), 0);
    }

    #[tokio::test]
    async fn an_append_dry_run_reports_the_landing_row_without_appending() {
        let api = fake_sheets(json!([["h"], ["1"], ["2"]]), &[]);
        let out =
            run(&api, "append_rows", json!({"item": SID, "tab": "Data", "rows": [["9"]], "dry_run": true}))
                .await
                .unwrap();
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["at_row"], 4);
        assert_eq!(out["appends_rows"], 1);
        assert_eq!(api.call_count("sheets_values_append"), 0);
    }

    #[tokio::test]
    async fn value_input_is_rejected_before_a_dry_run_can_report_success() {
        for tool in ["write_sheet", "append_rows"] {
            let api = fake_sheets(json!([]), &[]);
            let err = run(
                &api,
                tool,
                json!({
                    "item": SID, "tab": "Data", "rows": [["x"]],
                    "value_input": "MAGIC", "dry_run": true,
                }),
            )
            .await
            .unwrap_err();
            assert_eq!(err.to_string(), "value_input must be RAW or USER_ENTERED, got 'MAGIC'");
            assert_eq!(api.call_count("sheets_values_get"), 0, "{tool} read before validating");
        }
    }

    #[tokio::test]
    async fn a_lower_case_value_input_is_refused_rather_than_upper_cased() {
        // The Literal annotation made pydantic reject this before the body ran, so accepting it
        // would enable formula interpretation through a spelling the Python server turned down.
        let api = fake_sheets(json!([]), &[]);
        let err = run(
            &api,
            "write_sheet",
            json!({"item": SID, "tab": "Data", "rows": [["=A1*2"]], "value_input": "user_entered"}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "value_input must be RAW or USER_ENTERED, got 'user_entered'");
        assert_eq!(api.call_count("sheets_values_update"), 0);
    }

    // ---- cell formatting ---------------------------------------------------------------------

    #[tokio::test]
    async fn format_cells_builds_one_repeat_cell_over_the_resolved_box() {
        let api = fake_sheets(json!([]), &[("Data", 123)]);
        let out = run(
            &api,
            "format_cells",
            json!({"item": SID, "a1_range": "Data!A2:B3", "bold": true, "underline": false}),
        )
        .await
        .unwrap();
        let rc = api.last("sheets_batch_update")["body"]["requests"][0]["repeatCell"].clone();
        assert_eq!(
            rc["range"],
            json!({
                "sheetId": 123, "startRowIndex": 1, "endRowIndex": 3,
                "startColumnIndex": 0, "endColumnIndex": 2,
            })
        );
        assert_eq!(
            rc["cell"],
            json!({"userEnteredFormat": {"textFormat": {"bold": true, "underline": false}}})
        );
        // the fields mask names only the passed flags, so unset properties are left untouched
        assert_eq!(rc["fields"], "userEnteredFormat.textFormat.bold,userEnteredFormat.textFormat.underline");
        assert_eq!(out["cells"], 4);
        assert_eq!(out["applied"], json!({"bold": true, "underline": false}));
    }

    #[tokio::test]
    async fn a_range_with_no_tab_prefix_targets_the_first_tab() {
        let api = fake_sheets(json!([]), &[("Data", 123)]);
        let out =
            run(&api, "format_cells", json!({"item": SID, "a1_range": "B2", "italic": true})).await.unwrap();
        assert_eq!(out["tab"], "Data");
        assert_eq!(out["cells"], 1);
    }

    #[tokio::test]
    async fn format_cells_needs_a_flag_and_a_tab_that_exists() {
        let api = fake_sheets(json!([]), &[("Data", 123)]);
        let err = run(&api, "format_cells", json!({"item": SID, "a1_range": "A1:B2"})).await.unwrap_err();
        assert_eq!(err.to_string(), "nothing to format: pass at least one of bold/italic/underline");

        let err = run(&api, "format_cells", json!({"item": SID, "a1_range": "Bogus!A1:B2", "bold": true}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "tab 'Bogus' not found; available: ['Data']");
        assert_eq!(api.call_count("sheets_batch_update"), 0);
    }

    #[tokio::test]
    async fn a_format_dry_run_resolves_the_target_without_writing() {
        let api = fake_sheets(json!([]), &[("Data", 123)]);
        let out = run(
            &api,
            "format_cells",
            json!({"item": SID, "a1_range": "Data!A1:C2", "bold": true, "dry_run": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["cells"], 6);
        assert_eq!(out["applies"], json!({"bold": true}));
        assert_eq!(api.call_count("sheets_batch_update"), 0);
    }

    // ---- creation ------------------------------------------------------------------------------

    #[tokio::test]
    async fn create_spreadsheet_names_its_tabs_only_when_some_were_asked_for() {
        let api = FakeApi::new();
        api.on("sheets_create", json!({"spreadsheetId": "NEW", "spreadsheetUrl": "https://example/NEW"}));
        let out = run(&api, "create_spreadsheet", json!({"title": "T", "tabs": ["a", "b"]})).await.unwrap();
        assert_eq!(out["id"], "NEW");
        assert_eq!(out["url"], "https://example/NEW");
        assert_eq!(
            api.last("sheets_create")["body"],
            json!({
                "properties": {"title": "T"},
                "sheets": [{"properties": {"title": "a"}}, {"properties": {"title": "b"}}],
            })
        );

        let bare = FakeApi::new();
        run(&bare, "create_spreadsheet", json!({"title": "T"})).await.unwrap();
        assert_eq!(bare.last("sheets_create")["body"], json!({"properties": {"title": "T"}}));
    }

    #[tokio::test]
    async fn add_tab_sends_an_index_only_when_one_was_given() {
        let api = FakeApi::new();
        api.on(
            "sheets_batch_update",
            json!({
                "replies": [{"addSheet": {"properties": {"sheetId": 7, "title": "New", "index": 2}}}]
            }),
        );
        let out = run(&api, "add_tab", json!({"item": SID, "title": "New", "index": 2})).await.unwrap();
        assert_eq!(out["sheet_id"], 7);
        assert_eq!(out["title"], "New");
        assert_eq!(out["index"], 2);
        assert_eq!(
            api.last("sheets_batch_update")["body"]["requests"][0]["addSheet"]["properties"],
            json!({"title": "New", "index": 2})
        );

        let bare = FakeApi::new();
        run(&bare, "add_tab", json!({"item": SID, "title": "New"})).await.unwrap();
        assert_eq!(
            bare.last("sheets_batch_update")["body"]["requests"][0]["addSheet"]["properties"],
            json!({"title": "New"})
        );
    }

    // ---- shared helpers --------------------------------------------------------------------------

    #[tokio::test]
    async fn zero_and_false_are_data_but_the_empty_string_and_null_are_not() {
        let api = fake_sheets(json!([[0, false, "", null, "x"]]), &[]);
        let out = run(&api, "clear_range", json!({"item": SID, "a1_range": "Data!A1:E1"})).await.unwrap();
        assert_eq!(out["impact"]["nonempty_cells_cleared"], 3);
    }

    #[test]
    fn the_tools_are_declared_in_the_order_the_python_module_registered_them() {
        let names: Vec<&str> = defs().iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "read_sheet",
                "read_full_sheet",
                "write_sheet",
                "append_rows",
                "format_cells",
                "create_spreadsheet",
                "add_tab",
                "clear_range",
                "delete_rows",
            ]
        );
    }

    #[tokio::test]
    async fn a_tool_this_module_does_not_own_is_left_for_the_next_one() {
        let api = FakeApi::new();
        let args = Args::new("read_document", None, &[]).unwrap();
        assert!(dispatch("read_document", &api, &args).await.is_none());
    }
}
