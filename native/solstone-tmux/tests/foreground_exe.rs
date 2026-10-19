// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::io;
use std::path::{Path, PathBuf};

use solstone_tmux::foreground::{
    ExeFailure, exe_basename, linux_exe_link, linux_exe_path_for_stat, linux_foreground_pgrp,
    macos_foreground_pid, map_io_error,
};

#[test]
fn foreground_exe_linux_link_is_proc_exe_basename() {
    let stat = b"100 (sleep 999 (not-exe)) S 1 100 50 34816 200 0 0\n";
    assert_eq!(linux_foreground_pgrp(stat), Ok(200));
    let link = linux_exe_path_for_stat(stat).expect("foreground executable path");
    assert_eq!(link, Path::new("/proc/200/exe"));
    assert_ne!(link, Path::new("/proc/100/exe"));
    assert_ne!(link, Path::new("/proc/999/exe"));
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
    for stat in [
        b"100 (bash) S 1 100 100 34816 -1 0\n".as_slice(),
        b"100 (bash) S 1 100 100 34816 0 0\n".as_slice(),
        b"100 bash S 1 100 100 34816 200 0\n".as_slice(),
        b"100 (bash) S 1 100 100 34816\n".as_slice(),
    ] {
        assert_eq!(linux_foreground_pgrp(stat), Err(ExeFailure::Unavailable));
        assert_eq!(linux_exe_path_for_stat(stat), Err(ExeFailure::Unavailable));
    }
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
fn foreground_exe_macos_tpgid_selects_foreground() {
    let shell_pid = 100;
    let foreground_pid = macos_foreground_pid(200).expect("foreground process group");
    assert_eq!(foreground_pid, 200);
    assert_ne!(foreground_pid, shell_pid);
}

#[test]
fn foreground_exe_macos_errors() {
    assert_eq!(macos_foreground_pid(0), Err(ExeFailure::Unavailable));
    assert_eq!(macos_foreground_pid(u32::MAX), Err(ExeFailure::Unavailable));
}
