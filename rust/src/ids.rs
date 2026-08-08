//! Parse a Google Drive/Docs/Sheets URL or bare ID into an id + a kind hint.

use std::sync::LazyLock;

use regex::Regex;

use crate::error::{Result, ToolError};

/// Drive ids are URL-safe base64-ish, so every URL shape ends in the same capture.
const ID: &str = r"([a-zA-Z0-9_-]+)";

/// Order is load-bearing: the FIRST pattern that matches wins. A Docs URL carrying an unrelated
/// `?id=` query param must still resolve as a document, so the `/d/` shapes precede `[?&]id=`.
static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (format!(r"docs\.google\.com/document/d/{ID}"), "document"),
        (format!(r"docs\.google\.com/spreadsheets/d/{ID}"), "spreadsheet"),
        // Slides has no dedicated API here, so a presentation is just a Drive file.
        (format!(r"docs\.google\.com/presentation/d/{ID}"), "file"),
        (format!(r"drive\.google\.com/drive/folders/{ID}"), "folder"),
        (format!(r"drive\.google\.com/file/d/{ID}"), "file"),
        (format!(r"[?&]id={ID}"), "file"),
    ]
    .into_iter()
    // The patterns are literals, so compilation cannot fail on any input; dropping a
    // hypothetically bad one keeps this total instead of panicking inside a tool call.
    // `the_pattern_table_compiles_in_priority_order` pins that none are actually dropped.
    .filter_map(|(pattern, kind)| Some((Regex::new(&pattern).ok()?, kind)))
    .collect()
});

/// A bare id must be the WHOLE (trimmed) string — otherwise prose like "not a link" or an
/// unrecognised URL would be handed to Drive as an id.
static BARE_ID: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]{20,}$").ok());

static TAB: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"[?&#]tab=(t\.[A-Za-z0-9]+)").ok());

/// `kind` is one of: document | spreadsheet | folder | file | unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub id: String,
    pub kind: String,
}

pub fn parse_ref(url_or_id: &str) -> Result<Ref> {
    let s = url_or_id.trim();
    for (pattern, kind) in PATTERNS.iter() {
        if let Some(id) = pattern.captures(s).and_then(|c| c.get(1)) {
            return Ok(Ref { id: id.as_str().to_string(), kind: (*kind).to_string() });
        }
    }
    if BARE_ID.as_ref().is_some_and(|re| re.is_match(s)) {
        // No URL context to key off, so the caller must discover the type itself.
        return Ok(Ref { id: s.to_string(), kind: "unknown".to_string() });
    }
    // Python echoes the *original* argument via `{url_or_id!r}`, whitespace and all.
    Err(ToolError::msg(format!("could not parse a Drive ID from: '{url_or_id}'")))
}

