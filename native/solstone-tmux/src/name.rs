// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::path::{Component, Path, PathBuf};

pub const MAX_COMPONENT_BYTES: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedName(String);

impl DerivedName {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn session_filename(&self) -> String {
        format!("tmux_{}_screen.jsonl", self.0)
    }

    pub fn join_checked(&self, parent: &Path) -> Result<PathBuf, NameError> {
        let child = parent.join(&self.0);
        if child.parent() != Some(parent) {
            return Err(NameError::EscapesParent);
        }
        Ok(child)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NameError {
    Empty,
    ContainsNul,
    Absolute,
    Traversal,
    TooLong { actual: usize, limit: usize },
    EscapesParent,
}

impl fmt::Display for NameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "identity must not be empty"),
            Self::ContainsNul => write!(formatter, "identity must not contain NUL"),
            Self::Absolute => write!(formatter, "identity must not be an absolute path"),
            Self::Traversal => write!(formatter, "identity must not contain path traversal"),
            Self::TooLong { actual, limit } => write!(
                formatter,
                "derived identity is {actual} bytes; the limit is {limit} bytes"
            ),
            Self::EscapesParent => write!(formatter, "derived identity escapes its parent"),
        }
    }
}

impl std::error::Error for NameError {}

pub fn derive_component(raw: &str) -> Result<DerivedName, NameError> {
    if raw.is_empty() {
        return Err(NameError::Empty);
    }
    if raw.contains('\0') {
        return Err(NameError::ContainsNul);
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(NameError::Absolute);
    }
    if raw.split('/').any(|part| part == "." || part == "..")
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(NameError::Traversal);
    }

    let canonical = raw.bytes().enumerate().all(|(index, byte)| {
        if index == 0 {
            byte.is_ascii_lowercase() || byte.is_ascii_digit()
        } else {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        }
    });

    let mut component = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();

    if !canonical {
        component.push('~');
        for byte in raw.as_bytes() {
            use std::fmt::Write;
            write!(&mut component, "{byte:02x}").expect("writing to a String cannot fail");
        }
    }

    if component.len() > MAX_COMPONENT_BYTES {
        return Err(NameError::TooLong {
            actual: component.len(),
            limit: MAX_COMPONENT_BYTES,
        });
    }
    Ok(DerivedName(component))
}
