//! Google Calendar read and write tools.

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

use super::{ToolDef, ToolOutput};
use crate::args::Args;
use crate::clients::{CalendarEventsListParams, GoogleApi};
use crate::error::{Result, ToolError};
use crate::guard::{preview_response, preview_response_with};

const LIST_CALENDARS_DOC: &str =
    "List calendars available to the signed-in user, one bounded page at a time.\n\
    \n\
    `min_access_role` may be freeBusyReader, reader, writer, or owner. When `has_more` is true,\n\
    pass `next_page_token` to continue. Calendar names and IDs may contain sensitive data.";

const LIST_EVENTS_DOC: &str =
    "List expanded event instances in a bounded time window, ordered by start time.\n\
    \n\
    `calendar_id` defaults to the primary calendar. Times must be RFC3339 with an offset. With\n\
    neither bound, the window is now through 30 days from now; pass both for deterministic reads.\n\
    Free-text `query` searches event details and attendees. Use pagination when `has_more` is true.";

const GET_EVENT_DOC: &str = "Get one event by its API event ID and calendar ID (default: primary calendar).";

const QUERY_FREEBUSY_DOC: &str = "Return busy intervals for up to 50 calendars in an RFC3339 time window.\n\
    \n\
    `calendar_ids` defaults to [\"primary\"]. Results reveal availability only, not event details.\n\
    `time_zone`, when supplied, must be an IANA name such as America/Los_Angeles.";

const CREATE_EVENT_DOC: &str =
    "Create an event after previewing its time, attendees, and notification impact.\n\
    \n\
    Timed values require RFC3339 offsets; all-day values are YYYY-MM-DD and `end` is exclusive.\n\
    Optional `event_id` makes retries idempotent and must be 5-1024 lowercase base32hex characters.\n\
    `send_updates` is none, externalOnly, or all and defaults to none. Recurrence entries use\n\
    RRULE:/EXRULE:/RDATE:/EXDATE:; recurring timed events require `time_zone`. dry_run=true returns\n\
    the proposed event. Without confirm=true, nothing is created and a confirmation preview is\n\
    returned.";

const UPDATE_EVENT_DOC: &str = "Partially update an event after returning a live before/after preview.\n\
    \n\
    Supply start and end together when changing time; adding recurrence to a timed event requires\n\
    zoned start/end values. Passing attendees=[] or recurrence=[] clears that list; leaving an\n\
    argument unset preserves it. `send_updates` defaults to none. Preview first, then pass its event\n\
    `etag` with confirm=true; a changed event is refused. dry_run=true previews, and without\n\
    confirm=true nothing is written.";

const DELETE_EVENT_DOC: &str = "Delete one event or recurring instance after a live impact preview.\n\
    \n\
    Pass a recurring master event ID to delete the series, or an expanded instance ID to delete\n\
    one occurrence. `send_updates` defaults to none. Preview first, then pass its event `etag` with\n\
    confirm=true; a changed event is refused. dry_run=true previews; without confirm=true nothing\n\
    is deleted.";

const RESPOND_TO_EVENT_DOC: &str =
    "Accept, tentatively accept, or decline an invitation after a live preview.\n\
    \n\
    `response_status` is accepted, tentative, or declined. The signed-in attendee must be present\n\
    on the event. `send_updates` defaults to none. Preview first, then pass its event `etag` with\n\
    confirm=true; a changed invitation is refused. dry_run=true previews; without confirm=true the\n\
    response is not changed.";

pub fn defs() -> Vec<ToolDef> {
    let string = || json!({"type": "string"});
    let boolean = || json!({"type": "boolean"});
    let strings = || json!({"type": "array", "items": {"type": "string"}});
    let reminders = || json!({"type": "array", "items": {"type": "object"}});
    vec![
        ToolDef::new(
            "list_calendars",
            LIST_CALENDARS_DOC,
            json!({"properties": {
                "page_size": {"type": "integer", "default": 100},
                "page_token": string(),
                "min_access_role": string(),
                "show_hidden": {"type": "boolean", "default": false}
            }}),
        ),
        ToolDef::new(
            "list_events",
            LIST_EVENTS_DOC,
            json!({"properties": {
                "calendar_id": {"type": "string", "default": "primary"},
                "time_min": string(),
                "time_max": string(),
                "query": string(),
                "page_size": {"type": "integer", "default": 25},
                "page_token": string(),
                "include_cancelled": {"type": "boolean", "default": false}
            }}),
        ),
        ToolDef::new(
            "get_event",
            GET_EVENT_DOC,
            json!({
                "properties": {
                    "event_id": string(),
                    "calendar_id": {"type": "string", "default": "primary"}
                },
                "required": ["event_id"]
            }),
        ),
        ToolDef::new(
            "query_freebusy",
            QUERY_FREEBUSY_DOC,
            json!({
                "properties": {
                    "time_min": string(),
                    "time_max": string(),
                    "calendar_ids": strings(),
                    "time_zone": string()
                },
                "required": ["time_min", "time_max"]
            }),
        ),
        ToolDef::new(
            "create_event",
            CREATE_EVENT_DOC,
            json!({
                "properties": {
                    "summary": string(),
                    "start": string(),
                    "end": string(),
                    "calendar_id": {"type": "string", "default": "primary"},
                    "event_id": string(),
                    "all_day": {"type": "boolean", "default": false},
                    "time_zone": string(),
                    "description": string(),
                    "location": string(),
                    "attendees": strings(),
                    "recurrence": strings(),
                    "reminders": reminders(),
                    "send_updates": {"type": "string", "default": "none"},
                    "add_google_meet": {"type": "boolean", "default": false},
                    "availability": {"type": "string", "default": "busy"},
                    "visibility": {"type": "string", "default": "default"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false}
                },
                "required": ["summary", "start", "end"]
            }),
        ),
        ToolDef::new(
            "update_event",
            UPDATE_EVENT_DOC,
            json!({
                "properties": {
                    "event_id": string(),
                    "calendar_id": {"type": "string", "default": "primary"},
                    "etag": string(),
                    "summary": string(),
                    "start": string(),
                    "end": string(),
                    "all_day": boolean(),
                    "time_zone": string(),
                    "description": string(),
                    "location": string(),
                    "attendees": strings(),
                    "recurrence": strings(),
                    "reminders": reminders(),
                    "send_updates": {"type": "string", "default": "none"},
                    "add_google_meet": boolean(),
                    "availability": string(),
                    "visibility": string(),
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false}
                },
                "required": ["event_id"]
            }),
        ),
        ToolDef::new(
            "delete_event",
            DELETE_EVENT_DOC,
            json!({
                "properties": {
                    "event_id": string(),
                    "calendar_id": {"type": "string", "default": "primary"},
                    "etag": string(),
                    "send_updates": {"type": "string", "default": "none"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false}
                },
                "required": ["event_id"]
            }),
        ),
        ToolDef::new(
            "respond_to_event",
            RESPOND_TO_EVENT_DOC,
            json!({
                "properties": {
                    "event_id": string(),
                    "response_status": string(),
                    "calendar_id": {"type": "string", "default": "primary"},
                    "etag": string(),
                    "send_updates": {"type": "string", "default": "none"},
                    "confirm": {"type": "boolean", "default": false},
                    "dry_run": {"type": "boolean", "default": false}
                },
                "required": ["event_id", "response_status"]
            }),
        ),
    ]
}

