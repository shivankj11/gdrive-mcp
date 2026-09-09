"""Google Calendar read and write tools."""

from __future__ import annotations

from datetime import date, datetime, timedelta, timezone
import re
from uuid import uuid4
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from gdrive_mcp.clients import calendar
from gdrive_mcp.errors import api_errors
from gdrive_mcp.guard import preview_response

_RESPONSES = {"needsAction", "declined", "tentative", "accepted"}
_SEND_UPDATES = {"none", "externalOnly", "all"}
_VISIBILITY = {"default", "public", "private", "confidential"}
_AVAILABILITY = {"busy": "opaque", "free": "transparent"}
_EVENT_ID = re.compile(r"[0-9a-v]{5,1024}")


def _timezone(value: str | None) -> None:
    if value is None:
        return
    try:
        ZoneInfo(value)
    except ZoneInfoNotFoundError as exc:
        raise RuntimeError(f"unknown IANA time zone: {value!r}") from exc


def _datetime(value: str, label: str) -> datetime:
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise RuntimeError(f"{label} must be an RFC3339 timestamp with a UTC offset") from exc
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise RuntimeError(f"{label} must include a UTC offset (for example Z or -07:00)")
    return parsed


def _date(value: str, label: str) -> date:
    try:
        parsed = date.fromisoformat(value)
    except ValueError as exc:
        raise RuntimeError(f"{label} must be an ISO date (YYYY-MM-DD) for an all-day event") from exc
    if "T" in value or len(value) != 10:
        raise RuntimeError(f"{label} must be an ISO date (YYYY-MM-DD) for an all-day event")
    return parsed


def _time_pair(start: str, end: str, all_day: bool, time_zone: str | None) -> tuple[dict, dict]:
    _timezone(time_zone)
    if all_day:
        s, e = _date(start, "start"), _date(end, "end")
        if e <= s:
            raise RuntimeError("end must be after start; an all-day end date is exclusive")
        return {"date": start}, {"date": end}
    s, e = _datetime(start, "start"), _datetime(end, "end")
    if e <= s:
        raise RuntimeError("end must be after start")
    start_value: dict = {"dateTime": start}
    end_value: dict = {"dateTime": end}
    if time_zone:
        start_value["timeZone"] = time_zone
        end_value["timeZone"] = time_zone
    return start_value, end_value


def _send_updates(value: str) -> str:
    if value not in _SEND_UPDATES:
        raise RuntimeError("send_updates must be one of: none, externalOnly, all")
    return value


def _checked_etag(raw: dict, expected: str | None, *, require: bool) -> str:
    current = raw.get("etag")
    if not current:
        raise RuntimeError("Google did not return an ETag for this event; refusing an unversioned write")
    if expected is not None and expected != current:
        raise RuntimeError("the event changed after preview; read it again and approve the new preview")
    if require and expected is None:
        raise RuntimeError("confirm=true requires the etag returned by the preview")
    return expected or current


def _conference_url(event: dict) -> str | None:
    if event.get("hangoutLink"):
        return event["hangoutLink"]
    for point in event.get("conferenceData", {}).get("entryPoints", []):
        if point.get("entryPointType") == "video" and point.get("uri"):
            return point["uri"]
    return None


def _event(event: dict) -> dict:
    """Stable, bounded event representation shared with the Rust implementation."""
    return {
        "id": event.get("id"),
        "status": event.get("status"),
        "html_link": event.get("htmlLink"),
        "summary": event.get("summary"),
        "description": event.get("description"),
        "location": event.get("location"),
        "creator": event.get("creator"),
        "organizer": event.get("organizer"),
        "start": event.get("start"),
        "end": event.get("end"),
        "attendees": event.get("attendees", []),
        "recurrence": event.get("recurrence", []),
        "recurring_event_id": event.get("recurringEventId"),
        "original_start_time": event.get("originalStartTime"),
        "conference_url": _conference_url(event),
        "visibility": event.get("visibility"),
        "availability": "free" if event.get("transparency") == "transparent" else "busy",
        "etag": event.get("etag"),
        "created": event.get("created"),
        "updated": event.get("updated"),
    }


