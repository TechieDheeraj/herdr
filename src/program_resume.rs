//! Server-owned OSC 7577 records. Only the authenticated handoff between
//! servers that own the same live PTY carries the verifier and replay state.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose, Engine as _};
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const TOKEN_ENV: &str = "OSC7577_TOKEN";
const CLOCK_WINDOW: u64 = 60;
const MAX_ARGUMENTS: usize = 64;
// The PTY OSC collector discards longer, unverifiable sequences.
const MAX_BODY: usize = 4096;
const MAX_RECENT_OWNERS: usize = 128;
static ALLOWLIST: RwLock<Vec<String>> = RwLock::new(Vec::new());

pub(crate) fn set_allowlist(programs: &[String]) {
    if let Ok(mut list) = ALLOWLIST.write() {
        programs.clone_into(&mut list);
    }
}

fn allowed(program: &str) -> bool {
    ALLOWLIST
        .read()
        .is_ok_and(|list| list.iter().any(|name| name == program))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeRecord {
    uid: u32,
    pub(crate) argv: Vec<String>,
    cwd: PathBuf,
}

// Arguments can contain secrets. Snapshots and handoff manifests derive Debug.
impl std::fmt::Debug for ResumeRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeRecord").finish_non_exhaustive()
    }
}

impl ResumeRecord {
    pub(crate) fn restore_cwd(&self) -> Option<PathBuf> {
        (crate::platform::resume_user_uid() == Some(self.uid)
            && valid_argv(&self.argv)
            && allowed(&self.argv[0])
            && self.cwd.is_absolute()
            && self.cwd.is_dir())
        .then(|| self.cwd.clone())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct OwnedRecord {
    pid: u32,
    shell_pid: u32,
    start: u64,
    record: ResumeRecord,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ProgramResume {
    token: String,
    active: Option<OwnedRecord>,
    // Keep timestamps after clear and owner replacement through the replay window.
    last_time: HashMap<u32, u64>,
    #[serde(skip)]
    missing_since: Option<Instant>,
}

impl std::fmt::Debug for ProgramResume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramResume").finish_non_exhaustive()
    }
}

impl ProgramResume {
    pub(crate) fn for_command(cmd: &mut CommandBuilder) -> Option<Self> {
        cmd.env_remove(TOKEN_ENV);
        crate::platform::resume_user_uid()?;
        if ALLOWLIST.read().ok()?.is_empty() {
            return None;
        }
        let mut random = [0; 32];
        getrandom::fill(&mut random).ok()?;
        let token = general_purpose::URL_SAFE_NO_PAD.encode(random);
        cmd.env(TOKEN_ENV, &token);
        Some(Self {
            token,
            active: None,
            last_time: HashMap::new(),
            missing_since: None,
        })
    }

    pub(crate) fn observe(&mut self, shell_pid: u32, body: &[u8]) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        self.apply(body, now.as_secs(), |pid, report| {
            let uid = crate::platform::resume_user_uid()?;
            if crate::platform::resume_owner_uid(pid) != Some(uid) || !allowed(&report.argv[0]) {
                return None;
            }
            let start = crate::platform::process_start_token(pid)?;
            let cwd = report.cwd.or_else(|| crate::platform::process_cwd(pid))?;
            crate::platform::live_pane_process_group(shell_pid, pid, start)?;
            cwd.is_absolute().then_some(OwnedRecord {
                pid,
                shell_pid,
                start,
                record: ResumeRecord {
                    uid,
                    argv: report.argv,
                    cwd,
                },
            })
        })
    }

    fn apply(
        &mut self,
        body: &[u8],
        now: u64,
        owner: impl FnOnce(u32, CommandReport) -> Option<OwnedRecord>,
    ) -> bool {
        let Some(fields) = authenticated_fields(body, &self.token) else {
            return false;
        };
        let Some(time) = report_number(fields, b"time=") else {
            return false;
        };
        let Some(pid) = report_number(fields, b"pid=").and_then(|n| u32::try_from(n).ok()) else {
            return false;
        };
        if now.abs_diff(time) > CLOCK_WINDOW
            || self.last_time.get(&pid).is_some_and(|last| time < *last)
        {
            return false;
        }
        self.last_time
            .retain(|_, t| t.saturating_add(CLOCK_WINDOW) >= now);
        let (replacement, clear) = match parse_report(fields) {
            Ok(Some(command)) => (owner(pid, command), false),
            Ok(None) => (None, true),
            Err(()) => (None, false),
        };
        if replacement.is_some() || clear {
            // A fresh owner can be refused under pressure without evicting a
            // different owner's replay protection.
            if self.last_time.len() >= MAX_RECENT_OWNERS && !self.last_time.contains_key(&pid) {
                return self.remove_owner(pid);
            }
            self.last_time.insert(pid, time);
        }
        if let Some(next) = replacement {
            let changed = self.active.as_ref().is_none_or(|old| {
                old.pid != next.pid || old.start != next.start || old.record != next.record
            });
            self.active = Some(next);
            self.missing_since = None;
            return changed;
        }
        self.remove_owner(pid)
    }

