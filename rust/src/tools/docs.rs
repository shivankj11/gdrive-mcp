//! Google Docs tools: read text/markdown, extract images, create, append/insert (opt-in
//! markdown formatting), comments.
//!
//! Every tool references its target Doc/file via `item` (a URL or ID).

use std::collections::HashSet;
use std::sync::LazyLock;

use serde_json::{json, Map, Value};

use super::{merge, Image, ToolDef, ToolOutput};
use crate::args::Args;
use crate::chunking::{paginate, DEFAULT_MAX_CHARS};
use crate::clients::GoogleApi;
use crate::error::{Result, ToolError};
use crate::guard::preview_response;
use crate::ids::{parse_ref, parse_tab};
use crate::locate;
use crate::md::{
    self, has_table, parse_cell, parse_markdown, render_markdown_preview, render_table_markdown,
    split_blocks, style_requests, u16len, ParsedMarkdown, Segment, Span,
};

/// HEADING_1..6 -> (level, the markdown prefix that renders it).
fn heading_hashes(named_style: &str) -> Option<(i64, String)> {
    let level: i64 = named_style.strip_prefix("HEADING_")?.parse().ok()?;
    (1..=6).contains(&level).then(|| (level, "#".repeat(level as usize)))
}

/// Only fetch image contentUris from Google-owned hosts — the fetch carries the user's Drive
/// OAuth token, so it must never reach a host a document could point at.
const ALLOWED_IMAGE_HOST_SUFFIXES: [&str; 4] =
    [".googleusercontent.com", ".google.com", ".googleapis.com", ".gstatic.com"];

fn is_google_host(url: &str) -> bool {
    let host =
        url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_lowercase)).unwrap_or_default();
    ALLOWED_IMAGE_HOST_SUFFIXES.iter().any(|s| host == s.trim_start_matches('.') || host.ends_with(s))
}

static HEX_RE: LazyLock<Option<regex::Regex>> =
    LazyLock::new(|| regex::Regex::new(r"^#?([0-9a-fA-F]{6})$").ok());

/// `'#3366CC'` (or `'3366CC'`) -> Docs rgbColor with 0..1 float channels.
fn hex_to_rgb(color: &str) -> Result<Value> {
    let caps = HEX_RE.as_ref().and_then(|re| re.captures(color.trim()));
    let Some(h) = caps.as_ref().and_then(|c| c.get(1)).map(|m| m.as_str()) else {
        return Err(ToolError::msg(format!(
            "color must be a 6-digit hex string like '#3366CC', got '{color}'"
        )));
    };
    // The pattern already guarantees six hex digits, so the parse cannot fail.
    let channel = |at: usize| i64::from_str_radix(&h[at..at + 2], 16).unwrap_or(0) as f64 / 255.0;
    Ok(json!({"red": channel(0), "green": channel(2), "blue": channel(4)}))
}

/// Python's `round()`: half-to-even, unlike Rust's `f64::round` (half away from zero).
fn py_round(x: f64) -> i64 {
    let floor = x.floor();
    let frac = x - floor;
    let n = if frac > 0.5 || (frac == 0.5 && (floor as i64) % 2 != 0) { floor + 1.0 } else { floor };
    n as i64
}

/// One channel as `"%02x"` renders it (a negative channel keeps Python's `-ff` spelling).
fn hex_channel(c: f64) -> String {
    let n = py_round(c * 255.0);
    if n < 0 {
        format!("-{:x}", -n)
    } else {
        format!("{n:02x}")
    }
}

fn rgb_to_hex(rgb: &Value) -> String {
    let channel = |key: &str| rgb.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    format!(
        "#{}{}{}",
        hex_channel(channel("red")),
        hex_channel(channel("green")),
        hex_channel(channel("blue"))
    )
}

fn array(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// An element's index field. The API omits proto3 defaults, so an absent index reads as 0.
fn index_of(el: &Value, key: &str) -> i64 {
    el.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Text runs carrying an explicit foreground color, as `{text, color}` (hex).
fn colored_runs(content: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for el in content {
        for pe in array(el.get("paragraph").and_then(|p| p.get("elements"))) {
            let Some(tr) = pe.get("textRun") else {
                continue;
            };
            let Some(fg) = tr.pointer("/textStyle/foregroundColor/color/rgbColor") else {
                continue;
            };
            let text = tr.get("content").and_then(Value::as_str).unwrap_or("");
            out.push(json!({"text": text.trim_end_matches('\n'), "color": rgb_to_hex(fg)}));
        }
    }
    out
}

/// An updateTextStyle request coloring `[start, start+length)` — applied after the insert.
fn color_request(start: i64, length: i64, tab_id: Option<&str>, rgb: &Value) -> Value {
    json!({
        "updateTextStyle": {
            "range": doc_range(start, start + length, tab_id),
            "textStyle": {"foregroundColor": {"color": {"rgbColor": rgb}}},
            "fields": "foregroundColor",
        }
    })
}

fn para_text(para: &Value) -> String {
    array(para.get("elements"))
        .iter()
        .filter_map(|el| el.pointer("/textRun/content").and_then(Value::as_str))
        .collect()
}

fn content_to_markdown(content: &[Value]) -> (String, Vec<Value>) {
    let mut lines: Vec<String> = Vec::new();
    let mut outline: Vec<Value> = Vec::new();
    for el in content {
        if let Some(para) = el.get("paragraph") {
            let owned = para_text(para);
            let text = owned.trim_end_matches('\n');
            let style = para.pointer("/paragraphStyle/namedStyleType").and_then(Value::as_str).unwrap_or("");
            match heading_hashes(style) {
                Some((level, hashes)) if !text.is_empty() => {
                    lines.push(format!("{hashes} {text}"));
                    // start/end are the API's own UTF-16 offsets, making each outline entry a
                    // usable write anchor (see locate.rs) rather than just a table of contents.
                    outline.push(json!({
                        "level": level,
                        "text": text,
                        "start": index_of(el, "startIndex"),
                        "end": index_of(el, "endIndex"),
                    }));
                }
                _ => lines.push(text.to_string()),
            }
            continue;
        }
        if let Some(table) = el.get("table") {
            for (ri, row) in array(table.get("tableRows")).iter().enumerate() {
                let mut cells: Vec<String> = Vec::new();
                for cell in array(row.get("tableCells")) {
                    let ctext: String = array(cell.get("content"))
                        .iter()
                        .filter_map(|c| c.get("paragraph"))
                        .map(para_text)
                        .collect();
                    // escape '|' so cell text stays one field and re-parses (see md::split_row)
                    cells.push(ctext.trim().replace('\n', " ").replace('|', "\\|"));
                }
                lines.push(format!("| {} |", cells.join(" | ")));
                if ri == 0 {
                    // GFM delimiter row (column-matched) so the table re-parses on write
                    lines.push(format!("| {} |", vec!["---"; cells.len()].join(" | ")));
                }
            }
        }
    }
    (lines.join("\n"), outline)
}

fn content_to_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|el| el.get("paragraph"))
        .map(|p| para_text(p).trim_end_matches('\n').to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// One tab's identity and content, borrowed from the fetched document.
#[derive(Clone, Copy)]
struct TabView<'a> {
    id: Option<&'a str>,
    title: Option<&'a str>,
    body: &'a [Value],
    inline_objects: Option<&'a Value>,
    positioned_objects: Option<&'a Value>,
}

/// Every tab, depth-first incl. children.
fn flatten_tabs(tabs: &[Value]) -> Vec<TabView<'_>> {
    let mut out = Vec::new();
    collect_tabs(tabs, &mut out);
    out
}

fn collect_tabs<'a>(tabs: &'a [Value], out: &mut Vec<TabView<'a>>) {
    for t in tabs {
        let props = t.get("tabProperties");
        let dtab = t.get("documentTab");
        out.push(TabView {
            id: props.and_then(|p| p.get("tabId")).and_then(Value::as_str),
            title: props.and_then(|p| p.get("title")).and_then(Value::as_str),
            body: array(dtab.and_then(|d| d.pointer("/body/content"))),
            inline_objects: dtab.and_then(|d| d.get("inlineObjects")),
            positioned_objects: dtab.and_then(|d| d.get("positionedObjects")),
        });
        collect_tabs(array(t.get("childTabs")), out);
    }
}

/// `repr()` of an optional string, so the "tab not found" list reads like Python's.
fn py_repr(s: Option<&str>) -> String {
    s.map_or_else(|| "None".to_string(), |s| format!("'{s}'"))
}

fn selected_tabs<'a>(all_tabs: &[TabView<'a>], tab_id: Option<&str>) -> Result<Vec<TabView<'a>>> {
    let Some(tab_id) = tab_id.filter(|t| !t.is_empty()) else {
        return Ok(all_tabs.to_vec());
    };
    let selected: Vec<TabView<'a>> = all_tabs.iter().filter(|t| t.id == Some(tab_id)).copied().collect();
    if selected.is_empty() {
        let available: Vec<String> =
            all_tabs.iter().map(|t| format!("({}, {})", py_repr(t.id), py_repr(t.title))).collect();
        return Err(ToolError::msg(format!(
            "tab '{tab_id}' not found; available: [{}]",
            available.join(", ")
        )));
    }
    Ok(selected)
}

/// Pick a write target: (tab_id, body_content) for a tabbed doc, or (None, top-level body).
///
/// Defaults to the first tab when the doc has tabs but none was requested. A batchUpdate on a
/// tabbed doc must carry the tab id in location.tabId, so writes always resolve one.
fn resolve_write_tab<'a>(doc: &'a Value, tab_id: Option<&str>) -> Result<(Option<String>, &'a [Value])> {
    let body = array(doc.pointer("/body/content"));
    let all_tabs = flatten_tabs(array(doc.get("tabs")));
    if all_tabs.is_empty() {
        return Ok((None, body));
    }
    match selected_tabs(&all_tabs, tab_id)?.first() {
        Some(t) => Ok((t.id.map(str::to_string), t.body)),
        // Unreachable: selected_tabs only yields an empty list when all_tabs is empty.
        None => Ok((None, body)),
    }
}

/// The tab to act on: the explicit `tab` argument, else the one in the `item` URL.
///
/// An empty `tab` reads as "not given", because Python's `tab or parse_tab(item)` treated it that
/// way. Without the filter, `tab=""` suppresses the URL's `tab=t.xxxx` and the call silently
/// retargets the document's *first* tab — which for `delete_text` means deleting from the wrong
/// tab rather than reporting no match.
fn tab_arg(tab: Option<String>, item: &str) -> Option<String> {
    tab.filter(|t| !t.is_empty()).or_else(|| parse_tab(item))
}

fn embedded_image_uri<'a>(obj: Option<&'a Value>, props_key: &str) -> Option<&'a str> {
    obj?.get(props_key)?.pointer("/embeddedObject/imageProperties/contentUri")?.as_str()
}

/// contentUris of embedded images in `content`, in document order.
///
/// Covers both inline images and positioned (floating/wrapped) images; a positioned object is
/// anchored to the start of its paragraph, so it is emitted before that paragraph's inline
/// images.
fn image_uris(
    content: &[Value],
    inline_objects: Option<&Value>,
    positioned_objects: Option<&Value>,
) -> Vec<String> {
    let mut uris: Vec<String> = Vec::new();
    for el in content {
        let para = el.get("paragraph");
        for oid in array(para.and_then(|p| p.get("positionedObjectIds"))) {
            let Some(oid) = oid.as_str() else { continue };
            let obj = positioned_objects.and_then(|p| p.get(oid));
            if let Some(uri) = embedded_image_uri(obj, "positionedObjectProperties") {
                uris.push(uri.to_string());
            }
        }
        for pe in array(para.and_then(|p| p.get("elements"))) {
            let Some(oid) = pe.pointer("/inlineObjectElement/inlineObjectId").and_then(Value::as_str) else {
                continue;
            };
            let obj = inline_objects.and_then(|i| i.get(oid));
            if let Some(uri) = embedded_image_uri(obj, "inlineObjectProperties") {
                uris.push(uri.to_string());
            }
        }
    }
    uris
}

fn md_summary(parsed: &ParsedMarkdown) -> Value {
    json!({
        "headings": parsed.headings.len(),
        "list_items": parsed.list_items,
        "styled_spans": parsed.spans.len(),
    })
}

fn set_key(target: &mut Value, key: &str, value: Value) {
    if let Some(obj) = target.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
}

/// A JSON object's fields, for splatting one result into another (Python's `{**impact}`).
fn fields(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

// ---- table writes (shared by insert_table and the markdown pipe-table path) --------------

const MAX_TABLE_COLS: usize = 20;
const MAX_TABLE_CELLS: usize = 10_000;

/// Validate a rectangular, sanely-sized grid; return (n_rows, n_cols) or raise.
fn validate_rows(rows: &[Vec<Value>]) -> Result<(usize, usize)> {
    if rows.is_empty() {
        return Err(ToolError::msg("rows must be a non-empty list of row lists"));
    }
    let widths: Vec<usize> = rows.iter().map(Vec::len).collect();
    let n_cols = widths[0];
    if n_cols < 1 {
        return Err(ToolError::msg("rows must have at least one column"));
    }
    if widths.iter().any(|w| *w != n_cols) {
        return Err(ToolError::msg(format!(
            "all rows must have the same number of columns; got row widths {widths:?}"
        )));
    }
    let n_rows = rows.len();
    if n_cols > MAX_TABLE_COLS {
        return Err(ToolError::msg(format!("too many columns ({n_cols}); max {MAX_TABLE_COLS}")));
    }
    if n_rows * n_cols > MAX_TABLE_CELLS {
        return Err(ToolError::msg(format!(
            "table too large ({n_rows}x{n_cols} = {} cells); max {MAX_TABLE_CELLS}",
            n_rows * n_cols
        )));
    }
    Ok((n_rows, n_cols))
}

fn loc(index: i64, tab_id: Option<&str>) -> Value {
    let mut out = Map::new();
    out.insert("index".into(), json!(index));
    if let Some(t) = tab_id.filter(|t| !t.is_empty()) {
        out.insert("tabId".into(), json!(t));
    }
    Value::Object(out)
}

fn doc_range(start: i64, end: i64, tab_id: Option<&str>) -> Value {
    let mut out = Map::new();
    out.insert("startIndex".into(), json!(start));
    out.insert("endIndex".into(), json!(end));
    if let Some(t) = tab_id.filter(|t| !t.is_empty()) {
        out.insert("tabId".into(), json!(t));
    }
    Value::Object(out)
}

/// startIndexes of every table element in a body — the pre-insert snapshot for table selection.
fn table_starts(body: &[Value]) -> HashSet<i64> {
    body.iter()
        .filter(|el| el.get("table").is_some())
        .filter_map(|el| el.get("startIndex").and_then(Value::as_i64))
        .collect()
}

/// The just-inserted table: the table element whose startIndex is not in the pre-insert snapshot.
///
/// Deterministic regardless of pre-existing tables below the insert point (which shift down and
/// would otherwise also satisfy `start >= insert_index`). insertTable adds a newline before the
/// table, so its start is `requested_index + 1` — never rely on `start == requested_index`.
fn find_new_table<'a>(body: &'a [Value], pre: &HashSet<i64>) -> Result<&'a Value> {
    body.iter()
        .filter(|el| el.get("table").is_some())
        .filter(|el| el.get("startIndex").and_then(Value::as_i64).is_none_or(|s| !pre.contains(&s)))
        .min_by_key(|el| index_of(el, "startIndex"))
        .ok_or_else(|| ToolError::msg("could not locate the inserted table after re-fetch"))
}

