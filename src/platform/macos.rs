use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub(super) const REMOTE_BRIDGE_CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;

use super::{
    read_limited_reader, ClipboardCommand, ClipboardImage, ForegroundJob, ForegroundProcess,
    LimitedRead, Signal,
};

pub(crate) use super::unix_common::{
    configure_status_command, create_remote_private_dir, create_remote_ssh_config_dir,
    create_remote_ssh_config_file, hostname, local_datetime, remote_bridge_endpoint_path,
    remote_private_temp_base, remote_reattach_argument, remote_reattach_program,
    remote_ssh_config_paths, set_default_plugin_pane_pwd, shutdown_client_stream,
    status_commands_supported, wait_client_stream_readable, write_client_stream,
    ClientStreamReader, PinnedSshControlSocket, StatusCommandGuard,
};

mod bootstrap;
mod writable_files;
pub(crate) use bootstrap::{configure_server_daemon_context, prepare_server_process};
pub(crate) use writable_files::writable_file_paths;

mod shutdown;
pub(crate) use shutdown::monitor_host_shutdown;
mod power;
pub(crate) use power::monitor_host_power;

mod launchd_spawn;
pub(crate) use launchd_spawn::{leave_launchd_job, spawn_launchd_job};

#[cfg(test)]
mod config_file_tests;

#[cfg(test)]
mod input_tests;

mod input;
pub(crate) use input::PtyInputGuard;

pub(crate) fn config_file_link_count(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.nlink())
}

pub(crate) fn check_config_write_target(_target: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

pub(crate) fn write_existing_config(
    _target: &std::path::Path,
    _contents: &[u8],
) -> std::io::Result<bool> {
    // Unix keeps atomic replacement for existing files too.
    Ok(false)
}

pub(super) fn file_has_no_extended_acl(fd: std::os::fd::RawFd) -> bool {
    // Darwin extended ACLs can grant access independently of the mode bits.
    // These libc functions from <sys/acl.h> are absent from the Rust bindings.
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            index: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
    const ACL_TYPE_EXTENDED: libc::c_int = 0x100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    // A live file descriptor returns ENOENT when no extended ACL is attached.
    let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT);
    }
    let mut entry = std::ptr::null_mut();
    // A valid, empty ACL has no first entry (EINVAL).
    let empty = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL);
    unsafe { acl_free(acl) };
    empty
}

