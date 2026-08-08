//! The Google API surface this server needs, behind one trait.
//!
//! [`GoogleApi`] is the seam the tool modules are written against: [`GoogleClient`] talks to the
//! real Drive/Docs/Sheets REST endpoints with the user's OAuth credentials, and the tool tests
//! substitute a fake. Credentials are loaded (and refreshed when stale) on first use; if no
//! cached token exists that surfaces as the same "run `gdrive-mcp auth`" message the Python
//! implementation raised.

use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::Method;
use serde_json::{json, Value};
use tokio::sync::{OnceCell, RwLock};

use crate::auth::{load_credentials, refresh, write_token, Credentials};
use crate::error::{Result, ToolError};

const DRIVE: &str = "https://www.googleapis.com/drive/v3";
const DRIVE_UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3";
const DOCS: &str = "https://docs.googleapis.com/v1";
const SHEETS: &str = "https://sheets.googleapis.com/v4";

/// Everything not unreserved per RFC 3986 — Sheets A1 ranges go in the path and contain
/// `!`, `'`, `:` and spaces, all of which must survive as data rather than as path syntax.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'%')
    .add(b'!')
    .add(b'\'')
    .add(b':')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b';')
    .add(b'=')
    .add(b'&')
    .add(b'$')
    .add(b'@')
    .add(b'[')
    .add(b']');