/// (row, col, first-paragraph startIndex) in row-major order for an empty table element.
fn table_cell_indexes(table_el: &Value) -> Vec<(usize, usize, i64)> {
    let mut out: Vec<(usize, usize, i64)> = Vec::new();
    let rows = array(table_el.pointer("/table/tableRows"));
    for (r, row) in rows.iter().enumerate() {
        for (c, cell) in array(row.get("tableCells")).iter().enumerate() {
            // A Docs cell always holds at least one paragraph; a cell without one is skipped
            // rather than filled at a guessed index (Python raised StopIteration here).
            let first_para = array(cell.get("content")).iter().find(|x| x.get("paragraph").is_some());
            if let Some(p) = first_para {
                out.push((r, c, index_of(p, "startIndex")));
            }
        }
    }
    out
}

/// `(row, col) -> (plain text, inline style spans)`. `Send + Sync` so the fill future — which
/// holds this across its awaits — stays `Send` for the server's handler.
type CellContent<'a> = &'a (dyn Fn(usize, usize) -> (String, Vec<Span>) + Send + Sync);

/// Fill requests for the empty cells, forward with a running offset so a single batch is correct.
///
/// Empty cells are skipped (empty insertText is rejected). Each insertText shifts later indexes,
/// so `at = empty_idx + offset` accounts for prior inserts; style ranges are computed post-shift
/// for their own cell.
fn fill_requests(
    cells: &[(usize, usize, i64)],
    tid: Option<&str>,
    content_fn: CellContent<'_>,
) -> (Vec<Value>, i64) {
    let mut reqs: Vec<Value> = Vec::new();
    let mut offset: i64 = 0;
    let mut filled: i64 = 0;
    for &(r, c, empty_idx) in cells {
        let (plain, spans) = content_fn(r, c);
        if plain.is_empty() {
            continue;
        }
        let at = empty_idx + offset;
        reqs.push(json!({"insertText": {"location": loc(at, tid), "text": plain}}));
        for (s, e, styles) in spans {
            let mut text_style = Map::new();
            for name in &styles {
                text_style.insert((*name).to_string(), Value::Bool(true));
            }
            reqs.push(json!({
                "updateTextStyle": {
                    "range": doc_range(at + s, at + e, tid),
                    "textStyle": Value::Object(text_style),
                    "fields": styles.join(","),
                }
            }));
        }
        offset += u16len(&plain);
        filled += 1;
    }
    (reqs, filled)
}

/// Re-fetch, locate the table not in `pre`, and fill it. Rolls back the empty table if the fill
/// batch fails (the two-phase write is non-atomic), then re-raises. Returns cells filled.
async fn fill_new_table(
    api: &dyn GoogleApi,
    did: &str,
    tid: Option<&str>,
    pre: &HashSet<i64>,
    content_fn: CellContent<'_>,
) -> Result<i64> {
    let doc = api.docs_get(did).await?;
    let (_tid, body) = resolve_write_tab(&doc, tid)?;
    let table_el = find_new_table(body, pre)?;
    let cells = table_cell_indexes(table_el);
    let (fill_reqs, filled) = fill_requests(&cells, tid, content_fn);
    if !fill_reqs.is_empty() {
        if let Err(err) = api.docs_batch_update(did, &json!({ "requests": fill_reqs })).await {
            // Only a Google API failure leaves an orphaned table behind; a locally-raised error
            // never got as far as writing one.
            if err.status().is_some() {
                let rng = doc_range(index_of(table_el, "startIndex"), index_of(table_el, "endIndex"), tid);
                // best-effort: remove the orphaned empty table before surfacing the error
                let rollback = json!({"requests": [{"deleteContentRange": {"range": rng}}]});
                let _ = api.docs_batch_update(did, &rollback).await;
            }
            return Err(err);
        }
    }
    Ok(filled)
}

/// Insert+fill one table (markdown path): snapshot, insert empty grid, fill with inline styling.
async fn insert_one_table(
    api: &dyn GoogleApi,
    did: &str,
    tid: Option<&str>,
    anchor_index: i64,
    rows: &[Vec<String>],
) -> Result<i64> {
    let doc = api.docs_get(did).await?;
    let (_tid, body) = resolve_write_tab(&doc, tid)?;
    let pre = table_starts(body);
    let (n_rows, n_cols) = (rows.len(), rows.first().map_or(0, Vec::len));
    let insert = json!({"requests": [
        {"insertTable": {"rows": n_rows, "columns": n_cols, "location": loc(anchor_index, tid)}}
    ]});
    api.docs_batch_update(did, &insert).await?;

    let content =
        |r: usize, c: usize| parse_cell(rows.get(r).and_then(|row| row.get(c)).map_or("", String::as_str));
    fill_new_table(api, did, tid, &pre, &content).await
}

/// Insert mixed text/table segments bottom-up at a fixed anchor.
///
/// Processing last->first means each newly inserted segment sits above the already-placed ones, so
/// we never re-reference an already-placed segment's indexes (no cross-segment offset math). Text
/// segments reuse the prose renderer with a TRAILING newline and insertion index == style base (do
/// not adopt append_text's prepend/base+1). Table segments delegate to insert_one_table (whose
/// insertTable auto-inserts its own leading newline, so tables need no lead_newline handling).
async fn insert_markdown_segments(
    api: &dyn GoogleApi,
    did: &str,
    tid: Option<&str>,
    anchor_index: i64,
    segments: &[Segment],
    lead_newline: bool,
) -> Result<()> {
    for i in (0..segments.len()).rev() {
        match &segments[i] {
            Segment::Table(rows) => {
                insert_one_table(api, did, tid, anchor_index, rows).await?;
            }
            Segment::Text(payload) => {
                let prefix = if i == 0 && lead_newline { "\n" } else { "" };
                let parsed = parse_markdown(payload);
                if parsed.text.trim().is_empty() && prefix.is_empty() {
                    continue; // skip an empty text run (empty insertText is rejected)
                }
                let base = anchor_index + u16len(prefix);
                let text = format!("{prefix}{}\n", parsed.text);
                let mut requests =
                    vec![json!({"insertText": {"location": loc(anchor_index, tid), "text": text}})];
                requests.extend(style_requests(&parsed, base, tid));
                api.docs_batch_update(did, &json!({ "requests": requests })).await?;
            }
        }
    }
    Ok(())
}

fn table_segment_count(segments: &[Segment]) -> usize {
    segments.iter().filter(|s| matches!(s, Segment::Table(_))).count()
}

// ---- locator-anchored edits (delete/replace) ---------------------------------------------

const PREVIEW_CHARS: usize = 200;

fn clip(s: &str) -> String {
    if s.chars().count() <= PREVIEW_CHARS {
        s.to_string()
    } else {
        s.chars().take(PREVIEW_CHARS).chain(['…']).collect()
    }
}

/// Python's `s[-n:]` — the last `n` characters (scalars, not bytes).
fn last_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    s.chars().skip(total.saturating_sub(n)).collect()
}

/// Pin the write to the revision the ranges were resolved against.
///
/// Resolving and writing are two round-trips; without this a concurrent edit between them would
/// silently shift every index and the write would land on the wrong text. With it, the batch is
/// rejected instead.
fn write_control(doc: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(rev) = doc.get("revisionId").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        out.insert("writeControl".into(), json!({"requiredRevisionId": rev}));
    }
    out
}

/// Resolve a write anchor from `after`/`before` (content locators) or a raw `index`.
///
/// `snap` pins the anchor to a paragraph boundary — insertTable rejects any other location, and
/// block markdown must not land mid-paragraph. Locator anchors are therefore boundary-valid by
/// construction, which is what raw `index` cannot guarantee.
fn anchor(
    body: &[Value],
    index: Option<i64>,
    after: Option<&str>,
    before: Option<&str>,
    snap: bool,
    required: bool,
) -> Result<i64> {
    let given: Vec<&str> =
        [("index", index.is_some()), ("after", after.is_some()), ("before", before.is_some())]
            .into_iter()
            .filter_map(|(name, present)| present.then_some(name))
            .collect();
    if given.len() > 1 {
        return Err(ToolError::msg(format!(
            "pass only one of index=, after=, before=; got {}",
            given.join(", ")
        )));
    }
    if given.is_empty() {
        if required {
            return Err(ToolError::msg(
                "pass one of after=, before= (locate by text) or index= (a raw Docs offset)",
            ));
        }
        return Ok(locate::body_end(body));
    }

    if let Some(index) = index {
        return Ok(index);
    }
    let needle = after.or(before).unwrap_or_default();
    let hits = locate::find_matches(body, needle)?;
    let Some(&(start, end)) = hits.first() else {
        return Err(ToolError::msg(format!(
            "no match for '{needle}' in this tab (matching is case-sensitive and cannot span paragraphs)"
        )));
    };
    if !snap {
        return Ok(if after.is_some() { end } else { start });
    }
    let Some((para_start, para_end)) = locate::paragraph_bounds(body, start) else {
        return Err(ToolError::msg(format!(
            "'{needle}' is inside a table cell; block content (headings, lists, tables) needs a \
             paragraph boundary, so anchor it to text outside the table instead."
        )));
    };
    Ok(if after.is_some() { para_end.min(locate::body_end(body)) } else { para_start })
}

/// Shared front half of delete_text/replace_text: fetch, pick the tab, resolve the locator.
///
/// `texts` (the document text each range covers) is taken here too: the resolved body borrows the
/// fetched document, which cannot outlive this call.
struct Located {
    did: String,
    doc: Value,
    tid: Option<String>,
    ranges: Vec<(i64, i64)>,
    described: String,
    texts: Vec<String>,
}

async fn locate_target(
    api: &dyn GoogleApi,
    item: &str,
    tab: Option<&str>,
    match_text: Option<&str>,
    section: Option<&str>,
    occurrence: i64,
) -> Result<Located> {
    let did = parse_ref(item)?.id;
    let doc = api.docs_get(&did).await?;
    let tab_id = tab_arg(tab.map(str::to_string), item);
    let (tid, ranges, described, texts) = {
        let (tid, body) = resolve_write_tab(&doc, tab_id.as_deref())?;
        let (ranges, described) = locate::resolve(body, match_text, section, occurrence)?;
        let texts = ranges.iter().map(|&(s, e)| locate::text_in_range(body, s, e)).collect();
        (tid, ranges, described, texts)
    };
    Ok(Located { did, doc, tid, ranges, described, texts })
}

/// Ranges bottom-up: each edit shifts everything after it, so applying the later ranges first
/// keeps the earlier ones valid with no offset arithmetic.
fn descending(ranges: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut out = ranges.to_vec();
    out.sort_by(|a, b| b.cmp(a));
    out
}

// ---- the tools ---------------------------------------------------------------------------

async fn read_document(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let output_format = args.str_or("output_format", "markdown")?;
    let chunk = args.i64_or("chunk", 0)?;
    let max_chars = args.i64_or("max_chars", DEFAULT_MAX_CHARS)?;
    let tab = args.opt_str("tab")?;
    let include_colors = args.bool_or("include_colors", false)?;

    let did = parse_ref(&item)?.id;
    let tab_id = tab_arg(tab, &item);
    let doc = api.docs_get(&did).await?;
    let all_tabs = flatten_tabs(array(doc.get("tabs")));

    let mut outline: Vec<Value> = Vec::new();
    let mut colored: Vec<Value> = Vec::new();
    let content = if all_tabs.is_empty() {
        let body = array(doc.pointer("/body/content"));
        let text = if output_format == "text" {
            content_to_text(body)
        } else {
            let (rendered, tab_outline) = content_to_markdown(body);
            outline = tab_outline;
            rendered
        };
        if include_colors {
            colored.extend(colored_runs(body));
        }
        text
    } else {
        let selected = selected_tabs(&all_tabs, tab_id.as_deref())?;
        let mut parts: Vec<String> = Vec::new();
        for t in &selected {
            if selected.len() > 1 {
                let title = t.title.unwrap_or_default();
                parts.push(if output_format == "text" { title.to_string() } else { format!("# {title}") });
                outline.push(json!({"level": 1, "text": t.title, "tab": t.id}));
            }
            if output_format == "text" {
                parts.push(content_to_text(t.body));
            } else {
                let (rendered, tab_outline) = content_to_markdown(t.body);
                parts.push(rendered);
                for mut entry in tab_outline {
                    set_key(&mut entry, "tab", json!(t.id));
                    outline.push(entry);
                }
            }
            if include_colors {
                colored.extend(colored_runs(t.body));
            }
        }
        parts.join("\n")
    };

    let tab_read = match tab_id.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => json!(t),
        None if all_tabs.len() > 1 => json!("all"),
        None => Value::Null,
    };
    let tabs: Vec<Value> = all_tabs.iter().map(|t| json!({"id": t.id, "title": t.title})).collect();
    let mut result = json!({
        "document_id": did,
        "title": doc.get("title").cloned().unwrap_or(Value::Null),
        // The revision the outline offsets describe. Pass it to a write tool to have the write
        // rejected rather than misapplied if the doc changed since this read.
        "revision_id": doc.get("revisionId").cloned().unwrap_or(Value::Null),
        "tabs": tabs,
        "tab_read": tab_read,
        "outline": outline,
    });
    merge(&mut result, paginate(&content, chunk, max_chars));
    if include_colors {
        set_key(&mut result, "colored_runs", json!(colored));
    }
    Ok(result.into())
}

async fn extract_images(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let tab = args.opt_str("tab")?;

    let did = parse_ref(&item)?.id;
    let tab_id = tab_arg(tab, &item);
    let doc = api.docs_get(&did).await?;
    let all_tabs = flatten_tabs(array(doc.get("tabs")));
    let uris: Vec<String> = if all_tabs.is_empty() {
        image_uris(
            array(doc.pointer("/body/content")),
            doc.get("inlineObjects"),
            doc.get("positionedObjects"),
        )
    } else {
        selected_tabs(&all_tabs, tab_id.as_deref())?
            .iter()
            .flat_map(|t| image_uris(t.body, t.inline_objects, t.positioned_objects))
            .collect()
    };

    let mut images: Vec<Image> = Vec::new();
    for uri in uris {
        if !is_google_host(&uri) {
            continue;
        }
        // fetch_image refuses redirects; None means "skip this one".
        if let Some((data, format)) = api.fetch_image(&uri).await? {
            images.push(Image { data, format });
        }
    }
    Ok(ToolOutput::Images(images))
}

