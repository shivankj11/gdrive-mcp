from __future__ import annotations

from datetime import datetime, timedelta, timezone
import json

import pytest

from gdrive_mcp import auth
from gdrive_mcp.config import SCOPES
from gdrive_mcp.tools import calendar as tools


class _Request:
    def __init__(self, response):
        self.response = response
        self.headers = {}

    def execute(self):
        return self.response


class _Resource:
    def __init__(self, fake, prefix):
        self.fake = fake
        self.prefix = prefix

    def __getattr__(self, method):
        def call(**kwargs):
            name = f"{self.prefix}.{method}"
            self.fake.calls.append((name, kwargs))
            values = self.fake.responses.get(name, [])
            request = _Request(values.pop(0) if values else {})
            self.fake.requests.append((name, request))
            return request

        return call


class FakeCalendar:
    def __init__(self):
        self.calls = []
        self.responses = {}
        self.requests = []

    def on(self, name, *responses):
        self.responses.setdefault(name, []).extend(responses)
        return self

    def calendarList(self):
        return _Resource(self, "calendarList")

    def events(self):
        return _Resource(self, "events")

    def freebusy(self):
        return _Resource(self, "freebusy")

    def calls_to(self, name):
        return [kwargs for method, kwargs in self.calls if method == name]


@pytest.fixture
def cal(monkeypatch):
    fake = FakeCalendar()
    monkeypatch.setattr(tools, "calendar", lambda: fake)
    return fake


EVENT = {
    "id": "evt-1",
    "status": "confirmed",
    "htmlLink": "https://calendar.google.com/event?eid=x",
    "summary": "Planning",
    "description": "Agenda",
    "location": "Room 1",
    "start": {"dateTime": "2026-09-09T10:00:00-07:00"},
    "end": {"dateTime": "2026-09-09T11:00:00-07:00"},
    "attendees": [{"email": "me@example.com", "self": True, "responseStatus": "needsAction"}],
    "recurrence": ["RRULE:FREQ=WEEKLY"],
    "hangoutLink": "https://meet.google.com/abc-defg-hij",
    "transparency": "transparent",
    "etag": '"etag-1"',
}


def test_list_calendars_clamps_pages_and_returns_a_stable_shape(cal):
    cal.on(
        "calendarList.list",
        {
            "items": [
                {
                    "id": "primary@example.com",
                    "summary": "Original",
                    "summaryOverride": "Mine",
                    "timeZone": "America/Los_Angeles",
                    "accessRole": "owner",
                    "primary": True,
                }
            ],
            "nextPageToken": "next",
        },
    )
    out = tools.list_calendars(page_size=999, page_token="page", min_access_role="reader", show_hidden=True)
    assert out == {
        "count": 1,
        "calendars": [
            {
                "id": "primary@example.com",
                "summary": "Mine",
                "description": None,
                "location": None,
                "time_zone": "America/Los_Angeles",
                "access_role": "owner",
                "primary": True,
                "selected": False,
                "hidden": False,
            }
        ],
        "has_more": True,
        "next_page_token": "next",
    }
    assert cal.calls_to("calendarList.list") == [
        {"maxResults": 250, "pageToken": "page", "minAccessRole": "reader", "showHidden": True}
    ]


def test_list_events_expands_recurrence_forwards_filters_and_normalizes(cal):
    cal.on("events.list", {"timeZone": "UTC", "items": [EVENT], "nextPageToken": "next"})
    out = tools.list_events(
        calendar_id="team@example.com",
        time_min="2026-09-01T00:00:00Z",
        time_max="2026-10-01T00:00:00Z",
        query="Planning",
        page_size=9999,
        page_token="p",
        include_cancelled=True,
    )
    assert out["events"][0]["conference_url"] == "https://meet.google.com/abc-defg-hij"
    assert out["events"][0]["availability"] == "free"
    assert out["has_more"] is True
    assert cal.calls_to("events.list") == [
        {
            "calendarId": "team@example.com",
            "timeMin": "2026-09-01T00:00:00Z",
            "timeMax": "2026-10-01T00:00:00Z",
            "q": "Planning",
            "maxResults": 2500,
            "pageToken": "p",
            "showDeleted": True,
            "singleEvents": True,
            "orderBy": "startTime",
        }
    ]