fn seg(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

/// The Google API operations the tools use. One method per distinct REST call.
#[async_trait]
pub trait GoogleApi: Send + Sync {
    // ---- Drive ---------------------------------------------------------------------------
    async fn drive_files_get(&self, file_id: &str, fields: &str) -> Result<Value>;
    async fn drive_files_list(
        &self,
        q: &str,
        page_size: i64,
        page_token: Option<&str>,
        fields: &str,
        order_by: &str,
    ) -> Result<Value>;
    async fn drive_get_media(&self, file_id: &str) -> Result<Vec<u8>>;
    /// Drive's `files.export`; note it has no `supportsAllDrives` parameter.
    async fn drive_export(&self, file_id: &str, mime_type: &str) -> Result<Vec<u8>>;
    /// `files.update` with metadata only. `params` carries extras like addParents/removeParents.
    async fn drive_files_update(
        &self,
        file_id: &str,
        body: &Value,
        params: &[(String, String)],
        fields: &str,
    ) -> Result<Value>;
    async fn drive_files_create_media(
        &self,
        metadata: &Value,
        mime_type: &str,
        data: Vec<u8>,
        fields: &str,
    ) -> Result<Value>;
    async fn drive_files_update_media(
        &self,
        file_id: &str,
        metadata: &Value,
        mime_type: &str,
        data: Vec<u8>,
        fields: &str,
    ) -> Result<Value>;
    async fn drive_comments_list(
        &self,
        file_id: &str,
        page_size: i64,
        page_token: Option<&str>,
        fields: &str,
    ) -> Result<Value>;
    async fn drive_comments_create(&self, file_id: &str, content: &str, fields: &str) -> Result<Value>;
    async fn drive_about(&self, fields: &str) -> Result<Value>;

    // ---- Docs ----------------------------------------------------------------------------
    /// `documents.get` with `includeTabsContent=true`.
    async fn docs_get(&self, document_id: &str) -> Result<Value>;
    async fn docs_create(&self, title: &str, fields: &str) -> Result<Value>;
    async fn docs_batch_update(&self, document_id: &str, body: &Value) -> Result<Value>;

    // ---- Sheets --------------------------------------------------------------------------
    async fn sheets_get(&self, spreadsheet_id: &str, fields: &str) -> Result<Value>;
    async fn sheets_create(&self, body: &Value, fields: &str) -> Result<Value>;
    async fn sheets_values_get(&self, spreadsheet_id: &str, range: &str, render: &str) -> Result<Value>;
    async fn sheets_values_update(
        &self,
        spreadsheet_id: &str,
        range: &str,
        value_input: &str,
        values: &Value,
    ) -> Result<Value>;
    async fn sheets_values_append(
        &self,
        spreadsheet_id: &str,
        range: &str,
        value_input: &str,
        values: &Value,
    ) -> Result<Value>;
    async fn sheets_values_clear(&self, spreadsheet_id: &str, range: &str) -> Result<Value>;
    async fn sheets_batch_update(&self, spreadsheet_id: &str, body: &Value) -> Result<Value>;

    // ---- misc ----------------------------------------------------------------------------
    /// Authorized fetch of a Docs image `contentUri`; `Ok(None)` means "skip this one"
    /// (the URI answered with a redirect, which the Python port also refused to follow).
    /// Returns `(bytes, format)` where format is the content-type subtype, e.g. `png`.
    async fn fetch_image(&self, uri: &str) -> Result<Option<(Vec<u8>, String)>>;

    /// The signed-in user's email, for audit logging. Best-effort (`None` on failure).
    async fn authed_user_email(&self) -> Option<String>;
}

/// The live client: one credential set, refreshed in place, shared by every tool call.
pub struct GoogleClient {
    http: reqwest::Client,
    /// Separate client because image fetches must not follow redirects (the OAuth token
    /// would otherwise ride along to wherever the redirect points).
    no_redirect: reqwest::Client,
    creds: RwLock<Option<Credentials>>,
    email: OnceCell<Option<String>>,
}

impl Default for GoogleClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GoogleClient {
    pub fn new() -> Self {
        // Only the TLS backend can fail here, and only at startup — a server that cannot make an
        // HTTPS client has nothing to serve, so say why and stop rather than fail every call.
        let build = |builder: reqwest::ClientBuilder| {
            builder.user_agent(concat!("gdrive-mcp/", env!("CARGO_PKG_VERSION"))).build().unwrap_or_else(
                |e| panic!("gdrive-mcp: could not initialise the HTTPS client (TLS backend): {e}"),
            )
        };
        GoogleClient {
            http: build(reqwest::Client::builder()),
            no_redirect: build(reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())),
            creds: RwLock::new(None),
            email: OnceCell::new(),
        }
    }

    /// A currently-valid access token, loading and refreshing the cached credentials as needed.
    async fn token(&self) -> Result<String> {
        if let Some(c) = self.creds.read().await.as_ref() {
            if c.valid() {
                return Ok(c.token.clone());
            }
        }
        let mut guard = self.creds.write().await;
        // Another task may have refreshed while this one waited for the write lock.
        if let Some(c) = guard.as_ref() {
            if c.valid() {
                return Ok(c.token.clone());
            }
        }
        let fresh = match guard.take() {
            Some(stale) => {
                let refreshed = refresh(&stale).await?;
                // Caching the refreshed token matters; persisting it is a nicety. A read-only
                // config dir must not turn a working session into a hard failure — Python never
                // rewrote token.json after startup at all.
                if let Err(e) = write_token(&refreshed) {
                    eprintln!("gdrive-mcp: could not cache the refreshed token: {e}");
                }
                refreshed
            }
            None => load_credentials().await?,
        };
        let token = fresh.token.clone();
        *guard = Some(fresh);
        Ok(token)
    }

    /// Refresh even though the cached token still looks unexpired, because Google rejected it.
    ///
    /// No-ops unless the cache still holds the token that failed, so several tasks racing on the
    /// same 401 trigger one refresh between them rather than one each.
    async fn force_refresh(&self, used: &str) -> Result<()> {
        let mut guard = self.creds.write().await;
        let Some(stale) = guard.as_ref().filter(|c| c.token == used).cloned() else {
            return Ok(());
        };
        let refreshed = refresh(&stale).await?;
        if let Err(e) = write_token(&refreshed) {
            eprintln!("gdrive-mcp: could not cache the refreshed token: {e}");
        }
        *guard = Some(refreshed);
        Ok(())
    }

    /// Issue an authorized request, refreshing and retrying once if Google rejects the token.
    ///
    /// google-auth's `AuthorizedSession` did exactly this. Without it, a token invalidated
    /// server-side (or one whose expiry the clock disagrees about) would fail every call until
    /// the server is restarted, instead of recovering by itself.
    ///
    /// `build` may be called twice, so it must not consume its inputs.
    async fn authorized<F>(&self, build: F) -> Result<reqwest::Response>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let token = self.token().await?;
        let resp = build(&token).send().await.map_err(transport_error)?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return check(resp).await;
        }
        self.force_refresh(&token).await?;
        let retried = self.token().await?;
        let resp = build(&retried).send().await.map_err(transport_error)?;
        check(resp).await
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<reqwest::Response> {
        self.authorized(|token| {
            let mut req = self.http.request(method.clone(), url).bearer_auth(token);
            if !query.is_empty() {
                req = req.query(query);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            req
        })
        .await
    }

    async fn json(
        &self,
        method: Method,
        url: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let resp = self.send(method, url, query, body).await?;
        let text = resp.text().await.map_err(transport_error)?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| ToolError::msg(format!("unparseable API response: {e}")))
    }

    async fn bytes(&self, url: &str, query: &[(&str, String)]) -> Result<Vec<u8>> {
        let resp = self.send(Method::GET, url, query, None).await?;
        Ok(resp.bytes().await.map_err(transport_error)?.to_vec())
    }

    /// `uploadType=multipart`: a `multipart/related` body of metadata JSON + the media bytes.
    async fn upload(
        &self,
        method: Method,
        url: &str,
        metadata: &Value,
        mime_type: &str,
        data: Vec<u8>,
        fields: &str,
    ) -> Result<Value> {
        let boundary = format!("gdrive-mcp-{}", uuid::Uuid::new_v4().simple());
        let mut body: Vec<u8> = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{}\r\n",
                serde_json::to_string(metadata)?
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: {mime_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let resp = self
            .authorized(|token| {
                self.http
                    .request(method.clone(), url)
                    .bearer_auth(token)
                    // Cloned per attempt: the retry needs its own copy of the media.
                    .body(body.clone())
                    .query(&drive_upload_query(fields))
                    .header("Content-Type", format!("multipart/related; boundary={boundary}"))
            })
            .await?;
        let text = resp.text().await.map_err(transport_error)?;
        serde_json::from_str(&text).map_err(|e| ToolError::msg(format!("unparseable API response: {e}")))
    }
}

