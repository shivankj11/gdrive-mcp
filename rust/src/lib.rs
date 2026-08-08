//! Google Drive / Docs / Sheets MCP (read/write) for agents — browser-OAuth, local stdio.
//!
//! A Rust re-implementation of the Python `gdrive_mcp` package, module-for-module:
//!
//! | module      | role                                                              |
//! | ----------- | ----------------------------------------------------------------- |
//! | [`config`]  | OAuth scope + on-disk locations                                    |
//! | [`auth`]    | one-time browser consent, cached-token load/refresh                |
//! | [`clients`] | the Google API surface, behind a trait the tool tests can fake      |
//! | [`ids`]     | Drive URL/ID parsing                                               |
//! | [`a1`]      | A1-notation helpers for Sheets                                     |
//! | [`chunking`]| bounded, natural-boundary paging for read tools                    |
//! | [`md`]      | the small markdown dialect -> Docs styling requests                |
//! | [`locate`]  | content locators -> Docs index ranges                              |
//! | [`localfs`] | the local file-I/O sandbox and its retention sweep                 |
//! | [`audit`]   | append-only, content-free audit log                                    |
//! | [`guard`]   | confirm-before-destructive previews                                |
//! | [`gating`]  | the per-action manual-verification gate                            |
//! | [`tools`]   | the MCP tools themselves                                           |
//! | [`server`]  | tool registry + stdio MCP wiring                                   |

// The module docs port the Python docstrings verbatim, including their aligned continuation
// lines; clippy reads that alignment as an overindented list item.
#![allow(clippy::doc_overindented_list_items)]
// Several tests take a mutex around a process-global env var and hold it across an await so the
// setting cannot change mid-test. `#[tokio::test]` runs them on a current-thread runtime, so the
// guard is never sent across threads.
#![cfg_attr(test, allow(clippy::await_holding_lock))]

/// One process-wide lock for every test that mutates a `GDRIVE_MCP_*` variable.
///
/// Environment variables are per-process, not per-test, and the test binary is threaded — a
/// per-module lock lets `localfs`'s sandbox tests and the file tools' sandbox tests retarget
/// `GDRIVE_MCP_FILES_DIR` out from under each other. Every such test takes THIS lock.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod a1;
pub mod args;
pub mod audit;
pub mod auth;
pub mod chunking;
pub mod clients;
pub mod config;
pub mod error;
pub mod gating;
pub mod guard;
pub mod ids;
pub mod localfs;
pub mod locate;
pub mod md;
pub mod server;
pub mod tools;