def test_list_events_rejects_naive_or_reversed_windows_before_the_api(cal):
    with pytest.raises(RuntimeError, match="UTC offset"):
        tools.list_events(time_min="2026-09-01T00:00:00", time_max="2026-09-02T00:00:00Z")
    with pytest.raises(RuntimeError, match="after"):
        tools.list_events(time_min="2026-09-02T00:00:00Z", time_max="2026-09-01T00:00:00Z")
    assert cal.calls == []


def test_list_events_defaults_to_a_bounded_thirty_day_window(cal):
    before = datetime.now(timezone.utc)
    cal.on("events.list", {"items": []})
    out = tools.list_events()
    after = datetime.now(timezone.utc)
    time_min = datetime.fromisoformat(out["time_min"].replace("Z", "+00:00"))
    time_max = datetime.fromisoformat(out["time_max"].replace("Z", "+00:00"))
    assert before <= time_min <= after
    assert time_max - time_min == timedelta(days=30)
    request = cal.calls_to("events.list")[0]
    assert request["calendarId"] == "primary"
    assert request["maxResults"] == 25


def test_get_event_returns_the_same_shape_as_list(cal):
    cal.on(
        "events.get",
        {
            **EVENT,
            "hangoutLink": "",
            "conferenceData": {
                "entryPoints": [{"entryPointType": "video", "uri": "https://meet.google.com/fallback"}]
            },
        },
    )
    out = tools.get_event("evt-1", "team@example.com")
    assert out["calendar_id"] == "team@example.com"
    assert out["event"]["id"] == "evt-1"
    assert out["event"]["recurrence"] == ["RRULE:FREQ=WEEKLY"]
    assert out["event"]["conference_url"] == "https://meet.google.com/fallback"


def test_freebusy_defaults_to_primary_and_preserves_per_calendar_errors(cal):
    cal.on(
        "freebusy.query",
        {
            "timeMin": "2026-09-09T00:00:00Z",
            "timeMax": "2026-09-10T00:00:00Z",
            "calendars": {
                "primary": {"busy": [{"start": "a", "end": "b"}]},
                "missing": {"errors": [{"reason": "notFound"}]},
            },
        },
    )
    out = tools.query_freebusy("2026-09-09T00:00:00Z", "2026-09-10T00:00:00Z")
    assert out["calendars"]["primary"]["busy"] == [{"start": "a", "end": "b"}]
    assert out["calendars"]["missing"]["errors"] == [{"reason": "notFound"}]
    assert cal.calls_to("freebusy.query")[0]["body"]["items"] == [{"id": "primary"}]
    cal.on("freebusy.query", {"calendars": {}})
    tools.query_freebusy("2026-09-09T00:00:00Z", "2026-09-10T00:00:00Z", calendar_ids=[])
    assert cal.calls_to("freebusy.query")[1]["body"]["items"] == [{"id": "primary"}]


def test_freebusy_rejects_more_than_google_allows(cal):
    with pytest.raises(RuntimeError, match="at most 50"):
        tools.query_freebusy(
            "2026-09-09T00:00:00Z", "2026-09-10T00:00:00Z", [f"c{i}" for i in range(51)]
        )
    assert cal.calls == []


def test_create_previews_without_writing_and_dry_run_never_writes(cal):
    args = dict(summary="Planning", start="2026-09-09T10:00:00-07:00", end="2026-09-09T11:00:00-07:00")
    preview = tools.create_event(**args)
    assert preview["status"] == "confirmation_required"
    assert preview["impact"]["send_updates"] == "none"
    assert cal.calls == []
    dry = tools.create_event(**args, dry_run=True, confirm=True)
    assert dry["dry_run"] is True
    assert cal.calls == []


