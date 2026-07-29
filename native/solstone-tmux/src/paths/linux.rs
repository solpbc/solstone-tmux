// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::path::PathBuf;

use super::{Environment, PathError, PlatformPaths};

pub fn resolve(environment: &dyn Environment) -> Result<PlatformPaths, PathError> {
    Ok(PlatformPaths {
        data_root: resolve_data_root(environment)?,
        config_root: resolve_config_root(environment)?,
    })
}

pub fn resolve_data_root(environment: &dyn Environment) -> Result<PathBuf, PathError> {
    let home = nonempty(environment.var_os("HOME")).ok_or(PathError::MissingHome)?;
    Ok(nonempty(environment.var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".local/share"))
        .join("solstone-tmux"))
}

pub fn resolve_config_root(environment: &dyn Environment) -> Result<PathBuf, PathError> {
    let home = nonempty(environment.var_os("HOME")).ok_or(PathError::MissingHome)?;
    Ok(nonempty(environment.var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".config"))
        .join("solstone-tmux"))
}

fn nonempty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}
