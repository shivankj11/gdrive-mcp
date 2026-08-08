//! Confine the server's local file I/O to one operator-configured sandbox directory.
//!
//! Every local read (upload source) and write (download/export/sheet spill) is resolved inside
//! `GDRIVE_MCP_FILES_DIR` (default a private 0700 dir under the config dir). Absolute paths and
//! `..` escapes are rejected, so a prompt-injected agent can neither write outside the sandbox
//! (e.g. ~/.ssh/authorized_keys, cron files) nor read arbitrary local files (e.g. id_rsa) to
//! exfiltrate them to Drive.
//!
//! Deletion is gated on ownership: the retention sweep only runs in a directory carrying the
//! `.gdrive-mcp-sandbox` marker, which is written only when gdrive-mcp created the directory
//! itself (or in the app-default location). Pointing `GDRIVE_MCP_FILES_DIR` at a pre-existing
//! directory therefore confines I/O there but never lets the sweep delete the files that were
//! already in it.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{chmod, config_dir, expanduser};
use crate::error::{Result, ToolError};

/// Marks a directory as gdrive-mcp's own disposable sandbox. `sweep_expired` deletes nothing in
/// a directory that lacks it; operators can opt a hand-picked directory in by creating it.
pub const MARKER_NAME: &str = ".gdrive-mcp-sandbox";

const MARKER_BODY: &str = "This directory is gdrive-mcp's disposable file sandbox: files older than the\n\
                           retention TTL (GDRIVE_MCP_FILES_TTL_HOURS, default 24h) are deleted on server\n\
                           start. Do not keep anything here.\n";

/// The sandbox override. An empty value is "unset", matching Python's falsy-string test.
fn files_dir_override() -> Option<String> {
    std::env::var("GDRIVE_MCP_FILES_DIR").ok().filter(|v| !v.is_empty())
}

/// Drop the ownership marker (best-effort — an unwritable root just leaves the sweep off).
fn mark_owned(root: &Path) {
    let marker = root.join(MARKER_NAME);
    if !marker.exists() {
        let _ = fs::write(&marker, MARKER_BODY);
    }
}

/// POSIX `realpath` semantics, the same ones Python's non-strict `Path.resolve()` has: walk the
/// path one component at a time, expanding every symlink as it is met, and let missing components
/// through untouched.
///
/// `canonicalize()` cannot stand in for this. It fails outright on a path whose tail does not
/// exist, and backing off to the longest existing ancestor would leave a **dangling** symlink
/// unexpanded — so `<sandbox>/link -> /etc`, created before its target, would pass the containment
/// check as `<sandbox>/link` and then be written through. `symlink_metadata` sees the link whether
/// or not its target exists, which closes that hole. Walking also preserves the caller's spelling,
/// so on a case-insensitive filesystem `../FILES/x` stays outside a sandbox named `files` instead
/// of being folded back into it.
fn resolve_lenient(path: &Path) -> PathBuf {
    /// POSIX `MAXSYMLINKS`. Past this, stop expanding and treat the name literally — that
    /// terminates a symlink cycle without escaping the sandbox (the unexpanded name stays inside).
    const MAX_HOPS: u32 = 40;

    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    // `resolved` accumulates the answer; `queue` holds the components still to walk. Splitting
    // them this way is what lets a symlink target be spliced in ahead of the remaining path.
    let mut resolved = PathBuf::new();
    let mut queue: VecDeque<OsString> = VecDeque::new();
    let push_components = |p: &Path, resolved: &mut PathBuf, out: &mut Vec<OsString>| {
        for c in p.components() {
            match c {
                Component::Prefix(_) | Component::RootDir => resolved.push(c.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => out.push(OsString::from("..")),
                Component::Normal(s) => out.push(s.to_os_string()),
            }
        }
    };
    let mut initial = Vec::new();
    push_components(&abs, &mut resolved, &mut initial);
    queue.extend(initial);

    let mut hops = 0u32;
    while let Some(part) = queue.pop_front() {
        if part == ".." {
            // `..` applies to the already-resolved path, so a link expanded above it is honoured.
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&part);
        let is_link = std::fs::symlink_metadata(&candidate).is_ok_and(|m| m.file_type().is_symlink());
        if is_link && hops < MAX_HOPS {
            if let Ok(target) = std::fs::read_link(&candidate) {
                hops += 1;
                let mut spliced = Vec::new();
                if target.is_absolute() {
                    resolved = PathBuf::new(); // an absolute target restarts from the root
                }
                push_components(&target, &mut resolved, &mut spliced);
                for c in spliced.into_iter().rev() {
                    queue.push_front(c);
                }
                continue;
            }
        }
        resolved.push(part);
    }
    resolved
}

/// Lexical containment, like Python's `Path.relative_to` — both compare whole path components,
/// so `/files-elsewhere` is not "inside" `/files`.
fn within(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok()
}

/// The last component the way `pathlib` reports `Path(name).name`: unlike Rust's `file_name()`,
/// `pathlib` keeps a trailing `..` (which `_sanitize` then passes through, and containment
/// rejects) and drops a trailing `.`.
fn basename(name: &str) -> &str {
    name.rsplit('/').find(|part| !part.is_empty() && *part != ".").unwrap_or("")
}

fn sanitize(name: &str) -> String {
    // `str.isalnum()` is Unicode-aware, so accented letters and non-Latin scripts survive.
    let keep = |c: char| c.is_alphanumeric() || "-_.".contains(c);
    let base = basename(name); // drop any directory components from an untrusted name
    let cleaned: String = base.chars().map(|c| if keep(c) { c } else { '_' }).collect();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

/// Python's `{x!r}` for an optional string: quoted when present, bare `None` when not.
fn repr(value: Option<&str>) -> String {
    match value {
        Some(v) => format!("'{v}'"),
        None => "None".to_string(),
    }
}

/// The sandbox directory; created 0700 + ownership-marked when it doesn't exist.
///
/// A pre-existing directory supplied via `GDRIVE_MCP_FILES_DIR` is used as-is — permissions
/// untouched, no ownership marker — so only directories gdrive-mcp created (or the app-default
/// location, which is ours by definition) are ever eligible for the retention sweep.
pub fn files_root() -> Result<PathBuf> {
    let override_dir = files_dir_override();
    let root = resolve_lenient(&match &override_dir {
        Some(v) => expanduser(v),
        None => config_dir().join("files"),
    });
    // Python used `mkdir(parents=True)` without `exist_ok`, because "it was already there" is
    // precisely the signal that the directory is not ours to chmod, mark or later sweep.
    let created = match fs::create_dir(&root) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if !root.is_dir() {
                return Err(e.into());
            }
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&root)?;
            true
        }
        Err(e) => return Err(e.into()),
    };
    if created || override_dir.is_none() {
        chmod(&root, 0o700);
        mark_owned(&root);
    }
    Ok(root)
}

