// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;

use solstone_tmux_observer::paths::{
    Environment, PathError, PlatformKind, PlatformPaths, ensure_private_directory,
};
use solstone_tmux_observer::storage::{StorageError, atomic_write_bytes};
use support::TestDirectory;

#[test]
fn linux_xdg_paths() {
    let environment = FakeEnvironment::from([
        ("HOME", "/owner"),
        ("XDG_DATA_HOME", "/data"),
        ("XDG_CONFIG_HOME", "/config"),
    ]);
    let paths = PlatformPaths::resolve(PlatformKind::Linux, &environment).expect("Linux paths");
    assert_eq!(paths.data_root, std::path::Path::new("/data/solstone-tmux"));
    assert_eq!(
        paths.config_root,
        std::path::Path::new("/config/solstone-tmux")
    );
}

#[test]
fn linux_home_fallback_paths() {
    let environment = FakeEnvironment::from([
        ("HOME", "/owner"),
        ("XDG_DATA_HOME", ""),
        ("XDG_CONFIG_HOME", ""),
    ]);
    let paths = PlatformPaths::resolve(PlatformKind::Linux, &environment).expect("Linux paths");
    assert_eq!(
        paths.data_root,
        std::path::Path::new("/owner/.local/share/solstone-tmux")
    );
    assert_eq!(
        paths.config_root,
        std::path::Path::new("/owner/.config/solstone-tmux")
    );
}

#[test]
fn macos_application_support_paths() {
    let environment = FakeEnvironment::from([("HOME", "/Users/owner")]);
    let paths = PlatformPaths::resolve(PlatformKind::Macos, &environment).expect("macOS paths");
    let expected = std::path::Path::new("/Users/owner/Library/Application Support/solstone-tmux");
    assert_eq!(paths.data_root, expected);
    assert_eq!(paths.config_root, expected);
}

#[test]
fn missing_home_fails_without_system_fallback() {
    let environment = FakeEnvironment::default();
    assert!(matches!(
        PlatformPaths::resolve(PlatformKind::Linux, &environment),
        Err(PathError::MissingHome)
    ));
    assert!(matches!(
        PlatformPaths::resolve(PlatformKind::Macos, &environment),
        Err(PathError::MissingHome)
    ));
}

#[test]
fn directories_are_0700_independent_of_umask() {
    let temporary = TestDirectory::new("directory-mode");
    let parent = temporary.path().join("private");
    let directory = parent.join("nested");

    ensure_private_directory(&directory).expect("secure directory");

    for path in [&parent, &directory] {
        let mode = fs::metadata(path)
            .expect("directory metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[test]
fn files_are_0600_independent_of_umask() {
    let temporary = TestDirectory::new("file-mode");
    let file = temporary.path().join("state.json");
    fs::write(&file, b"old").expect("write old file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).expect("set permissive mode");

    atomic_write_bytes(&file, temporary.path(), b"new").expect("replace private file");

    let metadata = fs::metadata(&file).expect("file metadata");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read(file).expect("file bytes"), b"new");
}

#[test]
fn symlink_targets_are_rejected_without_referent_change() {
    let temporary = TestDirectory::new("symlink");
    let referent = temporary.path().join("referent");
    let link = temporary.path().join("link");
    fs::write(&referent, b"owner data").expect("write referent");
    symlink(&referent, &link).expect("create symlink");

    assert!(matches!(
        atomic_write_bytes(&link, temporary.path(), b"replacement"),
        Err(StorageError::InvalidTarget(path)) if path == link
    ));
    assert_eq!(fs::read(referent).expect("referent bytes"), b"owner data");
}

#[test]
fn nonregular_targets_are_rejected() {
    let temporary = TestDirectory::new("nonregular");
    let socket = temporary.path().join("socket");
    let _listener = UnixListener::bind(&socket).expect("bind test socket");
    assert!(matches!(
        atomic_write_bytes(&socket, temporary.path(), b"replacement"),
        Err(StorageError::InvalidTarget(path)) if path == socket
    ));

    let ordinary_file = temporary.path().join("not-a-directory");
    fs::write(&ordinary_file, b"data").expect("write ordinary file");
    assert!(matches!(
        ensure_private_directory(&ordinary_file),
        Err(PathError::InvalidTarget(path)) if path == ordinary_file
    ));
}

#[test]
fn fake_environment_is_process_independent() {
    let first = FakeEnvironment::from([("HOME", "/first")]);
    let second = FakeEnvironment::from([("HOME", "/second")]);
    let first_paths = PlatformPaths::resolve(PlatformKind::Linux, &first).expect("first paths");
    let second_paths = PlatformPaths::resolve(PlatformKind::Linux, &second).expect("second paths");
    assert!(first_paths.data_root.starts_with("/first"));
    assert!(second_paths.data_root.starts_with("/second"));
}

#[derive(Default)]
struct FakeEnvironment(HashMap<String, OsString>);

impl<const N: usize> From<[(&str, &str); N]> for FakeEnvironment {
    fn from(entries: [(&str, &str); N]) -> Self {
        Self(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), OsString::from(value)))
                .collect(),
        )
    }
}

impl Environment for FakeEnvironment {
    fn var_os(&self, key: &str) -> Option<OsString> {
        self.0.get(key).cloned()
    }
}
