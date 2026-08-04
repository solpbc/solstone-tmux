// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[path = "support/package_model.rs"]
mod package_model;
#[path = "support/release_validator.rs"]
mod validator;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;
use minisign_verify::PublicKey;
use package_model::{
    ArtifactKind, ArtifactRecord, DEB_DEPENDS, EXECUTABLE_NAME, ExecutableArchitecture,
    ExecutableRecord, Lane, PRODUCT, PRODUCT_VERSION, SHA256SUMS_NAME, SIGNATURE_NAME,
    TargetRecord, artifacts_for_lane, checksummed_names, install_instructions, render_sha256sums,
};
use sha2::{Digest, Sha256};
use validator::{
    ValidationError, run_version_probe_for_test, validate_complete_files_for_test,
    validate_complete_files_with_hooks_for_test, validate_complete_set,
    validate_executable_for_test, validate_linux_lane, validate_minisign, validate_unsigned_set,
};

const EPOCH: u64 = 1_700_000_000;
const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FIXTURE_ROOT: &str = "tests/data/packaging/minisign";
const ELF_FIXTURE_ROOT: &str = "tests/data/elf";

#[test]
fn complete_candidate_fixture_is_accepted() {
    validate_complete_files_for_test(&complete_fixture(), EPOCH)
        .expect("complete fixture should validate");
}

#[test]
fn unauthenticated_candidates_are_rejected_without_executing_a_probe() {
    let signature_probe_called = Cell::new(false);
    assert_eq!(
        validate_complete_files_with_hooks_for_test(
            &complete_fixture(),
            EPOCH,
            |_payload, _signature| Err(ValidationError::SignatureInvalid),
            |_executable| {
                signature_probe_called.set(true);
                Ok(Vec::new())
            },
        ),
        Err(ValidationError::SignatureInvalid)
    );
    assert!(!signature_probe_called.get());

    let mut invalid_digest = complete_fixture();
    let rpm = artifact(Lane::LinuxX86_64, ArtifactKind::Rpm);
    invalid_digest
        .get_mut(&rpm.name)
        .expect("RPM fixture")
        .push(0);
    let digest_probe_called = Cell::new(false);
    assert_eq!(
        validate_complete_files_with_hooks_for_test(
            &invalid_digest,
            EPOCH,
            |_payload, _signature| Ok(()),
            |_executable| {
                digest_probe_called.set(true);
                Ok(Vec::new())
            },
        ),
        Err(ValidationError::DigestMismatch)
    );
    assert!(!digest_probe_called.get());
}

#[test]
fn aggregate_validation_accepts_compiler_split_version_template() {
    let mut files = complete_fixture();
    for lane in Lane::ALL {
        let mut executable = match lane {
            Lane::LinuxX86_64 => elf_fixture(62),
            Lane::LinuxAarch64 => elf_fixture(183),
            Lane::MacosAarch64 => macho_fixture(14, 0),
        };
        executable.truncate(executable.len() - version_line().len());
        executable.extend_from_slice(format!("{PRODUCT} {PRODUCT_VERSION} (source ").as_bytes());
        executable.extend_from_slice(b"\xc0\x01)\0");
        executable.extend_from_slice(COMMIT.as_bytes());
        replace_executable_and_tar(&mut files, lane, executable);
    }

    assert_eq!(
        validate_complete_files_with_hooks_for_test(
            &files,
            EPOCH,
            |_payload, _signature| Ok(()),
            |_executable| Ok(version_line().into_bytes()),
        ),
        Ok(())
    );
}

#[test]
fn native_version_probe_closes_the_staged_executable_before_running_it() {
    let output = run_version_probe_for_test(b"#!/bin/sh\nprintf 'probe passed\\n'\n")
        .expect("staged executable should run");
    assert_eq!(output, b"probe passed\n");
}

#[test]
fn committed_x86_64_elf_fixtures_have_expected_verdicts() {
    let static_pie = committed_elf_fixture("x86_64/static-pie.bin");
    assert_eq!(
        validate_executable_for_test(&static_pie, ExecutableArchitecture::ElfX86_64),
        Ok(())
    );

    let dynamic_interpreter = committed_elf_fixture("x86_64/dynamic-interp.bin");
    assert_eq!(
        validate_executable_for_test(&dynamic_interpreter, ExecutableArchitecture::ElfX86_64),
        Err(ValidationError::ExecutableInterpreter)
    );
    assert_eq!(maximum_version_marker(&dynamic_interpreter), Some((2, 34)));

    let shared_object = committed_elf_fixture("x86_64/shared-nodeps.so");
    assert_eq!(
        validate_executable_for_test(&shared_object, ExecutableArchitecture::ElfX86_64),
        Err(ValidationError::ExecutableEntryPoint)
    );

    let relocatable = committed_elf_fixture("x86_64/relocatable.o");
    assert_eq!(
        validate_executable_for_test(&relocatable, ExecutableArchitecture::ElfX86_64),
        Err(ValidationError::ExecutableType)
    );
}

