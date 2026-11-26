use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::warn;

use super::snapshot::{
    parse_history_snapshot, parse_snapshot, snapshot_file_version, SessionHistorySnapshot,
    SessionSnapshot, SNAPSHOT_VERSION,
};

pub(super) fn session_path() -> PathBuf {
    crate::session::data_dir().join("session.json")
}

fn session_history_path() -> PathBuf {
    crate::session::data_dir().join("session-history.json")
}

// Follow symlinks manually so a write through a (possibly dangling) symlink
// lands on the target. `fs::canonicalize` requires the target to exist, which
// excludes the dangling-symlink case stow users hit on the very first save.
fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(_) => return Ok(current),
        };
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let link = std::fs::read_link(&current)?;
        current = if link.is_absolute() {
            link
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(link)
        };
    }
    Ok(current)
}

pub(super) fn save_to_path(path: &Path, snapshot: &SessionSnapshot) -> std::io::Result<()> {
    save_json_to_path(path, snapshot)
}

fn save_json_to_path<T: serde::Serialize>(path: &Path, snapshot: &T) -> std::io::Result<()> {
    let target = resolve_write_target(path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(snapshot)?;
    let (tmp_path, mut temporary) = private_temporary(&target)?;
    // A rename can survive a hard crash before unflushed file contents reach disk.
    let result = temporary
        .write_all(json.as_bytes())
        .and_then(|()| temporary.sync_all());
    drop(temporary);
    if let Err(err) = result.and_then(|()| crate::platform::replace_file(&tmp_path, &target)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    crate::platform::sync_parent_directory(target.parent().unwrap_or_else(|| Path::new(".")))
}

fn private_temporary(target: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = target.with_file_name(format!(".herdr-session-{}-{n}.tmp", std::process::id()));
        match crate::platform::create_config_temporary(&path, true) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate session staging file",
    ))
}

pub(super) fn save_history_to_path(
    path: &Path,
    history: Option<&SessionHistorySnapshot>,
) -> std::io::Result<()> {
    match history {
        Some(history) => save_json_to_path(path, history),
        None => clear_path(path),
    }
}

pub(super) fn clear_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn clear_history() {
    let path = session_history_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
    }
}

pub fn load_for_restore() -> Option<SessionSnapshot> {
    load_from_path(&session_path(), true)
}

#[cfg(test)]
pub fn load() -> Option<SessionSnapshot> {
    load_from_path(&session_path(), false)
}

fn load_from_path(path: &Path, consume: bool) -> Option<SessionSnapshot> {
    let (content, private) = match std::fs::File::open(path).and_then(|mut file| {
        let private = crate::platform::program_resume::private_snapshot(&file);
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        Ok((content, private))
    }) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                event = "persist.restore", subsystem = "persist", outcome = "missing",
                path = %path.display(), "session file is missing"
            );
            return None;
        }
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), err = %err, "failed to read session file"
            );
            return None;
        }
    };
    match parse_snapshot(&content) {
        Ok(mut snapshot) => {
            if !private {
                snapshot.discard_program_resumes();
            }
            if consume && snapshot.has_program_resumes() {
                // Once a program starts, a crash must not replay its old command.
                // Commit the removal before making any launch intent available.
                if let Err(err) = consume_program_resumes(path, &content) {
                    warn!(%err, path = %path.display(), "could not consume program resume records");
                    snapshot.discard_program_resumes();
                }
            }
            Some(snapshot)
        }
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        event = "persist.restore", subsystem = "persist", outcome = "unsupported_version",
                        path = %path.display(), file_version = version, supported = SNAPSHOT_VERSION,
                        "session file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), err = %err, "failed to parse session file, ignoring"
            );
            None
        }
    }
}

fn consume_program_resumes(path: &Path, content: &str) -> std::io::Result<()> {
    let mut value = serde_json::from_str(content)?;
    if !super::snapshot::strip_program_resumes(&mut value) {
        return Err(std::io::Error::other(
            "could not locate saved program resumes",
        ));
    }
    save_json_to_path(path, &value)
}

