// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::ffi::OsString;
use std::path::PathBuf;

use super::{Environment, PathError, PlatformPaths};

pub fn resolve(environment: &dyn Environment) -> Result<PlatformPaths, PathError> {
    let home = nonempty(environment.var_os("HOME")).ok_or(PathError::MissingHome)?;
    let data_base = nonempty(environment.var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".local/share"));
    let config_base = nonempty(environment.var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".config"));
    Ok(PlatformPaths {
        data_root: data_base.join("solstone-tmux"),
        config_root: config_base.join("solstone-tmux"),
    })
}

fn nonempty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}
