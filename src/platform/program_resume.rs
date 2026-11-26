use std::fs::File;

// Check the open descriptor, so the authority applies to the bytes actually read.
#[cfg(unix)]
pub(crate) fn private_snapshot(file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    // SAFETY: geteuid takes no arguments and has no preconditions.
    metadata.is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
        && private_snapshot_acl(file)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn private_snapshot_acl(_file: &File) -> bool {
    // Linux includes the POSIX ACL mask in the file's group permission bits.
    cfg!(target_os = "linux")
}

#[cfg(target_os = "macos")]
fn private_snapshot_acl(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    crate::platform::macos::file_has_no_extended_acl(file.as_raw_fd())
}

#[cfg(not(unix))]
pub(crate) fn private_snapshot(_file: &File) -> bool {
    false
}
