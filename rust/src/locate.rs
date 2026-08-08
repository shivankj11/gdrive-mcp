//! Locators -> Docs index ranges: resolve human-meaningful anchors to API offsets.
//!
//! The Docs API's Range/Location indexes are UTF-16 code units in a per-tab index space, and the
//! markdown a caller reads back is lossy (heading prefixes, a synthesized table delimiter row,
//! escaped pipes, stripped trailing newlines). A caller therefore can never compute a valid index
//! from what it read. These helpers resolve a locator against the API's *own* element offsets
//! instead, so no offset is ever derived from rendered text.
//!
//! Two locator forms:
//!   - match:   a literal substring, resolved per-paragraph (including paragraphs inside table
//!              cells). A needle spanning a paragraph break never matches — the same limit the
//!              API's own replaceAllText has, and what keeps a range from straddling a table cell
//!              boundary (which deleteContentRange rejects).
//!   - section: a heading's text, resolved to that heading plus everything under it, up to the
//!              next heading of the same or higher level.
//!
//! Pure index math over already-fetched body content: no service calls, no I/O. `body` is the
//! Docs API's `body.content` array, as `serde_json::Value` elements.

use serde_json::Value;

use crate::error::{Result, ToolError};
use crate::md::u16len;

/// (offset into the paragraph's flat text, document index of the run starting there) — both UTF-16.
type Run = (i64, i64);