pub(crate) fn create_config_temporary(
    path: &Path,
    private: bool,
) -> std::io::Result<std::fs::File> {
    if !private {
        return std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path);
    }
    use std::os::fd::FromRawFd;
    // Darwin's opaque ACL/filesec APIs (<sys/acl.h>, <sys/fcntl.h>) are not
    // exposed by libc. Supply a non-inheriting empty ACL at creation: clearing
    // inherited ACEs later cannot revoke descriptors opened in the meantime.
    unsafe extern "C" {
        fn acl_init(count: libc::c_int) -> *mut libc::c_void;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
        fn acl_get_flagset_np(acl: *mut libc::c_void, flags: *mut *mut libc::c_void)
            -> libc::c_int;
        fn acl_add_flag_np(flags: *mut libc::c_void, flag: libc::c_uint) -> libc::c_int;
        fn filesec_init() -> *mut libc::c_void;
        fn filesec_free(security: *mut libc::c_void);
        fn filesec_set_property(
            security: *mut libc::c_void,
            property: libc::c_int,
            value: *const libc::c_void,
        ) -> libc::c_int;
        fn openx_np(
            path: *const libc::c_char,
            flags: libc::c_int,
            security: *mut libc::c_void,
        ) -> libc::c_int;
    }
    const FILESEC_MODE: libc::c_int = 4;
    const FILESEC_ACL: libc::c_int = 5;
    const ACL_FLAG_NO_INHERIT: libc::c_uint = 1 << 17;
    let path =
        std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let acl = unsafe { acl_init(0) };
    if acl.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let security = unsafe { filesec_init() };
    if security.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            acl_free(acl);
        }
        return Err(error);
    }
    let result = (|| {
        let mut flags = std::ptr::null_mut();
        let mode: libc::mode_t = 0o600;
        if unsafe { acl_get_flagset_np(acl, &mut flags) } != 0
            || unsafe { acl_add_flag_np(flags, ACL_FLAG_NO_INHERIT) } != 0
            || unsafe {
                filesec_set_property(security, FILESEC_MODE, std::ptr::from_ref(&mode).cast())
            } != 0
            || unsafe {
                filesec_set_property(security, FILESEC_ACL, std::ptr::from_ref(&acl).cast())
            } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe {
            openx_np(
                path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                security,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The successful exclusive create returns one owned descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    })();
    unsafe {
        filesec_free(security);
        acl_free(acl);
    }
    result
}

pub(crate) fn write_config_temporary(
    source: Option<&Path>,
    temporary: &Path,
    contents: &[u8],
) -> std::io::Result<()> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(temporary)?;
    if let Some(source) = source {
        let input = std::fs::File::open(source)?;
        let metadata = input.metadata()?;
        let current = output.metadata()?;
        if (metadata.uid(), metadata.gid()) != (current.uid(), current.gid())
            && unsafe { libc::fchown(output.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // Prepare access controls while the temporary is still empty. Copy ACLs
        // before mode bits so no inherited/default grant can expose the content.
        // Do not copy data or old timestamps.
        if unsafe {
            libc::fcopyfile(
                input.as_raw_fd(),
                output.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_ACL | libc::COPYFILE_XATTR,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        output.set_permissions(metadata.permissions())?;
    }
    output.write_all(contents)?;
    output.sync_all()
}

extern "C" {
    fn mach_host_self() -> libc::mach_port_t;
}

const PROC_PGRP_ONLY: u32 = 2;
const SERVER_NOFILE_LIMIT_TARGET: libc::rlim_t = 8192;
const NOTIFICATION_ACTIVATION_READ_LIMIT: u64 = 32;
static NOTIFICATION_ENDPOINT_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn should_draw_host_cursor_by_default() -> bool {
    false
}

pub(crate) fn should_query_host_terminal_palette() -> bool {
    true
}

pub(crate) fn health_probe() -> crate::health::PlatformHealthProbe {
    crate::health::PlatformHealthProbe {
        cpu_times: read_cpu_times(),
        temperature_celsius: command_stdout("/usr/sbin/ioreg", &["-r", "-n", "AppleSmartBattery"])
            .as_deref()
            .and_then(parse_battery_temperature),
        battery: command_stdout("/usr/bin/pmset", &["-g", "batt"])
            .as_deref()
            .and_then(parse_battery_health),
        ram: read_memory_usage(),
        disk: read_disk_usage("/"),
    }
}

fn parse_battery_temperature(contents: &str) -> Option<i16> {
    let raw = contents.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        (key.trim() == "\"Temperature\"")
            .then(|| value.trim().parse::<u32>().ok())
            .flatten()
    })?;
    crate::health::celsius_from_scaled(i64::from(raw), 100, -273_150)
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn read_cpu_times() -> Option<crate::health::CpuTimes> {
    // SAFETY: `host_cpu_load_info` is a plain Mach output structure, and the
    // count tells `host_statistics` exactly how much writable storage is
    // available through the cast output pointer.
    let mut info = unsafe { std::mem::zeroed::<libc::host_cpu_load_info>() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    let result = unsafe {
        libc::host_statistics(
            mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            (&mut info as *mut libc::host_cpu_load_info).cast::<libc::integer_t>(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS || count < libc::HOST_CPU_LOAD_INFO_COUNT {
        return None;
    }

    let total = info
        .cpu_ticks
        .iter()
        .copied()
        .map(u64::from)
        .fold(0_u64, u64::saturating_add);
    let idle = u64::from(info.cpu_ticks[libc::CPU_STATE_IDLE as usize]);
    Some(crate::health::CpuTimes {
        busy: total.saturating_sub(idle),
        total,
    })
}

fn read_memory_usage() -> Option<crate::health::ResourceUsage> {
    let total_bytes = command_stdout("/usr/sbin/sysctl", &["-n", "hw.memsize"])?
        .trim()
        .parse::<u64>()
        .ok()?;
    let vm_stat = command_stdout("/usr/bin/vm_stat", &[])?;
    parse_memory_usage(total_bytes, &vm_stat)
}

fn parse_memory_usage(total_bytes: u64, vm_stat: &str) -> Option<crate::health::ResourceUsage> {
    let page_size = vm_stat
        .lines()
        .next()?
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    let mut available_pages = 0_u64;
    for line in vm_stat.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !matches!(
            name.trim(),
            "Pages free" | "Pages inactive" | "Pages speculative"
        ) {
            continue;
        }
        let pages = value.trim().trim_end_matches('.').parse::<u64>().ok()?;
        available_pages = available_pages.saturating_add(pages);
    }
    let available_bytes = available_pages.saturating_mul(page_size);
    crate::health::ResourceUsage::new(total_bytes.saturating_sub(available_bytes), total_bytes)
}

fn parse_battery_health(contents: &str) -> Option<crate::health::BatteryHealth> {
    let line = contents.lines().find(|line| line.contains('%'))?;
    let percentage_end = line.find('%')?;
    let percentage_start = line[..percentage_end]
        .rfind(|ch: char| !ch.is_ascii_digit())
        .map_or(0, |index| index + 1);
    let percentage = line[percentage_start..percentage_end]
        .parse::<u8>()
        .ok()?
        .min(100);
    let status = line.to_ascii_lowercase();
    let charging = if status.contains("discharging") {
        Some(false)
    } else if status.contains("charging") || status.contains("charged") {
        Some(true)
    } else {
        None
    };
    Some(crate::health::BatteryHealth {
        percentage,
        charging,
    })
}

fn read_disk_usage(path: &str) -> Option<crate::health::ResourceUsage> {
    let path = std::ffi::CString::new(path).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated string and `stats` points to
    // writable storage for one `statvfs` result.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: a successful `statvfs` call initialized the output structure.
    let stats = unsafe { stats.assume_init() };
    let block_size = stats.f_frsize;
    let total_bytes = u64::from(stats.f_blocks).saturating_mul(block_size);
    let free_bytes = u64::from(stats.f_bfree).saturating_mul(block_size);
    crate::health::ResourceUsage::new(total_bytes.saturating_sub(free_bytes), total_bytes)
}

fn raw_command_argv(command: &str, flag: &str) -> Vec<std::ffi::OsString> {
    vec!["/bin/sh".into(), flag.into(), command.into()]
}

pub(crate) fn detached_custom_command_process_platform(command: &str) -> std::process::Command {
    let argv = raw_command_argv(command, "-lc");
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}

pub(crate) fn pane_custom_command_pty_builder_platform(
    command: &str,
) -> portable_pty::CommandBuilder {
    portable_pty::CommandBuilder::from_argv(raw_command_argv(command, "-c"))
}

pub(crate) fn scrollback_editor_argv(path: &Path) -> std::io::Result<Vec<String>> {
    let quoted_path = shell_quote(&path.display().to_string());
    let command = format!(
        r#"scrollback_file={quoted_path}; eval "${{EDITOR:-vi}} \"\$scrollback_file\""; status=$?; rm -f "$scrollback_file"; exit $status"#
    );
    Ok(vec!["/bin/sh".to_string(), "-c".to_string(), command])
}

pub(crate) fn interactive_shell_command(argv: &[String], shell_name: &str) -> Option<String> {
    super::interactive_unix_shell_command(argv, shell_name, shell_quote)
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

#[repr(C)]
struct TisInputSource {
    _private: [u8; 0],
}

type TisInputSourceRef = *const TisInputSource;
type CfTypeRef = *const libc::c_void;
type CfStringRef = *const libc::c_void;
type OsStatus = libc::c_int;
type Boolean = libc::c_uchar;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    #[link_name = "kTISPropertyInputSourceID"]
    static TIS_PROPERTY_INPUT_SOURCE_ID: CfStringRef;

    #[link_name = "TISCopyCurrentKeyboardInputSource"]
    fn tis_copy_current_keyboard_input_source() -> TisInputSourceRef;

    #[link_name = "TISCopyCurrentASCIICapableKeyboardLayoutInputSource"]
    fn tis_copy_current_ascii_capable_keyboard_layout_input_source() -> TisInputSourceRef;

    #[link_name = "TISGetInputSourceProperty"]
    fn tis_get_input_source_property(
        input_source: TisInputSourceRef,
        property_key: CfStringRef,
    ) -> CfTypeRef;

    #[link_name = "TISSelectInputSource"]
    fn tis_select_input_source(input_source: TisInputSourceRef) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    #[link_name = "CFRelease"]
    fn cf_release(value: CfTypeRef);

    #[link_name = "CFEqual"]
    fn cf_equal(left: CfTypeRef, right: CfTypeRef) -> Boolean;

    #[link_name = "kCFRunLoopDefaultMode"]
    static CF_RUN_LOOP_DEFAULT_MODE: CfStringRef;

    #[link_name = "CFRunLoopRunInMode"]
    fn cf_run_loop_run_in_mode(
        mode: CfStringRef,
        seconds: f64,
        return_after_source_handled: Boolean,
    ) -> libc::c_int;
}

/// Pump the main thread's run loop once (non-blocking) so the process receives the
/// `kTISNotifySelectedKeyboardInputSourceChanged` notification and refreshes the per-process cache
/// that `TISCopyCurrentKeyboardInputSource` reads. That notification arrives only via the main
/// thread's run loop, so a process that never runs a CFRunLoop (the headless server) reads a stale
/// source. Must run on the main thread.
pub(crate) fn pump_input_source_runloop() {
    debug_assert!(
        // SAFETY: `pthread_main_np` is always safe to call.
        unsafe { libc::pthread_main_np() } != 0,
        "pump_input_source_runloop must run on the main thread"
    );
    // SAFETY: `CFRunLoopRunInMode` is thread-safe; a 0-second call drains the ready sources and
    // returns immediately (no blocking). `CF_RUN_LOOP_DEFAULT_MODE` is a framework-owned constant.
    unsafe {
        let _ = cf_run_loop_run_in_mode(CF_RUN_LOOP_DEFAULT_MODE, 0.0, 0);
    }
}

#[derive(Debug)]
struct RetainedInputSource(NonNull<TisInputSource>);

impl RetainedInputSource {
    /// Takes ownership of a retained reference returned by a TIS `Copy` function.
    unsafe fn from_copy(raw: TisInputSourceRef) -> Option<Self> {
        NonNull::new(raw as *mut TisInputSource).map(Self)
    }

    fn select(&self) -> OsStatus {
        // SAFETY: this wrapper keeps the retained input source alive for the call.
        unsafe { tis_select_input_source(self.0.as_ptr()) }
    }

    fn has_same_id(&self, other: &Self) -> bool {
        // SAFETY: TIS property values stay valid while their input sources are alive;
        // both wrappers outlive this comparison.
        unsafe {
            let left = tis_get_input_source_property(self.0.as_ptr(), TIS_PROPERTY_INPUT_SOURCE_ID);
            let right =
                tis_get_input_source_property(other.0.as_ptr(), TIS_PROPERTY_INPUT_SOURCE_ID);
            !left.is_null() && !right.is_null() && cf_equal(left, right) != 0
        }
    }
}

impl Drop for RetainedInputSource {
    fn drop(&mut self) {
        // SAFETY: `from_copy` gives this wrapper ownership of one retain.
        unsafe { cf_release(self.0.as_ptr().cast()) }
    }
}

#[derive(Debug)]
pub(crate) struct InputSourceRestore {
    previous: RetainedInputSource,
}

impl Drop for InputSourceRestore {
    fn drop(&mut self) {
        let status = self.previous.select();
        if status != 0 {
            tracing::debug!(
                status,
                "failed to restore host input source after prefix mode"
            );
        }
    }
}

pub(crate) fn switch_to_ascii_input_source() -> Option<InputSourceRestore> {
    // SAFETY: both Carbon `Copy` functions transfer one retain to the caller.
    let current =
        unsafe { RetainedInputSource::from_copy(tis_copy_current_keyboard_input_source())? };
    let ascii = unsafe {
        RetainedInputSource::from_copy(
            tis_copy_current_ascii_capable_keyboard_layout_input_source(),
        )?
    };

    if current.has_same_id(&ascii) {
        return None;
    }

    let status = ascii.select();
    if status != 0 {
        tracing::debug!(status, "failed to switch host input source for prefix mode");
        return None;
    }

    Some(InputSourceRestore { previous: current })
}

pub fn raise_server_nofile_limit() {
    match raise_nofile_limit(SERVER_NOFILE_LIMIT_TARGET) {
        Ok(None) => {}
        Ok(Some((previous, target))) => {
            tracing::info!(previous, target, "raised server file descriptor soft limit")
        }
        Err(err) => tracing::warn!(err = %err, "failed to raise server file descriptor limit"),
    }
}

fn raise_nofile_limit(
    target: libc::rlim_t,
) -> std::io::Result<Option<(libc::rlim_t, libc::rlim_t)>> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut limit = unsafe { limit.assume_init() };
    let Some(target) = target_nofile_soft_limit(limit.rlim_cur, limit.rlim_max, target) else {
        return Ok(None);
    };

    let previous = limit.rlim_cur;
    limit.rlim_cur = target;
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(Some((previous, target)))
}

fn target_nofile_soft_limit(
    current: libc::rlim_t,
    hard: libc::rlim_t,
    target: libc::rlim_t,
) -> Option<libc::rlim_t> {
    let target = if hard == libc::RLIM_INFINITY {
        target
    } else {
        target.min(hard)
    };

    (current < target).then_some(target)
}

pub(crate) fn available_pane_shell(child_pid: u32) -> Option<String> {
    super::available_pane_shell_from_job(child_pid, foreground_job(child_pid)?)
}

/// Collect the foreground terminal job for a given child PID.
pub fn foreground_job(child_pid: u32) -> Option<ForegroundJob> {
    if child_pid == 0 {
        return None;
    }

    let fg_pgid = foreground_process_group_id(child_pid)?;
    let mut processes = Vec::new();

    for pid in process_group_pids(fg_pgid) {
        let Some(info) = process_bsdinfo(pid) else {
            continue;
        };
        if info.pbi_pgid != fg_pgid {
            continue;
        }

        let Some(name) = comm_from_bsdinfo(&info) else {
            continue;
        };
        let argv = process_argv(pid);
        processes.push(ForegroundProcess {
            pid,
            name,
            argv0: process_argv0_name(pid),
            cmdline: argv.as_ref().map(|parts| parts.join(" ")),
            argv,
        });
    }

    if processes.is_empty() {
        return None;
    }

    Some(ForegroundJob {
        process_group_id: fg_pgid,
        processes,
    })
}

pub fn foreground_group_leader_job(process_group_id: u32) -> Option<ForegroundJob> {
    let info = process_bsdinfo(process_group_id)?;
    if info.pbi_pgid != process_group_id {
        return None;
    }

    let name = comm_from_bsdinfo(&info)?;
    let argv = process_argv(process_group_id);
    Some(ForegroundJob {
        process_group_id,
        processes: vec![ForegroundProcess {
            pid: process_group_id,
            name,
            argv0: process_argv0_name(process_group_id),
            cmdline: argv.as_ref().map(|parts| parts.join(" ")),
            argv,
        }],
    })
}

fn process_group_pids(process_group_id: u32) -> Vec<u32> {
    let mut capacity = 16usize;

    for _ in 0..8 {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let buffer_bytes = pids.len() * std::mem::size_of::<libc::pid_t>();
        let returned_bytes = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                process_group_id,
                pids.as_mut_ptr() as *mut libc::c_void,
                buffer_bytes as libc::c_int,
            )
        };
        if returned_bytes <= 0 {
            return Vec::new();
        }

        let returned_bytes = returned_bytes as usize;
        let count = returned_bytes / std::mem::size_of::<libc::pid_t>();
        if returned_bytes < buffer_bytes {
            return collect_positive_pids(pids, count);
        }
        capacity = capacity.saturating_mul(2);
    }

    Vec::new()
}

/// Read `e_tpgid` (foreground process group of the controlling terminal)
/// for the given PID.
pub fn foreground_process_group_id(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;

    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };

    if ret != size {
        return None;
    }

    let fg = info.e_tpgid;
    if fg > 0 {
        #[allow(clippy::unnecessary_cast)] // info.e_tpgid (pid_t) type is platform-dependent
        Some(fg as u32)
    } else {
        None
    }
}

pub fn foreground_process_group_id_for_tty_fd(fd: RawFd) -> Option<u32> {
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    (pgid > 0).then_some(pgid as u32)
}

/// Get the effective process name from `argv[0]` via `sysctl(KERN_PROCARGS2)`.
///
/// This is the macOS equivalent of reading `/proc/{pid}/cmdline` on Linux.
/// It reflects runtime title changes like Node.js `process.title = "pi"`.
fn process_argv0_name(pid: u32) -> Option<String> {
    let buf = kern_procargs2(pid)?;

    // Layout: [argc: i32] [exec_path\0] [padding\0...] [argv[0]\0] [argv[1]\0] ...
    if buf.len() < 4 {
        return None;
    }

    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if argc < 1 {
        return None;
    }

    // Skip past exec_path and null padding to reach argv[0]
    let rest = &buf[4..];
    let exec_end = rest.iter().position(|&b| b == 0)?;
    let mut pos = exec_end;
    while pos < rest.len() && rest[pos] == 0 {
        pos += 1;
    }
    if pos >= rest.len() {
        return None;
    }

    // Read argv[0]
    let argv0_end = rest[pos..]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(rest.len() - pos);
    let argv0 = std::str::from_utf8(&rest[pos..pos + argv0_end]).ok()?;

    if argv0.is_empty() {
        return None;
    }

    // Return basename (argv[0] may be a full path like "/usr/bin/node")
    let basename = Path::new(argv0).file_name()?.to_str()?;

    // Strip leading dash (login shells show as "-zsh")
    let name = basename.strip_prefix('-').unwrap_or(basename);
    if name.is_empty() {
        return None;
    }

    Some(name.to_string())
}

/// Raw `sysctl(KERN_PROCARGS2)` call. Returns the full buffer.
fn kern_procargs2(pid: u32) -> Option<Vec<u8>> {
    unsafe {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];

        // First call: query required buffer size
        let mut size: libc::size_t = 0;
        let ret = libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        );
        if ret != 0 || size == 0 {
            return None;
        }

        // Second call: read data
        let mut buf = vec![0u8; size];
        let ret = libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        );
        if ret != 0 {
            return None;
        }
        buf.truncate(size);
        Some(buf)
    }
}

