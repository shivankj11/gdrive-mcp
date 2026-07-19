"""Lazily-built, cached Google API clients sharing one set of user credentials.

Credentials are loaded (and refreshed if needed) on first use via load_credentials();
if no cached token exists that raises AuthError telling the user to run `gdrive-mcp auth`.
"""

from __future__ import annotations

from functools import lru_cache

from google.auth.transport.requests import AuthorizedSession
from googleapiclient.discovery import build

from gdrive_mcp.auth import load_credentials


@lru_cache(maxsize=1)
def _creds():
    return load_credentials()


@lru_cache(maxsize=1)
def drive():
    return build("drive", "v3", credentials=_creds(), cache_discovery=False)


@lru_cache(maxsize=1)
def sheets():
    return build("sheets", "v4", credentials=_creds(), cache_discovery=False)


@lru_cache(maxsize=1)
def docs():
    return build("docs", "v1", credentials=_creds(), cache_discovery=False)


@lru_cache(maxsize=1)
def authed_session() -> AuthorizedSession:
    """For fetching short-lived Docs image contentUris, which require Authorization."""
    return AuthorizedSession(_creds())


@lru_cache(maxsize=1)
def authed_user_email() -> str | None:
    """The signed-in user's email, for audit logging. Cached; best-effort (None on failure)."""
    try:
        return drive().about().get(fields="user/emailAddress").execute().get("user", {}).get("emailAddress")
    except Exception:
        return None