#[test]
fn elf_lane_type_expectations_are_not_swappable() {
    assert_eq!(
        validate_executable_for_test(
            &committed_elf_fixture("x86_64/static-pie.bin"),
            ExecutableArchitecture::ElfX86_64,
        ),
        Ok(())
    );
    assert_eq!(
        validate_executable_for_test(
            &committed_elf_fixture("x86_64/static-exec.bin"),
            ExecutableArchitecture::ElfX86_64,
        ),
        Err(ValidationError::ExecutableType)
    );
    assert_eq!(
        validate_executable_for_test(
            &committed_elf_fixture("aarch64/static-exec.bin"),
            ExecutableArchitecture::ElfAarch64,
        ),
        Ok(())
    );
    assert_eq!(
        validate_executable_for_test(
            &committed_elf_fixture("aarch64/static-pie.bin"),
            ExecutableArchitecture::ElfAarch64,
        ),
        Err(ValidationError::ExecutableType)
    );
}

#[test]
fn program_headers_are_required_for_static_executables() {
    let static_executable = committed_elf_fixture("aarch64/static-exec.bin");
    assert_eq!(
        validate_executable_for_test(&static_executable, ExecutableArchitecture::ElfAarch64),
        Ok(())
    );

    let mut no_headers = static_executable.clone();
    set_elf_u16(&mut no_headers, 56, 0);
    assert_eq!(
        validate_executable_for_test(&no_headers, ExecutableArchitecture::ElfAarch64),
        Err(ValidationError::ExecutableProgramHeaders)
    );
}

#[test]
fn program_headers_must_describe_a_loadable_image() {
    let static_pie = committed_elf_fixture("x86_64/static-pie.bin");
    assert!(elf_program_header_count(&static_pie) > 0);
    assert!(
        elf_program_header_offsets(&static_pie)
            .into_iter()
            .any(|offset| elf_u32(&static_pie, offset) == 1)
    );

    let mut no_load = static_pie;
    for offset in elf_program_header_offsets(&no_load) {
        if elf_u32(&no_load, offset) == 1 {
            set_elf_u32(&mut no_load, offset, 0);
        }
    }
    assert_eq!(
        validate_executable_for_test(&no_load, ExecutableArchitecture::ElfX86_64),
        Err(ValidationError::ExecutableProgramHeaders)
    );
}

#[test]
fn program_header_validation_ignores_section_headers() {
    let mut fixture = committed_elf_fixture("x86_64/static-pie.bin");
    set_elf_u64(&mut fixture, 40, 0);
    set_elf_u16(&mut fixture, 60, 0);
    assert_eq!(
        validate_executable_for_test(&fixture, ExecutableArchitecture::ElfX86_64),
        Ok(())
    );
}

#[test]
fn malformed_elf_program_header_layouts_fail_closed() {
    let fixture = committed_elf_fixture("x86_64/static-pie.bin");
    let mut past_end = fixture.clone();
    let past_end_offset = past_end.len() as u64 + 1;
    set_elf_u64(&mut past_end, 32, past_end_offset);

    let mut bad_entry_size = fixture.clone();
    set_elf_u16(&mut bad_entry_size, 54, 55);

    let mut dynamic_past_end = fixture;
    let dynamic_header = elf_program_header_offsets(&dynamic_past_end)
        .into_iter()
        .find(|offset| elf_u32(&dynamic_past_end, *offset) == 2)
        .expect("dynamic program header");
    let dynamic_past_end_offset = dynamic_past_end.len() as u64;
    set_elf_u64(
        &mut dynamic_past_end,
        dynamic_header + 8,
        dynamic_past_end_offset,
    );
    set_elf_u64(&mut dynamic_past_end, dynamic_header + 32, 1);

    for malformed in [past_end, bad_entry_size, dynamic_past_end] {
        assert_eq!(
            validate_executable_for_test(&malformed, ExecutableArchitecture::ElfX86_64),
            Err(ValidationError::ExecutableElfLayout)
        );
    }
}

