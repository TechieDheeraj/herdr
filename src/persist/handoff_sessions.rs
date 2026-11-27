//! Recover missing native session references on the receiving side of a Unix
//! handoff. The sender can predate this feature. Never send input to a PTY.
use super::snapshot::{PaneAgentSessionSnapshot, SessionSnapshot};
use crate::detect::{self, Agent};
use crate::{integration, platform};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

mod claude;

#[derive(Clone, Copy)]
struct PaneJob {
    shell: u32,
    group: u32,
}

struct Claim {
    job: PaneJob,
    pid: u32,
    birth: u64,
    agent: Agent,
    id: String,
}

impl Claim {
    fn still_foreground(&self) -> bool {
        platform::foreground_process_group_id(self.job.shell) == Some(self.job.group)
            && platform::live_pane_process_group(self.job.shell, self.pid, self.birth)
                == Some(self.job.group)
    }
}

/// Enrich the importer's in-memory snapshot; the caller saves only after commit.
pub(crate) fn recover(
    snapshot: &mut SessionSnapshot,
    imports: &HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
) -> bool {
    let mut targets = Vec::new();
    let mut claimed = HashSet::new();
    for tab in snapshot.workspaces.iter().flat_map(|ws| &ws.tabs) {
        for (&pane_id, pane) in &tab.panes {
            if let Some(saved) = &pane.agent_session {
                claimed.insert((saved.agent.clone(), saved.value.clone()));
            } else if pane.agent_resume.is_none() && pane.program_resume.is_none() {
                if let Some(runtime) = imports.get(&pane_id) {
                    if let Some(group) =
                        platform::foreground_process_group_id_for_tty_fd(runtime.master_fd)
                    {
                        targets.push((
                            pane_id,
                            PaneJob {
                                shell: runtime.state.child_pid,
                                group,
                            },
                        ));
                    }
                }
            }
        }
    }
    if targets.is_empty() {
        return false;
    }
    // Leave the handoff handshake ample time even if an agent home is on an
    // offline network mount. An abandoned worker only reads metadata.
    let budget = Duration::from_secs(3);
    let deadline = Instant::now() + budget;
    let (tx, rx) = std::sync::mpsc::channel();
    if crate::thread_spawn::spawn_named("herdr-handoff-sessions", move || {
        let stores = Stores::new(deadline);
        let mut found = HashMap::new();
        for (pane, job) in targets {
            if Instant::now() >= deadline {
                // An unvisited pane could claim the same conversation. Do not
                // publish a partial batch before checking all live owners.
                return;
            }
            if let Some(claim) = stores.identify(job) {
                found.insert(pane, claim);
            }
        }
        if Instant::now() >= deadline {
            return;
        }
        let _ = tx.send(found);
    })
    .is_err()
    {
        return false;
    }
    let Ok(mut found) = rx.recv_timeout(budget) else {
        tracing::info!("live session lookup did not finish before handoff");
        return false;
    };
    let mut counts = HashMap::new();
    for claim in found.values() {
        *counts
            .entry((detect::agent_label(claim.agent), claim.id.clone()))
            .or_insert(0usize) += 1;
    }
    let mut recovered = 0;
    for tab in snapshot.workspaces.iter_mut().flat_map(|ws| &mut ws.tabs) {
        for (pane_id, pane) in &mut tab.panes {
            let Some(claim) = found.remove(pane_id) else {
                continue;
            };
            let agent = detect::agent_label(claim.agent);
            if counts.get(&(agent, claim.id.clone())) != Some(&1)
                || claimed.contains(&(agent.to_owned(), claim.id.clone()))
                || !claim.still_foreground()
            {
                continue;
            }
            pane.agent_session = Some(PaneAgentSessionSnapshot {
                source: format!("herdr:{agent}"),
                agent: agent.into(),
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: claim.id,
            });
            recovered += 1;
            tracing::info!(
                pane = pane_id,
                agent,
                "recovered native session from live agent during handoff"
            );
        }
    }
    recovered > 0
}