def _event_body(
    *,
    summary: str | None = None,
    start: str | None = None,
    end: str | None = None,
    all_day: bool | None = None,
    time_zone: str | None = None,
    description: str | None = None,
    location: str | None = None,
    attendees: list[str] | None = None,
    recurrence: list[str] | None = None,
    reminders: list[dict] | None = None,
    add_google_meet: bool | None = None,
    availability: str | None = None,
    visibility: str | None = None,
) -> tuple[dict, int]:
    body: dict = {}
    if summary is not None:
        if not summary.strip():
            raise RuntimeError("summary cannot be empty")
        body["summary"] = summary
    if (start is None) != (end is None):
        raise RuntimeError("start and end must be supplied together")
    if start is not None and end is not None:
        s, e = _time_pair(start, end, bool(all_day), time_zone)
        body.update(start=s, end=e)
    elif all_day is not None or time_zone is not None:
        raise RuntimeError("all_day/time_zone can only be changed together with start and end")
    if description is not None:
        body["description"] = description
    if location is not None:
        body["location"] = location
    if attendees is not None:
        bad = [email for email in attendees if not isinstance(email, str) or "@" not in email]
        if bad:
            raise RuntimeError("each attendee must be an email address")
        body["attendees"] = [{"email": email} for email in attendees]
    if recurrence is not None:
        if any(not rule.startswith(("RRULE:", "EXRULE:", "RDATE:", "EXDATE:")) for rule in recurrence):
            raise RuntimeError("recurrence entries must start with RRULE:, EXRULE:, RDATE:, or EXDATE:")
        body["recurrence"] = recurrence
    if reminders is not None:
        if len(reminders) > 5:
            raise RuntimeError("Google Calendar accepts at most 5 reminder overrides")
        overrides = []
        for reminder in reminders:
            method, minutes = reminder.get("method"), reminder.get("minutes")
            if (
                method not in {"email", "popup"}
                or not isinstance(minutes, int)
                or isinstance(minutes, bool)
                or not 0 <= minutes <= 40320
            ):
                raise RuntimeError(
                    "each reminder needs method=email|popup and integer minutes between 0 and 40320"
                )
            overrides.append({"method": method, "minutes": minutes})
        body["reminders"] = {"useDefault": False, "overrides": overrides}
    conference_version = 0
    if add_google_meet:
        body["conferenceData"] = {
            "createRequest": {
                "requestId": str(uuid4()),
                "conferenceSolutionKey": {"type": "hangoutsMeet"},
            }
        }
        conference_version = 1
    if availability is not None:
        if availability not in _AVAILABILITY:
            raise RuntimeError("availability must be busy or free")
        body["transparency"] = _AVAILABILITY[availability]
    if visibility is not None:
        if visibility not in _VISIBILITY:
            raise RuntimeError("visibility must be one of: default, public, private, confidential")
        body["visibility"] = visibility
    return body, conference_version


@api_errors
def list_calendars(
    page_size: int = 100,
    page_token: str | None = None,
    min_access_role: str | None = None,
    show_hidden: bool = False,
) -> dict:
    """List calendars available to the signed-in user, one bounded page at a time.

    `min_access_role` may be freeBusyReader, reader, writer, or owner. When `has_more` is true,
    pass `next_page_token` to continue. Calendar names and IDs may contain sensitive data.
    """
    if min_access_role not in {None, "freeBusyReader", "reader", "writer", "owner"}:
        raise RuntimeError("min_access_role must be one of: freeBusyReader, reader, writer, owner")
    resp = (
        calendar()
        .calendarList()
        .list(
            maxResults=max(1, min(page_size, 250)),
            pageToken=page_token,
            minAccessRole=min_access_role,
            showHidden=show_hidden,
        )
        .execute()
    )
    calendars = [
        {
            "id": c.get("id"),
            "summary": c.get("summaryOverride") or c.get("summary"),
            "description": c.get("description"),
            "location": c.get("location"),
            "time_zone": c.get("timeZone"),
            "access_role": c.get("accessRole"),
            "primary": c.get("primary", False),
            "selected": c.get("selected", False),
            "hidden": c.get("hidden", False),
        }
        for c in resp.get("items", [])
    ]
    token = resp.get("nextPageToken")
    return {"count": len(calendars), "calendars": calendars, "has_more": bool(token), "next_page_token": token}


