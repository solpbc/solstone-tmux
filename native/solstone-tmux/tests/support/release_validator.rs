// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::read::GzDecoder;
use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

use super::package_model::{
    ARTIFACTS, ArtifactKind, ArtifactSpec, DEB_DEPENDS, EXECUTABLE_MODE, EXECUTABLE_NAME,
    ExecutableArchitecture, FORBIDDEN_MEMBER_BASENAMES, GLIBC_FLOOR, Lane, MACOS_DEPLOYMENT_FLOOR,
    ModelError, PRODUCT, PRODUCT_VERSION, SECRET_CANARIES, SHA256SUMS_NAME, SIGNATURE_NAME,
    TargetRecord, artifacts_for_lane, checksummed_names, complete_candidate_names,
    install_instructions, modeled_members, parse_sha256sums, render_sha256sums,
};

const MAX_CANDIDATE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_MEMBERS: usize = 64;
static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationError {
    CandidateSet,
    CandidateType,
    CandidateTooLarge,
    SecretCanary,
    ForbiddenMember,
    ChecksumSyntax,
    ChecksumSet,
    DigestMismatch,
    SignaturePlaceholder,
    SignatureInvalid,
    RecordSchema,
    Record,
    ArchiveDecode,
    ArchivePath,
    ArchiveType,
    ArchiveMembers,
    ArchiveMetadata,
    DeclaredArchitecture,
    Dependency,
    ExecutableArchitecture,
    GlibcFloor,
    MacosFloor,
    ExecutableDigest,
    SourceCommit,
    VersionOutput,
    Io,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CandidateSet => "candidate file set is incomplete or unlisted",
            Self::CandidateType => "candidate entries must be regular files",
            Self::CandidateTooLarge => "candidate or extracted member exceeds the size limit",
            Self::SecretCanary => "candidate contains a release secrets canary",
            Self::ForbiddenMember => "package contains forbidden private state",
            Self::ChecksumSyntax => "SHA256SUMS syntax is invalid",
            Self::ChecksumSet => "SHA256SUMS file set is incomplete or unlisted",
            Self::DigestMismatch => "candidate digest does not match its declared digest",
            Self::SignaturePlaceholder => {
                "release public key is still the placeholder; VPE must replace packaging/keys/solstone-tmux-release.pub"
            }
            Self::SignatureInvalid => "SHA256SUMS minisign signature is invalid",
            Self::RecordSchema => "target record schema is invalid",
            Self::Record => "target record content is invalid",
            Self::ArchiveDecode => "package archive could not be decoded",
            Self::ArchivePath => "package member path is not confined",
            Self::ArchiveType => "package member is not an allowed regular file or directory",
            Self::ArchiveMembers => "package member set is incomplete or unlisted",
            Self::ArchiveMetadata => "package member metadata is not deterministic",
            Self::DeclaredArchitecture => "package declared architecture is invalid",
            Self::Dependency => "package tmux dependency is invalid",
            Self::ExecutableArchitecture => "package executable architecture is invalid",
            Self::GlibcFloor => "package executable requires a GLIBC version newer than 2.35",
            Self::MacosFloor => "package executable deployment target is not macOS 14.0",
            Self::ExecutableDigest => "packaged executable bytes differ from the target record",
            Self::SourceCommit => "executable source commit does not match the target record",
            Self::VersionOutput => "executable --version output is not source-bound",
            Self::Io => "candidate validation I/O failed",
        })
    }
}

impl std::error::Error for ValidationError {}

pub fn validate_minisign(
    public_key_bytes: &[u8],
    payload: &[u8],
    signature_bytes: &[u8],
) -> Result<(), ValidationError> {
    let public_key_text =
        std::str::from_utf8(public_key_bytes).map_err(|_| ValidationError::SignatureInvalid)?;
    if public_key_text.contains("PLACEHOLDER") || public_key_text.contains("NOT_A_MINISIGN") {
        return Err(ValidationError::SignaturePlaceholder);
    }
    if !has_exact_lines(public_key_text, 2) {
        return Err(ValidationError::SignatureInvalid);
    }
    let signature_text =
        std::str::from_utf8(signature_bytes).map_err(|_| ValidationError::SignatureInvalid)?;
    if !has_exact_lines(signature_text, 4) {
        return Err(ValidationError::SignatureInvalid);
    }
    let public_key =
        PublicKey::decode(public_key_text).map_err(|_| ValidationError::SignatureInvalid)?;
    let signature =
        Signature::decode(signature_text).map_err(|_| ValidationError::SignatureInvalid)?;
    public_key
        .verify(payload, &signature, false)
        .map_err(|_| ValidationError::SignatureInvalid)
}

