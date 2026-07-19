"""Drive file tools: read-as-text, download, upload/replace, move, rename, export.

Local file I/O (upload source, download/export destinations) is confined to the sandbox in
`localfs` — a prompt-injected agent cannot read arbitrary local files or write outside it.
`supportsAllDrives=True` is set on every Drive call that accepts it so shared-drive items work
(the `export` method has no such parameter).
"""

from __future__ import annotations

import base64
import io

from googleapiclient.http import MediaFileUpload, MediaIoBaseUpload

from gdrive_mcp.chunking import DEFAULT_MAX_CHARS, paginate
from gdrive_mcp.clients import drive
from gdrive_mcp.errors import api_errors
from gdrive_mcp.guard import preview_response
from gdrive_mcp.ids import parse_ref
from gdrive_mcp.localfs import safe_read_path, safe_write_path

# Above this size download/export write to the sandbox instead of returning base64 inline.
_INLINE_MAX = 5 * 1024 * 1024

_EXPORT_MIME = {
    "pdf": "application/pdf",
    "docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "xlsx": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "pptx": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "csv": "text/csv",
    "txt": "text/plain",
    "md": "text/markdown",
    "html": "text/html",
}

_GOOGLE_EXPORT_TEXT = {
    "application/vnd.google-apps.document": "text/plain",
    "application/vnd.google-apps.spreadsheet": "text/csv",
    "application/vnd.google-apps.presentation": "text/plain",
}


@api_errors
def read_file_as_text(item: str, chunk: int = 0, max_chars: int = DEFAULT_MAX_CHARS) -> dict:
    """Read a file's content as text, one bounded chunk at a time.

    Google-native files are exported; PDFs are text-extracted. Returns the requested chunk
    (0-based) in `content` plus name, mime_type, and paging metadata (total_chunks,
    total_chars, has_more) — page through by incrementing chunk. Set max_chars<=0 for the
    whole document.
    """
    ref = parse_ref(item)
    meta = drive().files().get(fileId=ref.id, fields="id,name,mimeType,size", supportsAllDrives=True).execute()
    mime = meta["mimeType"]
    extra: dict = {}
    if mime in _GOOGLE_EXPORT_TEXT:
        data = drive().files().export(fileId=ref.id, mimeType=_GOOGLE_EXPORT_TEXT[mime]).execute()
        text = data.decode("utf-8", "replace")
    else:
        raw = drive().files().get_media(fileId=ref.id, supportsAllDrives=True).execute()
        if mime == "application/pdf":
            from pypdf import PdfReader

            reader = PdfReader(io.BytesIO(raw))
            text = "\n".join((page.extract_text() or "") for page in reader.pages)
            extra["pages"] = len(reader.pages)
        elif mime.startswith("text/") or mime in ("application/json", "application/csv"):
            text = raw.decode("utf-8", "replace")
        else:
            raise RuntimeError(f"{mime} is not text-extractable; use download_file instead.")
    return {"name": meta["name"], "mime_type": mime, **extra, **paginate(text, chunk, max_chars)}


@api_errors
def download_file(item: str, dest_path: str | None = None) -> dict:
    """Download a binary file. Small (<5MB, no dest_path) → base64 inline; otherwise written to
    the sandbox files dir (dest_path is resolved inside it; absolute/`..` rejected)."""
    ref = parse_ref(item)
    meta = drive().files().get(fileId=ref.id, fields="id,name,mimeType,size", supportsAllDrives=True).execute()
    mime = meta["mimeType"]
    if mime.startswith("application/vnd.google-apps"):
        raise RuntimeError("Google-native file: use export_file or read_file_as_text, not download_file.")
    raw = drive().files().get_media(fileId=ref.id, supportsAllDrives=True).execute()
    if dest_path or len(raw) > _INLINE_MAX:
        path = safe_write_path(dest_path, meta["name"])
        path.write_bytes(raw)
        path.chmod(0o600)
        note = None if dest_path else f"{len(raw)} bytes exceeded {_INLINE_MAX} inline limit; written to file"
        return {"name": meta["name"], "mime_type": mime, "bytes": len(raw), "path": str(path), "note": note}
    return {
        "name": meta["name"],
        "mime_type": mime,
        "bytes": len(raw),
        "base64": base64.b64encode(raw).decode(),
    }


