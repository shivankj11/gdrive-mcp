//! Live, credentialed checks C1-C12 for all eight Calendar tools.
//!
//! Skipped unless `GDRIVE_MCP_LIVE=1`. The test creates one synthetic recurring event on the
//! authenticated user's primary calendar with `sendUpdates=none`, then deletes it even after a
//! panic. The caller-supplied event id is printed first so failed cleanup is recoverable by hand.
//!
//!     GDRIVE_MCP_LIVE=1 cargo test --test live_calendar -- --ignored --nocapture

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use chrono::{Duration, SecondsFormat, Timelike, Utc};
use serde_json::{json, Value};

use gdrive_mcp::args::Args;
use gdrive_mcp::clients::{GoogleApi, GoogleClient};
use gdrive_mcp::error::Result;
use gdrive_mcp::tools::{self, ToolOutput};

fn live() -> bool {
    std::env::var("GDRIVE_MCP_LIVE").is_ok_and(|value| !value.is_empty())
}

async fn call(api: &dyn GoogleApi, tool: &str, arguments: Value) -> Result<Value> {
    let defs = tools::all_defs();
    let def = defs.iter().find(|def| def.name == tool).expect("tool is registered");
    let map = arguments.as_object().cloned().unwrap_or_default();
    let args = Args::new(tool, Some(map), &def.params())?;
    match tools::dispatch(tool, api, &args).await? {
        ToolOutput::Json(value) => Ok(value),
        ToolOutput::Images(_) => panic!("{tool} returned images"),
    }
}

async fn ok(api: &dyn GoogleApi, tool: &str, arguments: Value) -> Value {
    call(api, tool, arguments).await.unwrap_or_else(|error| panic!("{tool} failed: {error}"))
}

async fn missing(api: &dyn GoogleApi, event_id: &str) -> bool {
    match api.calendar_events_get("primary", event_id).await {
        // Calendar may retain a tombstone that GET returns as a cancelled event.
        Ok(event) => event["status"] == json!("cancelled"),
        Err(error) => matches!(error.status(), Some(404 | 410)),
    }
}

