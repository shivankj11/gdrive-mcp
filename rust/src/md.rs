//! Markdown -> Google Docs styling: parse a small dialect into plain text + batchUpdate requests.
//!
//! The Docs write tools render this opt-in (markdown=true). Dialect, kept deliberately small:
//!
//!   - headings:   '# ' .. '###### ' at line start -> HEADING_1..6
//!   - bullets:    '- ' / '* ' / '+ ' at line start -> bulleted list
//!   - numbered:   '1. ' / '1) ' at line start -> numbered list
//!   - nesting:    two spaces or one tab of indent per list level (list text is inserted with
//!                 leading tabs, which createParagraphBullets consumes to set the nesting level)
//!   - inline:     **bold**, __bold__, *italic*, _italic_, ***bold italic***, <u>underline</u>
//!
//! There is no escape syntax: text that looks like markup gets styled (the plain, non-markdown
//! write path stores text verbatim). All offsets are UTF-16 code units — the unit the Docs API's
//! Range/Location indexes count (a `char` count undercounts astral-plane chars like emoji, and a
//! byte count overcounts everything non-ASCII).

use std::sync::LazyLock;

use serde_json::{json, Map, Value};

/// Length in UTF-16 code units, the unit of Docs API indexes.
pub fn u16len(s: &str) -> i64 {
    s.chars().map(|c| c.len_utf16() as i64).sum()
}

/// A Docs list preset: bulleted or numbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ListKind {
    Bullet,
    Number,
}

impl ListKind {
    fn preset(self) -> &'static str {
        match self {
            ListKind::Bullet => "BULLET_DISC_CIRCLE_SQUARE",
            ListKind::Number => "NUMBERED_DECIMAL_ALPHA_ROMAN",
        }
    }
}

/// One inline style span: `(start, end, styles)` with UTF-16 offsets and Docs textStyle field
/// names ("bold", "italic", "underline").
pub type Span = (i64, i64, Vec<&'static str>);

/// Compiled patterns are `Option` so a (impossible, pattern-is-a-literal) compile failure
/// degrades to "no markup here" instead of taking the process down.
static HEADING_RE: LazyLock<Option<regex::Regex>> =
    LazyLock::new(|| regex::Regex::new(r"^(#{1,6}) (.*)$").ok());
static BULLET_RE: LazyLock<Option<regex::Regex>> =
    LazyLock::new(|| regex::Regex::new(r"^(?P<indent>[\t ]*)[-*+] (?P<content>.*)$").ok());
static NUMBER_RE: LazyLock<Option<regex::Regex>> =
    LazyLock::new(|| regex::Regex::new(r"^(?P<indent>[\t ]*)\d{1,9}[.)] (?P<content>.*)$").ok());

// Alternation order matters: longer delimiters first so '***' isn't eaten as '*' + '**'.
// Openers must not be followed by whitespace (closers not preceded by it) so 'a * b' stays
// literal, and '_' emphasis requires non-word boundaries so snake_case stays literal. The
// lookaround is why this one needs the backtracking engine.
/// `\s` and `\w` are spelled out rather than left to fancy-regex's defaults, because the two
/// engines disagree: Python's `re` counts the C0 separators U+001C..U+001F as whitespace, and its
/// `\w` is "alphanumeric or underscore" — where Rust's also includes combining marks and
/// connector punctuation. Left alone, `_snake_` next to a combining mark would be emphasised by
/// exactly one of the two implementations.
const PY_SPACE: &str = r"[\s\x1c-\x1f]";
const PY_WORD: &str = r"[\p{L}\p{N}_]";

static INLINE_RE: LazyLock<Option<fancy_regex::Regex>> = LazyLock::new(|| {
    let pattern = format!(
        concat!(
            r"\*\*\*(?!{s}){bi}(?<!{s})\*\*\*",
            r"|\*\*(?!{s})(?P<b>.+?)(?<!{s})\*\*",
            r"|\*(?!{s})(?P<i>[^*]+?)(?<!{s})\*",
            r"|(?<!{w})__(?!{s})(?P<b2>.+?)(?<!{s})__(?!{w})",
            r"|(?<!{w})_(?!{s})(?P<i2>[^_]+?)(?<!{s})_(?!{w})",
            r"|<u>(?P<u>.+?)</u>",
        ),
        s = PY_SPACE,
        w = PY_WORD,
        bi = r"(?P<bi>.+?)",
    );
    // The default backtrack limit is 1,000,000 steps, which a long line of prose can exhaust —
    // and the scan then stops silently, leaving the rest of the line's markup as literal text in
    // the document. Python's `re` has no such ceiling, so lift it.
    fancy_regex::RegexBuilder::new(&pattern).backtrack_limit(usize::MAX).build().ok()
});

/// Scan order is load-bearing: the FIRST named group that matched wins, so '***x***' reads as
/// bold+italic and never as the bare 'b'/'i' groups. A tuple's order is preserved into the
/// request's "fields" string: ("bold", "italic") -> "bold,italic".
const GROUP_STYLES: &[(&str, &[&str])] = &[
    ("bi", &["bold", "italic"]),
    ("b", &["bold"]),
    ("b2", &["bold"]),
    ("i", &["italic"]),
    ("i2", &["italic"]),
    ("u", &["underline"]),
];

const MAX_NESTING: usize = 8; // Docs lists support nesting levels 0..8

/// Recursion bound for nested emphasis, standing in for CPython's recursion limit: markup
/// nested deeper than this stays literal rather than overflowing the stack.
const MAX_INLINE_DEPTH: u32 = 500;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedMarkdown {
    /// Plain text to insert: markers stripped, list nesting encoded as leading tabs.
    pub text: String,
    pub spans: Vec<Span>,
    /// `(start, end, level)`, one per heading line.
    pub headings: Vec<(i64, i64, i64)>,
    /// Contiguous plain-line runs.
    pub normal_runs: Vec<(i64, i64)>,
    /// Contiguous non-list-line runs (headings included).
    pub nonlist_runs: Vec<(i64, i64)>,
    pub list_runs: Vec<(i64, i64, ListKind)>,
    pub list_items: i64,
}

impl ParsedMarkdown {
    /// Whether any paragraph-level construct (heading or list line) is present.
    pub fn has_blocks(&self) -> bool {
        !self.headings.is_empty() || !self.list_runs.is_empty()
    }
}

/// What one source line turns into at paragraph level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Normal,
    Heading(i64),
    List(ListKind),
}

