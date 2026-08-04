// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[allow(dead_code)]
#[path = "support/package_model.rs"]
mod package_model;

use std::collections::BTreeMap;

use package_model::{
    ARTIFACTS, ArtifactKind, ArtifactRecord, DEB_DEPENDS, DEB_DESTINATION, DOCUMENT_MODE,
    EXECUTABLE_MODE, EXECUTABLE_NAME, ExecutableArchitecture, ExecutableRecord,
    LINUX_TAR_DESTINATION, Lane, MACOS_DEPLOYMENT_FLOOR, MACOS_DESTINATION, ModelError,
    ModeledMemberKind, PRODUCT, PRODUCT_VERSION, ROOT_GID, ROOT_UID, RPM_DESTINATION, RPM_REQUIRES,
    SHA256SUMS_NAME, SIGNATURE_NAME, TargetRecord, artifacts_for_lane, checksummed_names,
    complete_candidate_names, install_instructions, modeled_members, parse_sha256sums,
    render_sha256sums,
};
use serde_json::json;

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";

#[test]
fn package_model_is_the_exact_release_set() {
    assert_eq!(PRODUCT, "solstone-tmux");
    assert_eq!(PRODUCT_VERSION, env!("CARGO_PKG_VERSION"));
    assert_eq!(ARTIFACTS.len(), 8);
    assert_eq!(MACOS_DEPLOYMENT_FLOOR, (14, 0));

    let linux_x86 = artifacts_for_lane(Lane::LinuxX86_64);
    assert_eq!(
        linux_x86
            .iter()
            .map(|artifact| artifact.name.clone())
            .collect::<Vec<_>>(),
        [
            format!("{PRODUCT}-{PRODUCT_VERSION}-x86_64-linux.tar.gz"),
            format!("{PRODUCT}_{PRODUCT_VERSION}_amd64.deb"),
            format!("{PRODUCT}-{PRODUCT_VERSION}-1.x86_64.rpm"),
        ]
    );
    assert_eq!(
        artifacts_for_lane(Lane::LinuxAarch64)
            .iter()
            .map(|artifact| artifact.name.clone())
            .collect::<Vec<_>>(),
        [
            format!("{PRODUCT}-{PRODUCT_VERSION}-aarch64-linux.tar.gz"),
            format!("{PRODUCT}_{PRODUCT_VERSION}_arm64.deb"),
            format!("{PRODUCT}-{PRODUCT_VERSION}-1.aarch64.rpm"),
        ]
    );
    assert_eq!(
        artifacts_for_lane(Lane::MacosAarch64)
            .iter()
            .map(|artifact| artifact.name.clone())
            .collect::<Vec<_>>(),
        [
            format!("{PRODUCT}-{PRODUCT_VERSION}-aarch64-macos.tar.gz"),
            format!("{PRODUCT}-{PRODUCT_VERSION}-aarch64-macos.pkg"),
        ]
    );

    let checked = checksummed_names();
    assert_eq!(checked.len(), 11);
    assert!(checked.windows(2).all(|pair| pair[0] < pair[1]));
    let complete = complete_candidate_names();
    assert_eq!(complete.len(), 13);
    assert!(complete.contains(&SHA256SUMS_NAME.to_owned()));
    assert!(complete.contains(&SIGNATURE_NAME.to_owned()));
}

#[test]
fn executable_architecture_elf_types_match_release_lanes() {
    // ET_EXEC (2) on both Linux lanes, verified against the real 1.0.1 artifacts. The x86_64
    // target spec requests static PIE, so reading the spec alone predicts 3. The release lanes
    // link through cargo-zigbuild, which does not honour that request, so the shipped binary is
    // ET_EXEC. See ExecutableArchitecture::elf_type for the full note.
    assert_eq!(ExecutableArchitecture::ElfX86_64.elf_type(), Some(2));
    assert_eq!(ExecutableArchitecture::ElfAarch64.elf_type(), Some(2));
    assert_eq!(ExecutableArchitecture::MachOAarch64.elf_type(), None);
}

