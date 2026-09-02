// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::path::Path;

use serde_json::json;
use support::TestDirectory;
use support::authority::{
    CLIENT_INGEST_AUTHORITY_PATH, CLIENT_INGEST_AUTHORITY_SHA256, JOURNAL_REVISION,
    PROTOCOL_SCHEMA_PATH, PROTOCOL_SCHEMA_REPOSITORY, PROTOCOL_SCHEMA_SHA256, ProtocolSchemaImport,
    verify_client_ingest_authority, verify_envelope_description, verify_generator_input,
    verify_import_pins, verify_schema_root,
};

const IMPORT_NAME: &str = "protocol-schema-import.json";
const SCHEMA_NAME: &str = "protocol.schema.json";
const OPERATION_DESCRIPTION: &str = "Upload one capture segment from a linked device as multipart form data. Identity is the linked-device mTLS certificate. The body has one JSON `envelope` part plus repeated `files` parts. Envelope grammar is `core/crates/solstone-core/src/contract/schemas/protocol.schema.json` (required `day`, `segment`, `files`; optional `source`, `meta`; `stream` and `observer` are forbidden). Each multipart part is at most 64 MiB; the total request is at most 128 MiB.";

#[test]
fn vendored_protocol_schema_matches_provenance() {
    verify_client_ingest_authority();
}

#[test]
fn rejects_wrong_schema_bytes() {
    let directory = TestDirectory::new("protocol-schema-wrong-bytes");
    write_schema_root(directory.path(), b"mutated-schema\n", &[]);
    assert!(verify_schema_root(directory.path(), PROTOCOL_SCHEMA_SHA256).is_err());
}

#[test]
fn rejects_wrong_source_path() {
    let mut import = valid_import();
    import.source_path = "mutated/path.json".to_owned();
    assert!(verify_import_pins(&import).is_err());
}

#[test]
fn rejects_wrong_authority_commit() {
    let mut import = valid_import();
    import.authority_commit = "0".repeat(40);
    assert!(verify_import_pins(&import).is_err());
}

#[test]
fn rejects_wrong_source_sha256() {
    let mut import = valid_import();
    import.source_sha256 = "0".repeat(64);
    assert!(verify_import_pins(&import).is_err());
}

#[test]
fn rejects_extra_file_in_schema_root() {
    let directory = TestDirectory::new("protocol-schema-extra");
    write_schema_root(
        directory.path(),
        b"schema\n",
        &[("extra.json", b"extra\n" as &[u8])],
    );
    assert!(verify_schema_root(directory.path(), PROTOCOL_SCHEMA_SHA256).is_err());
}

#[test]
fn rejects_missing_file_in_schema_root() {
    let directory = TestDirectory::new("protocol-schema-missing");
    fs::write(directory.path().join(IMPORT_NAME), b"{}\n").expect("write import");
    assert!(verify_schema_root(directory.path(), PROTOCOL_SCHEMA_SHA256).is_err());
}

#[test]
fn rejects_renamed_file_in_schema_root() {
    let directory = TestDirectory::new("protocol-schema-renamed");
    fs::write(directory.path().join(IMPORT_NAME), b"{}\n").expect("write import");
    fs::write(directory.path().join("renamed.schema.json"), b"schema\n")
        .expect("write renamed schema");
    assert!(verify_schema_root(directory.path(), PROTOCOL_SCHEMA_SHA256).is_err());
}

#[test]
fn rejects_altered_envelope_description_when_operation_description_still_matches() {
    let projection = json!({
        "paths": {
            "/app/devices/ingest": {
                "post": {
                    "description": OPERATION_DESCRIPTION,
                    "requestBody": {
                        "content": {
                            "multipart/form-data": {
                                "schema": {
                                    "properties": {
                                        "envelope": {
                                            "description": "mutated envelope grammar"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    assert!(verify_envelope_description(&projection).is_err());
}

#[test]
fn rejects_mismatched_generator_input_path() {
    assert!(verify_generator_input("mutated/path.json", CLIENT_INGEST_AUTHORITY_SHA256).is_err());
}

#[test]
fn rejects_mismatched_generator_input_sha256() {
    assert!(verify_generator_input(CLIENT_INGEST_AUTHORITY_PATH, &"0".repeat(64)).is_err());
}

fn valid_import() -> ProtocolSchemaImport {
    ProtocolSchemaImport {
        authority_repository: PROTOCOL_SCHEMA_REPOSITORY.to_owned(),
        authority_commit: JOURNAL_REVISION.to_owned(),
        source_path: PROTOCOL_SCHEMA_PATH.to_owned(),
        source_sha256: PROTOCOL_SCHEMA_SHA256.to_owned(),
    }
}

fn write_schema_root(root: &Path, schema_bytes: &[u8], extra: &[(&str, &[u8])]) {
    fs::write(root.join(IMPORT_NAME), b"{}\n").expect("write import");
    fs::write(root.join(SCHEMA_NAME), schema_bytes).expect("write schema");
    for (name, bytes) in extra {
        fs::write(root.join(name), bytes).expect("write extra protocol-schema file");
    }
}