fn indent_level(indent: &str) -> usize {
    let tabs = indent.matches('\t').count();
    let spaces = indent.chars().count() - tabs;
    (tabs + spaces / 2).min(MAX_NESTING)
}

/// One raw line -> (kind, the leading tabs that encode list nesting, the content still holding
/// inline markup).
fn classify(raw: &str) -> (Kind, String, &str) {
    // Named groups always participate when their alternative matched, so a missing one is
    // unreachable; "" / no indent is the inert fallback.
    fn group<'t>(c: &regex::Captures<'t>, name: &str) -> &'t str {
        c.name(name).map_or("", |m| m.as_str())
    }
    if let Some(c) = HEADING_RE.as_ref().and_then(|re| re.captures(raw)) {
        // '#{1,6}' caps the level, so '####### seven' is not a heading at all.
        let level = c.get(1).map_or(0, |m| m.as_str().chars().count() as i64);
        let content = c.get(2).map_or("", |m| m.as_str());
        return (Kind::Heading(level), String::new(), content);
    }
    if let Some(c) = BULLET_RE.as_ref().and_then(|re| re.captures(raw)) {
        let prefix = "\t".repeat(indent_level(group(&c, "indent")));
        return (Kind::List(ListKind::Bullet), prefix, group(&c, "content"));
    }
    if let Some(c) = NUMBER_RE.as_ref().and_then(|re| re.captures(raw)) {
        let prefix = "\t".repeat(indent_level(group(&c, "indent")));
        return (Kind::List(ListKind::Number), prefix, group(&c, "content"));
    }
    (Kind::Normal, String::new(), raw)
}

/// One line's inline markup -> (plain text, spans). Nested emphasis recurses; the outer span
/// and each inner span become separate (overlapping) style requests.
fn parse_inline(src: &str, depth: u32) -> (String, Vec<Span>) {
    let mut spans: Vec<Span> = Vec::new();
    let Some(re) = INLINE_RE.as_ref().filter(|_| depth < MAX_INLINE_DEPTH) else {
        return (src.to_string(), spans);
    };
    let mut text = String::new();
    let mut out: i64 = 0;
    let mut pos = 0usize;
    for caps in re.captures_iter(src) {
        // A backtrack-limit error is only reachable on pathological input; end the scan there
        // and leave the rest of the line literal.
        let Ok(caps) = caps else { break };
        let Some(whole) = caps.get(0) else { break };
        let literal = &src[pos..whole.start()];
        text.push_str(literal);
        out += u16len(literal);
        let Some((styles, inner)) =
            GROUP_STYLES.iter().find_map(|(g, styles)| caps.name(g).map(|m| (*styles, m.as_str())))
        else {
            break;
        };
        let (inner_plain, inner_spans) = parse_inline(inner, depth + 1);
        let width = u16len(&inner_plain);
        // Push order is load-bearing: the outer span first, then the inner ones it overlaps —
        // style_requests emits the spans in exactly this order.
        spans.push((out, out + width, styles.to_vec()));
        for (s, e, style) in inner_spans {
            spans.push((out + s, out + e, style));
        }
        text.push_str(&inner_plain);
        out += width;
        pos = whole.end();
    }
    text.push_str(&src[pos..]);
    (text, spans)
}

/// Merge consecutive lines whose `key(kind)` is set (and equal) into (start, end, key) runs.
fn runs<K: Copy + PartialEq>(
    metas: &[(i64, i64, Kind)],
    key: impl Fn(Kind) -> Option<K>,
) -> Vec<(i64, i64, K)> {
    let mut out: Vec<(i64, i64, K)> = Vec::new();
    for &(s, e, kind) in metas {
        let Some(k) = key(kind) else { continue };
        if s == e {
            continue; // an empty final line — nothing to style
        }
        match out.last_mut() {
            Some(last) if last.1 == s && last.2 == k => last.1 = e,
            _ => out.push((s, e, k)),
        }
    }
    out
}

