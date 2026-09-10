#!/usr/bin/env python3
"""Package an already-built Rust binary with explicit OAuth provisioning.

Uses only the Python standard library. Run with --help for private and public options.
Never reads the sender's token or discovers credentials implicitly.
"""

from __future__ import annotations

import argparse
import io
import json
from pathlib import Path
import tarfile

ROOT = Path(__file__).resolve().parent.parent


def desktop_client(path: Path) -> bytes:
    """Include only the app configuration needed for auth, never user token fields."""
    try:
        doc = json.loads(path.read_text())
    except (ValueError, UnicodeError) as exc:
        raise ValueError("OAuth file must be valid Desktop-app JSON") from exc
    installed = doc.get("installed") if isinstance(doc, dict) else None
    if not isinstance(installed, dict):
        raise ValueError("Expected Desktop-app JSON with an 'installed' object; not a token, web client, or service account")
    client = {}
    for key in ("client_id", "client_secret"):
        value = installed.get(key)
        if not isinstance(value, str) or not value.strip():
            raise ValueError(f"Desktop-app JSON is missing {key}")
        client[key] = value
    for key, expected in (
        ("auth_uri", "https://accounts.google.com/o/oauth2/auth"),
        ("token_uri", "https://oauth2.googleapis.com/token"),
    ):
        if installed.get(key, expected) != expected:
            raise ValueError(f"Desktop-app JSON has an unexpected {key}; use the Google-downloaded client")
        client[key] = expected
    return (json.dumps({"installed": client}, indent=2) + "\n").encode()


def package(binary: Path, output: Path, oauth_client: Path | None) -> None:
    # Read inputs before creating an output. The explicit allowlist excludes adjacent
    # token.json, audit logs, sandbox files, source trees, and private-only documentation.
    files = {
        "gdrive-mcp": (binary.read_bytes(), 0o700),
        "START_HERE.md": ((ROOT / "docs" / "BINARY_HANDOFF.md").read_bytes(), 0o600),
    }
    if oauth_client is not None:
        files["oauth_client.json"] = (desktop_client(oauth_client), 0o600)
        status = "INCLUDED: oauth_client.json is the Desktop-app configuration. Install it with mode 600."
    else:
        status = "NOT INCLUDED: obtain Desktop-app OAuth JSON from the sender or your Google Cloud project before auth."
    instructions = (
        "# Binary installation instructions for agents\n\n"
        "Read START_HERE.md before installing or registering this server.\n\n"
        f"OAuth client JSON: {status}\n\n"
        "A binary alone is not a complete first-time setup. Each recipient must run auth\n"
        "in their own browser; never copy the sender's token.json. Verify whoami, loaded\n"
        "MCP tools, and a read-only Drive query before claiming installation succeeded.\n"
    )
    files["AGENTS.md"] = (instructions.encode(), 0o600)
    output.parent.mkdir(parents=True, exist_ok=True)
    # Exclusive creation avoids overwriting an existing deliverable or following a symlink.
    with output.open("xb") as raw:
        try:
            output.chmod(0o600)
            with tarfile.open(fileobj=raw, mode="w:gz") as archive:
                directory = tarfile.TarInfo("gdrive-mcp-bundle")
                directory.type = tarfile.DIRTYPE
                directory.mode = 0o700
                archive.addfile(directory)
                for name, (data, mode) in files.items():
                    info = tarfile.TarInfo(f"gdrive-mcp-bundle/{name}")
                    info.size = len(data)
                    info.mode = mode
                    archive.addfile(info, io.BytesIO(data))
        except BaseException:
            output.unlink(missing_ok=True)
            raise


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="Already-built binary for the recipient's OS/architecture")
    parser.add_argument("--output", type=Path, required=True, help="New .tar.gz archive (will not overwrite)")
    credentials = parser.add_mutually_exclusive_group(required=True)
    credentials.add_argument("--oauth-client", type=Path, help="Include Desktop-app JSON for a private handoff")
    credentials.add_argument("--without-oauth-client", action="store_true", help="Public/credential-free bundle; recipient must obtain JSON separately")
    args = parser.parse_args()
    try:
        package(args.binary, args.output, args.oauth_client)
    except (OSError, ValueError) as exc:
        parser.exit(1, f"Packaging failed: {exc}\n")
    print(f"Created {args.output}")
    print("OAuth client JSON: " + ("INCLUDED — private handoff only" if args.oauth_client else "NOT INCLUDED — recipient must supply it"))
    print("Recipient instructions: gdrive-mcp-bundle/START_HERE.md and AGENTS.md")


if __name__ == "__main__":
    main()
