"""Live, credentialed checks C1-C12 for the Calendar tools.

Skipped unless GDRIVE_MCP_LIVE=1. The test creates one synthetic recurring event on the
authenticated user's primary calendar with notifications disabled, then deletes it in ``finally``.
The caller-supplied event id is printed first so a failed cleanup is recoverable by hand.

    GDRIVE_MCP_LIVE=1 uv run pytest tests/test_live_calendar.py -v
"""

from datetime import datetime, timedelta, timezone
import os
from uuid import uuid4

import pytest
from googleapiclient.errors import HttpError

from gdrive_mcp.clients import authed_user_email, calendar
from gdrive_mcp.tools import calendar as calendar_mod

live_only = pytest.mark.skipif(
    not os.environ.get("GDRIVE_MCP_LIVE"), reason="live run: set GDRIVE_MCP_LIVE=1"
)


def _rfc3339(value: datetime) -> str:
    return value.isoformat(timespec="seconds").replace("+00:00", "Z")


def _missing(event_id: str) -> bool:
    try:
        event = calendar().events().get(calendarId="primary", eventId=event_id).execute()
    except HttpError as exc:
        return exc.resp.status in (404, 410)
    # Calendar may retain a tombstone that GET returns as a cancelled event.
    return event.get("status") == "cancelled"


@live_only
def test_calendar_tools_hold_against_the_real_calendar_api():
    event_id = uuid4().hex
    summary = f"gdrive-mcp calendar live check {event_id[:8]}"
    start_dt = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0) + timedelta(days=400)
    start = _rfc3339(start_dt)
    end = _rfc3339(start_dt + timedelta(hours=1))
    window_end = _rfc3339(start_dt + timedelta(days=3))
    email = authed_user_email()
    assert email, "C1: could not determine the authenticated user's email"
    print(f"scratch calendar event: primary/{event_id}")

    try:
        # C1: CalendarList is authorized and includes the primary calendar.
        calendars = calendar_mod.list_calendars(page_size=250)
        assert any(item["primary"] for item in calendars["calendars"]), "C1: primary calendar missing"

        create_args = {
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
            "send_updates": "none",
        }

        # C2/C3: both preview modes are non-writing, and the caller id survives the preview.
        preview = calendar_mod.create_event(**create_args)
        assert preview["status"] == "confirmation_required"
        assert preview["impact"]["event"]["id"] == event_id
        assert _missing(event_id), "C2: unconfirmed create wrote an event"
        dry = calendar_mod.create_event(**create_args, dry_run=True)
        assert dry["dry_run"] is True and dry["event"]["id"] == event_id
        assert _missing(event_id), "C3: dry-run create wrote an event"

        # C4: confirmed create round-trips rich event fields without sending mail.
        made = calendar_mod.create_event(**create_args, confirm=True)
        assert made["created"] is True and made["event"]["id"] == event_id
        got = calendar_mod.get_event(event_id)
        event = got["event"]
        assert event["summary"] == summary
        assert event["location"] == "Synthetic test location"
        assert event["recurrence"] == ["RRULE:FREQ=DAILY;COUNT=2"]
        assert event["visibility"] == "private"
        assert event["etag"]

        # C5: list_events expands the series, applies q, orders by start, and paginates.
        first = calendar_mod.list_events(
            time_min=start, time_max=window_end, query=event_id[:8], page_size=1
        )
        assert first["count"] == 1 and first["has_more"] is True
        assert first["events"][0]["recurring_event_id"] == event_id
        second = calendar_mod.list_events(
            time_min=start,
            time_max=window_end,
            query=event_id[:8],
            page_size=1,
            page_token=first["next_page_token"],
        )
        assert second["count"] == 1
        assert second["events"][0]["recurring_event_id"] == event_id

        # C6: free/busy reports the first occurrence as busy.
        freebusy = calendar_mod.query_freebusy(start, end, time_zone="UTC")
        busy = freebusy["calendars"]["primary"]["busy"]
        assert any(slot["start"] <= start and slot["end"] >= end for slot in busy)

        # C7: If-Match rejects a write based on an obsolete event representation.
        svc = calendar().events()
        stale = svc.get(calendarId="primary", eventId=event_id).execute()
        svc.patch(
            calendarId="primary",
            eventId=event_id,
            body={"description": "C7-out-of-band"},
            sendUpdates="none",
        ).execute()
        rejected = svc.patch(
            calendarId="primary",
            eventId=event_id,
            body={"summary": "C7-SHOULD-NOT-LAND"},
            sendUpdates="none",
        )
        rejected.headers["If-Match"] = stale["etag"]
        with pytest.raises(HttpError) as exc:
            rejected.execute()
        assert exc.value.resp.status == 412
        assert calendar_mod.get_event(event_id)["event"]["summary"] == summary

        # C8/C9: update previews are non-writing; confirmed PATCH uses a fresh ETag.
        update_args = {
            "event_id": event_id,
            "summary": f"{summary} updated",
            "description": "C9-updated",
            "availability": "free",
            "send_updates": "none",
        }
        preview = calendar_mod.update_event(**update_args)
        assert preview["status"] == "confirmation_required"
        assert calendar_mod.get_event(event_id)["event"]["summary"] == summary
        changed = calendar_mod.update_event(
            **update_args, etag=preview["impact"]["before"]["etag"], confirm=True
        )
        assert changed["updated"] is True
        assert changed["event"]["summary"].endswith(" updated")
        assert changed["event"]["description"] == "C9-updated"
        assert changed["event"]["availability"] == "free"

        # C10: RSVP preview and confirmed response affect only the signed-in attendee.
        response_preview = calendar_mod.respond_to_event(event_id, "tentative")
        assert response_preview["status"] == "confirmation_required"
        assert response_preview["impact"]["to_status"] == "tentative"
        responded = calendar_mod.respond_to_event(
            event_id,
            "tentative",
            etag=response_preview["impact"]["etag"],
            confirm=True,
        )
        assert responded["responded"] is True
        me = next(item for item in responded["event"]["attendees"] if item.get("self"))
        assert me["responseStatus"] == "tentative"

        # C11/C12: delete preview preserves the entire series; confirmed delete removes it.
        deletion = calendar_mod.delete_event(event_id)
        assert deletion["status"] == "confirmation_required"
        assert deletion["impact"]["deletes_series"] is True
        assert not _missing(event_id), "C11: unconfirmed delete removed the event"
        deleted = calendar_mod.delete_event(
            event_id, etag=deletion["impact"]["event"]["etag"], confirm=True
        )
        assert deleted == {"calendar_id": "primary", "event_id": event_id, "deleted": True}
        assert _missing(event_id), "C12: confirmed delete left the event behind"
    finally:
        if not _missing(event_id):
            calendar().events().delete(
                calendarId="primary", eventId=event_id, sendUpdates="none"
            ).execute()


def test_live_harness_targets_all_calendar_tools():
    names = {tool.__name__ for tool in calendar_mod._TOOLS}
    assert names == {
        "list_calendars",
        "list_events",
        "get_event",
        "query_freebusy",
        "create_event",
        "update_event",
        "delete_event",
        "respond_to_event",
    }