fn transport_error(e: reqwest::Error) -> ToolError {
    match e.status() {
        Some(s) => ToolError::Api { status: s.as_u16(), reason: e.to_string() },
        None => ToolError::msg(format!("HttpError: {e}")),
    }
}

/// Turn a non-2xx response into a `ToolError::Api` carrying Google's own error message.
async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let reason = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| v.get("error_description").and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                status.canonical_reason().unwrap_or("request failed").to_string()
            } else {
                trimmed.chars().take(500).collect()
            }
        });
    Err(ToolError::Api { status: status.as_u16(), reason })
}

fn q(pairs: Vec<(&str, String)>) -> Vec<(&str, String)> {
    pairs
}

// ---- query parameters ------------------------------------------------------------------------
//
// One builder per endpoint, kept separate from the request plumbing so the parameters that no
// tool-level fake can observe — the shared-drive opt-in, `insertDataOption` — can be asserted
// without a network. `supportsAllDrives=true` goes on every Drive call whose method accepts it so
// items living on a shared drive work at all, exactly as the Python port states in
// `tools/files.py:5-6`; `files.export`, `comments.*` and `about.get` have no such parameter.

fn drive_files_get_query(fields: &str) -> Vec<(&'static str, String)> {
    q(vec![("fields", fields.to_string()), ("supportsAllDrives", "true".into())])
}

fn drive_files_list_query(
    query: &str,
    page_size: i64,
    page_token: Option<&str>,
    fields: &str,
    order_by: &str,
) -> Vec<(&'static str, String)> {
    let mut params = q(vec![
        ("q", query.to_string()),
        ("pageSize", page_size.to_string()),
        ("fields", fields.to_string()),
        ("orderBy", order_by.to_string()),
        ("supportsAllDrives", "true".into()),
        // Without this the opt-in above still hides shared-drive children from every listing.
        ("includeItemsFromAllDrives", "true".into()),
    ]);
    if let Some(t) = page_token {
        params.push(("pageToken", t.to_string()));
    }
    params
}

