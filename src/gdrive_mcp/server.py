"""FastMCP server wiring all tool modules onto one stdio server."""

from __future__ import annotations

from mcp.server.fastmcp import FastMCP
from mcp.server.fastmcp.utilities.func_metadata import ArgModelBase
from mcp.types import ToolAnnotations

from gdrive_mcp import localfs
from gdrive_mcp.gating import gated
from gdrive_mcp.tools import calendar, discovery, docs, files, sheets

# Tools that only read (never change Drive or local state).
_READ_ONLY = {
    "resolve_link", "search_files", "list_folder", "get_metadata", "read_sheet",
    "read_document", "extract_images", "read_comments", "read_file_as_text",
    "list_calendars", "list_events", "get_event", "query_freebusy",
}
# Note: read_full_sheet is NOT read-only — it spills a local CSV (a local side effect).
# Tools that can overwrite/remove existing data.
_DESTRUCTIVE = {
    "write_sheet", "clear_range", "delete_rows", "move_file", "rename_file", "upload_file",
    "delete_text", "replace_text",
    "create_event", "update_event", "delete_event", "respond_to_event",
}
# Everything else is additive (append/create/add, or a read that spills a new local file).


def _annotations(name: str) -> ToolAnnotations:
    if name in _READ_ONLY:
        return ToolAnnotations(readOnlyHint=True, openWorldHint=True)
    if name in _DESTRUCTIVE:
        return ToolAnnotations(readOnlyHint=False, destructiveHint=True, openWorldHint=True)
    return ToolAnnotations(readOnlyHint=False, destructiveHint=False, openWorldHint=True)


def build_server() -> FastMCP:
    # Reject unknown tool arguments instead of silently dropping them (pydantic defaults to
    # "ignore"). Every tool's argument model is generated from this shared base at registration
    # time, so this must be set before tools register below.
    ArgModelBase.model_config["extra"] = "forbid"
    localfs.sweep_expired()  # dispose of files spilled to the sandbox beyond the retention TTL
    mcp = FastMCP("gdrive")
    for module in (discovery, sheets, docs, files, calendar):
        for fn in module._TOOLS:
            mcp.tool(annotations=_annotations(fn.__name__))(gated(fn))
    return mcp


def run() -> None:
    build_server().run(transport="stdio")