    fn remove_owner(&mut self, pid: u32) -> bool {
        if self.active.as_ref().is_some_and(|r| r.pid == pid) {
            self.active = None;
            self.missing_since = None;
            return true;
        }
        false
    }

    pub(crate) fn record(&self) -> Option<ResumeRecord> {
        self.active.as_ref().map(|r| r.record.clone())
    }

    pub(crate) fn has_record(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn expire_if_dead(&mut self) -> bool {
        let Some(r) = &self.active else { return false };
        if crate::platform::live_pane_process_group(r.shell_pid, r.pid, r.start).is_some() {
            self.missing_since = None;
            return false;
        }
        if self
            .missing_since
            .get_or_insert_with(Instant::now)
            .elapsed()
            < Duration::from_secs(2)
        {
            return false;
        }
        self.active = None;
        self.missing_since = None;
        true
    }
}

// An invalid or ambiguous owner/time cannot authorize removing a record.
// Identical duplicates still reach strict parsing and are rejected there.
fn report_number(fields: &[u8], key: &[u8]) -> Option<u64> {
    let mut values = fields
        .split(|&b| b == b':')
        .filter_map(|p| p.strip_prefix(key));
    let first = values.next()?;
    if first.is_empty()
        || first[0] == b'0'
        || !first.iter().all(u8::is_ascii_digit)
        || !values.all(|value| value == first)
    {
        return None;
    }
    std::str::from_utf8(first).ok()?.parse().ok()
}

fn authenticated_fields<'a>(body: &'a [u8], token: &str) -> Option<&'a [u8]> {
    if body.len() > MAX_BODY {
        return None;
    }
    let (fields, trailer) = body.split_at(body.len().checked_sub(69)?);
    let mac = trailer.strip_prefix(b":mac=")?;
    let digest = Sha256::new()
        .chain_update(fields)
        .chain_update(b":")
        .chain_update(token)
        .finalize();
    let expected = format!("{digest:x}");
    (expected.as_bytes() == mac).then_some(fields)
}

struct CommandReport {
    argv: Vec<String>,
    cwd: Option<PathBuf>,
}

fn decode_text(value: &str) -> Option<String> {
    let codec = if value.ends_with('=') {
        general_purpose::STANDARD
    } else {
        general_purpose::STANDARD_NO_PAD
    };
    let text = String::from_utf8(codec.decode(value).ok()?).ok()?;
    (!text.chars().any(char::is_control)).then_some(text)
}

fn valid_argv(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    !program.is_empty()
        && (Path::new(program).is_absolute()
            || (!program.starts_with('-')
                && !program.contains('/')
                && program != "."
                && program != ".."))
        && argv.len() <= MAX_ARGUMENTS
        && argv.iter().map(String::len).sum::<usize>() <= MAX_BODY
        && !argv.iter().any(|s| s.chars().any(char::is_control))
}