@api_errors
def list_events(
    calendar_id: str = "primary",
    time_min: str | None = None,
    time_max: str | None = None,
    query: str | None = None,
    page_size: int = 25,
    page_token: str | None = None,
    include_cancelled: bool = False,
) -> dict:
    """List expanded event instances in a bounded time window, ordered by start time.

    `calendar_id` defaults to the primary calendar. Times must be RFC3339 with an offset. With
    neither bound, the window is now through 30 days from now; pass both for deterministic reads.
    Free-text `query` searches event details and attendees. Use pagination when `has_more` is true.
    """
    now = datetime.now(timezone.utc)
    if time_min is None:
        time_min = now.isoformat().replace("+00:00", "Z")
    if time_max is None:
        time_max = (now + timedelta(days=30)).isoformat().replace("+00:00", "Z")
    if _datetime(time_max, "time_max") <= _datetime(time_min, "time_min"):
        raise RuntimeError("time_max must be after time_min")
    resp = (
        calendar()
        .events()
        .list(
            calendarId=calendar_id,
            timeMin=time_min,
            timeMax=time_max,
            q=query,
            maxResults=max(1, min(page_size, 2500)),
            pageToken=page_token,
            showDeleted=include_cancelled,
            singleEvents=True,
            orderBy="startTime",
        )
        .execute()
    )
    events = [_event(event) for event in resp.get("items", [])]
    token = resp.get("nextPageToken")
    return {
        "calendar_id": calendar_id,
        "time_min": time_min,
        "time_max": time_max,
        "time_zone": resp.get("timeZone"),
        "count": len(events),
        "events": events,
        "has_more": bool(token),
        "next_page_token": token,
    }


@api_errors
def get_event(event_id: str, calendar_id: str = "primary") -> dict:
    """Get one event by its API event ID and calendar ID (default: primary calendar)."""
    raw = calendar().events().get(calendarId=calendar_id, eventId=event_id).execute()
    return {"calendar_id": calendar_id, "event": _event(raw)}


@api_errors
def query_freebusy(
    time_min: str,
    time_max: str,
    calendar_ids: list[str] | None = None,
    time_zone: str | None = None,
) -> dict:
    """Return busy intervals for up to 50 calendars in an RFC3339 time window.

    `calendar_ids` defaults to ["primary"]. Results reveal availability only, not event details.
    `time_zone`, when supplied, must be an IANA name such as America/Los_Angeles.
    """
    if _datetime(time_max, "time_max") <= _datetime(time_min, "time_min"):
        raise RuntimeError("time_max must be after time_min")
    _timezone(time_zone)
    ids = calendar_ids or ["primary"]
    if len(ids) > 50:
        raise RuntimeError("query_freebusy accepts at most 50 calendar_ids")
    body: dict = {"timeMin": time_min, "timeMax": time_max, "items": [{"id": cid} for cid in ids]}
    if time_zone:
        body["timeZone"] = time_zone
    resp = calendar().freebusy().query(body=body).execute()
    return {
        "time_min": resp.get("timeMin", time_min),
        "time_max": resp.get("timeMax", time_max),
        "calendars": {
            cid: {"busy": value.get("busy", []), "errors": value.get("errors", [])}
            for cid, value in resp.get("calendars", {}).items()
        },
        "groups": resp.get("groups", {}),
    }


