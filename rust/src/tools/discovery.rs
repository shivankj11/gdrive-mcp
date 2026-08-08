//! Discovery tools: resolve links, search, browse folders, read metadata.

use serde_json::{json, Value};

use super::{ToolDef, ToolOutput};
use crate::args::Args;
use crate::clients::GoogleApi;
use crate::error::Result;
use crate::ids::parse_ref;

const FILE_FIELDS: &str = "id, name, mimeType, modifiedTime, size, \
                           owners(displayName,emailAddress), webViewLink, parents";

fn kind(mime: &str) -> &'static str {
    match mime {
        "application/vnd.google-apps.spreadsheet" => "spreadsheet",
        "application/vnd.google-apps.document" => "document",
        "application/vnd.google-apps.folder" => "folder",
        _ => "file",
    }
}

/// Escape a value for a single-quoted Drive `q` literal. The backslash pass must run FIRST:
/// escaping the quote first would leave the backslash it inserts to be doubled by the
/// backslash pass, turning `\'` back into a literal backslash plus an unescaped quote.
fn esc(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn slim(f: &Value) -> Value {
    let field = |k: &str| f.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "id": field("id"),
        "name": field("name"),
        "mime_type": field("mimeType"),
        "kind": kind(f.get("mimeType").and_then(Value::as_str).unwrap_or("")),
        "modified": field("modifiedTime"),
        "size": field("size"),
        "web_view_link": field("webViewLink"),
    })
}

/// The `files(...)` mask both listing tools ask for.
fn list_fields() -> String {
    format!("nextPageToken, incompleteSearch, files({FILE_FIELDS})")
}

/// One page of `files.list`, reshaped the way both listing tools report it.
fn page(resp: &Value) -> (Vec<Value>, Value, bool, Value) {
    let files: Vec<Value> = resp
        .get("files")
        .and_then(Value::as_array)
        .map(|fs| fs.iter().map(slim).collect())
        .unwrap_or_default();
    let token = resp.get("nextPageToken").cloned().unwrap_or(Value::Null);
    // Python's `bool(token)`: a missing token and an empty-string token both mean "last page".
    let has_more = token.as_str().is_some_and(|t| !t.is_empty());
    let incomplete = resp.get("incompleteSearch").cloned().unwrap_or(Value::Bool(false));
    (files, token, has_more, incomplete)
}

const RESOLVE_LINK_DOC: &str =
    "Resolve a Google Drive/Docs/Sheets URL or bare ID (item) to its id, kind, name and link.";

const SEARCH_FILES_DOC: &str = "Search Drive by name, full-text content, mime type, and/or parent folder.\n\
    \n\
    Returns one page; when `has_more` is true, pass the returned `next_page_token` to continue.\n\
    `incomplete_search` true means Drive couldn't search every corpus (results may be partial).";

const LIST_FOLDER_DOC: &str = "List the direct children of a folder (item = folder URL or ID).\n\
    \n\
    Returns one page; when `has_more` is true, pass the returned `next_page_token` to continue.";

const GET_METADATA_DOC: &str = "Full metadata for a file: owner, timestamps, size, parents, sharing link.";

pub fn defs() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "resolve_link",
            RESOLVE_LINK_DOC,
            json!({
                "properties": {"item": {"type": "string"}},
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "search_files",
            SEARCH_FILES_DOC,
            json!({
                "properties": {
                    "name_contains": {"type": "string"},
                    "full_text": {"type": "string"},
                    "mime_type": {"type": "string"},
                    "in_folder": {"type": "string"},
                    "page_size": {"type": "integer", "default": 25},
                    "page_token": {"type": "string"},
                },
            }),
        ),
        ToolDef::new(
            "list_folder",
            LIST_FOLDER_DOC,
            json!({
                "properties": {
                    "item": {"type": "string"},
                    "page_size": {"type": "integer", "default": 100},
                    "page_token": {"type": "string"},
                },
                "required": ["item"],
            }),
        ),
        ToolDef::new(
            "get_metadata",
            GET_METADATA_DOC,
            json!({
                "properties": {"item": {"type": "string"}},
                "required": ["item"],
            }),
        ),
    ]
}

