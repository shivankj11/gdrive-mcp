"""Discovery tools: resolve links, search, browse folders, read metadata."""

from __future__ import annotations

from gdrive_mcp.clients import drive
from gdrive_mcp.errors import api_errors
from gdrive_mcp.ids import parse_ref

_FILE_FIELDS = (
    "id, name, mimeType, modifiedTime, size, "
    "owners(displayName,emailAddress), webViewLink, parents"
)

_KIND_BY_MIME = {
    "application/vnd.google-apps.spreadsheet": "spreadsheet",
    "application/vnd.google-apps.document": "document",
    "application/vnd.google-apps.folder": "folder",
}


def _kind(mime: str) -> str:
    return _KIND_BY_MIME.get(mime, "file")


def _esc(value: str) -> str:
    return value.replace("\\", "\\\\").replace("'", "\\'")


def _slim(f: dict) -> dict:
    return {
        "id": f.get("id"),
        "name": f.get("name"),
        "mime_type": f.get("mimeType"),
        "kind": _kind(f.get("mimeType", "")),
        "modified": f.get("modifiedTime"),
        "size": f.get("size"),
        "web_view_link": f.get("webViewLink"),
    }


@api_errors
def resolve_link(item: str) -> dict:
    """Resolve a Google Drive/Docs/Sheets URL or bare ID (item) to its id, kind, name and link."""
    ref = parse_ref(item)
    f = drive().files().get(fileId=ref.id, fields=_FILE_FIELDS, supportsAllDrives=True).execute()
    return _slim(f)


@api_errors
def search_files(
    name_contains: str | None = None,
    full_text: str | None = None,
    mime_type: str | None = None,
    in_folder: str | None = None,
    page_size: int = 25,
    page_token: str | None = None,
) -> dict:
    """Search Drive by name, full-text content, mime type, and/or parent folder.

    Returns one page; when `has_more` is true, pass the returned `next_page_token` to continue.
    `incomplete_search` true means Drive couldn't search every corpus (results may be partial).
    """
    clauses = ["trashed = false"]
    if name_contains:
        clauses.append(f"name contains '{_esc(name_contains)}'")
    if full_text:
        clauses.append(f"fullText contains '{_esc(full_text)}'")
    if mime_type:
        clauses.append(f"mimeType = '{_esc(mime_type)}'")
    if in_folder:
        clauses.append(f"'{parse_ref(in_folder).id}' in parents")
    q = " and ".join(clauses)
    resp = (
        drive()
        .files()
        .list(
            q=q,
            pageSize=max(1, min(page_size, 100)),
            pageToken=page_token,
            fields=f"nextPageToken, incompleteSearch, files({_FILE_FIELDS})",
            orderBy="modifiedTime desc",
            supportsAllDrives=True,
            includeItemsFromAllDrives=True,
        )
        .execute()
    )
    files = [_slim(f) for f in resp.get("files", [])]
    token = resp.get("nextPageToken")
    return {
        "query": q,
        "count": len(files),
        "files": files,
        "has_more": bool(token),
        "next_page_token": token,
        "incomplete_search": resp.get("incompleteSearch", False),
    }


@api_errors
def list_folder(item: str, page_size: int = 100, page_token: str | None = None) -> dict:
    """List the direct children of a folder (item = folder URL or ID).

    Returns one page; when `has_more` is true, pass the returned `next_page_token` to continue.
    """
    fid = parse_ref(item).id
    resp = (
        drive()
        .files()
        .list(
            q=f"'{fid}' in parents and trashed = false",
            pageSize=max(1, min(page_size, 1000)),
            pageToken=page_token,
            fields=f"nextPageToken, incompleteSearch, files({_FILE_FIELDS})",
            orderBy="folder,name",
            supportsAllDrives=True,
            includeItemsFromAllDrives=True,
        )
        .execute()
    )
    files = [_slim(f) for f in resp.get("files", [])]
    token = resp.get("nextPageToken")
    return {
        "folder_id": fid,
        "count": len(files),
        "files": files,
        "has_more": bool(token),
        "next_page_token": token,
        "incomplete_search": resp.get("incompleteSearch", False),
    }


@api_errors
def get_metadata(item: str) -> dict:
    """Full metadata for a file: owner, timestamps, size, parents, sharing link."""
    ref = parse_ref(item)
    return (
        drive()
        .files()
        .get(
            fileId=ref.id,
            fields=_FILE_FIELDS + ", createdTime, description, shared, trashed",
            supportsAllDrives=True,
        )
        .execute()
    )


_TOOLS = (resolve_link, search_files, list_folder, get_metadata)