def test_confirmed_create_sends_attendees_reminders_recurrence_and_meet(cal):
    cal.on("events.insert", EVENT)
    out = tools.create_event(
        "Planning",
        "2026-09-09T10:00:00-07:00",
        "2026-09-09T11:00:00-07:00",
        calendar_id="team@example.com",
        event_id="abcde12345",
        time_zone="America/Los_Angeles",
        attendees=["guest@example.com"],
        recurrence=["RRULE:FREQ=WEEKLY;COUNT=2"],
        reminders=[{"method": "popup", "minutes": 10}],
        send_updates="all",
        add_google_meet=True,
        availability="free",
        visibility="private",
        confirm=True,
    )
    assert out["created"] is True
    request = cal.calls_to("events.insert")[0]
    assert request["calendarId"] == "team@example.com"
    assert request["sendUpdates"] == "all"
    assert request["conferenceDataVersion"] == 1
    assert request["body"]["attendees"] == [{"email": "guest@example.com"}]
    assert request["body"]["id"] == "abcde12345"
    assert request["body"]["reminders"] == {
        "useDefault": False,
        "overrides": [{"method": "popup", "minutes": 10}],
    }
    assert request["body"]["transparency"] == "transparent"


def test_all_day_dates_are_exclusive_and_do_not_carry_a_timezone(cal):
    dry = tools.create_event(
        "Away",
        "2026-09-09",
        "2026-09-10",
        all_day=True,
        recurrence=["EXRULE:FREQ=YEARLY;COUNT=1"],
        dry_run=True,
    )
    assert dry["event"]["start"] == {"date": "2026-09-09"}
    assert dry["event"]["end"] == {"date": "2026-09-10"}
    with pytest.raises(RuntimeError, match="exclusive"):
        tools.create_event("Away", "2026-09-09", "2026-09-09", all_day=True)


@pytest.mark.parametrize(
    "kwargs,message",
    [
        ({"send_updates": "sometimes"}, "send_updates"),
        ({"attendees": ["not-an-email"]}, "email address"),
        ({"recurrence": ["WEEKLY"]}, "RRULE"),
        ({"recurrence": ["RRULE:FREQ=WEEKLY"]}, "require time_zone"),
        ({"reminders": [{"method": "sms", "minutes": 1}]}, "reminder"),
        ({"reminders": [{"method": "popup", "minutes": True}]}, "reminder"),
        ({"reminders": [{"method": "popup", "minutes": 1}] * 6}, "at most 5"),
        ({"visibility": "secret"}, "visibility"),
        ({"availability": "maybe"}, "availability"),
        ({"time_zone": "Not/AZone"}, "time zone"),
        ({"event_id": "BAD-ID"}, "base32hex"),
    ],
)
def test_create_validates_high_risk_inputs_before_preview(cal, kwargs, message):
    with pytest.raises(RuntimeError, match=message):
        tools.create_event(
            "Planning", "2026-09-09T10:00:00-07:00", "2026-09-09T11:00:00-07:00", **kwargs
        )
    assert cal.calls == []


def test_update_reads_then_previews_and_distinguishes_clear_from_unset(cal):
    cal.on("events.get", EVENT)
    out = tools.update_event("evt-1", attendees=[], recurrence=[], summary="New")
    assert out["status"] == "confirmation_required"
    assert out["impact"]["changed_fields"] == ["attendees", "recurrence", "summary"]
    assert out["impact"]["after"]["attendees"] == []
    assert cal.calls_to("events.patch") == []


def test_confirmed_update_patches_only_supplied_fields(cal):
    cal.on("events.get", EVENT).on("events.patch", {**EVENT, "summary": "New", "location": "Room 2"})
    out = tools.update_event(
        "evt-1",
        etag='"etag-1"',
        summary="New",
        location="Room 2",
        send_updates="externalOnly",
        confirm=True,
    )
    assert out["updated"] is True
    request = cal.calls_to("events.patch")[0]
    assert request["body"] == {"summary": "New", "location": "Room 2"}
    assert request["sendUpdates"] == "externalOnly"
    assert dict(cal.requests)["events.patch"].headers["If-Match"] == '"etag-1"'