struct Stores {
    claude: Option<claude::ClaudeStore>,
    codex: Option<PathBuf>,
    copilot: Option<PathBuf>,
    locks: OnceCell<Option<CopilotLocks>>,
    deadline: Instant,
}

type CopilotLocks = HashMap<u32, Vec<(String, SystemTime)>>;

impl Stores {
    fn new(deadline: Instant) -> Self {
        Self {
            claude: integration::claude_dir()
                .ok()
                .and_then(|p| claude::ClaudeStore::new(&p)),
            codex: integration::codex_dir()
                .ok()
                .and_then(|p| p.join("sessions").canonicalize().ok()),
            copilot: integration::copilot_dir()
                .ok()
                .and_then(|p| p.join("session-state").canonicalize().ok()),
            locks: OnceCell::new(),
            deadline,
        }
    }

    fn identify(&self, job: PaneJob) -> Option<Claim> {
        let leader_birth = platform::process_start_token(job.group)?;
        let leader = platform::foreground_group_leader_job(job.group)?;
        let process = leader.processes.first()?;
        let agent = detect::identify_agent(&detect::normalized_process_name(process))?;
        if !matches!(agent, Agent::Codex | Agent::Claude | Agent::GithubCopilot) {
            return None;
        }
        // An explicit pipe/exec invocation can belong to an interactive agent's tool call.
        let args = process.argv.as_deref()?;
        if args.iter().skip(1).any(|a| {
            matches!(
                a.as_str(),
                "exec" | "app-server" | "mcp-server" | "-p" | "--print" | "--prompt" | "--headless"
            )
        }) {
            return None;
        }
        let group = platform::foreground_job(job.shell)?;
        if group.process_group_id != job.group {
            return None;
        }
        let mut owners = vec![process.pid];
        // The npm launchers can wait for a native binary. Codex can also
        // run its rollout writer in its own app-server child.
        let wrapper = args
            .first()
            .and_then(|a| Path::new(a).file_name())
            .and_then(|n| n.to_str())
            .is_some_and(|n| matches!(n, "node" | "bun"));
        for child in &group.processes {
            if child.pid == process.pid {
                continue;
            }
            if let Some(kind) = detect::identify_agent(&detect::normalized_process_name(child)) {
                let child_args = child.argv.as_deref()?;
                // A JS agent can also start a second CLI as a tool. Accept a
                // launcher's native child only when it forwards its invocation.
                let forwarded = wrapper
                    && args
                        .get(2..)
                        .is_some_and(|cli_args| child_args.get(1..) == Some(cli_args));
                let app_server =
                    agent == Agent::Codex && child_args.get(1).is_some_and(|s| s == "app-server");
                if kind != agent
                    || !(forwarded || app_server)
                    || platform::process_parent_pid(child.pid) != Some(process.pid)
                {
                    return None;
                }
                owners.push(child.pid);
            }
        }
        let mut claims = Vec::new();
        for pid in owners {
            let birth = platform::process_start_token(pid)?;
            if platform::resume_owner_uid(pid)? != platform::agent_session_user_uid()? {
                return None;
            }
            let mut ids = HashSet::new();
            // Inspection failure is different from an idle agent with no open
            // transcripts: we must still be able to reject conflicting IDs.
            for path in platform::writable_file_paths(pid)? {
                if let Some(id) = self.transcript_id(agent, &path) {
                    ids.insert(id);
                }
            }
            if agent == Agent::Claude {
                if let Some(store) = &self.claude {
                    if let Some(id) = store.record_id(pid, birth, self.deadline) {
                        if !store.has_transcript(&id, self.deadline) {
                            return None;
                        }
                        ids.insert(id);
                    }
                }
            }
            if agent == Agent::GithubCopilot {
                let locks = self.locks.get_or_init(|| self.copilot_locks());
                if let Some(rows) = locks.as_ref().and_then(|index| index.get(&pid)) {
                    let started = platform::process_started_at(birth)?;
                    ids.extend(
                        rows.iter()
                            .filter(|(_, created)| *created >= started)
                            .map(|(id, _)| id.clone()),
                    );
                }
            }
            if ids.len() > 1 {
                return None;
            }
            if let Some(id) = ids.into_iter().next() {
                claims.push(Claim {
                    job,
                    pid,
                    birth,
                    agent,
                    id,
                });
            }
        }
        let first = claims.pop()?;
        if claims.iter().any(|other| other.id != first.id)
            || !first.still_foreground()
            || platform::process_start_token(job.group) != Some(leader_birth)
        {
            return None;
        }
        Some(first)
    }

