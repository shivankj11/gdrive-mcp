//! The MCP tools, grouped exactly as in the Python implementation.
//!
//! Each module exposes [`defs`](discovery::defs)-style metadata (name, description, JSON Schema)
//! and an async `dispatch` that runs one tool by name. The server derives each tool's accepted
//! argument list from its schema's `properties`, so a schema and its handler can never disagree
//! about which arguments exist.

pub mod calendar;
pub mod discovery;
pub mod docs;
pub mod files;
pub mod sheets;

use serde_json::{Map, Value};

use crate::args::Args;
use crate::clients::GoogleApi;
use crate::error::Result;

/// One embedded image, ready to become an MCP image content block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub data: Vec<u8>,
    /// The content-type subtype, e.g. `png`.
    pub format: String,
}

/// What a tool hands back: a JSON document, or viewable images.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutput {
    Json(Value),
    Images(Vec<Image>),
}

impl From<Value> for ToolOutput {
    fn from(v: Value) -> Self {
        ToolOutput::Json(v)
    }
}

/// A tool's advertised metadata.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// A JSON Schema object; `additionalProperties` is forced to `false` by [`ToolDef::new`].
    pub schema: Value,
}

impl ToolDef {
    pub fn new(name: &'static str, description: &'static str, mut schema: Value) -> ToolDef {
        if let Some(obj) = schema.as_object_mut() {
            obj.entry("type").or_insert_with(|| Value::from("object"));
            obj.insert("additionalProperties".into(), Value::Bool(false));
        }
        ToolDef { name, description, schema }
    }

