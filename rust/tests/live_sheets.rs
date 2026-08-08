//! Live, credentialed checks S1–S11 for the Sheets tools.
//!
//! Sheets has never had a verification plan in either language, and has never had a live run at
//! all. Everything the offline suites pin is a *request shape* asserted against a fake: that
//! `write_sheet` sends `valueInputOption=RAW`, that `append` carries `insertDataOption=INSERT_ROWS`,
//! that `delete_rows` turns a 1-based `start_row` into a 0-based half-open `deleteDimension`. None
//! of that says what Google then *does* with the request. These eleven checks are exactly the claims a
//! fake `GoogleApi` cannot falsify:
//!
//!   S1  a RAW-written `=1+1` is stored as text, never evaluated (the injection guarantee)
//!   S2  a USER_ENTERED `=1+1` is a live formula, so the two modes really differ upstream
//!   S3  `append_rows` lands after the last row and leaves the seeded rows untouched
//!   S4  the overwrite gate is real: no `confirm` -> nothing reaches the spreadsheet
//!   S5  `clear_range` clears its range and nothing around it
//!   S6  `delete_rows` deletes the rows the caller named (the 1-based -> 0-based off-by-one)
//!   S7  `read_full_sheet` spills a CSV matching the sheet, mode 0600, inside the sandbox
//!   S8  `format_cells` styles its range and leaves the values alone
//!   S9  A1 quoting survives a tab literally named `John's Data`
//!   S10 FORMULA / UNFORMATTED / FORMATTED really are three different renderings
//!   S11 a formula that *evaluates* to "" still trips the overwrite gate — it is data, and the
//!       gate used to miss it because `UNFORMATTED_VALUE` renders it as nothing
//!
//!     GDRIVE_MCP_LIVE=1 cargo test --test live_sheets -- --ignored --nocapture
//!
//! `#[ignore]` keeps a plain `cargo test` from ever reaching Google; the env var is a second latch,
//! so running the ignored test without it skips instead of writing to someone's Drive.
//!
//! Creates ONE scratch spreadsheet in the authenticated account's My Drive and trashes it at the
//! end, panic or not. Its id is printed first, so a failed teardown is recoverable by hand. All
//! content is synthetic `S<n>-` tokens — never point this at a real spreadsheet, and never put sensitive data
//! in it.
//!
//! The checks share that spreadsheet and mutate it, so this is one sequential test in S1..S11
//! order, and every assertion is labelled with its check id. Each check owns its own tab — S1, S2
//! and S10 share `Data` but at three disjoint columns, and S11 reuses S4's `Gate` tab two rows
//! below what S4 touched — so no check reads state another one wrote.
//! That independence is *enforced*: each check runs under its own `catch_unwind`, so a failing S1
//! costs one line of output rather than the other ten claims. A credentialed round trip is
//! expensive; finding one bug per run is not good enough. The run fails at the end, naming every
//! check that broke.
//!
//! Three things are worth stating plainly about what is and is not observed:
//!
//!   * `GDRIVE_MCP_FILES_DIR` is redirected at a temp dir for the whole run (restored on the way
//!     out) so S7/S9's spills never land in the operator's real sandbox. The redirect is
//!     *asserted* to have taken effect before the first Drive call, not just afterwards — a
//!     containment check that only fires after the CSV exists has already written the file.
//!   * S8 does read the styling back, via `spreadsheets.get` with a `data.rowData` field mask —
//!     `GoogleApi::sheets_get` takes the mask as a parameter, so no trait change is needed. If the
//!     API ever stops returning grid data for that mask the harness says so loudly and falls back
//!     to the guaranteed part of the claim (the values are unchanged and the batch was accepted).
//!     That fallback is carried into the closing summary line, so "all green" never silently means
//!     "all green except the half of S8 that was the reason to write it".
//!   * The run makes ~62 requests, ~46 of them Sheets reads, back to back with no pacing. Sheets
//!     allows 60 read requests per minute per user, so a *second* run started inside the same
//!     minute can draw a 429 — and `GoogleClient` retries only on 401, not on 429. A failure whose
//!     message carries status 429 is a quota artefact, not a falsified claim: wait a minute and
//!     re-run.

use std::cell::Cell;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use gdrive_mcp::a1::{build_range, quote_tab};
use gdrive_mcp::args::Args;
use gdrive_mcp::clients::{GoogleApi, GoogleClient};
use gdrive_mcp::error::Result;
use gdrive_mcp::tools::{self, ToolOutput};

/// One tab per check, so a broken claim is attributable to the check that broke it.
const TABS: [&str; 7] = ["Data", "Append", "Gate", "Clear", "Delete", "Spill", "Format"];

/// S9's hostile tab name: a bare `'` here terminates a quoted A1 name early unless doubled.
const HOSTILE_TAB: &str = "John's Data";

