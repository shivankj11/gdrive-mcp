//! Browser-OAuth setup and credential loading for the Drive MCP.
//!
//! The interactive browser consent is a one-time step (`gdrive-mcp auth`) kept separate from the
//! MCP server runtime: a stdio MCP server launched by an MCP client must complete its init
//! handshake promptly and cannot block on a browser prompt. After `auth`, callers use
//! [`load_credentials`], which loads the cached token and refreshes it silently — never opening
//! a browser.
//!
//! The on-disk token is byte-compatible with the Python implementation's
//! `google.oauth2.credentials.Credentials.to_json()`, so the two servers share one cached login.

use std::fmt;
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::{chmod, oauth_client_path, token_path, SCOPES};

/// No usable credentials — the caller should prompt the user to run `auth`.
#[derive(Debug, Clone)]
pub struct AuthError(pub String);

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AuthError {}

impl From<AuthError> for crate::error::ToolError {
    fn from(e: AuthError) -> Self {
        crate::error::ToolError::Msg(e.0)
    }
}

type Result<T> = std::result::Result<T, AuthError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(AuthError(msg.into()))
}

const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_AUTH_URI: &str = "https://accounts.google.com/o/oauth2/auth";
/// Refresh this far ahead of the recorded expiry, matching google-auth's clock skew allowance.
const SKEW_SECONDS: i64 = 60;

/// The cached token, in the exact shape google-auth writes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub token_uri: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
    #[serde(default = "default_universe")]
    pub universe_domain: String,
    #[serde(default)]
    pub account: String,
    /// `YYYY-MM-DDTHH:MM:SS.ffffffZ`, or absent for a token with no known expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<String>,
}

fn default_universe() -> String {
    "googleapis.com".to_string()
}

impl Credentials {
    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        let raw = self.expiry.as_deref()?;
        // google-auth writes a naive UTC isoformat with a 'Z' suffix; be lenient about the
        // fractional part and about a token written by something that used an offset.
        DateTime::parse_from_rfc3339(raw).map(|d| d.with_timezone(&Utc)).ok().or_else(|| {
            chrono::NaiveDateTime::parse_from_str(raw.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc())
        })
    }

    /// Whether the access token is usable right now (present and not within the skew window).
    pub fn valid(&self) -> bool {
        if self.token.is_empty() {
            return false;
        }
        match self.expires_at() {
            None => true,
            Some(exp) => Utc::now() + chrono::Duration::seconds(SKEW_SECONDS) < exp,
        }
    }

    pub fn expired(&self) -> bool {
        !self.valid()
    }
}

fn set_expiry(creds: &mut Credentials, expires_in: Option<i64>) {
    creds.expiry = expires_in.map(|secs| {
        (Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Micros, true)
    });
}

pub fn write_token(creds: &Credentials) -> Result<std::path::PathBuf> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return err(format!("could not create {}: {e}", parent.display()));
        }
    }
    let body = match serde_json::to_string(creds) {
        Ok(b) => b,
        Err(e) => return err(format!("could not serialize credentials: {e}")),
    };
    if let Err(e) = std::fs::write(&path, body) {
        return err(format!("could not write {}: {e}", path.display()));
    }
    chmod(&path, 0o600);
    Ok(path)
}

fn read_token(path: &Path) -> Result<Credentials> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) => return err(format!("could not read {}: {e}", path.display())),
    };
    serde_json::from_str(&raw)
        .map_err(|e| AuthError(format!("cached credentials at {} are unreadable: {e}", path.display())))
}

/// Load cached credentials, refreshing if expired. Never opens a browser.
pub async fn load_credentials() -> Result<Credentials> {
    let path = token_path();
    if !path.exists() {
        return err(format!("No cached credentials at {}. Run `gdrive-mcp auth` first.", path.display()));
    }
    let creds = read_token(&path)?;
    let missing: Vec<&str> = SCOPES
        .iter()
        .copied()
        .filter(|required| !creds.scopes.iter().any(|granted| granted == required))
        .collect();
    if !missing.is_empty() {
        let names: Vec<&str> =
            missing.iter().map(|scope| scope.rsplit('/').next().unwrap_or(scope)).collect();
        return err(format!(
            "Cached credentials at {} do not grant the required scopes ({}). Run `gdrive-mcp auth` again to approve the added access.",
            path.display(),
            names.join(", ")
        ));
    }
    if creds.valid() {
        return Ok(creds);
    }
    if creds.expired() && creds.refresh_token.is_some() {
        let refreshed = refresh(&creds).await?;
        write_token(&refreshed)?;
        return Ok(refreshed);
    }
    err("Cached credentials are invalid and cannot be refreshed. Run `gdrive-mcp auth` again.")
}

