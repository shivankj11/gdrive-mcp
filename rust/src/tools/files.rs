//! Drive file tools: read-as-text, download, upload/replace, move, rename, export.
//!
//! Local file I/O (upload source, download/export destinations) is confined to the sandbox in
//! `localfs` — a prompt-injected agent cannot read arbitrary local files or write outside it.
//! `supportsAllDrives=True` is set on every Drive call that accepts it so shared-drive items work
//! (the `export` method has no such parameter); that lives in [`crate::clients`] here, so a tool
//! cannot forget it.

use std::path::PathBuf;

use base64::Engine as _;
use serde_json::{json, Map, Value};

use super::{merge, ToolDef, ToolOutput};
use crate::args::Args;
use crate::chunking::{paginate, DEFAULT_MAX_CHARS};
use crate::clients::GoogleApi;
use crate::config::chmod;
use crate::error::{Result, ToolError};
use crate::guard::preview_response;
use crate::ids::parse_ref;
use crate::localfs::{safe_read_path, safe_write_path};

/// Above this size download/export write to the sandbox instead of returning base64 inline.
const INLINE_MAX: usize = 5 * 1024 * 1024;

const EXPORT_MIME: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    ("csv", "text/csv"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("html", "text/html"),
];

const GOOGLE_EXPORT_TEXT: &[(&str, &str)] = &[
    ("application/vnd.google-apps.document", "text/plain"),
    ("application/vnd.google-apps.spreadsheet", "text/csv"),
    ("application/vnd.google-apps.presentation", "text/plain"),
];

