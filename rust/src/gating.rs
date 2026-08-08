//! Per-action manual-verification gate, hardened against model spoofing/delegation.
//!
//! The gate decision is **escalate-only** so a self-reported model can never *downgrade* policy:
//!
//!   - `GDRIVE_MCP_REQUIRE_VERIFICATION=always` (operator-set at launch) gates every call.
//!     This is the anti-delegation lever: a caller routed through the same server cannot escape it.
//!   - otherwise, a call is gated when either the operator pin `GDRIVE_MCP_CALLING_MODEL` or the
//!     request's self-reported `_meta.model` matches a case-insensitive substring from the
//!     comma-separated `GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS` setting. A nonmatching request
//!     model cannot turn off a match from the operator pin.
//!
//! When gated, every action needs per-call user approval via MCP elicitation. It fails closed on
//! decline or if the client cannot prompt. `_meta.model` is advisory only; deployment-level policy
//! should use the operator-controlled environment variables.

use rmcp::service::{ElicitationError, Peer, RequestContext};
use rmcp::{elicit_safe, RoleServer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Result, ToolError};

const TRUTHY: &[&str] = &["always", "1", "true", "yes", "on"];

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct Approval {
    /// Approve this verification-gated Google Drive action?
    #[serde(deserialize_with = "lenient_bool")]
    approved: bool,
}
elicit_safe!(Approval);

/// Accept the boolean spellings pydantic's lax mode did.
///
/// The advertised schema still says `boolean`, but a client that answers the prompt with
/// `"true"` or `1` meant yes — and strict deserialization would turn that approval into a
/// parse error, which the gate reports as "the client cannot prompt" and fails closed on. A
/// user who clicked approve must not be told the action was blocked.
fn lenient_bool<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    use serde::de::Error as _;
    match Value::deserialize(d)? {
        Value::Bool(b) => Ok(b),
        Value::Number(n) if n.as_f64() == Some(1.0) => Ok(true),
        Value::Number(n) if n.as_f64() == Some(0.0) => Ok(false),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
            other => Err(D::Error::custom(format!("not a boolean: {other}"))),
        },
        other => Err(D::Error::custom(format!("not a boolean: {other}"))),
    }
}

/// What the gate needs from the in-flight request: the caller's self-reported model and a peer
/// to prompt through. Kept separate from rmcp's `RequestContext` so the policy logic is testable
/// without a live session.
pub struct GateCtx<'a> {
    pub model: Option<String>,
    pub peer: Option<&'a Peer<RoleServer>>,
}

