// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::io;
use std::path::{Path, PathBuf};

use solstone_tmux::foreground::{
    ExeFailure, exe_basename, linux_exe_link, macos_group_leader, map_io_error,
};

#[test]
fn foreground_exe_linux_link_is_proc_exe_basename() {
    let link = linux_exe_link(200);
    assert_eq!(link, Path::new("/proc/200/exe"));
    assert!(!link.ends_with("cmdline"));
    assert!(!link.ends_with("comm"));
    assert_eq!(
        exe_basename(Path::new("/bin/sleep")),
        Ok("sleep".to_owned())
    );
    assert!(!link.to_string_lossy().contains("100"));
    // argv is deliberately not accepted by the resolver, so rewritten argv0
    // cannot affect the returned foreground executable basename.
}

#[test]
fn foreground_exe_linux_errors() {
    for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotFound] {
        assert_eq!(
            map_io_error(&io::Error::from(kind)),
            ExeFailure::Unavailable
        );
    }
    assert_eq!(
        map_io_error(&io::Error::from(io::ErrorKind::Unsupported)),
        ExeFailure::Unsupported
    );
    for errno in [
        rustix::io::Errno::NOTSUP,
        rustix::io::Errno::NOSYS,
        rustix::io::Errno::OPNOTSUPP,
    ] {
        assert_eq!(
            map_io_error(&io::Error::from_raw_os_error(errno.raw_os_error())),
            ExeFailure::Unsupported
        );
    }
    assert_eq!(
        map_io_error(&io::Error::from_raw_os_error(
            rustix::io::Errno::NOTTY.raw_os_error()
        )),
        ExeFailure::Unavailable
    );
    assert_eq!(exe_basename(Path::new("/")), Err(ExeFailure::Unavailable));
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
        assert_eq!(exe_basename(&path), Err(ExeFailure::Unavailable));
    }
}

#[test]
fn foreground_exe_macos_leader_beats_pane_shell() {
    let shell_pid = 100;
    let foreground_pgrp = 200;
    let members = [shell_pid, foreground_pgrp];
    assert_eq!(
        macos_group_leader(foreground_pgrp, &members),
        Ok(foreground_pgrp)
    );
}

#[test]
fn foreground_exe_macos_missing_leader_does_not_use_shell() {
    let shell_pid = 100;
    assert_eq!(
        macos_group_leader(200, &[shell_pid]),
        Err(ExeFailure::Unavailable)
    );
}

#[test]
fn foreground_exe_macos_errors() {
    assert_eq!(macos_group_leader(200, &[]), Err(ExeFailure::Unavailable));
}
