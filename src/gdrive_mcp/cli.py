"""CLI entry points: `auth` (one-time browser consent) and `whoami` (verify)."""

from __future__ import annotations

import argparse
import sys

from googleapiclient.discovery import build

from gdrive_mcp.auth import AuthError, load_credentials, run_auth_flow
from gdrive_mcp.config import token_path


def _authed_user(creds) -> dict:
    """Confirm the token works by reading the authenticated user via drive.about."""
    service = build("drive", "v3", credentials=creds, cache_discovery=False)
    about = service.about().get(fields="user").execute()
    return about.get("user", {})


def _cmd_auth(_args: argparse.Namespace) -> int:
    creds = run_auth_flow()
    user = _authed_user(creds)
    print(
        f"Authenticated as {user.get('emailAddress', 'unknown')}.\n"
        f"Token cached at {token_path()} (read/write Drive and Calendar event access)."
    )
    return 0


def _cmd_whoami(_args: argparse.Namespace) -> int:
    user = _authed_user(load_credentials())
    label = f"{user.get('emailAddress', 'unknown')} ({user.get('displayName', '')})"
    print(label.replace(" ()", ""))
    return 0


def _cmd_serve(_args: argparse.Namespace) -> int:
    from gdrive_mcp.server import run

    run()
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="gdrive-mcp",
        description="Google Drive / Docs / Sheets / Calendar MCP (read/write) — auth + serve.",
    )
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("auth", help="Run the one-time browser OAuth consent and cache the token.")
    sub.add_parser("whoami", help="Verify cached credentials by printing the authenticated user.")
    sub.add_parser("serve", help="Run the MCP server over stdio.")

    args = parser.parse_args(argv)
    handler = {"auth": _cmd_auth, "whoami": _cmd_whoami, "serve": _cmd_serve}[args.command]
    try:
        return handler(args)
    except AuthError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