def test_update_needs_a_change_and_paired_times(cal):
    with pytest.raises(RuntimeError, match="nothing to update"):
        tools.update_event("evt-1")
    with pytest.raises(RuntimeError, match="supplied together"):
        tools.update_event("evt-1", start="2026-09-09T12:00:00Z")
    assert cal.calls == []


def test_update_requires_a_timezone_when_turning_a_timed_event_into_a_series(cal):
    cal.on("events.get", {**EVENT, "recurrence": []})
    with pytest.raises(RuntimeError, match="start/end with time_zone"):
        tools.update_event("evt-1", recurrence=["RRULE:FREQ=WEEKLY"])
    assert cal.calls_to("events.patch") == []


def test_confirmed_calendar_writes_require_the_preview_etag_and_reject_a_stale_one(cal):
    cal.on("events.get", EVENT, EVENT)
    with pytest.raises(RuntimeError, match="requires the etag"):
        tools.update_event("evt-1", summary="New", confirm=True)
    with pytest.raises(RuntimeError, match="changed after preview"):
        tools.update_event("evt-1", etag='"stale"', summary="New", confirm=True)
    assert cal.calls_to("events.patch") == []


def test_delete_identifies_series_and_only_deletes_after_confirmation(cal):
    cal.on("events.get", EVENT, EVENT).on("events.delete", {})
    preview = tools.delete_event("evt-1")
    assert preview["impact"]["deletes_series"] is True
    assert cal.calls_to("events.delete") == []
    out = tools.delete_event("evt-1", etag='"etag-1"', confirm=True, send_updates="all")
    assert out["deleted"] is True
    assert cal.calls_to("events.delete")[0]["sendUpdates"] == "all"
    assert dict(cal.requests)["events.delete"].headers["If-Match"] == '"etag-1"'


def test_respond_uses_attendees_omitted_to_change_only_self(cal):
    event = {
        **EVENT,
        "attendees": [
            {"email": "me@example.com", "self": True, "responseStatus": "needsAction"},
            {"email": "other@example.com", "responseStatus": "accepted", "optional": True},
        ],
    }
    cal.on("events.get", event, event).on("events.patch", {**event, "attendees": []})
    preview = tools.respond_to_event("evt-1", "tentative")
    assert preview["impact"]["from_status"] == "needsAction"
    assert cal.calls_to("events.patch") == []
    tools.respond_to_event("evt-1", "declined", etag='"etag-1"', confirm=True)
    sent = cal.calls_to("events.patch")[0]["body"]
    assert sent == {
        "attendeesOmitted": True,
        "attendees": [{"email": "me@example.com", "responseStatus": "declined"}],
    }
    assert [request for name, request in cal.requests if name == "events.patch"][-1].headers["If-Match"] == '"etag-1"'


def test_respond_refuses_an_event_where_the_user_is_not_an_attendee(cal):
    cal.on("events.get", {**EVENT, "attendees": [{"email": "other@example.com"}]})
    with pytest.raises(RuntimeError, match="not an attendee"):
        tools.respond_to_event("evt-1", "accepted")


def test_old_drive_only_token_gets_a_directed_reconsent_error(tmp_path, monkeypatch):
    token = tmp_path / "token.json"
    token.write_text(
        json.dumps(
            {
                "token": "access",
                "refresh_token": "refresh",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "client",
                "client_secret": "secret",
                "scopes": ["https://www.googleapis.com/auth/drive"],
            }
        )
    )
    monkeypatch.setattr(auth, "token_path", lambda: token)
    with pytest.raises(auth.AuthError, match="Run `gdrive-mcp auth` again") as caught:
        auth.load_credentials()
    assert "calendar.events" in str(caught.value)


def test_config_requests_each_required_narrow_calendar_scope():
    assert "https://www.googleapis.com/auth/calendar" not in SCOPES
    assert {
        "https://www.googleapis.com/auth/calendar.calendarlist.readonly",
        "https://www.googleapis.com/auth/calendar.events",
        "https://www.googleapis.com/auth/calendar.events.freebusy",
    } <= set(SCOPES)
