// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const AUTHORITY_REPOSITORY: &str = "https://github.com/solpbc/solstone-journal";
const AUTHORITY_COMMIT: &str = "c5cc2fbce0867e821916045b50716a6c0ed4e57e";
const BUNDLE_VERSION: &str = "2.1.9";
const MANIFEST_PATH: &str = "manifest.json";
const VENDORED_ROOT: &str = "native/solstone-tmux-observer/vendor/observer-client-contract";
const IMPORT_PATH: &str = "contracts/observer-client-import.json";
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
    bundle_version: String,
    manifest_path: String,
    manifest_sha256: String,
    vendored_root: String,
}

#[derive(Deserialize)]
struct Manifest {
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
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
    assert_eq!(provenance.bundle_version, BUNDLE_VERSION);
    assert_eq!(provenance.manifest_path, MANIFEST_PATH);
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
