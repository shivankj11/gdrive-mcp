//! Google Slides tools: read, create, add/duplicate/move/delete slides, rewrite text, speaker notes.
//!
//! Every tool references its target presentation via `item` (a URL or ID). Content is addressed by
//! the stable `objectId`s that `read_presentation` returns, or by a slide's 1-based number — never
//! by a character offset.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use super::{merge, ToolDef, ToolOutput};
use crate::args::Args;
use crate::chunking::{paginate, DEFAULT_MAX_CHARS};
use crate::clients::GoogleApi;
use crate::error::{Result, ToolError};
use crate::guard::preview_response;
use crate::ids::parse_ref;

const READ_PRESENTATION_DOC: &str = r#"Read a Google Slides presentation (item = URL or ID) as text, one bounded chunk at a time.

Every slide is rendered with its `id`, layout, each page element (its placeholder/shape type,
`id` and text; tables cell by cell), speaker notes and comment threads (status, author,
assignee, replies; ones not tied to a slide appear last under "Unanchored comments"). Pass `slide` (a 1-based number such as
"3", or a slide id) to read just that slide. Returns the requested chunk (0-based) plus title,
`revision_id`, `slide_count`, an `outline` of every slide (number, id, title), the deck's
`layouts` (id and name, usable as add_slide's `layout`) and paging metadata (total_chunks,
total_chars, has_more). Set max_chars<=0 to get everything.

Use the element `id`s with set_shape_text, and slide numbers or ids with the other slide tools.
Slide numbers shift when slides are added, moved or deleted; ids do not."#;

const CREATE_PRESENTATION_DOC: &str = r#"Create a new Google Slides presentation (in My Drive root) with the given title.

Slides starts it with one slide; its id is returned as `first_slide_id`. Add more with
add_slide. (For documents use create_document, for spreadsheets create_spreadsheet.)"#;

const ADD_SLIDE_DOC: &str = r#"Add a slide to a Google Slides presentation (item = URL or ID), optionally filling its text.

`layout` is a predefined layout (BLANK, CAPTION_ONLY, TITLE, TITLE_AND_BODY,
TITLE_AND_TWO_COLUMNS, TITLE_ONLY, SECTION_HEADER, SECTION_TITLE_AND_DESCRIPTION,
ONE_COLUMN_TEXT, MAIN_POINT, BIG_NUMBER) or the id or name of one of the deck's own layouts
(listed by read_presentation). `title` goes into the slide's title placeholder and `body` into
its body (or subtitle) placeholder; a layout without that placeholder is reported under
`unfilled` rather than failing after the slide was created. `position` is the 1-based number
the new slide takes (default: appended at the end). dry_run=true reports the resolved layout
and position without writing."#;

const DUPLICATE_SLIDE_DOC: &str = r#"Duplicate a slide in a Google Slides presentation (item = URL or ID).

`slide` is a 1-based number (e.g. "3") or a slide id from read_presentation. The copy is placed
immediately after the original; its id is returned as `new_slide_id`. dry_run=true reports
which slide would be copied without writing."#;

const MOVE_SLIDE_DOC: &str = r#"Move a slide to a new position in a Google Slides presentation (item = URL or ID).

`slide` is a 1-based number (e.g. "3") or a slide id from read_presentation; `position` is the
1-based number the slide has once moved. Nothing is lost, so this is not gated. dry_run=true
reports the move without writing."#;

const DELETE_SLIDE_DOC: &str = r#"Delete a slide from a Google Slides presentation (item = URL or ID).

`slide` is a 1-based number (e.g. "3") or a slide id from read_presentation. dry_run=true
reports exactly what would be removed without writing. This tool is destructive: without
confirm=true it returns a preview of the slide (its title, element count and text) and
deletes nothing."#;

const REPLACE_SLIDE_TEXT_DOC: &str = r#"Replace text on the slides of a Google Slides presentation (item = URL or ID).

`match` is a literal, case-sensitive substring within a single paragraph; every occurrence in
slide shapes and table cells is replaced. Pass `slide` (a 1-based number such as "3", or a
slide id) to limit it to one slide, else every slide is searched. Layouts, masters and speaker
notes are never touched (use set_speaker_notes for those). To rewrite a whole shape rather
than a phrase, use set_shape_text.

dry_run=true reports the occurrences per slide without writing. This tool is destructive:
without confirm=true it returns that preview and writes nothing. An unmatched `match` raises
rather than silently doing nothing."#;

const SET_SHAPE_TEXT_DOC: &str = r#"Set the entire text of one shape, placeholder or table cell on a slide (item = URL or ID).

`object_id` is an element id from read_presentation (a title, body, text box or table). For a
table pass its 0-based `row` and `column` as well; for anything else leave them out. The
element's existing text is replaced by `text` (pass "" to clear it); a "\n" in `text` starts a
new paragraph. To change just a phrase use replace_slide_text; for speaker notes use
set_speaker_notes.

dry_run=true reports the before and after without writing. Replacing existing text is
destructive: without confirm=true it returns a preview of the text it would overwrite and
writes nothing. Setting text on an empty shape needs no confirmation."#;

const SET_SPEAKER_NOTES_DOC: &str = r#"Set a slide's speaker notes in a Google Slides presentation (item = URL or ID).

`slide` is a 1-based number (e.g. "3") or a slide id from read_presentation. The existing notes
are replaced by `text` (pass "" to clear them); a "\n" in `text` starts a new paragraph.

dry_run=true reports the before and after without writing. Replacing existing notes is
destructive: without confirm=true it returns a preview of the notes it would overwrite and
writes nothing. Setting notes on a slide that has none needs no confirmation."#;

const ADD_SLIDE_COMMENT_DOC: &str = r#"Add a comment to a slide of a Google Slides presentation (item = URL or ID), optionally assigned.

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
writing. Read comments back with read_presentation."#;

const PREVIEW_CHARS: usize = 200;

/// Slides rejects an assignee that is not a well-formed address, and cannot resolve a name to one.
static EMAIL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[^@\s,;<>]+@[^@\s,;<>]+\.[^@\s,;<>]+$").ok());
/// insertComment's documented limit, in UTF-8 code units.
const COMMENT_MAX_BYTES: usize = 2048;

/// createSlide's predefinedLayout values, minus PREDEFINED_LAYOUT_UNSPECIFIED.
const PREDEFINED_LAYOUTS: &[&str] = &[
    "BLANK",
    "CAPTION_ONLY",
    "TITLE",
    "TITLE_AND_BODY",
    "TITLE_AND_TWO_COLUMNS",
    "TITLE_ONLY",
    "SECTION_HEADER",
    "SECTION_TITLE_AND_DESCRIPTION",
    "ONE_COLUMN_TEXT",
    "MAIN_POINT",
    "BIG_NUMBER",
];
const TITLE_TYPES: &[&str] = &["TITLE", "CENTERED_TITLE"];
const BODY_TYPES: &[&str] = &["BODY", "SUBTITLE"];

fn clip(s: &str) -> String {
    if s.chars().count() <= PREVIEW_CHARS {
        s.to_string()
    } else {
        s.chars().take(PREVIEW_CHARS).chain(['…']).collect()
    }
}

