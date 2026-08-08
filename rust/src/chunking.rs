//! Split large text into bounded, natural-boundary chunks for agent-friendly paging.
//!
//! The read tools return one chunk plus metadata (total size, total chunks, has_more) so an
//! agent pulls only what it needs instead of flooding its context with a whole document.
//!
//! All lengths here are Unicode scalar counts — the unit Python's `len()` used, so a document
//! chunks identically under both implementations.

use serde_json::{Map, Value};

/// ~8k chars ≈ ~2k tokens: one read stays well inside an agent's context budget while
/// keeping round-trips low. Overridable per call via the tools' max_chars argument.
pub const DEFAULT_MAX_CHARS: i64 = 8000;

/// Paragraphs (split on blank lines), each hard-split so none exceeds max_chars.
///
/// Only ever reached with `max_chars >= 1`; `chunk_text` returns early below that.
fn atoms(text: &str, max_chars: usize) -> Vec<String> {
    // `chunks` panics on a zero step, so pin the floor the caller already guarantees.
    let step = max_chars.max(1);
    let mut atoms: Vec<String> = Vec::new();
    for para in text.split("\n\n") {
        let scalars: Vec<char> = para.chars().collect();
        if scalars.len() <= max_chars {
            atoms.push(para.to_string());
        } else {
            // Slice by characters, not bytes: the budget is a scalar count, and a byte slice
            // could land inside a multi-byte scalar.
            for piece in scalars.chunks(step) {
                atoms.push(piece.iter().collect());
            }
        }
    }
    atoms
}

