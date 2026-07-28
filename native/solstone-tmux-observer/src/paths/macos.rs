// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::PathBuf;

use super::{Environment, PathError, PlatformPaths};

pub fn resolve(environment: &dyn Environment) -> Result<PlatformPaths, PathError> {
    let root = resolve_root(environment)?;
    Ok(PlatformPaths {
        data_root: root.clone(),
        config_root: root,
    })
}

pub fn resolve_data_root(environment: &dyn Environment) -> Result<PathBuf, PathError> {
    resolve_root(environment)
}

pub fn resolve_config_root(environment: &dyn Environment) -> Result<PathBuf, PathError> {
    resolve_root(environment)
}

fn resolve_root(environment: &dyn Environment) -> Result<PathBuf, PathError> {
    let home = environment
        .var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or(PathError::MissingHome)?;
    Ok(PathBuf::from(home).join("Library/Application Support/solstone-tmux"))
}