pub fn validate_complete_set(candidate_root: &Path) -> Result<(), ValidationError> {
    let files = read_candidate_directory(candidate_root, &complete_candidate_names())?;
    validate_complete_files(
        &files,
        |payload, signature| {
            let public_key = fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../packaging/keys/solstone-tmux-release.pub"),
            )
            .map_err(|_| ValidationError::Io)?;
            validate_minisign(&public_key, payload, signature)
        },
        git_source_epoch,
        run_version_probe,
    )
}

pub fn validate_unsigned_set(candidate_root: &Path) -> Result<(), ValidationError> {
    let mut files = read_candidate_directory(candidate_root, &checksummed_names())?;
    let digests = files
        .iter()
        .map(|(name, bytes)| (name.clone(), sha256_hex(bytes)))
        .collect::<BTreeMap<_, _>>();
    let sums = render_sha256sums(&digests).map_err(map_checksum_error)?;
    files.insert(SHA256SUMS_NAME.to_owned(), sums);
    files.insert(SIGNATURE_NAME.to_owned(), b"unsigned validation\n".to_vec());

    validate_complete_files(
        &files,
        |_payload, _signature| Ok(()),
        git_source_epoch,
        run_version_probe,
    )
}

pub fn validate_linux_lane(
    candidate_root: &Path,
    rust_target: &str,
    source_executable: &Path,
    captured_output: &[u8],
) -> Result<(), ValidationError> {
    scan_canaries(captured_output)?;
    let lane = Lane::from_target(rust_target).ok_or(ValidationError::Record)?;
    if lane == Lane::MacosAarch64 {
        return Err(ValidationError::Record);
    }
    let mut names = fs::read_dir(candidate_root)
        .map_err(|_| ValidationError::Io)?
        .map(|entry| {
            entry.map_err(|_| ValidationError::Io).and_then(|entry| {
                let metadata =
                    fs::symlink_metadata(entry.path()).map_err(|_| ValidationError::Io)?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(ValidationError::CandidateType);
                }
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| ValidationError::CandidateSet)
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    let record_name = lane.record_name();
    if !names.iter().any(|name| name == &record_name) {
        return Err(ValidationError::CandidateSet);
    }
    let artifact_names = names
        .iter()
        .filter(|name| name.as_str() != record_name)
        .cloned()
        .collect::<Vec<_>>();
    if artifact_names.is_empty()
        || artifact_names.iter().any(|name| {
            !artifacts_for_lane(lane)
                .iter()
                .any(|artifact| artifact.name == *name)
        })
    {
        return Err(ValidationError::CandidateSet);
    }
    let files = read_candidate_directory(candidate_root, &names)?;
    let executable = read_bounded(source_executable)?;
    scan_canaries(&executable)?;
    let output = Command::new(source_executable)
        .arg("--version")
        .output()
        .map_err(|_| ValidationError::VersionOutput)?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(ValidationError::VersionOutput);
    }
    let source_commit = parse_version_output(&output.stdout, &executable)?;
    let epoch = git_source_epoch(&source_commit)?;

    let record = decode_record(
        files
            .get(&record_name)
            .ok_or(ValidationError::CandidateSet)?,
    )?;
    let artifact_refs = artifact_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    record
        .validate(lane, &artifact_refs)
        .map_err(|_| ValidationError::Record)?;
    if source_commit != record.source_commit {
        return Err(ValidationError::SourceCommit);
    }
    if sha256_hex(&executable) != record.executable.sha256 {
        return Err(ValidationError::ExecutableDigest);
    }

    for artifact in &record.artifacts {
        let bytes = files
            .get(&artifact.name)
            .ok_or(ValidationError::CandidateSet)?;
        if sha256_hex(bytes) != artifact.sha256 {
            return Err(ValidationError::DigestMismatch);
        }
        let spec = artifacts_for_lane(lane)
            .into_iter()
            .find(|candidate| candidate.name == artifact.name)
            .ok_or(ValidationError::CandidateSet)?;
        let packaged = match spec.kind {
            ArtifactKind::TarGz => inspect_tar(bytes, &spec, Some(epoch))?.binary,
            ArtifactKind::Deb => inspect_deb(bytes, &spec, epoch)?,
            ArtifactKind::Rpm | ArtifactKind::Pkg => continue,
        };
        if packaged != executable {
            return Err(ValidationError::ExecutableDigest);
        }
    }
    validate_executable(&executable, lane.executable_architecture())?;
    Ok(())
}

pub fn validate_complete_files_for_test(
    files: &BTreeMap<String, Vec<u8>>,
    epoch: u64,
) -> Result<(), ValidationError> {
    validate_complete_files(
        files,
        |_payload, _signature| Ok(()),
        |_source_commit| Ok(epoch),
        embedded_version_output,
    )
}

pub fn validate_complete_files_with_hooks_for_test(
    files: &BTreeMap<String, Vec<u8>>,
    epoch: u64,
    verify_signature: impl Fn(&[u8], &[u8]) -> Result<(), ValidationError>,
    version_output: impl Fn(&[u8]) -> Result<Vec<u8>, ValidationError>,
) -> Result<(), ValidationError> {
    validate_complete_files(
        files,
        verify_signature,
        |_source_commit| Ok(epoch),
        version_output,
    )
}

fn validate_complete_files(
    files: &BTreeMap<String, Vec<u8>>,
    verify_signature: impl Fn(&[u8], &[u8]) -> Result<(), ValidationError>,
    source_epoch: impl Fn(&str) -> Result<u64, ValidationError>,
    version_output: impl Fn(&[u8]) -> Result<Vec<u8>, ValidationError>,
) -> Result<(), ValidationError> {
    let expected = complete_candidate_names()
        .into_iter()
        .collect::<BTreeSet<_>>();
    if files.keys().cloned().collect::<BTreeSet<_>>() != expected {
        return Err(ValidationError::CandidateSet);
    }
    for bytes in files.values() {
        if bytes.len() as u64 > MAX_CANDIDATE_BYTES {
            return Err(ValidationError::CandidateTooLarge);
        }
        scan_canaries(bytes)?;
    }
    let sums_bytes = files
        .get(SHA256SUMS_NAME)
        .ok_or(ValidationError::CandidateSet)?;
    verify_signature(
        sums_bytes,
        files
            .get(SIGNATURE_NAME)
            .ok_or(ValidationError::CandidateSet)?,
    )?;
    let sums = parse_sha256sums(sums_bytes).map_err(map_checksum_error)?;
    let expected_sums = checksummed_names().into_iter().collect::<BTreeSet<_>>();
    if sums.keys().cloned().collect::<BTreeSet<_>>() != expected_sums {
        return Err(ValidationError::ChecksumSet);
    }
    for (name, digest) in &sums {
        let bytes = files.get(name).ok_or(ValidationError::ChecksumSet)?;
        if sha256_hex(bytes) != *digest {
            return Err(ValidationError::DigestMismatch);
        }
    }
    let mut records = BTreeMap::new();
    for lane in Lane::ALL {
        let record = decode_record(
            files
                .get(&lane.record_name())
                .ok_or(ValidationError::CandidateSet)?,
        )?;
        let lane_artifacts = artifacts_for_lane(lane);
        let artifact_names = lane_artifacts
            .iter()
            .map(|artifact| artifact.name.as_str())
            .collect::<Vec<_>>();
        record
            .validate(lane, &artifact_names)
            .map_err(|_| ValidationError::Record)?;
        for artifact in &record.artifacts {
            let bytes = files
                .get(&artifact.name)
                .ok_or(ValidationError::CandidateSet)?;
            if sha256_hex(bytes) != artifact.sha256 {
                return Err(ValidationError::DigestMismatch);
            }
        }
        records.insert(lane, record);
    }

    let host_lane = Lane::LinuxX86_64;
    let mut source_commit = None;
    for lane in Lane::ALL {
        let tar = ARTIFACTS
            .iter()
            .find(|artifact| artifact.lane == lane && artifact.kind == ArtifactKind::TarGz)
            .ok_or(ValidationError::CandidateSet)?;
        let executable = inspect_tar(
            files.get(&tar.name).ok_or(ValidationError::CandidateSet)?,
            tar,
            None,
        )?
        .binary;
        let record = records.get(&lane).ok_or(ValidationError::Record)?;
        if record.executable.sha256 != sha256_hex(&executable) {
            return Err(ValidationError::ExecutableDigest);
        }
        let commit = parse_version_output(&embedded_version_output(&executable)?, &executable)?;
        if source_commit
            .as_ref()
            .is_some_and(|existing| existing != &commit)
        {
            return Err(ValidationError::SourceCommit);
        }
        source_commit = Some(commit);
    }
    let source_commit = source_commit.ok_or(ValidationError::SourceCommit)?;
    for record in records.values() {
        if record.source_commit != source_commit {
            return Err(ValidationError::SourceCommit);
        }
    }
    let epoch = source_epoch(&source_commit)?;

    let mut validated_host_executable = None;
    for spec in ARTIFACTS.iter() {
        let bytes = files.get(&spec.name).ok_or(ValidationError::CandidateSet)?;
        let packaged = match spec.kind {
            ArtifactKind::TarGz => Some(inspect_tar(bytes, spec, Some(epoch))?.binary),
            ArtifactKind::Deb => Some(inspect_deb(bytes, spec, epoch)?),
            ArtifactKind::Rpm | ArtifactKind::Pkg => None,
        };
        let Some(executable) = packaged else {
            continue;
        };
        let commit = parse_version_output(&embedded_version_output(&executable)?, &executable)?;
        if source_commit != commit {
            return Err(ValidationError::SourceCommit);
        }
        let record = records.get(&spec.lane).ok_or(ValidationError::Record)?;
        if record.source_commit != commit {
            return Err(ValidationError::SourceCommit);
        }
        if record.executable.sha256 != sha256_hex(&executable) {
            return Err(ValidationError::ExecutableDigest);
        }
        validate_executable(&executable, spec.lane.executable_architecture())?;
        if spec.lane == host_lane && spec.kind == ArtifactKind::TarGz {
            validated_host_executable = Some(executable);
        }
    }
    let validated_host_executable =
        validated_host_executable.ok_or(ValidationError::CandidateSet)?;
    let executed_commit = parse_version_output(
        &version_output(&validated_host_executable)?,
        &validated_host_executable,
    )?;
    if executed_commit != source_commit {
        return Err(ValidationError::SourceCommit);
    }
    Ok(())
}

fn read_candidate_directory(
    root: &Path,
    expected_names: &[String],
) -> Result<BTreeMap<String, Vec<u8>>, ValidationError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|_| ValidationError::Io)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(ValidationError::CandidateType);
    }
    let expected = expected_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(root).map_err(|_| ValidationError::Io)? {
        let entry = entry.map_err(|_| ValidationError::Io)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ValidationError::CandidateSet)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|_| ValidationError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ValidationError::CandidateType);
        }
        if !expected.contains(name.as_str()) {
            return Err(ValidationError::CandidateSet);
        }
        let bytes = read_bounded(&entry.path())?;
        scan_canaries(&bytes)?;
        if files.insert(name, bytes).is_some() {
            return Err(ValidationError::CandidateSet);
        }
    }
    if files.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Err(ValidationError::CandidateSet);
    }
    Ok(files)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, ValidationError> {
    let file = fs::File::open(path).map_err(|_| ValidationError::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_CANDIDATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ValidationError::Io)?;
    if bytes.len() as u64 > MAX_CANDIDATE_BYTES {
        return Err(ValidationError::CandidateTooLarge);
    }
    Ok(bytes)
}