/// Parse the dialect into insertable plain text plus style metadata (UTF-16 offsets).
///
/// Line ranges include their trailing newline (except the final line, whose paragraph is
/// closed by whatever follows the insertion point), so runs over blank lines stay contiguous
/// and empty paragraphs still get styled.
pub fn parse_markdown(md: &str) -> ParsedMarkdown {
    let lines: Vec<&str> = md.split('\n').collect();
    let mut parts: Vec<String> = Vec::with_capacity(lines.len());
    let mut spans: Vec<Span> = Vec::new();
    let mut metas: Vec<(i64, i64, Kind)> = Vec::new(); // (start, end incl. trailing newline, kind)
    let mut list_items = 0i64;
    let mut off = 0i64;
    for (n, raw) in lines.iter().enumerate() {
        let (kind, prefix, content) = classify(raw);
        if matches!(kind, Kind::List(_)) {
            list_items += 1;
        }
        let (plain, inline) = parse_inline(content, 0);
        let start = off;
        let text_start = off + prefix.chars().count() as i64; // tabs are one UTF-16 unit each
        for (s, e, style) in inline {
            spans.push((text_start + s, text_start + e, style));
        }
        let end_of_text = text_start + u16len(&plain);
        let last = n == lines.len() - 1;
        off = if last { end_of_text } else { end_of_text + 1 }; // +1: the '\n' separator
        metas.push((start, off, kind));
        parts.push(format!("{prefix}{plain}"));
    }

    ParsedMarkdown {
        text: parts.join("\n"),
        spans,
        headings: metas
            .iter()
            .filter_map(|&(s, e, k)| match k {
                Kind::Heading(level) if s < e => Some((s, e, level)),
                _ => None,
            })
            .collect(),
        normal_runs: runs(&metas, |k| (k == Kind::Normal).then_some(()))
            .into_iter()
            .map(|(s, e, ())| (s, e))
            .collect(),
        nonlist_runs: runs(&metas, |k| (!matches!(k, Kind::List(_))).then_some(()))
            .into_iter()
            .map(|(s, e, ())| (s, e))
            .collect(),
        list_runs: runs(&metas, |k| match k {
            Kind::List(kind) => Some(kind),
            _ => None,
        }),
        list_items,
    }
}

/// The batchUpdate styling requests for parsed markdown inserted at index `base`.
///
/// Paragraph-level styling only happens when the markdown has block constructs; inline-only
/// markdown never restyles the paragraphs it lands in. Request order is load-bearing: style
/// updates first (they never move indexes), createParagraphBullets last and bottom-up,
/// because it consumes the leading nesting tabs, shifting every index past the consumed run.
pub fn style_requests(parsed: &ParsedMarkdown, base: i64, tab_id: Option<&str>) -> Vec<Value> {
    // Python tested `if tab_id:`, so an empty tab id contributes no tabId key either.
    let tab = tab_id.filter(|t| !t.is_empty());
    let rng = |s: i64, e: i64| -> Value {
        let mut r = Map::new();
        r.insert("startIndex".into(), json!(base + s));
        r.insert("endIndex".into(), json!(base + e));
        if let Some(t) = tab {
            r.insert("tabId".into(), json!(t));
        }
        Value::Object(r)
    };

    let mut reqs: Vec<Value> = Vec::new();
    for (s, e, styles) in &parsed.spans {
        let mut text_style = Map::new();
        for name in styles {
            text_style.insert((*name).to_string(), Value::Bool(true));
        }
        reqs.push(json!({
            "updateTextStyle": {
                "range": rng(*s, *e),
                "textStyle": Value::Object(text_style),
                "fields": styles.join(","),
            }
        }));
    }
    if parsed.has_blocks() {
        for &(s, e) in &parsed.normal_runs {
            reqs.push(para_style(rng(s, e), "NORMAL_TEXT"));
        }
        for &(s, e, level) in &parsed.headings {
            reqs.push(para_style(rng(s, e), &format!("HEADING_{level}")));
        }
        // Inserted paragraphs inherit the insertion point's list membership; strip it so
        // non-list markdown lines don't continue a pre-existing bulleted/numbered list.
        for &(s, e) in &parsed.nonlist_runs {
            reqs.push(json!({"deleteParagraphBullets": {"range": rng(s, e)}}));
        }
    }
    let mut lists = parsed.list_runs.clone();
    lists.sort_by(|a, b| b.cmp(a)); // Python's sorted(..., reverse=True) over (start, end, kind)
    for (s, e, kind) in lists {
        reqs.push(json!({
            "createParagraphBullets": {"range": rng(s, e), "bulletPreset": kind.preset()}
        }));
    }
    reqs
}

fn para_style(rng: Value, named_style: &str) -> Value {
    json!({
        "updateParagraphStyle": {
            "range": rng,
            "paragraphStyle": {"namedStyleType": named_style},
            "fields": "namedStyleType",
        }
    })
}

// ---- pipe tables (Option B) --------------------------------------------------
// Markdown is split into ordered text/table segments. A table is a pipe row immediately
// followed by a delimiter row (GFM) — requiring the delimiter keeps a lone '| a | b |' line
// literal text, so table parsing never changes existing table-free markdown writes. '\|' is the
// one escape the dialect honors, and only inside table cells, so a cell can carry a literal '|'
// (this closes the read->write loop with the reader, which emits '\|' for pipes in cell text).

static DELIM_CELL_RE: LazyLock<Option<regex::Regex>> = LazyLock::new(|| regex::Regex::new(r"^:?-+:?$").ok());
/// Lookbehind again: the backtracking engine is what lets '\|' hide a pipe from the splitter.
static UNESCAPED_PIPE_RE: LazyLock<Option<fancy_regex::Regex>> =
    LazyLock::new(|| fancy_regex::Regex::new(r"(?<!\\)\|").ok());

