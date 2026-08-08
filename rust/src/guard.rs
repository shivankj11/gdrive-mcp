//! Confirm-before-destructive helper.
//!
//! Destructive tools take `confirm=false`. When not confirmed they compute a live impact
//! preview and return it via [`preview_response`] WITHOUT mutating; the caller re-invokes with
//! `confirm=true` to execute.

use serde_json::{json, Value};

pub fn preview_response(action: &str, impact: Value) -> Value {
    preview_response_with(action, impact, None)
}

pub fn preview_response_with(action: &str, impact: Value, message: Option<&str>) -> Value {
    json!({
        "status": "confirmation_required",
        "action": action,
        "impact": impact,
        "next": message.unwrap_or("Re-call the same tool with confirm=true to execute."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_names_the_action_and_never_claims_success() {
        let r = preview_response("delete_text", json!({"chars": 12}));
        assert_eq!(r["status"], "confirmation_required");
        assert_eq!(r["action"], "delete_text");
        assert_eq!(r["impact"]["chars"], 12);
        assert!(r["next"].as_str().unwrap().contains("confirm=true"));
    }
}