/// Python's `{s!r}`: single-quoted unless the text holds a `'` and no `"`.
fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn array(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn str_of(v: Option<&Value>) -> &str {
    v.and_then(Value::as_str).unwrap_or("")
}

/// A JSON object's fields, for splatting one result into another (Python's `{**impact}`).
fn fields(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// Plain text of a shape's or table cell's `text`, minus the implicit trailing newline.
fn text_of(text: Option<&Value>) -> String {
    let runs: String = array(text.and_then(|t| t.get("textElements")))
        .iter()
        .filter_map(|te| te.get("textRun"))
        .map(|run| str_of(run.get("content")))
        .collect();
    runs.strip_suffix('\n').map_or_else(|| runs.clone(), str::to_string)
}

/// Every page element, descending into groups (parents before their children).
fn walk(elements: &[Value]) -> Vec<&Value> {
    let mut out = Vec::new();
    for el in elements {
        out.push(el);
        out.extend(walk(array(el.get("elementGroup").and_then(|g| g.get("children")))));
    }
    out
}

fn placeholder_type(el: &Value) -> Option<&str> {
    el.get("shape")?.get("placeholder")?.get("type")?.as_str()
}

fn shape_text(el: &Value) -> String {
    text_of(el.get("shape").and_then(|s| s.get("text")))
}

/// (row, column, text) for every cell of a table element.
fn table_cells(el: &Value) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    for (r, row) in array(el.get("table").and_then(|t| t.get("tableRows"))).iter().enumerate() {
        for (c, cell) in array(row.get("tableCells")).iter().enumerate() {
            out.push((r, c, text_of(cell.get("text"))));
        }
    }
    out
}

fn slide_title(slide: &Value) -> String {
    walk(array(slide.get("pageElements")))
        .into_iter()
        .find(|el| placeholder_type(el).is_some_and(|t| TITLE_TYPES.contains(&t)))
        .map(shape_text)
        .unwrap_or_default()
}

/// (speaker-notes shape id, speaker-notes text) for a slide.
fn notes(slide: &Value) -> (Option<String>, String) {
    let page = slide.get("slideProperties").and_then(|p| p.get("notesPage"));
    let oid = page
        .and_then(|p| p.get("notesProperties"))
        .and_then(|p| p.get("speakerNotesObjectId"))
        .and_then(Value::as_str);
    if let Some(oid) = oid {
        for el in walk(array(page.and_then(|p| p.get("pageElements")))) {
            if el.get("objectId").and_then(Value::as_str) == Some(oid) {
                return (Some(oid.to_string()), shape_text(el));
            }
        }
    }
    // The shape is only materialised once text is inserted into it.
    (oid.map(str::to_string), String::new())
}

/// All of a slide's visible text (shapes and table cells), for previews.
fn slide_text(slide: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for el in walk(array(slide.get("pageElements"))) {
        if el.get("table").is_some() {
            parts.extend(table_cells(el).into_iter().map(|(_, _, t)| t).filter(|t| !t.is_empty()));
        } else {
            let t = shape_text(el);
            if !t.is_empty() {
                parts.push(t);
            }
        }
    }
    parts.join("\n")
}

fn render_element(el: &Value) -> String {
    let oid = str_of(el.get("objectId"));
    if let Some(table) = el.get("table") {
        let rows = array(table.get("tableRows"));
        let n_rows = table.get("rows").and_then(Value::as_i64).unwrap_or(rows.len() as i64);
        let n_cols = table.get("columns").and_then(Value::as_i64).unwrap_or(0);
        let mut lines = vec![format!("- [TABLE] id={oid} {n_rows}x{n_cols}")];
        for (r, row) in rows.iter().enumerate() {
            let cells: Vec<String> = array(row.get("tableCells"))
                .iter()
                .map(|c| text_of(c.get("text")).replace('\n', " "))
                .collect();
            lines.push(format!("  r{r}: {}", cells.join(" | ")));
        }
        return lines.join("\n");
    }
    if let Some(shape) = el.get("shape") {
        let label = placeholder_type(el)
            .or_else(|| shape.get("shapeType").and_then(Value::as_str))
            .unwrap_or("SHAPE");
        let text = shape_text(el).replace('\n', "\n    ");
        let text = if text.is_empty() { "(empty)" } else { text.as_str() };
        return format!("- [{label}] id={oid}: {text}");
    }
    if let Some(group) = el.get("elementGroup") {
        return format!("- [GROUP] id={oid} ({} children)", array(group.get("children")).len());
    }
    let kind = ["image", "video", "line", "sheetsChart", "wordArt"]
        .into_iter()
        .find(|k| el.get(*k).is_some())
        .map_or_else(|| "ELEMENT".to_string(), str::to_uppercase);
    format!("- [{kind}] id={oid}")
}

fn render_thread(thread: &Value) -> String {
    let head = thread.get("headPost");
    let named = |post: Option<&Value>| {
        post.and_then(|p| p.get("author"))
            .and_then(|a| a.get("displayName"))
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .unwrap_or("unknown")
            .to_string()
    };
    let status = thread.get("status").and_then(Value::as_str).unwrap_or("OPEN");
    let mut line = format!("- [{status}] id={} {}", str_of(thread.get("commentId")), named(head));
    if let Some(assignee) =
        head.and_then(|h| h.get("assigneeEmail")).and_then(Value::as_str).filter(|a| !a.is_empty())
    {
        line.push_str(&format!(" -> assigned to {assignee}"));
    }
    line.push_str(&format!(": {}", str_of(head.and_then(|h| h.get("content")))));
    for reply in array(thread.get("replies")) {
        if reply.get("deleted").and_then(Value::as_bool) != Some(true) {
            line.push_str(&format!(
                "\n    reply from {}: {}",
                named(Some(reply)),
                str_of(reply.get("content"))
            ));
        }
    }
    line
}

fn live_threads(pres: &Value) -> Vec<&Value> {
    array(pres.get("comments"))
        .iter()
        .filter(|t| t.get("headPost").and_then(|h| h.get("deleted")).and_then(Value::as_bool) != Some(true))
        .collect()
}

/// Threads anchored to this slide or to an element on it (matched by Slides' own anchor ids).
fn slide_threads<'a>(slide: &Value, threads: &[&'a Value]) -> Vec<&'a Value> {
    let anchors: HashSet<&str> = array(slide.get("commentAnchors"))
        .iter()
        .filter_map(|a| a.get("anchorId").and_then(Value::as_str))
        .collect();
    threads
        .iter()
        .copied()
        .filter(|t| t.get("anchorId").and_then(Value::as_str).is_some_and(|id| anchors.contains(id)))
        .collect()
}

fn render_slide(
    number: usize,
    slide: &Value,
    layout_names: &HashMap<String, String>,
    threads: &[&Value],
) -> String {
    let layout = slide
        .get("slideProperties")
        .and_then(|p| p.get("layoutObjectId"))
        .and_then(Value::as_str)
        .and_then(|id| layout_names.get(id))
        .filter(|n| !n.is_empty());
    let mut head = format!("## Slide {number} · id={}", str_of(slide.get("objectId")));
    if let Some(layout) = layout {
        head.push_str(&format!(" · layout={layout}"));
    }
    let mut lines = vec![head];
    lines.extend(array(slide.get("pageElements")).iter().map(render_element));
    let (_, notes) = notes(slide);
    if !notes.is_empty() {
        lines.push(format!("Notes: {}", notes.replace('\n', "\n    ")));
    }
    let mine = slide_threads(slide, threads);
    if !mine.is_empty() {
        lines.push("Comments:".to_string());
        lines.extend(mine.iter().map(|t| format!("  {}", render_thread(t).replace('\n', "\n  "))));
    }
    lines.join("\n")
}

async fn get(api: &dyn GoogleApi, item: &str, comments: bool) -> Result<(String, Value)> {
    let pid = parse_ref(item)?.id;
    let pres = api.slides_get(&pid, comments).await?;
    Ok((pid, pres))
}

fn slides_of(pres: &Value) -> &[Value] {
    array(pres.get("slides"))
}

/// Resolve a slide by objectId, else by 1-based number. Returns (0-based index, slide).
fn pick_slide<'a>(slide_list: &'a [Value], reference: &str) -> Result<(usize, &'a Value)> {
    let reference = reference.trim();
    if let Some(i) =
        slide_list.iter().position(|s| s.get("objectId").and_then(Value::as_str) == Some(reference))
    {
        return Ok((i, &slide_list[i]));
    }
    if !reference.is_empty() && reference.chars().all(|c| c.is_ascii_digit()) {
        let n = reference.parse::<usize>().unwrap_or(usize::MAX);
        if (1..=slide_list.len()).contains(&n) {
            return Ok((n - 1, &slide_list[n - 1]));
        }
        let shown = reference.trim_start_matches('0');
        return Err(ToolError::msg(format!(
            "slide {} is out of range; this presentation has {} slide(s)",
            if shown.is_empty() { "0" } else { shown },
            slide_list.len()
        )));
    }
    Err(ToolError::msg(format!(
        "no slide with id {} (pass a 1-based slide number or an id from read_presentation)",
        repr(reference)
    )))
}

/// Pin the write to the revision it was resolved against, so a concurrent edit rejects it.
fn write_control(pres: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(rev) = pres.get("revisionId").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        out.insert("writeControl".into(), json!({"requiredRevisionId": rev}));
    }
    out
}

async fn batch(api: &dyn GoogleApi, pid: &str, requests: Vec<Value>, pres: Option<&Value>) -> Result<Value> {
    let mut body = json!({ "requests": requests });
    if let Some(pres) = pres {
        merge(&mut body, write_control(pres));
    }
    api.slides_batch_update(pid, &body).await
}

/// Replace everything in a shape/cell. deleteText is skipped on empty text, which Slides rejects.
fn text_requests(object_id: &str, existing: &str, text: &str, cell: Option<&Value>) -> Vec<Value> {
    let mut where_ = json!({"objectId": object_id});
    if let Some(cell) = cell {
        merge(&mut where_, Map::from_iter([("cellLocation".to_string(), cell.clone())]));
    }
    let mut requests = Vec::new();
    if !existing.is_empty() {
        let mut delete = where_.clone();
        merge(&mut delete, Map::from_iter([("textRange".to_string(), json!({"type": "ALL"}))]));
        requests.push(json!({ "deleteText": delete }));
    }
    if !text.is_empty() {
        let mut insert = where_;
        merge(
            &mut insert,
            Map::from_iter([("insertionIndex".to_string(), json!(0)), ("text".to_string(), json!(text))]),
        );
        requests.push(json!({ "insertText": insert }));
    }
    requests
}