/// Greedily pack paragraphs into chunks of at most max_chars.
///
/// Splits on blank lines first (paragraph boundaries); a paragraph longer than max_chars
/// is hard-split so no chunk exceeds the budget. max_chars <= 0 disables chunking (one
/// chunk). Empty text yields a single empty chunk.
pub fn chunk_text(text: &str, max_chars: i64) -> Vec<String> {
    if max_chars <= 0 {
        return vec![text.to_string()];
    }
    if text.is_empty() {
        return vec![String::new()];
    }
    let budget = usize::try_from(max_chars).unwrap_or(usize::MAX);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    // Carried alongside `current` so its scalar count is not re-scanned per paragraph.
    let mut current_len = 0usize;
    for para in atoms(text, budget) {
        let para_len = para.chars().count();
        // The +2 pays for the "\n\n" that rejoining the two would insert.
        if !current.is_empty() && current_len + 2 + para_len > budget {
            chunks.push(current);
            current = para;
            current_len = para_len;
        } else if current.is_empty() {
            // Python tested `current` for truthiness, so an empty accumulator is replaced
            // rather than joined: a leading blank paragraph contributes no separator.
            current = para;
            current_len = para_len;
        } else {
            current.push_str("\n\n");
            current.push_str(&para);
            current_len += 2 + para_len;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        // Every atom was empty (the text was nothing but blank lines): still serve one chunk.
        chunks.push(String::new());
    }
    chunks
}

/// Chunk `text` and return the requested chunk plus paging metadata, as the JSON object the
/// read tools splat into their result: content, chunk_index, total_chunks, total_chars, has_more.
///
/// `chunk` is a 0-based index; out-of-range values clamp to the last chunk, and the
/// returned chunk_index reflects what was actually served.
pub fn paginate(text: &str, chunk: i64, max_chars: i64) -> Map<String, Value> {
    let chunks = chunk_text(text, max_chars);
    // `chunk_text` never returns an empty list, so there is always a last chunk to clamp to.
    let last = chunks.len().saturating_sub(1);
    let requested = usize::try_from(chunk.max(0)).unwrap_or(usize::MAX);
    let index = requested.min(last);
    let content = chunks.get(index).cloned().unwrap_or_default();
    let total_chunks = chunks.len() as u64;
    // Scalars, not bytes, so the caller sees the same total the Python reported.
    let total_chars = text.chars().count() as u64;
    let mut out = Map::new();
    out.insert("content".to_string(), Value::String(content));
    out.insert("chunk_index".to_string(), Value::from(index as u64));
    out.insert("total_chunks".to_string(), Value::from(total_chunks));
    out.insert("total_chars".to_string(), Value::from(total_chars));
    out.insert("has_more".to_string(), Value::Bool(index < last));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lens(chunks: &[String]) -> Vec<usize> {
        chunks.iter().map(|c| c.chars().count()).collect()
    }

    fn paras(n: usize) -> String {
        let items: Vec<String> = (0..n).map(|i| format!("para{i}")).collect();
        items.join("\n\n")
    }

    #[test]
    fn text_under_the_budget_stays_one_chunk() {
        assert_eq!(chunk_text("a\n\nb\n\nc", 1000), vec!["a\n\nb\n\nc"]);
    }

    #[test]
    fn empty_text_yields_a_single_empty_chunk() {
        assert_eq!(chunk_text("", 1000), vec![""]);
    }

    #[test]
    fn no_chunk_exceeds_the_budget() {
        let long = |i: usize| format!("paragraph number {i} ").repeat(20);
        let text = (0..50).map(long).collect::<Vec<_>>().join("\n\n");
        let chunks = chunk_text(&text, 500);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= 500));
    }

    #[test]
    fn packing_on_blank_lines_rejoins_losslessly() {
        let text = paras(30);
        assert_eq!(chunk_text(&text, 40).join("\n\n"), text);
    }

    #[test]
    fn a_paragraph_longer_than_the_budget_is_hard_split() {
        let text = "x".repeat(2500); // one paragraph, no blank lines
        let chunks = chunk_text(&text, 1000);
        assert_eq!(lens(&chunks), vec![1000, 1000, 500]);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn a_hard_split_measures_characters_not_bytes() {
        let text = "é".repeat(2500); // two bytes per scalar
        let chunks = chunk_text(&text, 1000);
        assert_eq!(lens(&chunks), vec![1000, 1000, 500]);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn the_rejoining_separator_counts_against_the_budget() {
        // "aaa" + "\n\n" + "bb" is 7 > 5, so the paragraphs cannot share a chunk.
        assert_eq!(chunk_text("aaa\n\nbb", 5), vec!["aaa", "bb"]);
    }

    #[test]
    fn max_chars_zero_disables_chunking() {
        let text = format!("a\n\n{}", "b".repeat(5000));
        assert_eq!(chunk_text(&text, 0), vec![text.clone()]);
    }

    #[test]
    fn a_negative_max_chars_also_disables_chunking() {
        assert_eq!(chunk_text("abc", -5), vec!["abc"]);
    }

    #[test]
    fn text_of_only_blank_lines_still_yields_one_empty_chunk() {
        assert_eq!(chunk_text("\n\n", 1000), vec![""]);
    }

    #[test]
    fn a_leading_blank_paragraph_adds_no_separator() {
        assert_eq!(chunk_text("\n\na", 1000), vec!["a"]);
    }

    #[test]
    fn paginate_reports_the_paging_metadata() {
        let text = paras(30);
        let total = chunk_text(&text, 40).len();
        let first = paginate(&text, 0, 40);
        assert_eq!(first["chunk_index"], json!(0));
        assert_eq!(first["total_chunks"], json!(total));
        assert_eq!(first["total_chars"], json!(text.chars().count()));
        assert_eq!(first["has_more"], json!(total > 1));
    }

    #[test]
    fn an_out_of_range_chunk_clamps_to_the_last_and_says_which_it_served() {
        let text = paras(30);
        let chunks = chunk_text(&text, 40);
        let last = paginate(&text, 999, 40);
        assert_eq!(last["chunk_index"], json!(chunks.len() - 1));
        assert_eq!(last["has_more"], json!(false));
        assert_eq!(last["content"], json!(chunks[chunks.len() - 1]));
    }

    #[test]
    fn a_negative_chunk_clamps_to_the_first() {
        let out = paginate("héllo", -3, 1000);
        assert_eq!(out["chunk_index"], json!(0));
        assert_eq!(out["content"], json!("héllo"));
    }

    #[test]
    fn paginate_counts_total_chars_in_scalars_not_bytes() {
        assert_eq!(paginate("héllo", 0, 1000)["total_chars"], json!(5));
    }

    #[test]
    fn paginate_returns_exactly_the_documented_keys() {
        let out = paginate("", 5, 1000);
        let keys = out.keys().cloned().collect::<Vec<_>>().join(",");
        let want = "content,chunk_index,total_chunks,total_chars,has_more";
        assert_eq!(keys, want);
        assert_eq!(out["content"], json!(""));
        assert_eq!(out["total_chunks"], json!(1));
        assert_eq!(out["total_chars"], json!(0));
        assert_eq!(out["has_more"], json!(false));
    }
}