fn drive_get_media_query() -> Vec<(&'static str, String)> {
    q(vec![("alt", "media".into()), ("supportsAllDrives", "true".into())])
}

fn drive_export_query(mime_type: &str) -> Vec<(&'static str, String)> {
    q(vec![("mimeType", mime_type.to_string())])
}

/// `params` carries the caller's extras (addParents/removeParents) after the fixed pair.
fn drive_files_update_query<'a>(params: &'a [(String, String)], fields: &str) -> Vec<(&'a str, String)> {
    let mut query = q(vec![("fields", fields.to_string()), ("supportsAllDrives", "true".into())]);
    for (k, v) in params {
        query.push((k.as_str(), v.clone()));
    }
    query
}

/// `files.create` and `files.update` with media, both through the multipart upload endpoint.
fn drive_upload_query(fields: &str) -> Vec<(&'static str, String)> {
    q(vec![
        ("uploadType", "multipart".into()),
        ("supportsAllDrives", "true".into()),
        ("fields", fields.to_string()),
    ])
}

fn drive_comments_list_query(
    page_size: i64,
    page_token: Option<&str>,
    fields: &str,
) -> Vec<(&'static str, String)> {
    let mut params = q(vec![("pageSize", page_size.to_string()), ("fields", fields.to_string())]);
    if let Some(t) = page_token {
        params.push(("pageToken", t.to_string()));
    }
    params
}

fn drive_comments_create_query(fields: &str) -> Vec<(&'static str, String)> {
    q(vec![("fields", fields.to_string())])
}

fn drive_about_query(fields: &str) -> Vec<(&'static str, String)> {
    q(vec![("fields", fields.to_string())])
}

/// `insertDataOption=INSERT_ROWS` makes Sheets add rows for the new data; the API's default,
/// `OVERWRITE`, would write over whatever already sits below the last row of the tab.
fn sheets_values_append_query(value_input: &str) -> Vec<(&'static str, String)> {
    q(vec![("valueInputOption", value_input.to_string()), ("insertDataOption", "INSERT_ROWS".into())])
}

#[async_trait]
impl GoogleApi for GoogleClient {
    async fn drive_files_get(&self, file_id: &str, fields: &str) -> Result<Value> {
        self.json(
            Method::GET,
            &format!("{DRIVE}/files/{}", seg(file_id)),
            &drive_files_get_query(fields),
            None,
        )
        .await
    }

    async fn drive_files_list(
        &self,
        query: &str,
        page_size: i64,
        page_token: Option<&str>,
        fields: &str,
        order_by: &str,
    ) -> Result<Value> {
        let params = drive_files_list_query(query, page_size, page_token, fields, order_by);
        self.json(Method::GET, &format!("{DRIVE}/files"), &params, None).await
    }

    async fn drive_get_media(&self, file_id: &str) -> Result<Vec<u8>> {
        self.bytes(&format!("{DRIVE}/files/{}", seg(file_id)), &drive_get_media_query()).await
    }

    async fn drive_export(&self, file_id: &str, mime_type: &str) -> Result<Vec<u8>> {
        self.bytes(&format!("{DRIVE}/files/{}/export", seg(file_id)), &drive_export_query(mime_type)).await
    }

    async fn drive_files_update(
        &self,
        file_id: &str,
        body: &Value,
        params: &[(String, String)],
        fields: &str,
    ) -> Result<Value> {
        let query = drive_files_update_query(params, fields);
        self.json(Method::PATCH, &format!("{DRIVE}/files/{}", seg(file_id)), &query, Some(body)).await
    }