#[test]
fn package_members_fix_destinations_metadata_and_dependency_text() {
    for artifact in ARTIFACTS.iter() {
        let members = modeled_members(artifact, 1_700_000_000);
        assert!(members.iter().all(|member| {
            member.kind == ModeledMemberKind::Regular
                && member.uid == ROOT_UID
                && member.gid == ROOT_GID
                && member.mtime == 1_700_000_000
        }));
        match artifact.kind {
            ArtifactKind::TarGz => {
                let expected_destination = if artifact.lane == Lane::MacosAarch64 {
                    MACOS_DESTINATION
                } else {
                    LINUX_TAR_DESTINATION
                };
                assert_eq!(artifact.binary_destination, expected_destination);
                assert_eq!(artifact.tmux_prerequisite, "tmux");
                assert_eq!(
                    members
                        .iter()
                        .map(|member| (member.path, member.mode))
                        .collect::<Vec<_>>(),
                    [
                        (EXECUTABLE_NAME, EXECUTABLE_MODE),
                        ("INSTALL.md", DOCUMENT_MODE),
                    ]
                );
                let instructions = install_instructions(artifact.lane);
                assert!(instructions.contains("requires tmux"));
                assert!(instructions.contains(artifact.binary_destination));
            }
            ArtifactKind::Deb => {
                assert_eq!(artifact.binary_destination, DEB_DESTINATION);
                assert_eq!(artifact.tmux_prerequisite, DEB_DEPENDS);
                assert_eq!(members[0].path, "usr/bin/solstone-tmux");
            }
            ArtifactKind::Rpm => {
                assert_eq!(artifact.binary_destination, RPM_DESTINATION);
                assert_eq!(artifact.tmux_prerequisite, RPM_REQUIRES);
                assert_eq!(members[0].path, "usr/bin/solstone-tmux");
            }
            ArtifactKind::Pkg => {
                assert_eq!(artifact.binary_destination, MACOS_DESTINATION);
                assert_eq!(members[0].path, "usr/local/bin/solstone-tmux");
            }
        }
    }
}

#[test]
fn target_record_requires_the_exact_canonical_schema() {
    let lane = Lane::LinuxX86_64;
    let artifact_names = artifacts_for_lane(lane)
        .iter()
        .map(|artifact| artifact.name.clone())
        .collect::<Vec<_>>();
    let record = valid_record(lane, &artifact_names);
    let expected = artifact_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(record.validate(lane, &expected), Ok(()));

    let mut unsorted = record.clone();
    unsorted.artifacts.swap(0, 1);
    assert_eq!(
        unsorted.validate(lane, &expected),
        Err(ModelError::ArtifactOrder)
    );

    let mut duplicate = record.clone();
    duplicate.artifacts[1] = duplicate.artifacts[0].clone();
    assert_eq!(
        duplicate.validate(lane, &expected),
        Err(ModelError::ArtifactDuplicate)
    );

    let mut noncanonical = record.clone();
    noncanonical.executable.sha256 = DIGEST_A.to_ascii_uppercase();
    assert_eq!(
        noncanonical.validate(lane, &expected),
        Err(ModelError::Digest)
    );

    let mut extra = record.clone();
    extra.artifacts.push(ArtifactRecord {
        name: "unlisted".to_owned(),
        sha256: DIGEST_A.to_owned(),
    });
    assert_eq!(
        extra.validate(lane, &expected),
        Err(ModelError::ArtifactSet)
    );

    let mut value = serde_json::to_value(record).expect("serialize target record");
    value
        .as_object_mut()
        .expect("record object")
        .insert("unknown".to_owned(), json!(true));
    assert!(serde_json::from_value::<TargetRecord>(value).is_err());
}

#[test]
fn sha256sums_has_one_strict_canonical_encoding() {
    let mut digests = BTreeMap::new();
    digests.insert("a.tar.gz".to_owned(), DIGEST_A.to_owned());
    digests.insert("b.target.json".to_owned(), DIGEST_B.to_owned());
    let rendered = render_sha256sums(&digests).expect("render sums");
    assert_eq!(
        rendered,
        format!("{DIGEST_A}  a.tar.gz\n{DIGEST_B}  b.target.json\n").as_bytes()
    );
    assert_eq!(parse_sha256sums(&rendered), Ok(digests));

    for invalid in [
        format!("{DIGEST_A} *a.tar.gz\n"),
        format!("{DIGEST_A}  dir/a.tar.gz\n"),
        format!("{DIGEST_A}  #comment\n"),
        format!("{DIGEST_A}  a.tar.gz\n\n"),
        format!("{DIGEST_A}  a.tar.gz"),
        format!("{}  a.tar.gz\n", DIGEST_A.to_ascii_uppercase()),
        format!("{DIGEST_B}  b\n{DIGEST_A}  a\n"),
        format!("{DIGEST_A}  a\n{DIGEST_A}  a\n"),
    ] {
        assert!(parse_sha256sums(invalid.as_bytes()).is_err(), "{invalid:?}");
    }
}

fn valid_record(lane: Lane, artifact_names: &[String]) -> TargetRecord {
    let mut artifacts = artifact_names
        .iter()
        .map(|name| ArtifactRecord {
            name: name.clone(),
            sha256: DIGEST_A.to_owned(),
        })
        .collect::<Vec<_>>();
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    TargetRecord {
        schema_version: 1,
        product_version: PRODUCT_VERSION.to_owned(),
        source_commit: COMMIT.to_owned(),
        rust_target: lane.rust_target().to_owned(),
        rustc_vv: "rustc 1.97.1\nhost: fixture\n".to_owned(),
        executable: ExecutableRecord {
            name: EXECUTABLE_NAME.to_owned(),
            sha256: DIGEST_B.to_owned(),
        },
        artifacts,
    }
}
