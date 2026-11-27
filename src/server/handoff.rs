#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Command};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tracing::{info, warn};

#[cfg(unix)]
const HANDOFF_VERSION: u32 = 1;
#[cfg(unix)]
const READY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const OWNED_ACK_TIMEOUT: Duration = Duration::from_millis(500);
// Descriptors are transferred in batches of this size. A single SCM_RIGHTS
// control message caps out at 253 descriptors on Linux and 254 on macOS, so the
// batch stays well below both limits and the number of panes stays unbounded.
#[cfg(unix)]
const FDS_PER_MESSAGE: usize = 64;
#[cfg(unix)]
pub(crate) const MAX_REPLAY_BYTES_PER_PANE: usize = 8 * 1024;
#[cfg(unix)]
pub(crate) const COMMIT_TIMEOUT: Duration = READY_TIMEOUT;

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
pub(crate) struct HandoffManifest {
    pub version: u32,
    pub source_version: String,
    pub source_protocol: u32,
    pub expected_version: Option<String>,
    pub expected_protocol: Option<u32>,
    pub snapshot: crate::persist::SessionSnapshot,
    pub panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    /// Pane order for optional anonymous history descriptors. Extra descriptors
    /// are sent only after the importer explicitly negotiates history-fds-v1.
    /// Older importers keep using the bounded inline replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history_panes: Vec<u32>,
    /// An outer window title set over the API outlives the server that took the
    /// call, so a handoff carries it rather than falling back to the config.
    /// Absent from manifests written before this field existed.
    #[serde(default)]
    pub api_window_title: Option<String>,
    /// A host restart or shutdown announced to the old server but not yet
    /// committed. Absent from manifests written before this field existed.
    #[serde(default)]
    pub host_shutdown_intent: Option<crate::platform::HostShutdownIntent>,
}

#[cfg(unix)]
pub(crate) struct ReceivedHandoff {
    pub manifest: HandoffManifest,
    pub fds: Vec<RawFd>,
    pub histories: std::collections::HashMap<u32, std::fs::File>,
    pub stream: UnixStream,
}

#[cfg(unix)]
pub(crate) fn handoff_socket_path() -> PathBuf {
    crate::session::data_dir().join(format!("herdr-handoff-{}.sock", std::process::id()))
}

#[cfg(unix)]
pub(crate) fn spawn_handoff_import(
    import_exe: Option<&Path>,
    socket_path: &Path,
    token: &str,
) -> io::Result<Child> {
    let fallback_exe;
    let exe = if let Some(import_exe) = import_exe {
        import_exe
    } else {
        fallback_exe = std::env::current_exe().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to determine herdr executable path: {err}"),
            )
        })?;
        &fallback_exe
    };
    let mut command = Command::new(exe);
    command
        .arg("server")
        .arg("--handoff-import")
        .arg(socket_path)
        .arg(token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if crate::session::explicit_session_requested() {
        // The import child no longer has the original `--session` argument, so
        // stale socket overrides must not mask the inherited HERDR_SESSION.
        command
            .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
            .env_remove(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR);
    }
    crate::platform::detach_server_daemon_command(&mut command);
    command.spawn().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to spawn handoff import server at {}: {err}",
                exe.display()
            ),
        )
    })
}

#[cfg(unix)]
pub(crate) fn cleanup_failed_import_child(child: &mut Child) {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => {
            info!(pid, status = %status, "handoff import server exited during rollback");
            return;
        }
        Ok(None) => {}
        Err(err) => {
            warn!(pid, err = %err, "failed to inspect handoff import server before rollback");
        }
    }

    if let Err(err) = child.kill() {
        warn!(pid, err = %err, "failed to kill handoff import server during rollback");
    }
    match child.wait() {
        Ok(status) => {
            info!(pid, status = %status, "handoff import server reaped during rollback");
        }
        Err(err) => {
            warn!(pid, err = %err, "failed to reap handoff import server during rollback");
        }
    }
}

