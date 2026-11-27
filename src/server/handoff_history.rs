//! Full history transferred through anonymous files outside the bounded manifest.
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

/// Files are unlinked before writing any terminal content. The kernel cleans
/// them up on success, rollback, or process death; no plaintext is left behind.
pub(super) struct HistoryFiles {
    directory: PathBuf,
    token: String,
    files: Vec<(u32, File)>,
}

impl HistoryFiles {
    pub(super) fn new(socket: &Path, token: String) -> io::Result<Self> {
        Ok(Self {
            directory: socket
                .parent()
                .ok_or_else(|| io::Error::other("handoff socket has no directory"))?
                .to_owned(),
            token,
            files: Vec::new(),
        })
    }

    pub(super) fn store(&mut self, pane_id: u32, ansi: &str) -> io::Result<()> {
        let path = self.directory.join(format!(
            "herdr-handoff-history-{}-{pane_id}.ansi",
            self.token
        ));
        // Suppress inherited ACLs at creation as well as setting private modes.
        let mut writer = crate::platform::create_config_temporary(&path, true)?;
        let reader = crate::platform::open_state_file_no_follow(&path);
        // Always unlink even when opening the reader failed. No content has
        // been written yet, so interruption before unlink leaves an empty file.
        std::fs::remove_file(&path)?;
        let reader = reader?;
        if !crate::platform::same_anonymous_state_file(&writer, &reader)?
            || !crate::platform::program_resume::private_snapshot(&reader)
        {
            return Err(io::Error::other("handoff history file is not private"));
        }
        writer.write_all(ansi.as_bytes())?;
        writer.flush()?;
        self.files.push((pane_id, reader));
        Ok(())
    }

    pub(super) fn panes(&self) -> Vec<u32> {
        self.files.iter().map(|(pane, _)| *pane).collect()
    }

    pub(super) fn fds(&self) -> Vec<RawFd> {
        self.files
            .iter()
            .map(|(_, file)| file.as_raw_fd())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn full_history_exceeds_manifest_limit_without_leaving_named_files() {
        let directory =
            std::env::temp_dir().join(format!("herdr-anonymous-history-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let mut files = HistoryFiles::new(&directory.join("test.sock"), "test".into()).unwrap();
        let history = "first\n".to_owned() + &"x".repeat(17 * 1024 * 1024) + "\nlast\n";
        files.store(1, &history).unwrap();
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        let mut received = String::new();
        files.files[0].1.read_to_string(&mut received).unwrap();
        assert_eq!(received, history);
        drop(files);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir(directory).unwrap();
    }
}
