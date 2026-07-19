"""Append-only audit log of tool activity.

Records identifiers and actions only — tool name, target item id, destination path, outcome,
and the authenticated user — NEVER content values (rows/text/content are not logged).
"""

from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path

from gdrive_mcp.config import _config_dir
from gdrive_mcp.ids import parse_ref

# Args safe to record: opaque identifiers + operational scalars only. Deliberately omits
# free-text fields (name, title, tab, dest_path, source_path, new_name) — a filename or tab
# title can itself contain sensitive data — and never records rows/text/content.
_SAFE_ARG_KEYS = ("item", "replace_id", "parent", "start_row", "count", "to", "value_input")
# Ref args reduced to their opaque Drive id so free text (potentially sensitive) in a raw URL/string
# can't be smuggled into the log verbatim.
_REF_ARG_KEYS = ("item", "replace_id", "parent")


def _safe_args(args: dict) -> dict:
    out: dict = {}
    for k in _SAFE_ARG_KEYS:
        if k not in args:
            continue
        if k in _REF_ARG_KEYS:
            try:
                out[k] = parse_ref(args[k]).id
            except Exception:
                out[k] = "<unparseable>"
        else:
            out[k] = args[k]
    return out


def _audit_path() -> Path:
    override = os.environ.get("GDRIVE_MCP_AUDIT_LOG")
    return Path(override).expanduser() if override else _config_dir() / "audit.log"


def record(tool: str, args: dict, outcome: str, user: str | None = None) -> None:
    """Append one JSON line describing a tool call. Best-effort — never raises into the caller."""
    entry = {
        "ts": datetime.now(timezone.utc).isoformat(),
        "user": user,
        "tool": tool,
        "args": _safe_args(args),
        "outcome": outcome,
    }
    try:
        path = _audit_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a") as f:
            f.write(json.dumps(entry, default=str) + "\n")
        path.chmod(0o600)
    except Exception:
        pass
