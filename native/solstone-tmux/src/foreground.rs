// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExeFailure {
    Unavailable,
    Unsupported,
}

pub fn exe_basename(path: &Path) -> Result<String, ExeFailure> {
    let name = path.file_name().ok_or(ExeFailure::Unavailable)?;
    let name = name.to_str().ok_or(ExeFailure::Unavailable)?;
    if name.is_empty() {
        return Err(ExeFailure::Unavailable);
    }
    Ok(name.to_owned())
}

pub fn linux_exe_link(pgrp: u32) -> PathBuf {
    PathBuf::from(format!("/proc/{pgrp}/exe"))
}

pub fn map_io_error(error: &io::Error) -> ExeFailure {
    if matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
    ) {
        return ExeFailure::Unavailable;
    }
    let unsupported = [
        rustix::io::Errno::NOTSUP.raw_os_error(),
        rustix::io::Errno::NOSYS.raw_os_error(),
        rustix::io::Errno::OPNOTSUPP.raw_os_error(),
    ];
    if let Some(code) = error.raw_os_error() {
        return if unsupported.contains(&code) {
            ExeFailure::Unsupported
        } else {
            ExeFailure::Unavailable
        };
    }
    if error.kind() == io::ErrorKind::Unsupported {
        ExeFailure::Unsupported
    } else {
        ExeFailure::Unavailable
    }
}

pub fn macos_group_leader(pgrp: i32, members: &[i32]) -> Result<i32, ExeFailure> {
    members
        .iter()
        .copied()
        .find(|member| *member == pgrp)
        .ok_or(ExeFailure::Unavailable)
}

pub trait ForegroundLookup: Send + Sync {
    fn lookup<'a>(
        &'a self,
        tty: &'a str,
        shell_pid: u32,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExeFailure>> + Send + 'a>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLookup;

impl ForegroundLookup for SystemLookup {
    fn lookup<'a>(
        &'a self,
        tty: &'a str,
        shell_pid: u32,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExeFailure>> + Send + 'a>> {
        Box::pin(lookup_basename(tty, shell_pid))
    }
}

pub async fn lookup_basename(tty: &str, shell_pid: u32) -> Result<String, ExeFailure> {
    let tty = tty.to_owned();
    tokio::task::spawn_blocking(move || blocking_basename(&tty, shell_pid))
        .await
        .unwrap_or(Err(ExeFailure::Unavailable))
}

#[cfg(target_os = "linux")]
fn blocking_basename(tty: &str, _shell_pid: u32) -> Result<String, ExeFailure> {
    use std::fs::File;
    use std::os::unix::ffi::OsStringExt;

    let terminal = File::open(tty).map_err(|error| map_io_error(&error))?;
    let foreground_pgrp = rustix::termios::tcgetpgrp(&terminal)
        .map_err(|error| map_io_error(&io::Error::from(error)))?
        .as_raw_pid();
    let pgrp = u32::try_from(foreground_pgrp).map_err(|_| ExeFailure::Unavailable)?;
    let path = linux_exe_link(pgrp);
    let target = rustix::fs::readlink(&path, Vec::new())
        .map_err(|error| map_io_error(&io::Error::from(error)))?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(target.into_bytes()));
    exe_basename(&path)
}

#[cfg(target_os = "macos")]
fn blocking_basename(tty: &str, _shell_pid: u32) -> Result<String, ExeFailure> {
    use std::fs::File;

    let terminal = File::open(tty).map_err(|error| map_io_error(&error))?;
    let foreground_pgrp = rustix::termios::tcgetpgrp(&terminal)
        .map_err(|error| map_io_error(&io::Error::from(error)))?
        .as_raw_pid();
    let pgrp = u32::try_from(foreground_pgrp).map_err(|_| ExeFailure::Unavailable)?;
    let members =
        libproc::processes::pids_by_type(libproc::processes::ProcFilter::ByProgramGroup {
            pgrpid: pgrp,
        })
        .map_err(|_| ExeFailure::Unavailable)?;
    let members = members
        .into_iter()
        .filter_map(|pid| i32::try_from(pid).ok())
        .collect::<Vec<_>>();
    let leader = macos_group_leader(foreground_pgrp, &members)?;
    let path = libproc::libproc::proc_pid::pidpath(leader).map_err(|_| ExeFailure::Unavailable)?;
    exe_basename(Path::new(&path))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn blocking_basename(_tty: &str, _shell_pid: u32) -> Result<String, ExeFailure> {
    Err(ExeFailure::Unsupported)
}