    /// The argument names this tool accepts, read off the schema so the two cannot drift.
    pub fn params(&self) -> Vec<&str> {
        self.schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

/// Merge `extra`'s keys into a JSON object in place — the Rust spelling of `{**a, **b}`.
pub fn merge(base: &mut Value, extra: Map<String, Value>) {
    if let Some(obj) = base.as_object_mut() {
        for (k, v) in extra {
            obj.insert(k, v);
        }
    }
}

/// Every tool, in the order the Python server registered them (discovery, sheets, docs, files).
pub fn all_defs() -> Vec<ToolDef> {
    let mut out = discovery::defs();
    out.extend(sheets::defs());
    out.extend(docs::defs());
    out.extend(files::defs());
    out.extend(calendar::defs());
    out
}

/// Run one tool by name. Unknown names are a caller error, not a panic.
pub async fn dispatch(name: &str, api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    if let Some(r) = discovery::dispatch(name, api, args).await {
        return r;
    }
    if let Some(r) = sheets::dispatch(name, api, args).await {
        return r;
    }
    if let Some(r) = docs::dispatch(name, api, args).await {
        return r;
    }
    if let Some(r) = files::dispatch(name, api, args).await {
        return r;
    }
    if let Some(r) = calendar::dispatch(name, api, args).await {
        return r;
    }
    Err(crate::error::ToolError::msg(format!("unknown tool: {name}")))
}

#[cfg(test)]
pub mod testing {
    //! A recording fake of [`GoogleApi`] for the tool tests.
    //!
    //! Responses are queued per method name and consumed in order; a method with nothing queued
    //! answers with an empty object (the Python tests' fake services behaved the same way), so a
    //! test only has to script the calls it actually cares about. Every call is recorded so a
    //! test can assert on the request a tool built — which is where most of the interesting
    //! behaviour lives (batchUpdate request order, A1 ranges, `supportsAllDrives`, and so on).

    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::{json, Value};

    use crate::clients::{CalendarEventsListParams, GoogleApi};
    use crate::error::{Result, ToolError};

    #[derive(Debug, Clone, PartialEq)]
    pub struct Call {
        pub method: &'static str,
        pub args: Value,
    }

    #[derive(Default)]
    pub struct FakeApi {
        queued: Mutex<HashMap<String, VecDeque<Result<Value>>>>,
        bytes: Mutex<HashMap<String, VecDeque<Result<Vec<u8>>>>>,
        calls: Mutex<Vec<Call>>,
        email: Mutex<Option<String>>,
        image: Mutex<Option<(Vec<u8>, String)>>,
    }

    impl FakeApi {
        pub fn new() -> Self {
            FakeApi::default()
        }

        /// Queue the next JSON response for `method`.
        pub fn on(&self, method: &str, value: Value) -> &Self {
            self.queued.lock().unwrap().entry(method.into()).or_default().push_back(Ok(value));
            self
        }

        /// Queue the next failure for `method`.
        pub fn fail(&self, method: &str, err: ToolError) -> &Self {
            self.queued.lock().unwrap().entry(method.into()).or_default().push_back(Err(err));
            self
        }

        /// Queue the next raw-bytes response for `method` (`drive_get_media`, `drive_export`).
        pub fn on_bytes(&self, method: &str, data: Vec<u8>) -> &Self {
            self.bytes.lock().unwrap().entry(method.into()).or_default().push_back(Ok(data));
            self
        }

        pub fn with_email(&self, email: &str) -> &Self {
            *self.email.lock().unwrap() = Some(email.to_string());
            self
        }

        pub fn with_image(&self, data: Vec<u8>, format: &str) -> &Self {
            *self.image.lock().unwrap() = Some((data, format.to_string()));
            self
        }

        /// Every call made, in order.
        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        /// The arguments of every call to `method`, in order.
        pub fn calls_to(&self, method: &str) -> Vec<Value> {
            self.calls.lock().unwrap().iter().filter(|c| c.method == method).map(|c| c.args.clone()).collect()
        }

        /// The arguments of the last call to `method`; panics if it was never called.
        pub fn last(&self, method: &str) -> Value {
            self.calls_to(method).pop().unwrap_or_else(|| panic!("no call to {method}"))
        }

        pub fn call_count(&self, method: &str) -> usize {
            self.calls_to(method).len()
        }

        fn record(&self, method: &'static str, args: Value) {
            self.calls.lock().unwrap().push(Call { method, args });
        }

        fn take(&self, method: &'static str, args: Value) -> Result<Value> {
            self.record(method, args);
            self.queued
                .lock()
                .unwrap()
                .get_mut(method)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Ok(json!({})))
        }

        fn take_bytes(&self, method: &'static str, args: Value) -> Result<Vec<u8>> {
            self.record(method, args);
            self.bytes
                .lock()
                .unwrap()
                .get_mut(method)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    #[async_trait]
    impl GoogleApi for FakeApi {
        async fn drive_files_get(&self, file_id: &str, fields: &str) -> Result<Value> {
            self.take("drive_files_get", json!({"file_id": file_id, "fields": fields}))
        }
        async fn drive_files_list(
            &self,
            q: &str,
            page_size: i64,
            page_token: Option<&str>,
            fields: &str,
            order_by: &str,
        ) -> Result<Value> {
            self.take(
                "drive_files_list",
                json!({"q": q, "page_size": page_size, "page_token": page_token, "fields": fields, "order_by": order_by}),
            )
        }
        async fn drive_get_media(&self, file_id: &str) -> Result<Vec<u8>> {
            self.take_bytes("drive_get_media", json!({"file_id": file_id}))
        }
        async fn drive_export(&self, file_id: &str, mime_type: &str) -> Result<Vec<u8>> {
            self.take_bytes("drive_export", json!({"file_id": file_id, "mime_type": mime_type}))
        }
        async fn drive_files_update(
            &self,
            file_id: &str,
            body: &Value,
            params: &[(String, String)],
            fields: &str,
        ) -> Result<Value> {
            let params: Value = params
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect::<serde_json::Map<_, _>>()
                .into();
            self.take(
                "drive_files_update",
                json!({"file_id": file_id, "body": body, "params": params, "fields": fields}),
            )
        }
        async fn drive_files_create_media(
            &self,
            metadata: &Value,
            mime_type: &str,
            data: Vec<u8>,
            fields: &str,
        ) -> Result<Value> {
            self.take(
                "drive_files_create_media",
                json!({"metadata": metadata, "mime_type": mime_type, "bytes": data.len(), "fields": fields}),
            )
        }
        async fn drive_files_update_media(
            &self,
            file_id: &str,
            metadata: &Value,
            mime_type: &str,
            data: Vec<u8>,
            fields: &str,
        ) -> Result<Value> {
            self.take("drive_files_update_media", json!({"file_id": file_id, "metadata": metadata, "mime_type": mime_type, "bytes": data.len(), "fields": fields}))
        }
        async fn drive_comments_list(
            &self,
            file_id: &str,
            page_size: i64,
            page_token: Option<&str>,
            fields: &str,
        ) -> Result<Value> {
            self.take("drive_comments_list", json!({"file_id": file_id, "page_size": page_size, "page_token": page_token, "fields": fields}))
        }
        async fn drive_comments_create(&self, file_id: &str, content: &str, fields: &str) -> Result<Value> {
            self.take(
                "drive_comments_create",
                json!({"file_id": file_id, "content": content, "fields": fields}),
            )
        }
        async fn drive_about(&self, fields: &str) -> Result<Value> {
            self.take("drive_about", json!({"fields": fields}))
        }
        async fn docs_get(&self, document_id: &str) -> Result<Value> {
            self.take("docs_get", json!({"document_id": document_id}))
        }
        async fn docs_create(&self, title: &str, fields: &str) -> Result<Value> {
            self.take("docs_create", json!({"title": title, "fields": fields}))
        }
        async fn docs_batch_update(&self, document_id: &str, body: &Value) -> Result<Value> {
            self.take("docs_batch_update", json!({"document_id": document_id, "body": body}))
        }
        async fn sheets_get(&self, spreadsheet_id: &str, fields: &str) -> Result<Value> {
            self.take("sheets_get", json!({"spreadsheet_id": spreadsheet_id, "fields": fields}))
        }
        async fn sheets_create(&self, body: &Value, fields: &str) -> Result<Value> {
            self.take("sheets_create", json!({"body": body, "fields": fields}))
        }
        async fn sheets_values_get(&self, spreadsheet_id: &str, range: &str, render: &str) -> Result<Value> {
            self.take(
                "sheets_values_get",
                json!({"spreadsheet_id": spreadsheet_id, "range": range, "render": render}),
            )
        }
        async fn sheets_values_update(
            &self,
            spreadsheet_id: &str,
            range: &str,
            value_input: &str,
            values: &Value,
        ) -> Result<Value> {
            self.take("sheets_values_update", json!({"spreadsheet_id": spreadsheet_id, "range": range, "value_input": value_input, "values": values}))
        }
        async fn sheets_values_append(
            &self,
            spreadsheet_id: &str,
            range: &str,
            value_input: &str,
            values: &Value,
        ) -> Result<Value> {
            self.take("sheets_values_append", json!({"spreadsheet_id": spreadsheet_id, "range": range, "value_input": value_input, "values": values}))
        }
        async fn sheets_values_clear(&self, spreadsheet_id: &str, range: &str) -> Result<Value> {
            self.take("sheets_values_clear", json!({"spreadsheet_id": spreadsheet_id, "range": range}))
        }
        async fn sheets_batch_update(&self, spreadsheet_id: &str, body: &Value) -> Result<Value> {
            self.take("sheets_batch_update", json!({"spreadsheet_id": spreadsheet_id, "body": body}))
        }
        async fn calendar_list(
            &self,
            max_results: i64,
            page_token: Option<&str>,
            min_access_role: Option<&str>,
            show_hidden: bool,
        ) -> Result<Value> {
            self.take(
                "calendar_list",
                json!({
                    "max_results": max_results,
                    "page_token": page_token,
                    "min_access_role": min_access_role,
                    "show_hidden": show_hidden,
                }),
            )
        }
        async fn calendar_events_list(&self, params: CalendarEventsListParams<'_>) -> Result<Value> {
            self.take(
                "calendar_events_list",
                json!({
                    "calendar_id": params.calendar_id,
                    "time_min": params.time_min,
                    "time_max": params.time_max,
                    "query": params.query,
                    "max_results": params.max_results,
                    "page_token": params.page_token,
                    "show_deleted": params.show_deleted,
                }),
            )
        }
        async fn calendar_events_get(&self, calendar_id: &str, event_id: &str) -> Result<Value> {
            self.take("calendar_events_get", json!({"calendar_id": calendar_id, "event_id": event_id}))
        }
        async fn calendar_freebusy(&self, body: &Value) -> Result<Value> {
            self.take("calendar_freebusy", json!({"body": body}))
        }
        async fn calendar_events_insert(
            &self,
            calendar_id: &str,
            body: &Value,
            send_updates: &str,
            conference_data_version: i64,
        ) -> Result<Value> {
            self.take(
                "calendar_events_insert",
                json!({
                    "calendar_id": calendar_id,
                    "body": body,
                    "send_updates": send_updates,
                    "conference_data_version": conference_data_version,
                }),
            )
        }
        async fn calendar_events_patch(
            &self,
            calendar_id: &str,
            event_id: &str,
            body: &Value,
            send_updates: &str,
            conference_data_version: i64,
            etag: Option<&str>,
        ) -> Result<Value> {
            self.take(
                "calendar_events_patch",
                json!({
                    "calendar_id": calendar_id,
                    "event_id": event_id,
                    "body": body,
                    "send_updates": send_updates,
                    "conference_data_version": conference_data_version,
                    "etag": etag,
                }),
            )
        }
        async fn calendar_events_delete(
            &self,
            calendar_id: &str,
            event_id: &str,
            send_updates: &str,
            etag: Option<&str>,
        ) -> Result<Value> {
            self.take(
                "calendar_events_delete",
                json!({
                    "calendar_id": calendar_id,
                    "event_id": event_id,
                    "send_updates": send_updates,
                    "etag": etag,
                }),
            )
        }
        async fn fetch_image(&self, uri: &str) -> Result<Option<(Vec<u8>, String)>> {
            self.record("fetch_image", json!({"uri": uri}));
            Ok(self.image.lock().unwrap().clone())
        }
        async fn authed_user_email(&self) -> Option<String> {
            self.email.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_tool_forbids_unknown_arguments_and_names_itself() {
        let defs = all_defs();
        assert_eq!(defs.len(), 37, "tool count should match the Python server");
        let mut seen = std::collections::HashSet::new();
        for d in &defs {
            assert!(seen.insert(d.name), "duplicate tool name {}", d.name);
            assert_eq!(d.schema["additionalProperties"], json!(false), "{} allows extras", d.name);
            assert_eq!(d.schema["type"], json!("object"), "{} is not an object schema", d.name);
            assert!(!d.description.trim().is_empty(), "{} has no description", d.name);
        }
    }

    #[test]
    fn optional_arguments_are_declared_the_same_way_everywhere() {
        // Optionality is expressed by absence from `required`, never by a null default: a
        // `{"type": "string", "default": null}` is not even a valid schema against itself, and
        // having some modules spell it that way while others omit it hands agents two shapes for
        // one idea.
        for d in all_defs() {
            let props = d.schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
            for (name, spec) in props {
                assert_ne!(
                    spec.get("default"),
                    Some(&Value::Null),
                    "{}.{name} declares a null default",
                    d.name
                );
                assert!(
                    spec.get("type").and_then(Value::as_str).is_some(),
                    "{}.{name} has no scalar type",
                    d.name
                );
            }
        }
    }

    /// The two opt-in gates exactly as the README advertises them: the dry-run bullet names the
    /// edit tools for *existing* files, and a ᶜ marks a tool that "returns an impact preview
    /// unless called with `confirm=true`". Written out here rather than derived from the schemas —
    /// these lists are the agent-facing promise, so a tool silently gaining a flag (with no
    /// preview branch behind it) or losing one the README still advertises is a documented lie
    /// either way. `server.rs` pins the confirm set a second time against `DESTRUCTIVE`; keeping a
    /// copy sourced from the README means updating that const cannot bless the change.
    const README_DRY_RUN_TOOLS: &[&str] = &[
        "append_text",
        "insert_text",
        "insert_table",
        "delete_text",
        "replace_text",
        "write_sheet",
        "append_rows",
        "format_cells",
        "clear_range",
        "delete_rows",
        "create_event",
        "update_event",
        "delete_event",
        "respond_to_event",
    ];
    const README_CONFIRM_TOOLS: &[&str] = &[
        "delete_text",
        "replace_text",
        "write_sheet",
        "clear_range",
        "delete_rows",
        "upload_file",
        "move_file",
        "rename_file",
        "create_event",
        "update_event",
        "delete_event",
        "respond_to_event",
    ];

    #[test]
    fn dry_run_and_confirm_are_offered_by_exactly_the_tools_the_readme_marks() {
        let defs = all_defs();
        for (flag, expected) in [("dry_run", README_DRY_RUN_TOOLS), ("confirm", README_CONFIRM_TOOLS)] {
            let offered: Vec<&str> =
                defs.iter().filter(|d| d.params().contains(&flag)).map(|d| d.name).collect();
            assert_eq!(
                offered.iter().copied().collect::<std::collections::BTreeSet<_>>(),
                expected.iter().copied().collect::<std::collections::BTreeSet<_>>(),
                "the tools offering {flag} are not the ones the README names"
            );
            assert_eq!(offered.len(), expected.len(), "{flag} is declared on a duplicate tool name");
        }
    }

    #[test]
    fn both_gates_default_to_false_so_an_omitted_flag_never_reads_as_permission() {
        // Omitting `confirm` must mean "not confirmed" and omitting `dry_run` must mean "write for
        // real" — a `true` default would gate every call forever, and a missing default would let
        // an unset flag arrive as null and leave the audit record unable to say which way it ran.
        for d in all_defs() {
            let props = d.schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
            let required = d.schema.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
            for flag in ["confirm", "dry_run"] {
                let Some(spec) = props.get(flag) else { continue };
                assert_eq!(spec.get("type"), Some(&json!("boolean")), "{}.{flag} is not a boolean", d.name);
                assert_eq!(
                    spec.get("default"),
                    Some(&json!(false)),
                    "{}.{flag} does not default to false",
                    d.name
                );
                assert!(!required.contains(&json!(flag)), "{}.{flag} is required", d.name);
            }
        }
    }

    #[test]
    fn declared_required_arguments_all_exist_as_properties() {
        for d in all_defs() {
            let props = d.schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
            for req in d.schema.get("required").and_then(Value::as_array).cloned().unwrap_or_default() {
                let name = req.as_str().unwrap();
                assert!(props.contains_key(name), "{}: required arg {name} is not a property", d.name);
            }
        }
    }
}
