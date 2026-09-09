# gdrive-mcp

A read/write MCP for Google Drive, Docs, Sheets & Calendar that fixes what trips agents on naive Google Workspace servers — context floods, unguarded destructive writes, surprise attendee notifications, and inconsistent argument names — with bounded/paged reads, spill-to-disk for full data, and confirm-gated mutations.

> Conventions: Drive/Docs/Sheets tools take their target as **`item`** (a URL or ID); Calendar tools use explicit `calendar_id`/`event_id` arguments. Unknown args are rejected; destructive tools (ᶜ) return an impact preview unless called with `confirm=true`.
>
> **Dry-run (edit-only):** the edit tools for *existing* files — `append_text`, `insert_text`, `insert_table`, `delete_text`, `replace_text`, `write_sheet`, `append_rows`, `format_cells`, `clear_range`, `delete_rows` — plus all Calendar mutation tools accept `dry_run=true`, returning a predicted before/after **without writing** (client-side; `write_sheet` shows formula cells literally, so Google's recalculation isn't simulated, and Docs `insert_text` reports the insertion point rather than a merged string). Dry-runs are still subject to the verification gate.
>
> **Editing a Doc by content, not by offset:** `delete_text`/`replace_text` (and `insert_text`/`insert_table`'s `after`/`before`) locate their target with a **locator** — `match` (a literal, case-sensitive substring lying within one paragraph) or `section` (a heading's exact text, covering that heading and everything under it up to the next heading of the same or higher level). Character offsets into `read_document`'s content are **not** valid Docs indexes: that content is rendered markdown (heading prefixes, synthesized table delimiter rows, escaped pipes) and does not align with the document's UTF-16 index space. The server resolves locators against the API's own element offsets instead, so anchors are always valid — and for block content they land on a paragraph boundary by construction. Each `read_document` `outline` entry also carries real `start`/`end` offsets, plus a `revision_id`; locator writes pin that revision, so a concurrent edit makes the write **fail** rather than land on shifted text.
>
> **Colored text (opt-in):** `append_text`/`insert_text` take an optional `color` (hex, e.g. `#3366CC`) applied *only when explicitly passed* — unset = plain text. `read_document(include_colors=true)` returns `colored_runs` (spans with an explicit foreground color).
>
> **Formatted writes (opt-in):** `append_text`/`insert_text`/`create_document` accept `markdown=true`, rendering a small dialect — `#`…`######` headings, `-`/`*` bullets, `1.` numbered lists (nest with two spaces or a tab per level), `**bold**`, `*italic*`, `<u>underline</u>`, and **GFM pipe tables** (a header row immediately followed by a `| --- | --- |` delimiter row — a lone `| a | b |` line with no delimiter stays literal text). The only escape is `\|` for a literal pipe **inside a table cell**; otherwise text that looks like markup gets styled (leave `markdown` unset to store text verbatim). Table round-trip preserves structure and plain cell text — cell emphasis and multi-paragraph cells are not preserved on read. In Sheets, `format_cells` sets bold/italic/underline on a cell range without touching values.
>
> **Local-file sandbox:** all local file I/O is confined to `GDRIVE_MCP_FILES_DIR` (default a private `0700` dir under the config dir) — `download_file`/`export_file`/`read_full_sheet` write there (relative `dest_path`; absolute paths and `..` rejected, files `0600`), and `upload_file(source_path)` reads only from there. This stops a prompt-injected agent from writing to `~/.ssh` or exfiltrating arbitrary local files to Drive. Fetched Doc images are pulled only from Google hosts (the OAuth token is never sent elsewhere).
>
> **Spilled-file retention + audit:** files spilled to the sandbox are swept on server start once older than `GDRIVE_MCP_FILES_TTL_HOURS` (default 24; `0` disables) — for a long-lived server, restart to dispose, or lower the TTL. The sweep is ownership-gated: it only runs in a sandbox gdrive-mcp itself created (tracked by a `.gdrive-mcp-sandbox` marker file), so pointing `GDRIVE_MCP_FILES_DIR` at a pre-existing directory never deletes the files already there — a startup warning notes the skipped sweep, and creating the marker file yourself opts the directory in. Every tool call is appended to an audit log (`GDRIVE_MCP_AUDIT_LOG`, default `audit.log` in the config dir) recording user, tool, target id, and outcome — **never** the content read or written.
>
> **Verification gate:** set `GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS` to comma-separated, case-insensitive model-name substrings when selected model families should require per-call user approval via MCP elicitation. A match from either the operator pin `GDRIVE_MCP_CALLING_MODEL` or the request's self-reported `_meta.model` turns the gate on; a nonmatching request model cannot turn off an operator-pin match. Set `GDRIVE_MCP_REQUIRE_VERIFICATION=always` to gate **every** call regardless of model. Gated calls fail closed on decline or when the client cannot prompt. Because request metadata is advisory, enforce mandatory policy with deployment-controlled environment variables and credential boundaries.