async fn read_presentation(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let slide = args.opt_str("slide")?;
    let chunk = args.i64_or("chunk", 0)?;
    let max_chars = args.i64_or("max_chars", DEFAULT_MAX_CHARS)?;

    let (pid, pres) = get(api, &item, true).await?;
    let slide_list = slides_of(&pres);
    let layouts: Vec<Value> = array(pres.get("layouts"))
        .iter()
        .map(|lay| {
            json!({
                "object_id": lay.get("objectId"),
                "name": lay.get("layoutProperties").and_then(|p| p.get("displayName")),
            })
        })
        .collect();
    let layout_names: HashMap<String, String> = array(pres.get("layouts"))
        .iter()
        .filter_map(|lay| {
            let id = lay.get("objectId")?.as_str()?.to_string();
            let name = lay.get("layoutProperties")?.get("displayName")?.as_str()?.to_string();
            Some((id, name))
        })
        .collect();
    let selected: Vec<(usize, &Value)> = match &slide {
        Some(reference) => {
            let (i, picked) = pick_slide(slide_list, reference)?;
            vec![(i + 1, picked)]
        }
        None => slide_list.iter().enumerate().map(|(i, s)| (i + 1, s)).collect(),
    };
    let threads = live_threads(&pres);
    let mut content = selected
        .iter()
        .map(|(n, s)| render_slide(*n, s, &layout_names, &threads))
        .collect::<Vec<_>>()
        .join("\n\n");
    if slide.is_none() {
        // Comments made through the Drive API are not tied to a slide; keep them visible anyway.
        let placed: HashSet<&str> = selected
            .iter()
            .flat_map(|(_, s)| slide_threads(s, &threads))
            .filter_map(|t| t.get("commentId").and_then(Value::as_str))
            .collect();
        let loose: Vec<String> = threads
            .iter()
            .filter(|t| !t.get("commentId").and_then(Value::as_str).is_some_and(|id| placed.contains(id)))
            .map(|t| render_thread(t))
            .collect();
        if !loose.is_empty() {
            content.push_str(&format!("\n\n## Unanchored comments\n{}", loose.join("\n")));
        }
    }
    let outline: Vec<Value> = slide_list
        .iter()
        .enumerate()
        .map(|(i, s)| json!({"number": i + 1, "object_id": s.get("objectId"), "title": slide_title(s)}))
        .collect();
    let mut result = json!({
        "presentation_id": pid,
        "title": pres.get("title"),
        // Pass-through for callers that want to reason about staleness; write tools pin it themselves.
        "revision_id": pres.get("revisionId"),
        "slide_count": slide_list.len(),
        "outline": outline,
        "layouts": layouts,
    });
    merge(&mut result, paginate(&content, chunk, max_chars));
    Ok(result.into())
}

async fn create_presentation(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let title = args.req_str("title")?;
    let pres = api.slides_create(&title).await?;
    let pid = pres
        .get("presentationId")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::msg("unexpected presentations.create response: no presentationId"))?;
    let first = slides_of(&pres).first().and_then(|s| s.get("objectId")).cloned();
    Ok(json!({
        "id": pid,
        "url": format!("https://docs.google.com/presentation/d/{pid}/edit"),
        "title": pres.get("title"),
        "first_slide_id": first,
    })
    .into())
}

/// A LayoutReference for a predefined layout name, or for a layout of this deck by id/name.
fn resolve_layout(pres: &Value, layout: &str) -> Result<Value> {
    let upper = layout.to_uppercase();
    if PREDEFINED_LAYOUTS.contains(&upper.as_str()) {
        return Ok(json!({"predefinedLayout": upper}));
    }
    let wanted = layout.trim().to_lowercase();
    for lay in array(pres.get("layouts")) {
        let name = lay
            .get("layoutProperties")
            .and_then(|p| p.get("displayName"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = str_of(lay.get("objectId"));
        if id == layout.trim() || name.to_lowercase() == wanted {
            return Ok(json!({"layoutId": id}));
        }
    }
    Err(ToolError::msg(format!(
        "unknown layout {}: use one of {} or a layout id/name from read_presentation's `layouts`",
        repr(layout),
        PREDEFINED_LAYOUTS.join(", ")
    )))
}

async fn add_slide(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let layout = args.str_or("layout", "TITLE_AND_BODY")?;
    let title = args.opt_str("title")?;
    let body = args.opt_str("body")?;
    let position = args.opt_i64("position")?;
    let dry_run = args.bool_or("dry_run", false)?;

    if position.is_some_and(|p| p < 1) {
        return Err(ToolError::msg("position is a 1-based slide number, so it must be >= 1"));
    }
    let pid = parse_ref(&item)?.id;
    let needs_deck = !PREDEFINED_LAYOUTS.contains(&layout.to_uppercase().as_str());
    let pres = if needs_deck { api.slides_get(&pid, false).await? } else { json!({}) };
    let reference = resolve_layout(&pres, &layout)?;
    let mut create = json!({"slideLayoutReference": reference});
    if let Some(p) = position {
        merge(&mut create, Map::from_iter([("insertionIndex".to_string(), json!(p - 1))]));
    }
    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "action": "add_slide",
            "layout": reference,
            "position": position,
            "title": title,
            "body": body,
        })
        .into());
    }

    let reply = batch(api, &pid, vec![json!({"createSlide": create})], None).await?;
    let slide_id = reply
        .pointer("/replies/0/createSlide/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::msg("unexpected createSlide response: no objectId"))?
        .to_string();
    let mut result = json!({"presentation_id": pid, "slide_id": slide_id, "layout": reference});
    if let Some(p) = position {
        result["position"] = json!(p);
    }
    if title.is_none() && body.is_none() {
        return Ok(result.into());
    }

    let page = api.slides_page_get(&pid, &slide_id).await?;
    let placeholders: Vec<&Value> =
        walk(array(page.get("pageElements"))).into_iter().filter(|el| el.get("shape").is_some()).collect();
    let wanted: [(&str, &Option<String>, &[&str]); 2] =
        [("title", &title, TITLE_TYPES), ("body", &body, BODY_TYPES)];
    let mut requests: Vec<Value> = Vec::new();
    let mut filled: Vec<&str> = Vec::new();
    let mut unfilled: Vec<&str> = Vec::new();
    for (name, text, types) in wanted {
        let Some(text) = text else { continue };
        let target = placeholders.iter().find(|el| placeholder_type(el).is_some_and(|t| types.contains(&t)));
        match target {
            None => unfilled.push(name),
            Some(el) => {
                requests.extend(text_requests(str_of(el.get("objectId")), "", text, None));
                filled.push(name);
            }
        }
    }
    if !requests.is_empty() {
        batch(api, &pid, requests, None).await?;
    }
    result["filled"] = json!(filled);
    if !unfilled.is_empty() {
        result["unfilled"] = json!(unfilled);
    }
    Ok(result.into())
}

async fn duplicate_slide(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let slide = args.req_str("slide")?;
    let dry_run = args.bool_or("dry_run", false)?;

    let (pid, pres) = get(api, &item, false).await?;
    let (index, picked) = pick_slide(slides_of(&pres), &slide)?;
    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "action": "duplicate_slide",
            "slide_number": index + 1,
            "slide_id": picked.get("objectId"),
            "title": slide_title(picked),
        })
        .into());
    }
    let reply =
        batch(api, &pid, vec![json!({"duplicateObject": {"objectId": picked.get("objectId")}})], None)
            .await?;
    let new_id = reply
        .pointer("/replies/0/duplicateObject/objectId")
        .cloned()
        .ok_or_else(|| ToolError::msg("unexpected duplicateObject response: no objectId"))?;
    Ok(json!({
        "presentation_id": pid,
        "duplicated": picked.get("objectId"),
        "new_slide_id": new_id,
        "new_slide_number": index + 2,
    })
    .into())
}

async fn move_slide(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let slide = args.req_str("slide")?;
    let position = args.req_i64("position")?;
    let dry_run = args.bool_or("dry_run", false)?;

    let (pid, pres) = get(api, &item, false).await?;
    let slide_list = slides_of(&pres);
    let (current, picked) = pick_slide(slide_list, &slide)?;
    if !(1..=slide_list.len() as i64).contains(&position) {
        return Err(ToolError::msg(format!(
            "position {position} is out of range; this presentation has {} slide(s)",
            slide_list.len()
        )));
    }
    let target = (position - 1) as usize;
    let moved = json!({"slide_id": picked.get("objectId"), "from": current + 1, "to": position});
    if target == current {
        let mut out = if dry_run { json!({"dry_run": true}) } else { json!({}) };
        merge(&mut out, fields(&moved));
        out["moved"] = json!(false);
        return Ok(out.into());
    }
    if dry_run {
        let mut out = json!({"dry_run": true, "action": "move_slide"});
        merge(&mut out, fields(&moved));
        return Ok(out.into());
    }
    // updateSlidesPosition's index is in the arrangement *before* the move, so a slide moving later
    // must be inserted before the slide that ends up after it — one past its final 0-based index.
    let insertion = if target > current { target + 1 } else { target };
    let request = json!({"updateSlidesPosition": {
        "slideObjectIds": [picked.get("objectId")],
        "insertionIndex": insertion,
    }});
    batch(api, &pid, vec![request], Some(&pres)).await?;
    let mut out = json!({"presentation_id": pid});
    merge(&mut out, fields(&moved));
    out["moved"] = json!(true);
    Ok(out.into())
}