/// An element's index field. The API omits proto3 defaults, so an absent index reads as 0.
fn index_of(el: &Value, key: &str) -> i64 {
    el.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn array(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Every paragraph element in document order, descending into table cells.
fn walk_paragraphs(content: &[Value]) -> Vec<&Value> {
    let mut out = Vec::new();
    collect_paragraphs(content, &mut out);
    out
}

fn collect_paragraphs<'a>(content: &'a [Value], out: &mut Vec<&'a Value>) {
    for el in content {
        if el.get("paragraph").is_some() {
            out.push(el);
        } else if let Some(table) = el.get("table") {
            for row in array(table.get("tableRows")) {
                for cell in array(row.get("tableCells")) {
                    collect_paragraphs(array(cell.get("content")), out);
                }
            }
        }
    }
}

/// A paragraph's flat text plus the breakpoints mapping flat offsets to document indexes.
///
/// Each text run records its own startIndex, so elements that occupy index space without
/// contributing text (inline images, footnote references) never desync the mapping.
fn para_runs(el: &Value) -> (String, Vec<Run>) {
    let mut text = String::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut off: i64 = 0;
    for pe in array(el.get("paragraph").and_then(|p| p.get("elements"))) {
        let Some(tr) = pe.get("textRun") else {
            continue;
        };
        let content = tr.get("content").and_then(Value::as_str).unwrap_or("");
        runs.push((off, index_of(pe, "startIndex")));
        text.push_str(content);
        off += u16len(content);
    }
    (text, runs)
}

/// Map a UTF-16 offset in a paragraph's flat text back to a document index.
fn to_doc_index(runs: &[Run], u16_off: i64) -> i64 {
    // Callers skip run-less paragraphs; a paragraph with no runs has no index space to map into.
    let Some(&(mut base_off, mut base_idx)) = runs.first() else {
        return u16_off;
    };
    for &(off, idx) in runs {
        if off > u16_off {
            break;
        }
        base_off = off;
        base_idx = idx;
    }
    base_idx + (u16_off - base_off)
}

/// Every occurrence of `needle` as (start, end) document ranges, in document order.
///
/// Matching is per-paragraph and case-sensitive. Case-insensitive matching is deliberately not
/// offered: folding can change a string's length (`'İ'.lower()` is two characters), which
/// would shift every offset computed from the folded text.
pub fn find_matches(body: &[Value], needle: &str) -> Result<Vec<(i64, i64)>> {
    if needle.is_empty() {
        return Err(ToolError::msg("match must be a non-empty string"));
    }
    let mut out: Vec<(i64, i64)> = Vec::new();
    for el in walk_paragraphs(body) {
        let (text, runs) = para_runs(el);
        if runs.is_empty() {
            continue;
        }
        // `from` is a byte offset, which is all `str::find` needs; the UTF-16 conversion below is
        // what turns it into the unit the Docs index space counts.
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(needle) {
            let pos = from + rel;
            let end = pos + needle.len();
            // u16len(text[..n]) converts a slice bound from str::find into the UTF-16 offset the
            // Docs index space counts — they diverge on astral-plane characters.
            out.push((to_doc_index(&runs, u16len(&text[..pos])), to_doc_index(&runs, u16len(&text[..end]))));
            // Resume after the hit: overlapping occurrences report the non-overlapping ones only.
            from = end;
        }
    }
    Ok(out)
}

fn heading_level(el: &Value) -> Option<i64> {
    let style = el
        .get("paragraph")
        .and_then(|p| p.get("paragraphStyle"))
        .and_then(|s| s.get("namedStyleType"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match style {
        "HEADING_1" => Some(1),
        "HEADING_2" => Some(2),
        "HEADING_3" => Some(3),
        "HEADING_4" => Some(4),
        "HEADING_5" => Some(5),
        "HEADING_6" => Some(6),
        _ => None,
    }
}

/// The last index content may occupy: the body's final newline can never be deleted.
pub fn body_end(body: &[Value]) -> i64 {
    body.last().map_or(1, |el| index_of(el, "endIndex") - 1)
}

/// (start, end) covering a heading paragraph and everything beneath it.
///
/// Ends at the next heading of the same or higher level, else at the end of the tab. Only
/// top-level elements are scanned — a heading inside a table cell is not a section. The range
/// includes the heading's own trailing newline so deleting a section leaves no empty paragraph.
pub fn find_section(body: &[Value], heading: &str) -> Result<(i64, i64)> {
    // (position, level, startIndex, endIndex)
    let mut levels: Vec<(usize, i64, i64, i64)> = Vec::new();
    for (i, el) in body.iter().enumerate() {
        if let Some(level) = heading_level(el) {
            levels.push((i, level, index_of(el, "startIndex"), index_of(el, "endIndex")));
        }
    }

    let hit = levels.iter().find(|x| para_text(&body[x.0]).trim_end_matches('\n') == heading).copied();
    let Some((pos, level, start, end)) = hit else {
        let available: Vec<String> =
            levels.iter().map(|x| format!("'{}'", para_text(&body[x.0]).trim_end_matches('\n'))).collect();
        return Err(ToolError::msg(format!(
            "no heading titled '{heading}'; headings in this tab: [{}]",
            available.join(", ")
        )));
    };

    let stop = levels.iter().find(|x| x.0 > pos && x.1 <= level).map_or_else(|| body_end(body), |x| x.2);
    // Clamp last: a heading that IS the final element has endIndex == body_end + 1, and a range
    // running through the body's final newline is rejected by deleteContentRange.
    Ok((start, end.max(stop).min(body_end(body))))
}

fn para_text(el: &Value) -> String {
    array(el.get("paragraph").and_then(|p| p.get("elements")))
        .iter()
        .filter_map(|pe| pe.get("textRun").and_then(|tr| tr.get("content")).and_then(Value::as_str))
        .collect()
}

/// Resolve a locator to (ranges, description). Exactly one of match/section is required.
///
/// `occurrence` is 1-based over a `match`; 0 means every occurrence. It does not apply to
/// `section` (heading titles resolve to a single span).
pub fn resolve(
    body: &[Value],
    match_text: Option<&str>,
    section: Option<&str>,
    occurrence: i64,
) -> Result<(Vec<(i64, i64)>, String)> {
    let needle = match (match_text, section) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(ToolError::msg("pass exactly one of match= or section="))
        }
        (None, Some(section)) => {
            if occurrence != 1 {
                return Err(ToolError::msg(
                    "occurrence applies to match=, not section= (a heading resolves to one span)",
                ));
            }
            return Ok((vec![find_section(body, section)?], format!("section '{section}'")));
        }
        (Some(match_text), None) => match_text,
    };

    let hits = find_matches(body, needle)?;
    if hits.is_empty() {
        return Err(ToolError::msg(format!(
            "no match for '{needle}' in this tab (matching is case-sensitive and cannot span paragraphs)"
        )));
    }
    if occurrence == 0 {
        let description = format!("all {} occurrences of '{needle}'", hits.len());
        return Ok((hits, description));
    }
    if occurrence < 0 {
        return Err(ToolError::msg(format!("occurrence must be >= 0 (0 means all); got {occurrence}")));
    }
    let Some(&range) = hits.get((occurrence - 1) as usize) else {
        return Err(ToolError::msg(format!(
            "occurrence {occurrence} is out of range: '{needle}' occurs {} time(s)",
            hits.len()
        )));
    };
    Ok((vec![range], format!("occurrence {occurrence} of '{needle}'")))
}

/// The document text covered by [start, end) — the payload a delete/replace preview shows.
///
/// Intersects per text run rather than per paragraph, so index jumps caused by non-text
/// elements inside a paragraph don't skew the slice.
pub fn text_in_range(body: &[Value], start: i64, end: i64) -> String {
    let mut out = String::new();
    for el in walk_paragraphs(body) {
        let (text, runs) = para_runs(el);
        let total = u16len(&text);
        for (i, &(off, idx)) in runs.iter().enumerate() {
            let run_end_off = runs.get(i + 1).map_or(total, |r| r.0);
            let (lo, hi) = (idx.max(start), (idx + (run_end_off - off)).min(end));
            if lo < hi {
                out.push_str(&slice_u16(&text, off + (lo - idx), off + (hi - idx)));
            }
        }
    }
    out
}

/// Slice by UTF-16 offsets rather than code points (they differ past the BMP).
fn slice_u16(s: &str, lo: i64, hi: i64) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len() as i64;
    let lo = lo.clamp(0, len) as usize;
    let hi = hi.clamp(lo as i64, len) as usize;
    // Python decoded with errors="ignore": a bound that splits a surrogate pair drops the orphaned
    // half rather than substituting U+FFFD, so a preview never invents a replacement character.
    char::decode_utf16(units[lo..hi].iter().copied()).flatten().collect()
}

