//! OAuth scope and on-disk locations for the client-secrets file and cached token.

use std::env;
use std::path::{Path, PathBuf};

/// Full read/write Drive access ("view and manage all your Drive files"). This is a
/// restricted scope; OAuth verification and administrator-policy requirements depend on
/// the deployment. Per-user consent bounds access to files available to the signed-in user.
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/drive",
    "https://www.googleapis.com/auth/calendar.calendarlist.readonly",
    "https://www.googleapis.com/auth/calendar.events",
    "https://www.googleapis.com/auth/calendar.events.freebusy",
];

const APP_DIR_NAME: &str = "gdrive-mcp";

fn home_dir() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Expand a leading `~` the way Python's `Path.expanduser()` does.
pub fn expanduser(p: &str) -> PathBuf {
    if p == "~" {
        return home_dir();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(p)
}

pub fn config_dir() -> PathBuf {
    let root = match env::var_os("XDG_CONFIG_HOME") {
        Some(base) if !base.is_empty() => PathBuf::from(base),
        _ => home_dir().join(".config"),
    };
    root.join(APP_DIR_NAME)
}

/// Desktop-app OAuth client file (client_id + client_secret) from Google Cloud.
///
/// Store this file securely and never commit it. Override with `GDRIVE_MCP_OAUTH_CLIENT`.
pub fn oauth_client_path() -> PathBuf {
    match env::var("GDRIVE_MCP_OAUTH_CLIENT") {
        Ok(v) if !v.is_empty() => expanduser(&v),
        _ => config_dir().join("oauth_client.json"),
    }
}

/// Cached per-user OAuth token (access + refresh). Override with `GDRIVE_MCP_TOKEN`.
pub fn token_path() -> PathBuf {
    match env::var("GDRIVE_MCP_TOKEN") {
        Ok(v) if !v.is_empty() => expanduser(&v),
        _ => config_dir().join("token.json"),
    }
}

/// Best-effort `chmod`; a filesystem that cannot represent the mode is not an error.
pub fn chmod(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ENV_LOCK;

    struct EnvGuard(&'static str, Option<String>);
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = env::var(key).ok();
            unsafe { env::set_var(key, value) };
            EnvGuard(key, prev)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.1 {
                Some(v) => unsafe { env::set_var(self.0, v) },
                None => unsafe { env::remove_var(self.0) },
            }
        }
    }

    #[test]
    fn config_dir_follows_xdg_config_home() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::set("XDG_CONFIG_HOME", "/tmp/xdg-example");
        assert_eq!(config_dir(), PathBuf::from("/tmp/xdg-example/gdrive-mcp"));
    }

    #[test]
    fn overrides_win_over_the_default_locations() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _t = EnvGuard::set("GDRIVE_MCP_TOKEN", "~/tok.json");
        let _c = EnvGuard::set("GDRIVE_MCP_OAUTH_CLIENT", "/abs/client.json");
        assert_eq!(token_path(), home_dir().join("tok.json"));
        assert_eq!(oauth_client_path(), PathBuf::from("/abs/client.json"));
    }
}