pub fn write_clipboard(bytes: &[u8]) -> bool {
    run_clipboard_command(
        &ClipboardCommand {
            program: "pbcopy",
            args: &[],
        },
        bytes,
    )
}

pub fn read_clipboard_text() -> Option<String> {
    const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

    let mut child = Command::new("pbpaste")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let read = match read_limited_reader(stdout, MAX_CLIPBOARD_TEXT_BYTES) {
        Ok(LimitedRead::Oversized) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Ok(read) => read,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }
    match read {
        LimitedRead::Complete(bytes) => String::from_utf8(bytes).ok(),
        LimitedRead::Empty => None,
        LimitedRead::Oversized => unreachable!("oversized clipboard text is handled before wait"),
    }
}

pub fn clipboard_text_matches(_bytes: &[u8]) -> Option<bool> {
    None
}

pub fn open_url(url: &str) -> std::io::Result<Option<std::process::Child>> {
    Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Some)
}

pub fn read_clipboard_image() -> Option<ClipboardImage> {
    let path = std::env::temp_dir().join(format!(
        "herdr-clipboard-image-{}-{}.png",
        std::process::id(),
        unique_timestamp_nanos()
    ));
    let script = format!(
        "set png_data to (the clipboard as «class PNGf»)\nset fp to open for access POSIX file \"{}\" with write permission\nwrite png_data to fp\nclose access fp",
        path.display()
    );

    let status = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;

    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return None;
    }

    let bytes = match std::fs::File::open(&path).ok().and_then(|file| {
        read_limited_reader(file, crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD).ok()
    }) {
        Some(LimitedRead::Complete(bytes)) => bytes,
        Some(LimitedRead::Empty | LimitedRead::Oversized) | None => {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    let _ = std::fs::remove_file(&path);
    Some(ClipboardImage {
        bytes,
        extension: "png",
    })
}

fn unique_timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Private callback endpoint used by the local thin client for actionable
/// desktop notifications. The endpoint never crosses an SSH boundary: its
/// callback is translated into a message on the client's existing connection.
pub(crate) struct NotificationActivationEndpoint {
    socket_path: PathBuf,
    directory: PathBuf,
    stop: Arc<AtomicBool>,
    listener_thread: Option<std::thread::JoinHandle<()>>,
}

impl NotificationActivationEndpoint {
    pub(crate) fn start(on_activation: impl Fn(u64) + Send + 'static) -> std::io::Result<Self> {
        let directory = create_notification_endpoint_directory()?;
        let socket_path = directory.join("activate.sock");
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(err) => {
                let _ = std::fs::remove_dir(&directory);
                return Err(err);
            }
        };
        if let Err(err) =
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        {
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_dir(&directory);
            return Err(err);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let listener_stop = Arc::clone(&stop);
        let listener_thread = match std::thread::Builder::new()
            .name("herdr-notification-activation".to_owned())
            .spawn(move || loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if listener_stop.load(Ordering::Acquire) {
                            break;
                        }
                        if let Some(activation_id) = read_notification_activation(stream) {
                            on_activation(activation_id);
                        }
                    }
                    Err(_) if listener_stop.load(Ordering::Acquire) => break,
                    Err(_) => std::thread::sleep(Duration::from_millis(25)),
                }
            }) {
            Ok(thread) => thread,
            Err(err) => {
                let _ = std::fs::remove_file(&socket_path);
                let _ = std::fs::remove_dir(&directory);
                return Err(err);
            }
        };

        Ok(Self {
            socket_path,
            directory,
            stop,
            listener_thread: Some(listener_thread),
        })
    }

    pub(crate) fn callback_command(&self, activation_id: u64) -> String {
        notification_callback_command(&self.socket_path, activation_id)
    }
}