/// (start, end) of the top-level paragraph containing `index`, or None if there isn't one.
///
/// insertTable rejects a location that is not at a paragraph boundary, so block-level inserts
/// snap to one instead of landing mid-paragraph. Returns None when `index` falls inside a table
/// (cell paragraphs are not top-level), which callers must treat as an error rather than
/// silently anchoring mid-cell.
pub fn paragraph_bounds(body: &[Value], index: i64) -> Option<(i64, i64)> {
    body.iter()
        .filter(|el| el.get("paragraph").is_some())
        .map(|el| (index_of(el, "startIndex"), index_of(el, "endIndex")))
        .find(|&(start, end)| start <= index && index < end)
}

#[cfg(test)]
mod tests {
    //! Resolver tests: locator -> Docs index ranges, over synthetic body content.
    //!
    //! Pure index math, no Google service and no fakes. This is where the correctness burden
    //! sits: an offset bug here is silent data loss at the delete/replace tools, and it is the
    //! only layer where the arithmetic is falsifiable without a live document.

    use super::*;
    use serde_json::json;

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

    /// A one-row table whose cells each hold a single paragraph.
    fn table(start: i64, cell_texts: &[&str]) -> Value {
        let mut cells = Vec::new();
        let mut idx = start + 3; // table/row/cell structural offsets precede the first paragraph
        for t in cell_texts {
            cells.push(json!({"content": [para(idx, &[t])]}));
            idx += t.chars().count() as i64 + 2;
        }
        json!({
            "startIndex": start,
            "endIndex": idx,
            "table": {"tableRows": [{"tableCells": cells}]},
        })
    }

