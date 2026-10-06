// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const ROOT: &str = "vendor/device-migration-contract/contracts";
const IMPORT: &str = "contracts/device-migration-import.json";
const REVISION: &str = "1e432dba3ecdfa43789c25f97077fdc3e71fab59";
const REPOSITORY: &str = "https://github.com/solpbc/solstone-journal";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    authority_repository: String,
    authority_commit: String,
    vendored_root: String,
    files: Vec<FilePin>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePin {
    path: String,
    source_path: String,
    sha256: String,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify(root: &Path) -> Result<(), String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let import: Import =
        serde_json::from_slice(&fs::read(manifest.join(IMPORT)).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if import.authority_repository != REPOSITORY || import.authority_commit != REVISION {
        return Err("device migration authority coordinate differs".to_owned());
    }
    if import.vendored_root != format!("native/solstone-tmux/{ROOT}") {
        return Err("device migration vendor root differs".to_owned());
    }
    let expected = BTreeSet::from(["v1.schema.json".to_owned(), "v1.vectors.json".to_owned()]);
    let actual = fs::read_dir(root)
        .map_err(|e| e.to_string())?
        .map(|entry| {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
                return Err("device migration bundle contains a non-file".to_owned());
            }
            entry
                .file_name()
                .into_string()
                .map_err(|_| "device migration filename is not UTF-8".to_owned())
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if actual != expected || import.files.len() != expected.len() {
        return Err("device migration bundle inventory differs".to_owned());
    }
    for pin in import.files {
        let (source, hash) = match pin.path.as_str() {
            "v1.schema.json" => (
                "contracts/device-migration/v1.schema.json",
                "5ea0ce5bf0bc5f07233f05dda3334ccea3bf173363fcd4fd4cdcd9479130a373",
            ),
            "v1.vectors.json" => (
                "contracts/device-migration/v1.vectors.json",
                "3ba1bb508a0cd5756626c6246bfbff77573c54538db60c8e7f938e38b306ef9c",
            ),
            _ => return Err("unexpected device migration import path".to_owned()),
        };
        if pin.source_path != source || pin.sha256 != hash {
            return Err("device migration source pin differs".to_owned());
        }
        let bytes = fs::read(root.join(&pin.path)).map_err(|e| e.to_string())?;
        if digest(&bytes) != hash {
            return Err(format!("device migration byte mismatch: {}", pin.path));
        }
    }
    Ok(())
}

#[test]
fn vendored_device_migration_authority_is_byte_exact() {
    verify(&Path::new(env!("CARGO_MANIFEST_DIR")).join(ROOT))
        .expect("device migration authority provenance");
}

#[test]
fn rejects_mutated_device_migration_bytes() {
    let directory = support::TestDirectory::new("device-migration-contract");
    fs::write(directory.path().join("v1.schema.json"), b"mutated").expect("write schema");
    fs::write(directory.path().join("v1.vectors.json"), b"mutated").expect("write vectors");
    assert!(verify(directory.path()).is_err());
}