#[test]
fn dynamic_tags_have_specific_rejections() {
    let dynamic_interpreter = committed_elf_fixture("x86_64/dynamic-interp.bin");

    let mut needed_library = dynamic_interpreter.clone();
    neutralize_program_header_type(&mut needed_library, 3);
    assert_eq!(
        validate_executable_for_test(&needed_library, ExecutableArchitecture::ElfX86_64),
        Err(ValidationError::ExecutableNeededLibraries)
    );

    let mut version_requirement = dynamic_interpreter;
    neutralize_program_header_type(&mut version_requirement, 3);
    for offset in dynamic_entry_offsets(&version_requirement) {
        if elf_i64(&version_requirement, offset) == 1 {
            set_elf_i64(&mut version_requirement, offset, 21);
        }
    }
    assert!(
        dynamic_entry_offsets(&version_requirement)
            .into_iter()
            .any(|offset| elf_i64(&version_requirement, offset) == 0x6fff_fffe)
    );
    assert_eq!(
        validate_executable_for_test(&version_requirement, ExecutableArchitecture::ElfX86_64),
        Err(ValidationError::ExecutableVersionRequirements)
    );
}

#[test]
fn synthetic_elf_fixture_has_real_program_headers() {
    for (machine, architecture) in [
        (62, ExecutableArchitecture::ElfX86_64),
        (183, ExecutableArchitecture::ElfAarch64),
    ] {
        let fixture = elf_fixture(machine);
        assert!(elf_program_header_count(&fixture) > 0);
        assert!(
            elf_program_header_offsets(&fixture)
                .into_iter()
                .any(|offset| elf_u32(&fixture, offset) == 1)
        );
        assert_eq!(validate_executable_for_test(&fixture, architecture), Ok(()));
    }
}

#[test]
fn archive_mutations_have_specific_rejections() {
    let cases = [
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::Traversal),
            ValidationError::ArchivePath,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::Symlink),
            ValidationError::ArchiveType,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::Extra),
            ValidationError::ArchiveMembers,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::WrongMode),
            ValidationError::ArchiveMetadata,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::Config),
            ValidationError::ForbiddenMember,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::Credentials),
            ValidationError::ForbiddenMember,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::Observer),
            ValidationError::ForbiddenMember,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::Health),
            ValidationError::ForbiddenMember,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::Captures),
            ValidationError::ForbiddenMember,
        ),
        (
            tar_fixture(Lane::LinuxX86_64, elf_fixture(62), TarMutation::PrivateKey),
            ValidationError::ForbiddenMember,
        ),
    ];
    for (replacement, expected) in cases {
        let mut files = complete_fixture();
        replace_artifact(
            &mut files,
            Lane::LinuxX86_64,
            ArtifactKind::TarGz,
            replacement,
        );
        assert_eq!(
            validate_complete_files_for_test(&files, EPOCH),
            Err(expected)
        );
    }
}

#[test]
fn architecture_and_platform_floor_mutations_have_specific_rejections() {
    let mut declared = complete_fixture();
    replace_artifact(
        &mut declared,
        Lane::LinuxX86_64,
        ArtifactKind::Deb,
        deb_fixture(Lane::LinuxX86_64, elf_fixture(62), "arm64"),
    );
    assert_eq!(
        validate_complete_files_for_test(&declared, EPOCH),
        Err(ValidationError::DeclaredArchitecture)
    );

    let mut elf_machine = complete_fixture();
    let wrong_elf = elf_fixture(183);
    replace_executable_and_tar(&mut elf_machine, Lane::LinuxX86_64, wrong_elf);
    assert_eq!(
        validate_complete_files_for_test(&elf_machine, EPOCH),
        Err(ValidationError::ExecutableArchitecture)
    );

    let mut macos = complete_fixture();
    let newer_macos = macho_fixture(15, 0);
    replace_executable_and_tar(&mut macos, Lane::MacosAarch64, newer_macos);
    assert_eq!(
        validate_complete_files_for_test(&macos, EPOCH),
        Err(ValidationError::MacosFloor)
    );
}

#[test]
fn byte_checksum_record_and_set_mutations_are_rejected() {
    let mut executable = complete_fixture();
    let tar = artifact(Lane::LinuxX86_64, ArtifactKind::TarGz);
    let mut changed_binary = elf_fixture(62);
    changed_binary.push(0);
    executable.insert(
        tar.name.clone(),
        tar_fixture(Lane::LinuxX86_64, changed_binary, TarMutation::None),
    );
    refresh_record_artifacts(&mut executable, Lane::LinuxX86_64, None);
    refresh_sums(&mut executable);
    assert_eq!(
        validate_complete_files_for_test(&executable, EPOCH),
        Err(ValidationError::ExecutableDigest)
    );

    let mut outer = complete_fixture();
    let rpm = artifact(Lane::LinuxX86_64, ArtifactKind::Rpm);
    outer.get_mut(&rpm.name).expect("rpm fixture").push(0);
    assert_eq!(
        validate_complete_files_for_test(&outer, EPOCH),
        Err(ValidationError::DigestMismatch)
    );

    let mut checksum = complete_fixture();
    checksum.get_mut(SHA256SUMS_NAME).expect("checksum fixture")[0] = b'A';
    assert_eq!(
        validate_complete_files_for_test(&checksum, EPOCH),
        Err(ValidationError::ChecksumSyntax)
    );

    let mut record = complete_fixture();
    let record_name = Lane::LinuxX86_64.record_name();
    let mut record_value: serde_json::Value =
        serde_json::from_slice(record.get(&record_name).expect("record fixture"))
            .expect("decode record");
    record_value
        .as_object_mut()
        .expect("record object")
        .insert("unknown".to_owned(), serde_json::Value::Bool(true));
    record.insert(
        record_name,
        serde_json::to_vec(&record_value).expect("encode record"),
    );
    refresh_sums(&mut record);
    assert_eq!(
        validate_complete_files_for_test(&record, EPOCH),
        Err(ValidationError::RecordSchema)
    );

    let mut missing = complete_fixture();
    missing.remove(&artifact(Lane::MacosAarch64, ArtifactKind::Pkg).name);
    assert_eq!(
        validate_complete_files_for_test(&missing, EPOCH),
        Err(ValidationError::CandidateSet)
    );
}