fn inspect_tar(
    bytes: &[u8],
    spec: &ArtifactSpec,
    epoch: Option<u64>,
) -> Result<ArchiveInspection, ValidationError> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut members = BTreeMap::new();
    let mut count = 0usize;
    for entry in archive
        .entries()
        .map_err(|_| ValidationError::ArchiveDecode)?
    {
        count += 1;
        if count > MAX_ARCHIVE_MEMBERS {
            return Err(ValidationError::ArchiveMembers);
        }
        let entry = entry.map_err(|_| ValidationError::ArchiveDecode)?;
        let path_bytes = entry.path_bytes();
        let path = std::str::from_utf8(&path_bytes)
            .map_err(|_| ValidationError::ArchivePath)?
            .to_owned();
        validate_member_path(&path)?;
        if !entry.header().entry_type().is_file() {
            return Err(ValidationError::ArchiveType);
        }
        let mode = entry
            .header()
            .mode()
            .map_err(|_| ValidationError::ArchiveDecode)?
            & 0o777;
        let uid = entry
            .header()
            .uid()
            .map_err(|_| ValidationError::ArchiveDecode)?;
        let gid = entry
            .header()
            .gid()
            .map_err(|_| ValidationError::ArchiveDecode)?;
        let mtime = entry
            .header()
            .mtime()
            .map_err(|_| ValidationError::ArchiveDecode)?;
        let mut member = Vec::new();
        entry
            .take(MAX_CANDIDATE_BYTES + 1)
            .read_to_end(&mut member)
            .map_err(|_| ValidationError::ArchiveDecode)?;
        if member.len() as u64 > MAX_CANDIDATE_BYTES {
            return Err(ValidationError::CandidateTooLarge);
        }
        scan_canaries(&member)?;
        if members
            .insert(path, (mode, uid, gid, mtime, member))
            .is_some()
        {
            return Err(ValidationError::ArchiveMembers);
        }
    }
    let modeled = modeled_members(spec, epoch.unwrap_or_default());
    let expected = modeled
        .iter()
        .map(|member| member.path)
        .collect::<BTreeSet<_>>();
    if members.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Err(ValidationError::ArchiveMembers);
    }
    for modeled in modeled {
        let (mode, uid, gid, mtime, _) = members
            .get(modeled.path)
            .ok_or(ValidationError::ArchiveMembers)?;
        if *mode != modeled.mode
            || *uid != modeled.uid
            || *gid != modeled.gid
            || epoch.is_some_and(|_| *mtime != modeled.mtime)
        {
            return Err(ValidationError::ArchiveMetadata);
        }
    }
    let executable = members
        .get(EXECUTABLE_NAME)
        .ok_or(ValidationError::ArchiveMembers)?
        .4
        .clone();
    let instructions = members
        .get("INSTALL.md")
        .ok_or(ValidationError::ArchiveMembers)?
        .4
        .as_slice();
    if instructions != install_instructions(spec.lane).as_bytes() {
        return Err(ValidationError::Dependency);
    }
    Ok(ArchiveInspection { binary: executable })
}