    async fn drive_files_create_media(
        &self,
        metadata: &Value,
        mime_type: &str,
        data: Vec<u8>,
        fields: &str,
    ) -> Result<Value> {
        self.upload(Method::POST, &format!("{DRIVE_UPLOAD}/files"), metadata, mime_type, data, fields).await
    }

    async fn drive_files_update_media(
        &self,
        file_id: &str,
        metadata: &Value,
        mime_type: &str,
        data: Vec<u8>,
        fields: &str,
    ) -> Result<Value> {
        self.upload(
            Method::PATCH,
            &format!("{DRIVE_UPLOAD}/files/{}", seg(file_id)),
            metadata,
            mime_type,
            data,
            fields,
        )
        .await
    }

    async fn drive_comments_list(
        &self,
        file_id: &str,
        page_size: i64,
        page_token: Option<&str>,
        fields: &str,
    ) -> Result<Value> {
        let params = drive_comments_list_query(page_size, page_token, fields);
        self.json(Method::GET, &format!("{DRIVE}/files/{}/comments", seg(file_id)), &params, None).await
    }

    async fn drive_comments_create(&self, file_id: &str, content: &str, fields: &str) -> Result<Value> {
        self.json(
            Method::POST,
            &format!("{DRIVE}/files/{}/comments", seg(file_id)),
            &drive_comments_create_query(fields),
            Some(&json!({ "content": content })),
        )
        .await
    }

    async fn drive_about(&self, fields: &str) -> Result<Value> {
        self.json(Method::GET, &format!("{DRIVE}/about"), &drive_about_query(fields), None).await
    }

    async fn docs_get(&self, document_id: &str) -> Result<Value> {
        self.json(
            Method::GET,
            &format!("{DOCS}/documents/{}", seg(document_id)),
            &q(vec![("includeTabsContent", "true".into())]),
            None,
        )
        .await
    }

    async fn docs_create(&self, title: &str, fields: &str) -> Result<Value> {
        self.json(
            Method::POST,
            &format!("{DOCS}/documents"),
            &q(vec![("fields", fields.to_string())]),
            Some(&json!({ "title": title })),
        )
        .await
    }

    async fn docs_batch_update(&self, document_id: &str, body: &Value) -> Result<Value> {
        self.json(
            Method::POST,
            &format!("{DOCS}/documents/{}:batchUpdate", seg(document_id)),
            &[],
            Some(body),
        )
        .await
    }

    async fn sheets_get(&self, spreadsheet_id: &str, fields: &str) -> Result<Value> {
        self.json(
            Method::GET,
            &format!("{SHEETS}/spreadsheets/{}", seg(spreadsheet_id)),
            &q(vec![("fields", fields.to_string())]),
            None,
        )
        .await
    }

    async fn sheets_create(&self, body: &Value, fields: &str) -> Result<Value> {
        self.json(
            Method::POST,
            &format!("{SHEETS}/spreadsheets"),
            &q(vec![("fields", fields.to_string())]),
            Some(body),
        )
        .await
    }

    async fn sheets_values_get(&self, spreadsheet_id: &str, range: &str, render: &str) -> Result<Value> {
        self.json(
            Method::GET,
            &format!("{SHEETS}/spreadsheets/{}/values/{}", seg(spreadsheet_id), seg(range)),
            &q(vec![("valueRenderOption", render.to_string())]),
            None,
        )
        .await
    }

    async fn sheets_values_update(
        &self,
        spreadsheet_id: &str,
        range: &str,
        value_input: &str,
        values: &Value,
    ) -> Result<Value> {
        self.json(
            Method::PUT,
            &format!("{SHEETS}/spreadsheets/{}/values/{}", seg(spreadsheet_id), seg(range)),
            &q(vec![("valueInputOption", value_input.to_string())]),
            Some(&json!({ "values": values })),
        )
        .await
    }

    async fn sheets_values_append(
        &self,
        spreadsheet_id: &str,
        range: &str,
        value_input: &str,
        values: &Value,
    ) -> Result<Value> {
        self.json(
            Method::POST,
            &format!("{SHEETS}/spreadsheets/{}/values/{}:append", seg(spreadsheet_id), seg(range)),
            &sheets_values_append_query(value_input),
            Some(&json!({ "values": values })),
        )
        .await
    }