## Tools

**Discovery**
- **`resolve_link`** — resolve any Drive/Docs/Sheets URL or ID into its id, kind, name, and link.
- **`search_files`** — find files by name, full-text content, MIME type, and/or parent folder.
- **`list_folder`** — list a folder's direct children.
- **`get_metadata`** — full metadata for a file: owner, timestamps, size, parents, sharing.

**Docs**
- **`read_document`** — read a Doc as markdown/text in bounded ~8k-char chunks (paginated, with an `outline` carrying each heading's `start`/`end` offsets, plus `revision_id`); reads all tabs by default or a specific `tab`.
- **`extract_images`** — pull embedded images (inline **and** positioned/floating) out as viewable images, in document order (tab-aware).
- **`create_document`** — create a new Doc, optionally with initial content (`markdown=true` for formatted).
- **`append_text`** — append text to the end of a Doc, targeting a chosen tab (`markdown=true` for headings/lists/bold/italic/underline).
- **`insert_text`** — insert text `after`/`before` a matched substring (or at a raw `index`), targeting a chosen tab (`markdown=true` as above).
- **`insert_table`** — insert a table filled from a `rows` grid (append, or `after`/`before` a match, or at an `index`; `header=true` bolds the first row).
- **`delete_text`**ᶜ — delete a matched substring or a whole heading `section`.
- **`replace_text`**ᶜ — replace a matched substring or a whole `section` (`markdown=true` for formatted replacements; use `insert_table` for tables).
- **`read_comments`** — list a doc/file's comments and replies.
- **`add_comment`** — add an (unanchored) comment.

**Sheets**
- **`read_sheet`** — a bounded **5×5 preview** of each tab (configurable `max_rows`/`max_cols`; pass `a1_range` for an exact range; `values=FORMULA` for formulas).
- **`read_full_sheet`** — read a whole tab, **save it to a local CSV**, and return the path + exact `total_rows`/`total_cols` + the 5×5 preview.
- **`write_sheet`**ᶜ — write rows starting at a cell; gated when it would overwrite non-empty cells.
- **`append_rows`** — append rows after the last row (non-destructive).
- **`format_cells`** — set bold/italic/underline on a bounded A1 range (tri-state flags; values untouched).
- **`create_spreadsheet`** — create a new spreadsheet with optional named tabs.
- **`add_tab`** — add a tab to an existing spreadsheet.
- **`clear_range`**ᶜ — clear the values in an A1 range.
- **`delete_rows`**ᶜ — delete N rows from a tab.

**Calendar**
- **`list_calendars`** — list subscribed calendars with time zone, access role, and pagination.
- **`list_events`** — list expanded event instances in a bounded time window, with search and pagination.
- **`get_event`** — retrieve one event by calendar ID and API event ID.
- **`query_freebusy`** — retrieve availability-only busy intervals for up to 50 calendars.
- **`create_event`**ᶜ — create timed/all-day or recurring events with optional attendees, reminders, and Google Meet; supports caller-supplied IDs for idempotent retries.
- **`update_event`**ᶜ — partially update an event with a live before/after preview and preview-bound ETag concurrency protection.
- **`delete_event`**ᶜ — delete one event, recurring instance, or recurring series with a live preview and preview-bound ETag protection.
- **`respond_to_event`**ᶜ — accept, tentatively accept, or decline an invitation without changing other attendees' responses, with preview-bound ETag protection.

Calendar writes default to `send_updates=none`; callers must explicitly select `externalOnly` or `all` to email attendees. Every Calendar mutation previews unless called with `confirm=true`, and `dry_run=true` never writes even when confirmed. For update, delete, and RSVP, copy the previewed event `etag` into the confirmed call; if the event changed after preview, the write is refused. Timed values require RFC3339 offsets, recurring timed events also require an IANA `time_zone`, all-day end dates are exclusive, and Google permits at most five reminder overrides.

**Files**
- **`read_file_as_text`** — a file's content as text in bounded chunks (Google-native exported; PDFs text-extracted).
- **`download_file`** — download a binary file (base64 if small, else written to disk).
- **`upload_file`**ᶜ — create a new file, or replace an existing file's content (replace is gated).
- **`move_file`**ᶜ — move a file into a `parent` folder.
- **`rename_file`**ᶜ — rename a file.
- **`export_file`** — export a Google-native file to pdf / docx / xlsx / pptx / csv / txt / md / html.

## Quick install

Requires [`uv`](https://docs.astral.sh/uv/), the [Claude Code CLI](https://docs.claude.com/en/docs/claude-code) (`claude`), and your own Google OAuth client (see [Setup](#setup-one-time-per-google-account)). Then:

```bash
git clone https://github.com/shivankj11/gdrive-mcp.git && cd gdrive-mcp && bash setup.sh
```

`setup.sh` is idempotent — re-run it any time. It installs `uv` (if missing), runs the one-time Google browser consent, and registers the server with Claude Code. It looks for the Desktop-app OAuth client JSON in this order: an existing `~/.config/gdrive-mcp/oauth_client.json`; a `GDRIVE_MCP_OAUTH_CLIENT_CMD` that prints the JSON; or a plaintext `oauth_client.json` at the repo root. If none is found it tells you how to create one.

## Setup (one-time, per Google account)

1. In a Google Cloud project you control, create an **OAuth client** (Application type **Desktop app**) and configure its consent screen. The `drive` scope is restricted, so follow Google's verification requirements (for personal use, add yourself as a test user).
2. Enable the [Drive](https://console.cloud.google.com/apis/library/drive.googleapis.com), [Docs](https://console.cloud.google.com/apis/library/docs.googleapis.com), [Sheets](https://console.cloud.google.com/apis/library/sheets.googleapis.com), and [Calendar](https://console.cloud.google.com/apis/library/calendar-json.googleapis.com) APIs. Drive/Docs/Sheets use the `drive` scope; Calendar uses separate narrow calendar-list, event, and free/busy scopes.
3. Save the client JSON to `~/.config/gdrive-mcp/oauth_client.json` (or set `GDRIVE_MCP_OAUTH_CLIENT`), then:

```bash
uv run gdrive-mcp auth      # one-time loopback-OAuth browser consent; caches a token
uv run gdrive-mcp whoami    # verify
```

An older cached token that predates Calendar support is rejected with a directed re-consent
message. Run `uv run gdrive-mcp auth` again to grant the added narrow Calendar scopes; existing
Drive access is retained through incremental authorization.

You authenticate as **yourself**, so the server can only reach what your own Google account already can.

## Register with an MCP client

```bash
claude mcp add gdrive -- uv run --directory /path/to/gdrive-mcp gdrive-mcp serve
```

## Rust implementation (`rust/`)

`rust/` holds a second, self-contained implementation of the same server — same 37 tools, same
argument names, same result shapes, same confirm/dry-run/locator/gate semantics — as a single
static binary with no Python runtime. The two are interchangeable: they read the **same**
`~/.config/gdrive-mcp/oauth_client.json` and write the **same** `token.json` (byte-compatible with
`google-auth`'s format), so authenticating with one authenticates the other, and every
`GDRIVE_MCP_*` variable in [Configuration](#configuration) means the same thing to both.

```bash
cargo install --path rust        # or: cargo build --release --manifest-path rust/Cargo.toml
gdrive-mcp auth                  # one-time browser consent (skip if the Python side already ran it)
gdrive-mcp whoami                # verify
claude mcp add gdrive -- gdrive-mcp serve
```

Notable differences, all internal:

- **Google APIs are called directly over REST** (`reqwest`) instead of through a discovery
  document, behind a `GoogleApi` trait so the tool tests run against a recording fake.
- **MCP is served by [`rmcp`]**, the official Rust SDK; the verification gate prompts through its
  elicitation support, and `tests/server_integration.rs` drives the registered path with a real
  in-memory MCP client (annotations, strict schemas, and every accept/decline/no-prompt branch).
- **Argument handling reproduces both of FastMCP's layers**, not just the strict one: unknown
  arguments are rejected (`extra="forbid"`), *and* a list sent as a JSON string or a boolean sent
  as `"true"` is coerced the way pydantic's lax mode did, because MCP clients really send those.
- **PDF text extraction** uses `pdf-extract` rather than `pypdf`, so extracted text may differ in
  whitespace for unusual PDFs.
- **The markdown dialect's inline rules need lookaround**, which Rust's default `regex` engine
  does not support, so `md.rs` uses `fancy-regex` for exactly those two patterns — with `\w` and
  `\s` spelled out, since the two engines define them differently.

Known behavioural divergences, both confined to how a spilled filename is spelled: A1 cells accept
only ASCII digits (Python's `\d` also matched other Unicode digits), and `localfs`'s filename
sanitiser keeps Unicode combining marks where Python replaced them with `_`. Neither affects
containment or content.

```bash
cargo test --manifest-path rust/Cargo.toml     # unit + integration tests, no credentials needed
python3 scripts/diff_tool_surface.py           # both servers advertise an identical tool surface
```

[`rmcp`]: https://crates.io/crates/rmcp

See [`VERIFICATION.md`](VERIFICATION.md) for what each implementation's tests actually pin, and for
the live credentialed runs.

## Configuration

| Env var | Default | Purpose |
| --- | --- | --- |
| `GDRIVE_MCP_OAUTH_CLIENT` | `~/.config/gdrive-mcp/oauth_client.json` | Desktop-app OAuth client JSON |
| `GDRIVE_MCP_TOKEN` | `~/.config/gdrive-mcp/token.json` | Cached per-user token |
| `GDRIVE_MCP_FILES_DIR` | `~/.config/gdrive-mcp/files` | Sandbox for local file I/O — download/export/CSV writes and upload sources are confined here (absolute/`..` rejected); swept only if created by gdrive-mcp (`.gdrive-mcp-sandbox` marker) |
| `GDRIVE_MCP_FILES_TTL_HOURS` | `24` | Age (hours) after which spilled sandbox files are swept on server start; `0` disables |
| `XDG_CONFIG_HOME` | `~/.config` | Base config dir |
| `GDRIVE_MCP_CALLING_MODEL` | *(unset)* | Operator-controlled caller-model pin; used when the client does not send `_meta.model` |
| `GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS` | *(unset)* | Comma-separated model-name substrings that require per-call verification |
| `GDRIVE_MCP_REQUIRE_VERIFICATION` | *(unset)* | Set to `always` to gate **every** call regardless of model (anti-delegation hard override) |

Credential files are git-ignored and must never be committed.

## Development

```bash
uv sync            # install deps (incl. dev group)
uv run pytest      # run the test suite
```

## License

MIT — see [LICENSE](LICENSE).