impl Drop for NotificationActivationEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn create_notification_endpoint_directory() -> std::io::Result<PathBuf> {
    for _ in 0..8 {
        let counter = NOTIFICATION_ENDPOINT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from("/tmp").join(format!(
            "herdr-notify-{}-{counter:x}-{:x}",
            std::process::id(),
            unique_timestamp_nanos()
        ));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&directory) {
            Ok(()) => return Ok(directory),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate notification activation endpoint",
    ))
}

fn read_notification_activation(stream: UnixStream) -> Option<u64> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut line = String::new();
    let mut reader = BufReader::new(stream).take(NOTIFICATION_ACTIVATION_READ_LIMIT);
    let read = reader.read_line(&mut line).ok()?;
    if read == 0 || !line.ends_with('\n') {
        return None;
    }
    let value = line.trim_end_matches(['\r', '\n']);
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().filter(|value| *value != 0)
}

fn notification_callback_command(socket_path: &Path, activation_id: u64) -> String {
    format!(
        "/usr/bin/printf '%s\\n' {} | /usr/bin/nc -w 1 -U {}",
        shell_single_quote(&activation_id.to_string()),
        shell_single_quote(&socket_path.to_string_lossy())
    )
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Show a native macOS notification.
///
/// Prefer `terminal-notifier` when it is installed because it can activate the
/// hosting terminal on click. Generic notifications retain the AppleScript
/// fallback; pane-targeted notifications use the separate actionable path
/// below so clicking a completion can never open Script Editor.
pub fn show_desktop_notification(title: &str, body: Option<&str>) -> std::io::Result<bool> {
    show_desktop_notification_with_command(title, body, |program| Command::new(program))
}

fn show_desktop_notification_with_command(
    title: &str,
    body: Option<&str>,
    mut command: impl FnMut(&str) -> Command,
) -> std::io::Result<bool> {
    if show_terminal_notifier_notification(title, body, &mut command).unwrap_or(false) {
        return Ok(true);
    }

    show_osascript_notification(title, body, &mut command)
}

pub(crate) fn show_actionable_desktop_notification(
    title: &str,
    body: Option<&str>,
    callback_command: &str,
) -> std::io::Result<bool> {
    let mut command = |program: &str| Command::new(program);
    if verified_terminal_bundle_identifier(&mut command).is_none() {
        return Ok(false);
    }
    show_terminal_notifier_notification_with_options(
        title,
        body,
        None,
        Some(callback_command),
        &mut command,
    )
}

/// Bring the verified host terminal application forward after the server has
/// focused the notification's pane.
pub(crate) fn activate_host_terminal() -> std::io::Result<bool> {
    let mut command = |program: &str| Command::new(program);
    activate_host_terminal_with_command(&mut command)
}

fn activate_host_terminal_with_command(
    command: &mut impl FnMut(&str) -> Command,
) -> std::io::Result<bool> {
    let Some(bundle_id) = verified_terminal_bundle_identifier(command) else {
        return Ok(false);
    };
    activate_terminal_bundle_with_command(&bundle_id, command)
}

fn activate_terminal_bundle_with_command(
    bundle_id: &str,
    command: &mut impl FnMut(&str) -> Command,
) -> std::io::Result<bool> {
    let mut cmd = command("/usr/bin/open");
    cmd.arg("-b").arg(bundle_id);
    run_notification_command(cmd)
}

fn show_terminal_notifier_notification(
    title: &str,
    body: Option<&str>,
    command: &mut impl FnMut(&str) -> Command,
) -> std::io::Result<bool> {
    let activate_bundle_id = verified_terminal_bundle_identifier(command);
    show_terminal_notifier_notification_with_options(
        title,
        body,
        activate_bundle_id.as_deref(),
        None,
        command,
    )
}

fn show_terminal_notifier_notification_with_options(
    title: &str,
    body: Option<&str>,
    activate_bundle_id: Option<&str>,
    callback_command: Option<&str>,
    command: &mut impl FnMut(&str) -> Command,
) -> std::io::Result<bool> {
    let mut cmd = command("terminal-notifier");
    build_terminal_notifier_command(&mut cmd, title, body, activate_bundle_id, callback_command);
    run_notification_command(cmd)
}

fn build_terminal_notifier_command(
    cmd: &mut Command,
    title: &str,
    body: Option<&str>,
    activate_bundle_id: Option<&str>,
    callback_command: Option<&str>,
) {
    cmd.arg("-title").arg(title);
    cmd.arg("-message").arg(body.unwrap_or_default());
    if let Some(bundle_id) = activate_bundle_id {
        cmd.arg("-activate").arg(bundle_id);
    }
    if let Some(callback_command) = callback_command {
        cmd.arg("-execute").arg(callback_command);
    }
}

fn show_osascript_notification(
    title: &str,
    body: Option<&str>,
    command: &mut impl FnMut(&str) -> Command,
) -> std::io::Result<bool> {
    let mut cmd = command("/usr/bin/osascript");
    cmd.arg("-e")
        .arg("on run argv")
        .arg("-e")
        .arg("display notification (item 2 of argv) with title (item 1 of argv)")
        .arg("-e")
        .arg("end run")
        .arg(title)
        .arg(body.unwrap_or_default());
    run_notification_command(cmd)
}

fn verified_terminal_bundle_identifier(
    command: &mut impl FnMut(&str) -> Command,
) -> Option<String> {
    static BUNDLE_ID: OnceLock<Option<String>> = OnceLock::new();
    BUNDLE_ID
        .get_or_init(|| {
            let bundle_id = detected_terminal_bundle_identifier()?;
            bundle_identifier_available(bundle_id, command).then(|| bundle_id.to_owned())
        })
        .clone()
}

fn bundle_identifier_available(bundle_id: &str, command: &mut impl FnMut(&str) -> Command) -> bool {
    let query = format!("kMDItemCFBundleIdentifier == '{bundle_id}'");
    let output = command("mdfind")
        .arg(query)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(output) if output.status.success() => !output.stdout.is_empty(),
        _ => false,
    }
}