fn err(message: impl Into<String>) -> ToolError {
    ToolError::msg(message)
}

fn parse_datetime(value: &str, label: &str) -> Result<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|_| err(format!("{label} must be an RFC3339 timestamp with a UTC offset")))
}

fn parse_date(value: &str, label: &str) -> Result<NaiveDate> {
    if value.len() != 10 || value.contains('T') {
        return Err(err(format!("{label} must be an ISO date (YYYY-MM-DD) for an all-day event")));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| err(format!("{label} must be an ISO date (YYYY-MM-DD) for an all-day event")))
}

fn validate_timezone(value: Option<&str>) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    if value.is_empty()
        || value.starts_with('/')
        || value.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(err(format!("unknown IANA time zone: {value:?}")));
    }
    let root = std::path::Path::new("/usr/share/zoneinfo");
    if root.is_dir() && !root.join(value).is_file() {
        return Err(err(format!("unknown IANA time zone: {value:?}")));
    }
    Ok(())
}

fn time_pair(start: &str, end: &str, all_day: bool, time_zone: Option<&str>) -> Result<(Value, Value)> {
    validate_timezone(time_zone)?;
    if all_day {
        let s = parse_date(start, "start")?;
        let e = parse_date(end, "end")?;
        if e <= s {
            return Err(err("end must be after start; an all-day end date is exclusive"));
        }
        return Ok((json!({"date": start}), json!({"date": end})));
    }
    let s = parse_datetime(start, "start")?;
    let e = parse_datetime(end, "end")?;
    if e <= s {
        return Err(err("end must be after start"));
    }
    let mut start_value = json!({"dateTime": start});
    let mut end_value = json!({"dateTime": end});
    if let Some(zone) = time_zone {
        start_value["timeZone"] = json!(zone);
        end_value["timeZone"] = json!(zone);
    }
    Ok((start_value, end_value))
}

fn send_updates(value: &str) -> Result<&str> {
    if ["none", "externalOnly", "all"].contains(&value) {
        Ok(value)
    } else {
        Err(err("send_updates must be one of: none, externalOnly, all"))
    }
}

fn checked_etag(args: &Args, raw: &Value, require: bool) -> Result<String> {
    let current =
        raw.get("etag").and_then(Value::as_str).filter(|value| !value.is_empty()).ok_or_else(|| {
            err("Google did not return an ETag for this event; refusing an unversioned write")
        })?;
    let expected = args.opt_str("etag")?;
    if expected.as_deref().is_some_and(|value| value != current) {
        return Err(err("the event changed after preview; read it again and approve the new preview"));
    }
    if require && expected.is_none() {
        return Err(err("confirm=true requires the etag returned by the preview"));
    }
    Ok(expected.unwrap_or_else(|| current.to_string()))
}

fn conference_url(event: &Value) -> Value {
    if let Some(value) = event.get("hangoutLink").and_then(Value::as_str).filter(|value| !value.is_empty()) {
        return json!(value);
    }
    event
        .pointer("/conferenceData/entryPoints")
        .and_then(Value::as_array)
        .and_then(|points| {
            points.iter().find_map(|point| {
                (point.get("entryPointType").and_then(Value::as_str) == Some("video"))
                    .then(|| point.get("uri").cloned())
                    .flatten()
            })
        })
        .unwrap_or(Value::Null)
}

fn field(event: &Value, key: &str) -> Value {
    event.get(key).cloned().unwrap_or(Value::Null)
}

fn event(event: &Value) -> Value {
    json!({
        "id": field(event, "id"),
        "status": field(event, "status"),
        "html_link": field(event, "htmlLink"),
        "summary": field(event, "summary"),
        "description": field(event, "description"),
        "location": field(event, "location"),
        "creator": field(event, "creator"),
        "organizer": field(event, "organizer"),
        "start": field(event, "start"),
        "end": field(event, "end"),
        "attendees": event.get("attendees").cloned().unwrap_or_else(|| json!([])),
        "recurrence": event.get("recurrence").cloned().unwrap_or_else(|| json!([])),
        "recurring_event_id": field(event, "recurringEventId"),
        "original_start_time": field(event, "originalStartTime"),
        "conference_url": conference_url(event),
        "visibility": field(event, "visibility"),
        "availability": if event.get("transparency").and_then(Value::as_str) == Some("transparent") { "free" } else { "busy" },
        "etag": field(event, "etag"),
        "created": field(event, "created"),
        "updated": field(event, "updated"),
    })
}

fn dry_run_response(action: &str, impact: Value) -> Value {
    let mut map = json!({"dry_run": true, "action": action}).as_object().unwrap().clone();
    if let Value::Object(extra) = impact {
        map.extend(extra);
    }
    Value::Object(map)
}

