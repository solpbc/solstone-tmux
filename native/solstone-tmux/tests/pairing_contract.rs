// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const AUTHORITY_REPOSITORY: &str = "https://github.com/solpbc/solstone-journal";
const AUTHORITY_COMMIT: &str = "460c0c3511ebe29b65fe93f99d2f77c6a1eaa658";
const AUTHORITY_CREATION_COMMIT: &str = "c1e9589e60213b39042b92cae94a5d2f0448535e";
const BUNDLE_VERSION: &str = "1.0.0";
const MANIFEST_PATH: &str = "manifest.json";
const AUTHORITY_INPUT_SHA256: &str =
    "34a0ca85485e7fbdeb8397fb33a1d0fb6e6d3845d85d0a7f8219dfd335affdda";
const VENDORED_ROOT: &str = "native/solstone-tmux/vendor/pairing-contract";
const IMPORT_PATH: &str = "contracts/pairing-contract-import.json";
const CONTRACT_FILES: [&str; 4] = [
    "consumer-audit.json",
    "fixtures/wire-behavior.json",
    "projection.openapi.json",
    "vectors.json",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportProvenance {
    authority_repository: String,
    authority_commit: String,
    authority_creation_commit: String,
    bundle_version: String,
    manifest_path: String,
    manifest_sha256: String,
    authority_input_sha256: String,
    vendored_root: String,
}

#[derive(Deserialize)]
struct Manifest {
    bundle_semver: String,
    files: Vec<ManifestFile>,
    generator_inputs: Vec<GeneratorInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
struct GeneratorInput {
    sha256: String,
}

#[test]
fn vendored_contract_matches_provenance() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("native crate has repository root");
    let provenance: ImportProvenance = serde_json::from_slice(
        &fs::read(manifest_dir.join(IMPORT_PATH)).expect("read contract import provenance"),
    )
    .expect("parse contract import provenance");

    assert_eq!(provenance.authority_repository, AUTHORITY_REPOSITORY);
    assert_eq!(provenance.authority_commit, AUTHORITY_COMMIT);
    assert_eq!(
        provenance.authority_creation_commit,
        AUTHORITY_CREATION_COMMIT
    );
    assert_eq!(provenance.bundle_version, BUNDLE_VERSION);
    assert_eq!(provenance.manifest_path, MANIFEST_PATH);
    assert_eq!(provenance.authority_input_sha256, AUTHORITY_INPUT_SHA256);
    assert_eq!(provenance.vendored_root, VENDORED_ROOT);

    let vendored_root = repository_root.join(&provenance.vendored_root);
    let manifest_bytes =
        fs::read(vendored_root.join(&provenance.manifest_path)).expect("read vendored manifest");
    assert_eq!(
        sha256_hex(&manifest_bytes),
        provenance.manifest_sha256,
        "vendored manifest digest differs from import provenance"
    );

    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).expect("parse vendored manifest");
    assert_eq!(
        manifest.bundle_semver, BUNDLE_VERSION,
        "vendored manifest SemVer differs from pinned fact"
    );
    let expected = CONTRACT_FILES.into_iter().collect::<BTreeSet<_>>();
    let listed = manifest
        .files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        manifest.files.len(),
        expected.len(),
        "manifest contract file count differs"
    );
    assert_eq!(listed, expected, "manifest contract file inventory differs");
    assert_eq!(
        manifest.generator_inputs.len(),
        1,
        "pairing authority input count differs"
    );
    assert_eq!(
        manifest.generator_inputs[0].sha256, AUTHORITY_INPUT_SHA256,
        "vendored manifest authority-input digest differs from pinned fact"
    );

    for entry in &manifest.files {
        let bytes = fs::read(vendored_root.join(&entry.path)).expect("read vendored contract file");
        assert_eq!(
            sha256_hex(&bytes),
            entry.sha256,
            "vendored contract file digest differs"
        );
    }

    let mut actual = BTreeSet::new();
    collect_vendored_files(&vendored_root, &vendored_root, &mut actual);
    let expected_with_manifest = expected
        .into_iter()
        .map(str::to_owned)
        .chain(std::iter::once(MANIFEST_PATH.to_owned()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual, expected_with_manifest,
        "vendored contract directory inventory differs"
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn collect_vendored_files(root: &Path, directory: &Path, files: &mut BTreeSet<String>) {
    for entry in fs::read_dir(directory).expect("read vendored contract directory") {
        let entry = entry.expect("read vendored contract directory entry");
        let file_type = entry.file_type().expect("inspect vendored file type");
        if file_type.is_dir() {
            collect_vendored_files(root, &entry.path(), files);
        } else {
            assert!(
                file_type.is_file(),
                "vendored contract contains a special entry"
            );
            let relative = entry
                .path()
                .strip_prefix(root)
                .expect("vendored file is below contract root")
                .to_str()
                .expect("vendored contract filename is UTF-8")
                .to_owned();
            assert!(
                files.insert(relative),
                "vendored contract contains a duplicate file"
            );
        }
    }
}
