//! Strict tool-argument extraction.
//!
//! The Python server set pydantic's `extra = "forbid"` on every generated argument model so an
//! unknown argument is rejected instead of silently dropped — a misspelled `confirm` must never
//! read as "not confirmed". [`Args`] reproduces that: construction fails on any key the tool did
//! not declare, and each accessor type-checks the value it reads.
//!
//! JSON `null` is treated as "absent", so a client that spells an unset optional argument
//! explicitly gets the documented default rather than a type error.
//!
//! Strict about *which* arguments exist, forgiving about how they are spelled. FastMCP validated
//! in pydantic's lax mode behind a `pre_parse_json` shim, because real MCP clients routinely send
//! `"[[\"a\"]]"` for a list and `"true"` for a boolean; a port that only accepted the strict JSON
//! types would reject calls the Python server executed. [`Args`] reproduces both layers.

use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::error::{Result, ToolError};

/// The string spellings pydantic accepts for a boolean in lax mode.
const TRUE_WORDS: &[&str] = &["1", "on", "t", "true", "y", "yes"];
const FALSE_WORDS: &[&str] = &["0", "off", "f", "false", "n", "no"];

#[derive(Debug, Clone)]
pub struct Args {
    tool: String,
    obj: Map<String, Value>,
}

impl Args {
    /// Reject unknown keys up front; `allowed` is the tool's full parameter list.
    pub fn new(tool: &str, arguments: Option<Map<String, Value>>, allowed: &[&str]) -> Result<Args> {
        let obj = arguments.unwrap_or_default();
        let mut unexpected: Vec<&str> =
            obj.keys().map(String::as_str).filter(|k| !allowed.contains(k)).collect();
        if !unexpected.is_empty() {
            unexpected.sort_unstable();
            return Err(ToolError::msg(format!(
                "{tool}: unexpected argument(s) {}; accepted arguments are {}",
                unexpected.join(", "),
                allowed.join(", ")
            )));
        }
        Ok(Args { tool: tool.to_string(), obj })
    }

    /// The raw argument map — what the audit log filters down to safe keys.
    pub fn map(&self) -> &Map<String, Value> {
        &self.obj
    }

    fn present(&self, key: &str) -> Option<&Value> {
        match self.obj.get(key) {
            Some(Value::Null) | None => None,
            Some(v) => Some(v),
        }
    }