async fn resolve_link(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let r = parse_ref(&args.req_str("item")?)?;
    let f = api.drive_files_get(&r.id, FILE_FIELDS).await?;
    Ok(slim(&f).into())
}

async fn search_files(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let name_contains = args.opt_str("name_contains")?;
    let full_text = args.opt_str("full_text")?;
    let mime_type = args.opt_str("mime_type")?;
    let in_folder = args.opt_str("in_folder")?;
    let page_size = args.i64_or("page_size", 25)?;
    let page_token = args.opt_str("page_token")?;

    let mut clauses = vec!["trashed = false".to_string()];
    // Python tested each filter for truthiness, so an empty string adds no clause (an
    // unconstrained `name contains ''` would otherwise match the whole drive).
    if let Some(v) = name_contains.filter(|s| !s.is_empty()) {
        clauses.push(format!("name contains '{}'", esc(&v)));
    }
    if let Some(v) = full_text.filter(|s| !s.is_empty()) {
        clauses.push(format!("fullText contains '{}'", esc(&v)));
    }
    if let Some(v) = mime_type.filter(|s| !s.is_empty()) {
        clauses.push(format!("mimeType = '{}'", esc(&v)));
    }
    if let Some(v) = in_folder.filter(|s| !s.is_empty()) {
        // Not escaped, and safely so: a parsed id is `[A-Za-z0-9_-]+` by construction.
        clauses.push(format!("'{}' in parents", parse_ref(&v)?.id));
    }
    let q = clauses.join(" and ");

    let resp = api
        .drive_files_list(
            &q,
            page_size.clamp(1, 100),
            page_token.as_deref(),
            &list_fields(),
            "modifiedTime desc",
        )
        .await?;
    let (files, token, has_more, incomplete) = page(&resp);
    Ok(json!({
        "query": q,
        "count": files.len(),
        "files": files,
        "has_more": has_more,
        "next_page_token": token,
        "incomplete_search": incomplete,
    })
    .into())
}

async fn list_folder(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let fid = parse_ref(&args.req_str("item")?)?.id;
    let page_size = args.i64_or("page_size", 100)?;
    let page_token = args.opt_str("page_token")?;

    let resp = api
        .drive_files_list(
            &format!("'{fid}' in parents and trashed = false"),
            page_size.clamp(1, 1000),
            page_token.as_deref(),
            &list_fields(),
            // Folders first, then by name — the ordering a person browsing expects.
            "folder,name",
        )
        .await?;
    let (files, token, has_more, incomplete) = page(&resp);
    Ok(json!({
        "folder_id": fid,
        "count": files.len(),
        "files": files,
        "has_more": has_more,
        "next_page_token": token,
        "incomplete_search": incomplete,
    })
    .into())
}

async fn get_metadata(api: &dyn GoogleApi, args: &Args) -> Result<ToolOutput> {
    let r = parse_ref(&args.req_str("item")?)?;
    // Returned verbatim, unlike the other tools' slimmed shape: this is the "tell me everything"
    // escape hatch, so the raw Drive keys are the point.
    let fields = format!("{FILE_FIELDS}, createdTime, description, shared, trashed");
    Ok(api.drive_files_get(&r.id, &fields).await?.into())
}

