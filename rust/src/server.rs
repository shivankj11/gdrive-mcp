//! MCP server wiring every tool module onto one stdio server.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Map, Value};

use crate::args::Args;
use crate::audit;
use crate::clients::{GoogleApi, GoogleClient};
use crate::error::ToolError;
use crate::gating::{require_verification, GateCtx};
use crate::localfs;
use crate::tools::{self, ToolDef, ToolOutput};

/// Tools that only read (never change Drive or local state).
const READ_ONLY: &[&str] = &[
    "resolve_link",
    "search_files",
    "list_folder",
    "get_metadata",
    "read_sheet",
    "read_document",
    "extract_images",
    "read_comments",
    "read_file_as_text",
];
// Note: read_full_sheet is NOT read-only — it spills a local CSV (a local side effect).
/// Tools that can overwrite/remove existing data.
const DESTRUCTIVE: &[&str] = &[
    "write_sheet",
    "clear_range",
    "delete_rows",
    "move_file",
    "rename_file",
    "upload_file",
    "delete_text",
    "replace_text",
];
// Everything else is additive (append/create/add, or a read that spills a new local file).

fn annotations(name: &str) -> ToolAnnotations {
    if READ_ONLY.contains(&name) {
        return ToolAnnotations::from_raw(None, Some(true), None, None, Some(true));
    }
    if DESTRUCTIVE.contains(&name) {
        return ToolAnnotations::from_raw(None, Some(false), Some(true), None, Some(true));
    }
    ToolAnnotations::from_raw(None, Some(false), Some(false), None, Some(true))
}

fn to_rmcp_tool(def: &ToolDef) -> Tool {
    let schema: Map<String, Value> = def.schema.as_object().cloned().expect("tool schemas are JSON objects");
    Tool::new(def.name, def.description, Arc::new(schema)).with_annotations(annotations(def.name))
}

pub struct GdriveServer {
    api: Arc<dyn GoogleApi>,
    defs: Vec<ToolDef>,
}

impl GdriveServer {
    pub fn new(api: Arc<dyn GoogleApi>) -> Self {
        GdriveServer { api, defs: tools::all_defs() }
    }

    fn find(&self, name: &str) -> Option<&ToolDef> {
        self.defs.iter().find(|d| d.name == name)
    }

    /// The arguments the tool actually ran with: what the caller sent, plus the schema default
    /// for anything they left out.
    ///
    /// FastMCP validated into a fully-populated model before the tool saw it, so Python's audit
    /// log recorded `count: 1` for a `delete_rows` that omitted `count`. Logging only what was
    /// literally sent would leave the record unable to say how many rows went.
    ///
    /// Null defaults are deliberately skipped. Python recorded those too, but `parse_ref(None)`
    /// then wrote `"<unparseable>"` for every unset optional ref — noise rather than a record.
    fn args_as_run(args: &Map<String, Value>, schema: &Value) -> Map<String, Value> {
        let mut out = args.clone();
        let Some(props) = schema.get("properties").and_then(Value::as_object) else {
            return out;
        };
        for (name, spec) in props {
            if out.contains_key(name) {
                continue;
            }
            if let Some(default) = spec.get("default").filter(|d| !d.is_null()) {
                out.insert(name.clone(), default.clone());
            }
        }
        out
    }

    /// Everything `call_tool` does apart from talking to rmcp: argument checking, the
    /// verification gate, dispatch, and the audit record. Split out so the integration tests
    /// can drive it without a live peer.
    async fn run_tool(
        &self,
        name: &str,
        arguments: Option<Map<String, Value>>,
        gate: Option<&GateCtx<'_>>,
    ) -> Result<ToolOutput, ToolError> {
        let Some(def) = self.find(name) else {
            return Err(ToolError::msg(format!("Unknown tool: {name}")));
        };
        let args = Args::new(name, arguments, &def.params())?;
        let user = self.api.authed_user_email().await;

        let outcome = async {
            require_verification(gate, name, args.map()).await?;
            tools::dispatch(name, self.api.as_ref(), &args).await
        }
        .await;

        // The Python gate wrapper logged both paths; the error label distinguishes a Google API
        // failure from a locally-raised one (Python collapsed both into RuntimeError).
        let label = match &outcome {
            Ok(_) => "ok".to_string(),
            Err(e) if e.status().is_some() => "error:HttpError".to_string(),
            Err(_) => "error:RuntimeError".to_string(),
        };
        audit::record(name, &Self::args_as_run(args.map(), &def.schema), &label, user.as_deref());
        outcome
    }
}

fn to_call_result(output: ToolOutput) -> CallToolResult {
    match output {
        // FastMCP rendered a dict return as pretty-printed JSON text; keep that shape so
        // existing prompts and clients see the same payload.
        ToolOutput::Json(v) => CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
        )]),
        ToolOutput::Images(images) => CallToolResult::success(
            images
                .into_iter()
                .map(|img| {
                    use base64::Engine;
                    ContentBlock::image(
                        base64::engine::general_purpose::STANDARD.encode(&img.data),
                        format!("image/{}", img.format),
                    )
                })
                .collect(),
        ),
    }
}

impl ServerHandler for GdriveServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("gdrive", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.defs.iter().map(to_rmcp_tool).collect()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let gate = GateCtx::from_request(&context);
        let result = match self.run_tool(&request.name, request.arguments, Some(&gate)).await {
            Ok(output) => to_call_result(output),
            // A tool failure is a result the agent can read and retry from, not a protocol error.
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
        };
        Ok(result.into())
    }
}

/// Build the server: dispose of files spilled to the sandbox beyond the retention TTL, then wire
/// every tool onto one handler.
pub fn build_server() -> GdriveServer {
    localfs::sweep_expired(None);
    GdriveServer::new(Arc::new(GoogleClient::new()))
}

