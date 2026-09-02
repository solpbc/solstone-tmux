// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use support::TestDirectory;
use support::authority::{
    CLIENT_INGEST_AUTHORITY_PATH, CLIENT_INGEST_AUTHORITY_SHA256, ClientIngestAuthorityImport,
    JOURNAL_REPOSITORY, JOURNAL_REVISION, OBSERVER_MANIFEST_SHA256, PROTOCOL_SCHEMA_PATH,
    PROTOCOL_SCHEMA_SHA256, ProtocolSchemaImport, sha256_hex, verify_client_ingest_authority,
    verify_client_ingest_authority_at, verify_client_ingest_import_pins,
    verify_client_ingest_upload_grammar_reference, verify_envelope_description,
    verify_generator_input, verify_import_pins, verify_schema_root,
};
use support::private_link_peer::PrivateLinkPeer;

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
fn rejects_wrong_client_ingest_authority_coordinate() {
    let mut import = valid_client_ingest_import();
    import.authority_commit = "0".repeat(40);
    assert!(verify_client_ingest_import_pins(&import).is_err());
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
fn rejects_changed_upload_operation_grammar_before_digest_verification() {
    let mut authority = read_vendored_client_ingest_authority();
    let description = authority
        .pointer_mut("/paths/~1app~1devices~1ingest/post/description")
        .and_then(|value| value.as_str().map(str::to_owned))
        .expect("read upload operation description");
    *authority
        .pointer_mut("/paths/~1app~1devices~1ingest/post/description")
        .expect("write upload operation description") = json!(description.replace(
        PROTOCOL_SCHEMA_PATH,
        "core/crates/solstone-core/src/contract/schemas/other.schema.json",
    ));
    assert!(
        verify_client_ingest_upload_grammar_reference(&authority, PROTOCOL_SCHEMA_PATH).is_err()
    );
}

#[tokio::test]
async fn common_verifier_blocks_self_certified_observer_manifest_before_bind() {
    let directory = copied_authority_repository("self-certified-observer");
    let manifest = directory
        .path()
        .join("native/solstone-tmux/vendor/observer-client-contract/manifest.json");
    let mut manifest_bytes = fs::read(&manifest).expect("read copied observer manifest");
    manifest_bytes.push(b'\n');
    fs::write(&manifest, &manifest_bytes).expect("mutate copied observer manifest");
    let import = directory
        .path()
        .join("native/solstone-tmux/contracts/observer-client-import.json");
    let replacement_digest = sha256_hex(&manifest_bytes);
    let import_bytes = fs::read_to_string(&import).expect("read copied observer import");
    fs::write(
        &import,
        import_bytes.replace(OBSERVER_MANIFEST_SHA256, &replacement_digest),
    )
    .expect("self-certify copied observer import");

    let bind_attempts = AtomicUsize::new(0);
    assert!(
        PrivateLinkPeer::start_with_authority_root(directory.path(), &bind_attempts)
            .await
            .is_err()
    );
    assert_eq!(bind_attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn common_verifier_blocks_mutated_observer_projection_before_bind() {
    let directory = copied_authority_repository("mutated-observer-projection");
    let projection = directory
        .path()
        .join("native/solstone-tmux/vendor/observer-client-contract/projection.openapi.json");
    let mut projection_bytes = fs::read(&projection).expect("read copied observer projection");
    projection_bytes.push(b'\n');
    fs::write(&projection, projection_bytes).expect("mutate copied observer projection");

    let bind_attempts = AtomicUsize::new(0);
    assert!(
        PrivateLinkPeer::start_with_authority_root(directory.path(), &bind_attempts)
            .await
            .is_err()
    );
    assert_eq!(bind_attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn common_verifier_blocks_mutated_client_authority_without_cross_fixture_leakage() {
    let invalid = copied_authority_repository("mutated-client-authority");
    let authority = invalid
        .path()
        .join("native/solstone-tmux/vendor/client-ingest-authority/client_ingest_authority.json");
    fs::write(&authority, b"{}\n").expect("mutate copied client-ingest authority");
    let intact = copied_authority_repository("intact-client-authority");
    let invalid_attempts = AtomicUsize::new(0);
    let intact_attempts = AtomicUsize::new(0);

    let (invalid_result, intact_result) = tokio::join!(
        PrivateLinkPeer::start_with_authority_root(invalid.path(), &invalid_attempts),
        PrivateLinkPeer::start_with_authority_root(intact.path(), &intact_attempts),
    );
    assert!(invalid_result.is_err());
    assert_eq!(invalid_attempts.load(Ordering::SeqCst), 0);
    let intact_peer = intact_result.expect("intact authority starts peer");
    assert_eq!(intact_attempts.load(Ordering::SeqCst), 1);
    intact_peer.shutdown().await;
}

#[cfg(unix)]
#[test]
fn common_verifier_rejects_symlinked_client_ingest_authority() {
    let directory = copied_authority_repository("symlinked-client-authority");
    let authority = directory
        .path()
        .join("native/solstone-tmux/vendor/client-ingest-authority/client_ingest_authority.json");
    fs::remove_file(&authority).expect("remove copied client-ingest authority");
    std::os::unix::fs::symlink(
        repository_root().join(
            "native/solstone-tmux/vendor/client-ingest-authority/client_ingest_authority.json",
        ),
        &authority,
    )
    .expect("symlink copied client-ingest authority");
    assert!(verify_client_ingest_authority_at(directory.path()).is_err());
}

#[cfg(unix)]
#[test]
fn common_verifier_rejects_symlinked_client_ingest_root() {
    let directory = copied_authority_repository("symlinked-client-ingest-root");
    let root = directory
        .path()
        .join("native/solstone-tmux/vendor/client-ingest-authority");
    let target = directory
        .path()
        .join("native/solstone-tmux/vendor/client-ingest-authority-real");
    fs::rename(&root, &target).expect("move copied client-ingest root");
    std::os::unix::fs::symlink(&target, &root).expect("symlink copied client-ingest root");
    assert!(verify_client_ingest_authority_at(directory.path()).is_err());
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
        authority_repository: JOURNAL_REPOSITORY.to_owned(),
        authority_commit: JOURNAL_REVISION.to_owned(),
        source_path: PROTOCOL_SCHEMA_PATH.to_owned(),
        source_sha256: PROTOCOL_SCHEMA_SHA256.to_owned(),
    }
}

fn valid_client_ingest_import() -> ClientIngestAuthorityImport {
    ClientIngestAuthorityImport {
        authority_repository: JOURNAL_REPOSITORY.to_owned(),
        authority_commit: JOURNAL_REVISION.to_owned(),
        source_path: CLIENT_INGEST_AUTHORITY_PATH.to_owned(),
        source_sha256: CLIENT_INGEST_AUTHORITY_SHA256.to_owned(),
    }
}

fn read_vendored_client_ingest_authority() -> serde_json::Value {
    let repository_root = repository_root();
    serde_json::from_slice(
        &fs::read(repository_root.join(
            "native/solstone-tmux/vendor/client-ingest-authority/client_ingest_authority.json",
        ))
        .expect("read vendored client-ingest authority"),
    )
    .expect("parse vendored client-ingest authority")
}

fn copied_authority_repository(label: &str) -> TestDirectory {
    let directory = TestDirectory::new(label);
    let source = repository_root();
    for relative in [
        "native/solstone-tmux/contracts/observer-client-import.json",
        "native/solstone-tmux/vendor/client-ingest-authority/client-ingest-authority-import.json",
        "native/solstone-tmux/vendor/client-ingest-authority/client_ingest_authority.json",
        "native/solstone-tmux/vendor/observer-client-contract/consumer-audit.json",
        "native/solstone-tmux/vendor/observer-client-contract/fixtures/wire-behavior.json",
        "native/solstone-tmux/vendor/observer-client-contract/manifest.json",
        "native/solstone-tmux/vendor/observer-client-contract/projection.openapi.json",
        "native/solstone-tmux/vendor/observer-client-contract/vectors.json",
        "native/solstone-tmux/vendor/protocol-schema/protocol-schema-import.json",
        "native/solstone-tmux/vendor/protocol-schema/protocol.schema.json",
    ] {
        let destination = directory.path().join(relative);
        fs::create_dir_all(destination.parent().expect("copied authority parent"))
            .expect("create copied authority parent");
        fs::copy(source.join(relative), destination).expect("copy authority input");
    }
    directory
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("native crate has repository root")
        .to_owned()
}

fn write_schema_root(root: &Path, schema_bytes: &[u8], extra: &[(&str, &[u8])]) {
    fs::write(root.join(IMPORT_NAME), b"{}\n").expect("write import");
    fs::write(root.join(SCHEMA_NAME), schema_bytes).expect("write schema");
    for (name, bytes) in extra {
        fs::write(root.join(name), bytes).expect("write extra protocol-schema file");
    }
}