fn has_pipe(line: &str) -> bool {
    let hit = UNESCAPED_PIPE_RE.as_ref().and_then(|re| re.find(line).ok());
    hit.flatten().is_some()
}

/// An ordered piece of markdown: prose, or a pipe table's raw cell source.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Text(String),
    Table(Vec<Vec<String>>),
}

/// Split markdown into ordered text/table segments.
///
/// A table segment holds raw cell source (inline markup preserved for later styling); body rows
/// are padded/truncated to the header's column count. A single-row table (header + delimiter, no
/// body rows) yields exactly one row. Text runs between tables are joined with newlines; empty
/// runs are still emitted and skipped by the renderer.
pub fn split_blocks(md: &str) -> Vec<Segment> {
    let lines: Vec<&str> = md.split('\n').collect();
    let n = lines.len();
    let mut segments: Vec<Segment> = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < n {
        if has_pipe(lines[i]) && i + 1 < n && is_delimiter(lines[i + 1]) {
            if !buf.is_empty() {
                segments.push(Segment::Text(buf.join("\n")));
                buf.clear();
            }
            let header = split_row(lines[i]);
            let ncols = header.len();
            let mut rows = vec![header];
            i += 2; // consume header + delimiter
            while i < n && has_pipe(lines[i]) && !is_delimiter(lines[i]) {
                let mut cells = split_row(lines[i]);
                cells.resize(ncols, String::new()); // pad short rows, truncate wide ones
                rows.push(cells);
                i += 1;
            }
            segments.push(Segment::Table(rows));
        } else {
            buf.push(lines[i]);
            i += 1;
        }
    }
    if !buf.is_empty() {
        segments.push(Segment::Text(buf.join("\n")));
    }
    segments
}

pub fn has_table(segments: &[Segment]) -> bool {
    segments.iter().any(|s| matches!(s, Segment::Table(_)))
}

/// A pipe-table row -> cells: split on unescaped '|', drop the bounding empties, unescape '\|'.
pub fn split_row(line: &str) -> Vec<String> {
    let Some(re) = UNESCAPED_PIPE_RE.as_ref() else {
        return Vec::new();
    };
    let parts: Vec<&str> = re.split(strip(line)).flatten().collect();
    let mut parts = &parts[..];
    if parts.first().is_some_and(|p| strip(p).is_empty()) {
        parts = &parts[1..];
    }
    if parts.last().is_some_and(|p| strip(p).is_empty()) {
        parts = &parts[..parts.len() - 1];
    }
    parts.iter().map(|p| strip(p).replace("\\|", "|")).collect()
}

/// Python's `str.strip()`, which counts the C0 separators U+001C..U+001F as whitespace where
/// Rust's `trim` does not. A stray one at the start of a row would otherwise survive the trim,
/// stop the leading empty from being dropped, and shift every column by one.
fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

fn is_delimiter(line: &str) -> bool {
    if !has_pipe(line) {
        return false;
    }
    let Some(re) = DELIM_CELL_RE.as_ref() else {
        return false;
    };
    let cells = split_row(line);
    !cells.is_empty() && cells.iter().all(|c| re.is_match(c))
}

/// A table cell's source -> (plain text, inline style spans) — same inline dialect as prose.
pub fn parse_cell(src: &str) -> (String, Vec<Span>) {
    parse_inline(src, 0)
}

/// One cell value as Python's `str()` would render it (`None` -> "", `True` -> "True").
pub fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64().filter(|_| n.is_f64()) {
            Some(f) => python_float(f),
            // An integer renders identically either way.
            None => n.to_string(),
        },
        // A list/object has no Python repr here, so its JSON form is the honest rendering.
        other => other.to_string(),
    }
}

/// `repr(float)` as CPython writes it, which is what lands in a spilled CSV.
///
/// Both languages print the shortest string that round-trips, but they disagree on the
/// presentation: CPython switches to exponential form at a decimal exponent below -4 (Rust holds
/// out longer, so `1e-05` comes out as `0.00001`) and always writes the exponent signed and
/// two-digit (`1e-07`, not `1e-7`).
fn python_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() { "-inf".to_string() } else { "inf".to_string() };
    }
    // `{:e}` is the same shortest round-trip digits, already split into mantissa and exponent.
    let sci = format!("{f:e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if !(-4..16).contains(&exponent) {
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    // Rust's Display never uses exponent notation, so this branch is always positional; it just
    // omits the trailing ".0" that marks a float in Python.
    let positional = format!("{f}");
    match positional.contains('.') {
        true => positional,
        false => format!("{positional}.0"),
    }
}

/// Render one cell value for markdown output: flatten newlines, escape '|' so it stays one cell.
pub fn escape_cell(value: &Value) -> String {
    cell_text(value).replace('\n', " ").replace('|', "\\|")
}

/// Rows -> a GFM pipe table (header + column-matched delimiter + body), cells escaped.
pub fn render_table_markdown(rows: &[Vec<Value>]) -> String {
    let ncols = rows.first().map_or(0, Vec::len);
    let mut out: Vec<String> = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let cells: Vec<String> = row.iter().map(escape_cell).collect();
        out.push(format!("| {} |", cells.join(" | ")));
        if r == 0 {
            out.push(format!("| {} |", vec!["---"; ncols].join(" | ")));
        }
    }
    out.join("\n")
}