/// Resolve a write destination inside the sandbox.
///
/// `dest_path` is treated as relative to the sandbox root; absolute paths and `..` escapes are
/// rejected. When None, a sanitized `default_name` at the root is used. Parent dirs are created.
pub fn safe_write_path(dest_path: Option<&str>, default_name: &str) -> Result<PathBuf> {
    let root = files_root()?;
    // An absolute `dest_path` replaces the root when joined, and is then caught by containment.
    let rel = match dest_path.filter(|d| !d.is_empty()) {
        Some(d) => d.to_string(),
        None => sanitize(default_name),
    };
    let final_path = resolve_lenient(&root.join(rel));
    if !within(&final_path, &root) {
        return Err(ToolError::msg(format!(
            "dest_path {} must stay within the files dir ({}); absolute paths and '..' are not \
             allowed. Set GDRIVE_MCP_FILES_DIR to change the sandbox.",
            repr(dest_path),
            root.display()
        )));
    }
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(final_path)
}

/// Resolve an upload source inside the sandbox; reject escapes and missing files.
pub fn safe_read_path(source_path: &str) -> Result<PathBuf> {
    let root = files_root()?;
    let final_path = resolve_lenient(&root.join(source_path));
    if !within(&final_path, &root) {
        return Err(ToolError::msg(format!(
            "source_path {} must be inside the files dir ({}); place the file there or set \
             GDRIVE_MCP_FILES_DIR. Absolute paths and '..' are not allowed.",
            repr(Some(source_path)),
            root.display()
        )));
    }
    if !final_path.is_file() {
        return Err(ToolError::msg(format!("source_path not found in files dir: {}", final_path.display())));
    }
    Ok(final_path)
}

/// An unset — or unparseable — `GDRIVE_MCP_FILES_TTL_HOURS` means the 24h default, never an error.
fn env_ttl_hours() -> f64 {
    std::env::var("GDRIVE_MCP_FILES_TTL_HOURS")
        .map(|v| v.trim().parse::<f64>().unwrap_or(24.0))
        .unwrap_or(24.0)
}

/// Why a directory without the ownership marker is left alone, and how to opt it in.
fn refusal_notice(root: &Path) -> String {
    format!(
        "gdrive-mcp: not sweeping {}: the directory pre-existed, so its contents may not be \
         disposable spills. To enable the retention sweep there, run: touch '{}'",
        root.display(),
        root.join(MARKER_NAME).display()
    )
}