async fn delete_slide(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let slide = args.req_str("slide")?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let (pid, pres) = get(api, &item, false).await?;
    let (index, picked) = pick_slide(slides_of(&pres), &slide)?;
    let impact = json!({
        "slide_number": index + 1,
        "slide_id": picked.get("objectId"),
        "title": slide_title(picked),
        "elements": walk(array(picked.get("pageElements"))).len(),
        "deletes_text": clip(&slide_text(picked)),
    });
    if dry_run {
        let mut out = json!({"dry_run": true, "action": "delete_slide"});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if !confirm {
        return Ok(preview_response("delete_slide", impact).into());
    }
    batch(api, &pid, vec![json!({"deleteObject": {"objectId": picked.get("objectId")}})], Some(&pres))
        .await?;
    let mut out = json!({"presentation_id": pid, "deleted": true});
    merge(&mut out, fields(&impact));
    Ok(out.into())
}

async fn replace_slide_text(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let match_text = args.req_str("match")?;
    let replacement = args.req_str("replacement")?;
    let slide = args.opt_str("slide")?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    if match_text.is_empty() {
        return Err(ToolError::msg("match must be a non-empty string"));
    }
    if match_text.contains('\n') {
        return Err(ToolError::msg("match cannot span paragraphs; replace one paragraph's text at a time"));
    }
    let (pid, pres) = get(api, &item, false).await?;
    let slide_list = slides_of(&pres);
    let scanned: Vec<(usize, &Value)> = match slide {
        Some(reference) => {
            let (i, picked) = pick_slide(slide_list, &reference)?;
            vec![(i + 1, picked)]
        }
        None => slide_list.iter().enumerate().map(|(i, s)| (i + 1, s)).collect(),
    };

    let mut per_slide: Vec<Value> = Vec::new();
    for (number, s) in &scanned {
        let mut count = 0;
        for el in walk(array(s.get("pageElements"))) {
            count += if el.get("table").is_some() {
                table_cells(el).iter().map(|(_, _, t)| t.matches(match_text.as_str()).count()).sum::<usize>()
            } else {
                shape_text(el).matches(match_text.as_str()).count()
            };
        }
        if count > 0 {
            per_slide
                .push(json!({"slide_number": number, "slide_id": s.get("objectId"), "occurrences": count}));
        }
    }
    if per_slide.is_empty() {
        return Err(ToolError::msg(format!(
            "no match for {} on the searched slide(s) (matching is case-sensitive)",
            repr(&match_text)
        )));
    }
    let impact = json!({
        "target": match_text,
        "replaces_with": clip(&replacement),
        "occurrences": per_slide.iter().map(|p| p["occurrences"].as_u64().unwrap_or(0)).sum::<u64>(),
        "slides": per_slide,
    });
    if dry_run {
        let mut out = json!({"dry_run": true, "action": "replace_slide_text"});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if !confirm {
        return Ok(preview_response("replace_slide_text", impact).into());
    }

    // Always name the pages: with none, replaceAllText also rewrites layouts, masters and notes.
    let request = json!({"replaceAllText": {
        "containsText": {"text": match_text, "matchCase": true},
        "replaceText": replacement,
        "pageObjectIds": scanned.iter().map(|(_, s)| s.get("objectId")).collect::<Vec<_>>(),
    }});
    let reply = batch(api, &pid, vec![request], Some(&pres)).await?;
    let changed =
        reply.pointer("/replies/0/replaceAllText/occurrencesChanged").cloned().unwrap_or_else(|| json!(0));
    let mut out = json!({"presentation_id": pid, "replaced": true});
    merge(&mut out, fields(&impact));
    out["occurrences_changed"] = changed;
    Ok(out.into())
}

fn find_element<'a>(slide_list: &'a [Value], object_id: &str) -> Result<(usize, &'a Value)> {
    for (i, s) in slide_list.iter().enumerate() {
        for el in walk(array(s.get("pageElements"))) {
            if el.get("objectId").and_then(Value::as_str) == Some(object_id) {
                return Ok((i + 1, el));
            }
        }
    }
    Err(ToolError::msg(format!(
        "no page element with id {} on any slide (ids come from read_presentation)",
        repr(object_id)
    )))
}

async fn set_shape_text(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let object_id = args.req_str("object_id")?;
    let text = args.req_str("text")?;
    let row = args.opt_i64("row")?;
    let column = args.opt_i64("column")?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let (pid, pres) = get(api, &item, false).await?;
    let (number, el) = find_element(slides_of(&pres), &object_id)?;
    let (existing, cell) = if el.get("table").is_some() {
        let (Some(row), Some(column)) = (row, column) else {
            return Err(ToolError::msg(format!(
                "{} is a table: pass its 0-based row and column",
                repr(&object_id)
            )));
        };
        let found = table_cells(el)
            .into_iter()
            .find(|(r, c, _)| *r as i64 == row && *c as i64 == column)
            .map(|(_, _, t)| t);
        let Some(existing) = found else {
            return Err(ToolError::msg(format!(
                "table {} has no cell at row {row}, column {column}",
                repr(&object_id)
            )));
        };
        (existing, Some(json!({"rowIndex": row, "columnIndex": column})))
    } else if el.get("shape").is_some() {
        if row.is_some() || column.is_some() {
            return Err(ToolError::msg(format!(
                "{} is not a table, so row and column do not apply",
                repr(&object_id)
            )));
        }
        (shape_text(el), None)
    } else {
        return Err(ToolError::msg(format!(
            "{} holds no text (it is an image, line, video, chart or group)",
            repr(&object_id)
        )));
    };

    let impact = json!({
        "slide_number": number,
        "object_id": object_id,
        "cell": cell,
        "overwrites_text": clip(&existing),
        "new_text": clip(&text),
    });
    if existing == text {
        let mut out = json!({"presentation_id": pid, "unchanged": true});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if dry_run {
        let mut out = json!({"dry_run": true, "action": "set_shape_text"});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if !existing.is_empty() && !confirm {
        return Ok(preview_response("set_shape_text", impact).into());
    }
    batch(api, &pid, text_requests(&object_id, &existing, &text, cell.as_ref()), Some(&pres)).await?;
    let mut out = json!({"presentation_id": pid, "updated": true});
    merge(&mut out, fields(&impact));
    Ok(out.into())
}

async fn set_speaker_notes(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let slide = args.req_str("slide")?;
    let text = args.req_str("text")?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    let (pid, pres) = get(api, &item, false).await?;
    let (index, picked) = pick_slide(slides_of(&pres), &slide)?;
    let (notes_id, existing) = notes(picked);
    let Some(notes_id) = notes_id else {
        return Err(ToolError::msg(format!("slide {} has no speaker-notes shape to write to", index + 1)));
    };
    let impact = json!({
        "slide_number": index + 1,
        "slide_id": picked.get("objectId"),
        "overwrites_notes": clip(&existing),
        "new_notes": clip(&text),
    });
    if existing == text {
        let mut out = json!({"presentation_id": pid, "unchanged": true});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if dry_run {
        let mut out = json!({"dry_run": true, "action": "set_speaker_notes"});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if !existing.is_empty() && !confirm {
        return Ok(preview_response("set_speaker_notes", impact).into());
    }
    batch(api, &pid, text_requests(&notes_id, &existing, &text, None), Some(&pres)).await?;
    let mut out = json!({"presentation_id": pid, "updated": true});
    merge(&mut out, fields(&impact));
    Ok(out.into())
}

fn parse_email(value: &str, arg: &str) -> Result<String> {
    let email = value.trim();
    if !EMAIL.as_ref().is_some_and(|re| re.is_match(email)) {
        return Err(ToolError::msg(format!(
            "{arg} must be an email address such as 'sam@company.com': a name like '@Sam' cannot be \
             resolved to an account (got {})",
            repr(value)
        )));
    }
    Ok(email.to_string())
}

async fn add_slide_comment(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let slide = args.req_str("slide")?;
    let content = args.req_str("content")?;
    let assignee = args.opt_str("assignee")?;
    let mention = args.opt_str("mention")?;
    let element = args.opt_str("element")?;
    let confirm = args.bool_or("confirm", false)?;
    let dry_run = args.bool_or("dry_run", false)?;

    if content.trim().is_empty() {
        return Err(ToolError::msg("content must be a non-empty string"));
    }
    let assignee = assignee.map(|a| parse_email(&a, "assignee")).transpose()?;
    let mention = mention.map(|m| parse_email(&m, "mention")).transpose()?;
    let (pid, pres) = get(api, &item, false).await?;
    let (index, picked) = pick_slide(slides_of(&pres), &slide)?;
    let mut target = str_of(picked.get("objectId")).to_string();
    if let Some(element) = &element {
        let on_slide = walk(array(picked.get("pageElements")))
            .into_iter()
            .any(|el| el.get("objectId").and_then(Value::as_str) == Some(element.as_str()));
        if !on_slide {
            return Err(ToolError::msg(format!(
                "no page element with id {} on slide {} (ids come from read_presentation)",
                repr(element),
                index + 1
            )));
        }
        target = element.clone();
    }

    // Assignee first, then the mention, each once.
    let mut people: Vec<String> = Vec::new();
    for email in [&assignee, &mention].into_iter().flatten() {
        if !people.contains(email) {
            people.push(email.clone());
        }
    }
    let missing: Vec<String> = people
        .iter()
        .filter(|e| !content.contains(&format!("+{e}")) && !content.contains(&format!("@{e}")))
        .map(|e| format!("+{e}"))
        .collect();
    let text = if missing.is_empty() { content.clone() } else { format!("{} {content}", missing.join(" ")) };
    if text.len() > COMMENT_MAX_BYTES {
        return Err(ToolError::msg(format!(
            "the comment (with its @mentions) exceeds {COMMENT_MAX_BYTES} bytes"
        )));
    }

    let impact = json!({
        "slide_number": index + 1,
        "slide_id": picked.get("objectId"),
        "element": element,
        "comment": clip(&text),
        "assigns": assignee,
        "mentions": people,
        "notifies_by_email": people,
    });
    if dry_run {
        let mut out = json!({"dry_run": true, "action": "add_slide_comment"});
        merge(&mut out, fields(&impact));
        return Ok(out.into());
    }
    if !people.is_empty() && !confirm {
        return Ok(preview_response("add_slide_comment", impact).into());
    }

    let mut request = json!({"content": text, "objectId": target});
    if let Some(assignee) = &assignee {
        request["assigneeEmailAddress"] = json!(assignee);
    }
    let reply = batch(api, &pid, vec![json!({"insertComment": request})], None).await?;
    let thread = reply
        .pointer("/replies/0/insertComment/commentThread")
        .ok_or_else(|| ToolError::msg("unexpected insertComment response: no commentThread"))?;
    let comment_id = thread
        .get("commentId")
        .cloned()
        .ok_or_else(|| ToolError::msg("unexpected insertComment response: no commentId"))?;
    let mut out = json!({
        "presentation_id": pid,
        "posted": true,
        "comment_id": comment_id,
        // What Slides reports back, not what was asked for.
        "assigned_to": thread.get("headPost").and_then(|h| h.get("assigneeEmail")).cloned().unwrap_or(Value::Null),
    });
    merge(&mut out, fields(&impact));
    Ok(out.into())
}

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "read_presentation",
            READ_PRESENTATION_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "slide": {"type": "string"},
                    "chunk": {"type": "integer", "default": 0},
                    "max_chars": {"type": "integer", "default": DEFAULT_MAX_CHARS},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "create_presentation",
            CREATE_PRESENTATION_DOC,
            json!({
                "properties": {"title": {"type": "string"}},
                "required": ["title"],
            }),
        ),
        ToolDef::new(
            "add_slide",
            ADD_SLIDE_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "layout": {"type": "string", "default": "TITLE_AND_BODY"},
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "position": {"type": "integer"},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "duplicate_slide",
            DUPLICATE_SLIDE_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "slide": {"type": "string"},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "slide"],
            }),
        ),
        ToolDef::new(
            "move_slide",
            MOVE_SLIDE_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "slide": {"type": "string"},
                    "position": {"type": "integer"},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "slide", "position"],
            }),
        ),
        ToolDef::new(
            "delete_slide",
            DELETE_SLIDE_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "slide": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "slide"],
            }),
        ),
        ToolDef::new(
            "replace_slide_text",
            REPLACE_SLIDE_TEXT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "match": {"type": "string"},
                    "replacement": {"type": "string"},
                    "slide": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "match", "replacement"],
            }),
        ),
        ToolDef::new(
            "set_shape_text",
            SET_SHAPE_TEXT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "object_id": {"type": "string"},
                    "text": {"type": "string"},
                    "row": {"type": "integer"},
                    "column": {"type": "integer"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "object_id", "text"],
            }),
        ),
        ToolDef::new(
            "set_speaker_notes",
            SET_SPEAKER_NOTES_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "slide": {"type": "string"},
                    "text": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "slide", "text"],
            }),
        ),
        ToolDef::new(
            "add_slide_comment",
            ADD_SLIDE_COMMENT_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "slide": {"type": "string"},
                    "content": {"type": "string"},
                    "assignee": {"type": "string"},
                    "mention": {"type": "string"},
                    "element": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false},
                },
                "required": ["item", "slide", "content"],
            }),
        ),
    ]
}

