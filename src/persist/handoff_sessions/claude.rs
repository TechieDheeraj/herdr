//! Claude closes idle transcripts. Its local PID registry can still identify
//! a live conversation without inspecting prompts or choosing by recency/cwd.
use super::{owned_regular_file, uuid};
use crate::platform;
use std::cell::OnceCell;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

const RECORD_LIMIT: u64 = 64 * 1024;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionRecord {
    pid: u32,
    session_id: String,
    proc_start: String,
    pid_domain: String,
    kind: String,
    entrypoint: String,
}

pub(super) struct ClaudeStore {
    root: PathBuf,
    projects: PathBuf,
    project_dirs: OnceCell<Option<Vec<PathBuf>>>,
    pid_domain: OnceCell<Option<String>>,
}

impl ClaudeStore {
    pub(super) fn new(root: &Path) -> Option<Self> {
        let root = root.canonicalize().ok()?;
        let projects = root.join("projects").canonicalize().ok()?;
        if !owned_directory(&root) || !owned_directory(&projects) {
            return None;
        }
        Some(Self {
            root,
            projects,
            project_dirs: OnceCell::new(),
            pid_domain: OnceCell::new(),
        })
    }

    /// Read only the verified foreground owner's record, never the peer keys.
    pub(super) fn record_id(&self, pid: u32, birth: u64, deadline: Instant) -> Option<String> {
        if Instant::now() >= deadline || !owned_directory(&self.root.join("sessions")) {
            return None;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.root.join("sessions").join(format!("{pid}.json")))
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file()
            || metadata.uid() != platform::agent_session_user_uid()?
            || metadata.mode() & 0o022 != 0
            || !platform::file_has_no_extended_acl(file.as_raw_fd())
            || metadata.len() > RECORD_LIMIT
            || metadata.modified().ok()? < platform::process_started_at(birth)?
        {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > RECORD_LIMIT {
            return None;
        }
        let record: SessionRecord = serde_json::from_slice(&bytes).ok()?;
        let domain = self
            .pid_domain
            .get_or_init(platform::claude_pid_domain)
            .as_ref()?;
        if record.pid != pid
            || record.kind != "interactive"
            || record.entrypoint != "cli"
            || &record.pid_domain != domain
            || record.proc_start != platform::process_start_label(pid, deadline)?
            || platform::process_start_token(pid) != Some(birth)
        {
            return None;
        }
        uuid(&record.session_id)
    }

    pub(super) fn transcript_id(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.projects).ok()?;
        if relative.components().count() != 2 || !owned_directory(path.parent()?) {
            return None;
        }
        let id = uuid(relative.file_name()?.to_str()?.strip_suffix(".jsonl")?)?;
        owned_regular_file(path).then_some(id)
    }

    pub(super) fn has_transcript(&self, id: &str, deadline: Instant) -> bool {
        let directories = self.project_dirs.get_or_init(|| {
            let mut paths = Vec::new();
            for entry in fs::read_dir(&self.projects).ok()? {
                if Instant::now() >= deadline {
                    return None;
                }
                let entry = entry.ok()?;
                if entry.file_type().ok()?.is_dir() && owned_directory(&entry.path()) {
                    paths.push(entry.path());
                }
            }
            Some(paths)
        });
        let Some(directories) = directories else {
            return false;
        };
        let mut found = false;
        for directory in directories {
            if Instant::now() >= deadline {
                return false;
            }
            // Probe just this UUID; transcript contents and cwd naming rules
            // are irrelevant. Duplicate copies are ambiguous for native resume.
            if self
                .transcript_id(&directory.join(format!("{id}.jsonl")))
                .is_some()
            {
                if found {
                    return false;
                }
                found = true;
            }
        }
        found
    }
}