fn detected_terminal_bundle_identifier() -> Option<&'static str> {
    terminal_bundle_identifier_from_env(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
        std::env::var_os("ALACRITTY_WINDOW_ID").is_some(),
    )
}

fn terminal_bundle_identifier_from_env(
    term_program: Option<&str>,
    term: Option<&str>,
    has_kitty_window_id: bool,
    has_alacritty_window_id: bool,
) -> Option<&'static str> {
    match term_program {
        Some("ghostty") => return Some("com.mitchellh.ghostty"),
        Some("iTerm.app") => return Some("com.googlecode.iterm2"),
        Some("WezTerm") => return Some("com.github.wez.wezterm"),
        Some("Apple_Terminal") => return Some("com.apple.Terminal"),
        _ => {}
    }

    if has_kitty_window_id || term == Some("xterm-kitty") {
        return Some("net.kovidgoyal.kitty");
    }
    if has_alacritty_window_id {
        return Some("org.alacritty");
    }

    None
}

fn run_notification_command(mut command: Command) -> std::io::Result<bool> {
    let status = match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };

    Ok(status.success())
}

fn run_clipboard_command(command: &ClipboardCommand, bytes: &[u8]) -> bool {
    let mut child = match Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };

    if stdin.write_all(bytes).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    drop(stdin);

    child.wait().map(|status| status.success()).unwrap_or(false)
}