fn inspect_deb(bytes: &[u8], spec: &ArtifactSpec, epoch: u64) -> Result<Vec<u8>, ValidationError> {
    let mut archive = ar::Archive::new(Cursor::new(bytes));
    let mut members = BTreeMap::new();
    while let Some(entry) = archive.next_entry() {
        let entry = entry.map_err(|_| ValidationError::ArchiveDecode)?;
        let identifier = std::str::from_utf8(entry.header().identifier())
            .map_err(|_| ValidationError::ArchiveDecode)?
            .trim_end_matches('/')
            .to_owned();
        if !matches!(
            identifier.as_str(),
            "debian-binary" | "control.tar.gz" | "data.tar.gz"
        ) {
            return Err(ValidationError::ArchiveMembers);
        }
        if entry.header().uid() != 0
            || entry.header().gid() != 0
            || entry.header().mtime() != epoch
            || entry.header().mode() & 0o777 != 0o644
        {
            return Err(ValidationError::ArchiveMetadata);
        }
        let mut member = Vec::new();
        entry
            .take(MAX_CANDIDATE_BYTES + 1)
            .read_to_end(&mut member)
            .map_err(|_| ValidationError::ArchiveDecode)?;
        if member.len() as u64 > MAX_CANDIDATE_BYTES {
            return Err(ValidationError::CandidateTooLarge);
        }
        if members.insert(identifier, member).is_some() {
            return Err(ValidationError::ArchiveMembers);
        }
    }
    if members.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != BTreeSet::from(["control.tar.gz", "data.tar.gz", "debian-binary"])
        || members.get("debian-binary").map(Vec::as_slice) != Some(b"2.0\n")
    {
        return Err(ValidationError::ArchiveMembers);
    }
    let control_members = inspect_inner_tar(
        members
            .get("control.tar.gz")
            .ok_or(ValidationError::ArchiveMembers)?,
        epoch,
        &["control"],
    )?;
    let control = std::str::from_utf8(
        control_members
            .get("control")
            .ok_or(ValidationError::ArchiveMembers)?,
    )
    .map_err(|_| ValidationError::ArchiveDecode)?;
    validate_deb_control(control, spec)?;
    let binary_path = spec.binary_destination.trim_start_matches('/');
    let data_members = inspect_inner_tar(
        members
            .get("data.tar.gz")
            .ok_or(ValidationError::ArchiveMembers)?,
        epoch,
        &[binary_path],
    )?;
    let executable = data_members
        .get(binary_path)
        .ok_or(ValidationError::ArchiveMembers)?
        .clone();
    Ok(executable)
}