    fn sectioned() -> Vec<Value> {
        vec![
            styled(1, &["Intro\n"], Some("HEADING_1")),
            para(7, &["intro body\n"]),
            styled(18, &["Sub\n"], Some("HEADING_2")),
            para(22, &["sub body\n"]),
            styled(31, &["Next\n"], Some("HEADING_1")),
            para(36, &["next body\n"]),
        ]
    }

    fn err(r: Result<impl std::fmt::Debug>) -> String {
        r.unwrap_err().to_string()
    }

    // ---- offset mapping ----------------------------------------------------------------

    #[test]
    fn a_single_run_match_uses_the_runs_own_start_index() {
        let body = vec![para(1, &["hello world\n"])];
        assert_eq!(find_matches(&body, "world").unwrap(), vec![(7, 12)]);
    }

    #[test]
    fn an_astral_char_before_the_needle_resolves_in_utf16_units() {
        // '😀' is one code point but TWO UTF-16 units, which is what Docs indexes count. 'a😀b '
        // is 5 units, so 'needle' starts at 1+5=6. Counting code points gives 5 — off by one,
        // and the delete would eat the preceding space and leave a trailing 'e'.
        let body = vec![para(1, &["a😀b needle\n"])];
        assert_eq!(find_matches(&body, "needle").unwrap(), vec![(6, 12)]);
        assert_eq!(text_in_range(&body, 6, 12), "needle");
    }

    #[test]
    fn a_match_spanning_two_text_runs_in_one_paragraph_resolves() {
        let body = vec![para(1, &["hel", "lo world\n"])];
        assert_eq!(find_matches(&body, "lo wo").unwrap(), vec![(4, 9)]);
    }

    #[test]
    fn a_non_text_element_between_runs_does_not_desync_the_mapping() {
        // An inline image occupies one index but contributes no text; the second run's own
        // startIndex accounts for it, so offsets after the image stay correct.
        let body = vec![json!({
            "startIndex": 1, "endIndex": 14,
            "paragraph": {"elements": [
                {"startIndex": 1, "textRun": {"content": "ab"}},
                {"startIndex": 3, "inlineObjectElement": {"inlineObjectId": "kix.1"}},
                {"startIndex": 4, "textRun": {"content": "cd target\n"}},
            ]},
        })];
        assert_eq!(find_matches(&body, "target").unwrap(), vec![(7, 13)]);
    }

    #[test]
    fn text_in_range_intersects_per_run_so_an_index_jump_does_not_skew_the_slice() {
        // Same body: index 4 is the start of the second run, not the fourth character of the
        // paragraph's flat text. A per-paragraph slice would return "d " here.
        let body = vec![json!({
            "startIndex": 1, "endIndex": 14,
            "paragraph": {"elements": [
                {"startIndex": 1, "textRun": {"content": "ab"}},
                {"startIndex": 3, "inlineObjectElement": {"inlineObjectId": "kix.1"}},
                {"startIndex": 4, "textRun": {"content": "cd target\n"}},
            ]},
        })];
        assert_eq!(text_in_range(&body, 4, 6), "cd");
        assert_eq!(text_in_range(&body, 1, 14), "abcd target\n");
    }

    #[test]
    fn a_needle_spanning_a_paragraph_break_never_matches() {
        let body = vec![para(1, &["first\n"]), para(7, &["second\n"])];
        assert_eq!(find_matches(&body, "first\nsecond").unwrap(), vec![]);
        assert_eq!(find_matches(&body, "st\nse").unwrap(), vec![]);
    }

    #[test]
    fn overlapping_occurrences_resume_after_the_previous_hit() {
        let body = vec![para(1, &["aaaa\n"])];
        assert_eq!(find_matches(&body, "aa").unwrap(), vec![(1, 3), (3, 5)]);
    }