/// Exchange the refresh token for a fresh access token.
pub async fn refresh(creds: &Credentials) -> Result<Credentials> {
    let Some(refresh_token) = creds.refresh_token.clone() else {
        return err("Cached credentials have no refresh token. Run `gdrive-mcp auth` again.");
    };
    let uri = if creds.token_uri.is_empty() { DEFAULT_TOKEN_URI } else { &creds.token_uri };
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", creds.client_id.as_str()),
        ("client_secret", creds.client_secret.as_str()),
    ];
    let resp = reqwest::Client::new()
        .post(uri)
        .form(&form)
        .send()
        .await
        .map_err(|e| AuthError(format!("token refresh failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return err(format!(
            "token refresh rejected ({}): {}. Run `gdrive-mcp auth` again.",
            status.as_u16(),
            body.trim()
        ));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|e| AuthError(format!("token refresh returned unparseable JSON: {e}")))?;
    let mut out = creds.clone();
    out.token = parsed
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError("token refresh response had no access_token".into()))?
        .to_string();
    // Google only re-issues a refresh token on rotation; keep the existing one otherwise.
    if let Some(rt) = parsed.get("refresh_token").and_then(Value::as_str) {
        out.refresh_token = Some(rt.to_string());
    }
    set_expiry(&mut out, parsed.get("expires_in").and_then(Value::as_i64));
    Ok(out)
}

// ---- one-time browser consent -------------------------------------------------------------

#[derive(Debug, Clone)]
struct ClientSecrets {
    client_id: String,
    client_secret: String,
    auth_uri: String,
    token_uri: String,
}

fn load_client_secrets(path: &Path) -> Result<ClientSecrets> {
    if !path.exists() {
        return err(format!(
            "OAuth client file not found at {}. Download the Desktop-app OAuth client JSON from \
             Google Cloud and place it there, or set GDRIVE_MCP_OAUTH_CLIENT to its path.",
            path.display()
        ));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| AuthError(format!("could not read {}: {e}", path.display())))?;
    let doc: Value = serde_json::from_str(&raw)
        .map_err(|e| AuthError(format!("{} is not valid JSON: {e}", path.display())))?;
    let inner = doc.get("installed").or_else(|| doc.get("web")).unwrap_or(&doc);
    let s = |k: &str, default: &str| {
        inner.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or(default).to_string()
    };
    let client_id = s("client_id", "");
    let client_secret = s("client_secret", "");
    if client_id.is_empty() || client_secret.is_empty() {
        return err(format!(
            "{} is missing client_id/client_secret — is it a Desktop-app OAuth client JSON?",
            path.display()
        ));
    }
    Ok(ClientSecrets {
        client_id,
        client_secret,
        auth_uri: s("auth_uri", DEFAULT_AUTH_URI),
        token_uri: s("token_uri", DEFAULT_TOKEN_URI),
    })
}

