//! Live, credentialed checks L1–L10 from `LOCATOR_WRITES_VERIFICATION.md` §7, ported from
//! `tests/test_live_locator.py`.
//!
//! These are the claims a fake `GoogleApi` cannot falsify: that Docs indexes really are UTF-16,
//! that `requiredRevisionId` really rejects a stale write, that the final-newline clamp is
//! load-bearing, and that locator anchors really do satisfy `insertTable`'s paragraph-boundary
//! rule. A reimplementation is exactly where those can go wrong in a new way, so the Rust port
//! needs its own run of them rather than inheriting the Python's.
//!
//! Creates ONE scratch Doc in the authenticated account's My Drive and trashes it at the end.
//! Content is synthetic — never point this at a real document.
//!
//!     GDRIVE_MCP_LIVE=1 cargo test --test live_locator -- --ignored --nocapture
//!
//! `#[ignore]` keeps a plain `cargo test` from ever reaching Google; the env var is a second
//! latch, so running the ignored test without it skips instead of writing to someone's Drive.
//!
//! The ten checks share one document and **mutate it in order** — L5 needs the table L9 inserted,
//! L6 needs the text L8 wrote. So this is one sequential test, in the Python file's order
//! (L1, L2, L3, L4, L10, L9, L5, L7, L8, L6), not ten independent ones.

use std::future::Future;
use std::sync::Arc;

use serde_json::{json, Value};

use gdrive_mcp::args::Args;
use gdrive_mcp::clients::{GoogleApi, GoogleClient};
use gdrive_mcp::error::{Result, ToolError};
use gdrive_mcp::locate;
use gdrive_mcp::tools::{self, ToolOutput};

/// Distinctive synthetic tokens so every assertion targets an unambiguous span.
const EMOJI_LINE: &str = "pre a\u{1F600}b NEEDLE post";
const TRIPLE_LINE: &str = "ZAP one ZAP two ZAP three";
const BOLD_LINE: &str = "hello **world** tail";

fn live() -> bool {
    std::env::var("GDRIVE_MCP_LIVE").is_ok_and(|v| !v.is_empty())
}

/// Run a tool the way the server does: look its schema up, build strict `Args`, dispatch.
async fn call(api: &dyn GoogleApi, tool: &str, arguments: Value) -> Result<Value> {
    let defs = tools::all_defs();
    let def = defs.iter().find(|d| d.name == tool).expect("tool is registered");
    let map = arguments.as_object().cloned().unwrap_or_default();
    let args = Args::new(tool, Some(map), &def.params())?;
    match tools::dispatch(tool, api, &args).await? {
        ToolOutput::Json(v) => Ok(v),
        ToolOutput::Images(_) => panic!("{tool} returned images"),
    }
}

async fn ok(api: &dyn GoogleApi, tool: &str, arguments: Value) -> Value {
    call(api, tool, arguments).await.unwrap_or_else(|e| panic!("{tool} failed: {e}"))
}