#[tokio::test]
#[ignore = "live: writes to a real Calendar; needs GDRIVE_MCP_LIVE=1 and cached credentials"]
async fn calendar_tools_hold_against_the_real_calendar_api() {
    if !live() {
        eprintln!("skipped: set GDRIVE_MCP_LIVE=1 to run the live Calendar checks");
        return;
    }
    let api: Arc<dyn GoogleApi> = Arc::new(GoogleClient::new());
    let event_id = uuid::Uuid::new_v4().simple().to_string();
    eprintln!("scratch calendar event: primary/{event_id}");

    let outcome = AssertUnwindSafe(checks(api.as_ref(), &event_id)).catch_unwind_async().await;
    if !missing(api.as_ref(), &event_id).await {
        if let Err(error) = api.calendar_events_delete("primary", &event_id, "none", None).await {
            eprintln!("WARNING: could not delete scratch Calendar event primary/{event_id}: {error}");
        }
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn checks(api: &dyn GoogleApi, event_id: &str) {
    let summary = format!("gdrive-mcp calendar live check {}", &event_id[..8]);
    let start_dt = (Utc::now() + Duration::days(400))
        .with_minute(0)
        .and_then(|value| value.with_second(0))
        .and_then(|value| value.with_nanosecond(0))
        .expect("valid rounded time");
    let start = start_dt.to_rfc3339_opts(SecondsFormat::Secs, true);
    let end = (start_dt + Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let window_end = (start_dt + Duration::days(3)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let email = api.authed_user_email().await.expect("C1: authenticated user email");

    // C1: CalendarList is authorized and includes the primary calendar.
    let calendars = ok(api, "list_calendars", json!({"page_size": 250})).await;
    assert!(
        calendars["calendars"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["primary"] == json!(true))),
        "C1: primary calendar missing"
    );

    let create_args = json!({
        "summary": summary,
        "start": start,
        "end": end,
        "event_id": event_id,
        "time_zone": "UTC",
        "description": "C4-original",
        "location": "Synthetic test location",
        "attendees": [email],
        "recurrence": ["RRULE:FREQ=DAILY;COUNT=2"],
        "reminders": [{"method": "popup", "minutes": 10}],
        "visibility": "private",
        "send_updates": "none"
    });

    // C2/C3: both preview modes are non-writing, and the caller id survives the preview.
    let preview = ok(api, "create_event", create_args.clone()).await;
    assert_eq!(preview["status"], json!("confirmation_required"));
    assert_eq!(preview["impact"]["event"]["id"], json!(event_id));
    assert!(missing(api, event_id).await, "C2: unconfirmed create wrote an event");
    let mut dry_args = create_args.clone();
    dry_args["dry_run"] = json!(true);
    let dry = ok(api, "create_event", dry_args).await;
    assert_eq!(dry["dry_run"], json!(true));
    assert_eq!(dry["event"]["id"], json!(event_id));
    assert!(missing(api, event_id).await, "C3: dry-run create wrote an event");

    // C4: confirmed create round-trips rich fields without sending mail.
    let mut confirmed_args = create_args;
    confirmed_args["confirm"] = json!(true);
    let made = ok(api, "create_event", confirmed_args).await;
    assert_eq!(made["created"], json!(true));
    assert_eq!(made["event"]["id"], json!(event_id));
    let got = ok(api, "get_event", json!({"event_id": event_id})).await;
    assert_eq!(got["event"]["summary"], json!(summary));
    assert_eq!(got["event"]["location"], json!("Synthetic test location"));
    assert_eq!(got["event"]["recurrence"], json!(["RRULE:FREQ=DAILY;COUNT=2"]));
    assert_eq!(got["event"]["visibility"], json!("private"));
    assert!(got["event"]["etag"].as_str().is_some_and(|value| !value.is_empty()));

    // C5: list_events expands the series, applies q, orders by start, and paginates.
    let query = &event_id[..8];
    let first = ok(
        api,
        "list_events",
        json!({"time_min": start, "time_max": window_end, "query": query, "page_size": 1}),
    )
    .await;
    assert_eq!(first["count"], json!(1));
    assert_eq!(first["has_more"], json!(true));
    assert_eq!(first["events"][0]["recurring_event_id"], json!(event_id));
    let second = ok(
        api,
        "list_events",
        json!({
            "time_min": start,
            "time_max": window_end,
            "query": query,
            "page_size": 1,
            "page_token": first["next_page_token"]
        }),
    )
    .await;
    assert_eq!(second["count"], json!(1));
    assert_eq!(second["events"][0]["recurring_event_id"], json!(event_id));

    // C6: free/busy reports the first occurrence as busy.
    let freebusy =
        ok(api, "query_freebusy", json!({"time_min": start, "time_max": end, "time_zone": "UTC"})).await;
    let busy = freebusy.pointer("/calendars/primary/busy").and_then(Value::as_array).expect("C6: busy list");
    assert!(busy.iter().any(|slot| {
        slot["start"].as_str().is_some_and(|value| value <= start.as_str())
            && slot["end"].as_str().is_some_and(|value| value >= end.as_str())
    }));

    // C7: If-Match rejects a write based on an obsolete representation.
    let stale = api.calendar_events_get("primary", event_id).await.expect("C7: get stale representation");
    api.calendar_events_patch(
        "primary",
        event_id,
        &json!({"description": "C7-out-of-band"}),
        "none",
        0,
        None,
    )
    .await
    .expect("C7: out-of-band update");
    let rejected = api
        .calendar_events_patch(
            "primary",
            event_id,
            &json!({"summary": "C7-SHOULD-NOT-LAND"}),
            "none",
            0,
            stale["etag"].as_str(),
        )
        .await;
    assert_eq!(rejected.expect_err("C7: stale patch unexpectedly landed").status(), Some(412));
    assert_eq!(ok(api, "get_event", json!({"event_id": event_id})).await["event"]["summary"], json!(summary));

    // C8/C9: update previews are non-writing; confirmed PATCH uses a fresh ETag.
    let update_args = json!({
        "event_id": event_id,
        "summary": format!("{summary} updated"),
        "description": "C9-updated",
        "availability": "free",
        "send_updates": "none"
    });
    let preview = ok(api, "update_event", update_args.clone()).await;
    assert_eq!(preview["status"], json!("confirmation_required"));
    assert_eq!(ok(api, "get_event", json!({"event_id": event_id})).await["event"]["summary"], json!(summary));
    let mut confirmed = update_args;
    confirmed["etag"] = preview["impact"]["before"]["etag"].clone();
    confirmed["confirm"] = json!(true);
    let changed = ok(api, "update_event", confirmed).await;
    assert_eq!(changed["updated"], json!(true));
    assert!(changed["event"]["summary"].as_str().is_some_and(|value| value.ends_with(" updated")));
    assert_eq!(changed["event"]["description"], json!("C9-updated"));
    assert_eq!(changed["event"]["availability"], json!("free"));

    // C10: RSVP preview and confirmed response affect the signed-in attendee.
    let response_args = json!({"event_id": event_id, "response_status": "tentative"});
    let response_preview = ok(api, "respond_to_event", response_args.clone()).await;
    assert_eq!(response_preview["status"], json!("confirmation_required"));
    assert_eq!(response_preview["impact"]["to_status"], json!("tentative"));
    let mut response_confirmed = response_args;
    response_confirmed["etag"] = response_preview["impact"]["etag"].clone();
    response_confirmed["confirm"] = json!(true);
    let responded = ok(api, "respond_to_event", response_confirmed).await;
    assert_eq!(responded["responded"], json!(true));
    let me = responded["event"]["attendees"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["self"] == json!(true)))
        .expect("C10: self attendee missing");
    assert_eq!(me["responseStatus"], json!("tentative"));

    // C11/C12: delete preview preserves the series; confirmed delete removes it.
    let deletion = ok(api, "delete_event", json!({"event_id": event_id})).await;
    assert_eq!(deletion["status"], json!("confirmation_required"));
    assert_eq!(deletion["impact"]["deletes_series"], json!(true));
    assert!(!missing(api, event_id).await, "C11: unconfirmed delete removed the event");
    let deleted = ok(
        api,
        "delete_event",
        json!({
            "event_id": event_id,
            "etag": deletion["impact"]["event"]["etag"],
            "confirm": true
        }),
    )
    .await;
    assert_eq!(deleted["deleted"], json!(true));
    assert!(missing(api, event_id).await, "C12: confirmed delete left the event behind");
}

trait CatchUnwindAsync: Future {
    async fn catch_unwind_async(self) -> std::result::Result<Self::Output, Box<dyn std::any::Any + Send>>;
}

impl<F: Future + std::panic::UnwindSafe> CatchUnwindAsync for F {
    async fn catch_unwind_async(self) -> std::result::Result<Self::Output, Box<dyn std::any::Any + Send>> {
        use std::pin::pin;
        use std::task::Poll;
        let mut future = pin!(self);
        std::future::poll_fn(move |context| {
            match std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
                Err(panic) => Poll::Ready(Err(panic)),
            }
        })
        .await
    }
}

#[test]
fn the_live_harness_targets_all_calendar_tools() {
    let names: Vec<&str> = tools::all_defs().into_iter().map(|definition| definition.name).collect();
    for tool in [
        "list_calendars",
        "list_events",
        "get_event",
        "query_freebusy",
        "create_event",
        "update_event",
        "delete_event",
        "respond_to_event",
    ] {
        assert!(names.contains(&tool), "{tool} is not registered");
    }
}
