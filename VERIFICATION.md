# Verification

What this server claims, what pins each claim, and what nothing pins.

This is the single source of truth for verification across the whole server. It replaces three
earlier planning documents, not published here — one for Docs table writes, one for locator-based
Docs writes, one for Sheets/Files/Discovery — and reorganises their contents by **functionality**
rather than by the order the work happened. Their check numbering does not carry over; this document
renumbers per section (`DSC-1`, `LOC-14`, `SHT-31`, …) so a check can be cited without knowing which
of them it came from. The live checks keep their original labels (`L1`–`L10` for Docs, `S1`–`S11`
for Sheets) because those labels appear in the harnesses' own assertion messages.

Everything dated is dated **2026-08-07** unless it says otherwise.

---

## 1. What this covers, and the two-implementation situation

There are two implementations of the same MCP server, and they are meant to be interchangeable:

| | Source | Tests | Status |
|---|---|---|---|
| **Python** | `src/gdrive_mcp/**` | `tests/**` | the behaviour of record |
| **Rust** | `rust/src/**` | `rust/src/**` (`mod tests`) + `rust/tests/**` | a port: same 37 tools, same argument names, same result shapes, same confirm / dry-run / locator / gate semantics |

They read the same OAuth client and write the same token file, so authenticating one authenticates
the other. That is convenient and it is also a hazard: it means a Rust defect is reachable by
anyone who set the server up through the Python instructions, with no separate credential step to
act as a speed bump.

A port inherits none of the original's evidence. The offsets are computed by different arithmetic,
the requests are built by different code, and the same misunderstanding of an API can be made
twice — independently, in both languages, by the same author. So the Rust side is verified check by
check against the Python behaviour rather than "the ported tests pass". Where a claim holds in one
language and nothing asserts it in the other, §4 says so explicitly. **That asymmetry is the most
useful thing in this document**: it is the list of places where "the two are interchangeable" rests
on inspection rather than on a test.

### Which parts of this were written before the code, and which after

A plan that pretends it drove the implementation is worth less than one that admits it did not.

- **Docs table writes (§4.6)** — written **before** the code, as a design plan with its checks
  enumerated up front, and reviewed adversarially before implementation. It carries the empirical
  API grounding in §6, which was gathered against a live Doc *while designing*, and which is why the
  design is shaped the way it is.
- **Locator-based Docs writes (§4.5)** — written **before** the code, with an explicit
  regression contract: run the full suite, and exactly two pre-existing assertions may change (the
  tool count, and one exact-dict outline assertion); a third would be a defect signal. That
  contract held exactly as written — the first run after the change failed on precisely those two
  assertions and nothing else.
- **Sheets, Files, Discovery (§§4.1, 4.7, 4.8)** — **retrospective**. The code in both languages
  was written first; these checks were derived afterwards from the Python tools' docstrings and
  signatures, from the existing tests, and from the guarantees the README makes. §§4.1/4.7/4.8 are
  therefore a *description of coverage that already existed*, audited claim by claim, with the
  gaps named — plus the live Sheets run, which is the one part that was genuinely new work.
- **The Rust re-verification of everything** — **retrospective** in the same sense. The port was
  written, then every check in this document was traced to the specific Rust test that fails if the
  behaviour regresses. Four locator checks turned out to be unpinned; one production defect turned
  up.

### Current state

| | Measured |
|---|---|
| Rust | `cargo test` on a Unix host: **420 passed, 3 ignored** (409 library tests + 8 server integration tests + 3 credential-free live-harness companions; the 3 ignored are the live harnesses). A few sandbox checks are `#[cfg(unix)]`, so a Windows run counts fewer. `cargo clippy --all-targets` and `cargo fmt --check` clean. |
| Python | `uv run pytest -q`: **249 passed, 11 skipped** (the skipped tests are the live locator and Calendar harnesses, latched off). |
| Cross-implementation | `scripts/diff_tool_surface.py`: green at **37/37** tools. |
| Live credentialed | Docs `L1`–`L10`: Python 2026-08-06 (10 passed); Rust 2026-08-07 (10 passed, 11.6 s). Sheets `S1`–`S11`: Rust 2026-08-07 (11 passed, 15.3 s); **Python has never had a Sheets live run.** Calendar `C1`–`C12`: Python 2026-09-08 (2 tests passed, 9.48 s); Rust 2026-09-08 (1 live test passed, 7.08 s). |

---

## 2. What the server guarantees, and therefore what has to be verified

Every guarantee below is a promise made to an agent, in the README's Conventions block or in a
tool's docstring — which *is* the agent-facing schema. Each one generates verification obligations,
listed with the sections that discharge them.

| Guarantee | What it means | Verified in |
|---|---|---|
| **Consistent target arguments** | Drive/Docs/Sheets tools take their target as `item`, a URL or bare id parsed by one resolver. Calendar uses explicit `calendar_id` and `event_id`, matching Google's two-part resource identity. | `DSC-1`–`DSC-6`, `CAL-2`–`CAL-6` |
| **The confirm gate** | The tools that can destroy data return `status: "confirmation_required"` plus an impact summary instead of writing, unless called with `confirm=true`. Additive tools are deliberately *not* gated. | `LOC-17`–`LOC-19`, `SHT-14`/`20`/`21`/`23`, `FIL-1`–`FIL-6`, `SRF-2`–`SRF-5` |
| **Dry-run predicts without writing** | Fourteen edit tools accept `dry_run=true` and return a predicted before/after with no API mutation. Dry-run is the *explore* affordance; confirm is the *gate*. Both exist, and a dry run must not present itself as a confirmation prompt. | `DW-1`, `LOC-20`, `TBL-7`/`18`, `SHT-38`–`41`/`45`, `CAL-8`/`12`/`15`/`17`, `SRF-5` |
| **Calendar writes do not surprise attendees** | Every Calendar mutation defaults to `send_updates=none`; notification-bearing values are explicit and validated. Caller-supplied event ids make creates safely retryable. | `CAL-8`–`CAL-18` |
| **Calendar writes reject stale previews** | Update, delete and RSVP read an ETag and send it as `If-Match`; HTTP 412 becomes a directed re-read-and-retry error. | `CAL-13`, `CAL-16`, `CAL-18`, live `C7` |
| **Locator, not offset, addressing** | Docs edits locate their target by content (`match`, `section`, `after`, `before`), resolved against the API's own element offsets. Character offsets into `read_document`'s output are *not* valid Docs indexes — that output is rendered markdown, and Docs indexes are UTF-16 code units. | `LOC-1`–`LOC-16`, `LOC-30`–`LOC-36` |
| **Revision pinning on Docs writes** | Locator writes send `writeControl: {requiredRevisionId}`, so a concurrent edit makes the write **fail** rather than land on shifted text. | `LOC-23`, live `L2` |
| **RAW by default** | `write_sheet` / `append_rows` default to `value_input="RAW"`, so a leading `=` in agent-supplied text is stored as literal text, not as a live formula. This is a formula-injection guard: the values an agent writes may have come from a document it just read. | `SHT-16`, `SHT-18`, `SHT-19`, live `S1`/`S2` |
| **The local-file sandbox** | All local file I/O is confined to `GDRIVE_MCP_FILES_DIR`. Spills reject absolute paths and `..` and land `0600`; upload sources read only from the same directory. This is what stops a prompt-injected agent writing to a private key directory or exfiltrating a local file to Drive. | `SBX-1`–`SBX-5`, `SHT-36`/`37`, `FIL-15`/`24`, live `S7` |
| **Spilled data at rest is disposed of** | Files spilled to the sandbox are swept on server start once older than `GDRIVE_MCP_FILES_TTL_HOURS`. The sweep is ownership-gated: it only runs in a directory gdrive-mcp itself created (or that an operator explicitly marked), so pointing the variable at a pre-existing directory never deletes what was already there. | `SBX-5`, `SBX-6` |
| **A content-free audit log** | Every call is appended to a log recording user, tool, target id and outcome — never document content. The safe-argument list is an **allowlist**, so content args are excluded by construction; the verification obligation is to pin that, because widening the allowlist would quietly start writing document content to disk. | `AUD-1`–`AUD-5` |
| **The verification gate** | Configured model families require per-call user approval through MCP elicitation. Gated calls fail **closed** on decline, on accept-without-approval, and when the client cannot prompt at all. | `GAT-1`–`GAT-7` |
| **Argument strictness** | Unknown arguments are rejected, not silently dropped — so a typo'd `confirm` cannot read as permission. Both gate flags default to `false`. | `SRF-6`–`SRF-9` |
| **Bounded reads** | Reads are paged in ~8 000-character chunks; sheet reads preview 5×5; whole-tab and whole-file reads spill to disk rather than flooding the context. | `DR-13`, `SHT-29`/`32`, `FIL-7`/`8`/`12`/`14`/`16` |
| **Images are fetched only from Google hosts** | The OAuth token is never sent to a non-Google host. | `DR-11` |
| **Interchangeability** | The surface an agent sees — names, descriptions, argument names and order, required sets, defaults — is identical between the two implementations. | `SRF-11` |

---

## 3. The verification layers

Defined once here and referenced throughout. A check is only as strong as its layer.

**Layer 1 — pure logic.** No API, no fake, no network. Synthetic inputs, asserted outputs. This is
where the index arithmetic, the A1 quoting, the markdown dialect, the chunker, the reference parser
and the sandbox path resolution live. It carries a disproportionate share of the correctness burden
because it is the only place the arithmetic is falsifiable without a live document, and because a
quoting bug here mis-targets a destructive write. `localfs` is layer 1 despite doing real
filesystem work: it runs against real temporary directories, real symlinks and a real
case-insensitive filesystem, because a containment check against a mocked filesystem verifies the
mock.

**Layer 2 — tool logic against a fake API.** The tool function is driven end to end with the HTTP
client replaced. Rust uses `tools::testing::FakeApi` behind the `GoogleApi` trait: it queues a
response per method name and **records every request**, so assertions are on the exact request body
that would have gone out. Python uses a `MagicMock` standing in for the discovery-built service,
and asserts on `call_args`. This layer proves request *shape*, gate behaviour, dry-run
no-write, validation ordering, and result *shape*. It cannot prove anything about how Google
behaves, because a fake agrees with the implementation by construction.

Rust adds a sub-layer with no Python analogue: **request construction** (`clients.rs`), where the
URL, query parameters and percent-encoding are built by hand. In Python `googleapiclient` does
that, so there is nothing local to test — which is exactly why the one defect in §7 was possible.

**Layer 3 — the registered MCP path.** The tool is called through a real MCP client over an
in-memory transport, so the assertions cover schema generation, annotations, argument validation and
the gate's elicitation round trip — everything between the client and the tool function. Rust:
`rust/tests/server_integration.rs` against `rmcp`. Python:
`tests/test_server_integration.py` against FastMCP.

**Layer 4 — live credentialed.** Real API, real Workspace resource, real credentials. This is the
only layer that can falsify a claim about Google's own behaviour. Every harness is double-latched —
the Rust ones use `#[ignore]` plus a `GDRIVE_MCP_LIVE` environment check, and the Python ones use a
`skipif` — so an ordinary test run can never reach Google. Each uses one disposable scratch
resource with synthetic content and cleans it up on teardown. §5 has the check tables.

---

## 4. Verification by functional area

Each table lists a claim, the test that fails if the behaviour regresses, and in which
implementation. **`—` in the Python column means the claim holds in the Python implementation *by
inspection* — nothing in `tests/` asserts it.** Rust test names are given relative to their module
(`a1::tests::`, `tools::sheets::tests::`, …); Python names relative to `tests/`.

### 4.1 Discovery and reference resolution — 20 checks

`resolve_link`, `search_files`, `list_folder`, `get_metadata`, plus the `ids` resolver every tool
depends on. All four tools are reads. They are in scope because they are the only tools that touch
a Drive item of an unmodelled type (a presentation, a PDF), and because their query construction
interpolates caller strings into Drive's query language.