    async fn sheets_values_clear(&self, spreadsheet_id: &str, range: &str) -> Result<Value> {
        self.json(
            Method::POST,
            &format!("{SHEETS}/spreadsheets/{}/values/{}:clear", seg(spreadsheet_id), seg(range)),
            &[],
            Some(&json!({})),
        )
        .await
    }

    async fn sheets_batch_update(&self, spreadsheet_id: &str, body: &Value) -> Result<Value> {
        self.json(
            Method::POST,
            &format!("{SHEETS}/spreadsheets/{}:batchUpdate", seg(spreadsheet_id)),
            &[],
            Some(body),
        )
        .await
    }

    async fn fetch_image(&self, uri: &str) -> Result<Option<(Vec<u8>, String)>> {
        let token = self.token().await?;
        let resp = self
            .no_redirect
            .get(uri)
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(transport_error)?;
        if resp.status().is_redirection() {
            return Ok(None);
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/png")
            .to_string();
        let resp = check(resp).await?;
        let bytes = resp.bytes().await.map_err(transport_error)?.to_vec();
        // "image/jpeg; charset=x" -> "jpeg", matching the Python port's split.
        let format = content_type
            .rsplit('/')
            .next()
            .unwrap_or("png")
            .split(';')
            .next()
            .unwrap_or("png")
            .trim()
            .to_string();
        Ok(Some((bytes, format)))
    }

    async fn authed_user_email(&self) -> Option<String> {
        self.email
            .get_or_init(|| async {
                self.drive_about("user/emailAddress")
                    .await
                    .ok()
                    .and_then(|v| v.pointer("/user/emailAddress").and_then(Value::as_str).map(str::to_string))
            })
            .await
            .clone()
    }
}

/// The request parameters no tool-level fake can see, plus a 5xx-free sanity check that our
/// status mapping matches what `errors.explain()` produced.
#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn ranges_are_percent_encoded_into_the_path() {
        assert_eq!(seg("'John''s Data'!A1:B2"), "%27John%27%27s%20Data%27%21A1%3AB2");
        assert_eq!(seg("Sheet1"), "Sheet1");
    }

    #[test]
    fn appending_inserts_rows_rather_than_overwriting_below_the_last_row() {
        let query = sheets_values_append_query("RAW");
        assert!(
            query.contains(&("insertDataOption", "INSERT_ROWS".to_string())),
            "an append without INSERT_ROWS overwrites whatever sits below the data: {query:?}"
        );
        assert!(query.contains(&("valueInputOption", "RAW".to_string())), "{query:?}");
    }

    /// Whether an endpoint's REST method takes `supportsAllDrives` at all.
    #[derive(PartialEq)]
    enum OptIn {
        Required,
        NotAccepted,
    }

    /// A `GoogleApi` method name, the query it builds, and the rule that query must follow.
    type DriveEndpoint = (&'static str, Vec<(&'static str, String)>, OptIn);

    /// Every Drive query this client builds, next to the rule its method is held to. The
    /// arguments are placeholders; only the fixed parameters are under test.
    fn drive_queries() -> Vec<DriveEndpoint> {
        vec![
            ("drive_files_get", drive_files_get_query("id,name"), OptIn::Required),
            (
                "drive_files_list",
                drive_files_list_query("trashed = false", 100, None, "files(id)", "modifiedTime desc"),
                OptIn::Required,
            ),
            ("drive_get_media", drive_get_media_query(), OptIn::Required),
            ("drive_files_update", drive_files_update_query(&[], "id,name"), OptIn::Required),
            ("drive_files_create_media", drive_upload_query("id,name"), OptIn::Required),
            ("drive_files_update_media", drive_upload_query("id,name"), OptIn::Required),
            // Methods with no such parameter: `files.export` is the documented exception, and
            // comments/about are not addressed per drive.
            ("drive_export", drive_export_query("text/plain"), OptIn::NotAccepted),
            ("drive_comments_list", drive_comments_list_query(100, None, "comments(id)"), OptIn::NotAccepted),
            ("drive_comments_create", drive_comments_create_query("id,content"), OptIn::NotAccepted),
            ("drive_about", drive_about_query("user/emailAddress"), OptIn::NotAccepted),
        ]
    }

    /// The `GoogleApi` methods that reach a Drive base URL, read out of this file's own source —
    /// the only way a test can notice an endpoint nobody remembered to describe above.
    fn drive_calls_in_source() -> Vec<String> {
        let source = include_str!("clients.rs");
        let live = source.split_once("impl GoogleApi for GoogleClient {").expect("the live impl").1;
        // Cut at the impl's closing brace — the first one in column 0 — so this test module,
        // which names the same base URLs, is not read as production code.
        let live = live.split("\n}\n").next().unwrap_or_default();
        let mut method = String::new();
        let mut calls: Vec<String> = Vec::new();
        for line in live.lines() {
            if let Some(rest) = line.trim_start().strip_prefix("async fn ") {
                method = rest.split('(').next().unwrap_or_default().to_string();
            }
            if (line.contains("{DRIVE}") || line.contains("{DRIVE_UPLOAD}")) && !calls.contains(&method) {
                calls.push(method.clone());
            }
        }
        calls
    }

    #[test]
    fn every_drive_call_whose_method_accepts_it_opts_into_shared_drives() {
        for (method, query, rule) in drive_queries() {
            let opted_in = query.iter().any(|(k, v)| *k == "supportsAllDrives" && v == "true");
            match rule {
                OptIn::Required => assert!(
                    opted_in,
                    "{method} must send supportsAllDrives=true or items on a shared drive 404: {query:?}"
                ),
                OptIn::NotAccepted => {
                    assert!(!opted_in, "{method}'s method has no supportsAllDrives parameter: {query:?}")
                }
            }
        }
    }

    #[test]
    fn listing_also_asks_for_the_items_that_live_on_other_drives() {
        let query = drive_files_list_query("trashed = false", 100, None, "files(id)", "name");
        assert!(
            query.contains(&("includeItemsFromAllDrives", "true".to_string())),
            "search/list would come back without any shared-drive item: {query:?}"
        );
    }

    /// The net under `drive_queries`: a Drive endpoint added later cannot dodge the shared-drive
    /// rule by simply not being listed there.
    #[test]
    fn the_shared_drive_table_covers_every_drive_call_this_client_makes() {
        let described: Vec<&str> = drive_queries().iter().map(|(method, ..)| *method).collect();
        let called = drive_calls_in_source();
        assert!(!called.is_empty(), "the source scan found no Drive calls, so it is measuring nothing");
        for method in &called {
            assert!(
                described.contains(&method.as_str()),
                "{method} calls Drive but is missing from drive_queries(): add it there with the \
                 rule its REST method follows"
            );
        }
        for method in described {
            assert!(
                called.iter().any(|c| c == method),
                "{method} no longer calls Drive; drop its drive_queries() entry"
            );
        }
    }

    #[tokio::test]
    async fn google_error_bodies_become_api_errors_with_their_message() {
        let resp = http_response(404, r#"{"error":{"code":404,"message":"File not found: abc."}}"#);
        let err = check(resp).await.unwrap_err();
        assert_eq!(err.status(), Some(404));
        assert_eq!(
            err.to_string(),
            "Google API error 404: File not found: abc. (check the ID/URL and that your account has access)"
        );
    }

    #[tokio::test]
    async fn non_json_error_bodies_still_produce_a_readable_message() {
        let resp = http_response(503, "upstream unavailable");
        let err = check(resp).await.unwrap_err();
        assert_eq!(err.to_string(), "Google API error 503: upstream unavailable");
    }

    fn http_response(status: u16, body: &str) -> reqwest::Response {
        let raw = http::Response::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .body(body.to_string())
            .unwrap();
        reqwest::Response::from(raw)
    }
}