pub async fn run() -> anyhow::Result<()> {
    let service = build_server().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_audit_record_carries_the_defaults_the_tool_actually_ran_with() {
        let delete_rows = tools::all_defs().into_iter().find(|d| d.name == "delete_rows").unwrap();
        let sent: Map<String, Value> =
            json!({"item": "abc", "tab": "Data", "start_row": 3}).as_object().cloned().unwrap();
        let as_run = GdriveServer::args_as_run(&sent, &delete_rows.schema);
        // Omitted, but it is what determines how much data went — the audit log must say so.
        assert_eq!(as_run["count"], json!(1));
        assert_eq!(as_run["start_row"], json!(3));
        assert_eq!(as_run["confirm"], json!(false));
    }

    #[test]
    fn an_unset_optional_reference_is_left_out_rather_than_logged_as_unparseable() {
        let upload = tools::all_defs().into_iter().find(|d| d.name == "upload_file").unwrap();
        let sent: Map<String, Value> =
            json!({"name": "x.txt", "content": "hi"}).as_object().cloned().unwrap();
        let as_run = GdriveServer::args_as_run(&sent, &upload.schema);
        assert!(!as_run.contains_key("parent"));
        assert!(!as_run.contains_key("replace_id"));
    }

    /// The two sets as the Python server spells them (`server.py:14-24`), written out here
    /// rather than read off the consts: a name quietly added to or dropped from `READ_ONLY` /
    /// `DESTRUCTIVE` changes what every client is told about a tool, so set membership is itself
    /// the contract, not an implementation detail.
    const PYTHON_READ_ONLY: &[&str] = &[
        "resolve_link",
        "search_files",
        "list_folder",
        "get_metadata",
        "read_sheet",
        "read_document",
        "extract_images",
        "read_comments",
        "read_file_as_text",
    ];
    const PYTHON_DESTRUCTIVE: &[&str] = &[
        "write_sheet",
        "clear_range",
        "delete_rows",
        "move_file",
        "rename_file",
        "upload_file",
        "delete_text",
        "replace_text",
    ];

    fn set(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn the_annotated_sets_are_the_python_sets_verbatim() {
        for (rust, python) in [(READ_ONLY, PYTHON_READ_ONLY), (DESTRUCTIVE, PYTHON_DESTRUCTIVE)] {
            assert_eq!(set(rust), set(python));
            // A duplicated name would survive the set comparison above.
            assert_eq!(rust.len(), python.len());
        }
        // `annotations` tests READ_ONLY first, so an overlap would advertise a destructive tool
        // as read-only — and a name that matches no registered tool annotates nothing at all.
        let defs = tools::all_defs();
        let registered = set(&defs.iter().map(|d| d.name).collect::<Vec<_>>());
        for name in READ_ONLY.iter().chain(DESTRUCTIVE) {
            assert!(registered.contains(*name), "{name} is annotated but not registered");
        }
        assert!(
            READ_ONLY.iter().all(|n| !DESTRUCTIVE.contains(n)),
            "a tool is both read-only and destructive"
        );
    }

    #[test]
    fn every_read_only_tool_is_annotated_as_such() {
        for name in PYTHON_READ_ONLY {
            let a = annotations(name);
            assert_eq!(a.read_only_hint, Some(true), "{name}");
            assert_eq!(a.open_world_hint, Some(true), "{name}");
            // Python left destructiveHint unset on the read-only branch; a read cannot destroy
            // anything, so saying `false` here would be a claim the Python server never made.
            assert_eq!(a.destructive_hint, None, "{name}");
        }
    }

    #[test]
    fn every_destructive_tool_carries_the_destructive_hint() {
        for name in PYTHON_DESTRUCTIVE {
            let a = annotations(name);
            assert_eq!(a.destructive_hint, Some(true), "{name}");
            assert_eq!(a.read_only_hint, Some(false), "{name}");
            assert_eq!(a.open_world_hint, Some(true), "{name}");
        }
    }

    #[test]
    fn confirm_is_offered_by_exactly_the_destructive_tools() {
        // The confirm gate and the destructive annotation are two halves of one promise: the
        // README marks destructive tools ᶜ ("returns an impact preview unless called with
        // confirm=true"), so a tool that gates on confirm but is not annotated destructive (or
        // the reverse) misinforms the agent about which calls need approval.
        let defs = tools::all_defs();
        let gated: Vec<&str> =
            defs.iter().filter(|d| d.params().contains(&"confirm")).map(|d| d.name).collect();
        assert_eq!(set(&gated), set(DESTRUCTIVE));
    }

    #[test]
    fn additive_tools_are_writes_but_not_destructive() {
        for name in ["append_text", "create_document", "insert_table", "format_cells", "read_full_sheet"] {
            let a = annotations(name);
            assert_eq!(a.read_only_hint, Some(false), "{name}");
            assert_eq!(a.destructive_hint, Some(false), "{name}");
        }
    }

    #[test]
    fn every_registered_tool_becomes_an_rmcp_tool_with_a_strict_schema() {
        for def in tools::all_defs() {
            let tool = to_rmcp_tool(&def);
            assert_eq!(tool.name, def.name);
            assert_eq!(tool.input_schema.get("additionalProperties"), Some(&Value::Bool(false)));
            // The Python gate injected a `ctx` parameter that FastMCP hid; nothing equivalent
            // may leak into the agent-facing schema here either.
            let props = tool.input_schema.get("properties").and_then(Value::as_object);
            assert!(props.is_none_or(|p| !p.contains_key("ctx")), "{} exposes ctx", def.name);
        }
    }
}