fn lookup(table: &[(&str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// `sorted(_EXPORT_MIME)` rendered the way Python printed a list of keys: `['csv', 'docx', ...]`.
/// Sorted here rather than written out so the message cannot drift from the table.
fn export_formats() -> String {
    let mut keys: Vec<&str> = EXPORT_MIME.iter().map(|(k, _)| *k).collect();
    keys.sort_unstable();
    let quoted: Vec<String> = keys.iter().map(|k| format!("'{k}'")).collect();
    format!("[{}]", quoted.join(", "))
}

/// Python indexed API responses directly (`meta["name"]`), so a missing key was a hard error and
/// never a silent default. Report it as a message instead of panicking inside a tool call.
fn field(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::msg(format!("Google API response is missing '{key}'")))
}

/// `.get(key)` — absent renders as JSON null, exactly as Python's `dict.get` did.
fn optional(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

/// Write bytes into the sandbox, 0600 so no other local account can read spilled data.
fn spill(dest_path: Option<&str>, default_name: &str, data: &[u8]) -> Result<PathBuf> {
    let path = safe_write_path(dest_path, default_name)?;
    std::fs::write(&path, data)?;
    chmod(&path, 0o600);
    Ok(path)
}

/// Python's truthiness test on an optional string argument: `""` is "unset", like `None`.
fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

async fn read_file_as_text(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let chunk = args.i64_or("chunk", 0)?;
    let max_chars = args.i64_or("max_chars", DEFAULT_MAX_CHARS)?;

    let r = parse_ref(&item)?;
    let meta = api.drive_files_get(&r.id, "id,name,mimeType,size").await?;
    let mime = field(&meta, "mimeType")?;
    let mut extra: Map<String, Value> = Map::new();
    let text = if let Some(export_mime) = lookup(GOOGLE_EXPORT_TEXT, &mime) {
        let data = api.drive_export(&r.id, export_mime).await?;
        // Python decoded with errors="replace"; `from_utf8_lossy` substitutes the same U+FFFD.
        String::from_utf8_lossy(&data).into_owned()
    } else {
        let raw = api.drive_get_media(&r.id).await?;
        if mime == "application/pdf" {
            let pages = pdf_extract::extract_text_from_mem_by_pages(&raw)
                .map_err(|e| ToolError::msg(format!("could not extract text from PDF: {e}")))?;
            extra.insert("pages".to_string(), Value::from(pages.len() as u64));
            pages.join("\n")
        } else if mime.starts_with("text/") || mime == "application/json" || mime == "application/csv" {
            String::from_utf8_lossy(&raw).into_owned()
        } else {
            return Err(ToolError::msg(format!(
                "{mime} is not text-extractable; use download_file instead."
            )));
        }
    };

    let mut out = json!({"name": field(&meta, "name")?, "mime_type": mime});
    merge(&mut out, extra);
    merge(&mut out, paginate(&text, chunk, max_chars));
    Ok(out.into())
}

async fn download_file(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let dest_path = nonempty(args.opt_str("dest_path")?);

    let r = parse_ref(&item)?;
    let meta = api.drive_files_get(&r.id, "id,name,mimeType,size").await?;
    let mime = field(&meta, "mimeType")?;
    if mime.starts_with("application/vnd.google-apps") {
        // Native files have no downloadable bytes; only an export has a representation.
        return Err(ToolError::msg(
            "Google-native file: use export_file or read_file_as_text, not download_file.",
        ));
    }
    let raw = api.drive_get_media(&r.id).await?;
    let name = field(&meta, "name")?;
    if dest_path.is_some() || raw.len() > INLINE_MAX {
        let path = spill(dest_path.as_deref(), &name, &raw)?;
        // Only a size-driven spill needs explaining; an explicit dest_path was the caller's choice.
        let note = match dest_path {
            Some(_) => Value::Null,
            None => Value::String(format!(
                "{} bytes exceeded {INLINE_MAX} inline limit; written to file",
                raw.len()
            )),
        };
        return Ok(json!({
            "name": name,
            "mime_type": mime,
            "bytes": raw.len(),
            "path": path.display().to_string(),
            "note": note,
        })
        .into());
    }
    Ok(json!({
        "name": name,
        "mime_type": mime,
        "bytes": raw.len(),
        "base64": base64::engine::general_purpose::STANDARD.encode(&raw),
    })
    .into())
}

async fn upload_file(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let name = args.req_str("name")?;
    let source_path = nonempty(args.opt_str("source_path")?);
    let content = args.opt_str("content")?;
    let parent = nonempty(args.opt_str("parent")?);
    let mime_type = nonempty(args.opt_str("mime_type")?);
    let replace_id = nonempty(args.opt_str("replace_id")?);
    let confirm = args.bool_or("confirm", false)?;

    if source_path.is_none() && content.is_none() {
        return Err(ToolError::msg("provide source_path or content"));
    }
    // The media is assembled first, exactly as in Python, so a sandbox escape in source_path is
    // refused before any Drive call happens.
    let (data, mime) = match &source_path {
        Some(p) => {
            let path = safe_read_path(p)?;
            // Python passed mimetype=None, and MediaFileUpload then ran mimetypes.guess_type on
            // the filename, falling back to application/octet-stream only when that failed. Guess
            // the same way: uploading a .pdf as octet-stream would store the wrong type in Drive.
            let mime = mime_type.unwrap_or_else(|| {
                mime_guess::from_path(&path).first_raw().unwrap_or("application/octet-stream").to_string()
            });
            (std::fs::read(&path)?, mime)
        }
        // `content` is Some on this branch (the guard above), and "" is legitimate content.
        None => {
            (content.unwrap_or_default().into_bytes(), mime_type.unwrap_or_else(|| "text/plain".to_string()))
        }
    };

    if let Some(replace_id) = replace_id {
        let rid = parse_ref(&replace_id)?.id;
        if !confirm {
            let old = api.drive_files_get(&rid, "id,name,mimeType,modifiedTime").await?;
            return Ok(preview_response(
                "upload_file(replace)",
                json!({"replacing": old, "with_name": name}),
            )
            .into());
        }
        let f = api
            .drive_files_update_media(&rid, &json!({"name": name}), &mime, data, "id,name,webViewLink")
            .await?;
        return Ok(json!({
            "id": field(&f, "id")?,
            "name": field(&f, "name")?,
            "url": optional(&f, "webViewLink"),
            "replaced": true,
        })
        .into());
    }

    let mut body = json!({"name": name});
    if let Some(parent) = parent {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("parents".to_string(), json!([parse_ref(&parent)?.id]));
        }
    }
    let f = api.drive_files_create_media(&body, &mime, data, "id,name,webViewLink").await?;
    Ok(json!({
        "id": field(&f, "id")?,
        "name": field(&f, "name")?,
        "url": optional(&f, "webViewLink"),
    })
    .into())
}

async fn move_file(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let parent = args.req_str("parent")?;
    let confirm = args.bool_or("confirm", false)?;

    let r = parse_ref(&item)?;
    let dest = parse_ref(&parent)?.id;
    let meta = api.drive_files_get(&r.id, "id,name,parents").await?;
    if !confirm {
        return Ok(preview_response(
            "move_file",
            json!({"file": field(&meta, "name")?, "from": optional(&meta, "parents"), "to": dest}),
        )
        .into());
    }
    // Drive adds a parent without dropping the old ones, so every current parent has to be named
    // in removeParents for this to be a move rather than a second placement.
    let current: Vec<&str> = meta
        .get("parents")
        .and_then(Value::as_array)
        .map(|ps| ps.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let params = [("addParents".to_string(), dest), ("removeParents".to_string(), current.join(","))];
    // Python sent no request body at all; an empty object is the same metadata-free patch.
    let f = api.drive_files_update(&r.id, &json!({}), &params, "id,name,parents").await?;
    Ok(json!({
        "id": field(&f, "id")?,
        "name": field(&f, "name")?,
        "parents": optional(&f, "parents"),
    })
    .into())
}

async fn rename_file(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let new_name = args.req_str("new_name")?;
    let confirm = args.bool_or("confirm", false)?;

    let r = parse_ref(&item)?;
    let meta = api.drive_files_get(&r.id, "id,name").await?;
    if !confirm {
        return Ok(
            preview_response("rename_file", json!({"from": field(&meta, "name")?, "to": new_name})).into()
        );
    }
    let f = api.drive_files_update(&r.id, &json!({"name": new_name}), &[], "id,name").await?;
    Ok(json!({"id": field(&f, "id")?, "name": field(&f, "name")?}).into())
}

async fn export_file(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let item = args.req_str("item")?;
    let to = args.req_str("to")?;
    let dest_path = nonempty(args.opt_str("dest_path")?);

    let r = parse_ref(&item)?;
    let format = to.to_lowercase();
    let Some(target) = lookup(EXPORT_MIME, &format) else {
        return Err(ToolError::msg(format!(
            "unsupported export format '{to}'; choose from {}",
            export_formats()
        )));
    };
    let data = api.drive_export(&r.id, target).await?;
    if dest_path.is_some() || data.len() > INLINE_MAX {
        let path = spill(dest_path.as_deref(), &format!("{}.{format}", r.id), &data)?;
        return Ok(json!({"format": to, "bytes": data.len(), "path": path.display().to_string()}).into());
    }
    Ok(json!({
        "format": to,
        "bytes": data.len(),
        "base64": base64::engine::general_purpose::STANDARD.encode(&data),
    })
    .into())
}

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "read_file_as_text",
            "Read a file's content as text, one bounded chunk at a time.\n\nGoogle-native files \
             are exported; PDFs are text-extracted. Returns the requested chunk\n(0-based) in \
             `content` plus name, mime_type, and paging metadata (total_chunks,\ntotal_chars, \
             has_more) — page through by incrementing chunk. Set max_chars<=0 for the\nwhole \
             document.",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "chunk": {"type": "integer", "default": 0},
                    "max_chars": {"type": "integer", "default": DEFAULT_MAX_CHARS},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "download_file",
            "Download a binary file. Small (<5MB, no dest_path) → base64 inline; otherwise \
             written to\nthe sandbox files dir (dest_path is resolved inside it; absolute/`..` \
             rejected).",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "dest_path": {"type": "string"},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "upload_file",
            "Create a new Drive file (from source_path or text content), or replace an existing \
             file's\ncontent (replace_id — requires confirm=true). `source_path` must be inside \
             the sandbox files dir.",
            json!({
                "properties": {
                    "name": {"type": "string"},
                    "source_path": {"type": "string"},
                    "content": {"type": "string"},
                    "parent": {"type": "string"},
                    "mime_type": {"type": "string"},
                    "replace_id": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                },
                "required": ["name"],
            }),
        ),
        ToolDef::new(
            "move_file",
            "Move a file (item) into a different folder (parent). Requires confirm=true.",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "parent": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                },
                "required": ["item", "parent"],
            }),
        ),
        ToolDef::new(
            "rename_file",
            "Rename a file. Requires confirm=true.",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "new_name": {"type": "string"},
                    "confirm": {"type": "boolean", "default": false},
                },
                "required": ["item", "new_name"],
            }),
        ),
        ToolDef::new(
            "export_file",
            "Export a Google-native file to a format: pdf, docx, xlsx, pptx, csv, txt, md, \
             html.\n\nWritten to the sandbox files dir when large or dest_path is given \
             (dest_path resolved inside\nit); small exports return base64 inline. (Drive's export \
             method has no supportsAllDrives param.)",
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "to": {"type": "string"},
                    "dest_path": {"type": "string"},
                },
                "required": ["item", "to"],
            }),
        ),
    ]
}

