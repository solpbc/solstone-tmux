// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const JOURNAL_REVISION: &str = "460c0c3511ebe29b65fe93f99d2f77c6a1eaa658";
pub const CLIENT_INGEST_AUTHORITY_PATH: &str =
    "core/crates/solstone-core-repository-contracts/src/contracts/client_ingest_authority.json";
pub const CLIENT_INGEST_AUTHORITY_SHA256: &str =
    "277d01ae96da68a5d1c64c2243b65875425031155204cb2710dd7273e71627e5";
pub const PROTOCOL_SCHEMA_PATH: &str =
    "core/crates/solstone-core/src/contract/schemas/protocol.schema.json";
pub const PROTOCOL_SCHEMA_SHA256: &str =
    "488aed9be35faf359e26bd22ee41f872a1c2647d788e925dd2bb695985fd34d0";
pub const PROTOCOL_SCHEMA_REPOSITORY: &str = "https://github.com/solpbc/solstone-journal";

const PROTOCOL_SCHEMA_VENDORED_ROOT: &str = "native/solstone-tmux/vendor/protocol-schema";
const PROTOCOL_SCHEMA_IMPORT_NAME: &str = "protocol-schema-import.json";
const PROTOCOL_SCHEMA_FILE_NAME: &str = "protocol.schema.json";
const OBSERVER_VENDORED_ROOT: &str = "native/solstone-tmux/vendor/observer-client-contract";
const OBSERVER_PROJECTION_NAME: &str = "projection.openapi.json";
const OBSERVER_MANIFEST_NAME: &str = "manifest.json";
const ENVELOPE_GRAMMAR_DESCRIPTION: &str =
    "JSON object. Grammar: core/crates/solstone-core/src/contract/schemas/protocol.schema.json";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolSchemaImport {
    pub authority_repository: String,
    pub authority_commit: String,
    pub source_path: String,
    pub source_sha256: String,
}

#[derive(Deserialize)]
struct ObserverManifest {
    generator_inputs: Vec<ObserverGeneratorInput>,
}

