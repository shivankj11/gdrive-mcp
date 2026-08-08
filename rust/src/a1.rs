//! A1-notation helpers for the Sheets tools.

use crate::error::{Result, ToolError};

/// 0-based column index -> letters (0 -> 'A', 26 -> 'AA').
pub fn col_to_letter(index0: i64) -> Result<String> {
    if index0 < 0 {
        return Err(ToolError::msg("column index must be >= 0"));
    }
    let mut letters = String::new();
    // Bijective base-26: there is no zero digit, so subtract one before each division —
    // 25 -> 'Z', 26 -> 'AA' rather than 'BA'. Widened to i128 so `index0 + 1` cannot overflow
    // at i64::MAX (Python's ints are unbounded and never wrap here).
    let mut n = index0 as i128 + 1;
    while n != 0 {
        let rem = (n - 1) % 26;
        n = (n - 1) / 26;
        letters.insert(0, (b'A' + rem as u8) as char);
    }
    Ok(letters)
}

/// Column letters -> 0-based index ('A' -> 0, 'AA' -> 26).
///
/// Empty letters yield -1; the callers that matter reach this through [`parse_cell`], whose
/// pattern already guarantees at least one letter.
pub fn letter_to_col(letters: &str) -> Result<i64> {
    let mut n: i128 = 0;
    // Python upper-cases the whole string first, so full Unicode case mapping applies; anything
    // that does not fold to an ASCII A-Z is rejected, and the message quotes the input as given.
    for ch in letters.to_uppercase().chars() {
        if !ch.is_ascii_uppercase() {
            return Err(invalid_letters(letters));
        }
        n = n * 26 + (ch as i128 - 'A' as i128 + 1);
        // Python would keep counting into a big int. A column past i64 addresses no cell any
        // Sheets API accepts, so reject it instead of wrapping — and the bound (result <=
        // i64::MAX - 1) is what lets `parse_range` add its half-open +1 without overflow.
        if n > i64::MAX as i128 {
            return Err(invalid_letters(letters));
        }
    }
    Ok((n - 1) as i64)
}

fn invalid_letters(letters: &str) -> ToolError {
    ToolError::msg(format!("invalid column letters: '{letters}'"))
}

/// 'B3' -> (col0 = 1, row0 = 2).
pub fn parse_cell(cell: &str) -> Result<(i64, i64)> {
    let trimmed = cell.trim();
    // A full match of `([A-Za-z]+)([0-9]+)`. ASCII [0-9] rather than `\d`: Python's `\d` also
    // matches non-ASCII digits that int() accepts, but no spreadsheet emits those and ASCII
    // keeps the parse total.
    let letters_len = trimmed.chars().take_while(char::is_ascii_alphabetic).count();
    // Safe split point: every counted char is ASCII, so the char count is also the byte offset.
    let (letters, digits) = trimmed.split_at(letters_len);
    if letters.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        // The message quotes `cell` as passed, whitespace and all — the strip is local.
        return Err(invalid_cell(cell));
    }
    // A row number too wide for i64 is not a row; Python's unbounded int would have kept it.
    let Ok(row) = digits.parse::<i64>() else {
        return Err(invalid_cell(cell));
    };
    Ok((letter_to_col(letters)?, row - 1))
}

fn invalid_cell(cell: &str) -> ToolError {
    ToolError::msg(format!("invalid A1 cell: '{cell}'"))
}

/// Split 'Tab!A1:B2' into (tab, 'A1:B2'), unquoting a quoted tab name; (None, a1) if no tab.
pub fn split_range(a1: &str) -> (Option<String>, String) {
    let a1 = a1.trim();
    // The quoted form is tried first because a quoted name may itself contain '!' — partitioning
    // "'Q1!Draft'!A1" on the first '!' would cut the tab name in half.
    if let Some((tab, cells)) = split_quoted_tab(a1) {
        return (Some(tab), cells);
    }
    match a1.split_once('!') {
        // An empty tab before the '!' is still a tab, not "no tab" — Python keys off the
        // separator, not the name.
        Some((tab, rest)) => (Some(tab.to_string()), rest.to_string()),
        None => (None, a1.to_string()),
    }
}

/// `^'((?:[^']|'')*)'!(.*)$` with the doubled quotes already un-doubled.
///
/// The starred alternation can never consume a lone `'`, so the first quote not followed by
/// another one is the only candidate terminator — no backtracking is needed to find it.
fn split_quoted_tab(a1: &str) -> Option<(String, String)> {
    let mut rest = a1.strip_prefix('\'')?;
    let mut tab = String::new();
    loop {
        // An unterminated name never matches, and the caller falls back to the '!' partition.
        let (before, after) = rest.split_once('\'')?;
        tab.push_str(before);
        match after.strip_prefix('\'') {
            // `''` is an escaped quote inside the name, not its terminator.
            Some(more) => {
                tab.push('\'');
                rest = more;
            }
            None => {
                let cells = after.strip_prefix('!')?;
                // `.` in the Python pattern does not cross a newline, so a cells part containing
                // one is not a quoted range at all.
                return match cells.contains('\n') {
                    true => None,
                    false => Some((tab, cells.to_string())),
                };
            }
        }
    }
}