fn owned_directory(path: &Path) -> bool {
    let Ok(directory) = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_DIRECTORY)
        .open(path)
    else {
        return false;
    };
    directory.metadata().is_ok_and(|info| {
        info.is_dir()
            && Some(info.uid()) == platform::agent_session_user_uid()
            && info.mode() & 0o022 == 0
            && platform::file_has_no_extended_acl(directory.as_raw_fd())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    const ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

    struct Fixture {
        root: PathBuf,
        pid: u32,
        birth: u64,
        record: serde_json::Value,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let pid = std::process::id();
            let root = std::env::temp_dir().join(format!(
                "herdr-claude-record-{pid}-{}",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("sessions")).unwrap();
            fs::create_dir_all(root.join("projects/project")).unwrap();
            let birth = platform::process_start_token(pid).unwrap();
            let record = serde_json::json!({
                "pid": pid, "sessionId": ID,
                "procStart": platform::process_start_label(pid, deadline()).unwrap(),
                "pidDomain": platform::claude_pid_domain().unwrap(),
                "kind": "interactive", "entrypoint": "cli",
                "name": "ignored display metadata"
            });
            let fixture = Self {
                root,
                pid,
                birth,
                record,
            };
            fixture.write_record(&fixture.record);
            fs::write(
                fixture.root.join(format!("projects/project/{ID}.jsonl")),
                "",
            )
            .unwrap();
            fixture
        }

        fn path(&self) -> PathBuf {
            self.root
                .join("sessions")
                .join(format!("{}.json", self.pid))
        }

        fn write_record(&self, record: &serde_json::Value) {
            fs::write(self.path(), serde_json::to_vec(record).unwrap()).unwrap();
            fs::set_permissions(self.path(), fs::Permissions::from_mode(0o600)).unwrap();
        }

        fn id(&self) -> Option<String> {
            ClaudeStore::new(&self.root)
                .unwrap()
                .record_id(self.pid, self.birth, deadline())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    #[test]
    fn closed_transcript_is_found_from_current_process_record() {
        let fixture = Fixture::new();
        assert_eq!(fixture.id().as_deref(), Some(ID));
        let store = ClaudeStore::new(&fixture.root).unwrap();
        assert!(store.has_transcript(ID, deadline()));
        assert!(!store.has_transcript(ID, Instant::now()));
        assert!(store
            .record_id(fixture.pid, fixture.birth + 1, deadline())
            .is_none());
        assert!(store
            .record_id(fixture.pid, fixture.birth, Instant::now())
            .is_none());
    }

    #[test]
    fn stale_foreign_and_malformed_records_are_rejected() {
        let fixture = Fixture::new();
        for (field, value) in [
            ("pid", serde_json::json!(fixture.pid + 1)),
            ("procStart", serde_json::json!("Mon Jan  1 00:00:00 2001")),
            ("pidDomain", serde_json::json!("foreign-host")),
            ("sessionId", serde_json::json!("../not-a-session")),
            ("procStart", serde_json::Value::Null),
            ("kind", serde_json::json!("daemon")),
            ("entrypoint", serde_json::json!("sdk")),
        ] {
            let mut record = fixture.record.clone();
            record[field] = value;
            fixture.write_record(&record);
            assert_eq!(fixture.id(), None, "accepted invalid {field}");
        }
        fixture.write_record(&fixture.record);
        fs::File::open(fixture.path())
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        assert_eq!(
            fixture.id(),
            None,
            "accepted record from before this process"
        );
        fs::write(fixture.path(), "not JSON").unwrap();
        assert_eq!(fixture.id(), None);
        fs::write(fixture.path(), vec![b' '; RECORD_LIMIT as usize + 1]).unwrap();
        assert_eq!(fixture.id(), None);
    }

    #[test]
    fn untrusted_record_file_types_and_permissions_are_rejected() {
        let fixture = Fixture::new();
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(fixture.id(), None);
        let target = fixture.root.join("record-copy");
        fs::rename(fixture.path(), &target).unwrap();
        symlink(&target, fixture.path()).unwrap();
        assert_eq!(fixture.id(), None);
        fs::remove_file(fixture.path()).unwrap();
        let path = std::ffi::CString::new(fixture.path().as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert_eq!(fixture.id(), None, "FIFO must not block the reader");
        fs::remove_file(fixture.path()).unwrap();
        fixture.write_record(&fixture.record);
        let sessions = fixture.root.join("sessions");
        fs::rename(&sessions, fixture.root.join("sessions-copy")).unwrap();
        symlink(fixture.root.join("sessions-copy"), &sessions).unwrap();
        assert_eq!(fixture.id(), None);
    }

    #[test]
    fn transcript_lookup_rejects_missing_symlinked_and_duplicate_files() {
        let fixture = Fixture::new();
        let exists = || {
            ClaudeStore::new(&fixture.root)
                .unwrap()
                .has_transcript(ID, deadline())
        };
        let transcript = fixture.root.join(format!("projects/project/{ID}.jsonl"));
        fs::remove_file(&transcript).unwrap();
        assert!(!exists());
        symlink(fixture.path(), &transcript).unwrap();
        assert!(!exists());
        fs::remove_file(&transcript).unwrap();
        fs::write(&transcript, "").unwrap();
        let duplicate = fixture.root.join("projects/duplicate");
        fs::create_dir(&duplicate).unwrap();
        fs::write(duplicate.join(format!("{ID}.jsonl")), "").unwrap();
        assert!(!exists());
        fs::remove_dir_all(&duplicate).unwrap();
        fs::rename(
            fixture.root.join("projects/project"),
            fixture.root.join("outside"),
        )
        .unwrap();
        symlink(
            fixture.root.join("outside"),
            fixture.root.join("projects/project"),
        )
        .unwrap();
        assert!(!exists());
    }
}