fn parse_report(fields: &[u8]) -> Result<Option<CommandReport>, ()> {
    let fields = std::str::from_utf8(fields).map_err(|_| ())?;
    let mut seen = HashSet::new();
    let mut argv = None;
    let mut cwd = None;
    let mut clear = false;
    for pair in fields.split(':') {
        let (key, value) = pair.split_once('=').ok_or(())?;
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_lowercase())
            || !seen.insert(key)
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b))
        {
            return Err(());
        }
        match key {
            "pid" | "time" => {} // The authentication pass already validated both integers.
            "mac" => return Err(()), // the final MAC has already been removed
            "argv" => {
                let args: Vec<_> = value
                    .split(',')
                    .take(MAX_ARGUMENTS + 1)
                    .map(decode_text)
                    .collect::<Option<_>>()
                    .ok_or(())?;
                if !valid_argv(&args) {
                    return Err(());
                }
                argv = Some(args);
            }
            "cwd" => {
                let path = PathBuf::from(decode_text(value).ok_or(())?);
                if !path.is_absolute() {
                    return Err(());
                }
                cwd = Some(path);
            }
            "clear" if value == "1" => clear = true,
            "clear" => return Err(()),
            "need"
                if value.split(',').all(|name| {
                    matches!(
                        name,
                        "pid" | "time" | "mac" | "argv" | "cwd" | "clear" | "need"
                    )
                }) => {}
            "need" => return Err(()),
            _ => {}
        }
    }
    match (argv, clear) {
        (Some(argv), false) => Ok(Some(CommandReport { argv, cwd })),
        (None, true) => Ok(None),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed(fields: &str) -> Vec<u8> {
        let mac = Sha256::digest(format!("{fields}:unit-test-token").as_bytes());
        format!("{fields}:mac={mac:x}").into_bytes()
    }

    fn tracker() -> ProgramResume {
        ProgramResume {
            token: "unit-test-token".into(),
            active: None,
            last_time: HashMap::new(),
            missing_since: None,
        }
    }

    fn accept(pid: u32, report: CommandReport) -> Option<OwnedRecord> {
        Some(OwnedRecord {
            pid,
            shell_pid: 1,
            start: 10,
            record: ResumeRecord {
                uid: 1000,
                argv: report.argv,
                cwd: report.cwd.unwrap_or_else(|| "/work".into()),
            },
        })
    }

    #[test]
    fn authenticates_the_exact_bytes_before_interpreting_the_report() {
        // sha256("argv=ZWRpdA==:pid=3:time=500:unit-test-token")
        let report = b"argv=ZWRpdA==:pid=3:time=500:mac=6dbadedd64431c6f861713ad1b9221ac030849ded964f7539ffaff9bcd2e586d";
        let mut tracker = tracker();
        assert!(tracker.apply(report, 500, accept));
        assert!(!tracker.apply(&signed("pid=3:time=439:clear=1"), 500, accept));
        assert!(!tracker.apply(&signed("pid=3:time=561:clear=1"), 500, accept));
        assert!(!tracker.apply(
            &signed("pid=3:time=18446744073709551616:clear=1"),
            500,
            accept
        ));
        assert!(!tracker.apply(&signed("pid=3:clear=1"), 500, accept));
        for ambiguous in [
            "time=0:time=500:pid=3",
            "time=500:time=499:pid=3",
            "time=500:pid=3:pid=4",
        ] {
            assert!(!tracker.apply(&signed(&format!("{ambiguous}:clear=1")), 500, accept));
        }
        let mut wrong = signed("pid=3:time=500:clear=1");
        wrong[0] = b'P';
        assert!(!tracker.apply(&wrong, 500, accept));
        let mut other_pane = tracker.clone();
        other_pane.token = "a-different-token".into();
        assert!(!other_pane.apply(&signed("pid=3:time=500:clear=1"), 500, accept));
        assert_eq!(tracker.record().unwrap().argv, ["edit"]);
        assert!(other_pane.has_record());
        assert!(!format!("{tracker:?}").contains("unit-test-token"));
    }

    #[test]
    fn authentic_rejections_and_clear_apply_only_to_their_owner() {
        let register = "argv=ZWRpdA==:time=500:pid=3";
        for invalid in [
            "argv=",
            "argv=LQ==",
            "argv=Li9lZGl0",
            "argv=ZWRpdA=,YQ==",
            "argv=ZWRpdA,AA==",
            "argv=ZWRpdA,/w==",
            "argv=ZWRpdA:wow",
            "argv=ZWRpdA:wow=!",
            "argv=ZWRpdA:need=env",
            "argv=ZWRpdA:cwd=cmVs",
            "argv=ZWRpdA:argv=ZWRpdA",
            "argv=ZWRpdA:clear=1",
            "clear=2",
            "mac=bad:clear=1",
            "clear=1:time=501",
        ] {
            let mut tracker = tracker();
            tracker.apply(&signed(register), 500, accept);
            assert!(!tracker.apply(&signed(&format!("pid=4:time=501:{invalid}")), 500, accept));
            assert!(tracker.has_record());
            assert!(
                tracker.apply(&signed(&format!("pid=3:time=501:{invalid}")), 500, accept),
                "{invalid}"
            );
            assert!(!tracker.has_record());
        }
    }

    #[test]
    fn replay_protection_survives_clears_and_replacement_by_another_owner() {
        let mut tracker = tracker();
        let report = |pid, time| signed(&format!("pid={pid}:time={time}:argv=ZWRpdA"));
        assert!(tracker.apply(&report(3, 500), 500, accept));
        assert!(tracker.apply(&report(4, 502), 500, accept));
        assert!(!tracker.apply(&report(3, 499), 500, accept));
        assert!(!tracker.apply(&signed("pid=3:time=503:clear=1"), 500, accept));
        assert!(tracker.apply(&signed("pid=4:time=505:clear=1"), 500, accept));
        assert!(!tracker.apply(&report(4, 504), 500, accept));
        assert!(!tracker.has_record());
    }

    #[test]
    fn preserves_literal_arguments_and_accepts_optional_base64_padding() {
        let text = ["/opt/tools/test app", "", "a'b", "a; b", "$HOME", "雪"];
        let encoded = text
            .iter()
            .map(|s| general_purpose::STANDARD_NO_PAD.encode(s))
            .collect::<Vec<_>>()
            .join(",");
        let mut tracker = tracker();
        let body = format!("extra=future:need=cwd,argv:time=500:pid=9:cwd=L3RtcA==:argv={encoded}");
        assert!(tracker.apply(&signed(&body), 500, accept));
        let record = tracker.record().unwrap();
        assert_eq!(record.argv, text);
        assert_eq!(record.cwd, Path::new("/tmp"));
    }

    #[test]
    fn handles_the_protocol_argument_limit_and_an_oversize_payload() {
        let mut tracker = tracker();
        let fields = format!("pid=9:time=500:argv=YQ{}", ",".repeat(63));
        assert!(tracker.apply(&signed(&fields), 500, accept));
        assert_eq!(tracker.record().unwrap().argv.len(), 64);
        assert!(!tracker.apply(
            &signed(&format!("{fields}:extra={}", "x".repeat(MAX_BODY))),
            500,
            accept
        ));
        assert!(tracker.apply(&signed(&format!("{fields},")), 500, accept));
        assert!(!tracker.has_record());
    }
}