| # | Claim | Rust | Python |
|---|---|---|---|
| DSC-1 | A Docs / Sheets / Drive-folder / Drive-file / `open?id=` URL yields its id and kind; a bare id parses with kind `unknown`; prose raises | `ids::tests::a_document_url_yields_the_document_id_and_kind`, `a_spreadsheet_url_with_a_gid_fragment_yields_only_the_file_id`, `a_folder_url_is_kind_folder`, `a_file_url_is_kind_file`, `an_open_query_url_yields_the_id_query_param`, `a_bare_id_has_no_kind_hint`, `prose_is_not_an_id` | `test_ids.py::test_document_url`, `test_spreadsheet_url_with_gid`, `test_folder_url`, `test_file_url`, `test_open_id_url`, `test_bare_id`, `test_unparseable` |
| DSC-2 | A Slides URL parses to kind `"file"` — there is no Slides model (see §8) | `ids::tests::a_presentation_url_is_a_plain_drive_file` | — |
| DSC-3 | A bare id must match the **whole** string and be at least 20 characters; surrounding whitespace is ignored | `ids::tests::a_bare_id_must_match_the_whole_string`, `a_bare_id_shorter_than_twenty_characters_is_rejected`, `surrounding_whitespace_is_ignored` | — |
| DSC-4 | Id capture stops at the next path segment; an earlier pattern beats a later one on the same URL; the pattern table compiles in priority order | `ids::tests::a_url_id_stops_at_the_next_path_segment`, `an_earlier_pattern_beats_a_later_one_on_the_same_url`, `the_pattern_table_compiles_in_priority_order` | — |
| DSC-5 | A `tab=` parameter is read from either the query or the fragment, its capture stops at the first non-alphanumeric, it needs a `?`/`#` before it, and its absence yields no tab | `ids::tests::a_tab_id_is_read_from_query_or_fragment`, `a_tab_id_capture_stops_at_the_first_non_alphanumeric`, `a_tab_param_needs_a_query_or_fragment_delimiter_before_it`, `a_url_without_a_tab_param_has_no_tab` | — |
| DSC-6 | The unparseable-reference error echoes the argument exactly as given, so a caller can see what was rejected | `ids::tests::the_error_echoes_the_argument_exactly_as_given` | `test_ids.py::test_unparseable` (raises; message not asserted) |
| DSC-7 | `resolve_link` slims Drive's response to the agent-facing shape (`id`/`name`/`mime_type`/`kind`/`modified`/`size`/`web_view_link`) | `tools::discovery::tests::resolve_link_slims_the_drive_response_to_the_agent_facing_shape` | — |
| DSC-8 | The id is parsed out of a URL **before** the call, and the file field mask survives intact (including `owners(displayName,emailAddress)` across the source's line continuation) | `tools::discovery::tests::resolve_link_sends_the_id_parsed_out_of_a_url_and_the_file_field_mask` | — |
| DSC-9 | `kind` maps spreadsheet / document / folder and reports **everything else as `"file"`** — including a Google Slides presentation | `tools::discovery::tests::a_mime_type_outside_the_table_is_reported_as_a_plain_file` | — |
| DSC-10 | Fields Drive omits come back as JSON nulls rather than missing keys, and a response with no `mimeType` at all still classifies | `tools::discovery::tests::fields_drive_omitted_come_back_as_nulls_rather_than_missing_keys` | — |
| DSC-11 | A single quote in a search term is escaped, and the **backslash is doubled first** — quote-first would produce `a\\\\'b` and close the query literal early | `tools::discovery::tests::search_files_escapes_a_single_quote_in_a_name_clause`, `escaping_doubles_the_backslash_before_it_escapes_the_quote` | `test_tools_unit.py::test_search_query_and_escaping` (quote only; **no backslash case**) |
| DSC-12 | Filters are joined in signature order; no filters yields only `trashed = false`; an empty filter string adds no clause | `tools::discovery::tests::search_files_joins_every_requested_filter_in_signature_order`, `search_files_with_no_filters_only_excludes_the_trash`, `an_empty_filter_string_adds_no_clause` | `test_search_query_and_escaping` (partial) |
| DSC-13 | `in_folder` is resolved to an id before it becomes a `parents` clause, and an unparseable one fails before any Drive call | `tools::discovery::tests::in_folder_is_resolved_to_an_id_before_it_becomes_a_parents_clause`, `an_unparseable_in_folder_fails_before_any_drive_call` | — |
| DSC-14 | Pagination signals are surfaced (`has_more` / `next_page_token` / `incomplete_search`) with `pageToken` forwarded; a **missing** token and an **empty-string** token both mean no more pages | `tools::discovery::tests::search_files_surfaces_pagination_signals`, `list_folder_reports_no_more_pages_when_drive_returns_no_token`, `an_empty_next_page_token_is_not_another_page` | `test_search_files_surfaces_pagination_signals`, `test_list_folder_no_more_pages` |
| DSC-15 | `page_size` is clamped into Drive's accepted range — 1–100 for search, 1–1000 for `list_folder` — and the defaults are the Python's (25 / 100) | `tools::discovery::tests::search_files_clamps_page_size_into_drives_accepted_range`, `list_folder_allows_ten_times_the_page_size_search_does`, `the_default_page_sizes_are_the_pythons` | — |
| DSC-16 | Ordering is fixed per tool: search by `modifiedTime desc`, `list_folder` by `folder,name` | `tools::discovery::tests::search_orders_by_recency_and_list_folder_puts_folders_first` | — |
| DSC-17 | Every listed file is slimmed and counted, and `list_folder` queries the parsed folder id while hiding trash | `tools::discovery::tests::each_listed_file_is_slimmed_and_counted`, `list_folder_queries_the_parsed_folder_id_and_hides_trash` | — |
| DSC-18 | `get_metadata` returns Drive's response **verbatim** (not slimmed) and asks for the sharing/lifecycle fields too | `tools::discovery::tests::get_metadata_returns_drives_response_verbatim`, `get_metadata_asks_for_the_sharing_and_lifecycle_fields_too` | — |
| DSC-19 | An unparseable or missing `item` is an argument error, raised before any Drive call | `tools::discovery::tests::an_unparseable_item_fails_before_any_drive_call`, `a_missing_item_is_an_argument_error_not_a_drive_call` | — |
| DSC-20 | Discovery's schemas and descriptions are the Python signatures and docstrings verbatim, declared in the Python module's order | `tools::discovery::tests::every_schema_matches_the_python_signature`, `each_description_is_the_python_docstring_verbatim`, `the_tools_are_declared_in_the_python_modules_order` | n/a — Python *is* the source; see `SRF-11` |

**Asymmetry.** Neither `resolve_link` nor `get_metadata` has a Python tool-level test. Python drives
`resolve_link` only through the registered path, and only to exercise the gate and argument
strictness (`test_server_integration.py`). Its shape, its field mask and its `kind` mapping are
pinned in Rust only.

### 4.2 Docs reads — 13 checks

`read_document`, `extract_images`, `read_comments`, `add_comment`, and the rendering and chunking
they rest on.

| # | Claim | Rust | Python |
|---|---|---|---|
| DR-1 | The markdown rendering prefixes headings, escapes `\|` inside table cells, and synthesises a **column-matched** delimiter row (a mismatched count is invalid GFM and would not re-parse) | `tools::docs::tests::markdown_rendering_prefixes_headings_and_re_parseable_tables`, `md::tests::render_table_markdown_emits_a_column_matched_delimiter_row`, `escaping_a_cell_flattens_newlines_and_hides_pipes` | `test_tools_unit.py::test_content_to_markdown_headings_and_table`, `test_tables.py::test_render_table_markdown_shape` |
| DR-2 | An escaped pipe emitted by the reader re-parses to the same table — this closes the reader→writer loop, so a cell's own text cannot forge extra columns or a fake delimiter row | `md::tests::an_escaped_pipe_round_trips_through_the_reader_format` | `test_tables.py::test_escaped_pipe_round_trips_through_reader_format` |
| DR-3 | The plain-text rendering **skips** tables while the locator resolver **descends** into them. Asserted as an intended asymmetry so it is a decision, not an accident | `tools::docs::tests::text_rendering_skips_tables_that_a_locator_can_still_reach` | `test_locate.py::test_resolver_descends_into_tables_while_content_to_text_skips_them` |
| DR-4 | A multi-paragraph table cell is flattened onto one markdown row (known lossiness, §8) | `tools::docs::tests::a_multi_paragraph_cell_is_flattened_onto_one_row` | — |
| DR-5 | Tabs flatten depth-first with their children, and a flattened tab carries its inline **and** positioned objects | `tools::docs::tests::tabs_flatten_depth_first_with_their_children`, `a_flattened_tab_carries_its_inline_and_positioned_objects` | `test_tools_unit.py::test_flatten_tabs_depth_first_with_children`, `test_flatten_tabs_carries_positioned_objects` |
| DR-6 | Reading all tabs tags every outline entry with its tab; a single requested tab is read without a title prefix; an unknown tab lists the ones that exist | `tools::docs::tests::every_tab_is_read_and_its_outline_entries_are_tagged_with_it`, `a_single_requested_tab_is_read_without_a_title_prefix`, `reading_an_unknown_tab_lists_the_ones_that_exist` | — |
| DR-7 | Write-tab resolution prefers `tabs` and falls back to a top-level `body`; a tabbed doc defaults to the first tab and names the ones it has; an **empty** `tab` argument still honours a tab named in the `item` URL | `tools::docs::tests::a_tabbed_doc_defaults_to_the_first_tab_and_names_the_ones_it_has`, `a_tabless_doc_writes_to_its_top_level_body`, `an_empty_tab_argument_still_honours_the_tab_in_the_item_url` | `test_tools_unit.py::test_resolve_write_tab` (tabbed/tabless only) |
| DR-8 | `colored_runs` reports only the runs carrying an explicit foreground colour, and is absent unless asked for | `tools::docs::tests::colored_runs_reports_only_the_runs_carrying_a_color`, `read_document_returns_colored_runs_only_when_asked` | `test_colored_runs_extracts_only_colored`, `test_read_document_include_colors` |
| DR-9 | Hex colours round-trip and colour **names** are rejected; each channel rounds half-to-even the way Python's `round` does | `tools::docs::tests::hex_colors_round_trip_and_reject_names`, `a_channel_rounds_half_to_even_the_way_python_does` | `test_hex_color_helpers_roundtrip_and_validation` (no rounding case) |
| DR-10 | Image URIs come back in document order, and a positioned (floating) image is emitted before its paragraph's inline images | `tools::docs::tests::image_uris_come_back_in_document_order`, `a_positioned_image_is_emitted_before_its_paragraphs_inline_images` | `test_image_uris_in_document_order`, `test_image_uris_includes_positioned_objects` |
| DR-11 | Only Google-owned hosts may receive the OAuth token, and a non-Google image host is never fetched at all | `tools::docs::tests::only_google_owned_hosts_may_receive_the_oauth_token`, `a_non_google_image_host_is_never_fetched` | `test_is_google_host` — the **host predicate only**; nothing drives `extract_images` |
| DR-12 | `read_comments` clamps its page size and surfaces the next token; a last page reports no more; `add_comment` returns the new comment id | `tools::docs::tests::read_comments_clamps_the_page_size_and_surfaces_the_next_token`, `a_last_page_of_comments_reports_no_more`, `adding_a_comment_returns_the_new_comment_id` | — **no Python test touches either tool** |
| DR-13 | Bounded reads: `paginate` returns exactly the documented keys and paging metadata, clamps an out-of-range chunk and says which it served, clamps a negative chunk to the first, counts **characters not bytes**, never exceeds the budget, hard-splits an over-budget paragraph, rejoins losslessly on blank lines, counts the rejoining separator against the budget, and treats `max_chars <= 0` as "no chunking" | `chunking::tests::` — 16 tests, incl. `paginate_returns_exactly_the_documented_keys`, `an_out_of_range_chunk_clamps_to_the_last_and_says_which_it_served`, `a_hard_split_measures_characters_not_bytes`, `packing_on_blank_lines_rejoins_losslessly`, `the_rejoining_separator_counts_against_the_budget`, `a_negative_max_chars_also_disables_chunking` | `test_chunking.py` — 7 tests: `test_paginate_metadata_and_clamping`, `test_no_chunk_exceeds_budget`, `test_oversized_paragraph_is_hard_split`, `test_paragraph_input_reconstructs`, `test_small_text_is_one_chunk`, `test_empty_text`, `test_max_chars_zero_disables_chunking` |

`DR-13` is load-bearing in a way that is easy to miss: `paginate` counts Python **code points**,
while the locator resolver counts **UTF-16 code units** (§4.5). The two must not converge. The
chunking suite is what stops the resolver's arithmetic leaking into the reader.

### 4.3 Docs writes: plain and formatted text — 9 checks

`append_text`, `insert_text`, `create_document`, and the markdown dialect (`md`). Tables are
§4.6; locator addressing is §4.5.

| # | Claim | Rust | Python |
|---|---|---|---|
| DW-1 | `append_text` and `insert_text` with `dry_run=true` predict the result and call no `batchUpdate` | `tools::docs::tests::append_text_dry_run_predicts_the_tail_and_writes_nothing`, `insert_text_dry_run_reports_the_payload_without_writing`, `a_markdown_dry_run_strips_the_markers_in_its_prediction` | `test_append_text_dry_run_does_not_write`, `test_insert_text_dry_run_does_not_write`, `test_append_text_markdown_dry_run_does_not_write` |
| DW-2 | Block markdown appended after a non-empty tail starts a fresh paragraph; an already-empty tail needs no extra newline; inline-only markdown merges into the tail instead | `tools::docs::tests::block_markdown_appended_after_a_non_empty_tail_starts_a_new_paragraph`, `an_already_empty_tail_paragraph_needs_no_extra_newline`, `inline_only_markdown_merges_into_the_tail_paragraph` | `test_append_text_markdown_blocks_start_fresh_paragraph`, `test_append_text_markdown_no_fresh_paragraph_when_tail_empty`, `test_append_text_markdown_inline_only_merges_into_tail` |
| DW-3 | Text is coloured **only** when a colour is explicitly passed, and a malformed colour is rejected before a dry run can report success | `tools::docs::tests::text_is_colored_only_when_a_color_is_given`, `a_bad_color_is_rejected_before_a_dry_run_reports_anything` | `test_append_text_colors_only_when_requested` (no reject-before-dry-run case) |
| DW-4 | `create_document` with no text writes nothing; with `markdown=true` it styles from index 1 | `tools::docs::tests::creating_a_document_without_text_writes_nothing`, `creating_a_document_with_markdown_styles_it_from_index_one` | `test_create_document_plain_and_empty`, `test_create_document_with_markdown_content` |
| DW-5 | Only one to six hashes open a heading; headings and numbered lists carry the right preset and `fields` mask; a heading line owns its trailing newline | `md::tests::only_one_to_six_hashes_open_a_heading`, `headings_and_numbered_lists_carry_their_preset_and_fields`, `a_heading_line_owns_its_trailing_newline` | `test_md.py::test_heading_levels`, `test_style_requests_numbered_preset_and_heading_fields`, `test_heading_and_body` |
| DW-6 | Inline markers are stripped and their spans recorded in **UTF-16 units**; nested emphasis pushes the outer span before the inner; markup lookalikes stay literal; a long line is styled to its end | `md::tests::inline_markers_are_stripped_and_their_spans_recorded`, `inline_offsets_count_an_emoji_as_two_units`, `nested_emphasis_pushes_the_outer_span_before_the_inner_one`, `markup_lookalikes_stay_literal`, `a_long_line_is_still_styled_to_its_end`, `u16len_counts_utf16_units_not_chars` | `test_inline_styles_and_offsets`, `test_inline_offsets_are_utf16`, `test_inline_nesting_and_bold_italic`, `test_markup_lookalikes_stay_literal`, `test_u16len_counts_utf16_units` |
| DW-7 | The Rust inline dialect matches the Python regexes case for case, with `\w` and `\s` spelled out because the two engines define them differently | `md::tests::the_inline_dialect_matches_the_python_regex_case_for_case`, `the_word_and_space_classes_follow_python_not_rust` | n/a — Python is the reference the Rust test encodes |
| DW-8 | A bullet run and a numbered run never merge; a blank line splits list runs but is still normalised; two spaces or a tab is one nesting level, saturating at eight | `md::tests::a_bullet_run_and_a_numbered_run_never_merge`, `a_blank_line_splits_list_runs_but_is_still_normalized`, `two_spaces_of_indent_become_one_nesting_tab`, `nesting_saturates_at_eight_levels` | `test_numbered_list_and_mixed_runs_split`, `test_blank_line_splits_list_runs`, `test_bullets_nest_via_tabs` |
| DW-9 | Request ordering within one batch: every text-style request precedes the paragraph requests; bullet creation comes last and bottom-up; bullet deletion covers every non-list run; inline-only markdown never touches paragraphs; a trailing newline makes no empty final run; the style-tuple order survives into the `fields` string; an empty `tabId` adds no key | `md::tests::every_text_style_request_precedes_the_paragraph_requests`, `bullet_creation_comes_last_and_bottom_up`, `deleting_bullets_covers_every_non_list_run`, `inline_only_markdown_never_touches_paragraphs`, `a_trailing_newline_makes_no_empty_final_run`, `a_style_tuples_order_survives_into_the_fields_string`, `an_empty_tab_id_adds_no_tab_id_key` | `test_style_requests_order_offsets_and_tab`, `test_style_requests_inline_only_never_touches_paragraphs`, `test_trailing_newline_makes_no_empty_run` |

Ordering (`DW-9`) is not cosmetic. `createParagraphBullets` consumes the leading tabs that encode
nesting, which shifts every index after it; emitting it last, bottom-up, is what keeps the style
ranges computed against the pre-bullet text valid.

### 4.4 The locator resolver — 16 checks

`locate` is pure index arithmetic over a document body. It is the single point where a
content-addressed edit becomes a numeric range, so a bug here is a wrong-range delete.

| # | Claim | Rust (`locate::tests::`) | Python (`test_locate.py::`) |
|---|---|---|---|
| LOC-1 | A single-run ASCII match returns the range the run's own `startIndex` implies | `a_single_run_match_uses_the_runs_own_start_index` | `test_single_run_match_uses_the_runs_own_start_index` |
| LOC-2 | **An astral character before the needle resolves in UTF-16 units.** Counting code points here is off by one; this check is the entire reason `locate` exists as its own module | `an_astral_char_before_the_needle_resolves_in_utf16_units` | `test_astral_char_before_needle_resolves_in_utf16_units` |
| LOC-3 | A match spanning two `textRun`s in one paragraph resolves | `a_match_spanning_two_text_runs_in_one_paragraph_resolves` | `test_match_spanning_two_text_runs_in_one_paragraph` |
| LOC-4 | A non-text element between runs (an inline object) consumes index space but contributes no text, and does not desync the mapping | `a_non_text_element_between_runs_does_not_desync_the_mapping` | `test_non_text_element_between_runs_does_not_desync_mapping` |
| LOC-5 | A needle spanning a paragraph break never matches — a documented limit, matching the API's own `replaceAllText` | `a_needle_spanning_a_paragraph_break_never_matches` | `test_needle_spanning_a_paragraph_break_never_matches` |
| LOC-6 | Occurrences come back in document order and `occurrence=N` selects the Nth; an out-of-range `occurrence` says how many were found; zero matches raises rather than no-oping; an empty needle is rejected; overlapping occurrences resume after the previous hit | `occurrences_come_back_in_document_order_and_are_selectable`, `an_empty_needle_is_rejected`, `overlapping_occurrences_resume_after_the_previous_hit` | `test_occurrences_in_document_order_and_selection`, `test_empty_needle_is_rejected` (**no overlapping case**) |
| LOC-7 | A match inside a table-cell paragraph resolves, and the returned range lies wholly inside that cell | `a_match_inside_a_table_cell_resolves_within_that_cell` | `test_match_inside_a_table_cell_resolves_within_that_cell` |
| LOC-8 | A section ends at the next same-level heading, at the next **higher**-level heading, and at end-of-tab when no heading follows | `a_section_ends_at_the_next_same_level_heading`, `a_section_ends_at_the_next_higher_level_heading`, `a_section_runs_to_the_end_of_the_tab_when_it_is_last` | `test_section_ends_at_next_same_level_heading`, `test_section_ends_at_next_higher_level_heading`, `test_section_runs_to_end_of_tab_when_last` |
| LOC-9 | A section range is clamped to `body_end`, and a section that *is* the last element never includes the body's final newline — which the API refuses to delete | `a_section_is_clamped_to_body_end`, `a_section_ending_at_body_end_never_includes_the_final_newline` | `test_section_is_clamped_to_body_end`, `test_section_ending_at_body_end_never_includes_the_final_newline` |
| LOC-10 | A section matches the **heading**, not body prose with the same text; a heading inside a table cell is not a section; a missing heading error lists the headings that exist | `a_section_matches_the_heading_not_prose_with_the_same_text`, `a_heading_inside_a_table_cell_is_not_a_section`, `a_missing_heading_error_lists_the_available_headings` | `test_section_matches_the_heading_not_prose_with_the_same_text`, `test_missing_heading_error_lists_available_headings` (**no heading-in-cell case**) |
| LOC-11 | Matching is case-sensitive, and **`delete_text` / `replace_text` offer no folding option** — folding can change string length (`'İ'.lower()` is two characters), which would shift every derived offset. Resolved by omitting the parameter, not by implementing it. **Scope caveat:** both tests iterate `delete_text` and `replace_text` only, so `insert_text` / `insert_table` — which also take locators via `after=` / `before=` — are *not* pinned against a folding parameter being added | `matching_is_case_sensitive`, `tools::docs::tests::no_locator_tool_offers_case_insensitive_matching` (2 of the 4 locator tools) | `test_matching_is_case_sensitive_and_offers_no_folding_option` (same 2) |
| LOC-12 | `occurrence` together with `section=` is **rejected**, not silently dropped — a heading resolves to one span, so a caller who passed `occurrence=2` believed something false | `an_occurrence_with_a_section_is_rejected_not_ignored` | `test_occurrence_with_a_section_is_rejected_not_ignored` |
| LOC-13 | `resolve` requires exactly one locator, and describes what it resolved | `resolve_requires_exactly_one_locator`, `resolve_describes_what_it_resolved` | `test_resolve_requires_exactly_one_locator` |
| LOC-14 | `paragraph_bounds` snaps to the containing paragraph, and reports **no boundary** inside a table rather than falling back to the match index | `paragraph_bounds_snaps_to_the_containing_paragraph`, `paragraph_bounds_reports_no_boundary_inside_a_table` | `test_paragraph_bounds_snaps_to_the_containing_paragraph`, `test_paragraph_bounds_reports_no_boundary_inside_a_table` |
| LOC-15 | `body_end` of an empty body is the first writable index | `body_end_of_an_empty_body_is_the_first_writable_index` | — |
| LOC-16 | `text_in_range` slices across runs and astral characters, intersecting **per run** so a gap in the index space does not skew the slice | `text_in_range_slices_across_runs_and_astral_chars`, `text_in_range_intersects_per_run_so_an_index_jump_does_not_skew_the_slice` | `test_text_in_range_slices_across_runs_and_astral_chars` (**no index-jump case**) |

### 4.5 Docs writes: locator-addressed edits — 20 checks

`delete_text`, `replace_text`, and the `after=` / `before=` anchors on `insert_text` /
`insert_table`. Rust names are relative to `tools::docs::tests::`, Python to
`test_tools_unit.py::`.

| # | Claim | Rust | Python |
|---|---|---|---|
| LOC-17 | `delete_text` without `confirm` returns `status=confirmation_required` with the occurrence count, character count and the exact text to be removed, and calls no `batchUpdate` | `delete_text_previews_instead_of_deleting_without_confirm` | `test_delete_text_gates_without_confirm` |
| LOC-18 | With `confirm=true` it executes, and reports what it deleted | `delete_text_executes_once_confirmed` | `test_delete_text_executes_with_confirm` |
| LOC-19 | `replace_text` gates the same way, then rewrites | `replace_text_gates_then_rewrites_in_a_single_batch` | `test_replace_text_gates_then_replaces_in_one_batch` |
| LOC-20 | `dry_run=true` on **both** tools writes nothing **and** does not return `confirmation_required` — dry-run explores, confirm gates | `a_delete_dry_run_neither_writes_nor_gates`, `a_replace_dry_run_neither_writes_nor_gates` | `test_delete_text_dry_run_neither_writes_nor_gates` only — **`replace_text`'s dry-run branch is unpinned in Python** |
| LOC-21 | A multi-occurrence delete emits `deleteContentRange` requests in **descending** `startIndex` order, so an earlier range stays valid after a later one is removed | `deleting_every_occurrence_emits_the_ranges_bottom_up` | `test_delete_text_all_occurrences_emit_descending_ranges` |
| LOC-22 | `replace_text` emits delete+insert **per range** inside a **single** `batchUpdate` — never a delete-then-insert window | `replace_text_gates_then_rewrites_in_a_single_batch` (one call), `replacing_every_occurrence_pairs_each_delete_with_its_own_insert` (per range) | `test_replace_text_gates_then_replaces_in_one_batch` — the single-call half only; it resolves **one** range, where per-range pairing and grouped emission are indistinguishable |
| LOC-23 | Both bodies carry `writeControl: {requiredRevisionId}` taken from the resolving `get`, and **omit the key** when the API returned no `revisionId` (sending null would be rejected outright) | `a_locator_write_pins_the_revision_it_resolved_against`, `a_replace_pins_the_revision_it_resolved_against`, `a_doc_with_no_revision_id_gets_no_write_control_key` | `test_locator_writes_pin_the_resolved_revision` — **plural name, but it drives `delete_text` only**; `test_write_omits_write_control_when_the_api_returns_no_revision` |
| LOC-24 | A tabbed doc puts `tabId` on every emitted range and location; an untabbed doc emits none | `an_untabbed_doc_emits_no_tab_id` | `test_untabbed_doc_emits_no_tab_id` |
| LOC-25 | `replace_text(markdown=true)` with a pipe table raises, pointing at `insert_table`, **before it even reads the document** | `replace_text_refuses_pipe_tables_before_it_even_reads_the_doc` | `test_replace_text_rejects_pipe_tables` |
| LOC-26 | Replacement markdown's styles land on the **new** text, at the new text's own offsets | `replacement_markdown_styles_land_on_the_new_text` | `test_replace_text_markdown_styles_land_on_the_new_text` |
| LOC-27 | An empty replacement only deletes — an empty `insertText` is rejected by the API | `an_empty_replacement_only_deletes` | `test_replace_text_with_empty_string_only_deletes` |
| LOC-28 | A section delete removes the heading and everything under it; a **final** section stops before the body's undeletable newline | `deleting_a_section_removes_the_heading_and_everything_under_it`, `a_final_section_stops_before_the_bodys_undeletable_newline` | `test_delete_text_section_removes_heading_and_body`, `test_delete_text_section_at_end_of_doc_stops_before_final_newline` |
| LOC-29 | `delete_text` with no locator raises before writing | `delete_text_requires_exactly_one_locator` | `test_delete_text_requires_exactly_one_locator` |
| LOC-30 | `after=` anchors at the matched range's **end**, `before=` at its **start** | `after_anchors_at_the_match_end_and_before_at_its_start` | `test_insert_text_after_anchors_at_match_end`, `test_insert_text_before_anchors_at_match_start` |
| LOC-31 | `insert_text(index=N)` with no locator produces the **identical** request stream it did before locators existed | `insert_text_markdown_builds_its_bullets_at_the_given_index` | `test_insert_text_markdown_builds_bullets_at_index` |
| LOC-32 | Exactly one anchor: `index=` together with a locator raises; none of the three raises (JSON Schema cannot express the xor, so it is enforced at runtime and stated in the docstring); an unmatched locator raises before any write | `an_insert_needs_exactly_one_anchor` (all three cases, and asserts zero `docs_batch_update` calls) | `test_insert_text_rejects_multiple_or_missing_anchors`, `test_insert_text_unmatched_locator_raises_before_writing` |
| LOC-33 | Block markdown snaps **past** the paragraph holding the match — a heading anchored mid-paragraph would restyle its host paragraph | `block_markdown_snaps_past_the_paragraph_holding_the_match` | `test_insert_text_block_markdown_snaps_to_paragraph_boundary` |
| LOC-34 | A **block** anchor inside a table cell is refused with a directed error; an **inline** one is not, because inline text has no boundary requirement | `a_block_anchor_inside_a_table_cell_is_refused_but_an_inline_one_is_not` | `test_block_anchor_inside_a_table_cell_is_refused` |
| LOC-35 | `insert_table(after=…)` resolves to something that **is** a paragraph boundary — asserted as membership in `{element startIndexes} ∪ {body_end}`, not merely as an expected number — so `insertTable`'s 400 remap path is never entered | `after_resolves_a_table_to_a_paragraph_boundary` | `test_insert_table_after_resolves_to_a_paragraph_boundary` |
| LOC-36 | The two new tools are registered destructive and the insert tools expose their locator parameters (layer 3) | `server_integration.rs::the_locator_edit_tools_are_registered_as_destructive`, `the_insert_tools_expose_their_locator_parameters` | `test_server_integration.py::test_locator_edit_tools_are_registered_as_destructive`, `test_locator_params_are_exposed_on_the_insert_tools` |

**The four asymmetries above are the ones worth acting on.** `LOC-20`, `LOC-22` and `LOC-23` are
each pinned in Rust and unpinned in Python — and they were unpinned in Rust too until this
verification pass, which is how they were found (§7). They are *parity* gaps rather than port
regressions: the Python implementation behaves correctly; nothing in `tests/` would notice if it
stopped.

### 4.6 Docs writes: tables — 18 checks

`insert_table`, and the GFM pipe-table dialect that `append_text` / `insert_text` /
`create_document` accept under `markdown=true`. §6 records the empirical API behaviour this design
is built on; the design decisions only make sense against it.

| # | Claim | Rust | Python |
|---|---|---|---|
| TBL-1 | The grid is validated — non-empty, every row a list, rectangular, column count and cell count bounded — **before any API call** | `tools::docs::tests::a_bad_grid_is_rejected_before_any_api_call`, `a_grid_must_be_rectangular_and_bounded` | `test_tables.py::test_insert_table_validation_rejects_bad_shapes` |
| TBL-2 | The fill targets the **new** table, selected as the one whose `startIndex` was not in the pre-insert snapshot — including when it sits **below** a pre-existing table, where several tables satisfy `start >= insert_index` because the old one shifted down. This was a wrong-table data-corruption bug, caught before it shipped | `the_fill_targets_the_new_table_not_a_pre_existing_one`, `the_fill_targets_the_new_table_even_when_it_sits_below_the_old_one` | `test_insert_table_selects_new_table_not_preexisting` |
| TBL-3 | The fill never reaches a table in **another tab** | `the_fill_never_reaches_a_table_in_another_tab` | — |
| TBL-4 | The fill batch advances by a forward **running offset**, so a single batch is correct without a second re-fetch, and bolds only row 0 when `header=true` | `the_fill_batch_advances_by_a_running_offset_and_bolds_only_the_header` | `test_insert_table_fills_with_running_offset_and_header_bold` |
| TBL-5 | An empty cell is **skipped**, not written as `""` — `insertText` with empty text is rejected by the API (§6). A `None` cell likewise, and the rest render the way Python's `str()` does | `an_empty_cell_is_skipped_rather_than_written_as_empty_text`, `a_none_cell_is_skipped_and_the_rest_render_the_way_python_str_does` | `test_insert_table_skips_empty_cells` |
| TBL-6 | A failed fill rolls back the orphaned empty table and re-raises the original error. The design is inherently non-atomic — two `execute()` calls with a mandatory re-fetch between, because `insertTable` does not return cell indexes — so without this a 429 leaves an empty table behind and a naive retry adds a second | `a_failed_fill_rolls_back_the_orphaned_empty_table` | `test_insert_table_rolls_back_orphan_on_fill_failure` |
| TBL-7 | `dry_run=true` previews the grid as pipe rows and writes nothing | `insert_table_dry_run_previews_the_grid_without_writing` | `test_insert_table_dry_run_previews_without_writing` |
| TBL-8 | A 400 from a **raw `index`** is re-raised as locator advice rather than leaking the opaque API error; a 400 that did **not** come from a raw index is surfaced as the API reported it | `a_400_from_a_raw_index_becomes_locator_advice`, `a_400_without_a_raw_index_is_surfaced_as_the_api_reported_it` | `test_insert_table_bad_index_400_is_remapped` (**first half only**) |
| TBL-9 | The splitter makes a table only from a pipe row **immediately followed by a delimiter row**; a lone `\| a \| b \|` line stays literal text (this is the backward-compatibility guard); interleaved text/table/text keeps order; alignment colons (`:--`, `--:`, `:-:`) are delimiters | `md::tests::a_header_delimiter_and_body_become_one_table_segment`, `a_pipe_row_with_no_delimiter_row_stays_literal_text`, `text_around_a_table_stays_in_order_and_alignment_colons_are_delimiters`, `markdown_without_a_delimiter_row_is_one_text_segment` | `test_split_blocks_basic_table`, `test_split_blocks_lone_pipe_line_stays_text`, `test_split_blocks_interleaved_and_delimiter_variants`, `test_split_blocks_text_only` |
| TBL-10 | Body rows are padded and truncated to the header's width; header + delimiter with **no** body rows is a one-row table (not zero-row); a row of bare pipes has no cells | `md::tests::body_rows_are_padded_and_truncated_to_the_header_width`, `a_table_with_no_body_rows_still_yields_its_header_row`, `a_row_of_bare_pipes_has_no_cells` | `test_split_blocks_pads_and_truncates_body_rows`, `test_split_blocks_single_row_table` |
| TBL-11 | `\|` is not a cell boundary and unescapes to a literal pipe — the one escape the dialect honours, inside table cells only | `md::tests::an_escaped_pipe_is_not_a_cell_boundary` | `test_split_row_honors_escaped_pipe` |
| TBL-12 | A cell is styled with the same inline dialect as prose, at the cell's own offsets | `md::tests::a_cell_is_styled_with_the_same_inline_dialect_as_prose` | — **planned but never written** |
| TBL-13 | Cell values render the way Python's `str()` does, including float exponent rules and `True`/`False` capitalisation | `md::tests::cell_values_render_the_way_pythons_str_does`, `floats_render_with_pythons_exponent_rules` | — |
| TBL-14 | A pipe table takes the segmented write path on `append_text`, `insert_text` and `create_document` — and **only** when a delimiter row makes it a table, so table-free markdown keeps the single-blob path and its exact old request stream | `appending_a_pipe_table_takes_the_segmented_write_path`, `insert_text_only_segments_when_a_delimiter_row_makes_it_a_table`, `create_document_only_segments_when_a_delimiter_row_makes_it_a_table` | `test_append_text_markdown_table_uses_segmented_path` (append only) |
| TBL-15 | Interleaved segments are written **last-first at one fixed anchor**, so no already-placed segment's indexes are ever referenced again and there is no cross-segment offset arithmetic | `interleaved_segments_are_written_last_first_at_one_fixed_anchor` | — |
| TBL-16 | A heading segment's `updateParagraphStyle` range starts **at** the anchor, not `anchor + 1` — the segmented renderer uses a *trailing* newline, so insertion index must equal style base; mixing in the append path's leading-newline idiom shifts every range by one. The leading-newline case is pinned separately | `a_heading_segments_paragraph_range_starts_at_the_anchor`, `a_leading_newline_moves_the_style_base_without_moving_the_insertion` | `test_segment_text_heading_range_starts_at_anchor` |
| TBL-17 | An empty or whitespace-only text segment emits no `insertText` (an empty insert is API-rejected), while empty markdown still yields exactly one empty text segment | `a_whitespace_only_text_segment_is_skipped`, `md::tests::empty_markdown_still_emits_one_empty_text_segment` | `test_segment_empty_text_is_skipped` |
| TBL-18 | A pipe-table dry run previews the rows without writing, and the preview renders tables as pipes while dropping empty parts | `a_pipe_table_dry_run_previews_the_rows_without_writing`, `md::tests::the_preview_renders_tables_as_pipes_and_drops_empty_parts` | `test_append_text_markdown_table_dry_run_does_not_write` |

### 4.7 Sheets — 49 checks

`read_sheet`, `read_full_sheet`, `write_sheet`, `append_rows`, `format_cells`,
`create_spreadsheet`, `add_tab`, `clear_range`, `delete_rows` — and `a1`, the notation module that
decides **which cells** every Sheets write lands on. A quoting bug in `a1` mis-targets a
destructive write, which is why it gets thirteen checks of its own.

`write_sheet` overwrites a rectangle, `clear_range` empties one, and `delete_rows` removes rows and
shifts everything below them up. All three are irreversible through this server; there is no undo
tool.

#### A1 notation (layer 1)

| # | Claim | Rust (`a1::tests::`) | Python (`test_a1.py::`) |
|---|---|---|---|
| SHT-1 | Column letters are bijective base-26 and round-trip (`0→A`, `25→Z`, `26→AA`, `701→ZZ`, `702→AAA`) | `letters_round_trip_back_to_the_column_index_they_came_from`, `column_indices_render_as_bijective_base_26_letters` | `test_col_letter_roundtrip`, `test_known_letters` |
| SHT-2 | A negative column index is rejected rather than wrapping, and non-letters are rejected as column letters (`"A1"` is not a column) | `a_negative_column_index_is_rejected`, `non_letters_are_rejected_as_column_letters` | — |
| SHT-3 | `parse_cell` yields 0-based `(col, row)`, and accepts a lower-case or whitespace-padded cell | `a_cell_parses_into_a_zero_based_column_and_row`, `a_cell_may_be_lower_case_or_padded_with_whitespace` | `test_parse_cell` (no case/padding case) |
| SHT-4 | A cell **requires** an explicit row number (`"A"`, `" 12 "`, `"A1B"` all fail) — the mechanism that makes `A:C` unparseable | `a_cell_without_a_row_number_is_not_a_cell` | — |
| SHT-5 | `build_range` spans `ncols` across and `nrows` down from its anchor, and a degenerate size (0, negative) clamps to the anchor cell itself rather than producing an inverted range | `a_block_spans_ncols_across_and_nrows_down_from_its_anchor`, `a_block_is_never_smaller_than_the_anchor_cell_itself` | `test_build_range` (no degenerate case) |
| SHT-6 | An empty tab name omits the `!` prefix, and the start cell is upper-cased | `an_empty_tab_omits_the_prefix_and_the_start_cell_is_upper_cased` | — |
| SHT-7 | `quote_tab` doubles embedded apostrophes (`John's Data` → `'John''s Data'`), and `build_range` applies that quoting, so an apostrophe cannot terminate the name early | `quote_tab_doubles_embedded_apostrophes`, `build_range_quotes_an_apostrophe_tab_rather_than_ending_the_name_early` | `test_quote_tab_doubles_embedded_apostrophes`, `test_build_range_quotes_apostrophe_tab` |
| SHT-8 | `split_range` splits tab from cells and **un**doubles a quoted name | `a_range_splits_into_its_tab_and_its_cells` | `test_split_range` |
| SHT-9 | A quoted tab may contain the `!` that would otherwise split the range; an unterminated quote falls back to splitting on the first `!` rather than raising; an empty name before `!` is a tab, not the absence of one | `a_quoted_tab_may_contain_the_bang_that_would_otherwise_split_it`, `an_unterminated_quote_falls_back_to_splitting_on_the_first_bang`, `an_empty_name_before_the_bang_is_a_tab_not_the_absence_of_one` | — |
| SHT-10 | `parse_range` yields 0-based **half-open** bounds, a single cell becomes a 1×1 box, and a trailing colon reads as the single start cell | `a_range_parses_into_zero_based_half_open_bounds`, `a_trailing_colon_reads_as_the_single_start_cell` | `test_parse_range` |
| SHT-11 | An open-ended range (`A:C`) is rejected — `format_cells` promises bounded ranges only | `an_open_ended_column_range_is_rejected` | `test_parse_range_rejects_open_ended_and_reversed` |
| SHT-12 | A range whose end precedes its start is rejected on **either** axis | `a_range_whose_end_precedes_its_start_is_rejected` | same test (one axis) |
| SHT-13 | Only the first colon splits, so a third cell is an error rather than being silently dropped | `only_the_first_colon_splits_a_range_so_a_third_cell_is_an_error` | — |

#### The confirm gate and write mechanics (layer 2)

Rust names relative to `tools::sheets::tests::`, Python to `test_tools_unit.py::`.

| # | Claim | Rust | Python |
|---|---|---|---|
| SHT-14 | `write_sheet` over a non-empty target returns `status=confirmation_required`, reports the overwrite count, and does **not** call `values.update` | `write_sheet_without_confirm_previews_the_overwrite_and_writes_nothing` | `test_write_sheet_gates_on_overwrite` |
| SHT-15 | With `confirm=true` it updates exactly the block the rows span (`'Data'!A1:B1` for 1×2), with `RAW` and body `{values}` | `write_sheet_with_confirm_updates_exactly_the_block_the_rows_span` | `test_write_sheet_executes_with_confirm` |
| SHT-16 | Treating a leading `=` as a formula is **opt-in**: `USER_ENTERED` reaches `valueInputOption`, and nothing else does | `interpreting_a_leading_equals_as_a_formula_is_opt_in` | `test_write_sheet_user_entered_opt_in` |
| SHT-17 | An empty target range is written **without** confirmation — the gate keys on data at risk, not on the verb | `an_empty_target_range_is_written_without_confirmation` | `test_write_sheet_no_gate_when_target_empty` |
| SHT-18 | A `value_input` outside `{RAW, USER_ENTERED}` is rejected **before** any read, in both `write_sheet` and `append_rows`, so a dry run cannot report success for a request that would fail | `value_input_is_rejected_before_a_dry_run_can_report_success` | — |
| SHT-19 | A lower-case `"user_entered"` is refused rather than upper-cased | `a_lower_case_value_input_is_refused_rather_than_upper_cased` | — **deliberate divergence, §8** |
| SHT-20 | `clear_range` gates, then clears exactly the range given | `clear_range_clears_only_once_confirmed` | `test_clear_range_gate` |
| SHT-21 | `delete_rows` gates, then emits one `deleteDimension` with the right `sheetId` and **0-based half-open** bounds (`start_row=3, count=2` → `startIndex 2, endIndex 4`) | `delete_rows_deletes_a_zero_based_half_open_dimension_range_once_confirmed` | `test_delete_rows_gate_and_range` |
| SHT-22 | `delete_rows` on an **unknown tab** is refused before the confirm gate, naming the tabs that exist, and reads nothing. The tab lookup precedes the gate, so a regression resolving an unknown tab to some other `sheetId` would delete rows from the wrong tab — and `confirm=true` would not save the caller, because they confirmed a tab that was never targeted | `deleting_from_an_unknown_tab_is_refused_before_the_confirm_gate` | — |
| SHT-23 | `append_rows` is never gated, defaults to `RAW`, and targets the whole tab (`'Data'`) so the API picks the first free row | `append_rows_is_never_gated_and_defaults_to_raw_input` | `test_append_rows_is_not_gated` |
| SHT-24 | The overwrite count treats `0` and `false` as data but `""` and `null` as empty — a numeric zero is worth confirming over | `zero_and_false_are_data_but_the_empty_string_and_null_are_not` | — |

#### Read side and preview shaping

| # | Claim | Rust | Python |
|---|---|---|---|
| SHT-25 | `read_sheet` returns `headers`/`rows`/`records` when `header_row` is on; a short row is padded with nulls out to the header width; a row **wider** than the header keeps its extra cells in `rows` but drops them from the record, so a record holds one entry per distinct header | `a_short_row_is_padded_with_nulls_out_to_the_header_width`, `a_row_wider_than_the_header_loses_its_extra_cells_from_the_record_only` | `test_read_sheet_builds_records` (padding + records; **no wider-row case**) |
| SHT-26 | `header_row=false` returns the raw grid with **no** `records` key at all | `header_row_false_returns_the_raw_grid_with_no_records` | — |
| SHT-27 | A tabless read previews every tab in the order the API reports them, and "the first tab" means the API's first, not the alphabetical first — which is why the port keeps titles in a `Vec` and not a map | `a_tabless_read_previews_every_tab_in_the_order_the_api_reports_them`, `the_default_tab_is_the_first_one_the_api_lists_not_the_alphabetical_first` | — |
| SHT-28 | A spreadsheet with no tabs at all says `spreadsheet has no tabs` rather than reading a null range | `reading_a_spreadsheet_with_no_tabs_at_all_says_so` | — |
| SHT-29 | The preview window is `max_rows × max_cols` anchored at A1 (`'Data'!A1:E5` by default) | `the_preview_window_is_max_rows_by_max_cols_anchored_at_a1` | — |
| SHT-30 | An explicit `a1_range` is passed through **verbatim** — neither re-quoted nor cropped to the window — outranks `tab`, and skips the tab-list fetch entirely | `an_explicit_a1_range_is_read_verbatim_and_outranks_the_tab` | — |
| SHT-31 | Only an explicit `FORMULA` render returns formulas; an unrecognised `values` falls back to `UNFORMATTED_VALUE` rather than erroring | `only_an_explicit_formula_render_returns_formulas` | — |

#### Spill to disk

| # | Claim | Rust | Python |
|---|---|---|---|
| SHT-32 | `read_full_sheet` writes the whole tab to a CSV, reports exact `total_rows`/`total_cols`, and previews only the first 5 rows × 5 cols | `read_full_sheet_spills_a_csv_and_previews_only_the_first_five_rows` | `test_read_full_sheet_saves_csv_and_previews` |
| SHT-33 | The spill forwards its render option to the API, so a caller who asked for `FORMULA` does not silently get computed numbers | `a_spill_forwards_its_render_option_to_the_api` | — |
| SHT-34 | The spilled CSV is byte-identical to what Python's `csv.writer` produced: `\r\n` terminators, `True` for a boolean, `"a,b"` quoted, `""` doubling, ragged rows accepted | `the_spilled_csv_renders_cells_the_way_pythons_csv_writer_did` | — |
| SHT-35 | A wholly blank sheet row (`[]`, which is how Sheets reports one) spills as a blank line, not as an empty quoted field | `a_blank_sheet_row_spills_as_a_blank_line_not_an_empty_field` | — |
| SHT-36 | The spill is `0600` — the CSV can hold sensitive data, so no other local account may read it | `a_spilled_csv_is_readable_only_by_its_owner` | — |
| SHT-37 | A `dest_path` escaping the sandbox is refused, and nothing lands outside it — asserted by walking the directory that *holds* the sandbox, so a containment regression is observable rather than merely un-asserted | `a_spill_dest_path_escaping_the_sandbox_is_rejected` | — |

#### Dry runs

| # | Claim | Rust | Python |
|---|---|---|---|
| SHT-38 | `write_sheet(dry_run=true)` returns `before`/`after` plus `formulas_not_recalculated`, and calls no update | `a_write_dry_run_predicts_before_and_after_without_writing` | `test_write_sheet_dry_run_predicts_and_does_not_write` |
| SHT-39 | `clear_range(dry_run=true)` reports what would go, and clears nothing | `a_clear_dry_run_reports_what_would_go_without_clearing` | `test_clear_range_dry_run_does_not_clear` |
| SHT-40 | `delete_rows(dry_run=true)` names the **inclusive** row span (`"2..4"`), pre-reads `'Data'!2:4`, and issues no `batchUpdate` | `a_delete_dry_run_names_the_inclusive_row_span_without_deleting` | `test_delete_rows_dry_run_does_not_delete` |
| SHT-41 | `append_rows(dry_run=true)` reports the landing row (`len(current) + 1`) and appends nothing | `an_append_dry_run_reports_the_landing_row_without_appending` | `test_append_rows_dry_run_does_not_append` |

#### Cell formatting and creation

| # | Claim | Rust | Python |
|---|---|---|---|
| SHT-42 | `format_cells` builds exactly one `repeatCell` over the resolved 0-based half-open box, and the `fields` mask names **only** the flags that were passed, so unset properties are not reset | `format_cells_builds_one_repeat_cell_over_the_resolved_box` | `test_format_cells_builds_repeat_cell` |
| SHT-43 | An `a1_range` with no tab prefix targets the first tab | `a_range_with_no_tab_prefix_targets_the_first_tab` | `test_format_cells_defaults_to_first_tab` |
| SHT-44 | `format_cells` requires at least one flag, and an unknown tab names the tabs that exist and writes nothing | `format_cells_needs_a_flag_and_a_tab_that_exists` | `test_format_cells_requires_a_flag_and_known_tab` |
| SHT-45 | `format_cells(dry_run=true)` resolves the target and cell count without writing | `a_format_dry_run_resolves_the_target_without_writing` | `test_format_cells_dry_run_does_not_write` |
| SHT-46 | `create_spreadsheet` sends a `sheets` array only when tabs were asked for | `create_spreadsheet_names_its_tabs_only_when_some_were_asked_for` | — |
| SHT-47 | `add_tab` sends an `index` only when one was given, rather than defaulting to position 0 | `add_tab_sends_an_index_only_when_one_was_given` | — |

#### Request construction (Rust-only layer)

| # | Claim | Rust (`clients::tests::`) | Python |
|---|---|---|---|
| SHT-48 | `append` sends `insertDataOption=INSERT_ROWS`; the API's default (`OVERWRITE`) would write over whatever sits below the data | `appending_inserts_rows_rather_than_overwriting_below_the_last_row` | `test_append_rows_is_not_gated` asserts the kwarg at the call site |
| SHT-49 | A1 ranges are percent-encoded into the URL path so `!`, `'`, `:` and spaces survive as **data** — including the doubled apostrophe of `'John''s Data'!A1:B2` | `ranges_are_percent_encoded_into_the_path` | n/a — `googleapiclient` encodes for the Python side |
| SHT-50 | The overwrite gate inspects the target as **formulas**, so a cell whose formula evaluates to `""` still counts as data and still trips the gate (§7.6) | `the_gate_inspects_the_target_as_formulas_so_one_rendering_empty_still_trips_it` | `test_write_sheet_gate_inspects_formulas_so_one_rendering_empty_still_trips_it` |
| SHT-51 | `dry_run`'s `before` shows a formula cell as its formula text — the same read feeds both, so a caller sees what it would destroy rather than what that formula produced | `a_write_dry_run_shows_a_formula_cell_as_its_formula` | `test_write_sheet_dry_run_shows_a_formula_cell_as_its_formula` |

`SHT-49` exists because two independent transformations compose on a hostile tab name: `quote_tab`
doubles the apostrophe, and then the client percent-encodes each `'` into `%27`. Live check `S9` is
what establishes that Google accepts the result.

### 4.8 Files — 24 checks

`read_file_as_text`, `download_file`, `upload_file`, `move_file`, `rename_file`, `export_file`.
These can replace a file's bytes, re-parent it, rename it — and they are the tools that touch the
**local** filesystem. Rust names relative to `tools::files::tests::`.

| # | Claim | Rust | Python |
|---|---|---|---|
| FIL-1 | `move_file` previews the move (`file`/`from`/`to`) and only re-parents after `confirm` | `move_file_previews_the_move_and_only_reparents_after_confirm` | `test_tools_unit.py::test_move_file_gate_and_parents` |
| FIL-2 | **Every** current parent is named in `removeParents`, so a move is a move and not a second placement — Drive's `addParents` alone does not detach | `every_current_parent_is_removed_so_a_move_is_not_a_second_placement` | `test_move_file_gate_and_parents` |
| FIL-3 | A file with no parents still moves (empty `removeParents`, not a crash) | `a_file_with_no_parents_still_moves` | — |
| FIL-4 | `rename_file` previews `from`/`to` and only writes after `confirm` | `rename_file_previews_the_new_name_and_only_writes_after_confirm` | `test_rename_file_gate` |
| FIL-5 | `upload_file(replace_id=…)` previews the file it would overwrite and never uploads without `confirm` | `upload_file_previews_a_replacement_and_never_overwrites_without_confirm` | `test_upload_replace_gate` |
| FIL-6 | A confirmed replacement goes through `files.update` with media (not `create`), and reports `replaced: true` | `a_confirmed_replacement_uploads_over_the_existing_file` | — |
| FIL-7 | A Google Doc is exported as `text/plain` and paginated; a Google Sheet as `text/csv` | `a_google_doc_is_exported_as_plain_text_and_paginated`, `a_google_sheet_is_exported_as_csv` | — |
| FIL-8 | A later `chunk` serves that chunk and reports `has_more` correctly; `max_chars<=0` returns the whole document in one chunk | `requesting_a_later_chunk_serves_that_chunk_and_reports_more`, `max_chars_of_zero_returns_the_whole_document_in_one_chunk` | — |
| FIL-9 | A PDF is text-extracted and reports its page count; an unreadable PDF returns an error rather than escaping as a panic | `a_pdf_is_text_extracted_and_reports_its_page_count`, `an_unreadable_pdf_reports_an_error_instead_of_escaping` | — |
| FIL-10 | Undecodable bytes are replaced (U+FFFD), not fatal — matching Python's `errors="replace"` | `undecodable_bytes_are_replaced_rather_than_failing_the_read` | — |
| FIL-11 | A non-text binary mime points the caller at `download_file` instead of returning garbage | `a_binary_mime_type_points_at_download_file` | — |
| FIL-12 | A small download comes back inline as base64 | `a_small_download_comes_back_inline_as_base64` | — |
| FIL-13 | A Google-native file cannot be downloaded — it has no bytes, only exports | `a_google_native_file_cannot_be_downloaded` | — |
| FIL-14 | A download spills to the sandbox above the 5 MB inline limit **with** an explanatory note, and on any explicit `dest_path` **without** one (the caller already knows why) | `a_download_over_the_inline_limit_spills_to_the_sandbox_with_a_note`, `an_explicit_dest_path_spills_even_a_tiny_file_and_needs_no_note` | — |
| FIL-15 | A spilled download is `0600` | `a_spilled_download_is_readable_only_by_its_owner` | — |
| FIL-16 | A small export comes back inline; a large one, or any `dest_path`, spills — defaulting to `<file-id>.<format>` | `a_small_export_comes_back_inline_as_base64`, `an_export_with_a_dest_path_is_written_there_instead_of_inlined`, `a_large_export_without_a_dest_path_spills_under_the_file_id` | — |
| FIL-17 | The export format is matched case-insensitively and echoed back as the caller spelled it; an unsupported one lists the supported set alphabetically | `the_export_format_is_matched_case_insensitively_and_echoed_as_given`, `an_unsupported_export_format_lists_the_supported_ones_alphabetically` | — |
| FIL-18 | An upload with neither `source_path` nor `content` is refused | `an_upload_with_neither_a_source_nor_content_is_refused` | — |
| FIL-19 | Text `content` defaults to `text/plain`, and `""` is legitimate content rather than "unset" | `text_content_defaults_to_text_plain_and_empty_content_still_uploads` | — |
| FIL-20 | An explicit `mime_type` wins over the default | `an_explicit_mime_type_wins_over_the_text_default` | — |
| FIL-21 | A `parent` becomes the new file's `parents`, resolved through the reference parser first | `a_parent_folder_becomes_the_new_files_parent` | — |
| FIL-22 | A file upload reads its bytes from inside the sandbox | `a_file_upload_reads_the_bytes_from_the_sandbox` | — |
| FIL-23 | An upload guesses its mime type from the filename, matching what `MediaFileUpload`'s `mimetypes.guess_type` did — uploading a `.pdf` as `octet-stream` would store the wrong type in Drive | `an_upload_guesses_its_mime_type_from_the_filename` | — |
| FIL-24 | Sandbox containment is enforced at **every** local-I/O site, and for uploads it is enforced *before* any Drive call (a missing file inside the sandbox likewise) | `an_upload_source_path_escaping_the_sandbox_is_rejected_before_any_drive_call`, `a_source_path_inside_the_sandbox_that_does_not_exist_says_so`, `a_download_dest_path_escaping_the_sandbox_is_rejected`, `an_export_dest_path_escaping_the_sandbox_is_rejected`, plus `SHT-37` for the CSV | — |
| FIL-25 | A corrupt or encrypted PDF is reported as `could not extract text from PDF: …` rather than leaking a raw pypdf exception type the agent cannot act on (§7.7) | `an_unreadable_pdf_reports_an_error_instead_of_escaping` | `test_read_file_as_text_curates_a_corrupt_pdf_instead_of_leaking_pypdf` |

**Asymmetry.** `read_file_as_text`, `download_file`, `export_file` and `create_spreadsheet` /
`add_tab` (`SHT-46`/`47`) have **no Python tool-level tests at all**, and `upload_file` has one — the
replacement gate. Everything else about those tools is pinned in Rust only. That is not a statement
that the Python is wrong; it is a statement that a Python regression in those tools would be caught
by nothing but `SRF-11`, which only checks the surface, not the behaviour.

### 4.9 The local-file sandbox and spill retention — 6 checks

`localfs` decides what a compromised or prompt-injected agent can reach on the operator's machine.
Layer 1, but against real temporary directories, real symlinks and a real case-insensitive
filesystem — a containment check against a mocked filesystem verifies the mock.

| # | Claim | Rust (`localfs::tests::`) | Python (`test_localfs.py::`) |
|---|---|---|---|
| SBX-1 | A write destination cannot leave the sandbox: a relative path stays inside and its parents are created, while an absolute path, `..`, a planted symlink, a **dangling** symlink, a symlink **cycle**, and a mis-cased escape on a case-insensitive filesystem are all rejected | `a_relative_write_destination_stays_inside_the_sandbox_and_its_parents_are_created`, `an_absolute_dest_path_is_rejected`, `a_dest_path_climbing_out_with_dotdot_is_rejected`, `a_symlink_planted_in_the_sandbox_cannot_be_used_to_escape_it`, `a_dangling_symlink_cannot_be_used_to_escape_either`, `a_symlink_cycle_terminates_and_stays_inside`, `a_miscased_escape_is_rejected_even_on_a_case_insensitive_filesystem` | `test_write_relative_stays_within_and_makes_parents`, `test_write_rejects_absolute`, `test_write_rejects_dotdot_escape` — **no symlink, cycle or case coverage in either language before this pass** |
| SBX-2 | An untrusted default filename is reduced to a sanitized basename at the sandbox root; a name with nothing usable left becomes `download`; the sanitizer keeps Unicode letters and replaces everything else | `a_default_name_is_reduced_to_a_sanitized_basename_at_the_root`, `a_name_with_nothing_usable_left_becomes_download`, `the_sanitizer_keeps_unicode_letters_and_replaces_everything_else` | `test_write_default_name_is_sanitized_basename` |
| SBX-3 | An upload source resolves only inside the sandbox; absolute and `..` are rejected; a missing file inside it says so rather than 404ing at Drive | `a_source_path_inside_the_sandbox_resolves_to_the_real_file`, `an_absolute_source_path_is_rejected`, `a_source_path_climbing_out_with_dotdot_is_rejected`, `a_missing_source_file_inside_the_sandbox_reports_not_found` | `test_read_within_ok`, `test_read_rejects_absolute`, `test_read_rejects_escape`, `test_read_missing_file_in_sandbox` |
| SBX-4 | A configured sandbox path that exists but is **not a directory** is an error for callers rather than a confusing I/O failure later | `a_path_that_exists_but_is_not_a_directory_is_an_error_for_callers` | — |
| SBX-5 | A sandbox the server creates is private to its owner (`0700`) and carries the ownership marker; the default location is treated as owned even when it already exists | `a_sandbox_the_server_created_is_private_to_its_owner`, `a_sandbox_the_server_created_carries_the_ownership_marker`, `the_default_location_is_owned_even_when_it_already_exists` | `test_created_sandbox_is_marked`, `test_default_location_is_owned_even_if_preexisting` (**mode not asserted**) |
| SBX-6 | Spilled data is disposed of on a bounded schedule, and **only** in a directory gdrive-mcp owns: the sweep deletes past the TTL including subdirectories, `TTL<=0` disables it, an unparseable TTL falls back to 24 h, an unmarked pre-existing directory is refused with a notice naming the opt-in `touch`, writing into such a directory does not opt it in, an operator-placed marker does, the marker itself is spared, and an unusable directory never fails startup | `the_sweep_deletes_files_past_the_ttl_and_keeps_fresh_ones`, `the_sweep_reaches_files_in_subdirectories`, `a_ttl_of_zero_disables_the_sweep`, `an_unparseable_ttl_env_value_falls_back_to_the_default`, `the_sweep_refuses_an_unmarked_preexisting_directory`, `the_refusal_notice_names_the_directory_and_the_touch_that_opts_it_in`, `writing_into_a_preexisting_directory_does_not_opt_it_into_sweeping`, `an_operator_placed_marker_opts_an_existing_directory_into_the_sweep`, `the_sweep_spares_the_marker_itself`, `an_unusable_files_dir_never_fails_the_sweep` | `test_sweep_deletes_old_keeps_fresh`, `test_sweep_ttl_zero_disables`, `test_sweep_bad_ttl_env_does_not_raise`, `test_sweep_refuses_preexisting_unmarked_dir`, `test_writes_do_not_opt_preexisting_dir_into_sweeping`, `test_operator_marker_opts_existing_dir_into_sweep`, `test_sweep_spares_the_marker_itself`, `test_sweep_bad_files_dir_does_not_raise` — **two of the ten sub-claims are Rust-only**: the sweep reaching **subdirectories**, and the refusal notice **naming the directory and the `touch`** (Python asserts only that `"not sweeping"` reached stderr) |

The sweep is ownership-gated because the alternative is worse than no sweep: an operator who points
`GDRIVE_MCP_FILES_DIR` at a directory that already holds their own files would otherwise have them
deleted on the next server start.

### 4.10 The audit log — 6 checks

The log exists so a regulated deployment has a record of who asked for what. It is therefore the
one place where writing the *wrong* thing is a compliance event rather than a bug.

| # | Claim | Rust (`audit::tests::`) | Python (`test_audit.py::`) |
|---|---|---|---|
| AUD-1 | One JSON line per call, recording user, tool, target id and outcome — success and error alike | `writes_one_json_line_per_call` | `test_records_only_safe_keys_and_no_content`, `test_error_outcome_and_scalars` |
| AUD-2 | Cell contents, filenames and free text are **never** recorded. The safe-key list is an allowlist, so this holds by construction — and the check exists because widening the allowlist would quietly start writing document text to disk | `free_text_and_content_arguments_are_never_recorded` | `test_records_only_safe_keys_and_no_content` |
| AUD-3 | Locator arguments (`match`, `section`, `replacement`, `after`, `before`) are never recorded — they are document content by definition | `locator_arguments_are_never_recorded` | `test_locator_args_are_never_logged` |
| AUD-4 | Reference arguments are reduced to their opaque Drive id, and an unparseable one is masked rather than logged verbatim (a raw URL or free-text `item` could itself carry sensitive data) | `ref_arguments_are_reduced_to_their_opaque_id`, `an_unparseable_ref_is_masked_rather_than_logged_verbatim` | `test_unparseable_ref_is_masked` |
| AUD-5 | The record carries the defaults the tool actually ran with, and an unset optional reference is left **out** rather than logged as `<unparseable>` | `server::tests::the_audit_record_carries_the_defaults_the_tool_actually_ran_with`, `an_unset_optional_reference_is_left_out_rather_than_logged_as_unparseable` | — |
| AUD-6 | Logging never raises on an unwritable path — an audit failure must not become a tool failure | — | `test_never_raises_on_unwritable_path` |

`AUD-6` is the one asymmetry that runs the *other* way: Python pins it, Rust does not.

### 4.11 The verification gate — 7 checks

| # | Claim | Rust | Python (`test_gating.py::`) |
|---|---|---|---|
| GAT-1 | With no patterns configured there is no gate, and a model that matches none of them is not gated | `gating::tests::no_patterns_configured_means_no_gate`, `an_unmatched_model_is_not_gated` | `test_no_gate_without_configured_patterns`, `test_no_gate_when_model_does_not_match` |
| GAT-2 | Patterns match case-insensitively and across underscores | `gating::tests::patterns_match_case_insensitively_and_across_underscores` | `test_model_requires_verification_uses_configured_patterns` |
| GAT-3 | The calling model is taken from `_meta.model` first, then the operator pin; a matching **request** model gates even without a pin; a nonmatching request model **cannot** downgrade an operator-pin match | `gating::tests::calling_model_prefers_meta_then_the_operator_pin`, `a_matching_request_model_gates_even_without_a_pin`, `a_nonmatching_request_model_cannot_downgrade_the_operator_pin` | `test_calling_model_prefers_meta_then_env`, `test_matching_meta_model_triggers_without_pin`, `test_meta_cannot_downgrade_env_pin` |
| GAT-4 | `GDRIVE_MCP_REQUIRE_VERIFICATION=always` gates every model regardless | `gating::tests::the_always_override_gates_every_model` | `test_always_override_gates_any_model` |
| GAT-5 | A gated call with no context to prompt through **fails closed** | `gating::tests::a_gated_call_without_a_context_fails_closed` | `test_configured_model_no_context_fails_closed`, `test_gated_blocks_configured_model_without_ctx` |
| GAT-6 | An approval answered with a string or a number still counts as approval (clients vary) | `gating::tests::an_approval_answered_with_a_string_or_number_still_counts` | `test_configured_model_accept_passes` |
| GAT-7 | On the **registered** path (layer 3): approval proceeds to the API, decline blocks, accept-**without**-approving blocks, and a client that cannot prompt fails closed | `server_integration.rs::approval_lets_the_call_proceed_to_the_api`, `declining_the_prompt_blocks_the_call`, `accepting_without_approving_still_blocks_the_call`, `the_gate_fails_closed_when_the_client_cannot_prompt` | `test_server_integration.py::test_gate_approve_proceeds_on_registered_path`, `test_gate_decline_blocks_on_registered_path`, `test_gate_fires_fail_closed_on_registered_path`; unit-level `test_configured_model_decline_blocks`, `test_configured_model_accept_but_not_approved_blocks` |

Because request metadata is advisory, the gate is a guard rail, not a boundary. Mandatory policy
belongs in deployment-controlled environment variables and credential scoping; the README says so,
and no check here contradicts it.

### 4.12 The request layer, credentials and errors — 6 checks

Invariants no tool-level fake can see. In Rust these live in `clients.rs`, which builds URLs and
query strings by hand; in Python `googleapiclient` does that from a discovery document, so there is
nothing local to assert — which is precisely why the defect in §7 was possible on one side only.

| # | Claim | Rust | Python |
|---|---|---|---|
| REQ-1 | `supportsAllDrives=true` goes on every Drive call whose REST method accepts it, and is **absent** where the method has no such parameter (`files.export`, `comments.*`, `about.get`) | `clients::tests::every_drive_call_whose_method_accepts_it_opts_into_shared_drives` | — spelled at each call site; **the check that found the 404 defect** |
| REQ-2 | The rule table above **covers every Drive call the client actually makes**, read out of the source, so a new endpoint cannot dodge the rule by not being listed | `clients::tests::the_shared_drive_table_covers_every_drive_call_this_client_makes` | — |
| REQ-3 | Listings also send `includeItemsFromAllDrives=true` — without it the opt-in above still hides every shared-drive child | `clients::tests::listing_also_asks_for_the_items_that_live_on_other_drives` | — |
| REQ-4 | Google error bodies become API errors carrying their message; a non-JSON error body still produces a readable message; a status-specific hint is attached where one exists, and unknown statuses get none | `clients::tests::google_error_bodies_become_api_errors_with_their_message`, `non_json_error_bodies_still_produce_a_readable_message`, `error::tests::api_errors_carry_a_status_specific_hint`, `plain_messages_pass_through`, `unknown_statuses_get_no_hint` | — |
| REQ-5 | The token file round-trips through the shape `google-auth` writes, including its naive-UTC expiry, a file written without an expiry, and a skew window that counts a nearly-expired token as expired | `auth::tests::round_trips_through_the_python_token_json_shape`, `parses_the_naive_utc_expiry_google_auth_writes`, `tolerates_a_token_file_google_auth_wrote_without_an_expiry`, `a_past_expiry_is_expired`, `an_expiry_inside_the_skew_window_counts_as_expired` | — |
| REQ-6 | The config directory follows `XDG_CONFIG_HOME`, and explicit overrides win over the default locations | `config::tests::config_dir_follows_xdg_config_home`, `overrides_win_over_the_default_locations` | — |

`REQ-2` is the load-bearing half of the shared-drive pair. `REQ-1` alone would have passed forever
if nobody had listed the endpoint.

`REQ-5` is what makes "authenticating one authenticates the other" true. It is asserted against the
serialised shape only; no test performs a real OAuth exchange in either language.

### 4.13 The registered tool surface — 13 checks

What the MCP client is actually told, and what happens to the arguments it sends.

| # | Claim | Rust | Python |
|---|---|---|---|
| SRF-1 | All **37** tools register, with unique names and non-empty descriptions | `server_integration.rs::every_tool_is_registered_with_the_right_annotations` (asserts `tools.len() == 37`), `tools::tests::every_tool_forbids_unknown_arguments_and_names_itself` | `test_server_integration.py::test_registered_tools_and_annotations` (`len(by_name) == 37`) |
| SRF-2 | The read-only and destructive sets are the Python sets **verbatim**, are disjoint, and every name in them is actually registered — set membership *is* the contract, since it is what every client is told | `server::tests::the_annotated_sets_are_the_python_sets_verbatim` | `test_registered_tools_and_annotations` (representatives only) |
| SRF-3 | Read-only tools carry `readOnlyHint` and leave `destructiveHint` **unset** — a read cannot destroy anything, so claiming `false` would be a claim the Python never made; destructive tools carry the inverse triple | `server::tests::every_read_only_tool_is_annotated_as_such`, `every_destructive_tool_carries_the_destructive_hint` | representatives only |
| SRF-4 | Additive tools are writes but not destructive — and `read_full_sheet` is in that set, **not** in read-only, because it spills a local file | `server::tests::additive_tools_are_writes_but_not_destructive` | — |
| SRF-5 | `confirm` is offered by exactly the destructive tools, and `dry_run`/`confirm` by exactly the tools the README names — the annotation and the gate are two halves of one promise | `server::tests::confirm_is_offered_by_exactly_the_destructive_tools`, `tools::tests::dry_run_and_confirm_are_offered_by_exactly_the_tools_the_readme_marks` | — |
| SRF-6 | Both gates are booleans defaulting to `false` and are never `required`, so an omitted flag never reads as permission | `tools::tests::both_gates_default_to_false_so_an_omitted_flag_never_reads_as_permission` | — |
| SRF-7 | Every schema forbids unknown arguments (`additionalProperties: false`), leaks no injected `ctx`, declares only `required` args that exist as properties, and declares optional arguments the same way everywhere | `server::tests::every_registered_tool_becomes_an_rmcp_tool_with_a_strict_schema`, `tools::tests::declared_required_arguments_all_exist_as_properties`, `optional_arguments_are_declared_the_same_way_everywhere` | `test_registered_tools_and_annotations` asserts `"ctx" not in inputSchema` |
| SRF-8 | An unknown argument is **rejected**, not dropped, on the registered path | `server_integration.rs::an_unknown_argument_is_rejected_rather_than_dropped`, `args::tests::unknown_arguments_are_rejected_rather_than_dropped` | `test_unknown_kwarg_rejected_on_registered_path`, `test_unknown_kwarg_rejected_on_new_tools` |
| SRF-9 | Argument coercion reproduces **both** of FastMCP's layers, not just the strict one: a list sent as a JSON string is re-parsed, a boolean sent as `"true"` is coerced, an integral float counts as an integer, an explicit `null` reads as the documented default — while a word that is not a boolean is still rejected, a string that merely looks numeric is left alone, wrong types are reported with the argument name, missing required arguments say so, and `rows` must be a list of lists | `args::tests::a_list_argument_sent_as_a_json_string_is_re_parsed`, `booleans_and_integers_accept_the_spellings_pydantic_coerced`, `integral_floats_count_as_integers`, `explicit_null_reads_as_the_documented_default`, `a_word_that_is_not_a_boolean_is_still_rejected`, `a_string_that_merely_looks_numeric_is_left_alone`, `wrong_types_are_reported_with_the_argument_name`, `missing_required_arguments_say_so`, `rows_must_be_a_list_of_lists` | n/a — pydantic's own behaviour, which the Rust tests encode |
| SRF-10 | Tools are declared in the order the Python modules registered them, and each module's dispatch ignores names it does not own | `tools::{discovery,docs,sheets,files,calendar}::tests::*declared*order*` and each module's unrelated-name dispatch test | n/a |
| SRF-11 | **Cross-language surface parity**: identical names, descriptions, argument names, argument **order**, required sets and defaults, and a schema that forbids unknown arguments | `scripts/diff_tool_surface.py` — reads the Python signatures and docstrings out of the AST (no import, no credentials), starts the Rust binary, asks it for `tools/list` over stdio, and diffs. Exits non-zero on any mismatch. **Green at 37/37.** | same script |
| SRF-12 | An impact preview names the action and never claims success | `guard::tests::a_preview_names_the_action_and_never_claims_success` | `test_guard.py::test_preview_shape` |
| SRF-13 | The locator edit tools are registered destructive, and the insert tools expose their locator parameters | see `LOC-36` | see `LOC-36` |

`SRF-11` cannot be a test inside either language: the Python side is source and the Rust side only
exists at runtime over stdio. Discovery additionally pins its own schemas and descriptions in-tree
(`DSC-20`); **Sheets, Docs, Files and Calendar still depend on the script for complete schema/docstring parity.**

### 4.14 Calendar — 21 checks

The Calendar implementation is new in both languages. Python drives the discovery client through
a recording fake; Rust drives the matching `GoogleApi` trait and separately pins query parameters
that are assembled below the fake seam. All four mutations are confirmation-gated and dry-runnable.

| # | Claim | Rust | Python |
|---|---|---|---|
| CAL-1 | OAuth asks only for Drive plus narrow CalendarList-read, event, and free/busy scopes; an old Drive-only token fails with a directed re-consent instruction; the Rust auth URL requests incremental grants | `config::SCOPES`, `auth::tests::an_old_drive_only_token_requests_reconsent_for_calendar` | `test_config_requests_each_required_narrow_calendar_scope`, `test_old_drive_only_token_gets_a_directed_reconsent_error` |
| CAL-2 | `list_calendars` forwards paging/access filters, clamps the API page size, and returns the stable bounded calendar shape | `calendar::tests::list_calendars_clamps_pages_and_returns_the_agent_facing_shape` | `test_list_calendars_clamps_pages_and_returns_a_stable_shape` |
| CAL-3 | `list_events` expands recurring instances, orders by start, forwards query/paging/cancelled filters, clamps page size, and normalizes events | `calendar::tests::list_events_expands_recurrence_forwards_filters_and_normalizes` | `test_list_events_expands_recurrence_forwards_filters_and_normalizes` |
| CAL-4 | Event windows default to now…+30 days, require offset-bearing RFC3339 values, and reject reversed bounds before I/O | `calendar::tests::list_events_rejects_naive_and_reversed_windows_before_the_api` | `test_list_events_rejects_naive_or_reversed_windows_before_the_api` |
| CAL-5 | `get_event` uses the same stable event representation as a listing | `calendar::tests::get_event_uses_the_same_shape_as_list` | `test_get_event_returns_the_same_shape_as_list` |
| CAL-6 | `query_freebusy` defaults to primary and preserves per-calendar busy intervals and errors without exposing event details | `calendar::tests::freebusy_defaults_to_primary_and_preserves_per_calendar_errors` | `test_freebusy_defaults_to_primary_and_preserves_per_calendar_errors` |
| CAL-7 | Free/busy validates time order/IANA zone and rejects more than Google's 50-calendar limit before I/O | `calendar::tests::freebusy_enforces_the_api_calendar_limit_before_the_call` plus shared validators | `test_freebusy_rejects_more_than_google_allows` plus shared validators |
| CAL-8 | `create_event` preview and dry-run make no insert, include notification impact, and default `send_updates` to `none` | `calendar::tests::create_previews_and_dry_runs_without_writing` | `test_create_previews_without_writing_and_dry_run_never_writes` |
| CAL-9 | Confirmed create sends attendees, recurrence (including EXRULE), reminder overrides, Meet conference version, availability/visibility, notification policy, and optional caller-supplied id | `calendar::tests::confirmed_create_sends_attendees_reminders_recurrence_and_meet` | `test_confirmed_create_sends_attendees_reminders_recurrence_and_meet` |
| CAL-10 | Timed events require offsets; all-day dates are ISO dates with an exclusive end and no illegal time-zone field | `calendar::tests::all_day_end_is_exclusive_and_dates_never_carry_a_timezone` | `test_all_day_dates_are_exclusive_and_do_not_carry_a_timezone` |
| CAL-11 | Empty summaries, malformed ids/emails/recurrence/reminders, more than five reminders, a timed recurrence without a zone, and invalid zones/visibility/availability/notification modes fail before writing | `calendar::tests::high_risk_create_inputs_are_validated_before_preview` | `test_create_validates_high_risk_inputs_before_preview` |
| CAL-12 | Update reads a live before-state, previews a merged after-state, distinguishes omitted lists from explicit clearing, and dry-run does not patch | `calendar::tests::update_previews_clears_and_confirmed_calls_patch_with_only_supplied_fields` | `test_update_reads_then_previews_and_distinguishes_clear_from_unset` |
| CAL-13 | Confirmed update requires the previewed ETag, rejects a missing/stale value before mutation, sends only supplied fields, and carries that ETag in `If-Match` | Rust update tests including `confirmed_calendar_writes_require_the_preview_etag_and_reject_a_stale_one` | Python update tests including `test_confirmed_calendar_writes_require_the_preview_etag_and_reject_a_stale_one` |
| CAL-14 | Update refuses an empty patch or half of a start/end pair before reading, and refuses turning a timed event into a series without supplying zoned start/end values | `calendar::tests::update_needs_a_change_and_paired_times_before_reading`, `update_requires_a_timezone_when_turning_a_timed_event_into_a_series` | `test_update_needs_a_change_and_paired_times`, `test_update_requires_a_timezone_when_turning_a_timed_event_into_a_series` |
| CAL-15 | Delete's preview distinguishes a recurring master from an instance and does not delete | `calendar::tests::delete_identifies_a_series_and_calls_delete_only_after_confirmation` | `test_delete_identifies_series_and_only_deletes_after_confirmation` |
| CAL-16 | Confirmed delete forwards notification policy and the live ETag | same Rust delete test | same Python delete test |
| CAL-17 | RSVP validates accepted/tentative/declined, previews old/new status, and makes no unconfirmed patch | `calendar::tests::responding_preserves_other_attendees_and_changes_only_self` | `test_respond_preserves_other_attendees_and_only_changes_self` |
| CAL-18 | Confirmed RSVP sends only the self attendee with `attendeesOmitted=true`, preserving every other participant, carries the live ETag, and refuses events without a self attendee | `calendar::tests::responding_uses_attendees_omitted_to_change_only_self`, `responding_requires_the_signed_in_attendee` | `test_respond_uses_attendees_omitted_to_change_only_self`, `test_respond_refuses_an_event_where_the_user_is_not_an_attendee` |
| CAL-19 | All eight tools register in Python order with parity-identical schemas and correct read/destructive annotations | Calendar schema-order test, server unit/integration tests, `scripts/diff_tool_surface.py` | server integration tests, surface script |
| CAL-20 | Calendar ids, summaries, descriptions, locations, attendee addresses and event content cannot enter the content-free audit log | `audit::tests::calendar_content_attendees_and_email_calendar_ids_are_never_recorded` | `test_calendar_content_attendees_and_email_calendar_ids_are_never_logged` |
| CAL-21 | The hand-built Rust REST queries pin expanded chronological listings, page/search filters, explicit notification policy, and conference-data version | `clients::tests::calendar_queries_pin_expansion_order_paging_notifications_and_conference_version` | discovery client request kwargs are asserted in `tests/test_calendar.py` |

### Check counts

| Section | Checks |
|---|---|
| 4.1 Discovery and reference resolution | 20 |
| 4.2 Docs reads | 13 |
| 4.3 Docs writes: plain and formatted text | 9 |
| 4.4 The locator resolver | 16 |
| 4.5 Docs writes: locator-addressed edits | 20 |
| 4.6 Docs writes: tables | 18 |
| 4.7 Sheets (incl. 13 A1-notation checks) | 51 |
| 4.8 Files | 25 |
| 4.9 Local-file sandbox and spill retention | 6 |
| 4.10 Audit log | 6 |
| 4.11 Verification gate | 7 |
| 4.12 Request layer, credentials and errors | 6 |
| 4.13 Registered tool surface | 13 |
| 4.14 Calendar | 21 |
| **Offline total** | **231** |
| 5.1 Live Docs (`L1`–`L10`) | 10 |
| 5.2 Live Sheets (`S1`–`S11`) | 11 |
| 5.3 Live Calendar (`C1`–`C12`) | 12 |
| **Total** | **264** |

---

## 5. The live credentialed runs

**Why these exist.** A fake API agrees with the implementation by construction. If the
implementation believes Docs indexes are code points, the fake believes it too, and every offline
check passes. So every claim about how *Google* behaves — index units, what it rejects, whether two
value-input modes really differ upstream — is unfalsifiable offline. Only these runs can falsify
them, and only for the implementation that ran.

All harnesses share a shape. **Double-latched**: `#[ignore]` (Rust) or `skipif` (Python) plus a
`GDRIVE_MCP_LIVE` environment check, so an ordinary test run can never reach Google, and running
the ignored test without the variable prints a skip and returns. **One scratch resource** is
created and removed on teardown — on panic too, in the Rust harnesses, via `catch_unwind_async`.
Its id is printed first, so a failed teardown is recoverable by hand. **All content is synthetic**
(`S<n>-` tokens, `NEEDLE`, `ZAP`, `CELLONE`, or a `gdrive-mcp calendar live check` event); no real
document or event is read or described anywhere in these runs, and no id from a run appears here.

The Rust harnesses and the Python Calendar harness also carry credential-free companion tests that
run in the normal suite, so a harness cannot rot into referencing tools that no longer exist.

### 5.1 Docs locator writes — `L1`–`L10`

    # Python
    GDRIVE_MCP_LIVE=1 uv run pytest tests/test_live_locator.py -v

    # Rust
    GDRIVE_MCP_LIVE=1 cargo test --manifest-path rust/Cargo.toml --test live_locator -- --ignored --nocapture

**Python: run 2026-08-06 — 10 passed. Rust: run 2026-08-07 — 10 passed, 11.6 s.** Same claims,
same pass conditions, same order, against a scratch Doc in the authenticated account's Drive.

| # | Claim under test | Pass condition | Observed |
|---|---|---|---|
| L1 | `documents.get(includeTabsContent=True)` returns `revisionId` | present and non-empty | present, and surfaced by `read_document` as `revision_id` |
| L2 | Stale writes are rejected | the batch fails and the document is unmodified | HTTP 400; text byte-identical afterwards |
| L3 | **UTF-16 index resolution** | exactly the needle removed, no neighbour eaten | `pre a😀b NEEDLE post` → `pre a😀b  post`; span length 6 |
| L4 | Descending multi-range delete | all three occurrences removed, surrounding text intact | 3 occurrences; `one two three` survives (no double-shift damage) |
| L5 | Delete inside a table cell | cell text updated, table structure intact | still 1×2; `CELLONE` → `CELL` |
| L6 | Section delete leaves no empty paragraph | heading and body gone, no orphan blank line | the H2 and its content gone, next section untouched |
| L7 | The final-newline clamp is required | the API rejects the **unclamped** range | HTTP 400; `body_end()` is exactly one below the rejected bound |
| L8 | `replace_text` atomicity and styling | styles land on the new text | the replacement run carries `bold: true`; the old text is gone |
| L9 | Locator anchors are paragraph boundaries | `insert_table(after=…)` succeeds without the 400 remap | 2 cells filled |
| L10 | Cross-run match | resolves and deletes correctly across a style boundary | `hello world tail` → `helld tail` |

**What `L7` does and does not assert.** It proves the unclamped bound is rejected and that
`body_end()` sits exactly one below it. It does not itself delete at the clamped bound — that the
clamped side is *accepted* follows from `L3`/`L4`/`L5`/`L6`/`L10`, all of which resolve through
`body_end` and succeed.

**One structural difference between the two harnesses, worth stating because it is a weakening.**
pytest's module-scoped fixture made `L1`–`L10` ten separately-reported tests over one shared
document. Rust has no fixtures, and the ten checks **mutate the document in order** (`L5` needs the
table `L9` inserted; `L6` needs the text `L8` wrote), so the Rust version is one sequential test
with per-check assertion labels. A failure still names its check, but the run stops at the first
one rather than reporting the rest.

### 5.2 Sheets writes — `S1`–`S11`

    GDRIVE_MCP_LIVE=1 cargo test --manifest-path rust/Cargo.toml --test live_sheets -- --ignored --nocapture

**Rust: run 2026-08-07 — 10 passed, 15.0 s**, against a scratch spreadsheet the harness created and
trashed. **The Python implementation has never had a Sheets live run**, and none is planned here.

Three harness decisions worth knowing when reading a failure: **one tab per check** (`Data`,
`Append`, `Gate`, `Clear`, `Delete`, `Spill`, `Format`, plus `S9`'s `John's Data`), so a failure
names one claim rather than the wreckage of an earlier one; **the harness redirects
`GDRIVE_MCP_FILES_DIR`** at a temporary directory for the whole run — for `S7`'s sake — and restores
it on the way out, so the spill never lands in the operator's real sandbox; and checks that write
over cells they seeded pass `confirm=true` — **`S4` is the one that must not**, since its whole point
is that the write does not happen.

| # | Claim under test | Pass condition | Observed |
|---|---|---|---|
| S1 | **RAW is not a formula.** `write_sheet(value_input="RAW")` of `"=1+1"` | read back under `values=FORMULA` **and** `values=UNFORMATTED`: both the string `"=1+1"`, never `2` | both `"=1+1"`; never the number |
| S2 | **`USER_ENTERED` is a formula.** The same write with `value_input="USER_ENTERED"` | `FORMULA` reads `"=1+1"`, `UNFORMATTED` reads `2` — proving the two modes differ **upstream**, not just in our request | `UNFORMATTED` `2`, `FORMULA` still `"=1+1"` |
| S3 | **`append_rows` inserts, never overwrites** (`insertDataOption=INSERT_ROWS`) | seeded rows byte-identical afterwards, appended rows strictly below them | the appended row landed at `A3`; seeded rows unchanged |
| S4 | **The overwrite gate is real.** `write_sheet` over non-empty cells **without** `confirm` | returns `status=confirmation_required` **and** the sheet is unchanged when re-read | the unconfirmed call changed nothing; the confirmed one landed |
| S5 | **`clear_range` clears exactly its range** | the range is empty and every neighbouring cell outside it survives | `B2` empty, all eight neighbours survived |
| S6 | **`delete_rows` deletes the rows the caller named.** `start_row` is 1-based, the API 0-based | asserted on distinctive per-row content, not on row count — an off-by-one preserves the count and deletes the wrong row | rows 2–3 went, rows 1/4/5 stayed (content-asserted) |
| S7 | **`read_full_sheet` spills a faithful CSV** | content matches the sheet, mode is `0600`, path is inside the sandbox | all three held |
| S8 | **`format_cells` styles exactly its range and leaves values alone** | bold/italic/underline set on the named range and on no cell outside it, every value in the range unchanged | `A1:B1` bold/italic/underlined, row 2 not, values unchanged — **the styling was read back**, not inferred |
| S9 | **A1 quoting survives a hostile tab name.** `add_tab` a tab literally named `John's Data`, then write and read a range on it | both succeed and round-trip the value | all three range spellings (bounded window, bare tab, caller-verbatim `'John''s Data'!A1`) resolved to the intended cells |
| S10 | **The render options differ as documented** | one formula cell reads three different ways | `"=DATE(2020,1,2)"` / `43832` / `"1/2/2020"` |

**Why `S9` is the check most likely to catch a real porting bug.** Two independent transformations
compose on that tab name: `quote_tab` doubles the apostrophe to `'John''s Data'`, and then the Rust
client percent-encodes the range into the URL path, turning each `'` into `%27` — a step
`googleapiclient` performed for the Python implementation and which the Rust client now performs
itself. `SHT-49` pins the encoded string offline using this exact range, but only a live call
establishes that Google accepts it. A wrong answer here is not an error; it is a write to the
*first* tab instead of the named one.

**How `S8` reads the styling back, and what it falls back to.** `S8` was first scoped to "the call
succeeded and the values are unchanged", on the assumption that reading formatting back needed a
trait change. It does not: the sheets-get method takes the field mask as a parameter, so
the harness passes
`sheets(properties(title),data(rowData(values(userEnteredFormat(textFormat(bold,italic,underline))))))`
and inspects the grid. Two caveats stay on the record:

- That method has **no `ranges` parameter**, so the mask returns grid data for every tab, and
  locating the formatted cells means indexing into `rowData`. Tractable on a scratch spreadsheet;
  not on a real one.
- Whether Google returns grid data at all for a masked `spreadsheets.get` (`includeGridData` is
  documented as ignored once a mask is set) is itself an empirical claim — one `S8` settles rather
  than assumes. If the grid comes back empty the harness says so loudly and falls back to the
  guaranteed part of the claim: the values are unchanged and the batch was accepted. **A green `S8`
  that took the fallback path has not verified the styling**, so read the run output, not just the
  exit code. The 2026-08-07 run did **not** take the fallback.

### 5.3 Calendar — `C1`–`C12`

    # Python
    GDRIVE_MCP_LIVE=1 uv run pytest tests/test_live_calendar.py -v

    # Rust
    GDRIVE_MCP_LIVE=1 cargo test --manifest-path rust/Cargo.toml --test live_calendar -- --ignored --nocapture

**Passed against a Google account in both implementations on 2026-09-08** (Python: 2 tests in
9.48 s; Rust: 1 live test in 7.08 s). Each creates
one synthetic two-occurrence series about 400 days in the future on the authenticated user's
primary calendar, sets `send_updates=none`, prints its caller-supplied event id, and removes the
series in teardown (including after a Rust panic). No existing calendar or event is used.

| # | Claim under test | Pass condition | Observed |
|---|---|---|---|
| C1 | CalendarList access | primary calendar appears and authenticated email is available | passed Python + Rust, 2026-09-08 |
| C2 | Create confirmation gate | preview contains the supplied id and a subsequent `events.get` is 404/410 | passed Python + Rust, 2026-09-08 |
| C3 | Create dry-run | dry-run contains the supplied id and a subsequent `events.get` is 404/410 | passed Python + Rust, 2026-09-08 |
| C4 | Rich confirmed create + `get_event` | id, summary, location, recurrence, visibility and ETag round-trip | passed Python + Rust, 2026-09-08 |
| C5 | Expanded ordered listing, search and pagination | two instances are returned one per page and both point to the master id | passed Python + Rust, 2026-09-08 |
| C6 | Free/busy | the first occurrence covers the expected busy interval | passed Python + Rust, 2026-09-08 |
| C7 | ETag concurrency | an out-of-band patch advances the ETag; a patch carrying the stale ETag fails HTTP 412 and its text does not land | passed Python + Rust, 2026-09-08 |
| C8 | Update confirmation gate | preview succeeds and a fresh get proves the summary unchanged | passed Python + Rust, 2026-09-08 |
| C9 | Confirmed partial update | summary, description and availability round-trip through the tool | passed Python + Rust, 2026-09-08 |
| C10 | RSVP | preview names the transition; confirmed response returns the self attendee as tentative | passed Python + Rust, 2026-09-08 |
| C11 | Delete confirmation gate | preview identifies a series and a fresh get proves it still exists | passed Python + Rust, 2026-09-08 |
| C12 | Confirmed series delete | delete reports success and a fresh get returns 404/410 or a cancelled tombstone | passed Python + Rust, 2026-09-08 |

These checks intentionally keep attendee mail disabled. They verify `send_updates=none` against
Google but do not send real notifications merely to prove that `externalOnly`/`all` sends them.

---

## 6. Empirical findings

Facts about the Docs and Sheets APIs, established by live runs, that the design rests on and that
cannot be re-derived from this repository's source. They are recorded here as a standing record so
a future change does not have to rediscover them — or, worse, contradict them and pass offline.

1. **Empty-cell indexes in a fresh table are deterministic.** An empty `R×C` table inserted by
   `insertTable` has empty-cell first-paragraph indexes with stride **+2** between columns in a row
   and **+3** crossing a row boundary (row stride `2*C + 1`). For a 2×3 table the six cells landed
   at `5, 7, 9, 12, 14, 16`. *(2026-07-18.)* The implementation still re-fetches rather than
   computing these in closed form — one fewer round trip is not worth pinning a hidden API
   invariant — but this is why the running-offset fill in `TBL-4` is sound.

2. **`insertText` with empty text is REJECTED by the API.** So an empty cell must be **skipped**,
   not filled with `""` (`TBL-5`), an empty replacement must emit only the delete (`LOC-27`), and a
   zero-length text segment must emit no request at all (`TBL-17`). Three separate design decisions
   trace to this one fact.

3. **`insertTable` inserts a newline BEFORE the table.** The new table's `startIndex` is
   `requested_index + 1`, **never** the requested index. "Find the table beginning exactly at
   `start`" is therefore always wrong — which is why table selection snapshots the pre-insert
   `startIndex` set and picks the one that is new (`TBL-2`).

4. **A Doc created through the API returns its content under `tabs`, with NO top-level `body` key
   at all.** The first live-harness draft assumed `doc["body"]["content"]` and raised on three
   checks. Production code was already correct — write-tab resolution tries `tabs` first — so this
   was a test-only defect, but it means **both** live runs exercised the **tabbed** path throughout:
   every request carried a `tabId`. The untabbed path is covered offline only (`DR-7`, `LOC-24`).

5. **Docs `Range`/`Location` indexes are UTF-16 code units**, confirmed against a real document
   (`L3`). Counting code points is off by one per astral character. This is the entire reason
   `locate` exists as a module separate from the chunker, which counts code points and must keep
   doing so (`DR-13`).

6. **Sheets has no revision-pinning equivalent to the Docs `writeControl`.** `values.update`,
   `values.append`, `values.clear` and the spreadsheet `batchUpdate` accept no `writeControl`, no
   etag and no `If-Match`, because the API offers no such option on these methods. Checked against
   the source in both implementations, not assumed. The consequence is in §8, and it is the sharpest
   asymmetry between the Docs and Sheets write paths.

---

## 7. Defects this verification found

Five, with the mechanism and the blast radius. Four were found by *writing* a check, not by a tool
failing in use — which is the argument for the pure and request layers existing at all.

### 7.1 Rust `files.get` was not sending `supportsAllDrives=true`

**Mechanism.** The Rust client's Drive file-get query builder omitted the parameter. Python sends it
at every call site, and `files.py` states the invariant in a header comment — but nothing in either
language pinned it *per endpoint*.

**Blast radius.** Every item that lives on a **shared drive** returned 404 through `resolve_link`,
`get_metadata`, `read_file_as_text`, `download_file`, `move_file`, `rename_file`, and
`upload_file`'s overwrite pre-check. Seven tools, silent, and indistinguishable from "the file does
not exist" — so a user would have concluded their link was wrong.

**How it was found.** By the test written to close the gap, on its first run, before any mutation
was attempted. A port defect, not a shared one.

**Now pinned twice** (`REQ-1`, `REQ-2`): once by a rule table naming every Drive method and whether
it accepts the parameter, and once by a source-scanning test that walks the client's own `impl`
block and fails if a **new** Drive endpoint is added without an entry in that table. The second is
the load-bearing one; the first would have passed forever if nobody had listed the endpoint.

### 7.2 `find_section` could return a range through the body's final newline

**Mechanism.** For a heading that is the last element, `max(end, min(stop, body_end))` returns the
heading's `endIndex`, which is `body_end + 1`.

**Blast radius.** `deleteContentRange` rejects that range, so `delete_text(section=…)` on the last
section of a document failed with an opaque 400. Not corruption — a hard failure on a legitimate
call.

Fixed by clamping **last**: `min(max(end, stop), body_end)`. Pinned by `LOC-9`, and the live check
`L7` confirms the unclamped bound really is rejected rather than merely assumed to be.

### 7.3 Block anchors could silently land mid-table-cell

**Mechanism.** `paragraph_bounds` scans only top-level elements. A match inside a table cell fell
through to a `return index, index` fallback.

**Blast radius.** `insert_table(after=<text inside a cell>)` would anchor mid-cell and 400 —
opaquely, since the caller passed a locator precisely to avoid thinking about indexes. Worse, the
failure mode was a *plausible-looking* index rather than an error, so the guarantee "for block
content, locators land on a paragraph boundary by construction" was false.

`paragraph_bounds` now returns `None` and the caller raises a directed error naming the table cell.
Pinned by `LOC-14` and `LOC-34`.

### 7.4 Four locator checks were unpinned in the Rust suite; three of them in Python too

Found by auditing check by check rather than by running the suite — in every case a Rust test
*touched* the area without asserting the claim.

1. **`replace_text`'s dry-run branch** was dead to the suite: nothing drove it. (Python: still
   absent — `LOC-20`.)
2. **`replace_text`'s `writeControl`** could have been deleted without failing anything: the replace
   tests read only `body["requests"]`, never the whole body. (Python's test is named plural but
   drives `delete_text` only — `LOC-23`.)
3. **The "per range" half of `replace_text`'s request shape**: every replace test resolved exactly
   **one** range, where per-range delete+insert pairing and grouped emission produce identical
   streams. (Python: same — `LOC-22`.)
4. **The paragraph-boundary check asserted the anchor's *value*, not that it **is** a boundary** —
   which is the actual claim. Python asserted membership in `{startIndexes} ∪ {body_end}`; the Rust
   now does too (`LOC-35`).

Each new test was confirmed by breaking the production line it covers, watching it fail, and
reverting — including two table-selection cases where the **pre-existing** test stayed green under
the mutation, which is precisely the evidence that it never covered them.

Three of the four are **parity gaps, not port regressions**: the Python behaves correctly and
nothing in `tests/` would notice if it stopped. They remain open on the Python side.

### 7.5 A dangling symlink could escape the local sandbox in the Rust port

**Mechanism.** `canonicalize()` cannot resolve a path whose tail does not exist, so it fails
outright on any new spill destination. Backing off to the longest existing ancestor and joining the
rest lexically leaves a **dangling** symlink unexpanded: a link at `<sandbox>/link` pointing outside
passes the containment check as `<sandbox>/link` and is then written **through**.

**Blast radius.** A destination path an agent controls could write outside
`GDRIVE_MCP_FILES_DIR` — the exact failure the sandbox exists to prevent. Requires a symlink
already planted inside the sandbox, so it is an escalation rather than a one-step compromise, but
the sandbox's whole claim is that a planted file cannot become an escape.

Containment now walks the path one component at a time, expanding every symlink as it is met, using
`symlink_metadata` (which sees the link whether or not its target exists) and letting missing
components pass through. Pinned by `SBX-1`, which covers a planted symlink, a **dangling** symlink,
a symlink **cycle**, and a mis-cased escape on a case-insensitive filesystem — none of which either
language covered before this pass.

### 7.6 The overwrite gate could be walked past by a formula that renders empty

**Both implementations, not a port defect.** `write_sheet`'s pre-check read the target with
`UNFORMATTED_VALUE`, and the non-empty count treats `""` as empty. A cell holding
`=IF(A9=1,"","x")`, or any lookup that currently misses, therefore rendered as `""` and read as
**empty** — so the gate did not fire and a `confirm`-less write destroyed the formula. The blast
radius is the gate's entire purpose: the one guarantee the tool makes about not losing data, absent
for exactly the cells whose contents are least visible.

Fixed by reading the pre-check with `FORMULA` in both languages. Formula *text* is never empty, so
the gate fires. The same read feeds `dry_run`'s `before`, which now shows the formula a caller would
destroy rather than the value it happened to produce — and the docstring says so, in both
implementations, byte-identically (`SRF-11` enforces that).

Pinned three ways, because the mechanism and the behaviour are separate claims: offline in each
language by asserting the render option the pre-check requests (`SHT-50`, plus `SHT-51` for the
`dry_run` half) —
which is all a fake can falsify, since it returns one canned grid regardless — and live by `S11`,
whose first assertion is that Google really does render the seeded formula as empty. Without that
precondition the check would pass vacuously on an API that behaved differently.

### 7.7 A corrupt PDF leaked pypdf's exception type to the agent

**Python only; the Rust port already curated it.** `read_file_as_text`'s `api_errors` decorator
wraps `HttpError` and nothing else, so a truncated or encrypted PDF surfaced as a raw
`pypdf.errors.PdfStreamError: Stream has ended unexpectedly` — an exception type an agent cannot act
on, and which points at the request rather than at the file. Reproduced before fixing.

Now raised as `could not extract text from PDF: <detail>`, matching the Rust message. The `except`
is deliberately broad: pypdf fails as `PdfStreamError`, `PdfReadError`, `DependencyError` on an
encrypted file, and a bare `KeyError` on a malformed xref, and none of that set is part of this
tool's contract. Pinned by `FIL-25` in both languages, mutation-verified.

---

## 8. What this does **not** prove

Stated so a green run is not over-read.

**Concurrency, beyond revision pinning on Docs.** `L2` proves a stale Docs write fails; it does not
prove resolve→write is atomic. The window between the resolving `get` and the `batchUpdate` remains.
`requiredRevisionId` converts it from silent corruption into a loud failure — that is the designed
guarantee, and nothing stronger.

**The Sheets read→write window is unguarded, and that is not fixable with the current API.** Per §6
finding 6, Sheets offers no `writeControl` equivalent. Consequently:

- `write_sheet`'s overwrite pre-check (`values.get`) and its write (`values.update`) are two
  separate round trips. A write arriving between them is silently overwritten; the confirm gate can
  be truthfully told "0 non-empty cells" and the write still destroys data that landed a moment
  later.
- The same window exists in `clear_range` (pre-read → clear) and `delete_rows` (pre-read →
  `deleteDimension`), where it is **worse**: `deleteDimension` names row *positions*, so a
  concurrent row insert above the target shifts which rows die.
- **No check here, offline or live, exercises any of this.** Doing so needs two concurrent clients,
  which the one-sequential-test harness pattern deliberately does not provide.
- **Accepted, not mitigated.** The Docs tools pin a revision because Docs offers one; Sheets
  offers nothing equivalent, so there is no version of these tools that closes the window. It is
  deliberately *not* mentioned in the `write_sheet`/`clear_range`/`delete_rows` docstrings: an
  agent told about it has no action available, and the docstrings are the agent-facing schema,
  where every line competes for attention with one it can act on.

**The overwrite gate no longer measures rendered values** — see §7.6. It reads the target with
`FORMULA`, so a cell whose formula evaluates to the empty string still counts as data and still
trips the gate. What remains unproven is the *converse*: a cell that is genuinely empty but carries
formatting, a note, or a data-validation rule is not data to the gate, and a `confirm`-less write
over it is not gated. That is deliberate — `values.get` cannot see any of those — but nothing states
it to the caller.

**`replace_text` with tables** is out of scope by construction (`LOC-25` refuses it) and remains
unverified.

**Slides is not supported at all.** Checked against the source in both implementations, not assumed:
neither builds a Slides client (Python builds only `drive`, `sheets`, `docs`; the Rust client has no
Slides method), and no tool targets a presentation's structure. A presentation URL parses to kind
`"file"` (`DSC-2`) and a presentation's mime type maps to kind `"file"` (`DSC-9`), so a deck is
reachable **only** through the generic Drive tools: `resolve_link`/`get_metadata` see it,
`export_file` can render it to pptx/pdf, `read_file_as_text` exports its text, `move_file`/
`rename_file` can relocate it, and `download_file` refuses it as Google-native. Slide content
cannot be read structurally or written at all. Nothing here verifies Slides behaviour because there
is no Slides behaviour to verify.

**Table reads are lossy, deliberately.** Cell **emphasis** is discarded on read (the reader emits
only text-run content), and a multi-paragraph cell is flattened onto one row (`DR-4`). So a styled
cell written by this server reads back plain, and re-writing what was read loses the emphasis. The
round-trip claim is scoped to **table structure plus plain cell text** — pipes *are* escaped, so
structure is faithful (`DR-2`) — and the README says so. Cell alignment from the delimiter row is
not supported at all.

**Locators match the document's plain text, not its rendered markdown.** A needle of `"# Title"`
will not match a `HEADING_1` paragraph whose text is `"Title"`. `LOC-10` pins the heading case — its
fixture's heading paragraph holds `"Results"`, not `"# Results"`, and that is what resolves. **The
general trap is not stated in the agent-facing schema.** What `delete_text` and `replace_text`
document is that `match` is "a literal substring, case-sensitive, must lie within a single
paragraph"; what `insert_text`'s docstring and the README's Conventions block warn about is a
*different* trap — that character offsets into `read_document`'s output are not valid Docs indexes
because that output is rendered markdown. Neither says the **needle** is matched against plain text,
so an agent that read a heading as `"# Title"` and passed that string back gets an opaque "no match"
rather than a directed error. Open: say so in the two locator docstrings.

**Shared drives are pinned at the request level only.** `REQ-1`–`REQ-3` prove the parameters go out.
No live call has been made against an item that actually lives on a shared drive — which is the
condition under which the §7.1 defect manifested.

**Google's own semantics beyond `S1`–`S11` and `L1`–`L10`.** Number formatting, locale-dependent
value parsing under `USER_ENTERED`, protected ranges, filters, named ranges, cell merges and
conditional formatting are untouched by these tools and unexamined here. A `write_sheet` into a
protected or merged range is covered by no check.

**Large-sheet and large-file behaviour.** `read_full_sheet` reads a whole tab in one `values.get`
with no paging. Nothing establishes where that falls over, and the sandbox has no size cap — a very
large tab spills a very large CSV.

**The audit log is checked for what it omits, not for completeness.** `AUD-2`–`AUD-4` prove content
never reaches it; nothing proves every call reaches it.

**Authentication is automated at the serialisation layer only.** `REQ-5` proves the token file
round-trips through the shape `google-auth` writes. A real incremental OAuth re-consent completed on
2026-09-08 and the cached token exposed all four configured scopes; refresh behavior remains
unverified by an automated live test.

**Calendar live verification passed in both implementations on 2026-09-08.** `C1`–`C12`
deliberately do not test attendee email delivery (`externalOnly`/`all`), Google Meet creation (which can depend on
Workspace policy), delegated calendars, or responding as a genuinely external invitee. Offline
tests pin those request shapes and safety defaults, but cannot establish tenant-specific behavior.

**`format_cells`' effect on real formatting is unverified in Python.** `S8` settles it for Rust.
There is no Python live Sheets harness.

### Deliberate port divergences

Not defects. Each is a decision, and each is pinned or documented as such rather than left implicit.

| Divergence | Why |
|---|---|
| A lower-case `"user_entered"` is **refused** by the Rust port rather than upper-cased (`SHT-19`) | The Python signature is `Literal["RAW", "USER_ENTERED"]`, so pydantic rejected it before the function body ran and the body's `.upper()` was unreachable. Reproducing the `.upper()` would let a caller enable formula interpretation through a path that used to fail closed. A divergence at the direct-call level; parity at the registered level. |
| A1 cells accept only ASCII digits | Python's `\d` also matched other Unicode digits. Affects only which cell strings parse, never where a parsed cell lands. |
| The filename sanitiser keeps Unicode combining marks where Python replaced them with `_` | Affects only how a **spilled filename** is spelled. Containment and content are unaffected. |
| PDF text extraction uses a different library | Extracted text may differ in whitespace for unusual PDFs. |
| Two markdown inline patterns use a lookaround-capable regex engine, with `\w`/`\s` spelled out | Rust's default engine has no lookaround, and the two engines define those classes differently. Pinned case for case by `DW-7`. |
| Argument handling reproduces **both** of FastMCP's layers, not just the strict one (`SRF-9`) | MCP clients really do send a list as a JSON string and a boolean as `"true"`; dropping pydantic's lax coercion would break callers that work against the Python server. |
| The Rust live locator harness is one sequential test, not ten (§5.1) | Rust has no module-scoped fixtures and the ten checks mutate the document in order. This is a **weakening**: a failure stops the run instead of reporting the remaining checks. |

---

## 9. How to run everything

```bash
# Python — the behaviour of record
uv run pytest -q                                   # 249 passed, 11 skipped (the live harnesses)

# Rust — unit + integration, no credentials needed
cargo test --manifest-path rust/Cargo.toml         # 420 passed, 3 ignored (the live harnesses)
cargo clippy --all-targets --manifest-path rust/Cargo.toml
cargo fmt --check --manifest-path rust/Cargo.toml

# Cross-implementation surface parity (SRF-11)
cargo build --release --manifest-path rust/Cargo.toml
python3 scripts/diff_tool_surface.py               # tool surfaces match: 37 tools

# Live, credentialed. Writes disposable resources to the authenticated account. Never run in CI.
GDRIVE_MCP_LIVE=1 uv run pytest tests/test_live_locator.py -v
GDRIVE_MCP_LIVE=1 uv run pytest tests/test_live_calendar.py -v
GDRIVE_MCP_LIVE=1 cargo test --manifest-path rust/Cargo.toml --test live_locator -- --ignored --nocapture
GDRIVE_MCP_LIVE=1 cargo test --manifest-path rust/Cargo.toml --test live_sheets  -- --ignored --nocapture
GDRIVE_MCP_LIVE=1 cargo test --manifest-path rust/Cargo.toml --test live_calendar -- --ignored --nocapture
```

Without `GDRIVE_MCP_LIVE` the Rust live tests are `#[ignore]`d and, if run anyway with `--ignored`,
print a skip and return; the Python ones are `skipif`-ed. Each live harness prints its scratch
resource id before it starts writing, so a failed teardown is recoverable by hand.

## 10. Exit checklist

Checked where a test or a recorded run backs the box. Unchecked where work is genuinely open.

**Offline coverage**

- [x] `DSC-1`–`DSC-20` (Discovery and reference resolution) present in `rust/src/ids.rs`, `rust/src/tools/discovery.rs`, `tests/test_ids.py`, `tests/test_tools_unit.py`
- [x] `DR-1`–`DR-13` (Docs reads) present in `rust/src/tools/docs.rs`, `rust/src/md.rs`, `rust/src/chunking.rs`, `tests/test_tools_unit.py`, `tests/test_chunking.py`
- [x] `DW-1`–`DW-9` (Docs text writes) present in `rust/src/tools/docs.rs`, `rust/src/md.rs`, `tests/test_tools_unit.py`, `tests/test_md.py`
- [x] `LOC-1`–`LOC-16` (the resolver) present in `rust/src/locate.rs` and `tests/test_locate.py`
- [x] `LOC-17`–`LOC-36` (locator-addressed edits) present in `rust/src/tools/docs.rs`, `rust/tests/server_integration.rs`, `tests/test_tools_unit.py`, `tests/test_server_integration.py`
- [x] `TBL-1`–`TBL-18` (tables) present in `rust/src/tools/docs.rs`, `rust/src/md.rs`, `tests/test_tables.py`
- [x] `SHT-1`–`SHT-49` (Sheets and A1) present in `rust/src/a1.rs`, `rust/src/tools/sheets.rs`, `rust/src/clients.rs`, `tests/test_a1.py`, `tests/test_tools_unit.py`
- [x] `FIL-1`–`FIL-24` (Files) present in `rust/src/tools/files.rs`
- [x] `SBX-1`–`SBX-6` (sandbox and retention) present in `rust/src/localfs.rs` and `tests/test_localfs.py`
- [x] `AUD-1`–`AUD-6`, `GAT-1`–`GAT-7`, `REQ-1`–`REQ-6`, `SRF-1`–`SRF-13` present in `rust/src/{audit,gating,clients,auth,config,error,server,guard,args}.rs`, `rust/src/tools/mod.rs`, `rust/tests/server_integration.rs`, `tests/test_audit.py`, `tests/test_gating.py`, `tests/test_guard.py`, `tests/test_server_integration.py`
- [x] `CAL-1`–`CAL-21` (Calendar reads, mutations, OAuth, concurrency and privacy) present in `rust/src/tools/calendar.rs`, `rust/src/clients.rs`, `rust/src/auth.rs`, `tests/test_calendar.py`, and both live harnesses

**Suite health**

- [x] `uv run pytest -q` green — 249 passed, 11 skipped
- [x] `cargo test --manifest-path rust/Cargo.toml` green — 420 passed, 3 ignored
- [x] `cargo clippy --all-targets` and `cargo fmt --check` clean
- [x] `scripts/diff_tool_surface.py` green at 37/37 against a release binary

**Live runs**

- [x] `L1`–`L10` against a live scratch Doc, **Python** — 10 passed, 2026-08-06
- [x] `L1`–`L10` against a live scratch Doc, **Rust** — 10 passed, 11.6 s, 2026-08-07
- [x] `S1`–`S11` against a live scratch spreadsheet, **Rust** — 11 passed, 15.3 s, 2026-08-07; `S8` read the styling back rather than taking its fallback, and `S11`'s precondition confirmed the gate's blind spot was genuinely reachable
- [ ] `S1`–`S11` against a live scratch spreadsheet, **Python** — never run, and no Python Sheets
      harness exists. Partly mitigated: `SRF-11` plus the per-request offline tests show both
      implementations send byte-identical requests, so the checks that are purely about *Google's*
      semantics (`S1`, `S2`, `S10`) transfer. The ones that do not are the implementation's own
      logic — the gate (`S4`, `S11`), the spill (`S7`) and A1 quoting (`S9`)
- [x] `C1`–`C12` against a live scratch Calendar event, **Python and Rust** — Python 2 tests passed
      in 9.48 s and Rust 1 live test passed in 7.08 s, 2026-09-08; incremental Calendar re-consent
      succeeded and both scratch series were deleted

**Defects**

- [x] §7.1 `supportsAllDrives` — fixed and pinned twice (rule table + source-scanning net)
- [x] §7.2 `find_section` final-newline clamp — fixed, pinned offline and confirmed live by `L7`
- [x] §7.3 mid-cell block anchors — refused with a directed error, pinned
- [x] §7.5 dangling-symlink sandbox escape — fixed, pinned alongside cycles and case-insensitive escapes
- [x] §7.4 the three **Python** parity gaps closed — `LOC-20`, `LOC-22`, `LOC-23` now pinned in both
      languages; each mutation-verified, and the pre-existing single-range test demonstrably passed
      against the broken multi-range code, which is what made the gap real
- [x] §7.6 the overwrite gate's formula blind spot — fixed in both languages, pinned offline in each
      and live by `S11`
- [x] §7.7 the corrupt-PDF error leak — curated in Python to match Rust, mutation-verified

**Open work**

- [x] The `write_sheet` gate's formula blind spot — decided and fixed: the pre-check reads `FORMULA`
      (§7.6). The residual limit, a genuinely empty cell carrying formatting or validation, is stated
      in §8
- [x] The unguarded Sheets read→write window — **recorded as an accepted risk** (§8), deliberately
      not put in the docstrings: Sheets exposes no revision-pinning equivalent, so there is no action
      an agent could take on being told, and three tools would carry a warning about a race none of
      them can avoid. Revisit if the API ever grows a `writeControl`
- [x] `TBL-12` (table cell inline styling) now pinned in Python, at cell-local offsets and across an
      astral character, plus the write-path half (spans anchored on each cell's post-shift index)
- [x] `AUD-6` (audit logging never raises on an unwritable path) now pinned in Rust
- [x] `LOC-11`'s no-folding pin extended to all four locator tools in both languages
- [x] The plain-text-vs-rendered-markdown needle trap is now stated in the `delete_text` and
      `replace_text` docstrings, identically in both languages (`SRF-11` keeps them identical)
- [x] All ten previously-untested tools now have Python tool-level coverage — `read_comments`,
      `add_comment`, `extract_images`, `read_file_as_text`, `download_file`, `export_file`,
      `create_spreadsheet`, `add_tab`, `resolve_link`, `get_metadata` — asserting the requests they
      build, not just their result shapes
- [ ] No live call against an item that actually lives on a shared drive (§8) — the condition under which §7.1 manifested
- [ ] Concurrency is unexercised in both languages; it needs a two-client harness the current pattern does not provide
