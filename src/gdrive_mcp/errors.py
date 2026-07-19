"""Turn Google API HttpErrors into concise, actionable messages for MCP clients."""

from __future__ import annotations

import functools

from googleapiclient.errors import HttpError

_HINTS = {
    403: " (is the API enabled for this project and the drive scope granted?)",
    404: " (check the ID/URL and that your account has access)",
    429: " (rate limited — retry shortly)",
}


def explain(exc: Exception) -> str:
    if isinstance(exc, HttpError):
        status = getattr(exc.resp, "status", "?")
        reason = getattr(exc, "reason", "") or str(exc)
        return f"Google API error {status}: {reason}{_HINTS.get(status, '')}"
    return f"{type(exc).__name__}: {exc}"


def api_errors(fn):
    """Wrap a tool so Google HttpErrors surface as clean RuntimeErrors."""

    @functools.wraps(fn)
    def wrapper(*args, **kwargs):
        try:
            return fn(*args, **kwargs)
        except HttpError as exc:
            raise RuntimeError(explain(exc)) from exc

    return wrapper
