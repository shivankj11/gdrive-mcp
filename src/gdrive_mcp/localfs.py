"""Confine the server's local file I/O to one operator-configured sandbox directory.

Every local read (upload source) and write (download/export/sheet spill) is resolved inside
`GDRIVE_MCP_FILES_DIR` (default a private 0700 dir under the config dir). Absolute paths and
`..` escapes are rejected, so a prompt-injected agent can neither write outside the sandbox
(e.g. ~/.ssh/authorized_keys, cron files) nor read arbitrary local files (e.g. id_rsa) to
exfiltrate them to Drive.

Deletion is gated on ownership: the retention sweep only runs in a directory carrying the
`.gdrive-mcp-sandbox` marker, which is written only when gdrive-mcp created the directory
itself (or in the app-default location). Pointing GDRIVE_MCP_FILES_DIR at a pre-existing
directory therefore confines I/O there but never lets the sweep delete the files that were
already in it.
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

from gdrive_mcp.config import _config_dir

# Marks a directory as gdrive-mcp's own disposable sandbox. sweep_expired() deletes nothing in
# a directory that lacks it; operators can opt a hand-picked directory in by creating it.
MARKER_NAME = ".gdrive-mcp-sandbox"


def _mark_owned(root: Path) -> None:
    """Drop the ownership marker (best-effort — an unwritable root just leaves the sweep off)."""
    marker = root / MARKER_NAME
    try:
        if not marker.exists():
            marker.write_text(
                "This directory is gdrive-mcp's disposable file sandbox: files older than the\n"
                "retention TTL (GDRIVE_MCP_FILES_TTL_HOURS, default 24h) are deleted on server\n"
                "start. Do not keep anything here.\n"
            )
    except OSError:
        pass


def files_root() -> Path:
    """The sandbox directory; created 0700 + ownership-marked when it doesn't exist.

    A pre-existing directory supplied via GDRIVE_MCP_FILES_DIR is used as-is — permissions
    untouched, no ownership marker — so only directories gdrive-mcp created (or the app-default
    location, which is ours by definition) are ever eligible for the retention sweep.
    """
    override = os.environ.get("GDRIVE_MCP_FILES_DIR")
    root = (Path(override).expanduser() if override else _config_dir() / "files").resolve()
    try:
        root.mkdir(parents=True)
        created = True
    except FileExistsError:
        if not root.is_dir():
            raise
        created = False
    if created or not override:
        try:
            root.chmod(0o700)
        except OSError:
            pass
        _mark_owned(root)
    return root


def _within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def _sanitize(name: str) -> str:
    base = Path(name).name  # drop any directory components from an untrusted name
    cleaned = "".join(c if c.isalnum() or c in "-_." else "_" for c in base)
    return cleaned or "download"


def safe_write_path(dest_path: str | None, default_name: str) -> Path:
    """Resolve a write destination inside the sandbox.

    `dest_path` is treated as relative to the sandbox root; absolute paths and `..` escapes are
    rejected. When None, a sanitized `default_name` at the root is used. Parent dirs are created.
    """
    root = files_root()
    rel = dest_path if dest_path else _sanitize(default_name)
    final = (root / rel).resolve()
    if not _within(final, root):
        raise RuntimeError(
            f"dest_path {dest_path!r} must stay within the files dir ({root}); absolute paths and "
            f"'..' are not allowed. Set GDRIVE_MCP_FILES_DIR to change the sandbox."
        )
    final.parent.mkdir(parents=True, exist_ok=True)
    return final


def sweep_expired(ttl_hours: float | None = None) -> None:
    """Delete sandbox files older than the retention TTL (default 24h; GDRIVE_MCP_FILES_TTL_HOURS).

    Bounds at-rest disposal of spilled downloads/exports/CSVs. TTL <= 0 disables the sweep.
    Refuses to sweep a directory without the `MARKER_NAME` ownership marker: a pre-existing
    GDRIVE_MCP_FILES_DIR may hold files that are not disposable spills, and deleting them on
    startup would be silent data loss. Creating the marker there opts the directory in.
    Best-effort and fully failure-safe: a bad TTL value or unusable files dir never breaks
    startup.
    """
    try:
        if ttl_hours is None:
            try:
                ttl_hours = float(os.environ.get("GDRIVE_MCP_FILES_TTL_HOURS", "24"))
            except (TypeError, ValueError):
                ttl_hours = 24.0
        if ttl_hours <= 0:
            return
        root = files_root()
        if not (root / MARKER_NAME).exists():
            print(
                f"gdrive-mcp: not sweeping {root}: the directory pre-existed, so its contents "
                f"may not be disposable spills. To enable the retention sweep there, run: "
                f"touch '{root / MARKER_NAME}'",
                file=sys.stderr,
            )
            return
        cutoff = time.time() - ttl_hours * 3600
        for path in root.rglob("*"):
            try:
                if path.name == MARKER_NAME:
                    continue
                if path.is_file() and path.stat().st_mtime < cutoff:
                    path.unlink()
            except OSError:
                pass
    except Exception:
        pass


def safe_read_path(source_path: str) -> Path:
    """Resolve an upload source inside the sandbox; reject escapes and missing files."""
    root = files_root()
    final = (root / source_path).resolve()
    if not _within(final, root):
        raise RuntimeError(
            f"source_path {source_path!r} must be inside the files dir ({root}); place the file "
            f"there or set GDRIVE_MCP_FILES_DIR. Absolute paths and '..' are not allowed."
        )
    if not final.is_file():
        raise RuntimeError(f"source_path not found in files dir: {final}")
    return final
