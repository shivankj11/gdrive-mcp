"""Browser-OAuth setup and credential loading for the Drive MCP.

The interactive browser consent is a one-time step (`gdrive-mcp auth`) kept
separate from the eventual MCP server runtime: a stdio MCP server launched by an
MCP client must complete its init handshake promptly and cannot block on a
browser prompt. After `auth`, callers use `load_credentials()`, which loads the
cached token and refreshes it silently — never opening a browser.
"""

from __future__ import annotations

import json
from pathlib import Path

from google.auth.transport.requests import Request
from google.oauth2.credentials import Credentials
from google_auth_oauthlib.flow import InstalledAppFlow

from gdrive_mcp.config import SCOPES, oauth_client_path, token_path


class AuthError(RuntimeError):
    """No usable credentials — the caller should prompt the user to run `auth`."""


def _write_token(creds: Credentials) -> Path:
    path = token_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(creds.to_json())
    path.chmod(0o600)
    return path


def run_auth_flow() -> Credentials:
    """Run the one-time loopback browser-consent flow and cache the token."""
    client_path = oauth_client_path()
    if not client_path.exists():
        raise AuthError(
            f"OAuth client file not found at {client_path}. Download the Desktop-app "
            "OAuth client JSON from Google Cloud and place it there, or set "
            "GDRIVE_MCP_OAUTH_CLIENT to its path."
        )
    flow = InstalledAppFlow.from_client_secrets_file(str(client_path), SCOPES)
    # Loopback redirect (127.0.0.1:<random free port>). prompt=consent forces Google
    # to return a refresh token so the server never needs the browser again.
    creds = flow.run_local_server(port=0, prompt="consent")
    _write_token(creds)
    return creds


def load_credentials() -> Credentials:
    """Load cached credentials, refreshing if expired. Never opens a browser.

    Raises AuthError if the token is missing or unusable.
    """
    path = token_path()
    if not path.exists():
        raise AuthError(f"No cached credentials at {path}. Run `gdrive-mcp auth` first.")
    creds = Credentials.from_authorized_user_info(json.loads(path.read_text()), SCOPES)
    if creds.valid:
        return creds
    if creds.expired and creds.refresh_token:
        creds.refresh(Request())
        _write_token(creds)
        return creds
    raise AuthError(
        "Cached credentials are invalid and cannot be refreshed. Run `gdrive-mcp auth` again."
    )