fn event_body(args: &Args, create: bool) -> Result<(Value, i64)> {
    let mut body = Map::new();
    let summary = if create { Some(args.req_str("summary")?) } else { args.opt_str("summary")? };
    if let Some(value) = summary {
        if value.trim().is_empty() {
            return Err(err("summary cannot be empty"));
        }
        body.insert("summary".into(), json!(value));
    }
    if create {
        if let Some(value) = args.opt_str("event_id")? {
            let valid = (5..=1024).contains(&value.len())
                && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'v').contains(&b));
            if !valid {
                return Err(err("event_id must be 5-1024 lowercase base32hex characters (0-9, a-v)"));
            }
            body.insert("id".into(), json!(value));
        }
    }
    let start = if create { Some(args.req_str("start")?) } else { args.opt_str("start")? };
    let end = if create { Some(args.req_str("end")?) } else { args.opt_str("end")? };
    if start.is_some() != end.is_some() {
        return Err(err("start and end must be supplied together"));
    }
    let all_day = if create { Some(args.bool_or("all_day", false)?) } else { args.opt_bool("all_day")? };
    let time_zone = args.opt_str("time_zone")?;
    if let (Some(start), Some(end)) = (start, end) {
        let (start, end) = time_pair(&start, &end, all_day.unwrap_or(false), time_zone.as_deref())?;
        body.insert("start".into(), start);
        body.insert("end".into(), end);
    } else if all_day.is_some() || time_zone.is_some() {
        return Err(err("all_day/time_zone can only be changed together with start and end"));
    }
    for key in ["description", "location"] {
        if let Some(value) = args.opt_str(key)? {
            body.insert(key.into(), json!(value));
        }
    }
    if let Some(attendees) = args.opt_str_list("attendees")? {
        if attendees.iter().any(|email| !email.contains('@')) {
            return Err(err("each attendee must be an email address"));
        }
        body.insert(
            "attendees".into(),
            Value::Array(attendees.into_iter().map(|email| json!({"email": email})).collect()),
        );
    }
    if let Some(recurrence) = args.opt_str_list("recurrence")? {
        if recurrence.iter().any(|rule| {
            !["RRULE:", "EXRULE:", "RDATE:", "EXDATE:"].iter().any(|prefix| rule.starts_with(prefix))
        }) {
            return Err(err("recurrence entries must start with RRULE:, EXRULE:, RDATE:, or EXDATE:"));
        }
        body.insert("recurrence".into(), json!(recurrence));
    }
    if let Some(reminders) = args.opt_object_list("reminders")? {
        if reminders.len() > 5 {
            return Err(err("Google Calendar accepts at most 5 reminder overrides"));
        }
        let mut overrides = Vec::new();
        for reminder in reminders {
            let method = reminder.get("method").and_then(Value::as_str);
            let minutes = reminder.get("minutes").and_then(Value::as_i64);
            if !matches!(method, Some("email" | "popup"))
                || !minutes.is_some_and(|m| (0..=40320).contains(&m))
            {
                return Err(err(
                    "each reminder needs method=email|popup and integer minutes between 0 and 40320",
                ));
            }
            overrides.push(json!({"method": method, "minutes": minutes}));
        }
        body.insert("reminders".into(), json!({"useDefault": false, "overrides": overrides}));
    }
    let add_meet = if create {
        Some(args.bool_or("add_google_meet", false)?)
    } else {
        args.opt_bool("add_google_meet")?
    };
    let mut conference_version = 0;
    if add_meet == Some(true) {
        body.insert(
            "conferenceData".into(),
            json!({"createRequest": {
                "requestId": uuid::Uuid::new_v4().to_string(),
                "conferenceSolutionKey": {"type": "hangoutsMeet"}
            }}),
        );
        conference_version = 1;
    }
    let availability =
        if create { Some(args.str_or("availability", "busy")?) } else { args.opt_str("availability")? };
    if let Some(value) = availability {
        let transparency = match value.as_str() {
            "busy" => "opaque",
            "free" => "transparent",
            _ => return Err(err("availability must be busy or free")),
        };
        body.insert("transparency".into(), json!(transparency));
    }
    let visibility =
        if create { Some(args.str_or("visibility", "default")?) } else { args.opt_str("visibility")? };
    if let Some(value) = visibility {
        if !["default", "public", "private", "confidential"].contains(&value.as_str()) {
            return Err(err("visibility must be one of: default, public, private, confidential"));
        }
        body.insert("visibility".into(), json!(value));
    }
    Ok((Value::Object(body), conference_version))
}