fn inspect_inner_tar(
    bytes: &[u8],
    epoch: u64,
    expected_regular: &[&str],
) -> Result<BTreeMap<String, Vec<u8>>, ValidationError> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut regular = BTreeMap::new();
    let allowed_regular = expected_regular.iter().copied().collect::<BTreeSet<_>>();
    let allowed_directories = expected_regular
        .iter()
        .flat_map(|path| ancestor_directories(path))
        .collect::<BTreeSet<_>>();
    let mut count = 0usize;
    for entry in archive
        .entries()
        .map_err(|_| ValidationError::ArchiveDecode)?
    {
        count += 1;
        if count > MAX_ARCHIVE_MEMBERS {
            return Err(ValidationError::ArchiveMembers);
        }
        let entry = entry.map_err(|_| ValidationError::ArchiveDecode)?;
        let raw = entry.path_bytes();
        let path = std::str::from_utf8(&raw)
            .map_err(|_| ValidationError::ArchivePath)?
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned();
        if path.is_empty() && entry.header().entry_type().is_dir() {
            continue;
        }
        validate_member_path(&path)?;
        let mode = entry
            .header()
            .mode()
            .map_err(|_| ValidationError::ArchiveDecode)?
            & 0o777;
        let uid = entry
            .header()
            .uid()
            .map_err(|_| ValidationError::ArchiveDecode)?;
        let gid = entry
            .header()
            .gid()
            .map_err(|_| ValidationError::ArchiveDecode)?;
        let mtime = entry
            .header()
            .mtime()
            .map_err(|_| ValidationError::ArchiveDecode)?;
        if uid != 0 || gid != 0 || mtime != epoch {
            return Err(ValidationError::ArchiveMetadata);
        }
        if entry.header().entry_type().is_dir() {
            if !allowed_directories.contains(path.as_str()) || mode != 0o755 {
                return Err(ValidationError::ArchiveMembers);
            }
            continue;
        }
        if !entry.header().entry_type().is_file() || !allowed_regular.contains(path.as_str()) {
            return Err(if entry.header().entry_type().is_file() {
                ValidationError::ArchiveMembers
            } else {
                ValidationError::ArchiveType
            });
        }
        let expected_mode = if path.ends_with(EXECUTABLE_NAME) {
            EXECUTABLE_MODE
        } else {
            0o644
        };
        if mode != expected_mode {
            return Err(ValidationError::ArchiveMetadata);
        }
        let mut member = Vec::new();
        entry
            .take(MAX_CANDIDATE_BYTES + 1)
            .read_to_end(&mut member)
            .map_err(|_| ValidationError::ArchiveDecode)?;
        scan_canaries(&member)?;
        if regular.insert(path, member).is_some() {
            return Err(ValidationError::ArchiveMembers);
        }
    }
    if regular.keys().map(String::as_str).collect::<BTreeSet<_>>() != allowed_regular {
        return Err(ValidationError::ArchiveMembers);
    }
    Ok(regular)
}