/// A parsed range as `(start_col0, start_row0, end_col0, end_row0)`, 0-based and half-open.
pub type Bounds = (i64, i64, i64, i64);

/// 'Tab!A2:C10' -> (tab, (start_col0, start_row0, end_col0, end_row0)), 0-based half-open.
///
/// Bounded ranges only ('A2:C10', or a single cell 'B4'); open-ended ranges like 'A:C' are
/// rejected ([`parse_cell`] requires an explicit row number).
pub fn parse_range(a1: &str) -> Result<(Option<String>, Bounds)> {
    let (tab, cells) = split_range(a1);
    // Split on the FIRST ':' so a malformed 'A1:B2:C3' fails in parse_cell rather than silently
    // dropping its tail.
    let (start, end) = cells.split_once(':').unwrap_or((cells.as_str(), ""));
    let (c0, r0) = parse_cell(start)?;
    let (c1, r1) = if end.is_empty() { (c0, r0) } else { parse_cell(end)? };
    if c1 < c0 || r1 < r0 {
        // Quotes `a1` as passed: split_range stripped only its own copy.
        return Err(ToolError::msg(format!("range end must not precede its start: '{a1}'")));
    }
    Ok((tab, (c0, r0, c1 + 1, r1 + 1)))
}

/// Quote a sheet/tab name for A1 notation, doubling embedded single quotes.
///
/// Google Sheets escapes a `'` inside a quoted name by doubling it, so a tab like `John's Data`
/// must render as `'John''s Data'` — without this the inner quote terminates the name early and
/// the range is invalid or mis-targeted.
pub fn quote_tab(tab: &str) -> String {
    format!("'{}'", tab.replace('\'', "''"))
}