    fn transcript_id(&self, agent: Agent, path: &Path) -> Option<String> {
        // Unlinked or renamed descriptors cannot identify a conversation the CLI
        // could reopen at this path. Check files only after matching the store.
        match agent {
            Agent::Claude => self.claude.as_ref()?.transcript_id(path),
            Agent::GithubCopilot => {
                let relative = path.strip_prefix(self.copilot.as_ref()?).ok()?;
                if relative.components().count() != 2 || relative.file_name()? != "events.jsonl" {
                    return None;
                }
                let id = uuid(relative.parent()?.to_str()?)?;
                owned_regular_file(path).then_some(id)
            }
            Agent::Codex => {
                let relative = path.strip_prefix(self.codex.as_ref()?).ok()?;
                if relative.components().count() != 4 {
                    return None;
                }
                let file = relative
                    .file_name()?
                    .to_str()?
                    .strip_prefix("rollout-")?
                    .strip_suffix(".jsonl")?;
                let id = uuid(file.get(file.len().checked_sub(36)?..)?)?;
                // A single Codex process can also write subagent rollouts.
                // Only inspect the first session_meta record, never conversational turns.
                let file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(path)
                    .ok()?;
                let metadata = file.metadata().ok()?;
                if !metadata.is_file() || metadata.uid() != platform::agent_session_user_uid()? {
                    return None;
                }
                let mut line = Vec::new();
                BufReader::new(file.take(128 * 1024))
                    .read_until(b'\n', &mut line)
                    .ok()?;
                let meta: serde_json::Value = serde_json::from_slice(&line).ok()?;
                (meta["type"] == "session_meta"
                    && meta["payload"]["source"] == "cli"
                    && meta["payload"]["id"] == id)
                    .then_some(id)
            }
            _ => None,
        }
    }

    fn copilot_locks(&self) -> Option<CopilotLocks> {
        let mut index = HashMap::<_, Vec<_>>::new();
        for entry in fs::read_dir(self.copilot.as_ref()?).ok()? {
            if Instant::now() >= self.deadline {
                return None;
            }
            let entry = entry.ok()?;
            let Some(id) = entry.file_name().to_str().and_then(uuid) else {
                continue;
            };
            if !entry.file_type().ok()?.is_dir()
                || !owned_regular_file(&entry.path().join("events.jsonl"))
            {
                continue;
            }
            for lock in fs::read_dir(entry.path()).ok()? {
                let lock = lock.ok()?;
                let name = lock.file_name();
                let Some(pid) = name.to_str().and_then(|s| {
                    s.strip_prefix("inuse.")?
                        .strip_suffix(".lock")?
                        .parse::<u32>()
                        .ok()
                }) else {
                    continue;
                };
                let meta = fs::symlink_metadata(lock.path()).ok()?;
                if meta.is_file() && meta.uid() == platform::agent_session_user_uid()? {
                    if let Ok(created) = meta.created() {
                        index.entry(pid).or_default().push((id.clone(), created));
                    }
                }
            }
        }
        Some(index)
    }
}

fn owned_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|info| info.is_file() && Some(info.uid()) == platform::agent_session_user_uid())
}

fn uuid(value: &str) -> Option<String> {
    (value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        }))
    .then(|| value.to_owned())
}