#[derive(Deserialize)]
struct ObserverGeneratorInput {
    path: String,
    sha256: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn verify_import_pins(import: &ProtocolSchemaImport) -> Result<(), String> {
    if import.authority_repository != PROTOCOL_SCHEMA_REPOSITORY {
        return Err(format!(
            "protocol-schema authority repository differs: expected {PROTOCOL_SCHEMA_REPOSITORY}, got {}",
            import.authority_repository
        ));
    }
    if import.authority_commit != JOURNAL_REVISION {
        return Err(format!(
            "protocol-schema authority commit differs: expected {JOURNAL_REVISION}, got {}",
            import.authority_commit
        ));
    }
    if import.source_path != PROTOCOL_SCHEMA_PATH {
        return Err(format!(
            "protocol-schema source path differs: expected {PROTOCOL_SCHEMA_PATH}, got {}",
            import.source_path
        ));
    }
    if import.source_sha256 != PROTOCOL_SCHEMA_SHA256 {
        return Err(format!(
            "protocol-schema source digest differs: expected {PROTOCOL_SCHEMA_SHA256}, got {}",
            import.source_sha256
        ));
    }
    Ok(())
}

pub fn verify_schema_root(root: &Path, expected_sha256: &str) -> Result<(), String> {
    let mut actual = BTreeSet::new();
    collect_files(root, root, &mut actual)?;
    let expected = BTreeSet::from([
        PROTOCOL_SCHEMA_IMPORT_NAME.to_owned(),
        PROTOCOL_SCHEMA_FILE_NAME.to_owned(),
    ]);
    if actual != expected {
        return Err(format!(
            "protocol-schema directory inventory differs: expected {expected:?}, got {actual:?}"
        ));
    }
    let schema_bytes = fs::read(root.join(PROTOCOL_SCHEMA_FILE_NAME))
        .map_err(|err| format!("read vendored protocol schema: {err}"))?;
    let digest = sha256_hex(&schema_bytes);
    if digest != expected_sha256 {
        return Err(format!(
            "protocol-schema digest differs: expected {expected_sha256}, got {digest}"
        ));
    }
    Ok(())
}

pub fn verify_envelope_description(projection: &Value) -> Result<(), String> {
    let description = projection
        .get("paths")
        .and_then(|value| value.get("/app/devices/ingest"))
        .and_then(|value| value.get("post"))
        .and_then(|value| value.get("requestBody"))
        .and_then(|value| value.get("content"))
        .and_then(|value| value.get("multipart/form-data"))
        .and_then(|value| value.get("schema"))
        .and_then(|value| value.get("properties"))
        .and_then(|value| value.get("envelope"))
        .and_then(|value| value.get("description"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "projection is missing the ingest envelope description pointer".to_owned()
        })?;
    if description != ENVELOPE_GRAMMAR_DESCRIPTION {
        return Err(format!(
            "projection envelope description differs: expected {ENVELOPE_GRAMMAR_DESCRIPTION}, got {description}"
        ));
    }
    Ok(())
}

pub fn verify_generator_input(path: &str, sha256: &str) -> Result<(), String> {
    if path != CLIENT_INGEST_AUTHORITY_PATH {
        return Err(format!(
            "client-ingest authority path differs: expected {CLIENT_INGEST_AUTHORITY_PATH}, got {path}"
        ));
    }
    if sha256 != CLIENT_INGEST_AUTHORITY_SHA256 {
        return Err(format!(
            "client-ingest authority digest differs: expected {CLIENT_INGEST_AUTHORITY_SHA256}, got {sha256}"
        ));
    }
    Ok(())
}

pub fn verify_client_ingest_authority_at(repository_root: &Path) -> Result<(), String> {
    let schema_root = repository_root.join(PROTOCOL_SCHEMA_VENDORED_ROOT);
    let import_bytes = fs::read(schema_root.join(PROTOCOL_SCHEMA_IMPORT_NAME))
        .map_err(|err| format!("read protocol-schema import: {err}"))?;
    let import: ProtocolSchemaImport = serde_json::from_slice(&import_bytes)
        .map_err(|err| format!("parse protocol-schema import: {err}"))?;
    verify_import_pins(&import)?;
    verify_schema_root(&schema_root, PROTOCOL_SCHEMA_SHA256)?;

    let observer_root = repository_root.join(OBSERVER_VENDORED_ROOT);
    let projection: Value = serde_json::from_slice(
        &fs::read(observer_root.join(OBSERVER_PROJECTION_NAME))
            .map_err(|err| format!("read vendored projection: {err}"))?,
    )
    .map_err(|err| format!("parse vendored projection: {err}"))?;
    verify_envelope_description(&projection)?;

    let manifest: ObserverManifest = serde_json::from_slice(
        &fs::read(observer_root.join(OBSERVER_MANIFEST_NAME))
            .map_err(|err| format!("read observer manifest: {err}"))?,
    )
    .map_err(|err| format!("parse observer manifest: {err}"))?;
    if manifest.generator_inputs.len() != 1 {
        return Err(format!(
            "client-ingest authority input count differs: expected 1, got {}",
            manifest.generator_inputs.len()
        ));
    }
    verify_generator_input(
        &manifest.generator_inputs[0].path,
        &manifest.generator_inputs[0].sha256,
    )
}

pub fn verify_client_ingest_authority() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("native crate has repository root");
    if let Err(error) = verify_client_ingest_authority_at(repository_root) {
        panic!("{error}");
    }
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), String> {
    let entries = fs::read_dir(directory).map_err(|err| {
        format!(
            "read protocol-schema directory {}: {err}",
            directory.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("read protocol-schema directory entry: {err}"))?;
        let file_type = entry
            .file_type()
            .map_err(|err| format!("inspect protocol-schema file type: {err}"))?;
        if file_type.is_dir() {
            collect_files(root, &entry.path(), files)?;
            continue;
        }
        if !file_type.is_file() {
            return Err("protocol-schema contains a special entry".to_owned());
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| "protocol-schema file is not below the contract root".to_owned())?
            .to_str()
            .ok_or_else(|| "protocol-schema filename is not UTF-8".to_owned())?
            .to_owned();
        if !files.insert(relative.clone()) {
            return Err(format!(
                "protocol-schema contains a duplicate file: {relative}"
            ));
        }
    }
    Ok(())
}