/// Start time of `pid` in microseconds. It tells a process apart from a later
/// one that reuses its pid.
pub fn process_start_token(pid: u32) -> Option<u64> {
    process_bsdinfo(pid).map(|info| bsdinfo_start_token(&info))
}

pub(crate) fn process_started_at(start_token: u64) -> Option<std::time::SystemTime> {
    std::time::UNIX_EPOCH.checked_add(Duration::from_micros(start_token))
}

/// Process group of `pid` while that same process, matched by its start
/// token, is alive on the terminal of the pane shell `shell_pid`. A stopped or
/// backgrounded job still counts.
pub fn live_pane_process_group(shell_pid: u32, pid: u32, start_token: u64) -> Option<u32> {
    let process = process_bsdinfo(pid)?;
    let shell = process_bsdinfo(shell_pid)?;
    (process.pbi_status != libc::SZOMB
        && bsdinfo_start_token(&process) == start_token
        && process.e_tdev == shell.e_tdev)
        .then_some(process.pbi_pgid)
}

pub(crate) fn resume_owner_uid(pid: u32) -> Option<u32> {
    let info = process_bsdinfo(pid)?;
    Some(info.pbi_uid)
}

fn bsdinfo_start_token(info: &libc::proc_bsdinfo) -> u64 {
    info.pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec)
}

pub(super) fn socket_peer_pid(fd: RawFd) -> Option<u32> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    (result == 0 && pid > 0).then_some(pid as u32)
}

pub(super) fn process_name_and_parent(pid: u32) -> Option<(String, u32)> {
    let info = process_bsdinfo(pid)?;
    Some((comm_from_bsdinfo(&info)?, info.pbi_ppid))
}

fn process_bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;

    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };

    (ret == size).then_some(info)
}

fn comm_from_bsdinfo(info: &libc::proc_bsdinfo) -> Option<String> {
    let end = info
        .pbi_comm
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(info.pbi_comm.len());
    if end == 0 {
        return None;
    }

    let bytes: Vec<u8> = info.pbi_comm[..end].iter().map(|&b| b as u8).collect();
    String::from_utf8(bytes).ok()
}

fn process_argv(pid: u32) -> Option<Vec<String>> {
    let buf = kern_procargs2(pid)?;
    procargs2_argv(&buf)
}

/// Read a Herdr agent identity hint from a process environment.
pub fn process_agent_hint(pid: u32) -> Option<crate::detect::Agent> {
    if pid == 0 {
        return None;
    }
    let buf = kern_procargs2(pid)?;
    super::parse_agent_env_hint(procargs2_env(&buf)?)
}

fn procargs2_argv_start(rest: &[u8]) -> Option<usize> {
    let exec_end = rest.iter().position(|&byte| byte == 0)?;
    let mut pos = exec_end;
    while pos < rest.len() && rest[pos] == 0 {
        pos += 1;
    }
    (pos < rest.len()).then_some(pos)
}

fn skip_nul_strings(bytes: &[u8], start: usize, count: usize) -> Option<usize> {
    let mut current = start;
    for _ in 0..count {
        let end = bytes.get(current..)?.iter().position(|&byte| byte == 0)?;
        current = current.checked_add(end)?.checked_add(1)?;
    }
    Some(current)
}

fn procargs2_argv(buf: &[u8]) -> Option<Vec<String>> {
    if buf.len() < 4 {
        return None;
    }

    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if argc < 1 {
        return None;
    }

    // Layout: [argc: i32] [exec_path\0] [padding\0...] [argv[0]\0] ... [env\0] ...
    let rest = &buf[4..];
    let mut current = procargs2_argv_start(rest)?;
    let mut argv = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        if current >= rest.len() {
            return None;
        }
        let end = rest[current..]
            .iter()
            .position(|&b| b == 0)
            .map(|offset| current + offset)
            .unwrap_or(rest.len());
        if end == current {
            return None;
        }
        argv.push(String::from_utf8_lossy(&rest[current..end]).into_owned());
        current = end + 1;
    }

    Some(argv)
}

fn procargs2_env(buf: &[u8]) -> Option<&[u8]> {
    if buf.len() < 4 {
        return None;
    }

    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if argc < 1 {
        return None;
    }

    let rest = &buf[4..];
    let argv_start = procargs2_argv_start(rest)?;
    let env_start = skip_nul_strings(rest, argv_start, argc as usize)?;
    rest.get(env_start..)
}