#[test]
fn candidate_canaries_are_scoped_and_rejected() {
    for canary in [
        "LEGACYKEYCANARY-do-not-copy",
        "SOLSTONE_TMUX_SECRET_CANARY_DO_NOT_SHIP",
        "MINISIGN_SECRET_CANARY_DO_NOT_SHIP",
    ] {
        let mut files = complete_fixture();
        let rpm = artifact(Lane::LinuxX86_64, ArtifactKind::Rpm);
        files.insert(rpm.name.clone(), canary.as_bytes().to_vec());
        refresh_record_artifacts(&mut files, Lane::LinuxX86_64, None);
        refresh_sums(&mut files);
        assert_eq!(
            validate_complete_files_for_test(&files, EPOCH),
            Err(ValidationError::SecretCanary)
        );
    }

    assert_eq!(
        validate_linux_lane(
            Path::new("not-inspected"),
            Lane::LinuxX86_64.rust_target(),
            Path::new("not-inspected"),
            b"SOLSTONE_TMUX_SECRET_CANARY_DO_NOT_SHIP",
        ),
        Err(ValidationError::SecretCanary)
    );

    let mut extracted = complete_fixture();
    let canary_tar = tar_with_install(elf_fixture(62), b"MINISIGN_SECRET_CANARY_DO_NOT_SHIP");
    replace_artifact(
        &mut extracted,
        Lane::LinuxX86_64,
        ArtifactKind::TarGz,
        canary_tar,
    );
    assert_eq!(
        validate_complete_files_for_test(&extracted, EPOCH),
        Err(ValidationError::SecretCanary)
    );
}

#[test]
fn minisign_fixtures_verify_cryptographically() {
    let public_key = fixture("test-only.pub");
    let payload = fixture("payload.txt");
    let signature = fixture("payload.txt.minisig");
    validate_minisign(&public_key, &payload, &signature).expect("valid test signature");
    assert_eq!(
        validate_minisign(&public_key, &fixture("payload-mutated.txt"), &signature),
        Err(ValidationError::SignatureInvalid)
    );
    assert_eq!(
        validate_minisign(&public_key, &payload, &fixture("signature-mutated.minisig")),
        Err(ValidationError::SignatureInvalid)
    );
}

#[test]
fn release_key_placeholder_fails_closed() {
    assert_eq!(
        validate_minisign(
            b"untrusted comment: PLACEHOLDER ONLY\nNOT_A_MINISIGN_PUBLIC_KEY\n",
            b"payload\n",
            b"signature\n",
        ),
        Err(ValidationError::SignaturePlaceholder)
    );
    assert!(
        ValidationError::SignaturePlaceholder
            .to_string()
            .contains("release operator must replace")
    );
}

#[test]
fn committed_release_key_is_a_real_minisign_key() {
    let public_key = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/keys/solstone-tmux-release.pub"),
    )
    .expect("read committed release public key");
    let public_key_text =
        std::str::from_utf8(&public_key).expect("committed release public key should be UTF-8");
    assert!(
        public_key_text.ends_with('\n')
            && !public_key_text.contains('\r')
            && public_key_text.lines().count() == 2,
        "committed release public key should be exactly two LF-terminated lines"
    );
    PublicKey::decode(public_key_text).expect("committed release public key should decode");
    assert_eq!(
        validate_minisign(
            &public_key,
            &fixture("payload.txt"),
            &fixture("payload.txt.minisig"),
        ),
        Err(ValidationError::SignatureInvalid)
    );
}

