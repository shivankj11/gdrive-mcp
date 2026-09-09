//! Append-only audit log of tool activity.
//!
//! Records identifiers and actions only — tool name, target item id, destination path, outcome,
//! and the authenticated user — NEVER content values (rows/text/content are not logged).

use std::io::Write;
use std::path::PathBuf;

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};

use crate::config::{chmod, config_dir, expanduser};
use crate::ids::parse_ref;

/// Args safe to record: opaque identifiers + operational scalars only. Deliberately omits
/// free-text fields (name, title, tab, dest_path, source_path, new_name) — a filename or tab
/// title can itself contain sensitive data — and never records rows/text/content.
const SAFE_ARG_KEYS: &[&str] = &["item", "replace_id", "parent", "start_row", "count", "to", "value_input"];
/// Ref args reduced to their opaque Drive id so free text (potentially sensitive) in a raw URL/string
/// can't be smuggled into the log verbatim.
const REF_ARG_KEYS: &[&str] = &["item", "replace_id", "parent"];

fn safe_args(args: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for key in SAFE_ARG_KEYS {
        let Some(value) = args.get(*key) else { continue };
        if REF_ARG_KEYS.contains(key) {
            let reduced = value
                .as_str()
                .and_then(|s| parse_ref(s).ok())
                .map(|r| Value::String(r.id))
                .unwrap_or_else(|| Value::String("<unparseable>".into()));
            out.insert((*key).to_string(), reduced);
        } else {
            out.insert((*key).to_string(), value.clone());
        }
    }
    out
}

fn audit_path() -> PathBuf {
    match std::env::var("GDRIVE_MCP_AUDIT_LOG") {
        Ok(v) if !v.is_empty() => expanduser(&v),
        _ => config_dir().join("audit.log"),
    }
}