/// Enough of a field mask to read a cell's text styling back out of `spreadsheets.get`.
/// `includeGridData` is ignored when a mask is set, so naming `rowData` is what returns the grid.
const FORMAT_FIELDS: &str =
    "sheets(properties(title),data(rowData(values(userEnteredFormat(textFormat(bold,italic,underline))))))";

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

/// The raw grid at a verbatim A1 range: `header_row=false`, so no record shaping sits between the
/// assertion and what Sheets returned, and `a1_range` is passed through as spelled.
async fn grid_at(api: &dyn GoogleApi, sid: &str, range: &str, values: &str) -> Vec<Vec<Value>> {
    let out =
        ok(api, "read_sheet", json!({"item": sid, "a1_range": range, "header_row": false, "values": values}))
            .await;
    out["tabs"][0]["rows"]
        .as_array()
        .map(|rows| rows.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect())
        .unwrap_or_default()
}

/// The first cell of a range, or null when the range came back empty.
async fn cell_at(api: &dyn GoogleApi, sid: &str, range: &str, values: &str) -> Value {
    grid_at(api, sid, range, values).await.into_iter().flatten().next().unwrap_or(Value::Null)
}

/// An empty cell: Sheets reports one inside a bounded range as `""` and omits it past the last
/// populated column, so both spellings mean "nothing there".
fn blank(cell: &Value) -> bool {
    cell.is_null() || cell.as_str() == Some("")
}