#[cfg(unix)]
pub(crate) fn bind_listener(socket_path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    restrict_socket_permissions(socket_path)?;
    Ok(listener)
}

#[cfg(unix)]
pub(crate) fn accept_and_validate_on(
    listener: UnixListener,
    socket_path: &Path,
    token: &str,
    manifest: &HandoffManifest,
) -> io::Result<(UnixStream, bool)> {
    let (mut stream, _) = accept_with_timeout(&listener, READY_TIMEOUT)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    stream.set_write_timeout(Some(READY_TIMEOUT))?;
    let token_line = read_line_unbuffered(&mut stream)?;
    if token_line.trim_end() != token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff import token mismatch",
        ));
    }

    serde_json::to_writer(&mut stream, manifest).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let validated = read_line_unbuffered(&mut stream)?;
    let full_history = match validated.trim_end() {
        "validated" => false,
        "validated history-fds-v1" if !manifest.history_panes.is_empty() => true,
        _ => return Err(io::Error::other("handoff import did not validate manifest")),
    };
    let _ = std::fs::remove_file(socket_path);
    Ok((stream, full_history))
}

#[cfg(unix)]
pub(crate) fn send_fds_and_wait_restored(
    stream: &mut UnixStream,
    fds: &[RawFd],
    history_fds: &[RawFd],
) -> io::Result<()> {
    send_fds(stream, fds)?;
    send_fds(stream, history_fds)?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let restored = read_line_unbuffered(&mut *stream)?;
    if restored.trim_end() != "restored" {
        return Err(io::Error::other(
            "handoff import did not report restored runtimes",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn wait_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let ready = read_line_unbuffered(&mut *stream)?;
    if ready.trim_end() != "ready" {
        return Err(io::Error::other("handoff import did not report ready"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"committed\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_owned_ack(stream: &mut UnixStream) {
    if let Err(err) = stream.set_read_timeout(Some(OWNED_ACK_TIMEOUT)) {
        warn!(err = %err, "failed to set handoff ownership ack timeout");
        return;
    }
    match read_line_unbuffered(&mut *stream) {
        Ok(owned) if owned.trim_end() == "owned" => {}
        Ok(other) => {
            warn!(
                response = %other.trim_end(),
                "handoff import sent unexpected ownership ack after commit"
            );
        }
        Err(err) => {
            warn!(err = %err, "handoff import ownership ack was not received after commit");
        }
    }
}

#[cfg(unix)]
pub(crate) fn receive(socket_path: &Path, token: &str) -> io::Result<ReceivedHandoff> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let manifest_line = read_line_unbuffered(&mut stream)?;
    let manifest: HandoffManifest =
        serde_json::from_str(&manifest_line).map_err(io::Error::other)?;
    if manifest.version != HANDOFF_VERSION {
        return Err(io::Error::other(format!(
            "unsupported handoff version {}",
            manifest.version
        )));
    }
    if manifest
        .expected_protocol
        .is_some_and(|protocol| protocol != crate::protocol::PROTOCOL_VERSION)
    {
        return Err(io::Error::other(format!(
            "handoff expected protocol {}, but this server speaks protocol {}",
            manifest.expected_protocol.unwrap_or_default(),
            crate::protocol::PROTOCOL_VERSION
        )));
    }
    if manifest
        .expected_version
        .as_deref()
        .is_some_and(|version| version != crate::build_info::version())
    {
        return Err(io::Error::other(format!(
            "handoff expected herdr v{}, but this server is v{}",
            manifest.expected_version.as_deref().unwrap_or("unknown"),
            crate::build_info::version()
        )));
    }
    let panes: std::collections::HashSet<_> =
        manifest.panes.iter().map(|pane| pane.pane_id).collect();
    let mut seen = std::collections::HashSet::new();
    if manifest
        .history_panes
        .iter()
        .any(|pane| !panes.contains(pane) || !seen.insert(*pane))
    {
        return Err(io::Error::other("invalid handoff history pane list"));
    }
    stream.write_all(if manifest.history_panes.is_empty() {
        b"validated\n"
    } else {
        b"validated history-fds-v1\n"
    })?;
    stream.flush()?;
    let fds = recv_fds(&stream, manifest.panes.len())?;
    let history_fds = match recv_fds(&stream, manifest.history_panes.len()) {
        Ok(fds) => fds,
        Err(error) => {
            close_raw_fds(&fds);
            return Err(error);
        }
    };
    use std::os::fd::FromRawFd;
    let histories: std::collections::HashMap<_, _> = manifest
        .history_panes
        .iter()
        .copied()
        .zip(history_fds.into_iter().map(|fd| {
            // SAFETY: recv_fds returned a new descriptor owned by this receiver.
            unsafe { std::fs::File::from_raw_fd(fd) }
        }))
        .collect();
    if histories
        .values()
        .any(|file| !crate::platform::program_resume::private_snapshot(file))
    {
        close_raw_fds(&fds);
        return Err(io::Error::other(
            "handoff history descriptor is not a private regular file",
        ));
    }
    Ok(ReceivedHandoff {
        manifest,
        fds,
        histories,
        stream,
    })
}

#[cfg(unix)]
pub(crate) fn report_restored(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"restored\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn report_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"ready\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let committed = read_line_unbuffered(&mut *stream)?;
    if committed.trim_end() != "committed" {
        return Err(io::Error::other("handoff source did not commit"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_owned(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"owned\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn manifest_for(
    snapshot: crate::persist::SessionSnapshot,
    panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    expected_protocol: Option<u32>,
    expected_version: Option<String>,
    api_window_title: Option<String>,
    host_shutdown_intent: Option<crate::platform::HostShutdownIntent>,
) -> HandoffManifest {
    HandoffManifest {
        version: HANDOFF_VERSION,
        source_version: crate::build_info::version(),
        source_protocol: crate::protocol::PROTOCOL_VERSION,
        expected_version,
        expected_protocol,
        snapshot,
        panes,
        history_panes: Vec::new(),
        api_window_title,
        host_shutdown_intent,
    }
}

#[cfg(unix)]
fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(accepted) => return Ok(accepted),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for handoff import connection",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn read_line_unbuffered(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = match stream.read(&mut byte) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "handoff stream closed while reading line",
            ));
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
        }
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff line exceeded maximum size",
            ));
        }
    }
}

#[cfg(unix)]
fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    for batch in fds.chunks(FDS_PER_MESSAGE) {
        send_fd_batch(stream, batch)?;
    }
    Ok(())
}