/// Get the current working directory of a process.
///
/// Uses `proc_pidinfo(PROC_PIDVNODEPATHINFO)` to read `pvi_cdir.vip_path`.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }

    let mut pathinfo: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;

    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut pathinfo as *mut _ as *mut libc::c_void,
            size,
        )
    };

    if ret != size {
        return None;
    }

    // vip_path is [[c_char; 32]; 32] in libc (workaround for old Rust const generics).
    // Reinterpret as flat bytes (total MAXPATHLEN = 1024).
    let vip_path = unsafe {
        std::slice::from_raw_parts(
            pathinfo.pvi_cdir.vip_path.as_ptr() as *const u8,
            libc::MAXPATHLEN as usize,
        )
    };

    let nul = vip_path.iter().position(|&b| b == 0)?;
    if nul == 0 {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&vip_path[..nul])))
}

pub fn session_processes(child_pid: u32) -> Vec<u32> {
    if child_pid == 0 {
        return Vec::new();
    }

    let target_session = unsafe { libc::getsid(child_pid as libc::c_int) };
    if target_session <= 0 {
        return Vec::new();
    }

    all_pids()
        .into_iter()
        .filter(|pid| unsafe { libc::getsid(*pid as libc::pid_t) } == target_session)
        .collect()
}

fn all_pids() -> Vec<u32> {
    let initial_count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut capacity = if initial_count > 0 {
        initial_count as usize + 128
    } else {
        4096
    };

    for _ in 0..8 {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr() as *mut libc::c_void,
                (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
            )
        };
        if count <= 0 {
            return Vec::new();
        }

        let count = count as usize;
        if count < capacity {
            return collect_positive_pids(pids, count);
        }
        capacity = capacity.saturating_mul(2);
    }

    Vec::new()
}

fn collect_positive_pids(pids: Vec<libc::pid_t>, count: usize) -> Vec<u32> {
    pids.into_iter()
        .take(count)
        .filter(|pid| *pid > 0)
        .map(|pid| pid as u32)
        .collect()
}

pub fn signal_processes(pids: &[u32], signal: Signal) {
    let sig = match signal {
        Signal::Hangup => libc::SIGHUP,
        Signal::Terminate => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };

    for &pid in pids {
        if pid == 0 {
            continue;
        }
        unsafe {
            libc::kill(pid as libc::c_int, sig);
        }
    }
}

