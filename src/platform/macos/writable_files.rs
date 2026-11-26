use std::ffi::OsString;
use std::mem::{size_of, size_of_val, MaybeUninit};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

// Darwin's <sys/proc_info.h>: libc exposes vnode_info_path and the proc calls,
// but not the enclosing vnode_fdinfowithpath or its proc_fileinfo header.
#[repr(C)]
struct FileInfo {
    flags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guard_flags: u32,
}

#[repr(C)]
struct VnodeFdPath {
    file: FileInfo,
    vnode: libc::vnode_info_path,
}

pub(crate) fn writable_file_paths(pid: u32) -> Option<Vec<PathBuf>> {
    let pid = i32::try_from(pid).ok()?;
    let mut descriptors = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        64
    ];
    loop {
        let bytes = i32::try_from(size_of_val(descriptors.as_slice())).ok()?;
        // SAFETY: buffer is writable for exactly bytes and holds proc_fdinfo entries.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                descriptors.as_mut_ptr().cast(),
                bytes,
            )
        };
        if filled <= 0 {
            return None;
        }
        if filled < bytes {
            descriptors.truncate(filled as usize / size_of::<libc::proc_fdinfo>());
            break;
        }
        if descriptors.len() >= 16384 {
            return None;
        }
        descriptors.resize(descriptors.len() * 2, descriptors[0]);
    }
    let mut paths = Vec::new();
    for fd in descriptors {
        if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
            continue;
        }
        let mut info = MaybeUninit::<VnodeFdPath>::uninit();
        let bytes = size_of::<VnodeFdPath>() as i32;
        // PROC_PIDFDVNODEPATHINFO = 2, FWRITE = 2. The kernel returns the byte count.
        // SAFETY: info is properly aligned and is read only following a complete write.
        let count =
            unsafe { libc::proc_pidfdinfo(pid, fd.proc_fd, 2, info.as_mut_ptr().cast(), bytes) };
        if count != bytes {
            continue;
        }
        let info = unsafe { info.assume_init() };
        if info.file.flags & 2 == 0 {
            continue;
        }
        let raw = info.vnode.vip_path.as_flattened();
        if let Some(end) = raw.iter().position(|&byte| byte == 0) {
            paths.push(PathBuf::from(OsString::from_vec(
                raw[..end].iter().map(|&b| b as u8).collect(),
            )));
        }
    }
    Some(paths)
}