fn validate_deb_control(control: &str, spec: &ArtifactSpec) -> Result<(), ValidationError> {
    if !control.ends_with('\n') || control.contains('\r') || control.contains("\n ") {
        return Err(ValidationError::ArchiveDecode);
    }
    let mut fields = BTreeMap::new();
    for line in control.strip_suffix('\n').unwrap_or(control).split('\n') {
        let (name, value) = line
            .split_once(": ")
            .ok_or(ValidationError::ArchiveDecode)?;
        if fields.insert(name, value).is_some() {
            return Err(ValidationError::ArchiveDecode);
        }
    }
    let expected_fields = BTreeSet::from([
        "Architecture",
        "Depends",
        "Description",
        "Maintainer",
        "Package",
        "Version",
    ]);
    if fields.keys().copied().collect::<BTreeSet<_>>() != expected_fields
        || fields.get("Package") != Some(&PRODUCT)
        || fields.get("Version") != Some(&PRODUCT_VERSION)
    {
        return Err(ValidationError::ArchiveMembers);
    }
    if fields.get("Architecture") != Some(&spec.declared_architecture) {
        return Err(ValidationError::DeclaredArchitecture);
    }
    if fields.get("Depends") != Some(&DEB_DEPENDS) {
        return Err(ValidationError::Dependency);
    }
    Ok(())
}