    #[test]
    fn occurrences_come_back_in_document_order_and_are_selectable() {
        let body = vec![para(1, &["x and x\n"]), para(9, &["x again\n"])];
        let hits = find_matches(&body, "x").unwrap();
        assert_eq!(hits, vec![(1, 2), (7, 8), (9, 10)]);
        assert_eq!(resolve(&body, Some("x"), None, 2).unwrap().0, vec![(7, 8)]);
        // 0 = every occurrence
        assert_eq!(resolve(&body, Some("x"), None, 0).unwrap().0, hits);

        assert!(err(resolve(&body, Some("x"), None, 4)).contains("occurs 3 time"));
        assert!(err(resolve(&body, Some("absent"), None, 1)).contains("no match"));
        assert!(err(resolve(&body, Some("x"), None, -1)).contains("occurrence must be"));
    }

    #[test]
    fn resolve_describes_what_it_resolved() {
        let body = vec![para(1, &["x and x\n"])];
        assert_eq!(resolve(&body, Some("x"), None, 1).unwrap().1, "occurrence 1 of 'x'");
        assert_eq!(resolve(&body, Some("x"), None, 0).unwrap().1, "all 2 occurrences of 'x'");
        assert_eq!(resolve(&sectioned(), None, Some("Intro"), 1).unwrap().1, "section 'Intro'");
    }

    #[test]
    fn resolve_requires_exactly_one_locator() {
        let body = vec![para(1, &["hi\n"])];
        assert!(err(resolve(&body, None, None, 1)).contains("exactly one"));
        assert!(err(resolve(&body, Some("hi"), Some("Heading"), 1)).contains("exactly one"));
    }

    #[test]
    fn an_occurrence_with_a_section_is_rejected_not_ignored() {
        // A heading resolves to a single span, so an occurrence the caller believed in would
        // otherwise be silently dropped.
        let e = err(resolve(&sectioned(), None, Some("Intro"), 2));
        assert!(e.contains("occurrence applies to match"), "{e}");
    }

    // ---- traversal ---------------------------------------------------------------------

    #[test]
    fn a_match_inside_a_table_cell_resolves_within_that_cell() {
        let cell_table = table(1, &["alpha", "beta"]);
        let tail_start = index_of(&cell_table, "endIndex");
        let body = vec![cell_table.clone(), para(tail_start, &["tail\n"])];
        let hits = find_matches(&body, "beta").unwrap();
        assert_eq!(hits.len(), 1);
        let (start, end) = hits[0];
        let inner = &cell_table["table"]["tableRows"][0]["tableCells"][1]["content"][0];
        let (inner_start, inner_end) = (index_of(inner, "startIndex"), index_of(inner, "endIndex"));
        assert_eq!((start, end), (inner_start, inner_start + 4));
        // wholly inside one cell: deleteContentRange rejects a range that straddles a cell boundary
        assert!(inner_start <= start && start < end && end <= inner_end);
    }

    // ---- sections ----------------------------------------------------------------------

    #[test]
    fn a_section_ends_at_the_next_same_level_heading() {
        // spans the nested H2, stops at the next H1
        assert_eq!(find_section(&sectioned(), "Intro").unwrap(), (1, 31));
    }

    #[test]
    fn a_section_ends_at_the_next_higher_level_heading() {
        // H2 stops at the following H1
        assert_eq!(find_section(&sectioned(), "Sub").unwrap(), (18, 31));
    }

    #[test]
    fn a_section_runs_to_the_end_of_the_tab_when_it_is_last() {
        let body = sectioned();
        assert_eq!(find_section(&body, "Next").unwrap(), (31, body_end(&body)));
    }

    #[test]
    fn a_section_is_clamped_to_body_end() {
        let body = sectioned();
        let (_start, end) = find_section(&body, "Next").unwrap();
        // the body's final newline is undeletable
        assert_eq!(end, index_of(&body[body.len() - 1], "endIndex") - 1);
    }