#[test]
fn validates_real_linux_lane_when_requested() {
    let Ok(candidate) = std::env::var("SOLSTONE_TMUX_TEST_CANDIDATE") else {
        return;
    };
    let target = std::env::var("SOLSTONE_TMUX_TEST_TARGET").expect("candidate target");
    let executable = std::env::var("SOLSTONE_TMUX_TEST_EXECUTABLE").expect("candidate executable");
    let captured = std::env::var("SOLSTONE_TMUX_TEST_PACKAGER_LOG").unwrap_or_default();
    let result = validate_linux_lane(
        Path::new(&candidate),
        &target,
        Path::new(&executable),
        captured.as_bytes(),
    );
    if let Ok(expected) = std::env::var("SOLSTONE_TMUX_TEST_EXPECTED_ERROR") {
        assert_eq!(
            result
                .expect_err("real Linux lane should reject")
                .to_string(),
            expected
        );
    } else {
        result.expect("real Linux lane should validate");
    }
}

#[test]
fn validates_real_complete_set_when_requested() {
    let Ok(candidate) = std::env::var("SOLSTONE_TMUX_TEST_COMPLETE_CANDIDATE") else {
        return;
    };
    validate_complete_set(Path::new(&candidate)).expect("real complete candidate should validate");
}

#[test]
fn validates_real_unsigned_set_when_requested() {
    let Ok(candidate) = std::env::var("SOLSTONE_TMUX_TEST_UNSIGNED_CANDIDATE") else {
        return;
    };
    validate_unsigned_set(Path::new(&candidate)).expect("real unsigned candidate should validate");
}

fn complete_fixture() -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for lane in Lane::ALL {
        let executable = match lane {
            Lane::LinuxX86_64 => elf_fixture(62),
            Lane::LinuxAarch64 => elf_fixture(183),
            Lane::MacosAarch64 => macho_fixture(14, 0),
        };
        for spec in artifacts_for_lane(lane) {
            let bytes = match spec.kind {
                ArtifactKind::TarGz => tar_fixture(lane, executable.clone(), TarMutation::None),
                ArtifactKind::Deb => {
                    deb_fixture(lane, executable.clone(), spec.declared_architecture)
                }
                ArtifactKind::Rpm => {
                    format!("test-only RPM bytes for {}\n", spec.name).into_bytes()
                }
                ArtifactKind::Pkg => {
                    format!("test-only pkg bytes for {}\n", spec.name).into_bytes()
                }
            };
            files.insert(spec.name, bytes);
        }
        insert_record(&mut files, lane, &executable);
    }
    refresh_sums(&mut files);
    files.insert(SIGNATURE_NAME.to_owned(), b"test-only signature\n".to_vec());
    files
}

fn insert_record(files: &mut BTreeMap<String, Vec<u8>>, lane: Lane, executable: &[u8]) {
    let mut artifacts = artifacts_for_lane(lane)
        .iter()
        .map(|artifact| ArtifactRecord {
            name: artifact.name.clone(),
            sha256: sha256_hex(files.get(&artifact.name).expect("artifact fixture")),
        })
        .collect::<Vec<_>>();
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    let record = TargetRecord {
        schema_version: 1,
        product_version: PRODUCT_VERSION.to_owned(),
        source_commit: COMMIT.to_owned(),
        rust_target: lane.rust_target().to_owned(),
        rustc_vv: "rustc 1.97.1\nbinary: test-only\n".to_owned(),
        executable: ExecutableRecord {
            name: EXECUTABLE_NAME.to_owned(),
            sha256: sha256_hex(executable),
        },
        artifacts,
    };
    files.insert(
        lane.record_name(),
        serde_json::to_vec(&record).expect("serialize record"),
    );
}

fn refresh_record_artifacts(
    files: &mut BTreeMap<String, Vec<u8>>,
    lane: Lane,
    executable: Option<&[u8]>,
) {
    let name = lane.record_name();
    let mut record: TargetRecord =
        serde_json::from_slice(files.get(&name).expect("record fixture")).expect("decode record");
    for artifact in &mut record.artifacts {
        artifact.sha256 = sha256_hex(files.get(&artifact.name).expect("artifact fixture"));
    }
    if let Some(executable) = executable {
        record.executable.sha256 = sha256_hex(executable);
    }
    files.insert(name, serde_json::to_vec(&record).expect("encode record"));
}

fn refresh_sums(files: &mut BTreeMap<String, Vec<u8>>) {
    let digests = checksummed_names()
        .into_iter()
        .map(|name| {
            let digest = sha256_hex(files.get(&name).expect("checksummed fixture"));
            (name, digest)
        })
        .collect::<BTreeMap<_, _>>();
    files.insert(
        SHA256SUMS_NAME.to_owned(),
        render_sha256sums(&digests).expect("render fixture sums"),
    );
}