/// Extract a Google Docs tab id (e.g. `t.70s4vio5pxrm`) from a doc URL, if present.
pub fn parse_tab(url_or_id: &str) -> Option<String> {
    let re = TAB.as_ref()?;
    re.captures(url_or_id)?.get(1).map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTHETIC_DOCUMENT_ID: &str = "TEST_DOCUMENT_ID_0000000001";

    #[test]
    fn a_document_url_yields_the_document_id_and_kind() {
        let r =
            parse_ref(&format!("https://docs.google.com/document/d/{SYNTHETIC_DOCUMENT_ID}/edit?tab=t.0"))
                .unwrap();
        assert_eq!(r.id, SYNTHETIC_DOCUMENT_ID);
        assert_eq!(r.kind, "document");
    }

    #[test]
    fn a_spreadsheet_url_with_a_gid_fragment_yields_only_the_file_id() {
        let r = parse_ref("https://docs.google.com/spreadsheets/d/ABC123_def-XYZ/edit#gid=0").unwrap();
        assert_eq!(r.id, "ABC123_def-XYZ");
        assert_eq!(r.kind, "spreadsheet");
    }

    #[test]
    fn a_folder_url_is_kind_folder() {
        let r = parse_ref("https://drive.google.com/drive/folders/FOLDERid-1").unwrap();
        assert_eq!(r.id, "FOLDERid-1");
        assert_eq!(r.kind, "folder");
    }

    #[test]
    fn a_file_url_is_kind_file() {
        assert_eq!(parse_ref("https://drive.google.com/file/d/FILEID_1/view").unwrap().kind, "file");
    }

    #[test]
    fn an_open_query_url_yields_the_id_query_param() {
        assert_eq!(parse_ref("https://drive.google.com/open?id=XYZ987_id").unwrap().id, "XYZ987_id");
    }

    #[test]
    fn a_bare_id_has_no_kind_hint() {
        let r = parse_ref(SYNTHETIC_DOCUMENT_ID).unwrap();
        assert_eq!(r.id, SYNTHETIC_DOCUMENT_ID);
        assert_eq!(r.kind, "unknown");
    }

    #[test]
    fn prose_is_not_an_id() {
        let e = parse_ref("not a link").unwrap_err();
        assert_eq!(e.to_string(), "could not parse a Drive ID from: 'not a link'");
    }

    #[test]
    fn the_error_echoes_the_argument_exactly_as_given() {
        // The message quotes the raw argument, not the trimmed one.
        assert_eq!(
            parse_ref("  nope  ").unwrap_err().to_string(),
            "could not parse a Drive ID from: '  nope  '"
        );
        assert_eq!(parse_ref("").unwrap_err().to_string(), "could not parse a Drive ID from: ''");
    }

    #[test]
    fn a_presentation_url_is_a_plain_drive_file() {
        let r = parse_ref("https://docs.google.com/presentation/d/SLIDES_id-9/edit").unwrap();
        assert_eq!(r.id, "SLIDES_id-9");
        assert_eq!(r.kind, "file");
    }

    #[test]
    fn an_earlier_pattern_beats_a_later_one_on_the_same_url() {
        // Both `/document/d/` and `[?&]id=` match; the document shape is listed first.
        let r = parse_ref("https://docs.google.com/document/d/DOCid_1/edit?id=OTHERid").unwrap();
        assert_eq!(r.id, "DOCid_1");
        assert_eq!(r.kind, "document");
    }

    #[test]
    fn the_pattern_table_compiles_in_priority_order() {
        let kinds: Vec<&str> = PATTERNS.iter().map(|(_, k)| *k).collect();
        assert_eq!(kinds, ["document", "spreadsheet", "file", "folder", "file", "file"]);
        assert!(BARE_ID.is_some());
        assert!(TAB.is_some());
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(parse_ref(&format!("  {SYNTHETIC_DOCUMENT_ID}\n")).unwrap().id, SYNTHETIC_DOCUMENT_ID);
        assert_eq!(parse_ref("\thttps://drive.google.com/file/d/FILEID_1/view ").unwrap().id, "FILEID_1");
    }

    #[test]
    fn a_bare_id_must_match_the_whole_string() {
        // A 20+ char run embedded in prose is not a reference to anything.
        assert!(parse_ref("see AAAAAAAAAAAAAAAAAAAA for details").is_err());
        assert!(parse_ref("AAAAAAAAAAAAAAAAAAAA/edit").is_err());
    }

    #[test]
    fn a_bare_id_shorter_than_twenty_characters_is_rejected() {
        assert!(parse_ref(&"A".repeat(19)).is_err());
        assert_eq!(parse_ref(&"A".repeat(20)).unwrap().kind, "unknown");
    }

    #[test]
    fn a_url_id_stops_at_the_next_path_segment() {
        let r = parse_ref("https://docs.google.com/document/d/DOCid_1/edit#heading=h.abc").unwrap();
        assert_eq!(r.id, "DOCid_1");
    }

    #[test]
    fn a_tab_id_is_read_from_query_or_fragment() {
        assert_eq!(parse_tab("https://docs.google.com/document/d/D/edit?tab=t.0").as_deref(), Some("t.0"));
        assert_eq!(
            parse_tab("https://docs.google.com/document/d/D/edit#tab=t.abc123").as_deref(),
            Some("t.abc123")
        );
        assert_eq!(
            parse_tab("https://docs.google.com/document/d/D/edit?usp=x&tab=t.70s4vio5pxrm").as_deref(),
            Some("t.70s4vio5pxrm")
        );
    }

    #[test]
    fn a_url_without_a_tab_param_has_no_tab() {
        assert_eq!(parse_tab("https://docs.google.com/document/d/D/edit"), None);
        assert_eq!(parse_tab(SYNTHETIC_DOCUMENT_ID), None);
        assert_eq!(parse_tab(""), None);
    }

    #[test]
    fn a_tab_param_needs_a_query_or_fragment_delimiter_before_it() {
        // `contab=t.0` must not read as a tab id.
        assert_eq!(parse_tab("https://example.com/contab=t.0"), None);
        assert_eq!(parse_tab("tab=t.0"), None);
    }

    #[test]
    fn a_tab_id_capture_stops_at_the_first_non_alphanumeric() {
        assert_eq!(parse_tab("?tab=t.0.1").as_deref(), Some("t.0"));
        assert_eq!(parse_tab("?tab=t.abc&x=1").as_deref(), Some("t.abc"));
        // The `t.` prefix is required.
        assert_eq!(parse_tab("?tab=0"), None);
    }
}
