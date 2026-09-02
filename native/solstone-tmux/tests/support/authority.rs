// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const JOURNAL_REVISION: &str = "460c0c3511ebe29b65fe93f99d2f77c6a1eaa658";
pub const JOURNAL_REPOSITORY: &str = "https://github.com/solpbc/solstone-journal";
pub const CLIENT_INGEST_AUTHORITY_PATH: &str =
    "core/crates/solstone-core-repository-contracts/src/contracts/client_ingest_authority.json";
pub const CLIENT_INGEST_AUTHORITY_SHA256: &str =
    "277d01ae96da68a5d1c64c2243b65875425031155204cb2710dd7273e71627e5";
pub const PROTOCOL_SCHEMA_PATH: &str =
    "core/crates/solstone-core/src/contract/schemas/protocol.schema.json";
pub const PROTOCOL_SCHEMA_SHA256: &str =
    "488aed9be35faf359e26bd22ee41f872a1c2647d788e925dd2bb695985fd34d0";
pub const OBSERVER_BUNDLE_VERSION: &str = "10.0.0";
pub const OBSERVER_MANIFEST_SHA256: &str =
    "7fe51e895f277a994c0c1459175cbf327e0dd7474e805110408eebc3b161ca75";

const PROTOCOL_SCHEMA_VENDORED_ROOT: &str = "native/solstone-tmux/vendor/protocol-schema";
const PROTOCOL_SCHEMA_IMPORT_NAME: &str = "protocol-schema-import.json";
const PROTOCOL_SCHEMA_FILE_NAME: &str = "protocol.schema.json";
const CLIENT_INGEST_VENDORED_ROOT: &str = "native/solstone-tmux/vendor/client-ingest-authority";
const CLIENT_INGEST_IMPORT_NAME: &str = "client-ingest-authority-import.json";
const CLIENT_INGEST_FILE_NAME: &str = "client_ingest_authority.json";
const OBSERVER_VENDORED_ROOT: &str = "native/solstone-tmux/vendor/observer-client-contract";
const OBSERVER_IMPORT_PATH: &str = "native/solstone-tmux/contracts/observer-client-import.json";
const OBSERVER_PROJECTION_NAME: &str = "projection.openapi.json";
const OBSERVER_MANIFEST_NAME: &str = "manifest.json";
const OBSERVER_MANIFEST_FILES: [&str; 4] = [
    "consumer-audit.json",
    "fixtures/wire-behavior.json",
    "projection.openapi.json",
    "vectors.json",
];
const ENVELOPE_GRAMMAR_DESCRIPTION: &str =
    "JSON object. Grammar: core/crates/solstone-core/src/contract/schemas/protocol.schema.json";