/// Full A1 range covering an nrows x ncols block anchored at start_cell on tab.
/// An empty `tab` omits the prefix (the Python `tab` falsy branch).
pub fn build_range(tab: &str, start_cell: &str, nrows: i64, ncols: i64) -> Result<String> {
    let (c0, r0) = parse_cell(start_cell)?;
    // A block is at least 1x1: a zero or negative count still names the anchor cell itself.
    // nrows/ncols arrive straight from tool input, so saturate instead of overflowing.
    let end_col = col_to_letter(c0.saturating_add(ncols.max(1)).saturating_sub(1))?;
    let end_row = r0.saturating_add(nrows.max(1));
    let prefix = if tab.is_empty() { String::new() } else { format!("{}!", quote_tab(tab)) };
    Ok(format!("{prefix}{}:{end_col}{end_row}", start_cell.to_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_round_trip_back_to_the_column_index_they_came_from() {
        for i in [0, 1, 25, 26, 27, 51, 52, 701, 702] {
            assert_eq!(letter_to_col(&col_to_letter(i).unwrap()).unwrap(), i);
        }
    }

    #[test]
    fn column_indices_render_as_bijective_base_26_letters() {
        assert_eq!(col_to_letter(0).unwrap(), "A");
        assert_eq!(col_to_letter(25).unwrap(), "Z");
        assert_eq!(col_to_letter(26).unwrap(), "AA");
        assert_eq!(col_to_letter(701).unwrap(), "ZZ");
        assert_eq!(col_to_letter(702).unwrap(), "AAA");
    }

    #[test]
    fn a_negative_column_index_is_rejected() {
        assert_eq!(col_to_letter(-1).unwrap_err().to_string(), "column index must be >= 0");
    }

    #[test]
    fn non_letters_are_rejected_as_column_letters() {
        let err = letter_to_col("A1").unwrap_err();
        assert_eq!(err.to_string(), "invalid column letters: 'A1'");
    }

    #[test]
    fn a_cell_parses_into_a_zero_based_column_and_row() {
        assert_eq!(parse_cell("A1").unwrap(), (0, 0));
        assert_eq!(parse_cell("B3").unwrap(), (1, 2));
        assert_eq!(parse_cell("AA10").unwrap(), (26, 9));
    }

    #[test]
    fn a_cell_may_be_lower_case_or_padded_with_whitespace() {
        assert_eq!(parse_cell("b3").unwrap(), (1, 2));
        assert_eq!(parse_cell("  B3\t").unwrap(), (1, 2));
    }

    #[test]
    fn a_cell_without_a_row_number_is_not_a_cell() {
        // What makes 'A:C' an open-ended range rather than a parseable one.
        let err = parse_cell("A").unwrap_err();
        assert_eq!(err.to_string(), "invalid A1 cell: 'A'");
        assert_eq!(parse_cell(" 12 ").unwrap_err().to_string(), "invalid A1 cell: ' 12 '");
        assert_eq!(parse_cell("A1B").unwrap_err().to_string(), "invalid A1 cell: 'A1B'");
    }

    #[test]
    fn a_block_spans_ncols_across_and_nrows_down_from_its_anchor() {
        assert_eq!(build_range("Data", "A1", 3, 2).unwrap(), "'Data'!A1:B3");
        assert_eq!(build_range("S", "B2", 1, 1).unwrap(), "'S'!B2:B2");
        assert_eq!(build_range("T", "C5", 4, 3).unwrap(), "'T'!C5:E8");
    }

    #[test]
    fn a_block_is_never_smaller_than_the_anchor_cell_itself() {
        assert_eq!(build_range("S", "B2", 0, 0).unwrap(), "'S'!B2:B2");
        assert_eq!(build_range("S", "B2", -5, -5).unwrap(), "'S'!B2:B2");
    }

    #[test]
    fn an_empty_tab_omits_the_prefix_and_the_start_cell_is_upper_cased() {
        assert_eq!(build_range("", "a1", 2, 2).unwrap(), "A1:B2");
    }

    #[test]
    fn quote_tab_doubles_embedded_apostrophes() {
        assert_eq!(quote_tab("Data"), "'Data'");
        assert_eq!(quote_tab("John's Data"), "'John''s Data'");
        assert_eq!(quote_tab("O'Brien's"), "'O''Brien''s'");
    }

    #[test]
    fn build_range_quotes_an_apostrophe_tab_rather_than_ending_the_name_early() {
        assert_eq!(build_range("John's Data", "A1", 1, 1).unwrap(), "'John''s Data'!A1:A1");
    }

    #[test]
    fn a_range_splits_into_its_tab_and_its_cells() {
        assert_eq!(split_range("Data!A1:B2"), (Some("Data".into()), "A1:B2".into()));
        assert_eq!(split_range("A1:B2"), (None, "A1:B2".into()));
        // unquoted + undoubled
        assert_eq!(split_range("'John''s Data'!B2"), (Some("John's Data".into()), "B2".into()));
    }

    #[test]
    fn a_quoted_tab_may_contain_the_bang_that_would_otherwise_split_it() {
        assert_eq!(split_range("'Q1!Draft'!A1:B2"), (Some("Q1!Draft".into()), "A1:B2".into()));
    }

    #[test]
    fn an_unterminated_quote_falls_back_to_splitting_on_the_first_bang() {
        assert_eq!(split_range("'Data!A1"), (Some("'Data".into()), "A1".into()));
        assert_eq!(split_range("'Data"), (None, "'Data".into()));
    }

    #[test]
    fn an_empty_name_before_the_bang_is_a_tab_not_the_absence_of_one() {
        assert_eq!(split_range("!A1"), (Some(String::new()), "A1".into()));
        assert_eq!(split_range("''!A1"), (Some(String::new()), "A1".into()));
    }

    #[test]
    fn a_range_parses_into_zero_based_half_open_bounds() {
        assert_eq!(parse_range("Data!A2:C10").unwrap(), (Some("Data".into()), (0, 1, 3, 10)));
        assert_eq!(parse_range("B2").unwrap(), (None, (1, 1, 2, 2)));
        assert_eq!(parse_range(" Data!A1:A1 ").unwrap(), (Some("Data".into()), (0, 0, 1, 1)));
    }

    #[test]
    fn a_trailing_colon_reads_as_the_single_start_cell() {
        assert_eq!(parse_range("A1:").unwrap(), (None, (0, 0, 1, 1)));
    }

    #[test]
    fn an_open_ended_column_range_is_rejected() {
        let err = parse_range("Data!A:C").unwrap_err();
        assert_eq!(err.to_string(), "invalid A1 cell: 'A'");
    }

    #[test]
    fn a_range_whose_end_precedes_its_start_is_rejected() {
        let err = parse_range("Data!C10:A2").unwrap_err();
        assert_eq!(err.to_string(), "range end must not precede its start: 'Data!C10:A2'");
        // Either axis alone is enough to reverse it.
        assert!(parse_range("A10:C2").is_err());
        assert!(parse_range("C1:A9").is_err());
    }

    #[test]
    fn only_the_first_colon_splits_a_range_so_a_third_cell_is_an_error() {
        assert_eq!(parse_range("A1:B2:C3").unwrap_err().to_string(), "invalid A1 cell: 'B2:C3'");
    }
}