async fn create_document(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let title = args.req_str("title")?;
    let text = args.opt_str("text")?;
    let markdown = args.bool_or("markdown", false)?;

    let doc = api.docs_create(&title, "documentId,title").await?;
    let did = doc.get("documentId").and_then(Value::as_str).unwrap_or_default().to_string();
    let mut result = json!({
        "id": did,
        "url": format!("https://docs.google.com/document/d/{did}/edit"),
        "title": doc.get("title").cloned().unwrap_or(Value::Null),
    });
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return Ok(result.into());
    };

    let segments = markdown.then(|| split_blocks(&text));
    if let Some(segments) = segments.as_deref().filter(|s| has_table(s)) {
        // structural (table) content
        insert_markdown_segments(api, &did, None, 1, segments, false).await?;
        set_key(&mut result, "tables", json!(table_segment_count(segments)));
        return Ok(result.into());
    }
    let parsed = markdown.then(|| parse_markdown(&text));
    let insert = parsed.as_ref().map_or(text, |p| p.text.clone());
    let mut requests = vec![json!({"insertText": {"location": {"index": 1}, "text": insert}})];
    if let Some(p) = &parsed {
        requests.extend(style_requests(p, 1, None));
    }
    api.docs_batch_update(&did, &json!({ "requests": requests })).await?;
    set_key(&mut result, "chars", json!(insert.chars().count()));
    if let Some(p) = &parsed {
        set_key(&mut result, "markdown", md_summary(p));
    }
    Ok(result.into())
}

/// The tail paragraph's text, minus its trailing newline — empty when block markdown may merge
/// into it.
fn tail_text(body: &[Value]) -> String {
    body.last()
        .and_then(|el| el.get("paragraph"))
        .map(para_text)
        .unwrap_or_default()
        .trim_end_matches('\n')
        .to_string()
}

async fn append_text(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let text = args.req_str("text")?;
    let tab = args.opt_str("tab")?;
    let color = args.opt_str("color")?;
    let markdown = args.bool_or("markdown", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let did = parse_ref(&item)?.id;
    let doc = api.docs_get(&did).await?;
    let tab_id = tab_arg(tab, &item);
    let (tid, body) = resolve_write_tab(&doc, tab_id.as_deref())?;
    // The last index content may occupy: body[-1].endIndex - 1.
    let start = locate::body_end(body);

    if markdown {
        let segments = split_blocks(&text);
        if has_table(&segments) {
            // structural (table) content -> segmented write path
            let n_tables = table_segment_count(&segments);
            if dry_run {
                return Ok(json!({
                    "dry_run": true, "action": "append_text", "tab": tid, "at_index": start,
                    "tables": n_tables, "preview": render_markdown_preview(&segments),
                })
                .into());
            }
            let tail_nonempty = !tail_text(body).is_empty();
            insert_markdown_segments(api, &did, tid.as_deref(), start, &segments, tail_nonempty).await?;
            return Ok(json!({
                "document_id": did, "tab": tid, "inserted_at": start, "tables": n_tables,
            })
            .into());
        }
    }

    // validate before any dry-run return
    let rgb = match color.as_deref() {
        Some(c) => Some(hex_to_rgb(c)?),
        None => None,
    };
    let parsed = markdown.then(|| parse_markdown(&text));
    let mut insert = parsed.as_ref().map_or(text, |p| p.text.clone());
    let mut base = start;
    if parsed.as_ref().is_some_and(ParsedMarkdown::has_blocks) && !tail_text(body).is_empty() {
        insert = format!("\n{insert}"); // blocks start on a fresh paragraph, not inside the tail one
        base = start + 1;
    }
    if dry_run {
        let tail = last_chars(&content_to_text(body), PREVIEW_CHARS);
        let mut out = json!({
            "dry_run": true,
            "action": "append_text",
            "tab": tid,
            "at_index": start,
            "chars": insert.chars().count(),
            "color": color,
            "before_tail": tail,
            "after_tail": format!("{tail}{insert}"),
        });
        if let Some(p) = &parsed {
            set_key(&mut out, "markdown", md_summary(p));
        }
        return Ok(out.into());
    }
    let mut requests = vec![json!({"insertText": {"location": loc(start, tid.as_deref()), "text": insert}})];
    if let Some(rgb) = &rgb {
        requests.push(color_request(start, u16len(&insert), tid.as_deref(), rgb));
    }
    if let Some(p) = &parsed {
        requests.extend(style_requests(p, base, tid.as_deref()));
    }
    api.docs_batch_update(&did, &json!({ "requests": requests })).await?;
    let mut result = json!({
        "document_id": did,
        "tab": tid,
        "inserted_at": start,
        "chars": insert.chars().count(),
        "color": color,
    });
    if let Some(p) = &parsed {
        set_key(&mut result, "markdown", md_summary(p));
    }
    Ok(result.into())
}

async fn insert_text(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let text = args.req_str("text")?;
    let index = args.opt_i64("index")?;
    let after = args.opt_str("after")?;
    let before = args.opt_str("before")?;
    let tab = args.opt_str("tab")?;
    let color = args.opt_str("color")?;
    let markdown = args.bool_or("markdown", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let did = parse_ref(&item)?.id;
    let doc = api.docs_get(&did).await?;
    let tab_id = tab_arg(tab, &item);
    let (tid, body) = resolve_write_tab(&doc, tab_id.as_deref())?;

    if markdown {
        let segments = split_blocks(&text);
        if has_table(&segments) {
            // structural (table) content -> segmented write path
            let at = anchor(body, index, after.as_deref(), before.as_deref(), true, true)?;
            let n_tables = table_segment_count(&segments);
            if dry_run {
                return Ok(json!({
                    "dry_run": true, "action": "insert_text", "tab": tid, "at_index": at,
                    "tables": n_tables, "preview": render_markdown_preview(&segments),
                })
                .into());
            }
            insert_markdown_segments(api, &did, tid.as_deref(), at, &segments, false).await?;
            return Ok(json!({
                "document_id": did, "tab": tid, "inserted_at": at, "tables": n_tables,
            })
            .into());
        }
    }

    // validate before any dry-run return
    let rgb = match color.as_deref() {
        Some(c) => Some(hex_to_rgb(c)?),
        None => None,
    };
    let parsed = markdown.then(|| parse_markdown(&text));
    let insert = parsed.as_ref().map_or(text, |p| p.text.clone());
    // Block markdown snaps to a paragraph boundary; inline/plain text inserts exactly where the
    // locator points, so `after="foo"` lands immediately after the 'foo' it matched.
    let snap = parsed.as_ref().is_some_and(ParsedMarkdown::has_blocks);
    let at = anchor(body, index, after.as_deref(), before.as_deref(), snap, true)?;
    if dry_run {
        let mut out = json!({
            "dry_run": true,
            "action": "insert_text",
            "tab": tid,
            "at_index": at,
            "chars": insert.chars().count(),
            "color": color,
            "would_insert": insert,
        });
        if let Some(p) = &parsed {
            set_key(&mut out, "markdown", md_summary(p));
        }
        return Ok(out.into());
    }
    let mut requests = vec![json!({"insertText": {"location": loc(at, tid.as_deref()), "text": insert}})];
    if let Some(rgb) = &rgb {
        requests.push(color_request(at, u16len(&insert), tid.as_deref(), rgb));
    }
    if let Some(p) = &parsed {
        requests.extend(style_requests(p, at, tid.as_deref()));
    }
    api.docs_batch_update(&did, &json!({ "requests": requests })).await?;
    let mut result = json!({
        "document_id": did,
        "tab": tid,
        "inserted_at": at,
        "chars": insert.chars().count(),
        "color": color,
    });
    if let Some(p) = &parsed {
        set_key(&mut result, "markdown", md_summary(p));
    }
    Ok(result.into())
}

async fn insert_table(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let rows = args.req_rows("rows")?;
    let index = args.opt_i64("index")?;
    let after = args.opt_str("after")?;
    let before = args.opt_str("before")?;
    let tab = args.opt_str("tab")?;
    let header = args.bool_or("header", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let (n_rows, n_cols) = validate_rows(&rows)?;
    let did = parse_ref(&item)?.id;
    let doc = api.docs_get(&did).await?;
    let tab_id = tab_arg(tab, &item);
    let (tid, body) = resolve_write_tab(&doc, tab_id.as_deref())?;
    let start = anchor(body, index, after.as_deref(), before.as_deref(), true, false)?;

    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "action": "insert_table",
            "tab": tid,
            "at_index": start,
            "rows": n_rows,
            "cols": n_cols,
            "header": header,
            "preview": render_table_markdown(&rows),
        })
        .into());
    }

    let pre = table_starts(body);
    let insert = json!({"requests": [
        {"insertTable": {"rows": n_rows, "columns": n_cols, "location": loc(start, tid.as_deref())}}
    ]});
    if let Err(err) = api.docs_batch_update(&did, &insert).await {
        if let (Some(index), Some(400)) = (index, err.status()) {
            return Err(ToolError::msg(format!(
                "could not insert a table at index {index}: index must be a Docs structural offset at \
                 a paragraph boundary (not a character count from read_document). Use after='<text>' or \
                 before='<text>' to locate the spot by content — those always resolve to a valid \
                 boundary — or omit index to append."
            )));
        }
        return Err(err);
    }

    let content = |r: usize, c: usize| -> (String, Vec<Span>) {
        let text = rows.get(r).and_then(|row| row.get(c)).map(md::cell_text).unwrap_or_default();
        let spans = if header && r == 0 && !text.is_empty() {
            vec![(0, u16len(&text), vec!["bold"])]
        } else {
            Vec::new()
        };
        (text, spans)
    };
    let filled = fill_new_table(api, &did, tid.as_deref(), &pre, &content).await?;
    Ok(json!({
        "document_id": did,
        "tab": tid,
        "inserted_at": start,
        "rows": n_rows,
        "cols": n_cols,
        "cells_filled": filled,
        "header": header,
    })
    .into())
}

async fn delete_text(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let match_text = args.opt_str("match")?;
    let section = args.opt_str("section")?;
    let occurrence = args.i64_or("occurrence", 1)?;
    let tab = args.opt_str("tab")?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let found =
        locate_target(api, &item, tab.as_deref(), match_text.as_deref(), section.as_deref(), occurrence)
            .await?;
    let impact = json!({
        "tab": found.tid,
        "target": found.described,
        "occurrences": found.ranges.len(),
        "chars": found.ranges.iter().map(|(s, e)| e - s).sum::<i64>(),
        "ranges": found.ranges.iter().map(|(s, e)| json!({"start": s, "end": e})).collect::<Vec<_>>(),
        "deletes_text": found.texts.iter().map(|t| clip(t)).collect::<Vec<_>>(),
    });
    if dry_run {
        let mut out = json!({"dry_run": true, "action": "delete_text"});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if !confirm {
        return Ok(preview_response("delete_text", impact).into());
    }

    let requests: Vec<Value> = descending(&found.ranges)
        .into_iter()
        .map(|(s, e)| json!({"deleteContentRange": {"range": doc_range(s, e, found.tid.as_deref())}}))
        .collect();
    let mut body = json!({ "requests": requests });
    merge(&mut body, write_control(&found.doc));
    api.docs_batch_update(&found.did, &body).await?;
    let mut result = json!({"document_id": found.did, "deleted": true});
    merge(&mut result, fields(&impact));
    Ok(result.into())
}

async fn replace_text(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let replacement = args.req_str("replacement")?;
    let match_text = args.opt_str("match")?;
    let section = args.opt_str("section")?;
    let occurrence = args.i64_or("occurrence", 1)?;
    let tab = args.opt_str("tab")?;
    let markdown = args.bool_or("markdown", false)?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    if markdown && has_table(&split_blocks(&replacement)) {
        return Err(ToolError::msg(
            "replace_text cannot write pipe tables (a table needs a separate insert+fill pass, \
             which would break the single-batch replace). Use insert_table, or drop markdown=true.",
        ));
    }
    let found =
        locate_target(api, &item, tab.as_deref(), match_text.as_deref(), section.as_deref(), occurrence)
            .await?;
    let parsed = markdown.then(|| parse_markdown(&replacement));
    let insert = parsed.as_ref().map_or(replacement, |p| p.text.clone());
    let mut impact = json!({
        "tab": found.tid,
        "target": found.described,
        "occurrences": found.ranges.len(),
        "ranges": found.ranges.iter().map(|(s, e)| json!({"start": s, "end": e})).collect::<Vec<_>>(),
        "replaces_text": found.texts.iter().map(|t| clip(t)).collect::<Vec<_>>(),
        "with_text": clip(&insert),
    });
    if let Some(p) = &parsed {
        set_key(&mut impact, "markdown", md_summary(p));
    }
    if dry_run {
        let mut out = json!({"dry_run": true, "action": "replace_text"});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if !confirm {
        return Ok(preview_response("replace_text", impact).into());
    }

    // Descending, delete+insert paired per range: going bottom-up means an earlier range's
    // indexes are never disturbed by an edit made below it, so one batch is enough.
    let tid = found.tid.as_deref();
    let mut requests: Vec<Value> = Vec::new();
    for (s, e) in descending(&found.ranges) {
        requests.push(json!({"deleteContentRange": {"range": doc_range(s, e, tid)}}));
        if !insert.is_empty() {
            requests.push(json!({"insertText": {"location": loc(s, tid), "text": insert}}));
            if let Some(p) = &parsed {
                requests.extend(style_requests(p, s, tid));
            }
        }
    }
    let mut body = json!({ "requests": requests });
    merge(&mut body, write_control(&found.doc));
    api.docs_batch_update(&found.did, &body).await?;
    let mut result = json!({"document_id": found.did, "replaced": true});
    merge(&mut result, fields(&impact));
    Ok(result.into())
}

const COMMENT_FIELDS: &str = "nextPageToken, comments(id,author/displayName,content,resolved,\
                              createdTime,replies(content,author/displayName))";

async fn read_comments(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let page_size = args.i64_or("page_size", 100)?;
    let page_token = args.opt_str("page_token")?;

    let fid = parse_ref(&item)?.id;
    let resp =
        api.drive_comments_list(&fid, page_size.clamp(1, 100), page_token.as_deref(), COMMENT_FIELDS).await?;
    let token = resp.get("nextPageToken").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "file_id": fid,
        "comments": resp.get("comments").cloned().unwrap_or_else(|| json!([])),
        // An empty nextPageToken means "no more pages"; Python's bool() read it that way, and a
        // caller that trusts has_more would otherwise page forever.
        "has_more": token.as_str().is_some_and(|t| !t.is_empty()),
        "next_page_token": token,
    })
    .into())
}