/// Every non-empty string in a grid, in row-major order — for asserting on distinctive content
/// rather than on a row count that an off-by-one would keep intact.
fn tokens(grid: &[Vec<Value>]) -> Vec<String> {
    grid.iter().flatten().filter_map(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

fn rows_of(values: &Value) -> Vec<Vec<Value>> {
    values
        .as_array()
        .map(|rows| rows.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect())
        .unwrap_or_default()
}

/// The `textFormat` of each cell in one row of one tab, as `spreadsheets.get` reports it.
/// Empty when the mask returned no grid data for that row (which S8 treats as "not observable").
fn text_formats(meta: &Value, tab: &str, row0: usize) -> Vec<Value> {
    let Some(sheets) = meta.get("sheets").and_then(Value::as_array) else { return Vec::new() };
    let Some(sheet) =
        sheets.iter().find(|s| s.pointer("/properties/title").and_then(Value::as_str) == Some(tab))
    else {
        return Vec::new();
    };
    let Some(cells) = sheet.pointer(&format!("/data/0/rowData/{row0}/values")).and_then(Value::as_array)
    else {
        return Vec::new();
    };
    cells.iter().map(|c| c.pointer("/userEnteredFormat/textFormat").cloned().unwrap_or(Value::Null)).collect()
}

/// Redirect the local-file sandbox at a temp dir for the run, and put it back afterwards.
///
/// S7 writes a real CSV. Without this it would land in the operator's configured sandbox, next to
/// spills that may hold sensitive data; with it the spill is disposable and its containment is checkable
/// against a root this test knows.
struct SpillDir {
    _dir: tempfile::TempDir,
    root: PathBuf,
    previous: Option<String>,
}

impl SpillDir {
    fn open() -> SpillDir {
        let dir = tempfile::tempdir().expect("temp dir for the spill sandbox");
        // Canonicalised because localfs expands symlinks and macOS's /tmp is one. `files` must not
        // exist yet, so files_root() creates it 0700 and marks it as the server's own.
        let root = dir.path().canonicalize().expect("canonical temp dir").join("files");
        let previous = std::env::var("GDRIVE_MCP_FILES_DIR").ok();
        unsafe { std::env::set_var("GDRIVE_MCP_FILES_DIR", &root) };
        let spill = SpillDir { _dir: dir, root, previous };
        // Fail here, before a single Drive call, if the redirect did not take: S7's `starts_with`
        // only notices an escape *after* the CSV has been written into the operator's real
        // sandbox, which is the one directory this harness must never put a file in.
        let resolved = gdrive_mcp::localfs::files_root().expect("the temp spill sandbox is usable");
        assert_eq!(
            resolved,
            spill.root,
            "GDRIVE_MCP_FILES_DIR did not take effect: spills would land in {}",
            resolved.display()
        );
        spill
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => unsafe { std::env::set_var("GDRIVE_MCP_FILES_DIR", v) },
            None => unsafe { std::env::remove_var("GDRIVE_MCP_FILES_DIR") },
        }
    }
}

#[tokio::test]
#[ignore = "live: writes to a real Drive; needs GDRIVE_MCP_LIVE=1 and cached credentials"]
async fn sheets_writes_hold_against_the_real_sheets_api() {
    if !live() {
        eprintln!("skipped: set GDRIVE_MCP_LIVE=1 to run the live sheets checks");
        return;
    }
    // Opened first, and deliberately before the HTTP client: it mutates a process-global env var,
    // which must happen before anything that could spawn a thread reads one. It also self-checks
    // the redirect, so a broken sandbox costs zero Drive calls. Held across the whole run; its Drop
    // restores GDRIVE_MCP_FILES_DIR even while unwinding.
    let spill = SpillDir::open();
    let api: Arc<dyn GoogleApi> = Arc::new(GoogleClient::new());

    // ---- scratch spreadsheet -------------------------------------------------------------
    let made = ok(
        api.as_ref(),
        "create_spreadsheet",
        json!({"title": "gdrive-mcp sheets live check (scratch)", "tabs": TABS}),
    )
    .await;
    let sid = made["id"].as_str().expect("created spreadsheet id").to_string();
    // Printed before anything can fail, so a failed teardown leaves the operator an id to trash.
    eprintln!("scratch spreadsheet: {sid}");

    // Trash it even if a check panics, then re-raise the panic.
    let outcome = AssertUnwindSafe(checks(api.as_ref(), &sid, &spill.root)).catch_unwind_async().await;

    if let Err(e) = api.drive_files_update(&sid, &json!({"trashed": true}), &[], "id").await {
        eprintln!("WARNING: could not trash scratch spreadsheet {sid}: {e}");
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Run one check, catching its panic so the other nine still run against the same scratch file.
///
/// Sound only because no check reads state another one wrote (see the module docs): the worst a
/// half-finished check can do is leave its own tab in an odd state. The alternative — straight-line
/// `.await`s — turns every bug into a separate credentialed run.
async fn attempt(id: &'static str, check: impl Future<Output = ()>, failed: &mut Vec<&'static str>) {
    if AssertUnwindSafe(check).catch_unwind_async().await.is_err() {
        eprintln!("{id} FAILED (panic above); the remaining checks still run — it owns its own tab");
        failed.push(id);
    }
}

async fn checks(api: &dyn GoogleApi, sid: &str, spill_root: &Path) {
    let mut failed: Vec<&'static str> = Vec::new();
    // Set only if S8 actually read the styling back out of `spreadsheets.get`.
    let styling_observed = Cell::new(false);

    attempt("S1", s1_raw_input_is_not_a_formula(api, sid), &mut failed).await;
    attempt("S2", s2_user_entered_input_is_a_formula(api, sid), &mut failed).await;
    attempt("S3", s3_append_lands_after_the_last_row(api, sid), &mut failed).await;
    attempt("S4", s4_the_overwrite_gate_is_real(api, sid), &mut failed).await;
    attempt("S5", s5_clear_range_clears_exactly_its_range(api, sid), &mut failed).await;
    attempt("S6", s6_delete_rows_deletes_the_rows_the_caller_named(api, sid), &mut failed).await;
    attempt("S7", s7_read_full_sheet_spills_the_sheet_into_the_sandbox(api, sid, spill_root), &mut failed)
        .await;
    attempt(
        "S8",
        s8_format_cells_styles_its_range_and_leaves_values_alone(api, sid, &styling_observed),
        &mut failed,
    )
    .await;
    attempt("S9", s9_a1_quoting_survives_an_apostrophe_in_a_tab_name(api, sid, spill_root), &mut failed)
        .await;
    attempt("S10", s10_the_render_options_differ(api, sid), &mut failed).await;
    attempt("S11", s11_a_formula_rendering_empty_still_trips_the_gate(api, sid), &mut failed).await;

    // Printed before the verdict so it survives a failing run too: whether `format_cells`' effect
    // on formatting was observed is a fact about this run either way.
    if !styling_observed.get() {
        eprintln!(
            "S8 CAVEAT: the styling itself was NOT read back (see S8's warning) — format_cells' \
             effect on formatting remains unverified against the real API"
        );
    }
    assert!(failed.is_empty(), "live sheets checks FAILED: {}", failed.join(", "));
    if styling_observed.get() {
        eprintln!("S1-S11: all green, S8 styling included");
    } else {
        eprintln!("S1-S11: all green EXCEPT the unobserved half of S8 (see the caveat above)");
    }
}

/// S1 — the RAW default is the formula-injection guarantee, and only Google can confirm it.
///
/// Catches: a port that sends USER_ENTERED (or lets `value_input` be case-folded into it), which
/// would turn any agent-supplied `=IMPORTDATA(...)`/`=HYPERLINK(...)` string into a live formula
/// running in the operator's spreadsheet.
async fn s1_raw_input_is_not_a_formula(api: &dyn GoogleApi, sid: &str) {
    let out = ok(api, "write_sheet", json!({"item": sid, "tab": "Data", "rows": [["=1+1"]]})).await;
    assert_eq!(out["updated_cells"], json!(1), "S1: the write did not land");

    // Under both renderings the cell must still be *text*. Sheets may echo a stored literal that
    // looks like a formula with a leading `'` in FORMULA mode, so the claim asserted here is the
    // load-bearing one: a string spelling `=1+1`, never the number 2.
    for render in ["FORMULA", "UNFORMATTED"] {
        let cell = cell_at(api, sid, "Data!A1", render).await;
        // The JSON *type* is the claim. An evaluated `=1+1` comes back as the number 2, so `as_str`
        // failing here IS "the literal was evaluated" — and a separate `as_f64().is_none()` assert
        // below this line could never fire, since a `Value::String` has no f64 to return.
        let text = cell
            .as_str()
            .unwrap_or_else(|| panic!("S1: {render} returned {cell}, not text — RAW was evaluated"));
        assert_eq!(text.strip_prefix('\'').unwrap_or(text), "=1+1", "S1: {render} altered the literal");
    }
    eprintln!("S1 ok: RAW stored '=1+1' as text under FORMULA and UNFORMATTED");
}

/// S2 — USER_ENTERED really does make the same string a live formula.
///
/// Catches: a port that always sends RAW, which would silently reduce `value_input` to a no-op —
/// every "write me a formula" call would leave dead text behind. Together with S1 it proves the two
/// modes reach Google as two different requests.
async fn s2_user_entered_input_is_a_formula(api: &dyn GoogleApi, sid: &str) {
    let out = ok(
        api,
        "write_sheet",
        json!({"item": sid, "tab": "Data", "rows": [["=1+1"]], "start_cell": "C1",
               "value_input": "USER_ENTERED"}),
    )
    .await;
    assert_eq!(out["updated_cells"], json!(1), "S2: the write did not land");

    let formula = cell_at(api, sid, "Data!C1", "FORMULA").await;
    assert_eq!(formula, json!("=1+1"), "S2: FORMULA did not return the formula");
    let computed = cell_at(api, sid, "Data!C1", "UNFORMATTED").await;
    assert_eq!(computed.as_f64(), Some(2.0), "S2: UNFORMATTED returned {computed}, not the value 2");
    eprintln!("S2 ok: USER_ENTERED '=1+1' evaluates to 2 while FORMULA still reads '=1+1'");
}

/// S3 — appending is the one Sheets write with no confirm gate, so it must be non-destructive.
///
/// Catches: an append aimed at a bounded range (which would overwrite the seed) or one landing on
/// top of the last row. Note INSERT_ROWS and the API default OVERWRITE only diverge when the
/// landing rows already hold data — Sheets picks the row after the last populated one either way —
/// so what is observable here is the claim itself: the seed survives byte-for-byte and the new row
/// lands after it.
async fn s3_append_lands_after_the_last_row(api: &dyn GoogleApi, sid: &str) {
    let seed = json!([["S3-SEED-1", "S3-KEEP-1"], ["S3-SEED-2", "S3-KEEP-2"]]);
    ok(api, "write_sheet", json!({"item": sid, "tab": "Append", "rows": seed})).await;

    let out =
        ok(api, "append_rows", json!({"item": sid, "tab": "Append", "rows": [["S3-APPENDED", "S3-NEW"]]}))
            .await;
    assert_eq!(out["appended_rows"], json!(1), "S3: the append reported no new row");
    // Anchored on the `!`/`:` boundaries: a bare `contains("A3")` would also accept `A30`.
    let landed = out["updated_range"].as_str().unwrap_or_default().to_string();
    assert!(
        landed.contains("!A3:") || landed.ends_with("!A3"),
        "S3: the row landed at {landed}, not at row 3, after the seeded two"
    );

    let mut expected = rows_of(&seed);
    expected.push(vec![json!("S3-APPENDED"), json!("S3-NEW")]);
    assert_eq!(
        grid_at(api, sid, "Append!A1:B3", "UNFORMATTED").await,
        expected,
        "S3: the append disturbed the seeded rows"
    );
    eprintln!("S3 ok: the appended row landed at A3 and the seeded rows are unchanged");
}

/// S4 — the confirm gate is the only thing standing between an agent and someone's data.
///
/// Catches: a gate that computes its preview but writes anyway (the fake can only prove
/// `values.update` was not *called*; this proves the spreadsheet was not *changed*). The confirmed
/// half is asserted too, so a gate that has degenerated into a wall fails here as well.
async fn s4_the_overwrite_gate_is_real(api: &dyn GoogleApi, sid: &str) {
    let seed = json!([["S4-ORIGINAL", "S4-KEEP"]]);
    ok(api, "write_sheet", json!({"item": sid, "tab": "Gate", "rows": seed})).await;

    let clobber = json!([["S4-CLOBBER", "S4-CLOBBER-2"]]);
    let gated = ok(api, "write_sheet", json!({"item": sid, "tab": "Gate", "rows": clobber})).await;
    assert_eq!(gated["status"], json!("confirmation_required"), "S4: the overwrite was not gated");
    assert_eq!(gated["impact"]["overwrites_nonempty_cells"], json!(2), "S4: wrong impact count");
    assert!(gated.get("updated_cells").is_none(), "S4: the gated call reported a write");
    assert_eq!(
        grid_at(api, sid, "Gate!A1:B1", "UNFORMATTED").await,
        rows_of(&seed),
        "S4: the gated write reached the spreadsheet anyway"
    );

    let done =
        ok(api, "write_sheet", json!({"item": sid, "tab": "Gate", "rows": clobber, "confirm": true})).await;
    assert_eq!(done["updated_cells"], json!(2), "S4: the confirmed write did not land");
    assert_eq!(
        grid_at(api, sid, "Gate!A1:B1", "UNFORMATTED").await,
        rows_of(&clobber),
        "S4: the confirmed write did not replace the cells"
    );
    eprintln!("S4 ok: an unconfirmed overwrite changed nothing; the confirmed one landed");
}

/// S5 — a clear must be surgical.
///
/// Catches: a range widened on the way to `values.clear` (an unbounded `Tab!B:B`, a half-open
/// bound applied to A1 notation, a tab-wide fallback) — every one of which would take the
/// neighbours with it. The ring of eight surrounding cells is what makes that visible.
async fn s5_clear_range_clears_exactly_its_range(api: &dyn GoogleApi, sid: &str) {
    let seed =
        json!([["S5-A1", "S5-B1", "S5-C1"], ["S5-A2", "S5-B2", "S5-C2"], ["S5-A3", "S5-B3", "S5-C3"],]);
    ok(api, "write_sheet", json!({"item": sid, "tab": "Clear", "rows": seed})).await;

    let out = ok(api, "clear_range", json!({"item": sid, "a1_range": "Clear!B2", "confirm": true})).await;
    // `cleared_range` is the tool echoing its own argument, so it pins the response shape and
    // nothing about Google. `cleared_nonempty_cells` comes from the live pre-read, so it does.
    // What settles the claim is the grid comparison below.
    assert_eq!(out["cleared_range"], json!("Clear!B2"), "S5: cleared the wrong range");
    assert_eq!(out["cleared_nonempty_cells"], json!(1), "S5: wrong cleared-cell count");

    let got: Vec<Vec<Value>> = grid_at(api, sid, "Clear!A1:C3", "UNFORMATTED")
        .await
        .into_iter()
        .map(|row| row.into_iter().map(|c| if blank(&c) { json!("") } else { c }).collect())
        .collect();
    let mut expected = rows_of(&seed);
    expected[1][1] = json!(""); // only the named cell
    assert_eq!(got, expected, "S5: the clear did not stop at its range");
    eprintln!("S5 ok: B2 is empty and all eight neighbours survived");
}

/// S6 — `start_row` is 1-based and `deleteDimension` is 0-based half-open.
///
/// Catches an off-by-one in either direction, which is why this asserts on distinctive row content
/// and not on a row count: deleting rows 3-4 instead of 2-3 leaves exactly as many rows behind.
/// The unconfirmed preview is checked first because it reads `'Delete'!2:3` — a real range the
/// Sheets API has to accept — so the row-span spelling is verified live too.
async fn s6_delete_rows_deletes_the_rows_the_caller_named(api: &dyn GoogleApi, sid: &str) {
    let seed = json!([["S6-ROW-1"], ["S6-ROW-2"], ["S6-ROW-3"], ["S6-ROW-4"], ["S6-ROW-5"]]);
    ok(api, "write_sheet", json!({"item": sid, "tab": "Delete", "rows": seed})).await;

    let gated =
        ok(api, "delete_rows", json!({"item": sid, "tab": "Delete", "start_row": 2, "count": 2})).await;
    assert_eq!(gated["status"], json!("confirmation_required"), "S6: the delete was not gated");
    assert_eq!(
        gated["impact"]["sample"],
        json!([["S6-ROW-2"], ["S6-ROW-3"]]),
        "S6: the preview named the wrong rows"
    );

    let out = ok(
        api,
        "delete_rows",
        json!({"item": sid, "tab": "Delete", "start_row": 2, "count": 2, "confirm": true}),
    )
    .await;
    // Echoes `count` back, so it proves only that the batch was accepted; the token list below is
    // what distinguishes "deleted rows 2-3" from "deleted rows 3-4", which leave equal row counts.
    assert_eq!(out["deleted_rows"], json!(2), "S6: wrong deleted-row count");

    let left = tokens(&grid_at(api, sid, "Delete!A1:A5", "UNFORMATTED").await);
    assert_eq!(left, ["S6-ROW-1", "S6-ROW-4", "S6-ROW-5"], "S6: the wrong rows were deleted");
    eprintln!("S6 ok: rows 2-3 went and rows 1, 4, 5 stayed");
}

/// S7 — the spill is real data on a real disk.
///
/// Catches: a CSV that does not match the sheet (a rendering or ragged-row bug), a spill left
/// world-readable (it can hold sensitive data), or one written outside the sandbox. Only a live sheet plus a
/// real filesystem can show all three at once.
async fn s7_read_full_sheet_spills_the_sheet_into_the_sandbox(
    api: &dyn GoogleApi,
    sid: &str,
    spill_root: &Path,
) {
    let seed = json!([["S7-h1", "S7-h2"], ["S7-a", "S7-b"], ["S7-c", "S7-d"]]);
    ok(api, "write_sheet", json!({"item": sid, "tab": "Spill", "rows": seed})).await;

    let out = ok(api, "read_full_sheet", json!({"item": sid, "tab": "Spill"})).await;
    assert_eq!(out["total_rows"], json!(3), "S7: wrong row total");
    assert_eq!(out["total_cols"], json!(2), "S7: wrong column total");
    assert_eq!(
        out["preview"]["records"][0],
        json!({"S7-h1": "S7-a", "S7-h2": "S7-b"}),
        "S7: the preview does not match the sheet"
    );

    let path = PathBuf::from(out["path"].as_str().expect("S7: no spill path"));
    assert!(path.starts_with(spill_root), "S7: the spill escaped the sandbox: {}", path.display());
    let body = std::fs::read_to_string(&path).expect("S7: the spilled CSV is unreadable");
    assert_eq!(body, "S7-h1,S7-h2\r\nS7-a,S7-b\r\nS7-c,S7-d\r\n", "S7: the CSV does not match the sheet");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("S7: stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "S7: the spill is not owner-only");
    }
    eprintln!("S7 ok: {} matches the sheet, owner-only, inside the sandbox", path.display());
}

/// S8 — formatting must not touch values, and must stop at its range.
///
/// Catches a `repeatCell` range built from the wrong `sheetId` or the wrong bounds. The values half
/// is unconditional; the styling half needs `spreadsheets.get` to return grid data for the field
/// mask, and says so rather than passing quietly if it ever stops doing that. `styling_observed` is
/// set only on the path that really inspected the grid, so the run's closing line cannot claim more
/// than was seen.
async fn s8_format_cells_styles_its_range_and_leaves_values_alone(
    api: &dyn GoogleApi,
    sid: &str,
    styling_observed: &Cell<bool>,
) {
    let seed = json!([["S8-A1", "S8-B1"], ["S8-A2", "S8-B2"]]);
    ok(api, "write_sheet", json!({"item": sid, "tab": "Format", "rows": seed})).await;

    let out = ok(
        api,
        "format_cells",
        json!({"item": sid, "a1_range": "Format!A1:B1", "bold": true, "italic": true,
               "underline": true}),
    )
    .await;
    // These four are the tool restating its own inputs (`cells` is computed locally from
    // `parse_range`), already pinned offline by check 54. They are kept as a response-shape guard;
    // the live claim is settled by the values re-read and the `spreadsheets.get` grid below.
    assert_eq!(out["tab"], json!("Format"), "S8: resolved the wrong tab");
    assert_eq!(out["formatted_range"], json!("Format!A1:B1"), "S8: reported the wrong range");
    assert_eq!(out["cells"], json!(2), "S8: wrong cell count");
    assert_eq!(
        out["applied"],
        json!({"bold": true, "italic": true, "underline": true}),
        "S8: reported the wrong styling"
    );
    assert_eq!(
        grid_at(api, sid, "Format!A1:B2", "UNFORMATTED").await,
        rows_of(&seed),
        "S8: formatting changed the values"
    );

    let meta = api.sheets_get(sid, FORMAT_FIELDS).await.expect("S8: spreadsheets.get");
    let styled = text_formats(&meta, "Format", 0);
    if styled.is_empty() {
        eprintln!(
            "S8 WARNING: spreadsheets.get returned no grid data for the field mask, so the styling \
             itself was NOT verified — only that the batch was accepted and the values are intact"
        );
    } else {
        for (i, tf) in styled.iter().enumerate() {
            for flag in ["bold", "italic", "underline"] {
                assert_eq!(tf.get(flag), Some(&json!(true)), "S8: row 1 cell {i} is missing {flag}: {tf}");
            }
        }
        // Row 2 is outside the range; it must not have picked the styling up.
        for (i, tf) in text_formats(&meta, "Format", 1).iter().enumerate() {
            for flag in ["bold", "italic", "underline"] {
                assert_ne!(tf.get(flag), Some(&json!(true)), "S8: row 2 cell {i} was styled too: {tf}");
            }
        }
        styling_observed.set(true);
        eprintln!("S8 ok: A1:B1 is bold/italic/underlined, row 2 is not, values unchanged");
    }
}

/// S9 — the check most likely to catch a real porting bug.
///
/// Google escapes a `'` inside a quoted A1 name by doubling it. Without `quote_tab`'s doubling the
/// name terminates at the apostrophe and the range is either rejected (400) or — worse — silently
/// mis-targeted. No fake can catch that: the fake accepts whatever string it is handed. This drives
/// all three range spellings the tools build for such a tab: bounded (`write_sheet`, `read_sheet`
/// via `tab`), bare tab (`append_rows`, `read_full_sheet`), and a caller-spelled verbatim range.
async fn s9_a1_quoting_survives_an_apostrophe_in_a_tab_name(
    api: &dyn GoogleApi,
    sid: &str,
    spill_root: &Path,
) {
    let made = ok(api, "add_tab", json!({"item": sid, "title": HOSTILE_TAB})).await;
    assert_eq!(made["title"], json!(HOSTILE_TAB), "S9: the tab was not created under that name");

    let out = ok(api, "write_sheet", json!({"item": sid, "tab": HOSTILE_TAB, "rows": [["S9-QUOTED"]]})).await;
    assert_eq!(out["updated_cells"], json!(1), "S9: the write did not land");
    // `updatedRange` is Google's own echo of the range it resolved. Whether Google re-serialises
    // the title with the apostrophe doubled is *its* spelling choice, not our correctness, so
    // demanding the doubled form would abort the run over a formatting nit. What is load-bearing is
    // that the name did not terminate at the apostrophe: the range must still name the whole tab,
    // and must be the single cell asked for rather than some other block.
    let echoed = out["updated_range"].as_str().unwrap_or_default();
    assert!(
        echoed.contains("John''s Data") || echoed.contains("John's Data"),
        "S9: Sheets resolved the range as {echoed}, not a range on `{HOSTILE_TAB}`"
    );
    assert!(
        echoed.ends_with("!A1") || echoed.ends_with("!A1:A1"),
        "S9: the write landed at {echoed}, not at A1 of `{HOSTILE_TAB}`"
    );

    ok(api, "append_rows", json!({"item": sid, "tab": HOSTILE_TAB, "rows": [["S9-APPENDED"]]})).await;

    // Read it back through the `tab` path (the tools quote it) ...
    let via_tab = ok(api, "read_sheet", json!({"item": sid, "tab": HOSTILE_TAB, "header_row": false})).await;
    assert_eq!(
        tokens(&rows_of(&via_tab["tabs"][0]["rows"])),
        ["S9-QUOTED", "S9-APPENDED"],
        "S9: reading by tab name found the wrong cells"
    );
    // ... and through a verbatim range the caller spelled with the doubled quote itself.
    assert_eq!(
        cell_at(api, sid, "'John''s Data'!A1", "UNFORMATTED").await,
        json!("S9-QUOTED"),
        "S9: a verbatim quoted range did not resolve"
    );

    // The spill path quotes the bare tab name and sanitizes it into a filename.
    let spilled = ok(api, "read_full_sheet", json!({"item": sid, "tab": HOSTILE_TAB})).await;
    let path = PathBuf::from(spilled["path"].as_str().expect("S9: no spill path"));
    assert!(path.starts_with(spill_root), "S9: the spill escaped the sandbox: {}", path.display());
    assert_eq!(
        std::fs::read_to_string(&path).expect("S9: the spilled CSV is unreadable"),
        "S9-QUOTED\r\nS9-APPENDED\r\n",
        "S9: the spill of the quoted tab is wrong"
    );
    eprintln!("S9 ok: every range spelling for `{HOSTILE_TAB}` resolved to the intended cells");
}

/// S10 — the three `values` render options are documented as different; only Google decides that.
///
/// A date formula is used because it separates all three cleanly: FORMULA gives the text, an
/// UNFORMATTED read gives the serial number, a FORMATTED read gives the displayed date. Catches a
/// `render_option` mapping that collapses two of them (e.g. everything falling through to
/// UNFORMATTED_VALUE), which offline can only be checked as the string sent to a fake.
async fn s10_the_render_options_differ(api: &dyn GoogleApi, sid: &str) {
    ok(
        api,
        "write_sheet",
        json!({"item": sid, "tab": "Data", "rows": [["=DATE(2020,1,2)"]], "start_cell": "E1",
               "value_input": "USER_ENTERED"}),
    )
    .await;

    let formula = cell_at(api, sid, "Data!E1", "FORMULA").await;
    let unformatted = cell_at(api, sid, "Data!E1", "UNFORMATTED").await;
    let formatted = cell_at(api, sid, "Data!E1", "FORMATTED").await;

    assert!(
        formula.as_str().is_some_and(|f| f.starts_with("=DATE")),
        "S10: FORMULA returned {formula}, not the formula text"
    );
    // 2020-01-02 as a Sheets serial (day 0 is 1899-12-30).
    assert_eq!(unformatted.as_f64(), Some(43832.0), "S10: UNFORMATTED returned {unformatted}, not a serial");
    let shown = formatted.as_str().unwrap_or_else(|| panic!("S10: FORMATTED returned {formatted}, not text"));
    // Locale decides the order of the fields, so only the year is pinned. The three renderings
    // being pairwise `!=` as `Value`s follows for free from the type checks above (string / number /
    // string), so asserting it would add nothing; these two are the discriminators that can fail —
    // FORMATTED collapsing into UNFORMATTED's serial, or into FORMULA's text.
    assert!(shown.contains("2020"), "S10: FORMATTED returned '{shown}', not a rendered date");
    assert_ne!(shown, "43832", "S10: FORMATTED returned the raw serial, not a rendered date");
    assert!(!shown.starts_with("=DATE"), "S10: FORMATTED returned the formula text '{shown}'");
    eprintln!("S10 ok: FORMULA {formula}, UNFORMATTED {unformatted}, FORMATTED {formatted}");
}

/// S11 — the overwrite gate must protect a formula that currently renders as nothing.
///
/// `write_sheet`'s pre-check used to read the target with `UNFORMATTED_VALUE`, and `count_nonempty`
/// treats `""` as empty. A cell holding `=IF(1=1,"","x")` therefore looked empty, the gate did not
/// fire, and a `confirm`-less write destroyed the formula. Reading `FORMULA` closes it.
///
/// Only a live run can falsify this: it turns entirely on Google rendering the same cell as `""`
/// under one option and as formula text under the other. A fake that returns one canned grid
/// regardless of the render option agrees with either implementation.
async fn s11_a_formula_rendering_empty_still_trips_the_gate(api: &dyn GoogleApi, sid: &str) {
    // Reuses S4's tab, which S4 left holding plain text — this overwrites A1 deliberately.
    let formula = "=IF(1=1,\"\",\"S11-VISIBLE\")";
    ok(
        api,
        "write_sheet",
        json!({"item": sid, "tab": "Gate", "rows": [[formula]], "start_cell": "A3",
               "value_input": "USER_ENTERED", "confirm": true}),
    )
    .await;

    // The precondition for the whole bug: to a values read, this cell looks empty.
    let rendered = cell_at(api, sid, "Gate!A3", "UNFORMATTED").await;
    assert!(
        blank(&rendered),
        "S11: precondition failed — the formula rendered as {rendered}, not empty, so this run \
         cannot exercise the blind spot"
    );
    assert_eq!(
        cell_at(api, sid, "Gate!A3", "FORMULA").await.as_str(),
        Some(formula),
        "S11: the cell does not hold the formula"
    );

    // ...and yet the gate must fire, because a formula is data.
    let gated = ok(
        api,
        "write_sheet",
        json!({"item": sid, "tab": "Gate", "rows": [["S11-CLOBBER"]],
                                             "start_cell": "A3"}),
    )
    .await;
    assert_eq!(
        gated["status"],
        json!("confirmation_required"),
        "S11: a formula rendering empty did NOT trip the gate — a confirm-less write would have \
         destroyed it"
    );
    assert_eq!(gated["impact"]["overwrites_nonempty_cells"], json!(1), "S11: wrong impact count");
    assert_eq!(
        cell_at(api, sid, "Gate!A3", "FORMULA").await.as_str(),
        Some(formula),
        "S11: the gated write reached the spreadsheet anyway"
    );

    // The gate is a gate, not a wall: confirming still writes.
    ok(
        api,
        "write_sheet",
        json!({"item": sid, "tab": "Gate", "rows": [["S11-CLOBBER"]],
                                  "start_cell": "A3", "confirm": true}),
    )
    .await;
    assert_eq!(
        cell_at(api, sid, "Gate!A3", "UNFORMATTED").await.as_str(),
        Some("S11-CLOBBER"),
        "S11: the confirmed write did not land"
    );
    eprintln!("S11 ok: a formula rendering empty tripped the gate; confirming still wrote");
}

/// `catch_unwind` for a future, so the scratch spreadsheet is always trashed.
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
    for tool in [
        "create_spreadsheet",
        "add_tab",
        "read_sheet",
        "read_full_sheet",
        "write_sheet",
        "append_rows",
        "clear_range",
        "delete_rows",
        "format_cells",
    ] {
        assert!(names.contains(&tool), "{tool} is not registered");
    }
    // S9 asserts Google resolves these spellings; this pins what the tools will actually send.
    assert_eq!(quote_tab(HOSTILE_TAB), "'John''s Data'");
    assert_eq!(build_range(HOSTILE_TAB, "A1", 1, 1).unwrap(), "'John''s Data'!A1:A1");
}