impl<'a> GateCtx<'a> {
    pub fn from_request(ctx: &'a RequestContext<RoleServer>) -> GateCtx<'a> {
        GateCtx {
            model: ctx.meta.0.get("model").and_then(Value::as_str).map(str::to_string),
            peer: Some(&ctx.peer),
        }
    }
}

fn normalize_model(value: &str) -> String {
    value.trim().to_lowercase().replace('_', "-")
}

fn verification_model_patterns() -> Vec<String> {
    std::env::var("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS")
        .unwrap_or_default()
        .split(',')
        .filter(|p| !p.trim().is_empty())
        .map(normalize_model)
        .collect()
}

pub fn model_requires_verification(model: Option<&str>) -> bool {
    let Some(model) = model.filter(|m| !m.is_empty()) else { return false };
    let normalized = normalize_model(model);
    verification_model_patterns().iter().any(|p| normalized.contains(p.as_str()))
}

fn env_pin() -> Option<String> {
    std::env::var("GDRIVE_MCP_CALLING_MODEL").ok().filter(|v| !v.is_empty())
}

/// Best-known caller model, for display only (advisory; never trusted to downgrade).
pub fn calling_model(ctx: Option<&GateCtx<'_>>) -> Option<String> {
    ctx.and_then(|c| c.model.clone()).or_else(env_pin)
}

/// Whether this call must be user-verified. Escalate-only + operator override.
pub fn gate_required(ctx: Option<&GateCtx<'_>>) -> bool {
    let always = std::env::var("GDRIVE_MCP_REQUIRE_VERIFICATION").unwrap_or_default().trim().to_lowercase();
    if TRUTHY.contains(&always.as_str()) {
        return true;
    }
    model_requires_verification(env_pin().as_deref())
        || model_requires_verification(ctx.and_then(|c| c.model.as_deref()))
}

/// Block until the user approves when the call is gated; no-op otherwise. Fails closed.
pub async fn require_verification(
    ctx: Option<&GateCtx<'_>>,
    tool_name: &str,
    args: &Map<String, Value>,
) -> Result<()> {
    if !gate_required(ctx) {
        return Ok(());
    }
    let Some(peer) = ctx.and_then(|c| c.peer) else {
        return Err(ToolError::msg(format!(
            "'{tool_name}' needs manual user verification but no request context is available; \
             action blocked."
        )));
    };
    let target = ["item", "name", "title"]
        .iter()
        .find_map(|k| args.get(*k).and_then(Value::as_str))
        .unwrap_or("the requested target");
    let model = calling_model(ctx).unwrap_or_else(|| "unknown".into());
    let message = format!("Approve `{tool_name}` on {target}? (verification-gated; caller model: {model})");

    match peer.elicit::<Approval>(message).await {
        Ok(Some(approval)) if approval.approved => Ok(()),
        Ok(_)
        | Err(ElicitationError::UserDeclined)
        | Err(ElicitationError::UserCancelled)
        | Err(ElicitationError::NoContent) => {
            Err(ToolError::msg(format!("'{tool_name}' was not approved by the user; action blocked.")))
        }
        Err(other) => Err(ToolError::msg(format!(
            "'{tool_name}' requires manual user verification but the client cannot prompt the \
             user ({}); action blocked.",
            elicitation_error_kind(&other)
        ))),
    }
}

fn elicitation_error_kind(e: &ElicitationError) -> &'static str {
    match e {
        ElicitationError::Service(_) => "ServiceError",
        ElicitationError::UserDeclined => "UserDeclined",
        ElicitationError::UserCancelled => "UserCancelled",
        ElicitationError::ParseError { .. } => "ParseError",
        ElicitationError::NoContent => "NoContent",
        ElicitationError::CapabilityNotSupported => "CapabilityNotSupported",
        _ => "ElicitationError",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ENV_LOCK;

    struct Env(Vec<(&'static str, Option<String>)>);
    impl Env {
        fn new(pairs: &[(&'static str, Option<&str>)]) -> Env {
            let mut saved = Vec::new();
            for (k, v) in pairs {
                saved.push((*k, std::env::var(k).ok()));
                match v {
                    Some(value) => unsafe { std::env::set_var(k, value) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
            Env(saved)
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(value) => unsafe { std::env::set_var(k, value) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    const PATTERNS: &str = "verification-required, restricted-family";

    #[test]
    fn an_approval_answered_with_a_string_or_number_still_counts() {
        for raw in [r#"{"approved": true}"#, r#"{"approved": "true"}"#, r#"{"approved": 1}"#] {
            let a: Approval = serde_json::from_str(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert!(a.approved, "{raw}");
        }
        for raw in [r#"{"approved": false}"#, r#"{"approved": "no"}"#, r#"{"approved": 0}"#] {
            assert!(!serde_json::from_str::<Approval>(raw).unwrap().approved, "{raw}");
        }
        // Anything that is not a yes/no answer is still a parse error, never a silent approval.
        assert!(serde_json::from_str::<Approval>(r#"{"approved": "later"}"#).is_err());
    }

    fn ctx(model: Option<&str>) -> GateCtx<'static> {
        GateCtx { model: model.map(str::to_string), peer: None }
    }

    #[test]
    fn patterns_match_case_insensitively_and_across_underscores() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _e = Env::new(&[
            ("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", Some(PATTERNS)),
            ("GDRIVE_MCP_CALLING_MODEL", None),
            ("GDRIVE_MCP_REQUIRE_VERIFICATION", None),
        ]);
        assert!(model_requires_verification(Some("vendor-verification-required-v2")));
        assert!(model_requires_verification(Some("RESTRICTED_FAMILY_LATEST")));
        assert!(!model_requires_verification(Some("standard-model")));
        assert!(!model_requires_verification(None));
    }

    #[test]
    fn calling_model_prefers_meta_then_the_operator_pin() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _e = Env::new(&[("GDRIVE_MCP_CALLING_MODEL", Some("operator-pinned-model"))]);
        assert_eq!(calling_model(Some(&ctx(Some("request-model")))).as_deref(), Some("request-model"));
        assert_eq!(calling_model(None).as_deref(), Some("operator-pinned-model"));
    }

    #[tokio::test]
    async fn no_patterns_configured_means_no_gate() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _e = Env::new(&[
            ("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", None),
            ("GDRIVE_MCP_CALLING_MODEL", None),
            ("GDRIVE_MCP_REQUIRE_VERIFICATION", None),
        ]);
        require_verification(None, "read_document", &Map::new()).await.unwrap();
    }

    #[tokio::test]
    async fn a_gated_call_without_a_context_fails_closed() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _e = Env::new(&[
            ("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", Some(PATTERNS)),
            ("GDRIVE_MCP_CALLING_MODEL", Some("verification-required-model")),
            ("GDRIVE_MCP_REQUIRE_VERIFICATION", None),
        ]);
        let err = require_verification(None, "read_document", &Map::new()).await.unwrap_err();
        assert!(err.to_string().contains("no request context is available"), "{err}");
    }

    #[test]
    fn a_nonmatching_request_model_cannot_downgrade_the_operator_pin() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _e = Env::new(&[
            ("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", Some(PATTERNS)),
            ("GDRIVE_MCP_CALLING_MODEL", Some("verification-required-model")),
            ("GDRIVE_MCP_REQUIRE_VERIFICATION", None),
        ]);
        assert!(gate_required(Some(&ctx(Some("standard-model")))));
    }

    #[test]
    fn a_matching_request_model_gates_even_without_a_pin() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _e = Env::new(&[
            ("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", Some(PATTERNS)),
            ("GDRIVE_MCP_CALLING_MODEL", None),
            ("GDRIVE_MCP_REQUIRE_VERIFICATION", None),
        ]);
        assert!(gate_required(Some(&ctx(Some("restricted-family-latest")))));
    }

    #[test]
    fn the_always_override_gates_every_model() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _e = Env::new(&[
            ("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", None),
            ("GDRIVE_MCP_CALLING_MODEL", None),
            ("GDRIVE_MCP_REQUIRE_VERIFICATION", Some("always")),
        ]);
        assert!(gate_required(Some(&ctx(Some("standard-model")))));
        assert!(gate_required(None));
    }

    #[test]
    fn an_unmatched_model_is_not_gated() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _e = Env::new(&[
            ("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", Some(PATTERNS)),
            ("GDRIVE_MCP_CALLING_MODEL", None),
            ("GDRIVE_MCP_REQUIRE_VERIFICATION", None),
        ]);
        assert!(!gate_required(Some(&ctx(Some("standard-model")))));
        assert!(!gate_required(None));
    }
}