async fn add_comment(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let content = args.req_str("content")?;

    let fid = parse_ref(&item)?.id;
    let created = api.drive_comments_create(&fid, &content, "id,content").await?;
    Ok(json!({
        "file_id": fid,
        "comment_id": created.get("id").cloned().unwrap_or(Value::Null),
    })
    .into())
}

// ---- registration -------------------------------------------------------------------------

const READ_DOCUMENT_DOC: &str = r"Read a Google Doc (item = URL or ID) as 'markdown' (default) or 'text', one bounded chunk at a time.

Multi-tab docs: by default every tab is read (each prefixed with its title as a heading);
pass a `tab` id, or an `item` URL containing `tab=t.xxxx`, to read only that tab. Returns the
requested chunk (0-based) plus title, the doc's `tabs`, the outline, `revision_id`, and paging
metadata (total_chunks, total_chars, has_more). Set max_chars<=0 to get the whole document. Set
include_colors=true to also return `colored_runs` (text spans with an explicit foreground color).

Each `outline` entry carries the heading's `start`/`end` document offsets. Offsets in the
returned *content* cannot be computed by counting characters — the markdown is rendered
(heading prefixes, table delimiter rows, escaped pipes), so it does not align with the doc's
index space. To edit, pass the heading text to a write tool's `section=`/`after=`/`before=`
locator and let the server resolve the offsets.";

const EXTRACT_IMAGES_DOC: &str = r"Extract embedded images from a Google Doc (item = URL or ID) as viewable images, in order.

Returns both inline images and positioned (floating/wrapped) images, each emitted at the
paragraph it is anchored to. Multi-tab docs: all tabs by default; pass a `tab` id or an
`item` URL with `tab=t.xxxx`.";

const CREATE_DOCUMENT_DOC: &str = r"Create a new Google Doc (in My Drive root), optionally with initial text content.

markdown=true renders `text` with the same dialect as append_text: headings, bullets,
numbered lists, **bold**, *italic*, <u>underline</u>, and GFM pipe tables. (For spreadsheets
use create_spreadsheet.)";

const APPEND_TEXT_DOC: &str = r"Append text to the end of a Google Doc (item = URL or ID).

markdown=true renders a small dialect instead of storing text verbatim: '#'..'######'
headings, '-'/'*' bullets, '1.' numbered lists (nest with two spaces or a tab per level),
**bold**, *italic*, <u>underline</u>, and GFM pipe tables (a header row immediately followed
by a '| --- | --- |' delimiter row; write a literal pipe inside a cell as '\|'). No other
escape syntax — text that looks like markup gets styled; leave markdown unset to write
literally. Block content (headings/lists/tables) starts on its own paragraph rather than
merging into the doc's last line. Multi-tab docs: appends to the given `tab` (id, or an
`item` URL with `tab=t.xxxx`), else the first tab. `color` (hex like '#3366CC') colors the
inserted text (ignored for tables); leave it unset for plain text. dry_run=true returns a
predicted before/after of the tab's tail without writing.";

const INSERT_TEXT_DOC: &str = r"Insert text into a Google Doc (item = URL or ID) at a location found by content.

Pass exactly one anchor. Prefer `after` or `before`: a literal, case-sensitive substring
(within one paragraph) that the insertion goes after or before — the server resolves it to a
real offset, so it is always valid. `index` is the raw alternative: a Docs STRUCTURAL offset,
NOT a character count into read_document's content (that text is rendered markdown and does
not align with the document's index space) — use it only if you already hold a real offset.

markdown=true renders the same dialect as append_text (headings, bullets, numbered lists,
**bold**, *italic*, <u>underline</u>, and GFM pipe tables). Block content anchored with
after/before snaps to a paragraph boundary so heading and list styling cannot bleed into a
neighbouring paragraph. Multi-tab docs: pass a `tab` (id, or an `item` URL with `tab=t.xxxx`),
else the first tab. `color` (hex like '#3366CC') colors the inserted text (ignored for
tables); leave it unset for plain text. dry_run=true reports what would be inserted and where
without writing.";

const INSERT_TABLE_DOC: &str = r#"Insert a table into a Google Doc (item = URL or ID), filled from `rows`.

`rows` is a list of equal-length row lists (e.g. [["Name","Role"],["Ada","Eng"]]); empty cells
are left blank. header=true bolds the first row. Appends at the end of the target tab by
default. To place it, prefer `after` or `before`: a literal, case-sensitive substring (within
one paragraph) whose paragraph the table goes after or before — these always resolve to the
paragraph boundary insertTable requires. `index` is the raw alternative, a Docs STRUCTURAL
offset at a paragraph boundary (not a character count from read_document), and is rejected by
the API if it lands mid-paragraph. Multi-tab docs: pass a `tab` id (or an `item` URL with
`tab=t.xxxx`), else the first tab. dry_run=true previews the table without writing."#;

const DELETE_TEXT_DOC: &str = r"Delete text from a Google Doc (item = URL or ID), located by content rather than by index.

Pass exactly one locator: `match` (a literal substring, case-sensitive, must lie within a
single paragraph) or `section` (a heading's exact text — removes that heading and everything
under it, up to the next heading of the same or higher level). Locators match the document's
plain text, not the markdown `read_document` renders: a heading that reads back as '# Title'
matches only as 'Title', and a table cell's escaped '\|' only as '|', so strip that rendering
out of a locator copied from a read. For `match`, `occurrence` is 1-based and defaults to the
first; pass occurrence=0 to delete every occurrence.

Multi-tab docs: pass a `tab` id (or an `item` URL with `tab=t.xxxx`), else the first tab.
dry_run=true reports exactly what would be removed without writing. This tool is destructive:
without confirm=true it returns a preview of the text it would delete and writes nothing.";

const REPLACE_TEXT_DOC: &str = r"Replace text in a Google Doc (item = URL or ID), located by content rather than by index.

Locators work exactly as in `delete_text`: exactly one of `match` (literal, case-sensitive,
within one paragraph) or `section` (a heading and its content), with 1-based `occurrence`
over a match (0 = every occurrence). Locators match the document's plain text, not the
markdown `read_document` renders: a heading that reads back as '# Title' matches only as
'Title', and a table cell's escaped '\|' only as '|', so strip that rendering out of a
locator copied from a read. markdown=true renders `replacement` in the same dialect as
`append_text` minus pipe tables (use `insert_table` for those).

Each occurrence is deleted and rewritten in a single batch, so the doc is never left with the
old text removed and the new text missing. Multi-tab docs: pass a `tab` id (or an `item` URL
with `tab=t.xxxx`), else the first tab. dry_run=true previews without writing. This tool is
destructive: without confirm=true it returns a preview and writes nothing.";

const READ_COMMENTS_DOC: &str = r"List comments on a Doc or file (item = URL or ID).

Returns one page; when `has_more` is true, pass the returned `next_page_token` to continue.";

const ADD_COMMENT_DOC: &str = "Add an (unanchored) comment to a Doc or file (item = URL or ID).";

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "read_document",
            READ_DOCUMENT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "output_format": {"type": "string", "default": "markdown"},
                    "chunk": {"type": "integer", "default": 0},
                    "max_chars": {"type": "integer", "default": DEFAULT_MAX_CHARS},
                    "tab": {"type": "string"},
                    "include_colors": {"type": "boolean", "default": false},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "extract_images",
            EXTRACT_IMAGES_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "tab": {"type": "string"},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "create_document",
            CREATE_DOCUMENT_DOC,
            json!({
                "properties": {
                    "title": {"type": "string"},
                    "text": {"type": "string"},
                    "markdown": {"type": "boolean", "default": false},
                },
                "required": ["title"],
            }),
        ),
        ToolDef::new(
            "append_text",
            APPEND_TEXT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "text": {"type": "string"},
                    "tab": {"type": "string"},
                    "color": {"type": "string"},
                    "markdown": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "text"],
            }),
        ),
        ToolDef::new(
            "insert_text",
            INSERT_TEXT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "text": {"type": "string"},
                    "index": {"type": "integer"},
                    "after": {"type": "string"},
                    "before": {"type": "string"},
                    "tab": {"type": "string"},
                    "color": {"type": "string"},
                    "markdown": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "text"],
            }),
        ),
        ToolDef::new(
            "insert_table",
            INSERT_TABLE_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "rows": {"type": "array", "items": {"type": "array"}},
                    "index": {"type": "integer"},
                    "after": {"type": "string"},
                    "before": {"type": "string"},
                    "tab": {"type": "string"},
                    "header": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "rows"],
            }),
        ),
        ToolDef::new(
            "delete_text",
            DELETE_TEXT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "match": {"type": "string"},
                    "section": {"type": "string"},
                    "occurrence": {"type": "integer", "default": 1},
                    "tab": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "replace_text",
            REPLACE_TEXT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "replacement": {"type": "string"},
                    "match": {"type": "string"},
                    "section": {"type": "string"},
                    "occurrence": {"type": "integer", "default": 1},
                    "tab": {"type": "string"},
                    "markdown": {"type": "boolean", "default": false},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "replacement"],
            }),
        ),
        ToolDef::new(
            "read_comments",
            READ_COMMENTS_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "page_size": {"type": "integer", "default": 100},
                    "page_token": {"type": "string"},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "add_comment",
            ADD_COMMENT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["item", "content"],
            }),
        ),
    ]
}