fn validate_member_path(path: &str) -> Result<(), ValidationError> {
    let candidate = Path::new(path);
    if path.is_empty()
        || path.contains('\\')
        || candidate.is_absolute()
        || candidate
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(ValidationError::ArchivePath);
    }
    if candidate.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        FORBIDDEN_MEMBER_BASENAMES.contains(&name.as_ref())
            || name.ends_with(".key")
            || name.ends_with(".pem")
    }) {
        return Err(ValidationError::ForbiddenMember);
    }
    Ok(())
}

fn ancestor_directories(path: &str) -> Vec<&str> {
    let mut directories = Vec::new();
    for (index, byte) in path.bytes().enumerate() {
        if byte == b'/' {
            directories.push(&path[..index]);
        }
    }
    directories
}

fn validate_executable(
    bytes: &[u8],
    expected: ExecutableArchitecture,
) -> Result<(), ValidationError> {
    match expected {
        ExecutableArchitecture::ElfX86_64 => validate_elf(bytes, 62),
        ExecutableArchitecture::ElfAarch64 => validate_elf(bytes, 183),
        ExecutableArchitecture::MachOAarch64 => validate_macho(bytes),
    }
}

fn validate_elf(bytes: &[u8], expected_machine: u16) -> Result<(), ValidationError> {
    if bytes.len() < 64
        || &bytes[..4] != b"\x7fELF"
        || bytes[4] != 2
        || bytes[5] != 1
        || u16::from_le_bytes([bytes[18], bytes[19]]) != expected_machine
    {
        return Err(ValidationError::ExecutableArchitecture);
    }
    for version in glibc_versions(bytes) {
        if version > GLIBC_FLOOR {
            return Err(ValidationError::GlibcFloor);
        }
    }
    Ok(())
}

fn glibc_versions(bytes: &[u8]) -> Vec<(u32, u32)> {
    let marker = b"GLIBC_";
    let mut versions = Vec::new();
    let mut offset = 0usize;
    while offset + marker.len() < bytes.len() {
        let Some(found) = bytes[offset..]
            .windows(marker.len())
            .position(|window| window == marker)
        else {
            break;
        };
        let start = offset + found + marker.len();
        let tail = &bytes[start..];
        let major_length = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if major_length > 0 && tail.get(major_length) == Some(&b'.') {
            let minor_start = major_length + 1;
            let minor_length = tail[minor_start..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if minor_length > 0
                && let (Ok(major), Ok(minor)) = (
                    std::str::from_utf8(&tail[..major_length])
                        .unwrap_or_default()
                        .parse(),
                    std::str::from_utf8(&tail[minor_start..minor_start + minor_length])
                        .unwrap_or_default()
                        .parse(),
                )
            {
                versions.push((major, minor));
            }
        }
        offset = start;
    }
    versions
}

fn validate_macho(bytes: &[u8]) -> Result<(), ValidationError> {
    if bytes.len() < 32
        || bytes[..4] != [0xcf, 0xfa, 0xed, 0xfe]
        || u32::from_le_bytes(bytes[4..8].try_into().unwrap_or_default()) != 0x0100_000c
    {
        return Err(ValidationError::ExecutableArchitecture);
    }
    let command_count = u32::from_le_bytes(bytes[16..20].try_into().unwrap_or_default()) as usize;
    let mut offset = 32usize;
    let mut deployment = None;
    for _ in 0..command_count {
        if offset + 8 > bytes.len() {
            return Err(ValidationError::ExecutableArchitecture);
        }
        let command = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or_default());
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap_or_default())
            as usize;
        if size < 8 || offset + size > bytes.len() {
            return Err(ValidationError::ExecutableArchitecture);
        }
        if command == 0x32 && size >= 24 {
            let packed = u32::from_le_bytes(
                bytes[offset + 12..offset + 16]
                    .try_into()
                    .unwrap_or_default(),
            );
            deployment = Some((packed >> 16, (packed >> 8) & 0xff));
        } else if command == 0x24 && size >= 16 {
            let packed = u32::from_le_bytes(
                bytes[offset + 8..offset + 12]
                    .try_into()
                    .unwrap_or_default(),
            );
            deployment = Some((packed >> 16, (packed >> 8) & 0xff));
        }
        offset += size;
    }
    if deployment != Some(MACOS_DEPLOYMENT_FLOOR) {
        return Err(ValidationError::MacosFloor);
    }
    Ok(())
}