/// The target tab's `body.content`.
///
/// Empirical finding recorded in §7: a Doc created through the API comes back under `tabs` with
/// **no top-level `body` key at all**, which is why the tools resolve tabs first. This helper
/// mirrors that, so the live run exercises the tabbed path exactly as production does.
fn body_of(doc: &Value) -> Vec<Value> {
    doc.pointer("/tabs/0/documentTab/body/content")
        .or_else(|| doc.pointer("/body/content"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

async fn body(api: &dyn GoogleApi, did: &str) -> Vec<Value> {
    body_of(&api.docs_get(did).await.expect("documents.get"))
}

async fn text(api: &dyn GoogleApi, did: &str) -> String {
    let out = ok(api, "read_document", json!({"item": did, "output_format": "text", "max_chars": 0})).await;
    out["content"].as_str().unwrap_or_default().to_string()
}

fn tables(body: &[Value]) -> Vec<&Value> {
    body.iter().filter(|el| el.get("table").is_some()).collect()
}

/// Every `textRun` in document order, for asserting on the styling a write left behind.
fn text_runs(body: &[Value]) -> Vec<&Value> {
    body.iter()
        .filter_map(|el| el.pointer("/paragraph/elements")?.as_array())
        .flatten()
        .filter_map(|pe| pe.get("textRun"))
        .collect()
}

#[tokio::test]
#[ignore = "live: writes to a real Drive; needs GDRIVE_MCP_LIVE=1 and cached credentials"]
async fn locator_writes_hold_against_the_real_docs_api() {
    if !live() {
        eprintln!("skipped: set GDRIVE_MCP_LIVE=1 to run the live locator checks");
        return;
    }
    let api: Arc<dyn GoogleApi> = Arc::new(GoogleClient::new());

    // ---- scratch doc -------------------------------------------------------------------
    let made =
        ok(api.as_ref(), "create_document", json!({"title": "gdrive-mcp locator live check (scratch)"}))
            .await;
    let did = made["id"].as_str().expect("created document id").to_string();
    eprintln!("scratch doc: {did}");

    let seeded = [
        "# Alpha",
        "alpha body text",
        "## Beta",
        "beta body text",
        "# Gamma",
        EMOJI_LINE,
        TRIPLE_LINE,
        BOLD_LINE,
        "anchor paragraph",
    ]
    .join("\n");
    ok(api.as_ref(), "append_text", json!({"item": &did, "text": seeded, "markdown": true})).await;

    // Trash the scratch doc even if a check panics, then re-raise the panic.
    let outcome = std::panic::AssertUnwindSafe(checks(api.as_ref(), &did)).catch_unwind_async().await;

    let trashed = api.drive_files_update(&did, &json!({"trashed": true}), &[], "id").await;
    if let Err(e) = trashed {
        eprintln!("WARNING: could not trash scratch doc {did}: {e}");
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// L1–L10 in the order the Python file ran them; each step mutates the doc the next relies on.
async fn checks(api: &dyn GoogleApi, did: &str) {
    // ---- L1: the revision is plumbed through ------------------------------------------
    let doc = api.docs_get(did).await.expect("documents.get");
    assert!(
        doc.get("revisionId").and_then(Value::as_str).is_some_and(|r| !r.is_empty()),
        "L1: documents.get returned no revisionId"
    );
    let read = ok(api, "read_document", json!({"item": did})).await;
    assert!(
        read["revision_id"].as_str().is_some_and(|r| !r.is_empty()),
        "L1: read_document dropped revision_id"
    );
    let stale = doc["revisionId"].as_str().unwrap().to_string();

    // ---- L2: a stale revision is rejected, and changes nothing -------------------------
    // Out-of-band edit, bumping the revision behind the resolver's back.
    api.docs_batch_update(
        did,
        &json!({"requests": [{"insertText": {"location": {"index": 1}, "text": "X"}}]}),
    )
    .await
    .expect("L2: out-of-band edit");
    let before = text(api, did).await;
    let rejected = api
        .docs_batch_update(
            did,
            &json!({
                "requests": [{"insertText": {"location": {"index": 1}, "text": "SHOULD-NOT-LAND"}}],
                "writeControl": {"requiredRevisionId": stale},
            }),
        )
        .await;
    let status = rejected.err().and_then(|e| e.status());
    assert!(matches!(status, Some(400) | Some(409)), "L2: stale write was not rejected (status {status:?})");
    let after = text(api, did).await;
    assert!(!after.contains("SHOULD-NOT-LAND"), "L2: the rejected batch still landed");
    assert_eq!(after, before, "L2: the rejected batch changed the document");
    ok(api, "delete_text", json!({"item": did, "match": "X", "confirm": true})).await;

    // ---- L3: the UTF-16 claim ----------------------------------------------------------
    assert!(text(api, did).await.contains(EMOJI_LINE), "L3: seed line missing");
    let out = ok(api, "delete_text", json!({"item": did, "match": "NEEDLE", "confirm": true})).await;
    assert_eq!(out["chars"], json!(6), "L3: wrong span length");
    let after = text(api, did).await;
    // Exactly the needle went; the astral char and both neighbours survive intact.
    assert!(after.contains("pre a\u{1F600}b  post"), "L3: neighbours were disturbed:\n{after}");
    assert!(!after.contains("NEEDLE"), "L3: needle survived");

    // ---- L4: descending multi-range delete ---------------------------------------------
    let out =
        ok(api, "delete_text", json!({"item": did, "match": "ZAP ", "occurrence": 0, "confirm": true})).await;
    assert_eq!(out["occurrences"], json!(3), "L4: wrong occurrence count");
    let after = text(api, did).await;
    assert!(!after.contains("ZAP"), "L4: an occurrence survived");
    // Surrounding text intact — a wrong order would have eaten into the neighbours.
    assert!(after.contains("one two three"), "L4: double-shift damage:\n{after}");

    // ---- L10: a match spanning two text runs -------------------------------------------
    assert!(text(api, did).await.contains("hello world tail"), "L10: seed line missing");
    ok(api, "delete_text", json!({"item": did, "match": "lo wor", "confirm": true})).await;
    assert!(text(api, did).await.contains("helld tail"), "L10: cross-run delete was wrong");

    // ---- L9: a locator anchor satisfies insertTable's boundary rule ---------------------
    let out = ok(
        api,
        "insert_table",
        json!({"item": did, "rows": [["CELLONE", "CELLTWO"]], "after": "anchor paragraph"}),
    )
    .await;
    assert_eq!(out["cells_filled"], json!(2), "L9: cells were not filled");
    let md = ok(api, "read_document", json!({"item": did, "max_chars": 0})).await;
    assert!(md["content"].as_str().unwrap().contains("CELLONE"), "L9: table content missing");

    // ---- L5: deleting inside a table cell keeps the table -------------------------------
    let before_count = tables(&body(api, did).await).len();
    ok(api, "delete_text", json!({"item": did, "match": "ONE", "confirm": true})).await;
    let b = body(api, did).await;
    let t = tables(&b);
    assert_eq!(t.len(), before_count, "L5: the table count changed");
    let rows = t.last().unwrap().pointer("/table/tableRows").and_then(Value::as_array).unwrap();
    assert_eq!(rows.len(), 1, "L5: row count changed");
    assert_eq!(
        rows[0].pointer("/tableCells").and_then(Value::as_array).unwrap().len(),
        2,
        "L5: cell count changed"
    );
    let md = ok(api, "read_document", json!({"item": did, "max_chars": 0})).await;
    let md = md["content"].as_str().unwrap();
    assert!(md.contains("CELL") && !md.contains("CELLONE"), "L5: cell text is wrong");

    // ---- L7: the final-newline clamp is load-bearing ------------------------------------
    let b = body(api, did).await;
    let unclamped = b.last().and_then(|el| el.get("endIndex")).and_then(Value::as_i64).expect("endIndex");
    let rejected = api
        .docs_batch_update(
            did,
            &json!({"requests": [{"deleteContentRange": {
                "range": {"startIndex": unclamped - 2, "endIndex": unclamped}}}]}),
        )
        .await;
    assert_eq!(rejected.err().and_then(|e| e.status()), Some(400), "L7: the unclamped range was accepted");
    // The clamp sits exactly one below the rejected bound, so body_end is the last legal index.
    // That the clamped side is *accepted* follows from every other delete here, which all resolve
    // through body_end and succeed.
    assert_eq!(locate::body_end(&b), unclamped - 1, "L7: body_end is not the last legal index");

    // ---- L8: replace is atomic and its styling lands on the new text --------------------
    ok(
        api,
        "replace_text",
        json!({"item": did, "replacement": "**BOLDED**", "match": "beta body text",
               "markdown": true, "confirm": true}),
    )
    .await;
    let b = body(api, did).await;
    let hit = text_runs(&b)
        .into_iter()
        .find(|r| r.get("content").and_then(Value::as_str).is_some_and(|c| c.contains("BOLDED")))
        .expect("L8: the replacement text is not in the document");
    assert_eq!(
        hit.pointer("/textStyle/bold"),
        Some(&json!(true)),
        "L8: the style did not land on the new text"
    );
    assert!(!text(api, did).await.contains("beta body text"), "L8: the old text survived");

    // ---- L6: a section delete leaves no orphan paragraph --------------------------------
    let before = text(api, did).await;
    assert!(before.contains("Beta"), "L6: the section is missing");
    ok(api, "delete_text", json!({"item": did, "section": "Beta", "confirm": true})).await;
    let after = text(api, did).await;
    assert!(!after.contains("Beta"), "L6: the heading survived");
    assert!(!after.contains("BOLDED"), "L6: the section body survived");
    assert!(!after.trim().contains("\n\n"), "L6: an empty paragraph was left behind:\n{after:?}");
    assert!(after.contains("Gamma"), "L6: the following section was disturbed");

    eprintln!("L1-L10: all green");
}

/// `catch_unwind` for a future, so the scratch doc is always trashed.
///
/// `std::panic::catch_unwind` cannot wrap an `.await`; polling the future inside the closure is
/// what lets a panicking check still run teardown before the panic resumes.
trait CatchUnwindAsync: Future {
    async fn catch_unwind_async(self) -> std::result::Result<Self::Output, Box<dyn std::any::Any + Send>>;
}

impl<F: Future + std::panic::UnwindSafe> CatchUnwindAsync for F {
    async fn catch_unwind_async(self) -> std::result::Result<Self::Output, Box<dyn std::any::Any + Send>> {
        use std::pin::pin;
        use std::task::Poll;
        let mut future = pin!(self);
        std::future::poll_fn(move |cx| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(cx))) {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
                Err(panic) => Poll::Ready(Err(panic)),
            }
        })
        .await
    }
}

/// Compile-time proof the harness is wired to the real tools, runnable without credentials.
#[test]
fn the_live_harness_targets_tools_that_exist() {
    let names: Vec<&str> = tools::all_defs().into_iter().map(|d| d.name).collect();
    for tool in
        ["create_document", "append_text", "read_document", "delete_text", "replace_text", "insert_table"]
    {
        assert!(names.contains(&tool), "{tool} is not registered");
    }
    // A ToolError carries the HTTP status L2 and L7 assert on.
    assert_eq!(ToolError::Api { status: 400, reason: "x".into() }.status(), Some(400));
}