#[cfg(unix)]
fn send_fd_batch(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() {
        return Ok(());
    }
    let byte = [b'F'];
    let iov = [libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = std::mem::size_of_val(fds);
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("failed to allocate fd control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_bytes);
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn close_raw_fds(fds: &[RawFd]) {
    for fd in fds {
        let _ = unsafe { libc::close(*fd) };
    }
}

#[cfg(unix)]
fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    let mut out: Vec<RawFd> = Vec::with_capacity(expected);
    while out.len() < expected {
        let wanted = (expected - out.len()).min(FDS_PER_MESSAGE);
        let batch = match recv_fd_batch(stream, wanted) {
            Ok(batch) => batch,
            Err(err) => {
                close_raw_fds(&out);
                return Err(err);
            }
        };
        if batch.is_empty() {
            let received = out.len();
            close_raw_fds(&out);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "handoff stream closed after {received} of {expected} pane file descriptors"
                ),
            ));
        }
        out.extend(batch);
    }
    Ok(out)
}

#[cfg(unix)]
fn recv_fd_batch(stream: &UnixStream, wanted: usize) -> io::Result<Vec<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = wanted * std::mem::size_of::<RawFd>();
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    let read = loop {
        msg.msg_controllen = control.len() as _;
        let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if read >= 0 {
            break read;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };

    let mut out = Vec::new();
    unsafe {
        let control_end = control.as_ptr() as usize + msg.msg_controllen as usize;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                // Bound the payload by both the header's own length and the
                // bytes the kernel wrote into `control`, so the read below can
                // never run past the buffer.
                let available = control_end.saturating_sub(data as usize);
                let data_len = ((*cmsg).cmsg_len as usize)
                    .saturating_sub(libc::CMSG_LEN(0) as usize)
                    .min(available);
                let count = data_len / std::mem::size_of::<RawFd>();
                let data = data as *const RawFd;
                for idx in 0..count {
                    out.push(*data.add(idx));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    // Truncation means the kernel closed the descriptors that did not fit, so
    // the batch is unrecoverable rather than merely short.
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        close_raw_fds(&out);
        return Err(io::Error::other("handoff fd control message was truncated"));
    }
    if read == 0 {
        close_raw_fds(&out);
        return Ok(Vec::new());
    }
    if out.len() > wanted {
        let received = out.len();
        close_raw_fds(&out);
        return Err(io::Error::other(format!(
            "handoff fd message carried {received} descriptors, expected at most {wanted}"
        )));
    }
    if out.is_empty() {
        return Err(io::Error::other("handoff fd message missing SCM_RIGHTS"));
    }
    for fd in &out {
        if let Err(error) = crate::platform::descriptor_close_on_exec(*fd) {
            close_raw_fds(&out);
            return Err(error);
        }
    }
    Ok(out)
}

#[cfg(unix)]
pub(crate) fn log_import_result(panes: usize) {
    info!(panes, "handoff import ready");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn history_fd_negotiation_preserves_both_legacy_directions() {
        use std::io::Read;
        for (full_export, full_import) in [(true, true), (true, false), (false, true)] {
            let socket = PathBuf::from(format!(
                "/tmp/herdr-history-{}-{full_export}-{full_import}.sock",
                std::process::id()
            ));
            let listener = bind_listener(&socket).unwrap();
            let history = "oldest\n".to_owned() + &"history line\n".repeat(3000) + "newest\n";
            let pane: crate::handoff_runtime::HandoffRuntimeState =
                serde_json::from_value(serde_json::json!({
                    "pane_id": 1, "child_pid": 0, "rows": 3, "cols": 10,
                    "cell_width_px": 0, "cell_height_px": 0,
                    "initial_history_ansi": "recent-only\r\n"
                }))
                .unwrap();
            let mut manifest = manifest_for(empty_snapshot(), vec![pane], None, None, None, None);
            let mut histories = super::super::handoff_history::HistoryFiles::new(
                &socket,
                format!("test-{}", std::process::id()),
            )
            .unwrap();
            if full_export {
                histories.store(1, &history).unwrap();
                manifest.history_panes = histories.panes();
            }
            let export_socket = socket.clone();
            let exporter = std::thread::spawn(move || {
                let (mut stream, negotiated) = if full_export {
                    accept_and_validate_on(listener, &export_socket, "test-token", &manifest)
                        .unwrap()
                } else {
                    // Freeze the old exporter's exact acknowledgment behavior:
                    // unknown suffixes fail rather than silently opt into FDs.
                    let (mut stream, _) = accept_with_timeout(&listener, READY_TIMEOUT).unwrap();
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(READY_TIMEOUT)).unwrap();
                    assert_eq!(read_line_unbuffered(&mut stream).unwrap(), "test-token\n");
                    serde_json::to_writer(&mut stream, &manifest).unwrap();
                    stream.write_all(b"\n").unwrap();
                    assert_eq!(read_line_unbuffered(&mut stream).unwrap(), "validated\n");
                    (stream, false)
                };
                let placeholder_pty = std::fs::File::open("/dev/null").unwrap();
                let extra = if negotiated {
                    histories.fds()
                } else {
                    Vec::new()
                };
                send_fds_and_wait_restored(&mut stream, &[placeholder_pty.as_raw_fd()], &extra)
                    .unwrap();
                negotiated
            });
            if full_import {
                let mut received = receive(&socket, "test-token").unwrap();
                assert_eq!(
                    received.manifest.panes[0].initial_history_ansi.as_deref(),
                    Some("recent-only\r\n")
                );
                if full_export {
                    let mut actual = String::new();
                    received
                        .histories
                        .remove(&1)
                        .unwrap()
                        .read_to_string(&mut actual)
                        .unwrap();
                    assert_eq!(actual, history);
                } else {
                    assert!(received.histories.is_empty());
                }
                close_raw_fds(&received.fds);
                report_restored(&mut received.stream).unwrap();
            } else {
                let mut stream = UnixStream::connect(&socket).unwrap();
                stream.set_read_timeout(Some(READY_TIMEOUT)).unwrap();
                stream.write_all(b"test-token\n").unwrap();
                let manifest: serde_json::Value =
                    serde_json::from_str(&read_line_unbuffered(&mut stream).unwrap()).unwrap();
                assert_eq!(
                    manifest["panes"][0]["initial_history_ansi"],
                    "recent-only\r\n"
                );
                stream.write_all(b"validated\n").unwrap();
                let fds = recv_fds(&stream, 1).unwrap();
                close_raw_fds(&fds);
                report_restored(&mut stream).unwrap();
            }
            assert_eq!(exporter.join().unwrap(), full_export && full_import);
            let _ = std::fs::remove_file(socket);
        }
    }

    fn empty_snapshot() -> crate::persist::SessionSnapshot {
        crate::persist::SessionSnapshot {
            version: 0,
            workspaces: Vec::new(),
            active: None,
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            sidebar_health_section_split: None,
            collapsed_space_keys: Default::default(),
        }
    }

    #[test]
    fn a_handoff_carries_an_api_set_window_title() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
            None,
        );

        assert_eq!(manifest.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn a_handoff_carries_an_announced_host_shutdown() {
        let intent: crate::platform::HostShutdownIntent =
            serde_json::from_value(serde_json::json!({ "announced_at_ns": 42 }))
                .expect("intent should deserialize");
        let manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, Some(intent));
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");
        let decoded: HandoffManifest =
            serde_json::from_slice(&encoded).expect("manifest should deserialize");

        assert_eq!(decoded.host_shutdown_intent, Some(intent));
    }

    #[test]
    fn a_manifest_written_before_the_host_shutdown_field_still_loads() {
        let manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("host_shutdown_intent");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.host_shutdown_intent.is_none());
    }

    #[test]
    fn an_older_server_imports_a_manifest_with_an_announced_host_shutdown() {
        /// The manifest as servers before `host_shutdown_intent` decode it,
        /// frozen here so the new field stays ignorable by them.
        #[derive(Deserialize)]
        #[allow(dead_code)] // Only decoding is under test.
        struct PreHostShutdownManifest {
            version: u32,
            source_version: String,
            source_protocol: u32,
            expected_version: Option<String>,
            expected_protocol: Option<u32>,
            snapshot: crate::persist::SessionSnapshot,
            panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
            #[serde(default)]
            api_window_title: Option<String>,
        }

        let intent: crate::platform::HostShutdownIntent =
            serde_json::from_value(serde_json::json!({ "announced_at_ns": 42 }))
                .expect("intent should deserialize");
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
            Some(intent),
        );
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");

        let older: PreHostShutdownManifest =
            serde_json::from_slice(&encoded).expect("an older server should still load it");

        assert_eq!(older.version, HANDOFF_VERSION);
        assert_eq!(older.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn a_manifest_written_before_the_title_field_still_loads() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
            None,
        );
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("api_window_title");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.api_window_title.is_none());
    }
}