/// Seconds since the epoch, signed so a pre-1970 mtime still compares as "old".
fn unix_secs(t: SystemTime) -> Option<f64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(d.as_secs_f64()),
        Err(e) => Some(-e.duration().as_secs_f64()),
    }
}

/// One level of `Path.rglob("*")` + unlink. Every step is skippable: an unreadable subdirectory
/// or a file that vanishes mid-sweep must not abort the rest.
fn sweep_dir(dir: &Path, cutoff: f64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(link_meta) = fs::symlink_metadata(&path) else { continue };
        if link_meta.is_dir() {
            // `rglob` does not descend through symlinked directories; neither do we, which also
            // makes a symlink loop unable to spin the sweep forever.
            sweep_dir(&path, cutoff);
            continue;
        }
        if path.file_name() == Some(OsStr::new(MARKER_NAME)) {
            continue;
        }
        // `is_file`/`stat` follow symlinks (so a link to a directory is not a candidate, and a
        // link to a file is judged by the target's mtime), while unlink removes the link itself.
        let Ok(meta) = fs::metadata(&path) else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(mtime) = meta.modified().ok().and_then(unix_secs) else { continue };
        if mtime < cutoff {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Delete sandbox files older than the retention TTL. Best-effort and fully failure-safe.
///
/// Default 24h, from `GDRIVE_MCP_FILES_TTL_HOURS`. Bounds at-rest disposal of spilled
/// downloads/exports/CSVs. TTL <= 0 disables the sweep. Refuses to sweep a directory without the
/// [`MARKER_NAME`] ownership marker: a pre-existing `GDRIVE_MCP_FILES_DIR` may hold files that
/// are not disposable spills, and deleting them on startup would be silent data loss. Creating
/// the marker there opts the directory in. A bad TTL value or unusable files dir never breaks
/// startup.
pub fn sweep_expired(ttl_hours: Option<f64>) {
    let ttl = ttl_hours.unwrap_or_else(env_ttl_hours);
    // A NaN TTL falls through this test exactly as Python's `ttl_hours <= 0` did; every later
    // comparison against the resulting NaN cutoff is false, so the sweep deletes nothing.
    if ttl <= 0.0 {
        return;
    }
    let Ok(root) = files_root() else { return };
    if !root.join(MARKER_NAME).exists() {
        let _ = writeln!(std::io::stderr(), "{}", refusal_notice(&root));
        return;
    }
    let Some(now) = unix_secs(SystemTime::now()) else { return };
    sweep_dir(&root, now - ttl * 3600.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use std::time::Duration;

    use crate::ENV_LOCK;

    struct Env(Vec<(&'static str, Option<String>)>);
    impl Env {
        fn new(pairs: &[(&'static str, Option<&str>)]) -> Env {
            let mut saved = Vec::new();
            for (k, v) in pairs {
                saved.push((*k, std::env::var(k).ok()));
                match v {
                    Some(value) => unsafe { std::env::set_var(k, value) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
            Env(saved)
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(value) => unsafe { std::env::set_var(k, value) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    fn point_files_dir_at(path: &Path) -> Env {
        Env::new(&[
            ("GDRIVE_MCP_FILES_DIR", Some(&path.to_string_lossy())),
            ("GDRIVE_MCP_FILES_TTL_HOURS", None),
        ])
    }

    /// The pytest `sandbox` fixture: aim at a nonexistent dir so `files_root()` creates and
    /// marks it — the real server flow.
    fn sandbox(tmp: &tempfile::TempDir) -> (Env, PathBuf) {
        let env = point_files_dir_at(&tmp.path().join("files"));
        let root = files_root().unwrap();
        (env, root)
    }

    fn write(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
    }

    fn age(path: &Path, hours: f64) {
        let past = SystemTime::now() - Duration::from_secs_f64(hours * 3600.0);
        let f = File::options().write(true).open(path).unwrap();
        f.set_times(FileTimes::new().set_modified(past)).unwrap();
    }

    #[test]
    fn a_relative_write_destination_stays_inside_the_sandbox_and_its_parents_are_created() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        let p = safe_write_path(Some("sub/dir/out.bin"), "ignored").unwrap();
        assert_eq!(p, root.join("sub").join("dir").join("out.bin"));
        assert!(p.parent().unwrap().is_dir());
    }

    #[test]
    fn a_default_name_is_reduced_to_a_sanitized_basename_at_the_root() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        let p = safe_write_path(None, "../../etc/pa ss/wd").unwrap();
        assert_eq!(p.parent().unwrap(), root);
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/') && !name.contains(".."), "{name}");
        assert_eq!(name, "wd");
    }

    #[test]
    fn a_name_with_nothing_usable_left_becomes_download() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        assert_eq!(safe_write_path(None, "/").unwrap(), root.join("download"));
    }

    #[test]
    fn the_sanitizer_keeps_unicode_letters_and_replaces_everything_else() {
        // Python's str.isalnum() is Unicode-aware: 'é' survives, the space and emoji do not.
        assert_eq!(sanitize("café ☕.txt"), "café__.txt");
        assert_eq!(sanitize("a:b*c?d.txt"), "a_b_c_d.txt");
        assert_eq!(sanitize("keep-this_name.v2"), "keep-this_name.v2");
    }

    #[test]
    fn an_absolute_dest_path_is_rejected() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, _root) = sandbox(&tmp);
        let err = safe_write_path(Some("/etc/cron.d/x"), "d").unwrap_err();
        assert!(err.to_string().starts_with("dest_path '/etc/cron.d/x' must stay within"), "{err}");
    }

    #[test]
    fn a_dest_path_climbing_out_with_dotdot_is_rejected() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, _root) = sandbox(&tmp);
        assert!(safe_write_path(Some("../../../etc/x"), "d").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_in_the_sandbox_cannot_be_used_to_escape_it() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        assert!(safe_write_path(Some("escape/loot.txt"), "d").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_cannot_be_used_to_escape_either() {
        // The link's target does not exist yet, so nothing along the path can be canonicalised —
        // the escape only shows up if the link itself is expanded.
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        std::os::unix::fs::symlink(tmp.path().join("not-created-yet"), root.join("escape")).unwrap();
        assert!(safe_write_path(Some("escape/loot.txt"), "d").is_err());
        assert!(safe_write_path(Some("escape"), "d").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_terminates_and_stays_inside() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        std::os::unix::fs::symlink("b", root.join("a")).unwrap();
        std::os::unix::fs::symlink("a", root.join("b")).unwrap();
        // Bounded expansion: this must return rather than spin, and must not land outside.
        assert!(safe_write_path(Some("a"), "d").unwrap().starts_with(&root));
    }

    #[test]
    fn a_miscased_escape_is_rejected_even_on_a_case_insensitive_filesystem() {
        // macOS's default filesystem folds case, so canonicalising `../FILES` would quietly
        // rewrite it back to the real `files` directory and let the escape through.
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, _root) = sandbox(&tmp);
        assert!(safe_write_path(Some("../FILES/loot.csv"), "d").is_err());
    }

    #[test]
    fn a_source_path_inside_the_sandbox_resolves_to_the_real_file() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        write(&root.join("in.txt"), "hi");
        assert_eq!(safe_read_path("in.txt").unwrap(), root.join("in.txt"));
    }

    #[test]
    fn an_absolute_source_path_is_rejected() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, _root) = sandbox(&tmp);
        let err = safe_read_path("/etc/hosts").unwrap_err();
        assert!(err.to_string().starts_with("source_path '/etc/hosts' must be inside"), "{err}");
    }

    #[test]
    fn a_source_path_climbing_out_with_dotdot_is_rejected() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, _root) = sandbox(&tmp);
        assert!(safe_read_path("../../etc/hosts").is_err());
    }

    #[test]
    fn a_missing_source_file_inside_the_sandbox_reports_not_found() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, _root) = sandbox(&tmp);
        let err = safe_read_path("nope.txt").unwrap_err();
        assert!(err.to_string().starts_with("source_path not found in files dir: "), "{err}");
    }

    #[test]
    fn the_sweep_deletes_files_past_the_ttl_and_keeps_fresh_ones() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        let (old, fresh) = (root.join("old.csv"), root.join("fresh.csv"));
        write(&old, "x");
        write(&fresh, "y");
        age(&old, 48.0);
        sweep_expired(Some(24.0));
        assert!(!old.exists() && fresh.exists());
    }

    #[test]
    fn the_sweep_reaches_files_in_subdirectories() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, _root) = sandbox(&tmp);
        let nested = safe_write_path(Some("a/b/old.csv"), "d").unwrap();
        write(&nested, "x");
        age(&nested, 48.0);
        sweep_expired(Some(24.0));
        assert!(!nested.exists());
        assert!(nested.parent().unwrap().is_dir()); // only files are unlinked, never the dirs
    }

    #[test]
    fn a_ttl_of_zero_disables_the_sweep() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        let old = root.join("old.csv");
        write(&old, "x");
        age(&old, 100.0);
        sweep_expired(Some(0.0));
        assert!(old.exists());
    }

    #[test]
    fn an_unparseable_ttl_env_value_falls_back_to_the_default() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        let _ttl = Env::new(&[("GDRIVE_MCP_FILES_TTL_HOURS", Some("forever"))]);
        let fresh = root.join("fresh.csv");
        write(&fresh, "x");
        sweep_expired(None); // must fall back to 24h, not fail
        assert!(fresh.exists());
        assert_eq!(env_ttl_hours(), 24.0);
    }

    #[test]
    fn an_unusable_files_dir_never_fails_the_sweep() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let afile = tmp.path().join("afile");
        write(&afile, "x");
        let _e = point_files_dir_at(&afile.join("under-a-file"));
        sweep_expired(Some(1.0)); // files_root() mkdir fails -> swallowed, no crash on boot
    }

    #[test]
    fn a_path_that_exists_but_is_not_a_directory_is_an_error_for_callers() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let afile = tmp.path().join("afile");
        write(&afile, "x");
        let _e = point_files_dir_at(&afile);
        assert!(files_root().is_err());
    }

    #[test]
    fn a_sandbox_the_server_created_carries_the_ownership_marker() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        assert!(root.join(MARKER_NAME).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_sandbox_the_server_created_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn the_sweep_refuses_an_unmarked_preexisting_directory() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // Simulates GDRIVE_MCP_FILES_DIR aimed at a real, existing directory.
        let pre = tmp.path().join("downloads");
        fs::create_dir(&pre).unwrap();
        let keep = pre.join("thesis-draft.txt");
        write(&keep, "precious");
        age(&keep, 24.0 * 365.0);
        let _e = point_files_dir_at(&pre);
        sweep_expired(Some(1.0));
        assert!(keep.exists());
        assert!(!pre.join(MARKER_NAME).exists()); // refusing must not itself opt the dir in
    }

    #[test]
    fn the_refusal_notice_names_the_directory_and_the_touch_that_opts_it_in() {
        let notice = refusal_notice(Path::new("/data/downloads"));
        assert!(notice.starts_with("gdrive-mcp: not sweeping /data/downloads: "), "{notice}");
        assert!(notice.ends_with("run: touch '/data/downloads/.gdrive-mcp-sandbox'"), "{notice}");
    }

    #[test]
    fn writing_into_a_preexisting_directory_does_not_opt_it_into_sweeping() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let pre = tmp.path().join("downloads");
        fs::create_dir(&pre).unwrap();
        let _e = point_files_dir_at(&pre);
        let p = safe_write_path(Some("spill.csv"), "d").unwrap(); // containment still applies
        assert_eq!(p, pre.canonicalize().unwrap().join("spill.csv"));
        assert!(!pre.join(MARKER_NAME).exists());
    }

    #[test]
    fn an_operator_placed_marker_opts_an_existing_directory_into_the_sweep() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let pre = tmp.path().join("chosen");
        fs::create_dir(&pre).unwrap();
        write(&pre.join(MARKER_NAME), "");
        let old = pre.join("old.csv");
        write(&old, "x");
        age(&old, 48.0);
        let _e = point_files_dir_at(&pre);
        sweep_expired(Some(24.0));
        assert!(!old.exists());
    }

    #[test]
    fn the_sweep_spares_the_marker_itself() {
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (_e, root) = sandbox(&tmp);
        let marker = root.join(MARKER_NAME);
        age(&marker, 24.0 * 30.0);
        let old = root.join("old.csv");
        write(&old, "x");
        age(&old, 24.0 * 30.0);
        sweep_expired(Some(24.0));
        assert!(marker.exists() && !old.exists());
    }

    #[test]
    fn the_default_location_is_owned_even_when_it_already_exists() {
        // An existing install's default dir (created before markers existed) keeps getting swept.
        let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let default_root = tmp.path().join("gdrive-mcp").join("files");
        fs::create_dir_all(&default_root).unwrap();
        let old = default_root.join("old.csv");
        write(&old, "x");
        age(&old, 48.0);
        {
            // XDG_CONFIG_HOME is read by other modules' tests too, so hold it only over the call.
            let _e = Env::new(&[
                ("GDRIVE_MCP_FILES_DIR", None),
                ("GDRIVE_MCP_FILES_TTL_HOURS", None),
                ("XDG_CONFIG_HOME", Some(&tmp.path().to_string_lossy())),
            ]);
            sweep_expired(Some(24.0));
        }
        assert!(!old.exists());
        assert!(default_root.join(MARKER_NAME).is_file());
    }
}