pub fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::c_int, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_parses_macos_memory_usage() {
        let stats = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\n\
                     Pages free:                               100.\n\
                     Pages active:                             700.\n\
                     Pages inactive:                           150.\n\
                     Pages speculative:                         50.\n";
        assert_eq!(
            parse_memory_usage(4_096_000, stats),
            Some(crate::health::ResourceUsage {
                used_bytes: 2_867_200,
                total_bytes: 4_096_000,
            })
        );
    }

    #[test]
    fn health_parses_macos_battery_percentage_and_state() {
        assert_eq!(
            parse_battery_health(
                "Now drawing from 'Battery Power'\n -InternalBattery-0 83%; discharging; 4:12 remaining\n"
            ),
            Some(crate::health::BatteryHealth {
                percentage: 83,
                charging: Some(false),
            })
        );
        assert_eq!(
            parse_battery_health(
                "Now drawing from 'AC Power'\n -InternalBattery-0 96%; charging; 0:20 remaining\n"
            ),
            Some(crate::health::BatteryHealth {
                percentage: 96,
                charging: Some(true),
            })
        );
    }

    #[test]
    fn health_parses_macos_battery_temperature() {
        assert_eq!(
            parse_battery_temperature(
                "{\n  \"Temperature\" = 3073\n  \"VirtualTemperature\" = 3409\n}\n"
            ),
            Some(34)
        );
        assert_eq!(parse_battery_temperature("{\n}\n"), None);
    }

    #[test]
    fn nofile_target_raises_low_soft_limit_to_cap_when_hard_is_unlimited() {
        assert_eq!(
            target_nofile_soft_limit(256, libc::RLIM_INFINITY, 8192),
            Some(8192)
        );
    }

    #[test]
    fn nofile_target_respects_finite_hard_limit() {
        assert_eq!(target_nofile_soft_limit(256, 4096, 8192), Some(4096));
    }

    #[test]
    fn nofile_target_does_not_lower_existing_soft_limit() {
        assert_eq!(
            target_nofile_soft_limit(16_384, libc::RLIM_INFINITY, 8192),
            None
        );
    }

    fn build_procargs2(exec_path: &str, argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(argv.len() as i32).to_ne_bytes());
        buf.extend_from_slice(exec_path.as_bytes());
        buf.push(0);
        buf.push(0);
        for arg in argv {
            buf.extend_from_slice(arg.as_bytes());
            buf.push(0);
        }
        for entry in env {
            buf.extend_from_slice(entry.as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn procargs2_argv_excludes_environment_entries() {
        let buf = build_procargs2(
            "/usr/bin/node",
            &["node", "/Users/can/.local/bin/pi"],
            &[
                "PATH=/usr/bin:/var/run/com.apple.security.cryptexd/codex.system/bootstrap/usr/bin",
                "TERM=tmux-256color",
            ],
        );

        let argv = procargs2_argv(&buf).expect("expected argv");
        assert_eq!(argv, vec!["node", "/Users/can/.local/bin/pi"]);
        assert_eq!(argv.join(" "), "node /Users/can/.local/bin/pi");
        assert!(!argv.join(" ").contains("codex.system"));
    }

    #[test]
    fn procargs2_env_reads_agent_hint_after_argv() {
        let buf = build_procargs2(
            "/opt/homebrew/bin/nono",
            &["nono", "run", "HERDR_AGENT=codex", "--", "claude"],
            &["PATH=/usr/bin", "HERDR_AGENT=claude", "TERM=xterm-256color"],
        );

        let env = procargs2_env(&buf).expect("expected env block");
        assert_eq!(
            crate::platform::parse_agent_env_hint(env),
            Some(crate::detect::Agent::Claude)
        );
    }

    #[test]
    fn procargs2_env_does_not_treat_argv_as_environment() {
        let buf = build_procargs2(
            "/opt/homebrew/bin/nono",
            &["nono", "run", "HERDR_AGENT=claude"],
            &["PATH=/usr/bin"],
        );

        let env = procargs2_env(&buf).expect("expected env block");
        assert_eq!(crate::platform::parse_agent_env_hint(env), None);
    }

    #[test]
    fn terminal_bundle_identifier_maps_known_terminal_env() {
        assert_eq!(
            terminal_bundle_identifier_from_env(Some("ghostty"), None, false, false),
            Some("com.mitchellh.ghostty")
        );
        assert_eq!(
            terminal_bundle_identifier_from_env(Some("iTerm.app"), None, false, false),
            Some("com.googlecode.iterm2")
        );
        assert_eq!(
            terminal_bundle_identifier_from_env(Some("WezTerm"), None, false, false),
            Some("com.github.wez.wezterm")
        );
        assert_eq!(
            terminal_bundle_identifier_from_env(Some("Apple_Terminal"), None, false, false),
            Some("com.apple.Terminal")
        );
        assert_eq!(
            terminal_bundle_identifier_from_env(None, Some("xterm-kitty"), false, false),
            Some("net.kovidgoyal.kitty")
        );
        assert_eq!(
            terminal_bundle_identifier_from_env(None, None, true, false),
            Some("net.kovidgoyal.kitty")
        );
        assert_eq!(
            terminal_bundle_identifier_from_env(None, None, false, true),
            Some("org.alacritty")
        );
        assert_eq!(
            terminal_bundle_identifier_from_env(None, None, false, false),
            None
        );
    }

    #[test]
    fn actionable_terminal_notifier_executes_without_early_activation() {
        let mut cmd = Command::new("terminal-notifier");
        build_terminal_notifier_command(
            &mut cmd,
            "pi finished",
            Some("workspace 1"),
            None,
            Some("notify callback"),
        );
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                "-title",
                "pi finished",
                "-message",
                "workspace 1",
                "-execute",
                "notify callback"
            ]
        );
    }

    #[test]
    fn host_terminal_activation_uses_bundle_id() {
        let mut command = |program: &str| {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("test \"$0\" = /usr/bin/open && test \"$1\" = -b && test \"$2\" = com.mitchellh.ghostty")
                .arg(program);
            cmd
        };

        assert!(
            activate_terminal_bundle_with_command("com.mitchellh.ghostty", &mut command)
                .expect("host terminal activation command")
        );
    }

    #[test]
    fn terminal_notifier_success_skips_osascript() {
        let path = std::env::temp_dir().join(format!(
            "herdr-terminal-notifier-args-{}",
            std::process::id()
        ));
        let script = "printf '%s:%s\\n' \"$0\" \"$*\" >> \"$HERDR_NOTIFY_ARGS\"";
        let mut command = |program: &str| {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(script)
                .arg(program)
                .env("HERDR_NOTIFY_ARGS", &path);
            cmd
        };

        let shown = show_terminal_notifier_notification_with_options(
            "title",
            Some("body"),
            Some("com.mitchellh.ghostty"),
            None,
            &mut command,
        )
        .expect("terminal-notifier command should run");

        assert!(shown);
        let args = std::fs::read_to_string(&path).expect("args file");
        let _ = std::fs::remove_file(&path);
        assert!(args.starts_with("terminal-notifier:"), "{args}");
        assert!(args.contains("-activate com.mitchellh.ghostty"), "{args}");
        assert!(!args.contains("osascript"), "{args}");
    }

    #[test]
    fn generic_desktop_notification_falls_back_to_osascript() {
        let path =
            std::env::temp_dir().join(format!("herdr-osascript-args-{}", std::process::id()));
        let script = r#"
if [ "$0" = "terminal-notifier" ]; then
  exit 1
fi
printf '%s\n' "$@" > "$HERDR_NOTIFY_ARGS"
"#;
        let mut command = |program: &str| {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(script)
                .arg(program)
                .env("HERDR_NOTIFY_ARGS", &path);
            cmd
        };
        let shown = show_desktop_notification_with_command("title", Some("body"), &mut command)
            .expect("osascript fallback should run");

        assert!(shown);
        let args = std::fs::read_to_string(&path).expect("osascript args");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            args,
            "-e\non run argv\n-e\ndisplay notification (item 2 of argv) with title (item 1 of argv)\n-e\nend run\ntitle\nbody\n"
        );
    }

    #[test]
    fn notification_callback_command_shell_quotes_socket_path() {
        let command = notification_callback_command(Path::new("/tmp/herdr's socket"), 42);
        assert_eq!(
            command,
            "/usr/bin/printf '%s\\n' '42' | /usr/bin/nc -w 1 -U '/tmp/herdr'\\''s socket'"
        );
    }

    #[test]
    fn notification_activation_endpoint_accepts_one_numeric_id() {
        let (tx, rx) = std::sync::mpsc::channel();
        let endpoint = NotificationActivationEndpoint::start(move |activation_id| {
            let _ = tx.send(activation_id);
        })
        .expect("activation endpoint");
        let directory = endpoint.directory.clone();
        let socket_path = endpoint.socket_path.clone();
        assert_eq!(
            std::fs::metadata(&directory)
                .expect("endpoint directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&socket_path)
                .expect("endpoint socket metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut stream =
            UnixStream::connect(&endpoint.socket_path).expect("connect activation endpoint");
        stream.write_all(b"73\n").expect("write activation id");
        drop(stream);

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("activation callback"),
            73
        );
        drop(endpoint);
        assert!(!socket_path.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn notification_activation_reader_rejects_malformed_values() {
        fn read_value(value: &[u8]) -> Option<u64> {
            let (mut writer, reader) = UnixStream::pair().expect("socket pair");
            writer.write_all(value).expect("write test value");
            drop(writer);
            read_notification_activation(reader)
        }

        assert_eq!(read_value(b"91\n"), Some(91));
        assert_eq!(read_value(b"0\n"), None);
        assert_eq!(read_value(b" 91\n"), None);
        assert_eq!(read_value(b"91"), None);
        assert_eq!(read_value(b"not-a-token\n"), None);
    }

    #[test]
    fn scrollback_editor_argv_preserves_unix_editor_shell_semantics() {
        let path = std::path::Path::new("/tmp/herdr scrollback.txt");
        let argv = scrollback_editor_argv(path).unwrap();

        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "-c");
        assert!(argv[2].contains("EDITOR:-vi"));
        assert!(argv[2].contains("/tmp/herdr scrollback.txt"));
    }
}