const UPLOAD_GRAMMAR_PREFIX: &str = "Envelope grammar is `";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolSchemaImport {
    pub authority_repository: String,
    pub authority_commit: String,
    pub source_path: String,
    pub source_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIngestAuthorityImport {
    pub authority_repository: String,
    pub authority_commit: String,
    pub source_path: String,
    pub source_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserverImport {
    authority_repository: String,
    authority_commit: String,
    bundle_version: String,
    manifest_path: String,
    manifest_sha256: String,
    vendored_root: String,
}

#[derive(Deserialize)]
struct ObserverManifest {
    files: Vec<ManifestFile>,
    generator_inputs: Vec<ObserverGeneratorInput>,
}

#[derive(Deserialize)]
struct ManifestFile {
    path: String,
    sha256: String,
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
    if import.authority_repository != JOURNAL_REPOSITORY {
        return Err(format!(
            "protocol-schema authority repository differs: expected {JOURNAL_REPOSITORY}, got {}",
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

pub fn verify_client_ingest_import_pins(
    import: &ClientIngestAuthorityImport,
) -> Result<(), String> {
    if import.authority_repository != JOURNAL_REPOSITORY {
        return Err(format!(
            "client-ingest authority repository differs: expected {JOURNAL_REPOSITORY}, got {}",
            import.authority_repository
        ));
    }
    if import.authority_commit != JOURNAL_REVISION {
        return Err(format!(
            "client-ingest authority commit differs: expected {JOURNAL_REVISION}, got {}",
            import.authority_commit
        ));
    }
    verify_generator_input(&import.source_path, &import.source_sha256)
}

fn verify_observer_import_pins(import: &ObserverImport) -> Result<(), String> {
    if import.authority_repository != JOURNAL_REPOSITORY {
        return Err(format!(
            "observer authority repository differs: expected {JOURNAL_REPOSITORY}, got {}",
            import.authority_repository
        ));
    }
    if import.authority_commit != JOURNAL_REVISION {
        return Err(format!(
            "observer authority commit differs: expected {JOURNAL_REVISION}, got {}",
            import.authority_commit
        ));
    }
    if import.bundle_version != OBSERVER_BUNDLE_VERSION {
        return Err(format!(
            "observer bundle version differs: expected {OBSERVER_BUNDLE_VERSION}, got {}",
            import.bundle_version
        ));
    }
    if import.manifest_path != OBSERVER_MANIFEST_NAME {
        return Err(format!(
            "observer manifest path differs: expected {OBSERVER_MANIFEST_NAME}, got {}",
            import.manifest_path
        ));
    }
    if import.manifest_sha256 != OBSERVER_MANIFEST_SHA256 {
        return Err(format!(
            "observer manifest digest differs: expected {OBSERVER_MANIFEST_SHA256}, got {}",
            import.manifest_sha256
        ));
    }
    if import.vendored_root != OBSERVER_VENDORED_ROOT {
        return Err(format!(
            "observer vendored root differs: expected {OBSERVER_VENDORED_ROOT}, got {}",
            import.vendored_root
        ));
    }
    Ok(())
}

pub fn verify_schema_root(root: &Path, expected_sha256: &str) -> Result<(), String> {
    let expected = BTreeSet::from([
        PROTOCOL_SCHEMA_IMPORT_NAME.to_owned(),
        PROTOCOL_SCHEMA_FILE_NAME.to_owned(),
    ]);
    verify_exact_inventory(root, &expected, "protocol-schema")?;
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

pub fn verify_client_ingest_upload_grammar_reference(
    authority: &Value,
    expected_path: &str,
) -> Result<(), String> {
    let operation = authority
        .get("paths")
        .and_then(|value| value.get("/app/devices/ingest"))
        .and_then(|value| value.get("post"))
        .ok_or_else(|| "client-ingest authority is missing the upload operation".to_owned())?;
    let operation_id = operation
        .get("operationId")
        .and_then(Value::as_str)
        .ok_or_else(|| "client-ingest authority upload operation has no operationId".to_owned())?;
    if operation_id != "client.ingestUpload" {
        return Err(format!(
            "client-ingest authority upload operation differs: expected client.ingestUpload, got {operation_id}"
        ));
    }
    let description = operation
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| "client-ingest authority upload operation has no description".to_owned())?;
    let (_, after_prefix) = description
        .split_once(UPLOAD_GRAMMAR_PREFIX)
        .ok_or_else(|| {
            "client-ingest authority upload operation lacks grammar declaration".to_owned()
        })?;
    let (grammar_path, _) = after_prefix.split_once('`').ok_or_else(|| {
        "client-ingest authority upload grammar declaration is unterminated".to_owned()
    })?;
    if grammar_path != expected_path {
        return Err(format!(
            "client-ingest authority upload grammar differs: expected {expected_path}, got {grammar_path}"
        ));
    }
    Ok(())
}

fn verify_client_ingest_root(root: &Path) -> Result<Value, String> {
    let expected = BTreeSet::from([
        CLIENT_INGEST_IMPORT_NAME.to_owned(),
        CLIENT_INGEST_FILE_NAME.to_owned(),
    ]);
    verify_exact_inventory(root, &expected, "client-ingest authority")?;
    let import_bytes = fs::read(root.join(CLIENT_INGEST_IMPORT_NAME))
        .map_err(|err| format!("read client-ingest import: {err}"))?;
    let import: ClientIngestAuthorityImport = serde_json::from_slice(&import_bytes)
        .map_err(|err| format!("parse client-ingest import: {err}"))?;
    verify_client_ingest_import_pins(&import)?;
    let authority_bytes = fs::read(root.join(CLIENT_INGEST_FILE_NAME))
        .map_err(|err| format!("read client-ingest authority: {err}"))?;
    let digest = sha256_hex(&authority_bytes);
    if digest != CLIENT_INGEST_AUTHORITY_SHA256 {
        return Err(format!(
            "client-ingest authority digest differs: expected {CLIENT_INGEST_AUTHORITY_SHA256}, got {digest}"
        ));
    }
    serde_json::from_slice(&authority_bytes)
        .map_err(|err| format!("parse client-ingest authority: {err}"))
}

fn verify_observer_contract(repository_root: &Path) -> Result<Value, String> {
    let import_bytes = fs::read(repository_root.join(OBSERVER_IMPORT_PATH))
        .map_err(|err| format!("read observer import: {err}"))?;
    let import: ObserverImport = serde_json::from_slice(&import_bytes)
        .map_err(|err| format!("parse observer import: {err}"))?;
    verify_observer_import_pins(&import)?;

    let observer_root = repository_root.join(&import.vendored_root);
    let expected_files = OBSERVER_MANIFEST_FILES
        .into_iter()
        .map(str::to_owned)
        .chain(std::iter::once(OBSERVER_MANIFEST_NAME.to_owned()))
        .collect::<BTreeSet<_>>();
    verify_exact_inventory(&observer_root, &expected_files, "observer contract")?;
    let manifest_bytes = fs::read(observer_root.join(&import.manifest_path))
        .map_err(|err| format!("read observer manifest: {err}"))?;
    let manifest_digest = sha256_hex(&manifest_bytes);
    if manifest_digest != OBSERVER_MANIFEST_SHA256 {
        return Err(format!(
            "observer manifest digest differs: expected {OBSERVER_MANIFEST_SHA256}, got {manifest_digest}"
        ));
    }
    let manifest: ObserverManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|err| format!("parse observer manifest: {err}"))?;
    let manifest_files = manifest
        .files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    let expected_manifest_files = OBSERVER_MANIFEST_FILES.into_iter().collect::<BTreeSet<_>>();
    if manifest_files != expected_manifest_files
        || manifest.files.len() != expected_manifest_files.len()
    {
        return Err("observer manifest file inventory differs".to_owned());
    }
    for file in &manifest.files {
        let bytes = fs::read(observer_root.join(&file.path))
            .map_err(|err| format!("read observer contract file {}: {err}", file.path))?;
        let digest = sha256_hex(&bytes);
        if digest != file.sha256 {
            return Err(format!(
                "observer contract file digest differs for {}: expected {}, got {digest}",
                file.path, file.sha256
            ));
        }
    }
    if manifest.generator_inputs.len() != 1 {
        return Err(format!(
            "client-ingest authority input count differs: expected 1, got {}",
            manifest.generator_inputs.len()
        ));
    }
    verify_generator_input(
        &manifest.generator_inputs[0].path,
        &manifest.generator_inputs[0].sha256,
    )?;
    serde_json::from_slice(
        &fs::read(observer_root.join(OBSERVER_PROJECTION_NAME))
            .map_err(|err| format!("read vendored projection: {err}"))?,
    )
    .map_err(|err| format!("parse vendored projection: {err}"))
}

pub fn verify_client_ingest_authority_at(repository_root: &Path) -> Result<(), String> {
    let schema_root = repository_root.join(PROTOCOL_SCHEMA_VENDORED_ROOT);
    let import_bytes = fs::read(schema_root.join(PROTOCOL_SCHEMA_IMPORT_NAME))
        .map_err(|err| format!("read protocol-schema import: {err}"))?;
    let import: ProtocolSchemaImport = serde_json::from_slice(&import_bytes)
        .map_err(|err| format!("parse protocol-schema import: {err}"))?;
    verify_import_pins(&import)?;
    verify_schema_root(&schema_root, PROTOCOL_SCHEMA_SHA256)?;

    let authority = verify_client_ingest_root(&repository_root.join(CLIENT_INGEST_VENDORED_ROOT))?;
    verify_client_ingest_upload_grammar_reference(&authority, &import.source_path)?;

    let projection = verify_observer_contract(repository_root)?;
    verify_envelope_description(&projection)
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

fn verify_exact_inventory(
    root: &Path,
    expected: &BTreeSet<String>,
    label: &str,
) -> Result<(), String> {
    let root_type = fs::symlink_metadata(root)
        .map_err(|err| format!("inspect {label} root {}: {err}", root.display()))?
        .file_type();
    if !root_type.is_dir() {
        return Err(format!("{label} root is not a regular directory"));
    }
    let mut actual = BTreeSet::new();
    collect_files(root, root, &mut actual)?;
    if &actual != expected {
        return Err(format!(
            "{label} directory inventory differs: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
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