pub async fn dispatch(name: &str, api: &dyn GoogleApi, args: &Args) -> Option<Result<ToolOutput>> {
    Some(match name {
        "read_file_as_text" => read_file_as_text(api, args).await,
        "download_file" => download_file(api, args).await,
        "upload_file" => upload_file(api, args).await,
        "move_file" => move_file(api, args).await,
        "rename_file" => rename_file(api, args).await,
        "export_file" => export_file(api, args).await,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::FakeApi;
    use std::path::Path;

    /// Bare ids must be >= 20 chars to parse, like the pytest fixtures' `"A" * 30`.
    const AN_ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const A_FOLDER_ID: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    const A_REPLACE_ID: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";

    use crate::ENV_LOCK;

    struct Sandbox {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        previous: Option<String>,
    }

    impl Sandbox {
        /// Point `GDRIVE_MCP_FILES_DIR` at a fresh, nonexistent dir so `localfs` creates (and
        /// owns) it — the real server flow.
        fn new() -> Sandbox {
            let tmp = tempfile::tempdir().unwrap();
            // Canonicalised because `localfs` resolves symlinks, and macOS's /tmp is one.
            let root = tmp.path().canonicalize().unwrap().join("files");
            let previous = std::env::var("GDRIVE_MCP_FILES_DIR").ok();
            unsafe { std::env::set_var("GDRIVE_MCP_FILES_DIR", &root) };
            Sandbox { _tmp: tmp, root, previous }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var("GDRIVE_MCP_FILES_DIR", v) },
                None => unsafe { std::env::remove_var("GDRIVE_MCP_FILES_DIR") },
            }
        }
    }

    fn args(tool: &str, arguments: Value) -> Args {
        let def = defs().into_iter().find(|d| d.name == tool).unwrap();
        let map = arguments.as_object().cloned().unwrap();
        Args::new(tool, Some(map), &def.params()).unwrap()
    }

    async fn run(api: &FakeApi, tool: &str, arguments: Value) -> Result<Value> {
        match dispatch(tool, api, &args(tool, arguments)).await.unwrap()? {
            ToolOutput::Json(v) => Ok(v),
            ToolOutput::Images(_) => panic!("{tool} returned images"),
        }
    }

    async fn ok(api: &FakeApi, tool: &str, arguments: Value) -> Value {
        run(api, tool, arguments).await.unwrap()
    }

    async fn err(api: &FakeApi, tool: &str, arguments: Value) -> String {
        run(api, tool, arguments).await.unwrap_err().to_string()
    }

    /// A two-page PDF built by hand (with a correct xref) so the page count is a real one.
    fn two_page_pdf() -> Vec<u8> {
        let page = |num: u32, contents: u32| {
            format!(
                "{num} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents \
                 {contents} 0 R /Resources << /Font << /F1 7 0 R >> >> >>\nendobj\n"
            )
        };
        let stream = |num: u32, word: &str| {
            let body = format!("BT /F1 24 Tf 20 100 Td ({word}) Tj ET\n");
            format!("{num} 0 obj\n<< /Length {} >>\nstream\n{body}endstream\nendobj\n", body.len())
        };
        let objects = [
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_string(),
            "2 0 obj\n<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>\nendobj\n".to_string(),
            page(3, 4),
            stream(4, "Hello"),
            page(5, 6),
            stream(6, "World"),
            "7 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n".to_string(),
        ];
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for object in &objects {
            offsets.push(out.len());
            out.push_str(object);
        }
        let startxref = out.len();
        out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
        for offset in offsets {
            out.push_str(&format!("{offset:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n",
            objects.len() + 1
        ));
        out.into_bytes()
    }

    fn meta(mime: &str, name: &str) -> Value {
        json!({"id": "F", "name": name, "mimeType": mime, "size": "1"})
    }

    fn moving(confirm: bool) -> Value {
        json!({"item": AN_ID, "parent": A_FOLDER_ID, "confirm": confirm})
    }

    fn renaming(confirm: bool) -> Value {
        json!({"item": AN_ID, "new_name": "new", "confirm": confirm})
    }

    #[test]
    fn the_tools_are_declared_in_the_python_module_order() {
        let names: Vec<&str> = defs().iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            ["read_file_as_text", "download_file", "upload_file", "move_file", "rename_file", "export_file"]
        );
    }

    #[tokio::test]
    async fn an_unrelated_tool_name_is_left_for_another_module() {
        let api = FakeApi::new();
        let borrowed = args("rename_file", json!({"item": AN_ID, "new_name": "x"}));
        assert!(dispatch("read_sheet", &api, &borrowed).await.is_none());
    }

    // ---- confirm-before-destructive gate ------------------------------------------------

    #[tokio::test]
    async fn move_file_previews_the_move_and_only_reparents_after_confirm() {
        let api = FakeApi::new();
        api.on("drive_files_get", json!({"id": "F", "name": "n", "parents": ["OLD"]}));
        let out = ok(&api, "move_file", moving(false)).await;
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"], json!({"file": "n", "from": ["OLD"], "to": A_FOLDER_ID}));
        assert_eq!(api.call_count("drive_files_update"), 0);

        api.on("drive_files_get", json!({"id": "F", "name": "n", "parents": ["OLD"]}));
        api.on("drive_files_update", json!({"id": "F", "name": "n", "parents": ["NEW"]}));
        let out = ok(&api, "move_file", moving(true)).await;
        let call = api.last("drive_files_update");
        assert_eq!(call["params"]["addParents"], A_FOLDER_ID);
        assert_eq!(call["params"]["removeParents"], "OLD");
        assert_eq!(out["parents"], json!(["NEW"]));
    }

    #[tokio::test]
    async fn every_current_parent_is_removed_so_a_move_is_not_a_second_placement() {
        let api = FakeApi::new();
        api.on("drive_files_get", json!({"id": "F", "name": "n", "parents": ["ONE", "TWO"]}));
        api.on("drive_files_update", json!({"id": "F", "name": "n", "parents": ["NEW"]}));
        ok(&api, "move_file", moving(true)).await;
        assert_eq!(api.last("drive_files_update")["params"]["removeParents"], "ONE,TWO");
    }

    #[tokio::test]
    async fn a_file_with_no_parents_still_moves() {
        let api = FakeApi::new();
        api.on("drive_files_get", json!({"id": "F", "name": "n"})); // e.g. a shared-drive root item
        let out = ok(&api, "move_file", moving(false)).await;
        assert_eq!(out["impact"]["from"], Value::Null);
        api.on("drive_files_get", json!({"id": "F", "name": "n"}));
        api.on("drive_files_update", json!({"id": "F", "name": "n", "parents": ["NEW"]}));
        ok(&api, "move_file", moving(true)).await;
        assert_eq!(api.last("drive_files_update")["params"]["removeParents"], "");
    }

    #[tokio::test]
    async fn rename_file_previews_the_new_name_and_only_writes_after_confirm() {
        let api = FakeApi::new();
        api.on("drive_files_get", json!({"id": "F", "name": "old"}));
        let out = ok(&api, "rename_file", renaming(false)).await;
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["impact"], json!({"from": "old", "to": "new"}));
        assert_eq!(api.call_count("drive_files_update"), 0);

        api.on("drive_files_get", json!({"id": "F", "name": "old"}));
        api.on("drive_files_update", json!({"id": "F", "name": "new"}));
        let out = ok(&api, "rename_file", renaming(true)).await;
        assert_eq!(api.last("drive_files_update")["body"], json!({"name": "new"}));
        assert_eq!(out, json!({"id": "F", "name": "new"}));
    }

    #[tokio::test]
    async fn upload_file_previews_a_replacement_and_never_overwrites_without_confirm() {
        let api = FakeApi::new();
        api.on(
            "drive_files_get",
            json!({"id": "R", "name": "old", "mimeType": "text/plain", "modifiedTime": "t"}),
        );
        let out = ok(
            &api,
            "upload_file",
            json!({"name": "newname", "content": "hi", "replace_id": A_REPLACE_ID, "confirm": false}),
        )
        .await;
        assert_eq!(out["status"], "confirmation_required");
        assert_eq!(out["action"], "upload_file(replace)");
        assert_eq!(out["impact"]["replacing"]["name"], "old");
        assert_eq!(out["impact"]["with_name"], "newname");
        assert_eq!(api.call_count("drive_files_update_media"), 0);
    }

    #[tokio::test]
    async fn a_confirmed_replacement_uploads_over_the_existing_file() {
        let api = FakeApi::new();
        api.on("drive_files_update_media", json!({"id": "R", "name": "newname", "webViewLink": "u"}));
        let out = ok(
            &api,
            "upload_file",
            json!({"name": "newname", "content": "hi", "replace_id": A_REPLACE_ID, "confirm": true}),
        )
        .await;
        assert_eq!(out, json!({"id": "R", "name": "newname", "url": "u", "replaced": true}));
        let call = api.last("drive_files_update_media");
        assert_eq!(call["file_id"], A_REPLACE_ID);
        assert_eq!(call["metadata"], json!({"name": "newname"}));
        assert_eq!(call["mime_type"], "text/plain");
        assert_eq!(call["bytes"], 2);
        // A confirmed replace goes straight to the upload — no metadata read first.
        assert_eq!(api.call_count("drive_files_get"), 0);
    }

    // ---- read_file_as_text ---------------------------------------------------------------

    #[tokio::test]
    async fn a_google_doc_is_exported_as_plain_text_and_paginated() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/vnd.google-apps.document", "Notes"));
        api.on_bytes("drive_export", b"para one\n\npara two".to_vec());
        let out = ok(&api, "read_file_as_text", json!({"item": AN_ID})).await;
        assert_eq!(api.last("drive_export")["mime_type"], "text/plain");
        assert_eq!(out["name"], "Notes");
        assert_eq!(out["mime_type"], "application/vnd.google-apps.document");
        assert_eq!(out["content"], "para one\n\npara two");
        assert_eq!(out["total_chunks"], 1);
        assert_eq!(out["has_more"], false);
        assert_eq!(api.call_count("drive_get_media"), 0); // native files are never downloaded
    }

    #[tokio::test]
    async fn a_google_sheet_is_exported_as_csv() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/vnd.google-apps.spreadsheet", "Grid"));
        api.on_bytes("drive_export", b"a,b\n1,2".to_vec());
        ok(&api, "read_file_as_text", json!({"item": AN_ID})).await;
        assert_eq!(api.last("drive_export")["mime_type"], "text/csv");
    }

    #[tokio::test]
    async fn requesting_a_later_chunk_serves_that_chunk_and_reports_more() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("text/plain", "long.txt"));
        api.on_bytes("drive_get_media", b"aaaa\n\nbbbb\n\ncccc".to_vec());
        let out = ok(&api, "read_file_as_text", json!({"item": AN_ID, "chunk": 1, "max_chars": 5})).await;
        assert_eq!(out["content"], "bbbb");
        assert_eq!(out["chunk_index"], 1);
        assert_eq!(out["total_chunks"], 3);
        assert_eq!(out["total_chars"], 16);
        assert_eq!(out["has_more"], true);
    }

    #[tokio::test]
    async fn max_chars_of_zero_returns_the_whole_document_in_one_chunk() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/json", "data.json"));
        api.on_bytes("drive_get_media", br#"{"a": 1}"#.to_vec());
        let out = ok(&api, "read_file_as_text", json!({"item": AN_ID, "max_chars": 0})).await;
        assert_eq!(out["content"], r#"{"a": 1}"#);
        assert_eq!(out["total_chunks"], 1);
    }

    #[tokio::test]
    async fn a_pdf_is_text_extracted_and_reports_its_page_count() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/pdf", "report.pdf"));
        api.on_bytes("drive_get_media", two_page_pdf());
        let out = ok(&api, "read_file_as_text", json!({"item": AN_ID})).await;
        assert_eq!(out["pages"], 2);
        let content = out["content"].as_str().unwrap();
        assert!(content.contains("Hello") && content.contains("World"), "{content}");
    }

    #[tokio::test]
    async fn an_unreadable_pdf_reports_an_error_instead_of_escaping() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/pdf", "broken.pdf"));
        api.on_bytes("drive_get_media", b"not a pdf at all".to_vec());
        let message = err(&api, "read_file_as_text", json!({"item": AN_ID})).await;
        assert!(message.starts_with("could not extract text from PDF: "), "{message}");
    }

    #[tokio::test]
    async fn undecodable_bytes_are_replaced_rather_than_failing_the_read() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("text/plain", "mixed.txt"));
        api.on_bytes("drive_get_media", vec![b'h', b'i', 0xff]);
        let out = ok(&api, "read_file_as_text", json!({"item": AN_ID})).await;
        assert_eq!(out["content"], "hi\u{fffd}");
    }

    #[tokio::test]
    async fn a_binary_mime_type_points_at_download_file() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("image/png", "shot.png"));
        let message = err(&api, "read_file_as_text", json!({"item": AN_ID})).await;
        assert_eq!(message, "image/png is not text-extractable; use download_file instead.");
    }

    // ---- download_file -------------------------------------------------------------------

    #[tokio::test]
    async fn a_small_download_comes_back_inline_as_base64() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("image/png", "shot.png"));
        api.on_bytes("drive_get_media", b"\x89PNG".to_vec());
        let out = ok(&api, "download_file", json!({"item": AN_ID})).await;
        assert_eq!(
            out,
            json!({"name": "shot.png", "mime_type": "image/png", "bytes": 4, "base64": "iVBORw=="})
        );
    }

    #[tokio::test]
    async fn a_google_native_file_cannot_be_downloaded() {
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/vnd.google-apps.document", "Notes"));
        let message = err(&api, "download_file", json!({"item": AN_ID})).await;
        assert_eq!(message, "Google-native file: use export_file or read_file_as_text, not download_file.");
        assert_eq!(api.call_count("drive_get_media"), 0);
    }

    #[tokio::test]
    async fn a_download_over_the_inline_limit_spills_to_the_sandbox_with_a_note() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sandbox = Sandbox::new();
        let big = vec![b'x'; INLINE_MAX + 1];
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/zip", "big.zip"));
        api.on_bytes("drive_get_media", big.clone());
        let out = ok(&api, "download_file", json!({"item": AN_ID})).await;
        assert_eq!(out["path"], sandbox.path("big.zip").display().to_string());
        assert_eq!(out["bytes"], big.len());
        assert_eq!(out["note"], "5242881 bytes exceeded 5242880 inline limit; written to file");
        assert!(out.get("base64").is_none());
        assert_eq!(std::fs::metadata(sandbox.path("big.zip")).unwrap().len() as usize, big.len());
    }

    #[tokio::test]
    async fn an_explicit_dest_path_spills_even_a_tiny_file_and_needs_no_note() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sandbox = Sandbox::new();
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/zip", "small.zip"));
        api.on_bytes("drive_get_media", b"pk".to_vec());
        let out = ok(&api, "download_file", json!({"item": AN_ID, "dest_path": "sub/out.zip"})).await;
        assert_eq!(out["path"], sandbox.path("sub/out.zip").display().to_string());
        assert_eq!(out["note"], Value::Null);
        assert_eq!(std::fs::read(sandbox.path("sub/out.zip")).unwrap(), b"pk");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_spilled_download_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sandbox = Sandbox::new();
        let api = FakeApi::new();
        api.on("drive_files_get", meta("application/zip", "sensitive.zip"));
        api.on_bytes("drive_get_media", b"pk".to_vec());
        ok(&api, "download_file", json!({"item": AN_ID, "dest_path": "sensitive.zip"})).await;
        let mode = std::fs::metadata(sandbox.path("sensitive.zip")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn a_download_dest_path_escaping_the_sandbox_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _sandbox = Sandbox::new();
        for escape in ["/etc/cron.d/pwn", "../../../etc/pwn", "sub/../../pwn"] {
            let api = FakeApi::new();
            api.on("drive_files_get", meta("application/zip", "x.zip"));
            api.on_bytes("drive_get_media", b"pk".to_vec());
            let message = err(&api, "download_file", json!({"item": AN_ID, "dest_path": escape})).await;
            assert!(message.contains("must stay within the files dir"), "{escape}: {message}");
        }
    }

    // ---- upload_file ---------------------------------------------------------------------

    #[tokio::test]
    async fn an_upload_with_neither_a_source_nor_content_is_refused() {
        let api = FakeApi::new();
        let message = err(&api, "upload_file", json!({"name": "x.txt"})).await;
        assert_eq!(message, "provide source_path or content");
        assert!(api.calls().is_empty());
    }

    #[tokio::test]
    async fn text_content_defaults_to_text_plain_and_empty_content_still_uploads() {
        let api = FakeApi::new();
        api.on("drive_files_create_media", json!({"id": "N", "name": "x.txt"}));
        let out = ok(&api, "upload_file", json!({"name": "x.txt", "content": ""})).await;
        assert_eq!(out, json!({"id": "N", "name": "x.txt", "url": null}));
        let call = api.last("drive_files_create_media");
        assert_eq!(call["mime_type"], "text/plain");
        assert_eq!(call["bytes"], 0);
        assert_eq!(call["metadata"], json!({"name": "x.txt"}));
        assert_eq!(call["fields"], "id,name,webViewLink");
    }

    #[tokio::test]
    async fn an_explicit_mime_type_wins_over_the_text_default() {
        let api = FakeApi::new();
        api.on("drive_files_create_media", json!({"id": "N", "name": "x.csv"}));
        ok(&api, "upload_file", json!({"name": "x.csv", "content": "a,b", "mime_type": "text/csv"})).await;
        assert_eq!(api.last("drive_files_create_media")["mime_type"], "text/csv");
    }

    #[tokio::test]
    async fn a_parent_folder_becomes_the_new_files_parent() {
        let api = FakeApi::new();
        api.on("drive_files_create_media", json!({"id": "N", "name": "x.txt"}));
        ok(
            &api,
            "upload_file",
            json!({"name": "x.txt", "content": "hi", "parent": format!("https://drive.google.com/drive/folders/{A_FOLDER_ID}")}),
        )
        .await;
        assert_eq!(
            api.last("drive_files_create_media")["metadata"],
            json!({"name": "x.txt", "parents": [A_FOLDER_ID]})
        );
    }

    #[tokio::test]
    async fn a_file_upload_reads_the_bytes_from_the_sandbox() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sandbox = Sandbox::new();
        std::fs::create_dir_all(&sandbox.root).unwrap();
        std::fs::write(sandbox.path("payload.bin"), b"12345").unwrap();
        let api = FakeApi::new();
        api.on("drive_files_create_media", json!({"id": "N", "name": "payload.bin"}));
        ok(&api, "upload_file", json!({"name": "payload.bin", "source_path": "payload.bin"})).await;
        let call = api.last("drive_files_create_media");
        assert_eq!(call["bytes"], 5);
        // An extension nothing recognises falls back the way MediaFileUpload did.
        assert_eq!(call["mime_type"], "application/octet-stream");
    }

    #[tokio::test]
    async fn an_upload_guesses_its_mime_type_from_the_filename() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sandbox = Sandbox::new();
        std::fs::create_dir_all(&sandbox.root).unwrap();
        std::fs::write(sandbox.path("report.pdf"), b"%PDF-1.4").unwrap();
        let api = FakeApi::new();
        api.on("drive_files_create_media", json!({"id": "N", "name": "report.pdf"}));
        ok(&api, "upload_file", json!({"name": "report.pdf", "source_path": "report.pdf"})).await;
        assert_eq!(api.last("drive_files_create_media")["mime_type"], "application/pdf");

        // An explicit mime_type still wins over the guess.
        let api = FakeApi::new();
        api.on("drive_files_create_media", json!({"id": "N", "name": "report.pdf"}));
        ok(
            &api,
            "upload_file",
            json!({"name": "report.pdf", "source_path": "report.pdf", "mime_type": "text/plain"}),
        )
        .await;
        assert_eq!(api.last("drive_files_create_media")["mime_type"], "text/plain");
    }

    #[tokio::test]
    async fn an_upload_source_path_escaping_the_sandbox_is_rejected_before_any_drive_call() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _sandbox = Sandbox::new();
        for escape in ["/etc/passwd", "../../.ssh/id_rsa"] {
            let api = FakeApi::new();
            let message = err(
                &api,
                "upload_file",
                json!({"name": "loot", "source_path": escape, "replace_id": A_REPLACE_ID, "confirm": true}),
            )
            .await;
            assert!(message.contains("must be inside the files dir"), "{escape}: {message}");
            assert!(api.calls().is_empty(), "{escape} reached the Drive API");
        }
    }

    #[tokio::test]
    async fn a_source_path_inside_the_sandbox_that_does_not_exist_says_so() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _sandbox = Sandbox::new();
        let api = FakeApi::new();
        let message = err(&api, "upload_file", json!({"name": "x", "source_path": "nope.bin"})).await;
        assert!(message.starts_with("source_path not found in files dir: "), "{message}");
    }

    // ---- export_file ---------------------------------------------------------------------

    #[tokio::test]
    async fn a_small_export_comes_back_inline_as_base64() {
        let api = FakeApi::new();
        api.on_bytes("drive_export", b"%PDF".to_vec());
        let out = ok(&api, "export_file", json!({"item": AN_ID, "to": "pdf"})).await;
        assert_eq!(out, json!({"format": "pdf", "bytes": 4, "base64": "JVBERg=="}));
        assert_eq!(api.last("drive_export")["mime_type"], "application/pdf");
    }

    #[tokio::test]
    async fn the_export_format_is_matched_case_insensitively_and_echoed_as_given() {
        let api = FakeApi::new();
        api.on_bytes("drive_export", b"<html>".to_vec());
        let out = ok(&api, "export_file", json!({"item": AN_ID, "to": "HTML"})).await;
        assert_eq!(api.last("drive_export")["mime_type"], "text/html");
        assert_eq!(out["format"], "HTML");
    }

    #[tokio::test]
    async fn an_unsupported_export_format_lists_the_supported_ones_alphabetically() {
        let api = FakeApi::new();
        let message = err(&api, "export_file", json!({"item": AN_ID, "to": "epub"})).await;
        assert_eq!(
            message,
            "unsupported export format 'epub'; choose from ['csv', 'docx', 'html', 'md', 'pdf', \
             'pptx', 'txt', 'xlsx']"
        );
        assert!(api.calls().is_empty());
    }

    #[tokio::test]
    async fn an_export_with_a_dest_path_is_written_there_instead_of_inlined() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sandbox = Sandbox::new();
        let api = FakeApi::new();
        api.on_bytes("drive_export", b"a,b\n".to_vec());
        let out = ok(&api, "export_file", json!({"item": AN_ID, "to": "csv", "dest_path": "grid.csv"})).await;
        assert_eq!(
            out,
            json!({"format": "csv", "bytes": 4, "path": sandbox.path("grid.csv").display().to_string()})
        );
        assert_eq!(std::fs::read(sandbox.path("grid.csv")).unwrap(), b"a,b\n");
    }

    #[tokio::test]
    async fn a_large_export_without_a_dest_path_spills_under_the_file_id() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let sandbox = Sandbox::new();
        let api = FakeApi::new();
        api.on_bytes("drive_export", vec![b'x'; INLINE_MAX + 1]);
        let out = ok(&api, "export_file", json!({"item": AN_ID, "to": "PDF"})).await;
        // The default name is built from the lowercased format, not the caller's spelling.
        assert_eq!(out["path"], sandbox.path(&format!("{AN_ID}.pdf")).display().to_string());
        assert!(Path::new(out["path"].as_str().unwrap()).is_file());
    }

    #[tokio::test]
    async fn an_export_dest_path_escaping_the_sandbox_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _sandbox = Sandbox::new();
        let api = FakeApi::new();
        api.on_bytes("drive_export", b"a,b\n".to_vec());
        let message =
            err(&api, "export_file", json!({"item": AN_ID, "to": "csv", "dest_path": "../../loot.csv"}))
                .await;
        assert!(message.contains("must stay within the files dir"), "{message}");
    }
}