@api_errors
def create_event(
    summary: str,
    start: str,
    end: str,
    calendar_id: str = "primary",
    event_id: str | None = None,
    all_day: bool = False,
    time_zone: str | None = None,
    description: str | None = None,
    location: str | None = None,
    attendees: list[str] | None = None,
    recurrence: list[str] | None = None,
    reminders: list[dict] | None = None,
    send_updates: str = "none",
    add_google_meet: bool = False,
    availability: str = "busy",
    visibility: str = "default",
    confirm: bool = False,
    dry_run: bool = False,
) -> dict:
    """Create an event after previewing its time, attendees, and notification impact.

    Timed values require RFC3339 offsets; all-day values are YYYY-MM-DD and `end` is exclusive.
    Optional `event_id` makes retries idempotent and must be 5-1024 lowercase base32hex characters.
    `send_updates` is none, externalOnly, or all and defaults to none. Recurrence entries use
    RRULE:/EXRULE:/RDATE:/EXDATE:; recurring timed events require `time_zone`. dry_run=true returns
    the proposed event. Without confirm=true, nothing is created and a confirmation preview is
    returned.
    """
    updates = _send_updates(send_updates)
    body, conference_version = _event_body(
        summary=summary,
        start=start,
        end=end,
        all_day=all_day,
        time_zone=time_zone,
        description=description,
        location=location,
        attendees=attendees,
        recurrence=recurrence,
        reminders=reminders,
        add_google_meet=add_google_meet,
        availability=availability,
        visibility=visibility,
    )
    if recurrence and not all_day and not time_zone:
        raise RuntimeError("recurring timed events require time_zone for recurrence expansion")
    if event_id is not None:
        if not _EVENT_ID.fullmatch(event_id):
            raise RuntimeError("event_id must be 5-1024 lowercase base32hex characters (0-9, a-v)")
        body["id"] = event_id
    impact = {
        "calendar_id": calendar_id,
        "event": _event(body),
        "attendee_count": len(attendees or []),
        "send_updates": updates,
        "creates_google_meet": add_google_meet,
    }
    if dry_run:
        return {"dry_run": True, "action": "create_event", **impact}
    if not confirm:
        return preview_response("create_event", impact)
    raw = (
        calendar()
        .events()
        .insert(
            calendarId=calendar_id,
            body=body,
            sendUpdates=updates,
            conferenceDataVersion=conference_version,
        )
        .execute()
    )
    return {"calendar_id": calendar_id, "created": True, "event": _event(raw)}


@api_errors
def update_event(
    event_id: str,
    calendar_id: str = "primary",
    etag: str | None = None,
    summary: str | None = None,
    start: str | None = None,
    end: str | None = None,
    all_day: bool | None = None,
    time_zone: str | None = None,
    description: str | None = None,
    location: str | None = None,
    attendees: list[str] | None = None,
    recurrence: list[str] | None = None,
    reminders: list[dict] | None = None,
    send_updates: str = "none",
    add_google_meet: bool | None = None,
    availability: str | None = None,
    visibility: str | None = None,
    confirm: bool = False,
    dry_run: bool = False,
) -> dict:
    """Partially update an event after returning a live before/after preview.

    Supply start and end together when changing time; adding recurrence to a timed event requires
    zoned start/end values. Passing attendees=[] or recurrence=[] clears that list; leaving an
    argument unset preserves it. `send_updates` defaults to none. Preview first, then pass its event
    `etag` with confirm=true; a changed event is refused. dry_run=true previews, and without
    confirm=true nothing is written.
    """
    updates = _send_updates(send_updates)
    patch, conference_version = _event_body(
        summary=summary,
        start=start,
        end=end,
        all_day=all_day,
        time_zone=time_zone,
        description=description,
        location=location,
        attendees=attendees,
        recurrence=recurrence,
        reminders=reminders,
        add_google_meet=add_google_meet,
        availability=availability,
        visibility=visibility,
    )
    if not patch:
        raise RuntimeError("nothing to update: pass at least one event field")
    svc = calendar().events()
    before_raw = svc.get(calendarId=calendar_id, eventId=event_id).execute()
    if patch.get("recurrence"):
        recurrence_start = patch.get("start", before_raw.get("start", {}))
        if recurrence_start.get("dateTime") and not recurrence_start.get("timeZone"):
            raise RuntimeError(
                "recurring timed events require start/end with time_zone for recurrence expansion"
            )
    version = _checked_etag(before_raw, etag, require=confirm and not dry_run)
    after_raw = {**before_raw, **patch}
    impact = {
        "calendar_id": calendar_id,
        "event_id": event_id,
        "before": _event(before_raw),
        "after": _event(after_raw),
        "changed_fields": sorted(patch),
        "send_updates": updates,
    }
    if dry_run:
        return {"dry_run": True, "action": "update_event", **impact}
    if not confirm:
        return preview_response(
            "update_event", impact, message="Re-call with confirm=true and the previewed event etag."
        )
    request = svc.patch(
        calendarId=calendar_id,
        eventId=event_id,
        body=patch,
        sendUpdates=updates,
        conferenceDataVersion=conference_version,
    )
    request.headers["If-Match"] = version
    raw = request.execute()
    return {"calendar_id": calendar_id, "updated": True, "event": _event(raw)}