fn replace_artifact(
    files: &mut BTreeMap<String, Vec<u8>>,
    lane: Lane,
    kind: ArtifactKind,
    bytes: Vec<u8>,
) {
    let spec = artifact(lane, kind);
    files.insert(spec.name, bytes);
    refresh_record_artifacts(files, lane, None);
    refresh_sums(files);
}

fn replace_executable_and_tar(
    files: &mut BTreeMap<String, Vec<u8>>,
    lane: Lane,
    executable: Vec<u8>,
) {
    let tar = artifact(lane, ArtifactKind::TarGz);
    files.insert(
        tar.name,
        tar_fixture(lane, executable.clone(), TarMutation::None),
    );
    if lane != Lane::MacosAarch64 {
        let deb = artifact(lane, ArtifactKind::Deb);
        files.insert(
            deb.name,
            deb_fixture(lane, executable.clone(), deb.declared_architecture),
        );
    }
    refresh_record_artifacts(files, lane, Some(&executable));
    refresh_sums(files);
}

fn artifact(lane: Lane, kind: ArtifactKind) -> package_model::ArtifactSpec {
    artifacts_for_lane(lane)
        .into_iter()
        .find(|artifact| artifact.kind == kind)
        .expect("modeled artifact")
}

fn elf_fixture(machine: u16) -> Vec<u8> {
    let has_dynamic = machine != 183;
    let program_header_count = if has_dynamic { 2 } else { 1 };
    let program_header_offset = 64usize;
    let program_header_size = 56usize;
    let dynamic_offset = program_header_offset + program_header_count * program_header_size;
    let mut bytes = vec![0u8; dynamic_offset];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    let elf_type = if has_dynamic { 3u16 } else { 2u16 };
    bytes[16..18].copy_from_slice(&elf_type.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[24..32].copy_from_slice(&0x1000u64.to_le_bytes());
    bytes[32..40].copy_from_slice(&(program_header_offset as u64).to_le_bytes());
    bytes[54..56].copy_from_slice(&(program_header_size as u16).to_le_bytes());
    bytes[56..58].copy_from_slice(&(program_header_count as u16).to_le_bytes());

    bytes[program_header_offset..program_header_offset + 4].copy_from_slice(&1u32.to_le_bytes());
    if has_dynamic {
        let dynamic_header_offset = program_header_offset + program_header_size;
        bytes[dynamic_header_offset..dynamic_header_offset + 4]
            .copy_from_slice(&2u32.to_le_bytes());
        bytes[dynamic_header_offset + 8..dynamic_header_offset + 16]
            .copy_from_slice(&(dynamic_offset as u64).to_le_bytes());
        bytes[dynamic_header_offset + 32..dynamic_header_offset + 40]
            .copy_from_slice(&16u64.to_le_bytes());
        bytes[dynamic_header_offset + 40..dynamic_header_offset + 48]
            .copy_from_slice(&16u64.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 16]);
    }
    bytes.extend_from_slice(version_line().as_bytes());
    let load_size = bytes.len() as u64;
    bytes[program_header_offset + 32..program_header_offset + 40]
        .copy_from_slice(&load_size.to_le_bytes());
    bytes[program_header_offset + 40..program_header_offset + 48]
        .copy_from_slice(&load_size.to_le_bytes());
    bytes
}

fn macho_fixture(major: u32, minor: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 56];
    bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    bytes[4..8].copy_from_slice(&0x0100_000cu32.to_le_bytes());
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    bytes[20..24].copy_from_slice(&24u32.to_le_bytes());
    bytes[32..36].copy_from_slice(&0x32u32.to_le_bytes());
    bytes[36..40].copy_from_slice(&24u32.to_le_bytes());
    let deployment = (major << 16) | (minor << 8);
    bytes[44..48].copy_from_slice(&deployment.to_le_bytes());
    bytes.extend_from_slice(version_line().as_bytes());
    bytes
}

fn version_line() -> String {
    format!("{PRODUCT} {PRODUCT_VERSION} (source {COMMIT})\n")
}

#[derive(Clone, Copy)]
enum TarMutation {
    None,
    Traversal,
    Symlink,
    Extra,
    WrongMode,
    Config,
    Credentials,
    Observer,
    Health,
    Captures,
    PrivateKey,
}

