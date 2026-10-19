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

pub fn linux_foreground_pgrp(stat: &[u8]) -> Result<u32, ExeFailure> {
    let comm_end = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or(ExeFailure::Unavailable)?;
    let mut fields = stat
        .get(comm_end + 1..)
        .ok_or(ExeFailure::Unavailable)?
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let tpgid = fields.nth(5).ok_or(ExeFailure::Unavailable)?;
    let tpgid = std::str::from_utf8(tpgid)
        .map_err(|_| ExeFailure::Unavailable)?
        .parse::<i64>()
        .map_err(|_| ExeFailure::Unavailable)?;
    if tpgid <= 0 {
        return Err(ExeFailure::Unavailable);
    }
    u32::try_from(tpgid).map_err(|_| ExeFailure::Unavailable)
}

pub fn linux_exe_path_for_stat(stat: &[u8]) -> Result<PathBuf, ExeFailure> {
    Ok(linux_exe_link(linux_foreground_pgrp(stat)?))
}

pub fn macos_foreground_pid(e_tpgid: u32) -> Result<i32, ExeFailure> {
    let pid = i32::try_from(e_tpgid).map_err(|_| ExeFailure::Unavailable)?;
    if pid <= 0 {
        return Err(ExeFailure::Unavailable);
    }
    Ok(pid)
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

pub trait ForegroundLookup: Send + Sync {
    fn lookup<'a>(
        &'a self,
        pane_pid: u32,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExeFailure>> + Send + 'a>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLookup;

impl ForegroundLookup for SystemLookup {
    fn lookup<'a>(
        &'a self,
        pane_pid: u32,
    ) -> Pin<Box<dyn Future<Output = Result<String, ExeFailure>> + Send + 'a>> {
        Box::pin(lookup_basename(pane_pid))
    }
}

pub async fn lookup_basename(pane_pid: u32) -> Result<String, ExeFailure> {
    tokio::task::spawn_blocking(move || blocking_basename(pane_pid))
        .await
        .unwrap_or(Err(ExeFailure::Unavailable))
}

#[cfg(target_os = "linux")]
fn blocking_basename(pane_pid: u32) -> Result<String, ExeFailure> {
    use std::os::unix::ffi::OsStringExt;

    let stat =
        std::fs::read(format!("/proc/{pane_pid}/stat")).map_err(|error| map_io_error(&error))?;
    let path = linux_exe_path_for_stat(&stat)?;
    let target = rustix::fs::readlink(&path, Vec::new())
        .map_err(|error| map_io_error(&io::Error::from(error)))?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(target.into_bytes()));
    exe_basename(&path)
}

#[cfg(target_os = "macos")]
fn blocking_basename(pane_pid: u32) -> Result<String, ExeFailure> {
    let pane_pid = i32::try_from(pane_pid).map_err(|_| ExeFailure::Unavailable)?;
    let info = libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pane_pid, 0)
        .map_err(|_| ExeFailure::Unavailable)?;
    let foreground_pid = macos_foreground_pid(info.e_tpgid)?;
    let path = libproc::proc_pid::pidpath(foreground_pid).map_err(|_| ExeFailure::Unavailable)?;
    exe_basename(Path::new(&path))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn blocking_basename(_pane_pid: u32) -> Result<String, ExeFailure> {
    Err(ExeFailure::Unsupported)
}