pub async fn dispatch(name: &str, api: &dyn GoogleApi, args: &Args) -> Option<Result<ToolOutput>> {
    Some(match name {
        "resolve_link" => resolve_link(api, args).await,
        "search_files" => search_files(api, args).await,
        "list_folder" => list_folder(api, args).await,
        "get_metadata" => get_metadata(api, args).await,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::FakeApi;

    const A_FOLDER_ID: &str = "FOLDERID_00000000000000000";
    const A_FILE_ID: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn args_for(tool: &str, v: Value) -> Args {
        let def = defs().into_iter().find(|d| d.name == tool).unwrap();
        let Value::Object(map) = v else { panic!("tool arguments must be an object") };
        Args::new(tool, Some(map), &def.params()).unwrap()
    }

    async fn run(api: &FakeApi, tool: &str, v: Value) -> Result<Value> {
        let args = args_for(tool, v);
        match dispatch(tool, api, &args).await {
            Some(Ok(ToolOutput::Json(out))) => Ok(out),
            Some(Ok(_)) => panic!("{tool} returned images"),
            Some(Err(e)) => Err(e),
            None => panic!("{tool} is not a discovery tool"),
        }
    }

    #[tokio::test]
    async fn resolve_link_slims_the_drive_response_to_the_agent_facing_shape() {
        let api = FakeApi::new();
        api.on(
            "drive_files_get",
            json!({
                "id": "F1",
                "name": "Q3 Plan",
                "mimeType": "application/vnd.google-apps.document",
                "modifiedTime": "2026-01-02T03:04:05Z",
                "size": "1234",
                "webViewLink": "https://docs.google.com/document/d/F1/edit",
                "owners": [{"displayName": "A", "emailAddress": "a@example.com"}],
                "parents": ["P1"],
            }),
        );

        let out = run(&api, "resolve_link", json!({"item": A_FILE_ID})).await.unwrap();

        assert_eq!(out["id"], "F1");
        assert_eq!(out["name"], "Q3 Plan");
        assert_eq!(out["mime_type"], "application/vnd.google-apps.document");
        assert_eq!(out["kind"], "document");
        assert_eq!(out["modified"], "2026-01-02T03:04:05Z");
        assert_eq!(out["size"], "1234");
        assert_eq!(out["web_view_link"], "https://docs.google.com/document/d/F1/edit");
        // The verbose fields Drive returned are dropped on the floor.
        assert!(out.get("owners").is_none() && out.get("parents").is_none());
    }

    #[tokio::test]
    async fn resolve_link_sends_the_id_parsed_out_of_a_url_and_the_file_field_mask() {
        let api = FakeApi::new();
        run(&api, "resolve_link", json!({"item": "https://docs.google.com/document/d/DOC_9/edit"}))
            .await
            .unwrap();

        let call = api.last("drive_files_get");
        assert_eq!(call["file_id"], "DOC_9");
        assert_eq!(call["fields"], FILE_FIELDS);
        assert!(
            FILE_FIELDS.contains("owners(displayName,emailAddress)"),
            "the field mask must survive the line continuation: {FILE_FIELDS}"
        );
    }

    #[tokio::test]
    async fn a_mime_type_outside_the_table_is_reported_as_a_plain_file() {
        for (mime, want) in [
            ("application/vnd.google-apps.spreadsheet", "spreadsheet"),
            ("application/vnd.google-apps.document", "document"),
            ("application/vnd.google-apps.folder", "folder"),
            ("application/pdf", "file"),
            ("application/vnd.google-apps.presentation", "file"),
        ] {
            let api = FakeApi::new();
            api.on("drive_files_get", json!({"id": "X", "mimeType": mime}));
            let out = run(&api, "resolve_link", json!({"item": A_FILE_ID})).await.unwrap();
            assert_eq!(out["kind"], want, "{mime}");
        }
    }

    #[tokio::test]
    async fn fields_drive_omitted_come_back_as_nulls_rather_than_missing_keys() {
        let api = FakeApi::new();
        api.on("drive_files_get", json!({}));
        let out = run(&api, "resolve_link", json!({"item": A_FILE_ID})).await.unwrap();
        for key in ["id", "name", "mime_type", "modified", "size", "web_view_link"] {
            assert_eq!(out[key], Value::Null, "{key}");
        }
        // No mimeType at all still classifies, rather than blowing up.
        assert_eq!(out["kind"], "file");
    }

    #[tokio::test]
    async fn search_files_escapes_a_single_quote_in_a_name_clause() {
        let api = FakeApi::new();
        api.on("drive_files_list", json!({"files": []}));

        run(&api, "search_files", json!({"name_contains": "O'Brien", "mime_type": "application/pdf"}))
            .await
            .unwrap();

        let q = api.last("drive_files_list")["q"].as_str().unwrap().to_string();
        assert!(q.contains("trashed = false"), "{q}");
        assert!(q.contains(r"name contains 'O\'Brien'"), "{q}");
        assert!(q.contains("mimeType = 'application/pdf'"), "{q}");
    }

    #[test]
    fn escaping_doubles_the_backslash_before_it_escapes_the_quote() {
        // Input `a\'b`. Quote-first would produce `a\\\\'b`, closing the literal early.
        assert_eq!(esc("a\\'b"), "a\\\\\\'b");
        assert_eq!(esc("C:\\temp"), "C:\\\\temp");
        assert_eq!(esc("plain"), "plain");
    }

    #[tokio::test]
    async fn search_files_joins_every_requested_filter_in_signature_order() {
        let api = FakeApi::new();
        let out = run(
            &api,
            "search_files",
            json!({
                "name_contains": "budget",
                "full_text": "revenue",
                "mime_type": "text/csv",
                "in_folder": format!("https://drive.google.com/drive/folders/{A_FOLDER_ID}"),
            }),
        )
        .await
        .unwrap();

        let expected = format!(
            "trashed = false and name contains 'budget' and fullText contains 'revenue' \
             and mimeType = 'text/csv' and '{A_FOLDER_ID}' in parents"
        );
        assert_eq!(api.last("drive_files_list")["q"], expected);
        // The query is echoed back so the agent can see what was actually asked.
        assert_eq!(out["query"], expected);
    }

    #[tokio::test]
    async fn search_files_with_no_filters_only_excludes_the_trash() {
        let api = FakeApi::new();
        run(&api, "search_files", json!({})).await.unwrap();
        assert_eq!(api.last("drive_files_list")["q"], "trashed = false");
    }

    #[tokio::test]
    async fn an_empty_filter_string_adds_no_clause() {
        let api = FakeApi::new();
        run(&api, "search_files", json!({"name_contains": "", "full_text": ""})).await.unwrap();
        assert_eq!(api.last("drive_files_list")["q"], "trashed = false");
    }

    #[tokio::test]
    async fn search_files_surfaces_pagination_signals() {
        let api = FakeApi::new();
        api.on("drive_files_list", json!({"files": [], "nextPageToken": "TOK", "incompleteSearch": true}));

        let out =
            run(&api, "search_files", json!({"name_contains": "x", "page_token": "prev"})).await.unwrap();

        assert_eq!(out["has_more"], true);
        assert_eq!(out["next_page_token"], "TOK");
        assert_eq!(out["incomplete_search"], true);
        let call = api.last("drive_files_list");
        assert_eq!(call["page_token"], "prev");
        assert!(call["fields"].as_str().unwrap().contains("nextPageToken"), "{call}");
    }

    #[tokio::test]
    async fn list_folder_reports_no_more_pages_when_drive_returns_no_token() {
        let api = FakeApi::new();
        api.on("drive_files_list", json!({"files": []}));

        let out = run(&api, "list_folder", json!({"item": A_FOLDER_ID})).await.unwrap();

        assert_eq!(out["has_more"], false);
        assert_eq!(out["next_page_token"], Value::Null);
        // Absent `incompleteSearch` reads as "the search was complete".
        assert_eq!(out["incomplete_search"], false);
        assert_eq!(out["count"], 0);
        assert_eq!(out["folder_id"], A_FOLDER_ID);
    }

    #[tokio::test]
    async fn an_empty_next_page_token_is_not_another_page() {
        let api = FakeApi::new();
        api.on("drive_files_list", json!({"files": [], "nextPageToken": ""}));
        let out = run(&api, "search_files", json!({})).await.unwrap();
        assert_eq!(out["has_more"], false);
    }

    #[tokio::test]
    async fn each_listed_file_is_slimmed_and_counted() {
        let api = FakeApi::new();
        api.on(
            "drive_files_list",
            json!({"files": [
                {"id": "1", "name": "a", "mimeType": "application/vnd.google-apps.folder"},
                {"id": "2", "name": "b", "mimeType": "text/plain"},
            ]}),
        );

        let out = run(&api, "list_folder", json!({"item": A_FOLDER_ID})).await.unwrap();

        assert_eq!(out["count"], 2);
        assert_eq!(out["files"][0]["kind"], "folder");
        assert_eq!(out["files"][1]["kind"], "file");
        assert_eq!(out["files"][1]["name"], "b");
    }

    #[tokio::test]
    async fn search_files_clamps_page_size_into_drives_accepted_range() {
        for (asked, sent) in [(0, 1), (-7, 1), (25, 25), (100, 100), (500, 100)] {
            let api = FakeApi::new();
            run(&api, "search_files", json!({"page_size": asked})).await.unwrap();
            assert_eq!(api.last("drive_files_list")["page_size"], sent, "asked {asked}");
        }
    }

    #[tokio::test]
    async fn list_folder_allows_ten_times_the_page_size_search_does() {
        for (asked, sent) in [(0, 1), (100, 100), (1000, 1000), (5000, 1000)] {
            let api = FakeApi::new();
            run(&api, "list_folder", json!({"item": A_FOLDER_ID, "page_size": asked})).await.unwrap();
            assert_eq!(api.last("drive_files_list")["page_size"], sent, "asked {asked}");
        }
    }

    #[tokio::test]
    async fn the_default_page_sizes_are_the_pythons() {
        let api = FakeApi::new();
        run(&api, "search_files", json!({})).await.unwrap();
        assert_eq!(api.last("drive_files_list")["page_size"], 25);

        let api = FakeApi::new();
        run(&api, "list_folder", json!({"item": A_FOLDER_ID})).await.unwrap();
        assert_eq!(api.last("drive_files_list")["page_size"], 100);
    }

    #[tokio::test]
    async fn in_folder_is_resolved_to_an_id_before_it_becomes_a_parents_clause() {
        let api = FakeApi::new();
        run(
            &api,
            "search_files",
            json!({"in_folder": format!("https://drive.google.com/drive/folders/{A_FOLDER_ID}?usp=sharing")}),
        )
        .await
        .unwrap();
        assert_eq!(
            api.last("drive_files_list")["q"],
            format!("trashed = false and '{A_FOLDER_ID}' in parents")
        );
    }

    #[tokio::test]
    async fn list_folder_queries_the_parsed_folder_id_and_hides_trash() {
        let api = FakeApi::new();
        let out = run(
            &api,
            "list_folder",
            json!({"item": format!("https://drive.google.com/drive/folders/{A_FOLDER_ID}")}),
        )
        .await
        .unwrap();
        assert_eq!(
            api.last("drive_files_list")["q"],
            format!("'{A_FOLDER_ID}' in parents and trashed = false")
        );
        assert_eq!(out["folder_id"], A_FOLDER_ID);
    }

    #[tokio::test]
    async fn search_orders_by_recency_and_list_folder_puts_folders_first() {
        let api = FakeApi::new();
        run(&api, "search_files", json!({})).await.unwrap();
        assert_eq!(api.last("drive_files_list")["order_by"], "modifiedTime desc");

        let api = FakeApi::new();
        run(&api, "list_folder", json!({"item": A_FOLDER_ID})).await.unwrap();
        assert_eq!(api.last("drive_files_list")["order_by"], "folder,name");
    }

    #[tokio::test]
    async fn get_metadata_returns_drives_response_verbatim() {
        let raw = json!({
            "id": "F1",
            "name": "Q3 Plan",
            "mimeType": "application/pdf",
            "owners": [{"displayName": "A", "emailAddress": "a@example.com"}],
            "parents": ["P1"],
            "createdTime": "2026-01-01T00:00:00Z",
            "shared": true,
            "trashed": false,
        });
        let api = FakeApi::new();
        api.on("drive_files_get", raw.clone());

        let out = run(&api, "get_metadata", json!({"item": A_FILE_ID})).await.unwrap();

        assert_eq!(out, raw);
    }

    #[tokio::test]
    async fn get_metadata_asks_for_the_sharing_and_lifecycle_fields_too() {
        let api = FakeApi::new();
        run(&api, "get_metadata", json!({"item": A_FILE_ID})).await.unwrap();
        assert_eq!(
            api.last("drive_files_get")["fields"],
            format!("{FILE_FIELDS}, createdTime, description, shared, trashed")
        );
    }

    #[tokio::test]
    async fn an_unparseable_item_fails_before_any_drive_call() {
        for tool in ["resolve_link", "list_folder", "get_metadata"] {
            let api = FakeApi::new();
            let err = run(&api, tool, json!({"item": "not a link"})).await.unwrap_err();
            assert_eq!(err.to_string(), "could not parse a Drive ID from: 'not a link'");
            assert!(api.calls().is_empty(), "{tool} still called Drive");
        }
    }

    #[tokio::test]
    async fn an_unparseable_in_folder_fails_before_any_drive_call() {
        let api = FakeApi::new();
        let err = run(&api, "search_files", json!({"in_folder": "my folder"})).await.unwrap_err();
        assert_eq!(err.to_string(), "could not parse a Drive ID from: 'my folder'");
        assert!(api.calls().is_empty());
    }

    #[tokio::test]
    async fn a_missing_item_is_an_argument_error_not_a_drive_call() {
        let api = FakeApi::new();
        let err = run(&api, "resolve_link", json!({})).await.unwrap_err();
        assert_eq!(err.to_string(), "resolve_link: missing required argument 'item'");
        assert!(api.calls().is_empty());
    }

    #[test]
    fn the_tools_are_declared_in_the_python_modules_order() {
        let names: Vec<&str> = defs().iter().map(|d| d.name).collect();
        assert_eq!(names, ["resolve_link", "search_files", "list_folder", "get_metadata"]);
    }

    #[test]
    fn every_schema_matches_the_python_signature() {
        let defs = defs();
        let params: Vec<Vec<&str>> = defs.iter().map(|d| d.params()).collect();
        assert_eq!(params[0], ["item"]);
        assert_eq!(
            params[1],
            ["name_contains", "full_text", "mime_type", "in_folder", "page_size", "page_token"]
        );
        assert_eq!(params[2], ["item", "page_size", "page_token"]);
        assert_eq!(params[3], ["item"]);

        assert_eq!(defs[1].schema["properties"]["page_size"]["default"], 25);
        assert_eq!(defs[2].schema["properties"]["page_size"]["default"], 100);
        // search_files has a default for every argument, so nothing is required.
        assert!(defs[1].schema.get("required").is_none());
        for i in [0, 2, 3] {
            assert_eq!(defs[i].schema["required"], json!(["item"]), "{}", defs[i].name);
        }
    }

    #[test]
    fn each_description_is_the_python_docstring_verbatim() {
        let defs = defs();
        assert_eq!(
            defs[0].description,
            "Resolve a Google Drive/Docs/Sheets URL or bare ID (item) to its id, kind, name and link."
        );
        assert_eq!(
            defs[1].description,
            "Search Drive by name, full-text content, mime type, and/or parent folder.\n\n\
             Returns one page; when `has_more` is true, pass the returned `next_page_token` to continue.\n\
             `incomplete_search` true means Drive couldn't search every corpus (results may be partial)."
        );
        assert_eq!(
            defs[2].description,
            "List the direct children of a folder (item = folder URL or ID).\n\n\
             Returns one page; when `has_more` is true, pass the returned `next_page_token` to continue."
        );
    }

    #[tokio::test]
    async fn dispatch_ignores_tools_belonging_to_other_modules() {
        let api = FakeApi::new();
        let args = Args::new("read_sheet", None, &["item"]).unwrap();
        assert!(dispatch("read_sheet", &api, &args).await.is_none());
        assert!(dispatch("", &api, &args).await.is_none());
    }
}