/// A plain-text preview of segmented markdown (tables shown as pipe rows) for dry-run.
pub fn render_markdown_preview(segments: &[Segment]) -> String {
    // Cells hold decoded source, so rendering re-escapes the pipes the splitter ate.
    let row_values = |r: &Vec<String>| -> Vec<Value> { r.iter().map(|c| json!(c)).collect() };
    let mut parts: Vec<String> = Vec::new();
    for segment in segments {
        match segment {
            Segment::Table(rows) => {
                let rows: Vec<Vec<Value>> = rows.iter().map(row_values).collect();
                parts.push(render_table_markdown(&rows));
            }
            Segment::Text(text) => parts.push(parse_markdown(text).text),
        }
    }
    parts.retain(|p| !p.is_empty());
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: i64, e: i64) -> Span {
        (s, e, vec!["bold"])
    }
    fn it(s: i64, e: i64) -> Span {
        (s, e, vec!["italic"])
    }
    fn un(s: i64, e: i64) -> Span {
        (s, e, vec!["underline"])
    }
    fn bi(s: i64, e: i64) -> Span {
        (s, e, vec!["bold", "italic"])
    }

    /// The request kinds in order — each request is a single-key object, like Python's
    /// `next(iter(r))`.
    fn kinds(reqs: &[Value]) -> Vec<String> {
        reqs.iter().filter_map(|r| r.as_object().and_then(|o| o.keys().next()).cloned()).collect()
    }

    #[test]
    fn u16len_counts_utf16_units_not_chars() {
        assert_eq!(u16len("abc"), 3);
        assert_eq!(u16len("😀"), 2); // astral-plane char: 2 UTF-16 units, one char
    }

    #[test]
    fn a_heading_line_owns_its_trailing_newline() {
        let p = parse_markdown("# Title\nbody");
        assert_eq!(p.text, "Title\nbody");
        assert_eq!(p.headings, vec![(0, 6, 1)]); // includes the trailing newline
        assert_eq!(p.normal_runs, vec![(6, 10)]);
        assert_eq!(p.nonlist_runs, vec![(0, 10)]); // heading + body merge into one non-list run
        assert!(p.list_runs.is_empty() && p.has_blocks());
    }

    #[test]
    fn only_one_to_six_hashes_open_a_heading() {
        assert_eq!(parse_markdown("### Deep").headings, vec![(0, 4, 3)]);
        assert!(parse_markdown("####### seven").headings.is_empty());
    }

    #[test]
    fn two_spaces_of_indent_become_one_nesting_tab() {
        let p = parse_markdown("- a\n  - b\n- c");
        assert_eq!(p.text, "a\n\tb\nc"); // two-space indent -> one leading tab
        assert_eq!(p.list_runs, vec![(0, 6, ListKind::Bullet)]); // one run despite nesting
        assert_eq!(p.list_items, 3);
        assert!(p.normal_runs.is_empty() && p.has_blocks());
    }

    #[test]
    fn nesting_saturates_at_eight_levels() {
        let capped = "\t".repeat(8) + "x";
        assert_eq!(parse_markdown(&(" ".repeat(20) + "- x")).text, capped);
        assert_eq!(parse_markdown(&("\t".repeat(10) + "- x")).text, capped);
    }

    #[test]
    fn a_bullet_run_and_a_numbered_run_never_merge() {
        let p = parse_markdown("1. x\n2) y");
        assert_eq!(p.text, "x\ny");
        assert_eq!(p.list_runs, vec![(0, 3, ListKind::Number)]);
        let mixed = parse_markdown("- a\n1. b").list_runs;
        assert_eq!(mixed, vec![(0, 2, ListKind::Bullet), (2, 3, ListKind::Number)]);
    }

    #[test]
    fn inline_markers_are_stripped_and_their_spans_recorded() {
        let p = parse_markdown("**b** *i* <u>u</u>");
        assert_eq!(p.text, "b i u");
        assert_eq!(p.spans, vec![b(0, 1), it(2, 3), un(4, 5)]);
        assert!(!p.has_blocks()); // inline-only markdown has no block constructs
    }

    #[test]
    fn nested_emphasis_pushes_the_outer_span_before_the_inner_one() {
        let p = parse_markdown("**b *i* b**");
        assert_eq!(p.text, "b i b");
        assert_eq!(p.spans, vec![b(0, 5), it(2, 3)]); // outer and inner overlap
        assert_eq!(parse_markdown("***x***").spans, vec![bi(0, 1)]);
        let underscores = parse_markdown("__b__ and _i_").spans;
        assert_eq!(underscores, vec![b(0, 1), it(6, 7)]);
    }

    #[test]
    fn inline_offsets_count_an_emoji_as_two_units() {
        let p = parse_markdown("😀 **b**");
        assert_eq!(p.text, "😀 b");
        assert_eq!(p.spans, vec![b(3, 4)]); // emoji occupies units 0-1
    }

    #[test]
    fn markup_lookalikes_stay_literal() {
        assert!(parse_markdown("snake_case_name").spans.is_empty()); // intraword '_'
        assert!(parse_markdown("2 * 3 * 4").spans.is_empty()); // space-padded '*'
        assert_eq!(parse_markdown("**unclosed").text, "**unclosed");
        assert!(parse_markdown("*text*").list_runs.is_empty()); // no space: emphasis, not bullet
        let bulleted = parse_markdown("* text").list_runs;
        assert_eq!(bulleted, vec![(0, 4, ListKind::Bullet)]);
    }

    /// Ground truth captured from the Python `md.parse_cell`; the Rust engine must agree
    /// match-for-match, including where the lookaround refuses to match.
    #[test]
    fn the_inline_dialect_matches_the_python_regex_case_for_case() {
        let cases: Vec<(&str, &str, Vec<Span>)> = vec![
            ("a * b", "a * b", vec![]),
            ("2 * 3 * 4", "2 * 3 * 4", vec![]),
            ("snake_case_name", "snake_case_name", vec![]),
            ("**unclosed", "**unclosed", vec![]),
            ("*text*", "text", vec![it(0, 4)]),
            ("* text", "* text", vec![]),
            ("_i_", "i", vec![it(0, 1)]),
            ("a_b_c", "a_b_c", vec![]),
            ("** b**", "** b**", vec![]),
            ("**b **", "**b **", vec![]),
            ("***a** b*", "*a b*", vec![b(0, 2)]),
            ("_ x _", "_ x _", vec![]),
            ("__b__x", "__b__x", vec![]),
            ("x__b__", "x__b__", vec![]),
            ("__a____b__", "a____b", vec![b(0, 6)]),
            ("__a__b__c__", "a__b__c", vec![b(0, 7)]),
            ("**a**__b__", "ab", vec![b(0, 1), b(1, 2)]),
            ("*a*_b_", "ab", vec![it(0, 1), it(1, 2)]),
            ("**a**b**c**", "abc", vec![b(0, 1), b(2, 3)]),
            ("*a*b*c*", "abc", vec![it(0, 1), it(2, 3)]),
            ("**_both_**", "both", vec![b(0, 4), it(0, 4)]),
            ("_**both**_", "both", vec![it(0, 4), b(0, 4)]),
            ("*a **b** c*", "*a b c*", vec![b(3, 4)]),
            ("<u>**x**</u>", "x", vec![un(0, 1), b(0, 1)]),
            ("<u>x</u> and <u>y</u>", "x and y", vec![un(0, 1), un(6, 7)]),
            ("a**b**c", "abc", vec![b(1, 2)]),
            ("***x**y*", "*xy*", vec![b(0, 2)]),
            ("**x*y***", "x*y*", vec![b(0, 3)]),
            ("😀**b**😀", "😀b😀", vec![b(2, 3)]),
            ("**a\tb**", "a\tb", vec![b(0, 3)]),
            ("**", "**", vec![]),
            ("***", "***", vec![]),
            ("____", "____", vec![]),
            ("_a__b_", "_a__b_", vec![]),
            ("<u></u>", "<u></u>", vec![]),
            ("<u> </u>", " ", vec![un(0, 1)]),
            ("**a *b* c *d* e**", "a b c d e", vec![b(0, 9), it(2, 3), it(6, 7)]),
            ("***bold italic*** and **b**", "bold italic and b", vec![bi(0, 11), b(16, 17)]),
            ("a | b", "a | b", vec![]),
            (r"a \| b", r"a \| b", vec![]),
        ];
        for (src, text, spans) in cases {
            assert_eq!(parse_cell(src), (text.to_string(), spans), "input {src:?}");
        }
    }

    /// The cases where the two regex engines' own `\w` and `\s` disagree, so the pattern spells
    /// both classes out. Ground truth captured from Python's `md._parse_inline`.
    #[test]
    fn the_word_and_space_classes_follow_python_not_rust() {
        const FS: &str = "\u{1c}"; // a C0 separator: whitespace to Python's re, not to Rust's
        const ZWJ: &str = "\u{200d}"; // Cf: a word char to Rust's \w, not to Python's
        const ACUTE: &str = "\u{301}"; // Mn: likewise
        let cases: Vec<(String, String, Vec<Span>)> = vec![
            // A combining mark is not a word character, so the emphasis still opens.
            (format!("e{ACUTE}_x_"), format!("e{ACUTE}x"), vec![it(2, 3)]),
            (format!("_x_{ACUTE}"), format!("x{ACUTE}"), vec![it(0, 1)]),
            (format!("a{ZWJ}_x_"), format!("a{ZWJ}x"), vec![it(2, 3)]),
            // ...but a digit in any Unicode numeric category is, so these stay literal.
            ("²_x_".into(), "²_x_".into(), vec![]),
            ("٣_x_".into(), "٣_x_".into(), vec![]),
            // A C0 separator counts as whitespace, so it blocks an opener and a closer.
            (format!("**{FS}a**"), format!("**{FS}a**"), vec![]),
            (format!("*a{FS}*"), format!("*a{FS}*"), vec![]),
            // ...but is unremarkable in the middle.
            (format!("**a{FS}b**"), format!("a{FS}b"), vec![b(0, 3)]),
        ];
        for (src, text, spans) in cases {
            assert_eq!(parse_cell(&src), (text, spans), "input {src:?}");
        }
    }

    #[test]
    fn a_long_line_is_still_styled_to_its_end() {
        // fancy-regex's default backtrack budget is exhaustible; Python's re has no such ceiling,
        // and running out used to abandon the scan and leave the markup literal in the document.
        let line = format!("{} **bold**", "word ".repeat(1200));
        let (plain, spans) = parse_cell(&line);
        assert!(plain.ends_with(" bold"), "markers survived: {:?}", &plain[plain.len() - 20..]);
        assert_eq!(spans.len(), 1, "the trailing bold span was dropped");
    }

    #[test]
    fn a_trailing_newline_makes_no_empty_final_run() {
        let p = parse_markdown("- a\n");
        assert_eq!(p.text, "a\n");
        assert_eq!(p.list_runs, vec![(0, 2, ListKind::Bullet)]);
        // the empty final line is skipped
        assert!(p.normal_runs.is_empty() && p.nonlist_runs.is_empty());
    }

    #[test]
    fn a_blank_line_splits_list_runs_but_is_still_normalized() {
        let p = parse_markdown("- a\n\n- b");
        assert_eq!(p.text, "a\n\nb");
        assert_eq!(p.list_runs, vec![(0, 2, ListKind::Bullet), (3, 4, ListKind::Bullet)]);
        assert_eq!(p.normal_runs, vec![(2, 3)]); // the blank paragraph still gets normalized
    }

    #[test]
    fn bullet_creation_comes_last_and_bottom_up() {
        let reqs = style_requests(&parse_markdown("- a\n\n- b"), 10, Some("t.1"));
        // styles first, bullet creation last (it consumes nesting tabs, shifting later indexes)
        assert_eq!(
            kinds(&reqs),
            vec![
                "updateParagraphStyle",
                "deleteParagraphBullets",
                "createParagraphBullets",
                "createParagraphBullets",
            ]
        );
        assert_eq!(
            reqs[0]["updateParagraphStyle"]["range"],
            json!({"startIndex": 12, "endIndex": 13, "tabId": "t.1"})
        );
        let creates: Vec<&Value> = reqs[2..].iter().filter_map(|r| r.get("createParagraphBullets")).collect();
        let starts: Vec<&Value> = creates.iter().map(|c| &c["range"]["startIndex"]).collect();
        assert_eq!(starts, vec![&json!(13), &json!(10)]); // bottom-up
        assert_eq!(creates[0]["bulletPreset"], "BULLET_DISC_CIRCLE_SQUARE");
    }

    #[test]
    fn inline_only_markdown_never_touches_paragraphs() {
        let reqs = style_requests(&parse_markdown("**b**"), 5, None);
        assert_eq!(kinds(&reqs), vec!["updateTextStyle"]);
        let st = &reqs[0]["updateTextStyle"];
        assert_eq!(st["range"], json!({"startIndex": 5, "endIndex": 6})); // no tabId key
        assert_eq!(st["textStyle"], json!({"bold": true}));
        assert_eq!(st["fields"], "bold");
    }

    #[test]
    fn a_style_tuples_order_survives_into_the_fields_string() {
        let reqs = style_requests(&parse_markdown("***x***"), 0, None);
        let st = &reqs[0]["updateTextStyle"];
        assert_eq!(st["fields"], "bold,italic");
        assert_eq!(st["textStyle"], json!({"bold": true, "italic": true}));
    }

    #[test]
    fn an_empty_tab_id_adds_no_tab_id_key() {
        let reqs = style_requests(&parse_markdown("**b**"), 0, Some(""));
        let range = &reqs[0]["updateTextStyle"]["range"];
        assert_eq!(range, &json!({"startIndex": 0, "endIndex": 1}));
    }

    #[test]
    fn headings_and_numbered_lists_carry_their_preset_and_fields() {
        let reqs = style_requests(&parse_markdown("# H\n1. one"), 1, None);
        let first = |kind: &str| reqs.iter().find_map(|r| r.get(kind)).unwrap().clone();
        let para = first("updateParagraphStyle");
        assert_eq!(para["paragraphStyle"], json!({"namedStyleType": "HEADING_1"}));
        assert_eq!(para["fields"], "namedStyleType");
        let bullets = first("createParagraphBullets");
        assert_eq!(bullets["bulletPreset"], "NUMBERED_DECIMAL_ALPHA_ROMAN");
    }

    #[test]
    fn every_text_style_request_precedes_the_paragraph_requests() {
        let reqs = style_requests(&parse_markdown("# **H**\n- *a*\ntail"), 0, Some("t.0"));
        assert_eq!(
            kinds(&reqs),
            vec![
                "updateTextStyle",
                "updateTextStyle",
                "updateParagraphStyle", // normal_runs
                "updateParagraphStyle", // headings
                "deleteParagraphBullets",
                "deleteParagraphBullets",
                "createParagraphBullets",
            ]
        );
    }

    #[test]
    fn deleting_bullets_covers_every_non_list_run() {
        let reqs = style_requests(&parse_markdown("# H\n- a\ntail"), 0, None);
        let ranges: Vec<&Value> =
            reqs.iter().filter_map(|r| r.get("deleteParagraphBullets")).map(|r| &r["range"]).collect();
        // heading (0..2) and the trailing paragraph (4..8) — the list line in between is skipped
        assert_eq!(
            ranges,
            vec![&json!({"startIndex": 0, "endIndex": 2}), &json!({"startIndex": 4, "endIndex": 8}),]
        );
    }

    // ---- pipe tables -----------------------------------------------------------------

    fn text(s: &str) -> Segment {
        Segment::Text(s.to_string())
    }

    fn table(rows: &[&[&str]]) -> Segment {
        let cells = |r: &&[&str]| r.iter().map(|c| c.to_string()).collect();
        Segment::Table(rows.iter().map(cells).collect())
    }

    #[test]
    fn markdown_without_a_delimiter_row_is_one_text_segment() {
        let segs = split_blocks("# H\n- a");
        assert_eq!(segs, vec![text("# H\n- a")]);
        assert!(!has_table(&segs));
    }

    #[test]
    fn a_pipe_row_with_no_delimiter_row_stays_literal_text() {
        assert_eq!(split_blocks("| a | b |"), vec![text("| a | b |")]);
    }

    #[test]
    fn a_header_delimiter_and_body_become_one_table_segment() {
        let segs = split_blocks("| Name | Role |\n| --- | --- |\n| Ada | Eng |");
        assert_eq!(segs, vec![table(&[&["Name", "Role"], &["Ada", "Eng"]])]);
        assert!(has_table(&segs));
    }

    #[test]
    fn a_table_with_no_body_rows_still_yields_its_header_row() {
        let segs = split_blocks("| a | b |\n| --- | --- |");
        assert_eq!(segs, vec![table(&[&["a", "b"]])]);
    }

    #[test]
    fn text_around_a_table_stays_in_order_and_alignment_colons_are_delimiters() {
        let segs = split_blocks("intro\n| A | B |\n| :-- | --: |\n| 1 | 2 |\nouttro");
        let want = vec![text("intro"), table(&[&["A", "B"], &["1", "2"]]), text("outtro")];
        assert_eq!(segs, want);
    }

    #[test]
    fn body_rows_are_padded_and_truncated_to_the_header_width() {
        let segs = split_blocks("| a | b | c |\n| --- | --- | --- |\n| 1 |\n| 1 | 2 | 3 | 4 |");
        let want = table(&[&["a", "b", "c"], &["1", "", ""], &["1", "2", "3"]]);
        assert_eq!(segs, vec![want]);
    }

    #[test]
    fn empty_markdown_still_emits_one_empty_text_segment() {
        assert_eq!(split_blocks(""), vec![text("")]);
    }

    #[test]
    fn an_escaped_pipe_is_not_a_cell_boundary() {
        assert_eq!(split_row(r"| a \| b | c |"), vec!["a | b", "c"]);
    }

    #[test]
    fn a_row_of_bare_pipes_has_no_cells() {
        assert_eq!(split_row("|"), Vec::<String>::new());
        assert_eq!(split_row("| a | b |"), vec!["a", "b"]);
        assert_eq!(split_row("a"), vec!["a"]); // no pipe at all: the whole line is one cell
    }

    #[test]
    fn an_escaped_pipe_round_trips_through_the_reader_format() {
        let emitted = render_table_markdown(&[vec![json!("a | b"), json!("c")]]);
        let header = emitted.lines().next().unwrap_or_default();
        assert_eq!(header, r"| a \| b | c |");
        assert_eq!(split_row(header), vec!["a | b", "c"]);
    }

    #[test]
    fn render_table_markdown_emits_a_column_matched_delimiter_row() {
        let header = vec![json!("H1"), json!("H2")];
        let body = vec![json!("x"), json!("y")];
        let out = render_table_markdown(&[header, body]);
        assert_eq!(out, "| H1 | H2 |\n| --- | --- |\n| x | y |");
        assert_eq!(render_table_markdown(&[]), "");
    }

    #[test]
    fn a_cell_is_styled_with_the_same_inline_dialect_as_prose() {
        assert_eq!(parse_cell("**b** x"), ("b x".to_string(), vec![b(0, 1)]));
    }

    #[test]
    fn cell_values_render_the_way_pythons_str_does() {
        assert_eq!(cell_text(&Value::Null), "");
        assert_eq!(cell_text(&json!(true)), "True");
        assert_eq!(cell_text(&json!(false)), "False");
        assert_eq!(cell_text(&json!(1)), "1");
        assert_eq!(cell_text(&json!(2.5)), "2.5");
        assert_eq!(cell_text(&json!("x")), "x");
    }

    /// Ground truth from CPython's `repr`, which is what `str()` on a float gives.
    #[test]
    fn floats_render_with_pythons_exponent_rules() {
        for (value, expected) in [
            (1e-7, "1e-07"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1.0, "1.0"),
            (1.5, "1.5"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (123456789012345678.0, "1.2345678901234568e+17"),
            (2.5e-10, "2.5e-10"),
            (-3e20, "-3e+20"),
            (0.0, "0.0"),
            (-0.5, "-0.5"),
        ] {
            assert_eq!(cell_text(&json!(value)), expected, "for {value}");
        }
        // An integer stays an integer — no spurious ".0".
        assert_eq!(cell_text(&json!(1500000000000000000i64)), "1500000000000000000");
    }

    #[test]
    fn escaping_a_cell_flattens_newlines_and_hides_pipes() {
        assert_eq!(escape_cell(&json!("a\nb|c")), r"a b\|c");
        assert_eq!(escape_cell(&Value::Null), "");
        let row = vec![json!(1), Value::Null, json!(true)];
        let out = render_table_markdown(&[row]);
        assert_eq!(out, "| 1 |  | True |\n| --- | --- | --- |");
    }

    #[test]
    fn the_preview_renders_tables_as_pipes_and_drops_empty_parts() {
        let segments = vec![text(""), table(&[&["a", "b"]]), text("# H")];
        let preview = render_markdown_preview(&segments);
        assert_eq!(preview, "| a | b |\n| --- | --- |\nH");
    }
}