fn parse_version_output(output: &[u8], executable: &[u8]) -> Result<String, ValidationError> {
    let output = std::str::from_utf8(output).map_err(|_| ValidationError::VersionOutput)?;
    let prefix = format!("{PRODUCT} {PRODUCT_VERSION} (source ");
    let commit = output
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(")\n"))
        .ok_or(ValidationError::VersionOutput)?;
    if commit.len() != 40
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || !executable
            .windows(commit.len())
            .any(|window| window == commit.as_bytes())
    {
        return Err(ValidationError::SourceCommit);
    }
    Ok(commit.to_owned())
}

fn embedded_version_output(executable: &[u8]) -> Result<Vec<u8>, ValidationError> {
    let prefix = format!("{PRODUCT} {PRODUCT_VERSION} (source ");
    let prefix = prefix.as_bytes();
    let start = executable
        .windows(prefix.len())
        .position(|window| window == prefix)
        .ok_or(ValidationError::VersionOutput)?;
    let length = prefix.len() + 40 + 2;
    let end = start
        .checked_add(length)
        .ok_or(ValidationError::VersionOutput)?;
    let line = executable
        .get(start..end)
        .ok_or(ValidationError::VersionOutput)?;
    if !line.ends_with(b")\n") {
        return Err(ValidationError::VersionOutput);
    }
    Ok(line.to_vec())
}

fn run_version_probe(executable: &[u8]) -> Result<Vec<u8>, ValidationError> {
    let directory = std::env::temp_dir().join(format!(
        "solstone-tmux-version-probe-{}-{}",
        std::process::id(),
        PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .map_err(|_| ValidationError::Io)?;
    let path = directory.join(EXECUTABLE_NAME);
    let result = (|| {
        if fs::symlink_metadata(&path).is_ok() {
            return Err(ValidationError::CandidateType);
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| ValidationError::Io)?;
        file.write_all(executable)
            .map_err(|_| ValidationError::Io)?;
        file.sync_all().map_err(|_| ValidationError::Io)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .map_err(|_| ValidationError::Io)?;
        let output = Command::new(&path)
            .arg("--version")
            .output()
            .map_err(|_| ValidationError::VersionOutput)?;
        if !output.status.success() || !output.stderr.is_empty() {
            return Err(ValidationError::VersionOutput);
        }
        Ok(output.stdout)
    })();
    let _ = fs::remove_dir_all(directory);
    result
}

fn git_source_epoch(source_commit: &str) -> Result<u64, ValidationError> {
    let output = Command::new("git")
        .args(["show", "-s", "--format=%ct", source_commit])
        .output()
        .map_err(|_| ValidationError::Io)?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(ValidationError::SourceCommit);
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| ValidationError::SourceCommit)?;
    text.strip_suffix('\n')
        .ok_or(ValidationError::SourceCommit)?
        .parse()
        .map_err(|_| ValidationError::SourceCommit)
}

fn decode_record(bytes: &[u8]) -> Result<TargetRecord, ValidationError> {
    serde_json::from_slice(bytes).map_err(|_| ValidationError::RecordSchema)
}

fn scan_canaries(bytes: &[u8]) -> Result<(), ValidationError> {
    if SECRET_CANARIES.iter().any(|canary| {
        bytes
            .windows(canary.len())
            .any(|window| window == canary.as_bytes())
    }) {
        return Err(ValidationError::SecretCanary);
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn has_exact_lines(text: &str, lines: usize) -> bool {
    text.ends_with('\n') && !text.contains('\r') && text.lines().count() == lines
}

fn map_checksum_error(error: ModelError) -> ValidationError {
    match error {
        ModelError::ChecksumSet => ValidationError::ChecksumSet,
        _ => ValidationError::ChecksumSyntax,
    }
}

struct ArchiveInspection {
    binary: Vec<u8>,
}
