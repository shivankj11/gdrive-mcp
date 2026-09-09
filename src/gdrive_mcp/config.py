"""OAuth scope and on-disk locations for the client-secrets file and cached token."""

from __future__ import annotations

import os
from pathlib import Path

# Full read/write Drive access ("view and manage all your Drive files"). This is a
# restricted scope; OAuth verification and administrator-policy requirements depend on
# the deployment. Per-user consent bounds access to files available to the signed-in user.
SCOPES = [
    "https://www.googleapis.com/auth/drive",
    # Calendar access is deliberately split rather than using the broad `calendar` scope,
    # which would also allow changing calendar properties, sharing, and deleting calendars.
    "https://www.googleapis.com/auth/calendar.calendarlist.readonly",
    "https://www.googleapis.com/auth/calendar.events",
    "https://www.googleapis.com/auth/calendar.events.freebusy",
]

_APP_DIR_NAME = "gdrive-mcp"


def _config_dir() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME")
    root = Path(base) if base else Path.home() / ".config"
    return root / _APP_DIR_NAME


def oauth_client_path() -> Path:
    """Desktop-app OAuth client file (client_id + client_secret) from Google Cloud.

    Store this file securely and never commit it. Override with GDRIVE_MCP_OAUTH_CLIENT.
    """
    override = os.environ.get("GDRIVE_MCP_OAUTH_CLIENT")
    return Path(override).expanduser() if override else _config_dir() / "oauth_client.json"


def token_path() -> Path:
    """Cached per-user OAuth token (access + refresh). Override with GDRIVE_MCP_TOKEN."""
    override = os.environ.get("GDRIVE_MCP_TOKEN")
    return Path(override).expanduser() if override else _config_dir() / "token.json"