/// Append one JSON line describing a tool call. Best-effort — never fails into the caller.
pub fn record(tool: &str, args: &Map<String, Value>, outcome: &str, user: Option<&str>) {
    let entry = json!({
        "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false),
        "user": user,
        "tool": tool,
        "args": Value::Object(safe_args(args)),
        "outcome": outcome,
    });
    let path = audit_path();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let Ok(line) = serde_json::to_string(&entry) else { return };
    let _ = writeln!(file, "{line}");
    chmod(&path, 0o600);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn free_text_and_content_arguments_are_never_recorded() {
        let out = safe_args(&args(json!({
            "item": "1AbC_defGHIjklMNOpqrstUVwxyz012345678",
            "text": "secret-document-text",
            "rows": [["secret-cell-A", "secret-cell-B"]],
            "name": "Confidential Report.csv",
            "dest_path": "confidential/report.csv",
            "count": 3,
        })));
        assert_eq!(out.keys().collect::<Vec<_>>(), vec!["item", "count"]);
        assert_eq!(out["count"], json!(3));
    }

    #[test]
    fn locator_arguments_are_never_recorded() {
        // Locators and replacement text are document content by definition, so they are
        // sensitive. SAFE_ARG_KEYS is an allowlist, which excludes them by construction — but
        // nothing said so, and widening the allowlist would quietly start writing document text
        // into the log. Driven through `record` rather than `safe_args` so the assertion covers
        // the whole entry, not just the filter.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("audit.log");
        let sent = args(json!({
            "item": "1AbC_defGHIjklMNOpqrstUVwxyz012345678",
            "match": "secret-match-text",
            "section": "Confidential Heading",
            "replacement": "secret-replacement-text",
            "after": "secret-anchor-text",
            "before": "another-secret-anchor",
        }));
        // Env is process-global; this test owns GDRIVE_MCP_AUDIT_LOG for its duration.
        let _lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GDRIVE_MCP_AUDIT_LOG", &log) };
        record("replace_text", &sent, "ok", None);
        unsafe { std::env::remove_var("GDRIVE_MCP_AUDIT_LOG") };

        let body = std::fs::read_to_string(&log).unwrap();
        let entry: Value = serde_json::from_str(body.trim()).unwrap();
        // The target id is the only argument of a locator write that may be recorded.
        assert_eq!(entry["args"].as_object().unwrap().keys().collect::<Vec<_>>(), vec!["item"]);
        for secret in [
            "secret-match-text",
            "Confidential Heading",
            "secret-replacement-text",
            "secret-anchor-text",
            "another-secret-anchor",
        ] {
            assert!(!body.contains(secret), "the audit log leaked {secret:?}: {body}");
        }
    }

    #[test]
    fn calendar_content_attendees_and_email_calendar_ids_are_never_recorded() {
        let out = safe_args(&args(json!({
            "calendar_id": "private-calendar@example.com",
            "event_id": "opaqueevent123",
            "summary": "Confidential Title",
            "description": "private-note-A",
            "location": "Restricted Room",
            "attendees": ["collaborator@example.com"],
            "query": "confidential search"
        })));
        assert!(out.is_empty(), "Calendar arguments leaked into the audit allowlist: {out:?}");
    }

    #[test]
    fn ref_arguments_are_reduced_to_their_opaque_id() {
        let out = safe_args(&args(json!({
            "item": "https://docs.google.com/document/d/DOC123456789012345678/edit#heading=h.private",
        })));
        assert_eq!(out["item"], json!("DOC123456789012345678"));
    }

    #[test]
    fn an_unparseable_ref_is_masked_rather_than_logged_verbatim() {
        let out = safe_args(&args(json!({"item": "not an id — confidential free text"})));
        assert_eq!(out["item"], json!("<unparseable>"));
    }

    #[test]
    fn an_unwritable_log_path_never_fails_into_the_caller() {
        // A missing log line is bad; an audit write that takes down the tool call is worse.
        // `record` swallows io errors by construction, but nothing said so, so a refactor that
        // let one escape would ship. A path under a regular file is the portable way to make the
        // write impossible — chmod-based tricks do not hold when the suite runs as root.
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("not-a-directory");
        std::fs::write(&not_a_dir, "x").unwrap();
        let already_a_dir = dir.path().join("already-a-directory");
        std::fs::create_dir(&already_a_dir).unwrap();
        let sent = args(json!({"item": "1AbC_defGHIjklMNOpqrstUVwxyz012345678"}));

        // Env is process-global; this test owns GDRIVE_MCP_AUDIT_LOG for its duration.
        let _lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The parent cannot be created: it would have to live inside a regular file.
        unsafe { std::env::set_var("GDRIVE_MCP_AUDIT_LOG", not_a_dir.join("sub").join("audit.log")) };
        record("read_sheet", &sent, "ok", None);
        // The parent exists, so the mkdir succeeds and it is the open that fails: the log path
        // itself names a directory.
        unsafe { std::env::set_var("GDRIVE_MCP_AUDIT_LOG", &already_a_dir) };
        record("read_sheet", &sent, "error:ToolError", Some("me@example.com"));
        unsafe { std::env::remove_var("GDRIVE_MCP_AUDIT_LOG") };

        // Returning at all is the claim; these pin that it returned without writing elsewhere.
        assert_eq!(std::fs::read_to_string(&not_a_dir).unwrap(), "x");
        assert!(std::fs::read_dir(&already_a_dir).unwrap().next().is_none());
    }

    #[test]
    fn writes_one_json_line_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("audit.log");
        // Env is process-global; this test owns GDRIVE_MCP_AUDIT_LOG for its duration.
        let _lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GDRIVE_MCP_AUDIT_LOG", &log) };
        record("read_document", &args(json!({"item": "abc"})), "ok", Some("me@example.com"));
        record("delete_text", &args(json!({"item": "abc"})), "error:ToolError", None);
        unsafe { std::env::remove_var("GDRIVE_MCP_AUDIT_LOG") };

        let body = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["tool"], "read_document");
        assert_eq!(first["outcome"], "ok");
        assert_eq!(first["user"], "me@example.com");
        assert!(first["ts"].as_str().unwrap().contains('T'));
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["user"], Value::Null);
    }
}
