use super::*;

#[test]
fn claude_session_metadata_rejects_acl_write_access_on_files_and_directories() {
    use std::os::fd::AsRawFd;
    let dir = std::env::temp_dir().join(format!("herdr-session-acl-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let record = dir.join("record.json");
    std::fs::write(&record, b"{}").unwrap();
    for (path, rule) in [
        (&record, "everyone allow write"),
        (&dir, "everyone allow add_file"),
    ] {
        let file = std::fs::File::open(path).unwrap();
        assert!(file_has_no_extended_acl(file.as_raw_fd()));
        assert!(Command::new("/bin/chmod")
            .args(["+a", rule])
            .arg(path)
            .status()
            .unwrap()
            .success());
        assert!(!file_has_no_extended_acl(file.as_raw_fd()));
        assert!(Command::new("/bin/chmod")
            .arg("-N")
            .arg(path)
            .status()
            .unwrap()
            .success());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn config_replacement_preserves_macos_acl() {
    let dir = std::env::temp_dir().join(format!("herdr-config-acl-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let source = dir.join("source");
    let temporary = dir.join("temporary");
    std::fs::write(&source, b"original").unwrap();
    assert!(Command::new("chmod")
        .args(["+a", "everyone deny execute"])
        .arg(&source)
        .status()
        .unwrap()
        .success());
    let acl = |path: &Path| {
        let output = Command::new("ls").arg("-le").arg(path).output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let original = acl(&source);
    assert!(original.contains("everyone deny execute"));
    use std::os::fd::AsRawFd;
    let source_fd = std::fs::File::open(&source).unwrap();
    assert!(!file_has_no_extended_acl(source_fd.as_raw_fd()));
    assert!(Command::new("chmod")
        .args(["+a", "everyone allow read,file_inherit"])
        .arg(&dir)
        .status()
        .unwrap()
        .success());
    drop(create_config_temporary(&temporary, true).unwrap());
    assert!(
        acl(&temporary).is_empty(),
        "staging must not inherit allow ACEs"
    );
    let temporary_fd = std::fs::File::open(&temporary).unwrap();
    assert!(file_has_no_extended_acl(temporary_fd.as_raw_fd()));
    write_config_temporary(Some(&source), &temporary, b"new").unwrap();
    assert_eq!(acl(&temporary), original);
    assert_eq!(std::fs::read(&source).unwrap(), b"original");
    assert_eq!(std::fs::read(&temporary).unwrap(), b"new");
    std::fs::remove_dir_all(dir).unwrap();
}