pub async fn dispatch(name: &str, api: &dyn GoogleApi, args: &Args) -> Option<Result<ToolOutput>> {
    Some(match name {
        "read_document" => read_document(api, args).await,
        "extract_images" => extract_images(api, args).await,
        "create_document" => create_document(api, args).await,
        "append_text" => append_text(api, args).await,
        "insert_text" => insert_text(api, args).await,
        "insert_table" => insert_table(api, args).await,
        "delete_text" => delete_text(api, args).await,
        "replace_text" => replace_text(api, args).await,
        "read_comments" => read_comments(api, args).await,
        "add_comment" => add_comment(api, args).await,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    //! Tool-logic tests against the recording fake.
    //!
    //! These verify the invariants this module owns — the confirm-before-destructive gate (no
    //! mutation without confirm), the two-phase table write, and the exact API requests we build
    //! — without any live Google API.

    use super::*;
    use crate::tools::testing::FakeApi;

    /// Bare IDs must be >= 20 chars to parse.
    fn sid() -> String {
        "A".repeat(30)
    }

    fn args_for(tool: &str, v: Value) -> Args {
        let def = defs().into_iter().find(|d| d.name == tool).unwrap();
        let Value::Object(map) = v else { panic!("test args must be an object") };
        Args::new(tool, Some(map), &def.params()).unwrap()
    }

    async fn call(api: &FakeApi, tool: &str, v: Value) -> Result<ToolOutput> {
        dispatch(tool, api, &args_for(tool, v)).await.unwrap()
    }

    async fn ok(api: &FakeApi, tool: &str, v: Value) -> Value {
        match call(api, tool, v).await.unwrap() {
            ToolOutput::Json(v) => v,
            ToolOutput::Images(_) => panic!("expected a JSON result"),
        }
    }

    async fn fails(api: &FakeApi, tool: &str, v: Value) -> String {
        call(api, tool, v).await.unwrap_err().to_string()
    }

    /// A paragraph element with one text run per string, indexed the way the API indexes them.
    fn para(start: i64, runs: &[&str]) -> Value {
        styled(start, runs, None)
    }

    fn styled(start: i64, runs: &[&str], style: Option<&str>) -> Value {
        let mut elements = Vec::new();
        let mut idx = start;
        for content in runs {
            elements.push(json!({"startIndex": idx, "textRun": {"content": content}}));
            idx += u16len(content);
        }
        let mut paragraph = json!({ "elements": elements });
        if let Some(style) = style {
            paragraph["paragraphStyle"] = json!({ "namedStyleType": style });
        }
        json!({"startIndex": start, "endIndex": idx, "paragraph": paragraph})
    }

    fn tab_doc(content: Value) -> Value {
        json!({"tabs": [{
            "tabProperties": {"tabId": "t.0", "title": "T"},
            "documentTab": {"body": {"content": content}},
        }]})
    }

    fn locator_doc(body: Value, revision: Option<&str>, tabbed: bool) -> Value {
        let mut doc = json!({});
        if let Some(rev) = revision {
            doc["revisionId"] = json!(rev);
        }
        if tabbed {
            doc["tabs"] = tab_doc(body)["tabs"].clone();
        } else {
            doc["body"] = json!({ "content": body });
        }
        doc
    }

    fn doc_body() -> Value {
        json!([para(1, &["keep alpha keep\n"]), para(17, &["alpha again\n"])])
    }

    fn nth_requests(api: &FakeApi, n: usize) -> Vec<Value> {
        api.calls_to("docs_batch_update")[n]["body"]["requests"].as_array().cloned().unwrap()
    }

    fn requests(api: &FakeApi) -> Vec<Value> {
        api.last("docs_batch_update")["body"]["requests"].as_array().cloned().unwrap()
    }

    fn body_arg(api: &FakeApi) -> Value {
        api.last("docs_batch_update")["body"].clone()
    }

    /// The request kinds in order — each request is a single-key object.
    fn kinds(reqs: &[Value]) -> Vec<String> {
        reqs.iter().filter_map(|r| r.as_object().and_then(|o| o.keys().next()).cloned()).collect()
    }

    fn of_kind(reqs: &[Value], kind: &str) -> Vec<Value> {
        reqs.iter().filter_map(|r| r.get(kind).cloned()).collect()
    }

    // ---- pure helpers --------------------------------------------------------------------

    #[test]
    fn only_google_owned_hosts_may_receive_the_oauth_token() {
        assert!(is_google_host("https://lh3.googleusercontent.com/AbC"));
        assert!(is_google_host("https://docs.google.com/x"));
        assert!(!is_google_host("https://evil.com/x"));
        assert!(!is_google_host("https://googleusercontent.com.evil.com/x")); // suffix-spoof
    }

    #[test]
    fn hex_colors_round_trip_and_reject_names() {
        assert_eq!(hex_to_rgb("#FF0000").unwrap(), json!({"red": 1.0, "green": 0.0, "blue": 0.0}));
        assert_eq!(hex_to_rgb("00ff00").unwrap()["green"], json!(1.0)); // leading # optional
        assert_eq!(rgb_to_hex(&json!({"red": 1.0})), "#ff0000"); // missing channels default to 0
        let e = hex_to_rgb("blue").unwrap_err().to_string();
        assert_eq!(e, "color must be a 6-digit hex string like '#3366CC', got 'blue'");
    }

    #[test]
    fn a_channel_rounds_half_to_even_the_way_python_does() {
        // 0.5/255 * 255 is exactly 0.5: Python's round() gives 0, not 1.
        assert_eq!(hex_channel(0.5 / 255.0), "00");
        assert_eq!(hex_channel(1.5 / 255.0), "02");
        assert_eq!(hex_channel(1.0), "ff");
    }

    #[test]
    fn colored_runs_reports_only_the_runs_carrying_a_color() {
        let content = [json!({"paragraph": {"elements": [
            {"textRun": {"content": "red\n", "textStyle": {"foregroundColor": {"color": {"rgbColor": {"red": 1.0}}}}}},
            {"textRun": {"content": "plain", "textStyle": {}}},
        ]}})];
        assert_eq!(colored_runs(&content), vec![json!({"text": "red", "color": "#ff0000"})]);
    }

    #[test]
    fn markdown_rendering_prefixes_headings_and_re_parseable_tables() {
        // Elements carry startIndex/endIndex exactly as the API returns them: the outline's write
        // anchors come from those offsets.
        let content = [
            json!({"startIndex": 1, "endIndex": 7,
                   "paragraph": {"paragraphStyle": {"namedStyleType": "HEADING_1"},
                                 "elements": [{"startIndex": 1, "textRun": {"content": "Title\n"}}]}}),
            json!({"startIndex": 7, "endIndex": 17,
                   "paragraph": {"elements": [{"startIndex": 7, "textRun": {"content": "body text\n"}}]}}),
            json!({"table": {"tableRows": [
                {"tableCells": [
                    {"content": [{"paragraph": {"elements": [{"textRun": {"content": "a | x"}}]}}]},
                    {"content": [{"paragraph": {"elements": [{"textRun": {"content": "b"}}]}}]},
                ]},
                {"tableCells": [
                    {"content": [{"paragraph": {"elements": [{"textRun": {"content": "1"}}]}}]},
                    {"content": [{"paragraph": {"elements": [{"textRun": {"content": "2"}}]}}]},
                ]},
            ]}}),
        ];
        let (rendered, outline) = content_to_markdown(&content);
        assert!(rendered.contains("# Title"));
        assert!(rendered.contains("body text"));
        // GFM: header row, a column-matched delimiter row, then the body row; '|' is escaped
        assert!(rendered.contains("| a \\| x | b |\n| --- | --- |\n| 1 | 2 |"));
        // start/end make each outline entry a usable write anchor, not just a table of contents
        assert_eq!(outline, vec![json!({"level": 1, "text": "Title", "start": 1, "end": 7})]);
    }

    #[test]
    fn a_multi_paragraph_cell_is_flattened_onto_one_row() {
        // A cell's newlines become spaces (and its pipes are escaped) before it is emitted, which
        // is what stops a cell forging extra rows — or a fake delimiter row — when the writer
        // re-parses the markdown this reader produced.
        let cell = |paras: Vec<Value>| json!({ "content": paras });
        let content = [json!({"table": {"tableRows": [
            {"tableCells": [
                cell(vec![para(1, &["one\n"]), para(5, &["| --- |\n"])]),
                cell(vec![para(13, &["b\n"])]),
            ]},
        ]}})];
        let (rendered, _) = content_to_markdown(&content);
        assert_eq!(rendered, "| one \\| --- \\| | b |\n| --- | --- |");
        // re-parses as the one two-column row it was, not as a second row or a delimiter
        assert_eq!(
            split_blocks(&rendered),
            vec![Segment::Table(vec![vec!["one | --- |".to_string(), "b".to_string()]])]
        );
    }

    #[test]
    fn text_rendering_skips_tables_that_a_locator_can_still_reach() {
        // Deliberate asymmetry: read_document(output_format='text') omits table text, but a
        // locator can still target it. Asserted so it stays a decision rather than an accident.
        let table = json!({"startIndex": 1, "endIndex": 13, "table": {"tableRows": [
            {"tableCells": [{"content": [para(4, &["in-cell"])]}]},
        ]}});
        let body = [table];
        assert_eq!(content_to_text(&body), "");
        assert_ne!(locate::find_matches(&body, "in-cell").unwrap(), vec![]);
    }

    #[test]
    fn tabs_flatten_depth_first_with_their_children() {
        let tabs = [
            json!({"tabProperties": {"tabId": "t.1", "title": "Parent"},
            "documentTab": {"body": {"content": ["p-body"]}},
            "childTabs": [
                {"tabProperties": {"tabId": "t.1a", "title": "Child"},
                 "documentTab": {"body": {"content": ["c-body"]}}},
            ]}),
            json!({"tabProperties": {"tabId": "t.2", "title": "Second"},
                   "documentTab": {"body": {"content": ["s-body"]}}}),
        ];
        let flat = flatten_tabs(&tabs);
        let seen: Vec<(Option<&str>, Option<&str>)> = flat.iter().map(|t| (t.id, t.title)).collect();
        assert_eq!(
            seen,
            vec![(Some("t.1"), Some("Parent")), (Some("t.1a"), Some("Child")), (Some("t.2"), Some("Second")),]
        );
    }

    #[test]
    fn a_flattened_tab_carries_its_inline_and_positioned_objects() {
        let tabs = [json!({
            "tabProperties": {"tabId": "t.1", "title": "T"},
            "documentTab": {
                "body": {"content": ["b"]},
                "inlineObjects": {"io": {}},
                "positionedObjects": {"po": {}},
            },
        })];
        let flat = flatten_tabs(&tabs);
        assert_eq!(flat[0].inline_objects, Some(&json!({"io": {}})));
        assert_eq!(flat[0].positioned_objects, Some(&json!({"po": {}})));
    }

    #[test]
    fn image_uris_come_back_in_document_order() {
        let content = [json!({"paragraph": {"elements": [
            {"inlineObjectElement": {"inlineObjectId": "io1"}},
            {"textRun": {"content": "between"}},
            {"inlineObjectElement": {"inlineObjectId": "io2"}},
        ]}})];
        let inline = json!({
            "io1": {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "u1"}}}},
            "io2": {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "u2"}}}},
        });
        assert_eq!(image_uris(&content, Some(&inline), None), ["u1", "u2"]);
    }

    #[test]
    fn a_positioned_image_is_emitted_before_its_paragraphs_inline_images() {
        let content = [
            json!({"paragraph": {
                "positionedObjectIds": ["po1"],
                "elements": [{"inlineObjectElement": {"inlineObjectId": "io1"}}],
            }}),
            json!({"paragraph": {"positionedObjectIds": ["po2", "po-imageless"], "elements": []}}),
        ];
        let inline = json!({
            "io1": {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "iu1"}}}},
        });
        let positioned = json!({
            "po1": {"positionedObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "pu1"}}}},
            "po2": {"positionedObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "pu2"}}}},
            "po-imageless": {"positionedObjectProperties": {"embeddedObject": {}}},  // e.g. a drawing
        });
        assert_eq!(image_uris(&content, Some(&inline), Some(&positioned)), ["pu1", "iu1", "pu2"]);
    }

    #[test]
    fn a_tabless_doc_writes_to_its_top_level_body() {
        let legacy = json!({"body": {"content": ["b"]}});
        let (tid, body) = resolve_write_tab(&legacy, None).unwrap();
        assert_eq!((tid, body), (None, &[json!("b")][..]));
    }

    #[test]
    fn a_tabbed_doc_defaults_to_the_first_tab_and_names_the_ones_it_has() {
        let tabbed = json!({"tabs": [
            {"tabProperties": {"tabId": "t.1", "title": "A"}, "documentTab": {"body": {"content": ["a"]}}},
            {"tabProperties": {"tabId": "t.2", "title": "B"}, "documentTab": {"body": {"content": ["b"]}}},
        ]});
        assert_eq!(resolve_write_tab(&tabbed, None).unwrap().0.as_deref(), Some("t.1"));
        let (tid, body) = resolve_write_tab(&tabbed, Some("t.2")).unwrap();
        assert_eq!((tid.as_deref(), body), (Some("t.2"), &[json!("b")][..]));
        assert_eq!(
            resolve_write_tab(&tabbed, Some("t.bogus")).unwrap_err().to_string(),
            "tab 't.bogus' not found; available: [('t.1', 'A'), ('t.2', 'B')]"
        );
    }

    #[test]
    fn a_grid_must_be_rectangular_and_bounded() {
        let rows = |v: Value| -> Vec<Vec<Value>> {
            v.as_array().unwrap().iter().map(|r| r.as_array().cloned().unwrap()).collect()
        };
        let msg = |v: Value| validate_rows(&rows(v)).unwrap_err().to_string();
        assert_eq!(msg(json!([])), "rows must be a non-empty list of row lists");
        assert_eq!(msg(json!([[]])), "rows must have at least one column");
        assert_eq!(
            msg(json!([["a", "b"], ["c"]])),
            "all rows must have the same number of columns; got row widths [2, 1]"
        );
        let wide: Value = json!([vec!["x"; 21]]);
        assert_eq!(msg(wide), "too many columns (21); max 20");
        let huge: Value = json!(vec![vec!["x"; 20]; 600]);
        assert_eq!(msg(huge), "table too large (600x20 = 12000 cells); max 10000");
        assert_eq!(validate_rows(&rows(json!([["a"], ["b"]]))).unwrap(), (2, 1));
    }

    // ---- read_document / extract_images ---------------------------------------------------

    #[tokio::test]
    async fn read_document_returns_colored_runs_only_when_asked() {
        let body = json!([{"paragraph": {"elements": [
            {"textRun": {"content": "hello\n", "textStyle": {"foregroundColor": {"color": {"rgbColor": {"blue": 1.0}}}}}}
        ]}}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body.clone())).on("docs_get", tab_doc(body));
        let with = ok(&api, "read_document", json!({"item": sid(), "include_colors": true})).await;
        assert_eq!(with["colored_runs"], json!([{"text": "hello", "color": "#0000ff"}]));
        let without = ok(&api, "read_document", json!({ "item": sid() })).await;
        assert_eq!(without.get("colored_runs"), None); // off by default
    }

    #[tokio::test]
    async fn every_tab_is_read_and_its_outline_entries_are_tagged_with_it() {
        let doc = json!({"revisionId": "rev-9", "title": "Doc", "tabs": [
            {"tabProperties": {"tabId": "t.1", "title": "First"},
             "documentTab": {"body": {"content": [styled(1, &["Alpha\n"], Some("HEADING_2"))]}}},
            {"tabProperties": {"tabId": "t.2", "title": "Second"},
             "documentTab": {"body": {"content": [para(1, &["plain\n"])]}}},
        ]});
        let api = FakeApi::new();
        api.on("docs_get", doc);
        let out = ok(&api, "read_document", json!({ "item": sid() })).await;
        assert_eq!(out["content"], "# First\n## Alpha\n# Second\nplain");
        assert_eq!(out["tab_read"], "all");
        assert_eq!(out["revision_id"], "rev-9");
        assert_eq!(
            out["outline"],
            json!([
                {"level": 1, "text": "First", "tab": "t.1"},
                {"level": 2, "text": "Alpha", "start": 1, "end": 7, "tab": "t.1"},
                {"level": 1, "text": "Second", "tab": "t.2"},
            ])
        );
        assert_eq!(out["tabs"], json!([{"id": "t.1", "title": "First"}, {"id": "t.2", "title": "Second"}]));
    }

    #[tokio::test]
    async fn a_single_requested_tab_is_read_without_a_title_prefix() {
        let doc = json!({"tabs": [
            {"tabProperties": {"tabId": "t.1", "title": "First"},
             "documentTab": {"body": {"content": [para(1, &["one\n"])]}}},
            {"tabProperties": {"tabId": "t.2", "title": "Second"},
             "documentTab": {"body": {"content": [para(1, &["two\n"])]}}},
        ]});
        let api = FakeApi::new();
        api.on("docs_get", doc);
        let out =
            ok(&api, "read_document", json!({"item": sid(), "tab": "t.2", "output_format": "text"})).await;
        assert_eq!(out["content"], "two");
        assert_eq!(out["tab_read"], "t.2");
    }

    #[tokio::test]
    async fn an_empty_tab_argument_still_honours_the_tab_in_the_item_url() {
        // `tab=""` must read as "not given" — otherwise it suppresses the URL's tab and the call
        // silently retargets the first tab, which for an edit is a write to the wrong place.
        let doc = json!({"tabs": [
            {"tabProperties": {"tabId": "t.1", "title": "First"},
             "documentTab": {"body": {"content": [para(1, &["one\n"])]}}},
            {"tabProperties": {"tabId": "t.2", "title": "Second"},
             "documentTab": {"body": {"content": [para(1, &["two\n"])]}}},
        ]});
        let url = "https://docs.google.com/document/d/DOC123456789012345678/edit?tab=t.2";
        let api = FakeApi::new();
        api.on("docs_get", doc);
        let out = ok(&api, "read_document", json!({"item": url, "tab": "", "output_format": "text"})).await;
        assert_eq!(out["tab_read"], "t.2");
        assert_eq!(out["content"], "two");
    }

    #[tokio::test]
    async fn reading_an_unknown_tab_lists_the_ones_that_exist() {
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(doc_body()));
        let e = fails(&api, "read_document", json!({"item": sid(), "tab": "t.9"})).await;
        assert_eq!(e, "tab 't.9' not found; available: [('t.0', 'T')]");
    }

    #[tokio::test]
    async fn a_non_google_image_host_is_never_fetched() {
        let doc = json!({"body": {"content": [
            {"paragraph": {"elements": [
                {"inlineObjectElement": {"inlineObjectId": "bad"}},
                {"inlineObjectElement": {"inlineObjectId": "good"}},
            ]}},
        ]}, "inlineObjects": {
            "bad": {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "https://evil.com/x.png"}}}},
            "good": {"inlineObjectProperties": {"embeddedObject": {"imageProperties": {"contentUri": "https://lh3.googleusercontent.com/ok"}}}},
        }});
        let api = FakeApi::new();
        api.on("docs_get", doc).with_image(vec![1, 2, 3], "png");
        let out = call(&api, "extract_images", json!({ "item": sid() })).await.unwrap();
        assert_eq!(out, ToolOutput::Images(vec![Image { data: vec![1, 2, 3], format: "png".into() }]));
        assert_eq!(api.calls_to("fetch_image"), vec![json!({"uri": "https://lh3.googleusercontent.com/ok"})]);
    }

    // ---- create_document -------------------------------------------------------------------

    #[tokio::test]
    async fn creating_a_document_without_text_writes_nothing() {
        let api = FakeApi::new();
        api.on("docs_create", json!({"documentId": "D1", "title": "T"}));
        let out = ok(&api, "create_document", json!({"title": "T"})).await;
        assert_eq!(out["id"], "D1");
        assert!(out["url"].as_str().unwrap().contains("docs.google.com/document/d/D1"));
        assert_eq!(api.call_count("docs_batch_update"), 0);
        assert_eq!(api.last("docs_create")["title"], "T");
    }

    #[tokio::test]
    async fn creating_a_document_with_markdown_styles_it_from_index_one() {
        let api = FakeApi::new();
        api.on("docs_create", json!({"documentId": "D1", "title": "T"}));
        let out =
            ok(&api, "create_document", json!({"title": "T", "text": "# H\n- a", "markdown": true})).await;
        let reqs = requests(&api);
        assert_eq!(reqs[0]["insertText"], json!({"location": {"index": 1}, "text": "H\na"}));
        assert_eq!(kinds(&reqs).last().unwrap(), "createParagraphBullets");
        assert_eq!(out["markdown"], json!({"headings": 1, "list_items": 1, "styled_spans": 0}));
    }

    // ---- append_text / insert_text -----------------------------------------------------------

    #[tokio::test]
    async fn append_text_dry_run_predicts_the_tail_and_writes_nothing() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "hello\n"}}]}, "endIndex": 7}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body));
        let out = ok(&api, "append_text", json!({"item": sid(), "text": " world", "dry_run": true})).await;
        assert_eq!(out["dry_run"], json!(true));
        assert_eq!(out["tab"], "t.0");
        assert_eq!(out["before_tail"], "hello");
        assert_eq!(out["after_tail"], "hello world");
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn block_markdown_appended_after_a_non_empty_tail_starts_a_new_paragraph() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "hello\n"}}]}, "endIndex": 7}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body));
        let out = ok(&api, "append_text", json!({"item": sid(), "text": "# Title", "markdown": true})).await;
        let reqs = requests(&api);
        assert_eq!(kinds(&reqs), ["insertText", "updateParagraphStyle", "deleteParagraphBullets"]);
        // tail paragraph is non-empty -> block content is pushed onto its own paragraph
        assert_eq!(
            reqs[0]["insertText"],
            json!({"location": {"index": 6, "tabId": "t.0"}, "text": "\nTitle"})
        );
        let heading = &reqs[1]["updateParagraphStyle"];
        assert_eq!(heading["range"], json!({"startIndex": 7, "endIndex": 12, "tabId": "t.0"}));
        assert_eq!(heading["paragraphStyle"], json!({"namedStyleType": "HEADING_1"}));
        assert_eq!(out["markdown"], json!({"headings": 1, "list_items": 0, "styled_spans": 0}));
    }

    #[tokio::test]
    async fn inline_only_markdown_merges_into_the_tail_paragraph() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "hello\n"}}]}, "endIndex": 7}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body));
        ok(&api, "append_text", json!({"item": sid(), "text": "**hi**", "markdown": true})).await;
        let reqs = requests(&api);
        assert_eq!(kinds(&reqs), ["insertText", "updateTextStyle"]); // no paragraph restyling
        assert_eq!(reqs[0]["insertText"]["text"], "hi"); // markers stripped, no forced newline
        let style = &reqs[1]["updateTextStyle"];
        assert_eq!(style["range"], json!({"startIndex": 6, "endIndex": 8, "tabId": "t.0"}));
        assert_eq!(style["textStyle"], json!({"bold": true}));
        assert_eq!(style["fields"], "bold");
    }

    #[tokio::test]
    async fn an_already_empty_tail_paragraph_needs_no_extra_newline() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "\n"}}]}, "endIndex": 2}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body));
        ok(&api, "append_text", json!({"item": sid(), "text": "# T", "markdown": true})).await;
        let reqs = requests(&api);
        assert_eq!(reqs[0]["insertText"]["text"], "T");
        assert_eq!(
            reqs[1]["updateParagraphStyle"]["range"],
            json!({"startIndex": 1, "endIndex": 2, "tabId": "t.0"})
        );
    }

    #[tokio::test]
    async fn a_markdown_dry_run_strips_the_markers_in_its_prediction() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "hello\n"}}]}, "endIndex": 7}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body));
        let out = ok(
            &api,
            "append_text",
            json!({"item": sid(), "text": "- a\n- b", "markdown": true, "dry_run": true}),
        )
        .await;
        assert_eq!(out["dry_run"], json!(true));
        assert_eq!(out["markdown"]["list_items"], json!(2));
        assert_eq!(out["after_tail"], "hello\na\nb");
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn text_is_colored_only_when_a_color_is_given() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body.clone())).on("docs_get", tab_doc(body));

        ok(&api, "append_text", json!({"item": sid(), "text": "X"})).await;
        assert_eq!(kinds(&requests(&api)), ["insertText"]);

        ok(&api, "append_text", json!({"item": sid(), "text": "X", "color": "#FF0000"})).await;
        let reqs = requests(&api);
        assert_eq!(kinds(&reqs), ["insertText", "updateTextStyle"]);
        let style = &reqs[1]["updateTextStyle"];
        assert_eq!(
            style["textStyle"]["foregroundColor"]["color"]["rgbColor"],
            json!({"red": 1.0, "green": 0.0, "blue": 0.0})
        );
        assert_eq!(style["range"]["tabId"], "t.0");
    }

    #[tokio::test]
    async fn a_bad_color_is_rejected_before_a_dry_run_reports_anything() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body));
        let e =
            fails(&api, "append_text", json!({"item": sid(), "text": "X", "color": "blue", "dry_run": true}))
                .await;
        assert!(e.starts_with("color must be a 6-digit hex string"), "{e}");
    }

    #[tokio::test]
    async fn insert_text_dry_run_reports_the_payload_without_writing() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body));
        let out =
            ok(&api, "insert_text", json!({"item": sid(), "text": "X", "index": 1, "dry_run": true})).await;
        assert_eq!(out["dry_run"], json!(true));
        assert_eq!(out["would_insert"], "X");
        assert_eq!(out["at_index"], json!(1));
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn insert_text_markdown_builds_its_bullets_at_the_given_index() {
        let body = json!([{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4}]);
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(body));
        ok(&api, "insert_text", json!({"item": sid(), "text": "- a\n- b", "index": 5, "markdown": true}))
            .await;
        let reqs = requests(&api);
        assert_eq!(reqs[0]["insertText"], json!({"location": {"index": 5, "tabId": "t.0"}, "text": "a\nb"}));
        let bullets = reqs.last().unwrap()["createParagraphBullets"].clone();
        assert_eq!(bullets["range"], json!({"startIndex": 5, "endIndex": 8, "tabId": "t.0"}));
        assert_eq!(bullets["bulletPreset"], "BULLET_DISC_CIRCLE_SQUARE");
    }

    #[tokio::test]
    async fn after_anchors_at_the_match_end_and_before_at_its_start() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true))
            .on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let after = ok(&api, "insert_text", json!({"item": sid(), "text": "!", "after": "alpha"})).await;
        assert_eq!(after["inserted_at"], json!(11));
        assert_eq!(requests(&api)[0]["insertText"]["location"]["index"], json!(11));
        let before = ok(&api, "insert_text", json!({"item": sid(), "text": "!", "before": "alpha"})).await;
        assert_eq!(before["inserted_at"], json!(6));
    }

    #[tokio::test]
    async fn block_markdown_snaps_past_the_paragraph_holding_the_match() {
        // 'alpha' sits mid-paragraph; a heading anchored there would restyle the host paragraph,
        // so block content snaps to the end of the containing paragraph instead.
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let out = ok(
            &api,
            "insert_text",
            json!({"item": sid(), "text": "# H", "after": "alpha", "markdown": true}),
        )
        .await;
        assert_eq!(out["inserted_at"], json!(17));
    }

    #[tokio::test]
    async fn an_insert_needs_exactly_one_anchor() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true))
            .on("docs_get", locator_doc(doc_body(), Some("rev-1"), true))
            .on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let both =
            fails(&api, "insert_text", json!({"item": sid(), "text": "x", "index": 1, "after": "alpha"}))
                .await;
        assert_eq!(both, "pass only one of index=, after=, before=; got index, after");
        let none = fails(&api, "insert_text", json!({"item": sid(), "text": "x"})).await;
        assert_eq!(none, "pass one of after=, before= (locate by text) or index= (a raw Docs offset)");
        let missed = fails(&api, "insert_text", json!({"item": sid(), "text": "x", "after": "absent"})).await;
        assert_eq!(
            missed,
            "no match for 'absent' in this tab (matching is case-sensitive and cannot span paragraphs)"
        );
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn a_block_anchor_inside_a_table_cell_is_refused_but_an_inline_one_is_not() {
        let body = json!([
            {"startIndex": 1, "endIndex": 13,
             "table": {"tableRows": [{"tableCells": [{"content": [para(4, &["in-cell"])]}]}]}},
            para(13, &["outside\n"]),
        ]);
        let api = FakeApi::new();
        for _ in 0..3 {
            api.on("docs_get", locator_doc(body.clone(), Some("rev-1"), true));
        }
        let table_err =
            fails(&api, "insert_table", json!({"item": sid(), "rows": [["x"]], "after": "in-cell"})).await;
        assert!(table_err.contains("is inside a table cell"), "{table_err}");
        let block_err = fails(
            &api,
            "insert_text",
            json!({"item": sid(), "text": "# H", "after": "in-cell", "markdown": true}),
        )
        .await;
        assert!(block_err.contains("is inside a table cell"), "{block_err}");
        // inline text has no boundary requirement, so it still anchors inside the cell
        let inline = ok(&api, "insert_text", json!({"item": sid(), "text": "!", "after": "in-cell"})).await;
        assert_eq!(inline["inserted_at"], json!(11));
    }

    // ---- delete_text / replace_text ----------------------------------------------------------

    #[tokio::test]
    async fn delete_text_previews_instead_of_deleting_without_confirm() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let out = ok(&api, "delete_text", json!({"item": sid(), "match": "alpha"})).await;
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"]["occurrences"], json!(1));
        assert_eq!(out["impact"]["deletes_text"], json!(["alpha"]));
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn delete_text_executes_once_confirmed() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let out = ok(&api, "delete_text", json!({"item": sid(), "match": "alpha", "confirm": true})).await;
        assert_eq!(out["deleted"], json!(true));
        assert_eq!(out["chars"], json!(5));
        assert_eq!(
            requests(&api),
            vec![json!({"deleteContentRange": {"range": {"startIndex": 6, "endIndex": 11, "tabId": "t.0"}}})]
        );
    }

    #[tokio::test]
    async fn a_delete_dry_run_neither_writes_nor_gates() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let out = ok(&api, "delete_text", json!({"item": sid(), "match": "alpha", "dry_run": true})).await;
        assert_eq!(out["dry_run"], json!(true));
        assert_eq!(out.get("status"), None); // dry-run explores; confirm gates
        assert_eq!(out["deletes_text"], json!(["alpha"]));
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn deleting_every_occurrence_emits_the_ranges_bottom_up() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        ok(&api, "delete_text", json!({"item": sid(), "match": "alpha", "occurrence": 0, "confirm": true}))
            .await;
        let starts: Vec<Value> = of_kind(&requests(&api), "deleteContentRange")
            .iter()
            .map(|r| r["range"]["startIndex"].clone())
            .collect();
        // bottom-up keeps the earlier ranges valid
        assert_eq!(starts, vec![json!(17), json!(6)]);
    }

    #[tokio::test]
    async fn a_locator_write_pins_the_revision_it_resolved_against() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        ok(&api, "delete_text", json!({"item": sid(), "match": "alpha", "confirm": true})).await;
        assert_eq!(body_arg(&api)["writeControl"], json!({"requiredRevisionId": "rev-1"}));
    }

    #[tokio::test]
    async fn a_doc_with_no_revision_id_gets_no_write_control_key() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), None, true));
        ok(&api, "delete_text", json!({"item": sid(), "match": "alpha", "confirm": true})).await;
        // sending requiredRevisionId=null would be rejected outright; omit the key instead
        assert_eq!(body_arg(&api).get("writeControl"), None);
    }

    #[tokio::test]
    async fn an_untabbed_doc_emits_no_tab_id() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), false));
        ok(&api, "delete_text", json!({"item": sid(), "match": "alpha", "confirm": true})).await;
        assert_eq!(
            requests(&api)[0]["deleteContentRange"]["range"],
            json!({"startIndex": 6, "endIndex": 11})
        );
    }

    #[tokio::test]
    async fn deleting_a_section_removes_the_heading_and_everything_under_it() {
        let body = json!([
            styled(1, &["Intro\n"], Some("HEADING_1")),
            para(7, &["body\n"]),
            styled(12, &["Next\n"], Some("HEADING_1")),
        ]);
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(body, Some("rev-1"), true));
        let out = ok(&api, "delete_text", json!({"item": sid(), "section": "Intro", "confirm": true})).await;
        // heading + its content, trailing newline included
        assert_eq!(out["deletes_text"], json!(["Intro\nbody\n"]));
        assert_eq!(
            requests(&api)[0]["deleteContentRange"]["range"],
            json!({"startIndex": 1, "endIndex": 12, "tabId": "t.0"})
        );
    }

    #[tokio::test]
    async fn a_final_section_stops_before_the_bodys_undeletable_newline() {
        let body = json!([styled(1, &["Only\n"], Some("HEADING_1")), para(6, &["tail\n"])]);
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(body.clone(), Some("rev-1"), true));
        ok(&api, "delete_text", json!({"item": sid(), "section": "Only", "confirm": true})).await;
        let last_end = body.as_array().unwrap().last().unwrap()["endIndex"].as_i64().unwrap();
        assert_eq!(requests(&api)[0]["deleteContentRange"]["range"]["endIndex"], json!(last_end - 1));
    }

    #[tokio::test]
    async fn delete_text_requires_exactly_one_locator() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let e = fails(&api, "delete_text", json!({"item": sid(), "confirm": true})).await;
        assert_eq!(e, "pass exactly one of match= or section=");
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn replace_text_gates_then_rewrites_in_a_single_batch() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true))
            .on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let gated =
            ok(&api, "replace_text", json!({"item": sid(), "replacement": "beta", "match": "alpha"})).await;
        assert_eq!(gated["status"], "confirmation_required");
        assert_eq!(gated["impact"]["replaces_text"], json!(["alpha"]));
        assert_eq!(gated["impact"]["with_text"], "beta");
        assert_eq!(api.call_count("docs_batch_update"), 0);

        ok(
            &api,
            "replace_text",
            json!({"item": sid(), "replacement": "beta", "match": "alpha", "confirm": true}),
        )
        .await;
        // never a delete-then-insert window
        assert_eq!(api.call_count("docs_batch_update"), 1);
        let reqs = requests(&api);
        assert_eq!(kinds(&reqs), ["deleteContentRange", "insertText"]);
        assert_eq!(reqs[1]["insertText"], json!({"location": {"index": 6, "tabId": "t.0"}, "text": "beta"}));
    }

    #[tokio::test]
    async fn a_replace_dry_run_neither_writes_nor_gates() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        let out = ok(
            &api,
            "replace_text",
            json!({"item": sid(), "replacement": "beta", "match": "alpha", "dry_run": true}),
        )
        .await;
        assert_eq!(out["dry_run"], json!(true));
        assert_eq!(out.get("status"), None); // dry-run explores; confirm gates
        assert_eq!(out["replaces_text"], json!(["alpha"]));
        assert_eq!(out["with_text"], "beta");
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn replacing_every_occurrence_pairs_each_delete_with_its_own_insert() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        ok(
            &api,
            "replace_text",
            json!({"item": sid(), "replacement": "beta", "match": "alpha",
                   "occurrence": 0, "confirm": true}),
        )
        .await;
        assert_eq!(api.call_count("docs_batch_update"), 1); // one batch, never a partial rewrite
        let reqs = requests(&api);
        // paired per range, not grouped into all-deletes-then-all-inserts
        assert_eq!(kinds(&reqs), ["deleteContentRange", "insertText", "deleteContentRange", "insertText"]);
        let deleted: Vec<Value> =
            of_kind(&reqs, "deleteContentRange").iter().map(|r| r["range"]["startIndex"].clone()).collect();
        let inserted: Vec<Value> =
            of_kind(&reqs, "insertText").iter().map(|r| r["location"]["index"].clone()).collect();
        assert_eq!(deleted, vec![json!(17), json!(6)]); // bottom-up keeps the earlier range valid
        assert_eq!(inserted, deleted); // each insert rewrites the range deleted just before it
    }

    #[tokio::test]
    async fn a_replace_pins_the_revision_it_resolved_against() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true))
            .on("docs_get", locator_doc(doc_body(), None, true));
        let call = json!({"item": sid(), "replacement": "beta", "match": "alpha", "confirm": true});
        ok(&api, "replace_text", call.clone()).await;
        assert_eq!(body_arg(&api)["writeControl"], json!({"requiredRevisionId": "rev-1"}));
        // sending requiredRevisionId=null would be rejected outright; omit the key instead
        ok(&api, "replace_text", call).await;
        assert_eq!(body_arg(&api).get("writeControl"), None);
    }

    #[tokio::test]
    async fn replacement_markdown_styles_land_on_the_new_text() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        ok(
            &api,
            "replace_text",
            json!({"item": sid(), "replacement": "**bold**", "match": "alpha",
                   "markdown": true, "confirm": true}),
        )
        .await;
        let reqs = requests(&api);
        assert_eq!(kinds(&reqs), ["deleteContentRange", "insertText", "updateTextStyle"]);
        assert_eq!(
            reqs[2]["updateTextStyle"]["range"],
            json!({"startIndex": 6, "endIndex": 10, "tabId": "t.0"})
        );
    }

    #[tokio::test]
    async fn replace_text_refuses_pipe_tables_before_it_even_reads_the_doc() {
        let api = FakeApi::new();
        let e = fails(
            &api,
            "replace_text",
            json!({"item": sid(), "replacement": "| a | b |\n| --- | --- |", "match": "alpha",
                   "markdown": true, "confirm": true}),
        )
        .await;
        assert!(e.contains("Use insert_table"), "{e}");
        assert_eq!(api.call_count("docs_batch_update"), 0);
        assert_eq!(api.call_count("docs_get"), 0);
    }

    #[tokio::test]
    async fn an_empty_replacement_only_deletes() {
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(doc_body(), Some("rev-1"), true));
        ok(
            &api,
            "replace_text",
            json!({"item": sid(), "replacement": "", "match": "alpha", "confirm": true}),
        )
        .await;
        // an empty insertText is rejected by the API
        assert_eq!(kinds(&requests(&api)), ["deleteContentRange"]);
    }

    // ---- insert_table (two-phase write) --------------------------------------------------

    fn table_doc_before() -> Value {
        tab_doc(json!([{"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 7}]))
    }

    #[tokio::test]
    async fn a_bad_grid_is_rejected_before_any_api_call() {
        let api = FakeApi::new();
        let e = fails(&api, "insert_table", json!({"item": sid(), "rows": []})).await;
        assert_eq!(e, "rows must be a non-empty list of row lists");
        assert_eq!(api.call_count("docs_get"), 0);
    }

    #[tokio::test]
    async fn insert_table_dry_run_previews_the_grid_without_writing() {
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(json!([{"paragraph": {"elements": []}, "endIndex": 2}])));
        let out = ok(
            &api,
            "insert_table",
            json!({"item": sid(), "rows": [["a", "b"], ["c", "d"]], "dry_run": true}),
        )
        .await;
        assert_eq!(out["dry_run"], json!(true));
        assert_eq!((out["rows"].clone(), out["cols"].clone()), (json!(2), json!(2)));
        assert_eq!(out["preview"], "| a | b |\n| --- | --- |\n| c | d |");
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn the_fill_batch_advances_by_a_running_offset_and_bolds_only_the_header() {
        let table_el = json!({"startIndex": 5, "endIndex": 30, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 8, "paragraph": {}}]},
                            {"content": [{"startIndex": 10, "paragraph": {}}]}]},
            {"tableCells": [{"content": [{"startIndex": 13, "paragraph": {}}]},
                            {"content": [{"startIndex": 15, "paragraph": {}}]}]},
        ]}});
        let after = tab_doc(json!([{"paragraph": {}, "endIndex": 7}, table_el]));
        let api = FakeApi::new();
        api.on("docs_get", table_doc_before()).on("docs_get", after);

        let out = ok(
            &api,
            "insert_table",
            json!({"item": sid(), "rows": [["A", "B"], ["C", "D"]], "header": true}),
        )
        .await;
        assert_eq!(out["cells_filled"], json!(4));
        assert_eq!(out["inserted_at"], json!(6));
        assert_eq!(out["header"], json!(true));

        assert_eq!(
            nth_requests(&api, 0),
            vec![json!({"insertTable": {"rows": 2, "columns": 2, "location": {"index": 6, "tabId": "t.0"}}})]
        );
        let fill = nth_requests(&api, 1);
        // forward running-offset: A@8, B@10+1, C@13+2, D@15+3
        assert_eq!(
            of_kind(&fill, "insertText"),
            vec![
                json!({"location": {"index": 8, "tabId": "t.0"}, "text": "A"}),
                json!({"location": {"index": 11, "tabId": "t.0"}, "text": "B"}),
                json!({"location": {"index": 15, "tabId": "t.0"}, "text": "C"}),
                json!({"location": {"index": 18, "tabId": "t.0"}, "text": "D"}),
            ]
        );
        let bolds: Vec<Value> =
            of_kind(&fill, "updateTextStyle").iter().map(|r| r["range"].clone()).collect();
        assert_eq!(
            bolds, // only row 0
            vec![
                json!({"startIndex": 8, "endIndex": 9, "tabId": "t.0"}),
                json!({"startIndex": 11, "endIndex": 12, "tabId": "t.0"}),
            ]
        );
    }

    #[tokio::test]
    async fn an_empty_cell_is_skipped_rather_than_written_as_empty_text() {
        let table_el = json!({"startIndex": 1, "endIndex": 20, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 3, "paragraph": {}}]},
                            {"content": [{"startIndex": 5, "paragraph": {}}]}]},
        ]}});
        let before = tab_doc(json!([{"paragraph": {}, "endIndex": 2}]));
        let after = tab_doc(json!([{"paragraph": {}, "endIndex": 2}, table_el]));
        let api = FakeApi::new();
        api.on("docs_get", before).on("docs_get", after);
        let out = ok(&api, "insert_table", json!({"item": sid(), "rows": [["", "y"]]})).await;
        assert_eq!(out["cells_filled"], json!(1));
        assert_eq!(
            of_kind(&nth_requests(&api, 1), "insertText"),
            vec![json!({"location": {"index": 5, "tabId": "t.0"}, "text": "y"})]
        );
    }

    #[tokio::test]
    async fn a_none_cell_is_skipped_and_the_rest_render_the_way_python_str_does() {
        let table_el = json!({"startIndex": 1, "endIndex": 24, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 3, "paragraph": {}}]},
                            {"content": [{"startIndex": 5, "paragraph": {}}]},
                            {"content": [{"startIndex": 7, "paragraph": {}}]},
                            {"content": [{"startIndex": 9, "paragraph": {}}]}]},
        ]}});
        let before = tab_doc(json!([{"paragraph": {}, "endIndex": 2}]));
        let after = tab_doc(json!([{"paragraph": {}, "endIndex": 2}, table_el]));
        let api = FakeApi::new();
        api.on("docs_get", before).on("docs_get", after);
        let out = ok(&api, "insert_table", json!({"item": sid(), "rows": [[null, true, "", "x"]]})).await;
        // None and "" render empty and are skipped (the API rejects an empty insertText), so the
        // running offset only advances for the cells actually written
        assert_eq!(out["cells_filled"], json!(2));
        assert_eq!(
            of_kind(&nth_requests(&api, 1), "insertText"),
            vec![
                json!({"location": {"index": 5, "tabId": "t.0"}, "text": "True"}), // Python's str(True)
                json!({"location": {"index": 13, "tabId": "t.0"}, "text": "x"}),   // 9 + offset 4
            ]
        );
    }

    #[tokio::test]
    async fn the_fill_targets_the_new_table_not_a_pre_existing_one() {
        // A pre-existing table (start 100) is present before AND after the insert; the fill must
        // target the NEW table (start 5), never the old one.
        let old = json!({"startIndex": 100, "endIndex": 130, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 101, "paragraph": {}}]}]}]}});
        let new = json!({"startIndex": 5, "endIndex": 12, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 6, "paragraph": {}}]},
                            {"content": [{"startIndex": 8, "paragraph": {}}]}]}]}});
        let before = tab_doc(json!([{"paragraph": {}, "endIndex": 7}, old.clone()]));
        let after = tab_doc(json!([{"paragraph": {}, "endIndex": 7}, new, old]));
        let api = FakeApi::new();
        api.on("docs_get", before).on("docs_get", after);

        ok(&api, "insert_table", json!({"item": sid(), "rows": [["x", "y"]], "index": 5})).await;
        let at: Vec<Value> = of_kind(&nth_requests(&api, 1), "insertText")
            .iter()
            .map(|r| r["location"]["index"].clone())
            .collect();
        // the new table's cells (6, 8+1), NOT the pre-existing table's 101
        assert_eq!(at, vec![json!(6), json!(9)]);
    }

    #[tokio::test]
    async fn the_fill_targets_the_new_table_even_when_it_sits_below_the_old_one() {
        // The discriminating case for the pre-insert snapshot: here "not in `pre`" and "the lowest
        // table start" disagree, because the NEW table (start 100) begins BELOW a pre-existing one
        // (start 5) that is still present. Selecting by index alone would fill the old table.
        let old = json!({"startIndex": 5, "endIndex": 12, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 6, "paragraph": {}}]}]}]}});
        let new = json!({"startIndex": 100, "endIndex": 107, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 101, "paragraph": {}}]},
                            {"content": [{"startIndex": 103, "paragraph": {}}]}]}]}});
        let tail = json!({"paragraph": {}, "endIndex": 100});
        let before = tab_doc(json!([{"paragraph": {}, "endIndex": 5}, old.clone(), tail.clone()]));
        let after = tab_doc(json!([{"paragraph": {}, "endIndex": 5}, old, tail, new]));
        let api = FakeApi::new();
        api.on("docs_get", before).on("docs_get", after);

        let out = ok(&api, "insert_table", json!({"item": sid(), "rows": [["x", "y"]]})).await;
        assert_eq!(out["cells_filled"], json!(2));
        let at: Vec<Value> = of_kind(&nth_requests(&api, 1), "insertText")
            .iter()
            .map(|r| r["location"]["index"].clone())
            .collect();
        // the new table's cells (101, 103+1), NOT the pre-existing table's 6
        assert_eq!(at, vec![json!(101), json!(104)]);
    }

    #[tokio::test]
    async fn the_fill_never_reaches_a_table_in_another_tab() {
        // t.0 holds a table the snapshot never saw (it snapshots the TARGET tab only) whose start
        // is lower than the new one's, so a doc-wide search would pick it and fill the wrong tab.
        let two_tabs = |first: Value, second: Value| {
            json!({"tabs": [
                {"tabProperties": {"tabId": "t.0", "title": "A"}, "documentTab": {"body": {"content": first}}},
                {"tabProperties": {"tabId": "t.1", "title": "B"}, "documentTab": {"body": {"content": second}}},
            ]})
        };
        let elsewhere = json!({"startIndex": 5, "endIndex": 12, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 6, "paragraph": {}}]}]}]}});
        let other_tab = json!([{"paragraph": {}, "endIndex": 5}, elsewhere]);
        let new = json!({"startIndex": 20, "endIndex": 27, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 21, "paragraph": {}}]},
                            {"content": [{"startIndex": 23, "paragraph": {}}]}]}]}});
        let target = json!([{"paragraph": {}, "endIndex": 20}]);
        let after = json!([{"paragraph": {}, "endIndex": 20}, new]);
        let api = FakeApi::new();
        api.on("docs_get", two_tabs(other_tab.clone(), target)).on("docs_get", two_tabs(other_tab, after));

        ok(&api, "insert_table", json!({"item": sid(), "rows": [["x", "y"]], "tab": "t.1"})).await;
        assert_eq!(
            nth_requests(&api, 0)[0]["insertTable"],
            json!({"rows": 1, "columns": 2, "location": {"index": 19, "tabId": "t.1"}})
        );
        assert_eq!(
            of_kind(&nth_requests(&api, 1), "insertText"),
            vec![
                json!({"location": {"index": 21, "tabId": "t.1"}, "text": "x"}),
                json!({"location": {"index": 24, "tabId": "t.1"}, "text": "y"}), // 23 + offset 1
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_fill_rolls_back_the_orphaned_empty_table() {
        let table_el = json!({"startIndex": 5, "endIndex": 12, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 6, "paragraph": {}}]}]}]}});
        let before = tab_doc(json!([{"paragraph": {}, "endIndex": 7}]));
        let after = tab_doc(json!([{"paragraph": {}, "endIndex": 7}, table_el]));
        let api = FakeApi::new();
        api.on("docs_get", before).on("docs_get", after);
        // insertTable ok, fill fails, rollback delete ok
        api.on("docs_batch_update", json!({}))
            .fail("docs_batch_update", ToolError::Api { status: 500, reason: "boom".into() })
            .on("docs_batch_update", json!({}));

        let e = fails(&api, "insert_table", json!({"item": sid(), "rows": [["x"]]})).await;
        assert_eq!(e, "Google API error 500: boom");
        assert_eq!(
            nth_requests(&api, 2),
            vec![json!({"deleteContentRange": {"range": {"startIndex": 5, "endIndex": 12, "tabId": "t.0"}}})]
        );
    }

    #[tokio::test]
    async fn a_400_from_a_raw_index_becomes_locator_advice() {
        let api = FakeApi::new();
        api.on("docs_get", table_doc_before())
            .fail("docs_batch_update", ToolError::Api { status: 400, reason: "Invalid requests[0]".into() });
        let e = fails(&api, "insert_table", json!({"item": sid(), "rows": [["x"]], "index": 999})).await;
        assert_eq!(
            e,
            "could not insert a table at index 999: index must be a Docs structural offset at a \
             paragraph boundary (not a character count from read_document). Use after='<text>' or \
             before='<text>' to locate the spot by content — those always resolve to a valid \
             boundary — or omit index to append."
        );
    }

    #[tokio::test]
    async fn a_400_without_a_raw_index_is_surfaced_as_the_api_reported_it() {
        let api = FakeApi::new();
        api.on("docs_get", table_doc_before())
            .fail("docs_batch_update", ToolError::Api { status: 400, reason: "Invalid requests[0]".into() });
        let e = fails(&api, "insert_table", json!({"item": sid(), "rows": [["x"]]})).await;
        assert_eq!(e, "Google API error 400: Invalid requests[0]");
    }

    #[tokio::test]
    async fn after_resolves_a_table_to_a_paragraph_boundary() {
        let body = doc_body();
        let api = FakeApi::new();
        api.on("docs_get", locator_doc(body.clone(), Some("rev-1"), true));
        let out = ok(
            &api,
            "insert_table",
            json!({"item": sid(), "rows": [["a"]], "after": "alpha", "dry_run": true}),
        )
        .await;
        // end of the paragraph holding the match -> a boundary insertTable accepts, so the
        // mid-paragraph 400 remap path is never reached
        assert_eq!(out["at_index"], json!(17));
        // and it really is one: an element start, or the body's own end
        let els = body.as_array().unwrap();
        let boundaries: Vec<i64> =
            els.iter().map(|el| index_of(el, "startIndex")).chain([locate::body_end(els)]).collect();
        assert!(boundaries.contains(&out["at_index"].as_i64().unwrap()), "{boundaries:?}");
    }

    // ---- the segmented markdown renderer -------------------------------------------------

    #[tokio::test]
    async fn a_heading_segments_paragraph_range_starts_at_the_anchor() {
        // the trailing-newline design keeps insertion index == style base (not anchor+1)
        let api = FakeApi::new();
        let segments = [Segment::Text("# H".into())];
        insert_markdown_segments(&api, "D", Some("t.0"), 10, &segments, false).await.unwrap();
        let reqs = requests(&api);
        assert_eq!(reqs[0]["insertText"], json!({"location": {"index": 10, "tabId": "t.0"}, "text": "H\n"}));
        assert_eq!(
            reqs[1]["updateParagraphStyle"]["range"],
            json!({"startIndex": 10, "endIndex": 11, "tabId": "t.0"})
        );
    }

    #[tokio::test]
    async fn a_leading_newline_moves_the_style_base_without_moving_the_insertion() {
        // lead_newline prepends '\n' to the first segment, so the styled text starts one further
        // on — but the insertion itself still happens at the anchor.
        let api = FakeApi::new();
        let segments = [Segment::Text("# H".into())];
        insert_markdown_segments(&api, "D", Some("t.0"), 10, &segments, true).await.unwrap();
        let reqs = requests(&api);
        assert_eq!(
            reqs[0]["insertText"],
            json!({"location": {"index": 10, "tabId": "t.0"}, "text": "\nH\n"})
        );
        assert_eq!(
            reqs[1]["updateParagraphStyle"]["range"],
            json!({"startIndex": 11, "endIndex": 12, "tabId": "t.0"})
        );
    }

    #[tokio::test]
    async fn interleaved_segments_are_written_last_first_at_one_fixed_anchor() {
        // Bottom-up at a fixed anchor is what removes cross-segment offset math: each segment is
        // inserted above the ones already placed, so no placed segment's indexes are re-used.
        let filler = json!({"paragraph": {}, "endIndex": 31});
        // the cell indexes come from the re-fetch, so they are not derivable from the anchor
        let new_table = json!({"startIndex": 32, "endIndex": 39, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 33, "paragraph": {}}]},
                            {"content": [{"startIndex": 35, "paragraph": {}}]}]}]}});
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(json!([filler.clone()]))) // pre-insert snapshot
            .on("docs_get", tab_doc(json!([filler, new_table]))); // post-insert re-fetch

        let segments = [
            Segment::Text("intro".into()),
            Segment::Table(vec![vec!["a".into(), "b".into()]]),
            Segment::Text("outtro".into()),
        ];
        insert_markdown_segments(&api, "D", Some("t.0"), 10, &segments, false).await.unwrap();

        assert_eq!(api.call_count("docs_batch_update"), 4);
        assert_eq!(
            nth_requests(&api, 0)[0]["insertText"], // the LAST segment goes first
            json!({"location": {"index": 10, "tabId": "t.0"}, "text": "outtro\n"})
        );
        assert_eq!(
            nth_requests(&api, 1)[0]["insertTable"], // same anchor, not shifted by the text below
            json!({"rows": 1, "columns": 2, "location": {"index": 10, "tabId": "t.0"}})
        );
        assert_eq!(
            of_kind(&nth_requests(&api, 2), "insertText"), // fills use the re-fetched indexes
            vec![
                json!({"location": {"index": 33, "tabId": "t.0"}, "text": "a"}),
                json!({"location": {"index": 36, "tabId": "t.0"}, "text": "b"}), // 35 + offset 1
            ]
        );
        assert_eq!(
            nth_requests(&api, 3)[0]["insertText"],
            json!({"location": {"index": 10, "tabId": "t.0"}, "text": "intro\n"})
        );
    }

    #[tokio::test]
    async fn a_whitespace_only_text_segment_is_skipped() {
        let api = FakeApi::new();
        let segments = [Segment::Text("   ".into())];
        insert_markdown_segments(&api, "D", None, 5, &segments, false).await.unwrap();
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    #[tokio::test]
    async fn appending_a_pipe_table_takes_the_segmented_write_path() {
        let para_el = json!({"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4});
        let new_table = json!({"startIndex": 3, "endIndex": 15, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 4, "paragraph": {}}]},
                            {"content": [{"startIndex": 6, "paragraph": {}}]}]}]}});
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(json!([para_el.clone()]))) // append: resolve tab + start
            .on("docs_get", tab_doc(json!([para_el.clone()]))) // pre-insert snapshot
            .on("docs_get", tab_doc(json!([para_el, new_table]))); // post-insert re-fetch

        let out = ok(
            &api,
            "append_text",
            json!({"item": sid(), "text": "| a | b |\n| --- | --- |", "markdown": true}),
        )
        .await;
        assert_eq!(out["tables"], json!(1));
        assert_eq!(out["tab"], "t.0");
        assert_eq!(out["inserted_at"], json!(3));
        assert_eq!(
            nth_requests(&api, 0)[0]["insertTable"],
            json!({"rows": 1, "columns": 2, "location": {"index": 3, "tabId": "t.0"}})
        );
        assert_eq!(
            of_kind(&nth_requests(&api, 1), "insertText"),
            vec![
                json!({"location": {"index": 4, "tabId": "t.0"}, "text": "a"}),
                json!({"location": {"index": 7, "tabId": "t.0"}, "text": "b"}), // b@6 + offset 1
            ]
        );
    }

    #[tokio::test]
    async fn insert_text_only_segments_when_a_delimiter_row_makes_it_a_table() {
        let para_el = json!({"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4});
        let new_table = json!({"startIndex": 6, "endIndex": 13, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 7, "paragraph": {}}]},
                            {"content": [{"startIndex": 9, "paragraph": {}}]}]}]}});
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(json!([para_el.clone()]))) // insert_text: resolve tab + anchor
            .on("docs_get", tab_doc(json!([para_el.clone()]))) // pre-insert snapshot
            .on("docs_get", tab_doc(json!([para_el.clone(), new_table]))) // post-insert re-fetch
            .on("docs_get", tab_doc(json!([para_el]))); // the table-free call below

        let table = ok(
            &api,
            "insert_text",
            json!({"item": sid(), "text": "| a | b |\n| --- | --- |", "index": 5, "markdown": true}),
        )
        .await;
        assert_eq!(table["tables"], json!(1));
        assert_eq!(
            nth_requests(&api, 0)[0]["insertTable"],
            json!({"rows": 1, "columns": 2, "location": {"index": 5, "tabId": "t.0"}})
        );

        // a lone pipe row (no delimiter row) is not a table: the single-blob path, written verbatim
        let blob = ok(
            &api,
            "insert_text",
            json!({"item": sid(), "text": "| a | b |", "index": 5, "markdown": true}),
        )
        .await;
        assert_eq!(blob.get("tables"), None);
        assert_eq!(blob["chars"], json!(9));
        assert_eq!(api.call_count("docs_batch_update"), 3); // one more batch, not an insert+fill
        assert_eq!(kinds(&requests(&api)), ["insertText"]);
        assert_eq!(
            requests(&api)[0]["insertText"],
            json!({"location": {"index": 5, "tabId": "t.0"}, "text": "| a | b |"})
        );
    }

    #[tokio::test]
    async fn create_document_only_segments_when_a_delimiter_row_makes_it_a_table() {
        let new_table = json!({"startIndex": 1, "endIndex": 8, "table": {"tableRows": [
            {"tableCells": [{"content": [{"startIndex": 2, "paragraph": {}}]},
                            {"content": [{"startIndex": 4, "paragraph": {}}]}]}]}});
        let empty = json!({"paragraph": {}, "endIndex": 2});
        let api = FakeApi::new();
        api.on("docs_create", json!({"documentId": "D1", "title": "T"}))
            .on("docs_get", json!({"body": {"content": [empty.clone()]}})) // pre-insert snapshot
            .on("docs_get", json!({"body": {"content": [empty, new_table]}})) // re-fetch
            .on("docs_create", json!({"documentId": "D2", "title": "T"}));

        let table = ok(
            &api,
            "create_document",
            json!({"title": "T", "text": "| a | b |\n| --- | --- |", "markdown": true}),
        )
        .await;
        assert_eq!(table["tables"], json!(1));
        assert_eq!(table.get("chars"), None); // the segmented path reports tables, not a char count
        assert_eq!(
            nth_requests(&api, 0)[0]["insertTable"],
            json!({"rows": 1, "columns": 2, "location": {"index": 1}}) // untabbed -> no tabId
        );
        assert_eq!(
            of_kind(&nth_requests(&api, 1), "insertText"),
            vec![
                json!({"location": {"index": 2}, "text": "a"}),
                json!({"location": {"index": 5}, "text": "b"}), // 4 + offset 1
            ]
        );

        // no delimiter row -> the single-blob path, and no re-fetch at all
        let blob =
            ok(&api, "create_document", json!({"title": "T", "text": "| a | b |", "markdown": true})).await;
        assert_eq!(blob.get("tables"), None);
        assert_eq!(blob["chars"], json!(9));
        assert_eq!(api.call_count("docs_get"), 2);
        assert_eq!(kinds(&requests(&api)), ["insertText"]);
        assert_eq!(requests(&api)[0]["insertText"], json!({"location": {"index": 1}, "text": "| a | b |"}));
    }

    #[tokio::test]
    async fn a_pipe_table_dry_run_previews_the_rows_without_writing() {
        let para_el = json!({"paragraph": {"elements": [{"textRun": {"content": "hi\n"}}]}, "endIndex": 4});
        let api = FakeApi::new();
        api.on("docs_get", tab_doc(json!([para_el])));
        let out = ok(
            &api,
            "append_text",
            json!({"item": sid(), "text": "| a | b |\n| --- | --- |\n| 1 | 2 |",
                   "markdown": true, "dry_run": true}),
        )
        .await;
        assert_eq!(out["dry_run"], json!(true));
        assert_eq!(out["tables"], json!(1));
        assert_eq!(out["preview"], "| a | b |\n| --- | --- |\n| 1 | 2 |");
        assert_eq!(api.call_count("docs_batch_update"), 0);
    }

    // ---- comments ---------------------------------------------------------------------------

    #[tokio::test]
    async fn read_comments_clamps_the_page_size_and_surfaces_the_next_token() {
        let api = FakeApi::new();
        api.on("drive_comments_list", json!({"comments": [{"id": "c1"}], "nextPageToken": "TOK"}));
        let out =
            ok(&api, "read_comments", json!({"item": sid(), "page_size": 5000, "page_token": "prev"})).await;
        assert_eq!(out["has_more"], json!(true));
        assert_eq!(out["next_page_token"], "TOK");
        assert_eq!(out["comments"], json!([{"id": "c1"}]));
        let call = api.last("drive_comments_list");
        assert_eq!(call["page_size"], json!(100));
        assert_eq!(call["page_token"], "prev");
    }

    #[tokio::test]
    async fn a_last_page_of_comments_reports_no_more() {
        let api = FakeApi::new();
        api.on("drive_comments_list", json!({}));
        let out = ok(&api, "read_comments", json!({"item": sid(), "page_size": 0})).await;
        assert_eq!(out["has_more"], json!(false));
        assert_eq!(out["next_page_token"], Value::Null);
        assert_eq!(out["comments"], json!([]));
        assert_eq!(api.last("drive_comments_list")["page_size"], json!(1));
    }

    #[tokio::test]
    async fn adding_a_comment_returns_the_new_comment_id() {
        let api = FakeApi::new();
        api.on("drive_comments_create", json!({"id": "cmt-1", "content": "hi"}));
        let out = ok(&api, "add_comment", json!({"item": sid(), "content": "hi"})).await;
        assert_eq!(out["comment_id"], "cmt-1");
        assert_eq!(api.last("drive_comments_create")["content"], "hi");
    }

    // ---- registration -----------------------------------------------------------------------

    #[test]
    fn the_tools_are_declared_in_the_python_modules_order() {
        let names: Vec<&str> = defs().iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            [
                "read_document",
                "extract_images",
                "create_document",
                "append_text",
                "insert_text",
                "insert_table",
                "delete_text",
                "replace_text",
                "read_comments",
                "add_comment",
            ]
        );
    }

    #[test]
    fn no_locator_tool_offers_case_insensitive_matching() {
        // Folding can change a string's length, which would shift every offset computed from the
        // folded text. Pinned here so the option is not added without handling that. All four
        // locator tools are covered: insert_text and insert_table take the same needles via
        // `after=`/`before=`, so a folding option on either would have the same consequence.
        for name in ["delete_text", "replace_text", "insert_text", "insert_table"] {
            let def = defs().into_iter().find(|d| d.name == name).unwrap();
            assert!(!def.params().contains(&"match_case"), "{name}");
        }
    }

    #[tokio::test]
    async fn a_name_this_module_does_not_own_is_left_to_the_next_module() {
        let api = FakeApi::new();
        let args = Args::new("read_sheet", None, &[]).unwrap();
        assert!(dispatch("read_sheet", &api, &args).await.is_none());
    }
}