fn tar_fixture(lane: Lane, executable: Vec<u8>, mutation: TarMutation) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::best());
    let mut builder = tar::Builder::new(encoder);
    let executable_path = match mutation {
        TarMutation::Traversal => "../solstone-tmux",
        TarMutation::Config => "config.json",
        TarMutation::Credentials => "credentials.json",
        TarMutation::Observer => "observer.json",
        TarMutation::Health => "health.json",
        TarMutation::Captures => "captures",
        TarMutation::PrivateKey => "release.key",
        _ => EXECUTABLE_NAME,
    };
    let entry_type = if matches!(mutation, TarMutation::Symlink) {
        tar::EntryType::Symlink
    } else {
        tar::EntryType::Regular
    };
    append_tar_entry(
        &mut builder,
        executable_path,
        if matches!(mutation, TarMutation::Symlink) {
            &[]
        } else {
            &executable
        },
        if matches!(mutation, TarMutation::WrongMode) {
            0o644
        } else {
            0o755
        },
        entry_type,
    );
    append_tar_entry(
        &mut builder,
        "INSTALL.md",
        install_instructions(lane).as_bytes(),
        0o644,
        tar::EntryType::Regular,
    );
    if matches!(mutation, TarMutation::Extra) {
        append_tar_entry(
            &mut builder,
            "extra",
            b"extra\n",
            0o644,
            tar::EntryType::Regular,
        );
    }
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish gzip")
}

fn tar_with_install(executable: Vec<u8>, instructions: &[u8]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::best());
    let mut builder = tar::Builder::new(encoder);
    append_tar_entry(
        &mut builder,
        EXECUTABLE_NAME,
        &executable,
        0o755,
        tar::EntryType::Regular,
    );
    append_tar_entry(
        &mut builder,
        "INSTALL.md",
        instructions,
        0o644,
        tar::EntryType::Regular,
    );
    let encoder = builder.into_inner().expect("finish tar");
    encoder.finish().expect("finish gzip")
}

fn append_tar_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
    mode: u32,
    entry_type: tar::EntryType,
) {
    let mut header = tar::Header::new_gnu();
    if header.set_path(path).is_err() {
        let raw = header.as_mut_bytes();
        raw[..100].fill(0);
        raw[..path.len()].copy_from_slice(path.as_bytes());
    }
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(EPOCH);
    header.set_entry_type(entry_type);
    if entry_type.is_symlink() {
        header
            .set_link_name("elsewhere")
            .expect("set test link target");
    }
    header.set_cksum();
    builder
        .append(&header, Cursor::new(bytes))
        .expect("append tar fixture");
}

fn deb_fixture(lane: Lane, executable: Vec<u8>, declared_architecture: &str) -> Vec<u8> {
    let control = format!(
        "Package: {PRODUCT}\nVersion: {PRODUCT_VERSION}\nArchitecture: {declared_architecture}\nMaintainer: sol pbc\nDepends: {DEB_DEPENDS}\nDescription: solstone tmux observer\n"
    );
    let control_tar = inner_tar(&[("control", control.as_bytes(), 0o644)]);
    let binary_path = artifact(lane, ArtifactKind::Deb)
        .binary_destination
        .trim_start_matches('/')
        .to_owned();
    let data_tar = inner_tar(&[(binary_path.as_str(), &executable, 0o755)]);
    let mut builder = ar::Builder::new(Vec::new());
    append_ar(&mut builder, "debian-binary", b"2.0\n");
    append_ar(&mut builder, "control.tar.gz", &control_tar);
    append_ar(&mut builder, "data.tar.gz", &data_tar);
    builder.into_inner().expect("finish deb fixture")
}

fn inner_tar(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::best());
    let mut builder = tar::Builder::new(encoder);
    for (path, bytes, mode) in entries {
        append_tar_entry(&mut builder, path, bytes, *mode, tar::EntryType::Regular);
    }
    let encoder = builder.into_inner().expect("finish inner tar");
    encoder.finish().expect("finish inner gzip")
}

fn append_ar(builder: &mut ar::Builder<Vec<u8>>, name: &str, bytes: &[u8]) {
    let mut header = ar::Header::new(name.as_bytes().to_vec(), bytes.len() as u64);
    header.set_mtime(EPOCH);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mode(0o644);
    builder
        .append(&header, Cursor::new(bytes))
        .expect("append ar fixture");
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn committed_elf_fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(ELF_FIXTURE_ROOT)
            .join(name),
    )
    .expect("read ELF fixture")
}

fn elf_u16(bytes: &[u8], offset: usize) -> u16 {
    let end = offset.checked_add(2).expect("ELF u16 end");
    let value: [u8; 2] = bytes
        .get(offset..end)
        .expect("ELF u16 bytes")
        .try_into()
        .expect("ELF u16 length");
    u16::from_le_bytes(value)
}

fn elf_u32(bytes: &[u8], offset: usize) -> u32 {
    let end = offset.checked_add(4).expect("ELF u32 end");
    let value: [u8; 4] = bytes
        .get(offset..end)
        .expect("ELF u32 bytes")
        .try_into()
        .expect("ELF u32 length");
    u32::from_le_bytes(value)
}