pub fn load_history() -> Option<SessionHistorySnapshot> {
    let path = session_history_path();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            warn!(err = %err, "failed to read session history file");
            return None;
        }
    };
    match parse_history_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        file_version = version,
                        supported = SNAPSHOT_VERSION,
                        "session history file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(err = %err, "failed to parse session history file, ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::snapshot::{
        PaneHistorySnapshot, TabHistorySnapshot, WorkspaceHistorySnapshot,
    };

    fn temp_session_path(name: &str) -> PathBuf {
        let unique = format!(
            "herdr-session-tests-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("session.json")
    }

    fn temp_session_paths(name: &str) -> (PathBuf, PathBuf) {
        let session = temp_session_path(name);
        let history = session.with_file_name("session-history.json");
        (session, history)
    }

    fn empty_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: vec![],
            active: None,
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            sidebar_health_section_split: None,
            collapsed_space_keys: std::collections::HashSet::new(),
        }
    }

    fn history_snapshot(secret: &str) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: None,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![TabHistorySnapshot {
                    panes: std::collections::HashMap::from([(
                        0,
                        PaneHistorySnapshot {
                            ansi: secret.to_string(),
                            lines: 1,
                        },
                    )]),
                }],
            }],
        }
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("split-secret"))).unwrap();

        let session = std::fs::read_to_string(&session_path).unwrap();
        let history = std::fs::read_to_string(&history_path).unwrap();
        assert!(!session.contains("split-secret"));
        assert!(!session.contains("history"));
        assert!(history.contains("split-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn only_private_session_files_restore_program_records() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let path = temp_session_path("private-programs");
        let mut snapshot = parse_snapshot(include_str!(
            "../../tests/fixtures/session/current-herdr-session.json"
        ))
        .unwrap();
        let pane = snapshot.workspaces[0].tabs[0].panes.get_mut(&1).unwrap();
        pane.program_resume = Some(
            serde_json::from_value(serde_json::json!({
                "uid": 1001, "cwd": "/tmp", "argv": ["editor", "literal argument"],
            }))
            .unwrap(),
        );
        save_to_path(&path, &snapshot).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o077, 0);
        let restored = load_from_path(&path, false).unwrap();
        assert!(restored.workspaces[0].tabs[0].panes[&1]
            .program_resume
            .is_some());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let shared = load_from_path(&path, false).unwrap();
        assert_eq!(shared.workspaces.len(), restored.workspaces.len());
        assert!(shared.workspaces[0].tabs[0].panes[&1]
            .program_resume
            .is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cold_restore_consumes_records_durably_before_launch() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_session_path("consume-programs");
        let mut original: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/session/current-herdr-session.json"
        ))
        .unwrap();
        let pane = &mut original["workspaces"][0]["tabs"][0]["panes"]["1"];
        pane["program_resume"] = serde_json::json!({"uid": 42, "cwd": "/tmp", "argv": ["editor"]});
        pane["future_metadata"] = serde_json::json!(true);
        save_json_to_path(&path, &original).unwrap();

        let dir = path.parent().unwrap();
        if crate::platform::resume_user_uid().is_some() {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            let failed = load_from_path(&path, true).unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(!failed.has_program_resumes());
        }
        let first = load_from_path(&path, true).unwrap();
        assert!(first.has_program_resumes());
        let next = load_from_path(&path, true).unwrap();
        assert!(!next.has_program_resumes());
        assert_eq!(
            super::super::snapshot::layout_fingerprint(&first),
            super::super::snapshot::layout_fingerprint(&next)
        );
        let mut expected = original;
        expected["workspaces"][0]["tabs"][0]["panes"]["1"]
            .as_object_mut()
            .unwrap()
            .remove("program_resume");
        let actual: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(expected, actual);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn failed_replacement_preserves_saved_files_and_retries() {
        use std::os::windows::fs::OpenOptionsExt;

        let (session_path, history_path) = temp_session_paths("failed-replacement");
        let has_staging_file = || {
            std::fs::read_dir(session_path.parent().unwrap())
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".herdr-session-")
                })
        };
        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("old history"))).unwrap();

        for path in [&session_path, &history_path] {
            let original = std::fs::read(path).unwrap();
            let locked = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
                .open(path)
                .unwrap();
            let mut changed = empty_snapshot();
            changed.selected = 7;
            let save = || {
                if path == &session_path {
                    save_to_path(path, &changed)
                } else {
                    save_history_to_path(path, Some(&history_snapshot("new history")))
                }
            };
            assert!(save().is_err());
            assert_eq!(std::fs::read(path).unwrap(), original);
            assert!(!has_staging_file());

            drop(locked);
            save().unwrap();
            assert!(!has_staging_file());
        }

        assert_eq!(
            parse_snapshot(&std::fs::read_to_string(&session_path).unwrap())
                .unwrap()
                .selected,
            7
        );
        let history = std::fs::read_to_string(&history_path).unwrap();
        assert!(history.contains("new history"));
        assert!(!history.contains("old history"));
        std::fs::remove_dir_all(session_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn save_to_paths_removes_stale_history_when_history_is_disabled() {
        let (session_path, history_path) = temp_session_paths("clear-history");
        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("stale-secret"))).unwrap();

        save_history_to_path(&history_path, None).unwrap();

        assert!(session_path.exists());
        assert!(!history_path.exists());
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot()).unwrap();

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn clear_path_ignores_missing_session_file() {
        let path = temp_session_path("clear-missing");

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_preserves_existing_symlink() {
        let target = temp_session_path("symlink-target");
        let link = target.with_file_name("link.json");
        save_to_path(&target, &empty_snapshot()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut snap = empty_snapshot();
        snap.selected = 7;
        save_to_path(&link, &snap).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let parsed = parse_snapshot(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(parsed.selected, 7);
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_writes_through_dangling_symlink() {
        let target = temp_session_path("dangling-target");
        let link = target.with_file_name("link.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_resolves_relative_symlink() {
        let session = temp_session_path("relative-symlink");
        let dir = session.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let target = dir.join("real.json");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }
}