@api_errors
def delete_event(
    event_id: str,
    calendar_id: str = "primary",
    etag: str | None = None,
    send_updates: str = "none",
    confirm: bool = False,
    dry_run: bool = False,
) -> dict:
    """Delete one event or recurring instance after a live impact preview.

    Pass a recurring master event ID to delete the series, or an expanded instance ID to delete
    one occurrence. `send_updates` defaults to none. Preview first, then pass its event `etag` with
    confirm=true; a changed event is refused. dry_run=true previews; without confirm=true nothing
    is deleted.
    """
    updates = _send_updates(send_updates)
    svc = calendar().events()
    raw = svc.get(calendarId=calendar_id, eventId=event_id).execute()
    version = _checked_etag(raw, etag, require=confirm and not dry_run)
    impact = {
        "calendar_id": calendar_id,
        "event_id": event_id,
        "event": _event(raw),
        "deletes_series": bool(raw.get("recurrence")),
        "deletes_instance": bool(raw.get("recurringEventId")),
        "attendee_count": len(raw.get("attendees", [])),
        "send_updates": updates,
    }
    if dry_run:
        return {"dry_run": True, "action": "delete_event", **impact}
    if not confirm:
        return preview_response(
            "delete_event", impact, message="Re-call with confirm=true and the previewed event etag."
        )
    request = svc.delete(calendarId=calendar_id, eventId=event_id, sendUpdates=updates)
    request.headers["If-Match"] = version
    request.execute()
    return {"calendar_id": calendar_id, "event_id": event_id, "deleted": True}


@api_errors
def respond_to_event(
    event_id: str,
    response_status: str,
    calendar_id: str = "primary",
    etag: str | None = None,
    send_updates: str = "none",
    confirm: bool = False,
    dry_run: bool = False,
) -> dict:
    """Accept, tentatively accept, or decline an invitation after a live preview.

    `response_status` is accepted, tentative, or declined. The signed-in attendee must be present
    on the event. `send_updates` defaults to none. Preview first, then pass its event `etag` with
    confirm=true; a changed invitation is refused. dry_run=true previews; without confirm=true the
    response is not changed.
    """
    if response_status not in _RESPONSES - {"needsAction"}:
        raise RuntimeError("response_status must be accepted, tentative, or declined")
    updates = _send_updates(send_updates)
    svc = calendar().events()
    raw = svc.get(calendarId=calendar_id, eventId=event_id).execute()
    version = _checked_etag(raw, etag, require=confirm and not dry_run)
    attendees = raw.get("attendees", [])
    current = next((a for a in attendees if a.get("self")), None)
    if current is None:
        raise RuntimeError("the signed-in user is not an attendee on this event")
    before_status = current.get("responseStatus", "needsAction")
    response_patch = {
        "attendeesOmitted": True,
        "attendees": [{"email": current.get("email"), "responseStatus": response_status}],
    }
    impact = {
        "calendar_id": calendar_id,
        "event_id": event_id,
        "summary": raw.get("summary"),
        "start": raw.get("start"),
        "from_status": before_status,
        "to_status": response_status,
        "etag": raw.get("etag"),
        "send_updates": updates,
    }
    if dry_run:
        return {"dry_run": True, "action": "respond_to_event", **impact}
    if not confirm:
        return preview_response(
            "respond_to_event", impact, message="Re-call with confirm=true and the previewed event etag."
        )
    request = svc.patch(
        calendarId=calendar_id,
        eventId=event_id,
        body=response_patch,
        sendUpdates=updates,
    )
    request.headers["If-Match"] = version
    changed = request.execute()
    return {"calendar_id": calendar_id, "responded": True, "event": _event(changed)}


_TOOLS = (
    list_calendars,
    list_events,
    get_event,
    query_freebusy,
    create_event,
    update_event,
    delete_event,
    respond_to_event,
)
