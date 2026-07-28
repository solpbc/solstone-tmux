// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::PathBuf;

use super::{Environment, PathError, PlatformPaths};

pub fn resolve(environment: &dyn Environment) -> Result<PlatformPaths, PathError> {
    let home = environment
        .var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or(PathError::MissingHome)?;
    let root = PathBuf::from(home).join("Library/Application Support/solstone-tmux");
    Ok(PlatformPaths {
        data_root: root.clone(),
        config_root: root,
    })
}