    /// FastMCP's `pre_parse_json`, for the accessors that expect a container.
    ///
    /// Clients that cannot express a nested list often send it as a JSON *string*. The result is
    /// only substituted when it parses to a list or an object: a string or number that happens to
    /// parse (a sheet tab literally named `12`, say) must stay verbatim, which is exactly the
    /// `isinstance(pre_parsed, (str, int, float))` skip in the Python. Booleans fall into that
    /// same skip there — `isinstance(True, int)` — so they are handled by [`Args::opt_bool`]'s
    /// coercion instead of here.
    fn container(&self, key: &str) -> Option<Cow<'_, Value>> {
        let raw = self.present(key)?;
        if let Value::String(s) = raw {
            match serde_json::from_str::<Value>(s) {
                Ok(parsed @ (Value::Array(_) | Value::Object(_))) => return Some(Cow::Owned(parsed)),
                // `json.loads("null")` substitutes None, which reads as "argument not given".
                Ok(Value::Null) => return None,
                _ => {}
            }
        }
        Some(Cow::Borrowed(raw))
    }

    fn wrong_type(&self, key: &str, want: &str, got: &Value) -> ToolError {
        ToolError::msg(format!("{}: argument '{key}' must be {want}, got {}", self.tool, type_name(got)))
    }

    fn missing(&self, key: &str) -> ToolError {
        ToolError::msg(format!("{}: missing required argument '{key}'", self.tool))
    }

    pub fn opt_str(&self, key: &str) -> Result<Option<String>> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => Err(self.wrong_type(key, "a string", other)),
        }
    }

    pub fn req_str(&self, key: &str) -> Result<String> {
        self.opt_str(key)?.ok_or_else(|| self.missing(key))
    }

    pub fn str_or(&self, key: &str, default: &str) -> Result<String> {
        Ok(self.opt_str(key)?.unwrap_or_else(|| default.to_string()))
    }

    pub fn opt_bool(&self, key: &str) -> Result<Option<bool>> {
        match self.present(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            // pydantic's lax mode accepted these spellings, and clients do send them. Getting
            // this wrong would read a caller's `"true"` as "not confirmed" and gate a call the
            // Python server would have run.
            Some(Value::String(s)) => {
                let word = s.trim().to_ascii_lowercase();
                if TRUE_WORDS.contains(&word.as_str()) {
                    Ok(Some(true))
                } else if FALSE_WORDS.contains(&word.as_str()) {
                    Ok(Some(false))
                } else {
                    Err(self.wrong_type(key, "a boolean", &Value::String(s.clone())))
                }
            }
            Some(Value::Number(n)) => match n.as_f64() {
                Some(1.0) => Ok(Some(true)),
                Some(0.0) => Ok(Some(false)),
                _ => Err(self.wrong_type(key, "a boolean", &Value::Number(n.clone()))),
            },
            Some(other) => Err(self.wrong_type(key, "a boolean", other)),
        }
    }

    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool> {
        Ok(self.opt_bool(key)?.unwrap_or(default))
    }

    pub fn opt_i64(&self, key: &str) -> Result<Option<i64>> {
        let not_an_int = || ToolError::msg(format!("{}: argument '{key}' must be an integer", self.tool));
        match self.present(key) {
            None => Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                // A JSON number that is integral but typed as a float ("2.0") is still an int.
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
                .map(Some)
                .ok_or_else(not_an_int),
            // A numeric string, which pydantic's lax mode also accepted.
            Some(Value::String(s)) => {
                let t = s.trim();
                t.parse::<i64>()
                    .ok()
                    .or_else(|| t.parse::<f64>().ok().filter(|f| f.fract() == 0.0).map(|f| f as i64))
                    .map(Some)
                    .ok_or_else(not_an_int)
            }
            Some(other) => Err(self.wrong_type(key, "an integer", other)),
        }
    }

    pub fn i64_or(&self, key: &str, default: i64) -> Result<i64> {
        Ok(self.opt_i64(key)?.unwrap_or(default))
    }

    /// A list of strings, e.g. `create_spreadsheet(tabs=[...])`.
    pub fn opt_str_list(&self, key: &str) -> Result<Option<Vec<String>>> {
        match self.container(key).as_deref() {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    other => Err(self.wrong_type(key, "a list of strings", other)),
                })
                .collect::<Result<Vec<_>>>()
                .map(Some),
            Some(other) => Err(self.wrong_type(key, "a list of strings", other)),
        }
    }

    /// A list of JSON objects, used for structured Calendar reminder inputs.
    pub fn opt_object_list(&self, key: &str) -> Result<Option<Vec<Map<String, Value>>>> {
        match self.container(key).as_deref() {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| match v {
                    Value::Object(obj) => Ok(obj.clone()),
                    other => Err(self.wrong_type(key, "a list of objects", other)),
                })
                .collect::<Result<Vec<_>>>()
                .map(Some),
            Some(other) => Err(self.wrong_type(key, "a list of objects", other)),
        }
    }

    /// A grid: a list of row lists whose cells stay untyped (`write_sheet`, `insert_table`).
    /// Shape beyond "list of lists" is the caller's to validate — Sheets accepts ragged rows,
    /// Docs tables do not.
    pub fn req_rows(&self, key: &str) -> Result<Vec<Vec<Value>>> {
        let Some(value) = self.container(key) else {
            return Err(self.missing(key));
        };
        let value = value.as_ref();
        let Value::Array(rows) = value else {
            return Err(ToolError::msg(format!(
                "{}: argument '{key}' must be a list of row lists, got {}",
                self.tool,
                type_name(value)
            )));
        };
        rows.iter()
            .enumerate()
            .map(|(i, row)| match row {
                Value::Array(cells) => Ok(cells.clone()),
                _ => Err(ToolError::msg(format!("row {i} must be a list of cell values"))),
            })
            .collect()
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value, allowed: &[&str]) -> Result<Args> {
        let Value::Object(map) = v else { panic!("test args must be an object") };
        Args::new("demo", Some(map), allowed)
    }

    #[test]
    fn unknown_arguments_are_rejected_rather_than_dropped() {
        let err = args(json!({"item": "x", "confrim": true}), &["item", "confirm"]).unwrap_err();
        assert!(err.to_string().contains("unexpected argument(s) confrim"), "{err}");
    }

    #[test]
    fn explicit_null_reads_as_the_documented_default() {
        let a = args(json!({"tab": null, "confirm": null}), &["tab", "confirm"]).unwrap();
        assert_eq!(a.opt_str("tab").unwrap(), None);
        assert!(!a.bool_or("confirm", false).unwrap());
    }

    #[test]
    fn wrong_types_are_reported_with_the_argument_name() {
        let a = args(json!({"confirm": ["yes"]}), &["confirm"]).unwrap();
        let err = a.bool_or("confirm", false).unwrap_err();
        assert!(err.to_string().contains("'confirm' must be a boolean"), "{err}");
    }

    #[test]
    fn missing_required_arguments_say_so() {
        let a = args(json!({}), &["item"]).unwrap();
        assert!(a.req_str("item").unwrap_err().to_string().contains("missing required argument 'item'"));
    }

    #[test]
    fn a_list_argument_sent_as_a_json_string_is_re_parsed() {
        // What Claude Desktop and friends actually send; FastMCP's pre_parse_json existed for it.
        let a = args(json!({"rows": "[[\"a\", 1], [\"b\", 2]]"}), &["rows"]).unwrap();
        assert_eq!(a.req_rows("rows").unwrap().len(), 2);

        let t = args(json!({"tabs": "[\"Q1\", \"Q2\"]"}), &["tabs"]).unwrap();
        assert_eq!(t.opt_str_list("tabs").unwrap().unwrap(), vec!["Q1", "Q2"]);
    }

    #[test]
    fn a_string_that_merely_looks_numeric_is_left_alone() {
        // pre_parse_json skips a parse that yields a str/int/float, so a tab named "12" survives.
        let a = args(json!({"tab": "12"}), &["tab"]).unwrap();
        assert_eq!(a.opt_str("tab").unwrap().as_deref(), Some("12"));
    }

    #[test]
    fn booleans_and_integers_accept_the_spellings_pydantic_coerced() {
        for (raw, want) in [("true", true), ("True", true), ("yes", true), ("1", true), ("on", true)] {
            let a = args(json!({ "confirm": raw }), &["confirm"]).unwrap();
            assert_eq!(a.bool_or("confirm", false).unwrap(), want, "{raw}");
        }
        for raw in ["false", "FALSE", "no", "0", "off"] {
            let a = args(json!({ "confirm": raw }), &["confirm"]).unwrap();
            assert!(!a.bool_or("confirm", true).unwrap(), "{raw}");
        }
        assert!(args(json!({"confirm": 1}), &["confirm"]).unwrap().bool_or("confirm", false).unwrap());
        assert_eq!(args(json!({"chunk": "3"}), &["chunk"]).unwrap().i64_or("chunk", 0).unwrap(), 3);
    }

    #[test]
    fn a_word_that_is_not_a_boolean_is_still_rejected() {
        let a = args(json!({"confirm": "maybe"}), &["confirm"]).unwrap();
        assert!(a.bool_or("confirm", false).unwrap_err().to_string().contains("must be a boolean"));
    }

    #[test]
    fn integral_floats_count_as_integers() {
        let a = args(json!({"chunk": 2.0}), &["chunk"]).unwrap();
        assert_eq!(a.i64_or("chunk", 0).unwrap(), 2);
    }

    #[test]
    fn rows_must_be_a_list_of_lists() {
        let a = args(json!({"rows": [["a", 1], ["b", 2]]}), &["rows"]).unwrap();
        assert_eq!(a.req_rows("rows").unwrap().len(), 2);

        let bad = args(json!({"rows": ["a", "b"]}), &["rows"]).unwrap();
        assert!(bad.req_rows("rows").unwrap_err().to_string().contains("row 0 must be a list"));
    }
}
