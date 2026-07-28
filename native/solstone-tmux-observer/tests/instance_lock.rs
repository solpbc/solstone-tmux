// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::os::unix::fs::MetadataExt;

use solstone_tmux_observer::cli::{CliCommand, command_requires_instance_lock};
use solstone_tmux_observer::instance_lock::{InstanceLock, InstanceLockError, LOCK_FILENAME};
use solstone_tmux_observer::paths::ensure_private_directory;
use support::TestDirectory;

#[test]
fn second_run_lock_fails_before_any_observer_side_effect() {
    let temporary = TestDirectory::new("second-lock");
    ensure_private_directory(temporary.path()).expect("private data root");
    let first = InstanceLock::acquire(temporary.path()).expect("first independent handle");

    let error = InstanceLock::acquire(temporary.path()).expect_err("second handle must fail");

    assert!(matches!(error, InstanceLockError::AlreadyLocked(_)));
    assert!(!temporary.path().join("captures").exists());
    assert!(!temporary.path().join("indicator-mutated").exists());
    drop(first);
}

#[test]
fn crash_release_allows_immediate_recovery() {
    let temporary = TestDirectory::new("crash-release");
    let first = InstanceLock::acquire(temporary.path()).expect("first lock");
    drop(first);

    let second = InstanceLock::acquire(temporary.path()).expect("immediate second lock");
    assert_eq!(second.path(), temporary.path().join(LOCK_FILENAME));
}

#[test]
fn release_never_unlinks_lock_inode() {
    let temporary = TestDirectory::new("persistent-inode");
    let lock = InstanceLock::acquire(temporary.path()).expect("lock");
    let inode = lock.inode();
    let path = lock.path().to_owned();
    drop(lock);

    assert!(path.is_file());
    assert_eq!(fs::metadata(path).expect("lock metadata").ino(), inode);
}

#[test]
fn status_does_not_lock() {
    let temporary = TestDirectory::new("status-no-lock");
    let _writer = InstanceLock::acquire(temporary.path()).expect("writer lock");

    assert!(!command_requires_instance_lock(CliCommand::Status));
    assert!(!temporary.path().join("status-lock").exists());
}

#[test]
fn service_install_stop_and_uninstall_do_not_lock() {
    let temporary = TestDirectory::new("service-no-lock");
    let _writer = InstanceLock::acquire(temporary.path()).expect("writer lock");

    assert!(!command_requires_instance_lock(CliCommand::InstallService));
    assert!(!command_requires_instance_lock(
        CliCommand::UninstallService
    ));
    assert!(!temporary.path().join("service-lock").exists());
}