@api_errors
def upload_file(
    name: str,
    source_path: str | None = None,
    content: str | None = None,
    parent: str | None = None,
    mime_type: str | None = None,
    replace_id: str | None = None,
    confirm: bool = False,
) -> dict:
    """Create a new Drive file (from source_path or text content), or replace an existing file's
    content (replace_id — requires confirm=true). `source_path` must be inside the sandbox files dir."""
    if not source_path and content is None:
        raise RuntimeError("provide source_path or content")
    if source_path:
        media = MediaFileUpload(str(safe_read_path(source_path)), mimetype=mime_type, resumable=False)
    else:
        media = MediaIoBaseUpload(io.BytesIO(content.encode()), mimetype=mime_type or "text/plain")
    if replace_id:
        rid = parse_ref(replace_id).id
        if not confirm:
            old = drive().files().get(fileId=rid, fields="id,name,mimeType,modifiedTime", supportsAllDrives=True).execute()
            return preview_response("upload_file(replace)", {"replacing": old, "with_name": name})
        f = (
            drive()
            .files()
            .update(fileId=rid, media_body=media, body={"name": name}, fields="id,name,webViewLink", supportsAllDrives=True)
            .execute()
        )
        return {"id": f["id"], "name": f["name"], "url": f.get("webViewLink"), "replaced": True}
    body: dict = {"name": name}
    if parent:
        body["parents"] = [parse_ref(parent).id]
    f = drive().files().create(body=body, media_body=media, fields="id,name,webViewLink", supportsAllDrives=True).execute()
    return {"id": f["id"], "name": f["name"], "url": f.get("webViewLink")}


@api_errors
def move_file(item: str, parent: str, confirm: bool = False) -> dict:
    """Move a file (item) into a different folder (parent). Requires confirm=true."""
    ref = parse_ref(item)
    dest = parse_ref(parent).id
    meta = drive().files().get(fileId=ref.id, fields="id,name,parents", supportsAllDrives=True).execute()
    if not confirm:
        return preview_response("move_file", {"file": meta["name"], "from": meta.get("parents"), "to": dest})
    f = (
        drive()
        .files()
        .update(
            fileId=ref.id,
            addParents=dest,
            removeParents=",".join(meta.get("parents", [])),
            fields="id,name,parents",
            supportsAllDrives=True,
        )
        .execute()
    )
    return {"id": f["id"], "name": f["name"], "parents": f.get("parents")}


@api_errors
def rename_file(item: str, new_name: str, confirm: bool = False) -> dict:
    """Rename a file. Requires confirm=true."""
    ref = parse_ref(item)
    meta = drive().files().get(fileId=ref.id, fields="id,name", supportsAllDrives=True).execute()
    if not confirm:
        return preview_response("rename_file", {"from": meta["name"], "to": new_name})
    f = drive().files().update(fileId=ref.id, body={"name": new_name}, fields="id,name", supportsAllDrives=True).execute()
    return {"id": f["id"], "name": f["name"]}


@api_errors
def export_file(item: str, to: str, dest_path: str | None = None) -> dict:
    """Export a Google-native file to a format: pdf, docx, xlsx, pptx, csv, txt, md, html.

    Written to the sandbox files dir when large or dest_path is given (dest_path resolved inside
    it); small exports return base64 inline. (Drive's export method has no supportsAllDrives param.)
    """
    ref = parse_ref(item)
    target = _EXPORT_MIME.get(to.lower())
    if not target:
        raise RuntimeError(f"unsupported export format {to!r}; choose from {sorted(_EXPORT_MIME)}")
    data = drive().files().export(fileId=ref.id, mimeType=target).execute()
    if dest_path or len(data) > _INLINE_MAX:
        path = safe_write_path(dest_path, f"{ref.id}.{to.lower()}")
        path.write_bytes(data)
        path.chmod(0o600)
        return {"format": to, "bytes": len(data), "path": str(path)}
    return {"format": to, "bytes": len(data), "base64": base64.b64encode(data).decode()}


_TOOLS = (
    read_file_as_text,
    download_file,
    upload_file,
    move_file,
    rename_file,
    export_file,
)