pub async fn dispatch(name: &str, api: &dyn GoogleApi, args: &Args) -> Option<Result<ToolOutput>> {
    Some(match name {
        "read_presentation" => read_presentation(api, args).await,
        "create_presentation" => create_presentation(api, args).await,
        "add_slide" => add_slide(api, args).await,
        "duplicate_slide" => duplicate_slide(api, args).await,
        "move_slide" => move_slide(api, args).await,
        "delete_slide" => delete_slide(api, args).await,
        "replace_slide_text" => replace_slide_text(api, args).await,
        "set_shape_text" => set_shape_text(api, args).await,
        "set_speaker_notes" => set_speaker_notes(api, args).await,
        "add_slide_comment" => add_slide_comment(api, args).await,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    //! Tool-logic tests against the recording fake: the confirm gates and the exact requests built.
    //! Each mirrors a test of the same name in `tests/test_slides.py`.

    use super::*;
    use crate::tools::testing::FakeApi;

    /// Bare IDs must be >= 20 chars to parse.
    fn pid() -> String {
        "P".repeat(30)
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

    fn text(content: &str) -> Value {
        json!({"textElements": [{"textRun": {"content": content}}]})
    }

    fn with_newline(t: &str) -> String {
        if t.is_empty() {
            "\n".to_string()
        } else {
            format!("{t}\n")
        }
    }

    fn shape(oid: &str, t: &str, placeholder: Option<&str>) -> Value {
        let mut shape = json!({ "text": text(&with_newline(t)) });
        if let Some(p) = placeholder {
            shape["placeholder"] = json!({"type": p});
        }
        json!({"objectId": oid, "shape": shape})
    }

    fn table(oid: &str, grid: &[&[&str]]) -> Value {
        let rows: Vec<Value> = grid
            .iter()
            .map(|row| json!({"tableCells": row.iter().map(|c| json!({"text": text(&with_newline(c))})).collect::<Vec<_>>()}))
            .collect();
        json!({"objectId": oid, "table": {"rows": grid.len(), "columns": grid[0].len(), "tableRows": rows}})
    }

    fn slide(oid: &str, elements: Vec<Value>, notes: Option<&str>) -> Value {
        let notes_els: Vec<Value> = notes.map(|n| shape("n_shape", n, None)).into_iter().collect();
        json!({
            "objectId": oid,
            "pageElements": elements,
            "slideProperties": {
                "layoutObjectId": "lay1",
                "notesPage": {"pageElements": notes_els, "notesProperties": {"speakerNotesObjectId": "n_shape"}},
            },
        })
    }

    fn deck() -> Value {
        json!({
            "title": "Q3 review",
            "revisionId": "rev-1",
            "layouts": [{"objectId": "lay1", "layoutProperties": {"displayName": "Title and body"}}],
            "slides": [
                slide("s1", vec![shape("t1", "Roadmap", Some("TITLE")), shape("b1", "Ship Acme v2\nAcme tests", Some("BODY"))], Some("Say hi")),
                slide("s2", vec![shape("t2", "Risks", Some("TITLE")), table("tab2", &[&["Acme", "Owner"], &["Perf", ""]])], None),
                slide("s3", vec![shape("t3", "Next steps", Some("TITLE")), shape("e3", "", None)], None),
            ],
        })
    }

    fn api_with_deck() -> FakeApi {
        let api = FakeApi::new();
        api.on("slides_get", deck());
        api
    }

    fn requests(api: &FakeApi) -> Vec<Value> {
        api.last("slides_batch_update")["body"]["requests"].as_array().unwrap().clone()
    }

    fn wrote(api: &FakeApi) -> bool {
        api.call_count("slides_batch_update") > 0
    }

    // ---- read ---------------------------------------------------------------------------

    #[tokio::test]
    async fn read_presentation_renders_ids_tables_notes_and_an_outline() {
        let api = api_with_deck();
        let out = ok(&api, "read_presentation", json!({"item": pid()})).await;
        assert_eq!(out["slide_count"], 3);
        assert_eq!(out["outline"][1], json!({"number": 2, "object_id": "s2", "title": "Risks"}));
        assert_eq!(out["layouts"], json!([{"object_id": "lay1", "name": "Title and body"}]));
        assert_eq!(out["revision_id"], "rev-1");
        let c = out["content"].as_str().unwrap();
        assert!(c.contains("## Slide 1 · id=s1 · layout=Title and body"));
        assert!(c.contains("- [BODY] id=b1: Ship Acme v2\n    Acme tests"));
        assert!(c.contains("- [TABLE] id=tab2 2x2") && c.contains("r1: Perf | "));
        assert!(c.contains("- [SHAPE] id=e3: (empty)"));
        assert!(c.contains("Notes: Say hi"));
    }

    #[tokio::test]
    async fn read_presentation_single_slide_by_number_or_id() {
        let api = FakeApi::new();
        api.on("slides_get", deck()).on("slides_get", deck());
        let by_number = ok(&api, "read_presentation", json!({"item": pid(), "slide": "2"})).await;
        let by_id = ok(&api, "read_presentation", json!({"item": pid(), "slide": "s2"})).await;
        assert_eq!(by_number["content"], by_id["content"]);
        let c = by_number["content"].as_str().unwrap();
        assert!(c.contains("Slide 2") && !c.contains("Slide 1"));
    }

    #[tokio::test]
    async fn unknown_or_out_of_range_slide_is_refused() {
        let api = FakeApi::new();
        api.on("slides_get", deck()).on("slides_get", deck());
        let out_of_range = fails(&api, "read_presentation", json!({"item": pid(), "slide": "9"})).await;
        assert!(out_of_range.contains("out of range"), "{out_of_range}");
        let unknown = fails(&api, "read_presentation", json!({"item": pid(), "slide": "nope"})).await;
        assert!(unknown.contains("no slide with id 'nope'"), "{unknown}");
    }

    // ---- replace_slide_text -------------------------------------------------------------

    #[tokio::test]
    async fn replace_slide_text_counts_shapes_and_table_cells_per_slide_and_gates() {
        let api = api_with_deck();
        let out =
            ok(&api, "replace_slide_text", json!({"item": pid(), "match": "Acme", "replacement": "Globex"}))
                .await;
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"]["occurrences"], 3);
        assert_eq!(
            out["impact"]["slides"],
            json!([
                {"slide_number": 1, "slide_id": "s1", "occurrences": 2},
                {"slide_number": 2, "slide_id": "s2", "occurrences": 1},
            ])
        );
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn replace_slide_text_names_the_slides_so_layouts_masters_and_notes_are_untouched() {
        let api = api_with_deck();
        api.on("slides_batch_update", json!({"replies": [{"replaceAllText": {"occurrencesChanged": 3}}]}));
        let out = ok(
            &api,
            "replace_slide_text",
            json!({"item": pid(), "match": "Acme", "replacement": "Globex", "confirm": true}),
        )
        .await;
        let req = &requests(&api)[0]["replaceAllText"];
        // omitted => the API also rewrites layouts/masters
        assert_eq!(req["pageObjectIds"], json!(["s1", "s2", "s3"]));
        assert_eq!(req["containsText"], json!({"text": "Acme", "matchCase": true}));
        assert_eq!(req["replaceText"], "Globex");
        assert_eq!(out["occurrences_changed"], 3);
        assert_eq!(
            api.last("slides_batch_update")["body"]["writeControl"],
            json!({"requiredRevisionId": "rev-1"})
        );
    }

    #[tokio::test]
    async fn replace_slide_text_scoped_to_one_slide_sends_only_that_page() {
        let api = api_with_deck();
        api.on("slides_batch_update", json!({"replies": [{}]}));
        ok(
            &api,
            "replace_slide_text",
            json!({"item": pid(), "match": "Acme", "replacement": "Globex", "slide": "2", "confirm": true}),
        )
        .await;
        assert_eq!(requests(&api)[0]["replaceAllText"]["pageObjectIds"], json!(["s2"]));
    }

    #[tokio::test]
    async fn replace_slide_text_dry_run_neither_writes_nor_gates() {
        let api = api_with_deck();
        let out = ok(
            &api,
            "replace_slide_text",
            json!({"item": pid(), "match": "Acme", "replacement": "Globex", "dry_run": true}),
        )
        .await;
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["occurrences"], 3);
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn replace_slide_text_unmatched_or_multiparagraph_match_raises_before_writing() {
        let api = api_with_deck();
        let args = |m: &str| json!({"item": pid(), "match": m, "replacement": "x", "confirm": true});
        // case-sensitive
        assert!(fails(&api, "replace_slide_text", args("acme")).await.contains("no match"));
        assert!(fails(&api, "replace_slide_text", args("a\nb")).await.contains("span paragraphs"));
        assert!(fails(&api, "replace_slide_text", args("")).await.contains("non-empty"));
        assert!(!wrote(&api));
    }

    // ---- set_shape_text -----------------------------------------------------------------

    #[tokio::test]
    async fn set_shape_text_overwrite_gates_then_deletes_all_and_inserts() {
        let api = FakeApi::new();
        api.on("slides_get", deck()).on("slides_get", deck());
        let out = ok(&api, "set_shape_text", json!({"item": pid(), "object_id": "t1", "text": "Plan"})).await;
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"]["overwrites_text"], "Roadmap");
        assert!(!wrote(&api));
        ok(
            &api,
            "set_shape_text",
            json!({"item": pid(), "object_id": "t1", "text": "Plan", "confirm": true}),
        )
        .await;
        assert_eq!(
            requests(&api),
            vec![
                json!({"deleteText": {"objectId": "t1", "textRange": {"type": "ALL"}}}),
                json!({"insertText": {"objectId": "t1", "insertionIndex": 0, "text": "Plan"}}),
            ]
        );
    }

    #[tokio::test]
    async fn set_shape_text_on_an_empty_shape_needs_no_confirm_and_sends_no_delete() {
        // deleteText on empty text is rejected by Slides, so it must not be sent.
        let api = api_with_deck();
        let out =
            ok(&api, "set_shape_text", json!({"item": pid(), "object_id": "e3", "text": "Hello"})).await;
        assert_eq!(out["updated"], true);
        assert_eq!(
            requests(&api),
            vec![json!({"insertText": {"objectId": "e3", "insertionIndex": 0, "text": "Hello"}})]
        );
    }

    #[tokio::test]
    async fn set_shape_text_clearing_sends_only_a_delete() {
        let api = api_with_deck();
        ok(&api, "set_shape_text", json!({"item": pid(), "object_id": "t1", "text": "", "confirm": true}))
            .await;
        assert_eq!(
            requests(&api),
            vec![json!({"deleteText": {"objectId": "t1", "textRange": {"type": "ALL"}}})]
        );
    }

    #[tokio::test]
    async fn set_shape_text_same_text_is_a_noop() {
        let api = api_with_deck();
        let out =
            ok(&api, "set_shape_text", json!({"item": pid(), "object_id": "t1", "text": "Roadmap"})).await;
        assert_eq!(out["unchanged"], true);
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn set_shape_text_targets_a_table_cell_by_row_and_column() {
        let api = api_with_deck();
        // empty cell: no confirm needed
        ok(
            &api,
            "set_shape_text",
            json!({"item": pid(), "object_id": "tab2", "text": "Sam", "row": 1, "column": 1}),
        )
        .await;
        assert_eq!(
            requests(&api),
            vec![json!({"insertText": {
                "objectId": "tab2",
                "cellLocation": {"rowIndex": 1, "columnIndex": 1},
                "insertionIndex": 0,
                "text": "Sam",
            }})]
        );
    }

    #[tokio::test]
    async fn set_shape_text_validates_the_target() {
        let api = FakeApi::new();
        for _ in 0..4 {
            api.on("slides_get", deck());
        }
        let msg =
            fails(&api, "set_shape_text", json!({"item": pid(), "object_id": "tab2", "text": "x"})).await;
        assert!(msg.contains("is a table"), "{msg}");
        let msg = fails(
            &api,
            "set_shape_text",
            json!({"item": pid(), "object_id": "tab2", "text": "x", "row": 5, "column": 0}),
        )
        .await;
        assert!(msg.contains("no cell at row 5"), "{msg}");
        let msg = fails(
            &api,
            "set_shape_text",
            json!({"item": pid(), "object_id": "t1", "text": "x", "row": 0, "column": 0}),
        )
        .await;
        assert!(msg.contains("not a table"), "{msg}");
        let msg =
            fails(&api, "set_shape_text", json!({"item": pid(), "object_id": "ghost", "text": "x"})).await;
        assert!(msg.contains("no page element"), "{msg}");
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn set_shape_text_dry_run_does_not_write() {
        let api = api_with_deck();
        let out = ok(
            &api,
            "set_shape_text",
            json!({"item": pid(), "object_id": "t1", "text": "Plan", "dry_run": true}),
        )
        .await;
        assert_eq!(out["dry_run"], true);
        assert!(!wrote(&api));
    }

    // ---- speaker notes ------------------------------------------------------------------

    #[tokio::test]
    async fn set_speaker_notes_gates_when_notes_exist_and_targets_the_notes_shape() {
        let api = FakeApi::new();
        api.on("slides_get", deck()).on("slides_get", deck());
        let out = ok(&api, "set_speaker_notes", json!({"item": pid(), "slide": "1", "text": "New"})).await;
        assert_eq!(out["status"], "confirmation_required");
        assert!(!wrote(&api));
        ok(&api, "set_speaker_notes", json!({"item": pid(), "slide": "1", "text": "New", "confirm": true}))
            .await;
        assert_eq!(
            requests(&api),
            vec![
                json!({"deleteText": {"objectId": "n_shape", "textRange": {"type": "ALL"}}}),
                json!({"insertText": {"objectId": "n_shape", "insertionIndex": 0, "text": "New"}}),
            ]
        );
    }

    #[tokio::test]
    async fn set_speaker_notes_on_a_slide_without_notes_inserts_without_confirm() {
        let api = api_with_deck();
        let out =
            ok(&api, "set_speaker_notes", json!({"item": pid(), "slide": "3", "text": "First notes"})).await;
        assert_eq!(out["updated"], true);
        assert_eq!(
            requests(&api),
            vec![json!({"insertText": {"objectId": "n_shape", "insertionIndex": 0, "text": "First notes"}})]
        );
    }

    // ---- delete / duplicate / move ------------------------------------------------------

    #[tokio::test]
    async fn delete_slide_gates_and_previews_then_deletes_the_page() {
        let api = FakeApi::new();
        api.on("slides_get", deck()).on("slides_get", deck());
        let out = ok(&api, "delete_slide", json!({"item": pid(), "slide": "2"})).await;
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"]["title"], "Risks");
        assert_eq!(out["impact"]["elements"], 2);
        assert!(out["impact"]["deletes_text"].as_str().unwrap().contains("Acme"));
        assert!(!wrote(&api));
        ok(&api, "delete_slide", json!({"item": pid(), "slide": "2", "confirm": true})).await;
        assert_eq!(requests(&api), vec![json!({"deleteObject": {"objectId": "s2"}})]);
    }

    #[tokio::test]
    async fn delete_slide_dry_run_does_not_write() {
        let api = api_with_deck();
        let out = ok(&api, "delete_slide", json!({"item": pid(), "slide": "2", "dry_run": true})).await;
        assert_eq!(out["dry_run"], true);
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn duplicate_slide_returns_the_new_id_placed_after_the_original() {
        let api = api_with_deck();
        api.on("slides_batch_update", json!({"replies": [{"duplicateObject": {"objectId": "s1_copy"}}]}));
        let out = ok(&api, "duplicate_slide", json!({"item": pid(), "slide": "s1"})).await;
        assert_eq!(requests(&api), vec![json!({"duplicateObject": {"objectId": "s1"}})]);
        assert_eq!(out["new_slide_id"], "s1_copy");
        assert_eq!(out["new_slide_number"], 2);
    }

    #[tokio::test]
    async fn duplicate_and_move_dry_runs_report_the_plan_without_writing() {
        let api = FakeApi::new();
        api.on("slides_get", deck()).on("slides_get", deck());
        let dup = ok(&api, "duplicate_slide", json!({"item": pid(), "slide": "2", "dry_run": true})).await;
        assert_eq!(dup["dry_run"], true);
        assert_eq!(dup["slide_id"], "s2");
        assert_eq!(dup["title"], "Risks");
        let mv = ok(&api, "move_slide", json!({"item": pid(), "slide": "3", "position": 1, "dry_run": true}))
            .await;
        assert_eq!(mv["dry_run"], true);
        assert_eq!((mv["from"].clone(), mv["to"].clone()), (json!(3), json!(1)));
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn move_slide_translates_the_final_position_to_a_pre_move_insertion_index() {
        // (slide, final position, expected insertionIndex)
        let cases = [
            ("1", 3, 3), // moving later: index is in the pre-move arrangement, so one past the target
            ("3", 1, 0), // moving earlier: the target index itself
            ("1", 2, 2),
            ("2", 1, 0),
        ];
        for (slide, position, insertion) in cases {
            let api = api_with_deck();
            ok(&api, "move_slide", json!({"item": pid(), "slide": slide, "position": position})).await;
            let reqs = requests(&api);
            assert_eq!(reqs.len(), 1);
            assert_eq!(
                reqs[0]["updateSlidesPosition"]["insertionIndex"], insertion,
                "moving slide {slide} to position {position}"
            );
        }
    }

    #[tokio::test]
    async fn move_slide_to_its_own_position_is_a_noop_and_range_is_checked() {
        let api = FakeApi::new();
        api.on("slides_get", deck()).on("slides_get", deck());
        let out = ok(&api, "move_slide", json!({"item": pid(), "slide": "2", "position": 2})).await;
        assert_eq!(out["moved"], false);
        assert!(!wrote(&api));
        let msg = fails(&api, "move_slide", json!({"item": pid(), "slide": "1", "position": 4})).await;
        assert!(msg.contains("out of range"), "{msg}");
    }

    // ---- create / add_slide -------------------------------------------------------------

    #[tokio::test]
    async fn create_presentation_returns_the_first_slide_id() {
        let api = FakeApi::new();
        api.on(
            "slides_create",
            json!({"presentationId": "NEW", "title": "T", "slides": [{"objectId": "p"}]}),
        );
        let out = ok(&api, "create_presentation", json!({"title": "T"})).await;
        assert_eq!(api.last("slides_create"), json!({"title": "T"}));
        assert_eq!(out["url"], "https://docs.google.com/presentation/d/NEW/edit");
        assert_eq!(out["first_slide_id"], "p");
    }

    fn add_slide_api(page_elements: Vec<Value>) -> FakeApi {
        let api = FakeApi::new();
        api.on("slides_batch_update", json!({"replies": [{"createSlide": {"objectId": "newslide"}}]}));
        api.on("slides_page_get", json!({"pageElements": page_elements}));
        api
    }

    #[tokio::test]
    async fn add_slide_predefined_layout_at_a_position_fills_title_and_body_placeholders() {
        let api = add_slide_api(vec![shape("nt", "", Some("TITLE")), shape("nb", "", Some("BODY"))]);
        let out = ok(
            &api,
            "add_slide",
            json!({"item": pid(), "layout": "title_and_body", "title": "Hi", "body": "There", "position": 2}),
        )
        .await;
        let batches = api.calls_to("slides_batch_update");
        assert_eq!(
            batches[0]["body"]["requests"],
            json!([{"createSlide": {"slideLayoutReference": {"predefinedLayout": "TITLE_AND_BODY"}, "insertionIndex": 1}}])
        );
        assert_eq!(
            requests(&api),
            vec![
                json!({"insertText": {"objectId": "nt", "insertionIndex": 0, "text": "Hi"}}),
                json!({"insertText": {"objectId": "nb", "insertionIndex": 0, "text": "There"}}),
            ]
        );
        assert_eq!(out["slide_id"], "newslide");
        assert_eq!(out["filled"], json!(["title", "body"]));
        assert!(out.get("unfilled").is_none());
        // a predefined layout needs no read of the deck first
        assert_eq!(api.call_count("slides_get"), 0);
    }

    #[tokio::test]
    async fn add_slide_reports_unfilled_instead_of_failing_after_the_slide_was_created() {
        // TITLE_ONLY has no body placeholder
        let api = add_slide_api(vec![shape("nt", "", Some("TITLE"))]);
        let out = ok(
            &api,
            "add_slide",
            json!({"item": pid(), "layout": "TITLE_ONLY", "title": "Hi", "body": "Lost"}),
        )
        .await;
        assert_eq!(out["filled"], json!(["title"]));
        assert_eq!(out["unfilled"], json!(["body"]));
    }

    #[tokio::test]
    async fn add_slide_accepts_a_deck_layout_by_name_and_rejects_unknown_ones() {
        let api = add_slide_api(vec![]);
        api.on("slides_get", deck()).on("slides_get", deck());
        ok(&api, "add_slide", json!({"item": pid(), "layout": "title and body"})).await;
        let first = &api.calls_to("slides_batch_update")[0]["body"]["requests"];
        assert_eq!(first[0]["createSlide"]["slideLayoutReference"], json!({"layoutId": "lay1"}));
        let msg = fails(&api, "add_slide", json!({"item": pid(), "layout": "Nonexistent"})).await;
        assert!(msg.contains("unknown layout 'Nonexistent'"), "{msg}");
    }

    #[tokio::test]
    async fn add_slide_dry_run_and_position_validation() {
        let api = FakeApi::new();
        let out = ok(&api, "add_slide", json!({"item": pid(), "layout": "BLANK", "dry_run": true})).await;
        assert_eq!(out["dry_run"], true);
        assert!(!wrote(&api));
        let msg = fails(&api, "add_slide", json!({"item": pid(), "layout": "BLANK", "position": 0})).await;
        assert!(msg.contains(">= 1"), "{msg}");
    }

    // ---- comments -----------------------------------------------------------------------

    const SAM: &str = "sam@company.com";
    const KAI: &str = "kai@company.com";

    fn thread(cid: &str, content: &str, anchor: Option<&str>, assignee: Option<&str>, status: &str) -> Value {
        let mut head = json!({"content": content, "author": {"displayName": "Pat"}, "deleted": false});
        if let Some(a) = assignee {
            head["assigneeEmail"] = json!(a);
        }
        json!({"commentId": cid, "anchorId": anchor, "status": status, "headPost": head, "replies": []})
    }

    /// An API whose next batchUpdate answers like Slides does for a posted comment.
    fn posted_api(assignee: Option<&str>) -> FakeApi {
        let api = api_with_deck();
        let head = assignee.map_or(json!({}), |a| json!({"assigneeEmail": a}));
        api.on(
            "slides_batch_update",
            json!({"replies": [{"insertComment": {"commentThread": {"commentId": "c_new", "headPost": head}}}]}),
        );
        api
    }

    #[tokio::test]
    async fn a_plain_comment_needs_no_confirm_and_targets_the_slide() {
        let api = posted_api(None);
        let out =
            ok(&api, "add_slide_comment", json!({"item": pid(), "slide": "2", "content": "Looks good"}))
                .await;
        assert_eq!(
            requests(&api),
            vec![json!({"insertComment": {"content": "Looks good", "objectId": "s2"}})]
        );
        assert_eq!(out["posted"], true);
        assert_eq!(out["comment_id"], "c_new");
        assert_eq!(out["assigned_to"], Value::Null);
        // no revision pin: adding a comment is not a content edit
        assert!(api.last("slides_batch_update")["body"].get("writeControl").is_none());
    }

    #[tokio::test]
    async fn assigning_gates_and_previews_who_would_be_emailed_without_posting() {
        let api = api_with_deck();
        let out = ok(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "Please review", "assignee": SAM}),
        )
        .await;
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"]["notifies_by_email"], json!([SAM]));
        assert_eq!(out["impact"]["assigns"], SAM);
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn confirmed_assignment_sends_the_assignee_and_mentions_them_in_the_text() {
        let api = posted_api(Some(SAM));
        let out = ok(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "Please review", "assignee": SAM, "confirm": true}),
        )
        .await;
        assert_eq!(
            requests(&api),
            vec![json!({"insertComment": {
                "content": format!("+{SAM} Please review"), "objectId": "s1", "assigneeEmailAddress": SAM}})]
        );
        assert_eq!(out["assigned_to"], SAM);
    }

    #[tokio::test]
    async fn assigned_to_reports_what_slides_returned_not_what_was_requested() {
        let api = posted_api(None); // the API echoes no assignee
        let out = ok(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "x", "assignee": SAM, "confirm": true}),
        )
        .await;
        assert_eq!(out["assigns"], SAM);
        assert_eq!(out["assigned_to"], Value::Null);
    }

    #[tokio::test]
    async fn a_mention_alone_notifies_and_gates_but_does_not_assign() {
        let api = api_with_deck();
        let gate = ok(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "fyi", "mention": KAI}),
        )
        .await;
        assert_eq!(gate["status"], "confirmation_required");
        let api = posted_api(None);
        ok(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "fyi", "mention": KAI, "confirm": true}),
        )
        .await;
        let req = &requests(&api)[0]["insertComment"];
        assert_eq!(req["content"], format!("+{KAI} fyi"));
        assert!(req.get("assigneeEmailAddress").is_none());
    }

    #[tokio::test]
    async fn assignee_and_a_second_mention_are_each_mentioned_once() {
        let api = posted_api(Some(SAM));
        ok(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": format!("+{SAM} see this"),
                   "assignee": SAM, "mention": KAI, "confirm": true}),
        )
        .await;
        assert_eq!(requests(&api)[0]["insertComment"]["content"], format!("+{KAI} +{SAM} see this"));
    }

    #[tokio::test]
    async fn a_name_instead_of_an_email_is_refused_before_any_io() {
        let api = FakeApi::new();
        for bad in ["@Sam", "Sam", "sam@", "sam@company", "a b@c.com"] {
            let msg = fails(
                &api,
                "add_slide_comment",
                json!({"item": pid(), "slide": "1", "content": "x", "assignee": bad, "confirm": true}),
            )
            .await;
            assert!(msg.contains("email address"), "{bad}: {msg}");
        }
        let msg = fails(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "x", "mention": "@Kai"}),
        )
        .await;
        assert!(msg.contains("email address"), "{msg}");
        assert_eq!(api.call_count("slides_get"), 0);
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn element_pins_the_comment_to_that_element_and_must_be_on_the_slide() {
        let api = posted_api(None);
        ok(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "check this", "element": "b1"}),
        )
        .await;
        assert_eq!(requests(&api)[0]["insertComment"]["objectId"], "b1");
        let api = api_with_deck();
        let msg = fails(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "x", "element": "t2"}),
        )
        .await; // exists, but on slide 2
        assert!(msg.contains("no page element"), "{msg}");
    }

    #[tokio::test]
    async fn comment_dry_run_previews_without_writing_or_gating() {
        let api = api_with_deck();
        let out = ok(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": "x", "assignee": SAM, "dry_run": true}),
        )
        .await;
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["assigns"], SAM);
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn empty_or_oversize_comments_are_refused_counting_the_added_mentions() {
        let api = api_with_deck();
        let msg =
            fails(&api, "add_slide_comment", json!({"item": pid(), "slide": "1", "content": "   "})).await;
        assert!(msg.contains("non-empty"), "{msg}");
        let big = "é".repeat(1030); // 2060 bytes before the mention is added
        let api = api_with_deck();
        let msg = fails(
            &api,
            "add_slide_comment",
            json!({"item": pid(), "slide": "1", "content": big, "assignee": SAM, "confirm": true}),
        )
        .await;
        assert!(msg.contains("2048 bytes"), "{msg}");
        assert!(!wrote(&api));
    }

    #[tokio::test]
    async fn read_presentation_shows_threads_on_their_slide_and_unanchored_ones_last() {
        let mut d = deck();
        d["slides"][0]["commentAnchors"] = json!([{"anchorId": "A1"}, {"anchorId": "A2"}]);
        let mut c1 = thread("c1", "Tighten this", Some("A1"), Some(SAM), "OPEN");
        c1["replies"] = json!([{"content": "Done", "author": {"displayName": "Lee"}}]);
        let mut gone = thread("c4", "gone", Some("A1"), None, "OPEN");
        gone["headPost"]["deleted"] = json!(true);
        d["comments"] = json!([
            c1,
            thread("c2", "On a shape", Some("A2"), None, "RESOLVED"),
            thread("c3", "Drive-API comment", None, None, "OPEN"),
            gone,
        ]);
        let api = FakeApi::new();
        api.on("slides_get", d.clone()).on("slides_get", d);
        let out = ok(&api, "read_presentation", json!({"item": pid()})).await;
        let content = out["content"].as_str().unwrap();
        let slide1 = content.split("## Slide 2").next().unwrap();
        assert!(
            slide1.contains(&format!("- [OPEN] id=c1 Pat -> assigned to {SAM}: Tighten this")),
            "{slide1}"
        );
        assert!(slide1.contains("reply from Lee: Done") && slide1.contains("- [RESOLVED] id=c2"));
        assert!(!content.contains("gone"));
        assert!(
            content.trim_end().ends_with("## Unanchored comments\n- [OPEN] id=c3 Pat: Drive-API comment"),
            "{content}"
        );
        // a single-slide read shows only that slide's threads, not the unanchored ones
        let only = ok(&api, "read_presentation", json!({"item": pid(), "slide": "1"})).await;
        let only = only["content"].as_str().unwrap();
        assert!(only.contains("c1") && !only.contains("Unanchored"));
    }

    #[tokio::test]
    async fn only_read_presentation_asks_for_comments_and_it_does_so_in_the_view_only_safe_mode() {
        // Comments are omitted from presentations.get unless requested, and INCLUDED is a 403 for a
        // user who cannot view comments; DEFAULT_FOR_CURRENT_ACCESS is the mode that degrades safely.
        // The mode itself is pinned by `clients::tests::slides_get_asks_for_comments_only_when_told_to…`;
        // here, that only read_presentation sends the flag at all.
        let api = FakeApi::new();
        api.on("slides_get", deck()).on("slides_get", deck());
        ok(&api, "read_presentation", json!({"item": pid()})).await;
        assert_eq!(api.last("slides_get")["comments"], true);
        ok(&api, "delete_slide", json!({"item": pid(), "slide": "1"})).await;
        assert_eq!(api.last("slides_get")["comments"], false);
    }

    // ---- helpers ------------------------------------------------------------------------

    #[test]
    fn repr_quotes_like_python() {
        assert_eq!(repr("nope"), "'nope'");
        assert_eq!(repr("it's"), "\"it's\"");
        assert_eq!(repr("a\"b'c"), "'a\"b\\'c'");
        assert_eq!(repr("a\nb"), "'a\\nb'");
    }
}