fn elf_u64(bytes: &[u8], offset: usize) -> u64 {
    let end = offset.checked_add(8).expect("ELF u64 end");
    let value: [u8; 8] = bytes
        .get(offset..end)
        .expect("ELF u64 bytes")
        .try_into()
        .expect("ELF u64 length");
    u64::from_le_bytes(value)
}

fn elf_i64(bytes: &[u8], offset: usize) -> i64 {
    let end = offset.checked_add(8).expect("ELF i64 end");
    let value: [u8; 8] = bytes
        .get(offset..end)
        .expect("ELF i64 bytes")
        .try_into()
        .expect("ELF i64 length");
    i64::from_le_bytes(value)
}

fn set_elf_u16(bytes: &mut [u8], offset: usize, value: u16) {
    let end = offset.checked_add(2).expect("ELF u16 end");
    bytes
        .get_mut(offset..end)
        .expect("ELF u16 bytes")
        .copy_from_slice(&value.to_le_bytes());
}

fn set_elf_u32(bytes: &mut [u8], offset: usize, value: u32) {
    let end = offset.checked_add(4).expect("ELF u32 end");
    bytes
        .get_mut(offset..end)
        .expect("ELF u32 bytes")
        .copy_from_slice(&value.to_le_bytes());
}

fn set_elf_u64(bytes: &mut [u8], offset: usize, value: u64) {
    let end = offset.checked_add(8).expect("ELF u64 end");
    bytes
        .get_mut(offset..end)
        .expect("ELF u64 bytes")
        .copy_from_slice(&value.to_le_bytes());
}

fn set_elf_i64(bytes: &mut [u8], offset: usize, value: i64) {
    let end = offset.checked_add(8).expect("ELF i64 end");
    bytes
        .get_mut(offset..end)
        .expect("ELF i64 bytes")
        .copy_from_slice(&value.to_le_bytes());
}

fn elf_program_header_count(bytes: &[u8]) -> usize {
    usize::from(elf_u16(bytes, 56))
}

fn elf_program_header_offsets(bytes: &[u8]) -> Vec<usize> {
    let offset = usize::try_from(elf_u64(bytes, 32)).expect("ELF program header offset");
    let size = usize::from(elf_u16(bytes, 54));
    (0..elf_program_header_count(bytes))
        .map(|index| {
            offset
                .checked_add(index.checked_mul(size).expect("ELF program header index"))
                .expect("ELF program header offset")
        })
        .collect()
}

fn dynamic_entry_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut entries = Vec::new();
    for program_header in elf_program_header_offsets(bytes) {
        if elf_u32(bytes, program_header) != 2 {
            continue;
        }
        let offset =
            usize::try_from(elf_u64(bytes, program_header + 8)).expect("ELF dynamic offset");
        let end = offset
            .checked_add(
                usize::try_from(elf_u64(bytes, program_header + 32)).expect("ELF dynamic length"),
            )
            .expect("ELF dynamic end");
        let mut entry = offset;
        while entry.checked_add(16).is_some_and(|next| next <= end) {
            entries.push(entry);
            if elf_i64(bytes, entry) == 0 {
                break;
            }
            entry = entry.checked_add(16).expect("ELF dynamic entry");
        }
    }
    entries
}

fn neutralize_program_header_type(bytes: &mut [u8], program_header_type: u32) {
    for offset in elf_program_header_offsets(bytes) {
        if elf_u32(bytes, offset) == program_header_type {
            set_elf_u32(bytes, offset, 0);
        }
    }
}

fn maximum_version_marker(bytes: &[u8]) -> Option<(u32, u32)> {
    let marker = b"GLIBC_";
    let mut maximum = None;
    let mut offset = 0usize;
    while let Some(found) = bytes
        .get(offset..)?
        .windows(marker.len())
        .position(|window| window == marker)
    {
        let start = offset.checked_add(found)?.checked_add(marker.len())?;
        let tail = bytes.get(start..)?;
        let major_length = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if major_length > 0 && tail.get(major_length) == Some(&b'.') {
            let minor_start = major_length.checked_add(1)?;
            let minor_length = tail
                .get(minor_start..)?
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if minor_length > 0 {
                let major = std::str::from_utf8(tail.get(..major_length)?)
                    .ok()?
                    .parse()
                    .ok()?;
                let minor_end = minor_start.checked_add(minor_length)?;
                let minor = std::str::from_utf8(tail.get(minor_start..minor_end)?)
                    .ok()?
                    .parse()
                    .ok()?;
                maximum = Some(maximum.map_or((major, minor), |current: (u32, u32)| {
                    current.max((major, minor))
                }));
            }
        }
        offset = start;
    }
    maximum
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(FIXTURE_ROOT)
            .join(name),
    )
    .expect("read minisign fixture")
}
