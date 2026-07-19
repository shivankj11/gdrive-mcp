"""Confirm-before-destructive helper.

Destructive tools take `confirm=False`. When not confirmed they compute a live impact
preview and return it via `preview_response` WITHOUT mutating; the caller re-invokes with
confirm=true to execute.
"""

from __future__ import annotations

from typing import Any


def preview_response(action: str, impact: dict[str, Any], *, message: str | None = None) -> dict:
    return {
        "status": "confirmation_required",
        "action": action,
        "impact": impact,
        "next": message or "Re-call the same tool with confirm=true to execute.",
    }