    #[test]
    fn a_section_matches_the_heading_not_prose_with_the_same_text() {
        let body = vec![
            para(1, &["Results\n"]),
            styled(9, &["Results\n"], Some("HEADING_1")),
            para(17, &["body\n"]),
        ];
        assert_eq!(find_section(&body, "Results").unwrap(), (9, body_end(&body)));
    }

    #[test]
    fn a_missing_heading_error_lists_the_available_headings() {
        assert_eq!(
            err(find_section(&sectioned(), "Nope")),
            "no heading titled 'Nope'; headings in this tab: ['Intro', 'Sub', 'Next']"
        );
    }

    #[test]
    fn a_heading_inside_a_table_cell_is_not_a_section() {
        // Sections are top-level only, but the same text is still reachable as a match.
        let body = vec![json!({
            "startIndex": 1, "endIndex": 12,
            "table": {"tableRows": [{"tableCells": [
                {"content": [styled(4, &["Cell\n"], Some("HEADING_1"))]},
            ]}]},
        })];
        assert_eq!(err(find_section(&body, "Cell")), "no heading titled 'Cell'; headings in this tab: []");
        assert_eq!(find_matches(&body, "Cell").unwrap(), vec![(4, 8)]);
    }

    #[test]
    fn a_section_ending_at_body_end_never_includes_the_final_newline() {
        // A heading that is the last element has endIndex == body_end + 1; an unclamped max()
        // would return that and the API would reject the delete.
        let body = vec![styled(1, &["Only\n"], Some("HEADING_1"))];
        let (start, end) = find_section(&body, "Only").unwrap();
        assert_eq!((start, end), (1, body_end(&body)));
        assert!(end < index_of(&body[0], "endIndex"));
    }

    #[test]
    fn body_end_of_an_empty_body_is_the_first_writable_index() {
        assert_eq!(body_end(&[]), 1);
    }

    // ---- case sensitivity ----------------------------------------------------------------

    #[test]
    fn matching_is_case_sensitive() {
        // Folding is deliberately absent: 'İ'.lower() is two characters, so any offset computed
        // from folded text would be shifted. Pinned here so it is not added without handling that.
        let body = vec![para(1, &["Needle\n"])];
        assert_eq!(find_matches(&body, "needle").unwrap(), vec![]);
        assert_eq!(find_matches(&body, "Needle").unwrap(), vec![(1, 7)]);
    }

    #[test]
    fn an_empty_needle_is_rejected() {
        let body = vec![para(1, &["hi\n"])];
        assert!(err(find_matches(&body, "")).contains("non-empty"));
    }

    // ---- slicing & boundaries --------------------------------------------------------------

    #[test]
    fn text_in_range_slices_across_runs_and_astral_chars() {
        let body = vec![para(1, &["ab😀", "cd\n"])];
        assert_eq!(text_in_range(&body, 1, 7), "ab😀cd");
        assert_eq!(text_in_range(&body, 3, 5), "😀");
    }

    #[test]
    fn paragraph_bounds_snaps_to_the_containing_paragraph() {
        let body = vec![para(1, &["first\n"]), para(7, &["second\n"])];
        assert_eq!(paragraph_bounds(&body, 9), Some((7, 14)));
        assert_eq!(paragraph_bounds(&body, 2), Some((1, 7)));
    }

    #[test]
    fn paragraph_bounds_reports_no_boundary_inside_a_table() {
        // Cell paragraphs are not top-level, so there is no boundary to snap to. Returning None
        // (rather than the raw index) is what lets the caller refuse instead of anchoring
        // mid-cell.
        let cell_table = table(1, &["inside"]);
        let tail_start = index_of(&cell_table, "endIndex");
        let body = vec![cell_table, para(tail_start, &["after\n"])];
        let hits = find_matches(&body, "inside").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(paragraph_bounds(&body, hits[0].0), None);
    }
}
