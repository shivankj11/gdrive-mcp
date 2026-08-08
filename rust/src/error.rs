//! Turn Google API failures into concise, actionable messages for MCP clients.
//!
//! `ToolError::Msg` is the Rust equivalent of the Python port's `RuntimeError`: a message the
//! agent is meant to read and act on. `ToolError::Api` carries the HTTP status so callers that
//! need to branch on it (e.g. `insert_table` rewriting a 400 into locator advice) still can,
//! while rendering exactly like `errors.explain()` did.

use std::fmt;

/// Status-keyed hints appended to a Google API error message.
fn hint(status: u16) -> &'static str {
    match status {
        403 => " (is the API enabled for this project and the drive scope granted?)",
        404 => " (check the ID/URL and that your account has access)",
        429 => " (rate limited — retry shortly)",
        _ => "",
    }
}

#[derive(Debug, Clone)]
pub enum ToolError {
    /// A message meant for the calling agent.
    Msg(String),
    /// A non-2xx response from a Google API.
    Api { status: u16, reason: String },
}

impl ToolError {
    pub fn msg(m: impl Into<String>) -> Self {
        ToolError::Msg(m.into())
    }

    /// The HTTP status, when this came from a Google API.
    pub fn status(&self) -> Option<u16> {
        match self {
            ToolError::Api { status, .. } => Some(*status),
            ToolError::Msg(_) => None,
        }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::Msg(m) => write!(f, "{m}"),
            ToolError::Api { status, reason } => {
                write!(f, "Google API error {status}: {reason}{}", hint(*status))
            }
        }
    }
}

impl std::error::Error for ToolError {}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::Msg(format!("{}: {e}", io_kind(&e)))
    }
}

fn io_kind(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        _ => "OSError",
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::Msg(format!("JSONDecodeError: {e}"))
    }
}

impl From<reqwest::Error> for ToolError {
    fn from(e: reqwest::Error) -> Self {
        match e.status() {
            Some(s) => ToolError::Api { status: s.as_u16(), reason: e.to_string() },
            None => ToolError::Msg(format!("HttpError: {e}")),
        }
    }
}

pub type Result<T> = std::result::Result<T, ToolError>;

/// The `errors.explain()` rendering, for callers holding a `&dyn Error`.
pub fn explain(e: &ToolError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_errors_carry_a_status_specific_hint() {
        let e = ToolError::Api { status: 404, reason: "Not Found".into() };
        assert_eq!(
            e.to_string(),
            "Google API error 404: Not Found (check the ID/URL and that your account has access)"
        );
        assert_eq!(e.status(), Some(404));
    }

    #[test]
    fn unknown_statuses_get_no_hint() {
        let e = ToolError::Api { status: 500, reason: "boom".into() };
        assert_eq!(e.to_string(), "Google API error 500: boom");
    }

    #[test]
    fn plain_messages_pass_through() {
        assert_eq!(ToolError::msg("nope").to_string(), "nope");
    }
}