/// Run the one-time loopback browser-consent flow and cache the token.
pub async fn run_auth_flow() -> Result<Credentials> {
    let secrets = load_client_secrets(&oauth_client_path())?;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| AuthError(format!("could not bind a loopback port for the OAuth redirect: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| AuthError(format!("could not read the loopback port: {e}")))?
        .port();
    let redirect_uri = format!("http://localhost:{port}/");

    let state = uuid::Uuid::new_v4().to_string();
    let mut auth_url = url::Url::parse(&secrets.auth_uri)
        .map_err(|e| AuthError(format!("invalid auth_uri in the OAuth client file: {e}")))?;
    auth_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &secrets.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("state", &state)
        // access_type=offline + prompt=consent force Google to return a refresh token, so the
        // server never needs the browser again.
        .append_pair("access_type", "offline")
        .append_pair("include_granted_scopes", "true")
        .append_pair("prompt", "consent");

    eprintln!("Please visit this URL to authorize this application:\n{auth_url}\n");
    open_browser(auth_url.as_str());

    let code = wait_for_code(&listener, &state).await?;

    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", secrets.client_id.as_str()),
        ("client_secret", secrets.client_secret.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
    ];
    let resp = reqwest::Client::new()
        .post(&secrets.token_uri)
        .form(&form)
        .send()
        .await
        .map_err(|e| AuthError(format!("token exchange failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return err(format!("token exchange rejected ({}): {}", status.as_u16(), body.trim()));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|e| AuthError(format!("token exchange returned unparseable JSON: {e}")))?;

    let mut creds = Credentials {
        token: parsed
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| AuthError("token exchange response had no access_token".into()))?
            .to_string(),
        refresh_token: parsed.get("refresh_token").and_then(Value::as_str).map(str::to_string),
        token_uri: secrets.token_uri.clone(),
        client_id: secrets.client_id.clone(),
        client_secret: secrets.client_secret.clone(),
        scopes: SCOPES.iter().map(|s| s.to_string()).collect(),
        universe_domain: default_universe(),
        account: String::new(),
        expiry: None,
    };
    set_expiry(&mut creds, parsed.get("expires_in").and_then(Value::as_i64));
    write_token(&creds)?;
    Ok(creds)
}

/// Serve the loopback redirect until the browser delivers a matching `code`.
async fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut socket, _) =
            listener.accept().await.map_err(|e| AuthError(format!("loopback listener failed: {e}")))?;

        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let Some(target) = request.split_whitespace().nth(1) else {
            let _ = reply(&mut socket, "400 Bad Request", "Malformed request.").await;
            continue;
        };
        // Browsers also request /favicon.ico against this listener; ignore anything without a query.
        let Ok(parsed) = url::Url::parse(&format!("http://localhost{target}")) else {
            let _ = reply(&mut socket, "400 Bad Request", "Malformed request.").await;
            continue;
        };
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        if let Some(oauth_err) = params.get("error") {
            let _ =
                reply(&mut socket, "400 Bad Request", "Authorization was denied. You can close this window.")
                    .await;
            return err(format!("authorization denied by Google: {oauth_err}"));
        }
        let Some(code) = params.get("code") else {
            let _ = reply(&mut socket, "404 Not Found", "Waiting for the authorization redirect…").await;
            continue;
        };
        if params.get("state").map(String::as_str) != Some(expected_state) {
            let _ = reply(&mut socket, "400 Bad Request", "State mismatch. Please retry the sign-in.").await;
            return err("OAuth state mismatch — the redirect did not come from this sign-in attempt.");
        }
        let _ = reply(
            &mut socket,
            "200 OK",
            "Authentication complete. You can close this window and return to the terminal.",
        )
        .await;
        return Ok(code.clone());
    }
}

async fn reply(socket: &mut tokio::net::TcpStream, status: &str, message: &str) -> std::io::Result<()> {
    let body = format!("<html><body><h3>{message}</h3></body></html>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let cmd = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let cmd: (&str, Vec<&str>) = ("", vec![url]);

    if cmd.0.is_empty() {
        return;
    }
    let _ = std::process::Command::new(cmd.0)
        .args(&cmd.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ENV_LOCK;

    fn creds_with_expiry(expiry: Option<&str>) -> Credentials {
        Credentials {
            token: "at".into(),
            refresh_token: Some("rt".into()),
            token_uri: DEFAULT_TOKEN_URI.into(),
            client_id: "cid".into(),
            client_secret: "secret".into(),
            scopes: SCOPES.iter().map(|s| s.to_string()).collect(),
            universe_domain: default_universe(),
            account: String::new(),
            expiry: expiry.map(str::to_string),
        }
    }

    #[test]
    fn parses_the_naive_utc_expiry_google_auth_writes() {
        let c = creds_with_expiry(Some("2030-01-02T03:04:05.123456Z"));
        let exp = c.expires_at().expect("expiry should parse");
        assert_eq!(exp.to_rfc3339_opts(SecondsFormat::Micros, true), "2030-01-02T03:04:05.123456Z");
        assert!(c.valid());
    }

    #[test]
    fn a_past_expiry_is_expired() {
        assert!(creds_with_expiry(Some("2000-01-01T00:00:00Z")).expired());
    }

    #[test]
    fn an_expiry_inside_the_skew_window_counts_as_expired() {
        let soon = (Utc::now() + chrono::Duration::seconds(SKEW_SECONDS / 2))
            .to_rfc3339_opts(SecondsFormat::Micros, true);
        assert!(creds_with_expiry(Some(&soon)).expired());
    }

    #[test]
    fn round_trips_through_the_python_token_json_shape() {
        let c = creds_with_expiry(Some("2030-01-02T03:04:05.123456Z"));
        let json = serde_json::to_value(&c).unwrap();
        for key in [
            "token",
            "refresh_token",
            "token_uri",
            "client_id",
            "client_secret",
            "scopes",
            "universe_domain",
            "account",
            "expiry",
        ] {
            assert!(json.get(key).is_some(), "missing {key} in serialized credentials");
        }
        let back: Credentials = serde_json::from_value(json).unwrap();
        assert_eq!(back.token, c.token);
        assert_eq!(back.scopes, c.scopes);
    }

    #[test]
    fn tolerates_a_token_file_google_auth_wrote_without_an_expiry() {
        let c: Credentials = serde_json::from_str(
            r#"{"token":"at","refresh_token":"rt","token_uri":"https://oauth2.googleapis.com/token",
                "client_id":"cid","client_secret":"cs","scopes":["https://www.googleapis.com/auth/drive"]}"#,
        )
        .unwrap();
        assert!(c.valid());
        assert_eq!(c.universe_domain, "googleapis.com");
    }

    #[tokio::test]
    async fn an_old_drive_only_token_requests_reconsent_for_calendar() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        std::fs::write(
            &path,
            r#"{"token":"at","refresh_token":"rt","token_uri":"https://oauth2.googleapis.com/token",
                "client_id":"cid","client_secret":"cs","scopes":["https://www.googleapis.com/auth/drive"],
                "expiry":"2030-01-02T03:04:05.123456Z"}"#,
        )
        .unwrap();
        let old = std::env::var_os("GDRIVE_MCP_TOKEN");
        unsafe { std::env::set_var("GDRIVE_MCP_TOKEN", &path) };
        let result = load_credentials().await;
        match old {
            Some(value) => unsafe { std::env::set_var("GDRIVE_MCP_TOKEN", value) },
            None => unsafe { std::env::remove_var("GDRIVE_MCP_TOKEN") },
        }
        let message = result.unwrap_err().to_string();
        assert!(message.contains("Run `gdrive-mcp auth` again"), "{message}");
        assert!(message.contains("calendar.events"), "{message}");
    }
}
