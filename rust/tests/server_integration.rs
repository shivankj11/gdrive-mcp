//! Integration tests over the ACTUAL registered path — the tool registry driven through an
//! in-memory MCP client. Covers what the unit tests (raw functions, fake API) miss: annotations,
//! the strict input schemas, and the verification gate firing on the shipping path.

use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, ContentBlock, ElicitRequestParams,
    ElicitResult, ElicitationAction, Implementation, Tool,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{ErrorData as McpError, RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

use gdrive_mcp::clients::GoogleApi;
use gdrive_mcp::error::{Result as ToolResult, ToolError};
use gdrive_mcp::server::GdriveServer;

// ---- a Google API that is never reachable ---------------------------------------------------
// These tests are about the registered path, not about Drive. Every call fails, so a tool that
// gets past argument checking and the gate reports a Google error — which is exactly how the
// "approval lets the call proceed" assertion tells the two apart.

struct OfflineApi;

macro_rules! offline {
    () => {
        Err(ToolError::msg("offline: no Google API in tests"))
    };
}

#[async_trait]
impl GoogleApi for OfflineApi {
    async fn drive_files_get(&self, _: &str, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn drive_files_list(
        &self,
        _: &str,
        _: i64,
        _: Option<&str>,
        _: &str,
        _: &str,
    ) -> ToolResult<Value> {
        offline!()
    }
    async fn drive_get_media(&self, _: &str) -> ToolResult<Vec<u8>> {
        offline!()
    }
    async fn drive_export(&self, _: &str, _: &str) -> ToolResult<Vec<u8>> {
        offline!()
    }
    async fn drive_files_update(
        &self,
        _: &str,
        _: &Value,
        _: &[(String, String)],
        _: &str,
    ) -> ToolResult<Value> {
        offline!()
    }
    async fn drive_files_create_media(&self, _: &Value, _: &str, _: Vec<u8>, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn drive_files_update_media(
        &self,
        _: &str,
        _: &Value,
        _: &str,
        _: Vec<u8>,
        _: &str,
    ) -> ToolResult<Value> {
        offline!()
    }
    async fn drive_comments_list(&self, _: &str, _: i64, _: Option<&str>, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn drive_comments_create(&self, _: &str, _: &str, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn drive_about(&self, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn docs_get(&self, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn docs_create(&self, _: &str, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn docs_batch_update(&self, _: &str, _: &Value) -> ToolResult<Value> {
        offline!()
    }
    async fn sheets_get(&self, _: &str, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn sheets_create(&self, _: &Value, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn sheets_values_get(&self, _: &str, _: &str, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn sheets_values_update(&self, _: &str, _: &str, _: &str, _: &Value) -> ToolResult<Value> {
        offline!()
    }
    async fn sheets_values_append(&self, _: &str, _: &str, _: &str, _: &Value) -> ToolResult<Value> {
        offline!()
    }
    async fn sheets_values_clear(&self, _: &str, _: &str) -> ToolResult<Value> {
        offline!()
    }
    async fn sheets_batch_update(&self, _: &str, _: &Value) -> ToolResult<Value> {
        offline!()
    }
    async fn fetch_image(&self, _: &str) -> ToolResult<Option<(Vec<u8>, String)>> {
        Ok(None)
    }
    async fn authed_user_email(&self) -> Option<String> {
        None
    }
}

// ---- a client that answers elicitation however the test wants -------------------------------

#[derive(Clone, Copy)]
enum Elicit {
    /// No elicitation capability at all — the server cannot prompt.
    Unsupported,
    Decline,
    Accept {
        approved: bool,
    },
}

struct TestClient(Elicit);

impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientInfo {
        let capabilities = match self.0 {
            Elicit::Unsupported => ClientCapabilities::default(),
            _ => ClientCapabilities::builder().enable_elicitation().build(),
        };
        ClientInfo::new(capabilities, Implementation::new("test-client", "0"))
    }

    async fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> std::result::Result<ElicitResult, McpError> {
        Ok(match self.0 {
            Elicit::Accept { approved } => {
                ElicitResult::new(ElicitationAction::Accept).with_content(json!({ "approved": approved }))
            }
            _ => ElicitResult::new(ElicitationAction::Decline),
        })
    }
}

async fn connect(elicit: Elicit) -> RunningService<RoleClient, TestClient> {
    let (server_side, client_side) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(server_side);
    let (cr, cw) = tokio::io::split(client_side);

    tokio::spawn(async move {
        if let Ok(running) = GdriveServer::new(Arc::new(OfflineApi)).serve((sr, sw)).await {
            let _ = running.waiting().await;
        }
    });
    TestClient(elicit).serve((cr, cw)).await.expect("client handshake")
}

async fn list_tools() -> Vec<Tool> {
    let client = connect(Elicit::Unsupported).await;
    let tools = client.list_tools(Default::default()).await.expect("tools/list").tools;
    let _ = client.cancel().await;
    tools
}

async fn call(tool: &str, args: Value, elicit: Elicit) -> CallToolResult {
    let client = connect(elicit).await;
    let arguments: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
        .await
        .expect("tools/call");
    let _ = client.cancel().await;
    result
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn find<'a>(tools: &'a [Tool], name: &str) -> &'a Tool {
    tools.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("{name} is not registered"))
}

fn properties(tool: &Tool) -> Map<String, Value> {
    tool.input_schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default()
}

// `GDRIVE_MCP_REQUIRE_VERIFICATION` is process-global, so the gate tests take this lock and put
// the variable back when they are done.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct AlwaysGate(#[allow(dead_code)] MutexGuard<'static, ()>, Option<String>);

impl AlwaysGate {
    fn on() -> AlwaysGate {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("GDRIVE_MCP_REQUIRE_VERIFICATION").ok();
        unsafe { std::env::set_var("GDRIVE_MCP_REQUIRE_VERIFICATION", "always") };
        AlwaysGate(guard, prev)
    }
}

impl Drop for AlwaysGate {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => unsafe { std::env::set_var("GDRIVE_MCP_REQUIRE_VERIFICATION", v) },
            None => unsafe { std::env::remove_var("GDRIVE_MCP_REQUIRE_VERIFICATION") },
        }
    }
}

const AN_ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

#[tokio::test]
async fn every_tool_is_registered_with_the_right_annotations() {
    let tools = list_tools().await;
    assert_eq!(tools.len(), 29);
    assert_eq!(find(&tools, "read_sheet").annotations.as_ref().unwrap().read_only_hint, Some(true));
    assert_eq!(find(&tools, "delete_rows").annotations.as_ref().unwrap().destructive_hint, Some(true));
    for additive in ["append_text", "create_document", "insert_table"] {
        assert_eq!(
            find(&tools, additive).annotations.as_ref().unwrap().destructive_hint,
            Some(false),
            "{additive} should be additive, not destructive"
        );
    }
    assert_eq!(find(&tools, "format_cells").annotations.as_ref().unwrap().read_only_hint, Some(false));
}

#[tokio::test]
async fn the_locator_edit_tools_are_registered_as_destructive() {
    let tools = list_tools().await;
    for name in ["delete_text", "replace_text"] {
        let a = find(&tools, name).annotations.as_ref().unwrap();
        assert_eq!(a.destructive_hint, Some(true), "{name}");
        assert_ne!(a.read_only_hint, Some(true), "{name}");
    }
}

#[tokio::test]
async fn the_insert_tools_expose_their_locator_parameters() {
    let tools = list_tools().await;
    for name in ["insert_text", "insert_table"] {
        let props = properties(find(&tools, name));
        for param in ["after", "before", "index"] {
            assert!(props.contains_key(param), "{name} is missing {param}");
        }
    }
}

#[tokio::test]
async fn an_unknown_argument_is_rejected_rather_than_dropped() {
    for (tool, args) in [
        ("delete_text", json!({"item": AN_ID, "match": "x", "bogus": 1})),
        ("resolve_link", json!({"item": AN_ID, "bogus": 1})),
    ] {
        let result = call(tool, args, Elicit::Unsupported).await;
        assert_eq!(result.is_error, Some(true), "{tool} accepted an unknown argument");
        assert!(text(&result).contains("bogus"), "{tool}: {}", text(&result));
    }
}

#[tokio::test]
async fn the_gate_fails_closed_when_the_client_cannot_prompt() {
    let _gate = AlwaysGate::on();
    let result = call("resolve_link", json!({"item": AN_ID}), Elicit::Unsupported).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("verification"), "{}", text(&result));
}

#[tokio::test]
async fn declining_the_prompt_blocks_the_call() {
    let _gate = AlwaysGate::on();
    let result = call("resolve_link", json!({"item": AN_ID}), Elicit::Decline).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("not approved"), "{}", text(&result));
}

#[tokio::test]
async fn accepting_without_approving_still_blocks_the_call() {
    let _gate = AlwaysGate::on();
    let result = call("resolve_link", json!({"item": AN_ID}), Elicit::Accept { approved: false }).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("not approved"), "{}", text(&result));
}

#[tokio::test]
async fn approval_lets_the_call_proceed_to_the_api() {
    // After approval the gate passes and the tool actually runs — so the only error left is the
    // offline Google client, NOT a verification block. That is what proves approve -> proceed.
    let _gate = AlwaysGate::on();
    let result = call("resolve_link", json!({"item": AN_ID}), Elicit::Accept { approved: true }).await;
    let body = text(&result);
    assert!(!body.contains("verification"), "{body}");
    assert!(!body.contains("not approved"), "{body}");
    assert!(body.contains("offline"), "{body}");
}