async fn list_calendars(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let page_size = args.i64_or("page_size", 100)?.clamp(1, 250);
    let page_token = args.opt_str("page_token")?;
    let role = args.opt_str("min_access_role")?;
    if role.as_deref().is_some_and(|value| !["freeBusyReader", "reader", "writer", "owner"].contains(&value))
    {
        return Err(err("min_access_role must be one of: freeBusyReader, reader, writer, owner"));
    }
    let resp = api
        .calendar_list(page_size, page_token.as_deref(), role.as_deref(), args.bool_or("show_hidden", false)?)
        .await?;
    let calendars: Vec<Value> = resp
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|cal| {
                    json!({
                        "id": field(cal, "id"),
                        "summary": cal
                            .get("summaryOverride")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map_or_else(|| field(cal, "summary"), |value| json!(value)),
                        "description": field(cal, "description"),
                        "location": field(cal, "location"),
                        "time_zone": field(cal, "timeZone"),
                        "access_role": field(cal, "accessRole"),
                        "primary": cal.get("primary").and_then(Value::as_bool).unwrap_or(false),
                        "selected": cal.get("selected").and_then(Value::as_bool).unwrap_or(false),
                        "hidden": cal.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let token = field(&resp, "nextPageToken");
    let has_more = token.as_str().is_some_and(|value| !value.is_empty());
    Ok(json!({"count": calendars.len(), "calendars": calendars, "has_more": has_more, "next_page_token": token}).into())
}

async fn list_events(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let now = Utc::now();
    let time_min =
        args.opt_str("time_min")?.unwrap_or_else(|| now.to_rfc3339_opts(SecondsFormat::Micros, true));
    let time_max = args
        .opt_str("time_max")?
        .unwrap_or_else(|| (now + chrono::Duration::days(30)).to_rfc3339_opts(SecondsFormat::Micros, true));
    if parse_datetime(&time_max, "time_max")? <= parse_datetime(&time_min, "time_min")? {
        return Err(err("time_max must be after time_min"));
    }
    let calendar_id = args.str_or("calendar_id", "primary")?;
    let query = args.opt_str("query")?;
    let page_token = args.opt_str("page_token")?;
    let resp = api
        .calendar_events_list(CalendarEventsListParams {
            calendar_id: &calendar_id,
            time_min: &time_min,
            time_max: &time_max,
            query: query.as_deref(),
            max_results: args.i64_or("page_size", 25)?.clamp(1, 2500),
            page_token: page_token.as_deref(),
            show_deleted: args.bool_or("include_cancelled", false)?,
        })
        .await?;
    let events: Vec<Value> = resp
        .get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(event).collect())
        .unwrap_or_default();
    let token = field(&resp, "nextPageToken");
    let has_more = token.as_str().is_some_and(|value| !value.is_empty());
    Ok(json!({
        "calendar_id": calendar_id,
        "time_min": time_min,
        "time_max": time_max,
        "time_zone": field(&resp, "timeZone"),
        "count": events.len(),
        "events": events,
        "has_more": has_more,
        "next_page_token": token,
    })
    .into())
}

async fn get_event(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let calendar_id = args.str_or("calendar_id", "primary")?;
    let raw = api.calendar_events_get(&calendar_id, &args.req_str("event_id")?).await?;
    Ok(json!({"calendar_id": calendar_id, "event": event(&raw)}).into())
}

async fn query_freebusy(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let time_min = args.req_str("time_min")?;
    let time_max = args.req_str("time_max")?;
    if parse_datetime(&time_max, "time_max")? <= parse_datetime(&time_min, "time_min")? {
        return Err(err("time_max must be after time_min"));
    }
    let time_zone = args.opt_str("time_zone")?;
    validate_timezone(time_zone.as_deref())?;
    let ids = args
        .opt_str_list("calendar_ids")?
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec!["primary".into()]);
    if ids.len() > 50 {
        return Err(err("query_freebusy accepts at most 50 calendar_ids"));
    }
    let mut body = json!({
        "timeMin": time_min,
        "timeMax": time_max,
        "items": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()
    });
    if let Some(zone) = time_zone {
        body["timeZone"] = json!(zone);
    }
    let resp = api.calendar_freebusy(&body).await?;
    let mut calendars = Map::new();
    if let Some(values) = resp.get("calendars").and_then(Value::as_object) {
        for (id, value) in values {
            calendars.insert(
                id.clone(),
                json!({
                    "busy": value.get("busy").cloned().unwrap_or_else(|| json!([])),
                    "errors": value.get("errors").cloned().unwrap_or_else(|| json!([])),
                }),
            );
        }
    }
    Ok(json!({
        "time_min": resp.get("timeMin").cloned().unwrap_or_else(|| json!(time_min)),
        "time_max": resp.get("timeMax").cloned().unwrap_or_else(|| json!(time_max)),
        "calendars": calendars,
        "groups": resp.get("groups").cloned().unwrap_or_else(|| json!({})),
    })
    .into())
}

async fn create_event(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let calendar_id = args.str_or("calendar_id", "primary")?;
    let updates = send_updates(&args.str_or("send_updates", "none")?)?.to_string();
    let (body, conference_version) = event_body(args, true)?;
    if body.get("recurrence").and_then(Value::as_array).is_some_and(|values| !values.is_empty())
        && body.get("start").and_then(|value| value.get("dateTime")).is_some()
        && body.get("start").and_then(|value| value.get("timeZone")).is_none()
    {
        return Err(err("recurring timed events require time_zone for recurrence expansion"));
    }
    let attendee_count = body.get("attendees").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let impact = json!({
        "calendar_id": calendar_id,
        "event": event(&body),
        "attendee_count": attendee_count,
        "send_updates": updates,
        "creates_google_meet": args.bool_or("add_google_meet", false)?,
    });
    if args.bool_or("dry_run", false)? {
        return Ok(dry_run_response("create_event", impact).into());
    }
    if !args.bool_or("confirm", false)? {
        return Ok(preview_response("create_event", impact).into());
    }
    let raw = api.calendar_events_insert(&calendar_id, &body, &updates, conference_version).await?;
    Ok(json!({"calendar_id": calendar_id, "created": true, "event": event(&raw)}).into())
}

async fn update_event(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let event_id = args.req_str("event_id")?;
    let calendar_id = args.str_or("calendar_id", "primary")?;
    let updates = send_updates(&args.str_or("send_updates", "none")?)?.to_string();
    let (patch, conference_version) = event_body(args, false)?;
    if patch.as_object().is_none_or(Map::is_empty) {
        return Err(err("nothing to update: pass at least one event field"));
    }
    let before = api.calendar_events_get(&calendar_id, &event_id).await?;
    if patch.get("recurrence").and_then(Value::as_array).is_some_and(|values| !values.is_empty()) {
        let recurrence_start = patch.get("start").or_else(|| before.get("start"));
        if recurrence_start.and_then(|value| value.get("dateTime")).is_some()
            && recurrence_start.and_then(|value| value.get("timeZone")).is_none()
        {
            return Err(err(
                "recurring timed events require start/end with time_zone for recurrence expansion",
            ));
        }
    }
    let version =
        checked_etag(args, &before, args.bool_or("confirm", false)? && !args.bool_or("dry_run", false)?)?;
    let mut after = before.clone();
    if let (Some(base), Some(changes)) = (after.as_object_mut(), patch.as_object()) {
        base.extend(changes.clone());
    }
    let mut changed_fields: Vec<&str> = patch.as_object().unwrap().keys().map(String::as_str).collect();
    changed_fields.sort_unstable();
    let impact = json!({
        "calendar_id": calendar_id,
        "event_id": event_id,
        "before": event(&before),
        "after": event(&after),
        "changed_fields": changed_fields,
        "send_updates": updates,
    });
    if args.bool_or("dry_run", false)? {
        return Ok(dry_run_response("update_event", impact).into());
    }
    if !args.bool_or("confirm", false)? {
        return Ok(preview_response_with(
            "update_event",
            impact,
            Some("Re-call with confirm=true and the previewed event etag."),
        )
        .into());
    }
    let raw = api
        .calendar_events_patch(&calendar_id, &event_id, &patch, &updates, conference_version, Some(&version))
        .await?;
    Ok(json!({"calendar_id": calendar_id, "updated": true, "event": event(&raw)}).into())
}

async fn delete_event(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let event_id = args.req_str("event_id")?;
    let calendar_id = args.str_or("calendar_id", "primary")?;
    let updates = send_updates(&args.str_or("send_updates", "none")?)?.to_string();
    let raw = api.calendar_events_get(&calendar_id, &event_id).await?;
    let version =
        checked_etag(args, &raw, args.bool_or("confirm", false)? && !args.bool_or("dry_run", false)?)?;
    let impact = json!({
        "calendar_id": calendar_id,
        "event_id": event_id,
        "event": event(&raw),
        "deletes_series": raw.get("recurrence").and_then(Value::as_array).is_some_and(|v| !v.is_empty()),
        "deletes_instance": raw
            .get("recurringEventId")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        "attendee_count": raw.get("attendees").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        "send_updates": updates,
    });
    if args.bool_or("dry_run", false)? {
        return Ok(dry_run_response("delete_event", impact).into());
    }
    if !args.bool_or("confirm", false)? {
        return Ok(preview_response_with(
            "delete_event",
            impact,
            Some("Re-call with confirm=true and the previewed event etag."),
        )
        .into());
    }
    api.calendar_events_delete(&calendar_id, &event_id, &updates, Some(&version)).await?;
    Ok(json!({"calendar_id": calendar_id, "event_id": event_id, "deleted": true}).into())
}

async fn respond_to_event(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let response = args.req_str("response_status")?;
    if !["accepted", "tentative", "declined"].contains(&response.as_str()) {
        return Err(err("response_status must be accepted, tentative, or declined"));
    }
    let event_id = args.req_str("event_id")?;
    let calendar_id = args.str_or("calendar_id", "primary")?;
    let updates = send_updates(&args.str_or("send_updates", "none")?)?.to_string();
    let raw = api.calendar_events_get(&calendar_id, &event_id).await?;
    let version =
        checked_etag(args, &raw, args.bool_or("confirm", false)? && !args.bool_or("dry_run", false)?)?;
    let attendees = raw.get("attendees").and_then(Value::as_array).cloned().unwrap_or_default();
    let current = attendees.iter().find(|a| a.get("self").and_then(Value::as_bool) == Some(true));
    let Some(current) = current else {
        return Err(err("the signed-in user is not an attendee on this event"));
    };
    let before_status = current.get("responseStatus").cloned().unwrap_or_else(|| json!("needsAction"));
    let response_patch = json!({
        "attendeesOmitted": true,
        "attendees": [{"email": field(current, "email"), "responseStatus": response}],
    });
    let impact = json!({
        "calendar_id": calendar_id,
        "event_id": event_id,
        "summary": field(&raw, "summary"),
        "start": field(&raw, "start"),
        "from_status": before_status,
        "to_status": response,
        "etag": field(&raw, "etag"),
        "send_updates": updates,
    });
    if args.bool_or("dry_run", false)? {
        return Ok(dry_run_response("respond_to_event", impact).into());
    }
    if !args.bool_or("confirm", false)? {
        return Ok(preview_response_with(
            "respond_to_event",
            impact,
            Some("Re-call with confirm=true and the previewed event etag."),
        )
        .into());
    }
    let changed = api
        .calendar_events_patch(&calendar_id, &event_id, &response_patch, &updates, 0, Some(&version))
        .await?;
    Ok(json!({"calendar_id": calendar_id, "responded": true, "event": event(&changed)}).into())
}

pub async fn dispatch(name: &str, api: &dyn GoogleApi, args: &Args) -> Option<Result<ToolOutput>> {
    Some(match name {
        "list_calendars" => list_calendars(api, args).await,
        "list_events" => list_events(api, args).await,
        "get_event" => get_event(api, args).await,
        "query_freebusy" => query_freebusy(api, args).await,
        "create_event" => create_event(api, args).await,
        "update_event" => update_event(api, args).await,
        "delete_event" => delete_event(api, args).await,
        "respond_to_event" => respond_to_event(api, args).await,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::FakeApi;

    fn args_for(tool: &str, value: Value) -> Args {
        let def = defs().into_iter().find(|d| d.name == tool).unwrap();
        let Value::Object(map) = value else { panic!("tool arguments must be an object") };
        Args::new(tool, Some(map), &def.params()).unwrap()
    }

    async fn run(api: &FakeApi, tool: &str, value: Value) -> Result<Value> {
        let args = args_for(tool, value);
        match dispatch(tool, api, &args).await {
            Some(Ok(ToolOutput::Json(out))) => Ok(out),
            Some(Ok(_)) => panic!("{tool} returned images"),
            Some(Err(e)) => Err(e),
            None => panic!("{tool} was not dispatched"),
        }
    }

    fn fixture_event() -> Value {
        json!({
            "id": "evt-1",
            "status": "confirmed",
            "htmlLink": "https://calendar.google.com/event?eid=x",
            "summary": "Planning",
            "description": "Agenda",
            "location": "Room 1",
            "start": {"dateTime": "2026-09-09T10:00:00-07:00"},
            "end": {"dateTime": "2026-09-09T11:00:00-07:00"},
            "attendees": [{"email": "me@example.com", "self": true, "responseStatus": "needsAction"}],
            "recurrence": ["RRULE:FREQ=WEEKLY"],
            "hangoutLink": "https://meet.google.com/abc-defg-hij",
            "transparency": "transparent",
            "etag": "etag-1"
        })
    }

    #[tokio::test]
    async fn list_calendars_clamps_pages_and_returns_the_agent_facing_shape() {
        let api = FakeApi::new();
        api.on(
            "calendar_list",
            json!({
                "items": [{
                    "id": "primary@example.com", "summary": "Original", "summaryOverride": "Mine",
                    "timeZone": "America/Los_Angeles", "accessRole": "owner", "primary": true
                }, {"id": "other", "summary": "Original", "summaryOverride": ""}],
                "nextPageToken": "next"
            }),
        );
        let out = run(
            &api,
            "list_calendars",
            json!({"page_size": 999, "page_token": "page", "min_access_role": "reader", "show_hidden": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["calendars"][0]["summary"], "Mine");
        assert_eq!(out["calendars"][1]["summary"], "Original");
        assert_eq!(out["calendars"][0]["selected"], false);
        assert_eq!(out["has_more"], true);
        assert_eq!(
            api.last("calendar_list"),
            json!({"max_results": 250, "page_token": "page", "min_access_role": "reader", "show_hidden": true})
        );
    }

    #[tokio::test]
    async fn list_events_expands_recurrence_forwards_filters_and_normalizes() {
        let api = FakeApi::new();
        api.on(
            "calendar_events_list",
            json!({"timeZone": "UTC", "items": [fixture_event()], "nextPageToken": "n"}),
        );
        let out = run(
            &api,
            "list_events",
            json!({
                "calendar_id": "team@example.com",
                "time_min": "2026-09-01T00:00:00Z",
                "time_max": "2026-10-01T00:00:00Z",
                "query": "Planning", "page_size": 9999, "page_token": "p", "include_cancelled": true
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["events"][0]["conference_url"], "https://meet.google.com/abc-defg-hij");
        assert_eq!(out["events"][0]["availability"], "free");
        assert_eq!(out["has_more"], true);
        assert_eq!(api.last("calendar_events_list")["max_results"], 2500);
        assert_eq!(api.last("calendar_events_list")["show_deleted"], true);
    }

    #[tokio::test]
    async fn list_events_rejects_naive_and_reversed_windows_before_the_api() {
        let api = FakeApi::new();
        let naive = run(
            &api,
            "list_events",
            json!({"time_min": "2026-09-01T00:00:00", "time_max": "2026-09-02T00:00:00Z"}),
        )
        .await
        .unwrap_err();
        assert!(naive.to_string().contains("UTC offset"));
        let reversed = run(
            &api,
            "list_events",
            json!({"time_min": "2026-09-02T00:00:00Z", "time_max": "2026-09-01T00:00:00Z"}),
        )
        .await
        .unwrap_err();
        assert!(reversed.to_string().contains("after"));
        assert_eq!(api.call_count("calendar_events_list"), 0);
    }

    #[tokio::test]
    async fn list_events_defaults_to_a_bounded_thirty_day_window() {
        let api = FakeApi::new();
        api.on("calendar_events_list", json!({"items": []}));
        let before = Utc::now();
        let out = run(&api, "list_events", json!({})).await.unwrap();
        let after = Utc::now();
        let time_min = parse_datetime(out["time_min"].as_str().unwrap(), "time_min").unwrap();
        let time_max = parse_datetime(out["time_max"].as_str().unwrap(), "time_max").unwrap();
        assert!(time_min >= before.fixed_offset() && time_min <= after.fixed_offset());
        assert_eq!(time_max - time_min, chrono::Duration::days(30));
        let request = api.last("calendar_events_list");
        assert_eq!(request["calendar_id"], "primary");
        assert_eq!(request["max_results"], 25);
    }

    #[tokio::test]
    async fn get_event_uses_the_same_shape_as_list() {
        let api = FakeApi::new();
        let mut fixture = fixture_event();
        fixture["hangoutLink"] = json!("");
        fixture["conferenceData"] =
            json!({"entryPoints": [{"entryPointType": "video", "uri": "https://meet.google.com/fallback"}]});
        api.on("calendar_events_get", fixture);
        let out = run(&api, "get_event", json!({"event_id": "evt-1", "calendar_id": "team"})).await.unwrap();
        assert_eq!(out["event"]["id"], "evt-1");
        assert_eq!(out["event"]["recurrence"], json!(["RRULE:FREQ=WEEKLY"]));
        assert_eq!(out["event"]["conference_url"], "https://meet.google.com/fallback");
        assert_eq!(api.last("calendar_events_get"), json!({"calendar_id": "team", "event_id": "evt-1"}));
    }

    #[tokio::test]
    async fn freebusy_defaults_to_primary_and_preserves_per_calendar_errors() {
        let api = FakeApi::new();
        api.on(
            "calendar_freebusy",
            json!({
                "timeMin": "2026-09-09T00:00:00Z", "timeMax": "2026-09-10T00:00:00Z",
                "calendars": {
                    "primary": {"busy": [{"start": "a", "end": "b"}]},
                    "missing": {"errors": [{"reason": "notFound"}]}
                }
            }),
        );
        let out = run(
            &api,
            "query_freebusy",
            json!({"time_min": "2026-09-09T00:00:00Z", "time_max": "2026-09-10T00:00:00Z"}),
        )
        .await
        .unwrap();
        assert_eq!(out["calendars"]["primary"]["busy"][0]["start"], "a");
        assert_eq!(out["calendars"]["missing"]["errors"][0]["reason"], "notFound");
        assert_eq!(api.last("calendar_freebusy")["body"]["items"], json!([{"id": "primary"}]));

        api.on("calendar_freebusy", json!({"calendars": {}}));
        run(
            &api,
            "query_freebusy",
            json!({
                "time_min": "2026-09-09T00:00:00Z", "time_max": "2026-09-10T00:00:00Z",
                "calendar_ids": []
            }),
        )
        .await
        .unwrap();
        assert_eq!(api.last("calendar_freebusy")["body"]["items"], json!([{"id": "primary"}]));
    }

    #[tokio::test]
    async fn freebusy_enforces_the_api_calendar_limit_before_the_call() {
        let api = FakeApi::new();
        let ids: Vec<String> = (0..51).map(|i| format!("c{i}")).collect();
        let e = run(
            &api,
            "query_freebusy",
            json!({
                "time_min": "2026-09-09T00:00:00Z", "time_max": "2026-09-10T00:00:00Z",
                "calendar_ids": ids
            }),
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("at most 50"));
        assert_eq!(api.call_count("calendar_freebusy"), 0);
    }

    #[tokio::test]
    async fn create_previews_and_dry_runs_without_writing() {
        let api = FakeApi::new();
        let base = json!({
            "summary": "Planning", "start": "2026-09-09T10:00:00-07:00",
            "end": "2026-09-09T11:00:00-07:00"
        });
        let preview = run(&api, "create_event", base.clone()).await.unwrap();
        assert_eq!(preview["status"], "confirmation_required");
        assert_eq!(preview["impact"]["send_updates"], "none");
        let mut dry = base.as_object().unwrap().clone();
        dry.insert("dry_run".into(), json!(true));
        dry.insert("confirm".into(), json!(true));
        let out = run(&api, "create_event", Value::Object(dry)).await.unwrap();
        assert_eq!(out["dry_run"], true);
        assert_eq!(api.call_count("calendar_events_insert"), 0);
    }

    #[tokio::test]
    async fn confirmed_create_sends_attendees_reminders_recurrence_and_meet() {
        let api = FakeApi::new();
        api.on("calendar_events_insert", fixture_event());
        let out = run(
            &api,
            "create_event",
            json!({
                "summary": "Planning", "start": "2026-09-09T10:00:00-07:00",
                "end": "2026-09-09T11:00:00-07:00", "calendar_id": "team@example.com",
                "event_id": "abcde12345",
                "time_zone": "America/Los_Angeles", "attendees": ["guest@example.com"],
                "recurrence": ["RRULE:FREQ=WEEKLY;COUNT=2"],
                "reminders": [{"method": "popup", "minutes": 10}], "send_updates": "all",
                "add_google_meet": true, "availability": "free", "visibility": "private", "confirm": true
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["created"], true);
        let call = api.last("calendar_events_insert");
        assert_eq!(call["calendar_id"], "team@example.com");
        assert_eq!(call["send_updates"], "all");
        assert_eq!(call["conference_data_version"], 1);
        assert_eq!(call["body"]["attendees"], json!([{"email": "guest@example.com"}]));
        assert_eq!(call["body"]["id"], "abcde12345");
        assert_eq!(call["body"]["reminders"]["overrides"][0]["minutes"], 10);
        assert_eq!(call["body"]["transparency"], "transparent");
    }

    #[tokio::test]
    async fn all_day_end_is_exclusive_and_dates_never_carry_a_timezone() {
        let api = FakeApi::new();
        let out = run(
            &api,
            "create_event",
            json!({
                "summary": "Away", "start": "2026-09-09", "end": "2026-09-10",
                "all_day": true, "recurrence": ["EXRULE:FREQ=YEARLY;COUNT=1"], "dry_run": true
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["event"]["start"], json!({"date": "2026-09-09"}));
        assert_eq!(out["event"]["end"], json!({"date": "2026-09-10"}));
        let e = run(
            &api,
            "create_event",
            json!({"summary": "Away", "start": "2026-09-09", "end": "2026-09-09", "all_day": true}),
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("exclusive"));
    }

    #[tokio::test]
    async fn high_risk_create_inputs_are_validated_before_preview() {
        let api = FakeApi::new();
        let base = |extra: Value| {
            let mut map = json!({
                "summary": "Planning", "start": "2026-09-09T10:00:00-07:00",
                "end": "2026-09-09T11:00:00-07:00"
            })
            .as_object()
            .unwrap()
            .clone();
            map.extend(extra.as_object().unwrap().clone());
            Value::Object(map)
        };
        for (extra, message) in [
            (json!({"send_updates": "sometimes"}), "send_updates"),
            (json!({"attendees": ["not-an-email"]}), "email address"),
            (json!({"recurrence": ["WEEKLY"]}), "RRULE"),
            (json!({"recurrence": ["RRULE:FREQ=WEEKLY"]}), "require time_zone"),
            (json!({"reminders": [{"method": "sms", "minutes": 1}]}), "reminder"),
            (json!({"reminders": [{"method": "popup", "minutes": true}]}), "reminder"),
            (json!({"reminders": vec![json!({"method": "popup", "minutes": 1}); 6]}), "at most 5"),
            (json!({"visibility": "secret"}), "visibility"),
            (json!({"availability": "maybe"}), "availability"),
            (json!({"time_zone": "Not/AZone"}), "time zone"),
            (json!({"event_id": "BAD-ID"}), "base32hex"),
        ] {
            let e = run(&api, "create_event", base(extra)).await.unwrap_err();
            assert!(e.to_string().contains(message), "{e}");
        }
        assert_eq!(api.call_count("calendar_events_insert"), 0);
    }

    #[tokio::test]
    async fn update_previews_clears_and_confirmed_calls_patch_with_only_supplied_fields() {
        let api = FakeApi::new();
        api.on("calendar_events_get", fixture_event());
        let preview = run(
            &api,
            "update_event",
            json!({"event_id": "evt-1", "attendees": [], "recurrence": [], "summary": "New"}),
        )
        .await
        .unwrap();
        assert_eq!(preview["impact"]["changed_fields"], json!(["attendees", "recurrence", "summary"]));
        assert_eq!(preview["impact"]["after"]["attendees"], json!([]));
        assert_eq!(api.call_count("calendar_events_patch"), 0);

        api.on("calendar_events_get", fixture_event());
        api.on("calendar_events_patch", json!({"id": "evt-1", "summary": "New", "location": "Room 2"}));
        let changed = run(
            &api,
            "update_event",
            json!({
                "event_id": "evt-1", "summary": "New", "location": "Room 2",
                "etag": "etag-1", "send_updates": "externalOnly", "confirm": true
            }),
        )
        .await
        .unwrap();
        assert_eq!(changed["updated"], true);
        let call = api.last("calendar_events_patch");
        assert_eq!(call["body"], json!({"summary": "New", "location": "Room 2"}));
        assert_eq!(call["send_updates"], "externalOnly");
        assert_eq!(call["etag"], "etag-1");
    }

    #[tokio::test]
    async fn update_needs_a_change_and_paired_times_before_reading() {
        let api = FakeApi::new();
        let empty = run(&api, "update_event", json!({"event_id": "evt-1"})).await.unwrap_err();
        assert!(empty.to_string().contains("nothing to update"));
        let half = run(&api, "update_event", json!({"event_id": "evt-1", "start": "2026-09-09T12:00:00Z"}))
            .await
            .unwrap_err();
        assert!(half.to_string().contains("supplied together"));
        assert_eq!(api.call_count("calendar_events_get"), 0);
    }

    #[tokio::test]
    async fn update_requires_a_timezone_when_turning_a_timed_event_into_a_series() {
        let api = FakeApi::new();
        let mut fixture = fixture_event();
        fixture["recurrence"] = json!([]);
        api.on("calendar_events_get", fixture);
        let error =
            run(&api, "update_event", json!({"event_id": "evt-1", "recurrence": ["RRULE:FREQ=WEEKLY"]}))
                .await
                .unwrap_err();
        assert!(error.to_string().contains("start/end with time_zone"));
        assert_eq!(api.call_count("calendar_events_patch"), 0);
    }

    #[tokio::test]
    async fn confirmed_calendar_writes_require_the_preview_etag_and_reject_a_stale_one() {
        let api = FakeApi::new();
        api.on("calendar_events_get", fixture_event());
        let missing =
            run(&api, "update_event", json!({"event_id": "evt-1", "summary": "New", "confirm": true}))
                .await
                .unwrap_err();
        assert!(missing.to_string().contains("requires the etag"));

        api.on("calendar_events_get", fixture_event());
        let stale = run(
            &api,
            "update_event",
            json!({"event_id": "evt-1", "etag": "stale", "summary": "New", "confirm": true}),
        )
        .await
        .unwrap_err();
        assert!(stale.to_string().contains("changed after preview"));
        assert_eq!(api.call_count("calendar_events_patch"), 0);
    }

    #[tokio::test]
    async fn delete_identifies_a_series_and_calls_delete_only_after_confirmation() {
        let api = FakeApi::new();
        api.on("calendar_events_get", fixture_event());
        let preview = run(&api, "delete_event", json!({"event_id": "evt-1"})).await.unwrap();
        assert_eq!(preview["impact"]["deletes_series"], true);
        assert_eq!(api.call_count("calendar_events_delete"), 0);

        api.on("calendar_events_get", fixture_event());
        let out = run(
            &api,
            "delete_event",
            json!({"event_id": "evt-1", "etag": "etag-1", "send_updates": "all", "confirm": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["deleted"], true);
        assert_eq!(api.last("calendar_events_delete")["send_updates"], "all");
        assert_eq!(api.last("calendar_events_delete")["etag"], "etag-1");
    }

    #[tokio::test]
    async fn responding_uses_attendees_omitted_to_change_only_self() {
        let api = FakeApi::new();
        let invited = json!({
            "id": "evt-1", "summary": "Planning", "start": {"dateTime": "2026-09-09T10:00:00Z"},
            "etag": "etag-rsvp",
            "attendees": [
                {"email": "me@example.com", "self": true, "responseStatus": "needsAction"},
                {"email": "other@example.com", "responseStatus": "accepted", "optional": true}
            ]
        });
        api.on("calendar_events_get", invited.clone());
        let preview =
            run(&api, "respond_to_event", json!({"event_id": "evt-1", "response_status": "tentative"}))
                .await
                .unwrap();
        assert_eq!(preview["impact"]["from_status"], "needsAction");
        assert_eq!(api.call_count("calendar_events_patch"), 0);

        api.on("calendar_events_get", invited);
        api.on("calendar_events_patch", json!({"id": "evt-1"}));
        run(
            &api,
            "respond_to_event",
            json!({"event_id": "evt-1", "response_status": "declined", "etag": "etag-rsvp", "confirm": true}),
        )
        .await
        .unwrap();
        assert_eq!(
            api.last("calendar_events_patch")["body"],
            json!({
                "attendeesOmitted": true,
                "attendees": [{"email": "me@example.com", "responseStatus": "declined"}]
            })
        );
        assert_eq!(api.last("calendar_events_patch")["etag"], "etag-rsvp");
    }

    #[tokio::test]
    async fn responding_requires_the_signed_in_attendee() {
        let api = FakeApi::new();
        api.on(
            "calendar_events_get",
            json!({"etag": "etag-1", "attendees": [{"email": "other@example.com"}]}),
        );
        let e = run(&api, "respond_to_event", json!({"event_id": "evt-1", "response_status": "accepted"}))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not an attendee"));
        assert_eq!(api.call_count("calendar_events_patch"), 0);
    }

    #[test]
    fn schemas_are_declared_in_python_registration_order() {
        assert_eq!(
            defs().iter().map(|d| d.name).collect::<Vec<_>>(),
            vec![
                "list_calendars",
                "list_events",
                "get_event",
                "query_freebusy",
                "create_event",
                "update_event",
                "delete_event",
                "respond_to_event"
            ]
        );
    }

    #[tokio::test]
    async fn dispatch_leaves_other_modules_tools_alone() {
        let api = FakeApi::new();
        let args = Args::new("other", None, &[]).unwrap();
        assert!(dispatch("read_document", &api, &args).await.is_none());
    }
}
