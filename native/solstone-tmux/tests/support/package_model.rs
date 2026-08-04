// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

pub const PRODUCT: &str = "solstone-tmux";
pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const EXECUTABLE_NAME: &str = "solstone-tmux";
pub const EXECUTABLE_MODE: u32 = 0o755;
pub const DOCUMENT_MODE: u32 = 0o644;
pub const ROOT_UID: u64 = 0;
pub const ROOT_GID: u64 = 0;
pub const MACOS_DEPLOYMENT_FLOOR: (u32, u32) = (14, 0);
pub const DEB_DEPENDS: &str = "tmux";
pub const RPM_REQUIRES: &str = "tmux";
pub const SHA256SUMS_NAME: &str = "SHA256SUMS";
pub const SIGNATURE_NAME: &str = "SHA256SUMS.minisig";
pub const LINUX_TAR_DESTINATION: &str = "/usr/local/bin/solstone-tmux";
pub const DEB_DESTINATION: &str = "/usr/bin/solstone-tmux";
pub const RPM_DESTINATION: &str = "/usr/bin/solstone-tmux";
pub const MACOS_DESTINATION: &str = "/usr/local/bin/solstone-tmux";

pub const SECRET_CANARIES: [&str; 3] = [
    "LEGACYKEYCANARY-do-not-copy",
    "SOLSTONE_TMUX_SECRET_CANARY_DO_NOT_SHIP",
    "MINISIGN_SECRET_CANARY_DO_NOT_SHIP",
];

pub const FORBIDDEN_MEMBER_BASENAMES: [&str; 8] = [
    "config.json",
    "credentials.json",
    "observer.json",
    "sync-health.json",
    "health.json",
    "captures",
    "minisign.key",
    "secret.key",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Lane {
    LinuxX86_64,
    LinuxAarch64,
    MacosAarch64,
}

impl Lane {
    pub const ALL: [Self; 3] = [Self::LinuxX86_64, Self::LinuxAarch64, Self::MacosAarch64];

    pub const fn rust_target(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "x86_64-unknown-linux-gnu",
            Self::LinuxAarch64 => "aarch64-unknown-linux-gnu",
            Self::MacosAarch64 => "aarch64-apple-darwin",
        }
    }

    pub fn record_name(self) -> String {
        format!(
            "{PRODUCT}-{PRODUCT_VERSION}-{}.target.json",
            self.rust_target()
        )
    }

    pub fn from_target(target: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|lane| lane.rust_target() == target)
    }

    pub const fn executable_architecture(self) -> ExecutableArchitecture {
        match self {
            Self::LinuxX86_64 => ExecutableArchitecture::ElfX86_64,
            Self::LinuxAarch64 => ExecutableArchitecture::ElfAarch64,
            Self::MacosAarch64 => ExecutableArchitecture::MachOAarch64,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutableArchitecture {
    ElfX86_64,
    ElfAarch64,
    MachOAarch64,
}

impl ExecutableArchitecture {
    pub const fn elf_type(self) -> Option<u16> {
        match self {
            // Both Linux release binaries are ET_EXEC. Verified against the real artifacts for
            // 1.0.1 on both architectures.
            //
            // Reading the target spec alone predicts ET_DYN for x86_64, because
            // x86_64-unknown-linux-musl sets static_position_independent_executables. That
            // prediction is wrong for what we ship: the release lanes link through
            // cargo-zigbuild, and zig's driver does not honour rustc's static-pie request, so the
            // emitted binary is ET_EXEC. aarch64-musl is ET_EXEC from its own spec.
            //
            // Pin what we actually ship, so a toolchain change that alters the ELF type trips this
            // gate loudly instead of shipping quietly.
            Self::ElfX86_64 => Some(2),
            Self::ElfAarch64 => Some(2),
            Self::MachOAarch64 => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ArtifactKind {
    TarGz,
    Deb,
    Rpm,
    Pkg,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactSpec {
    pub lane: Lane,
    pub kind: ArtifactKind,
    pub name: String,
    pub declared_architecture: &'static str,
    pub binary_destination: &'static str,
    pub tmux_prerequisite: &'static str,
}

pub static ARTIFACTS: LazyLock<Vec<ArtifactSpec>> = LazyLock::new(|| {
    vec![
        ArtifactSpec {
            lane: Lane::LinuxX86_64,
            kind: ArtifactKind::TarGz,
            name: format!("{PRODUCT}-{PRODUCT_VERSION}-x86_64-linux.tar.gz"),
            declared_architecture: "x86_64",
            binary_destination: LINUX_TAR_DESTINATION,
            tmux_prerequisite: "tmux",
        },
        ArtifactSpec {
            lane: Lane::LinuxX86_64,
            kind: ArtifactKind::Deb,
            name: format!("{PRODUCT}_{PRODUCT_VERSION}_amd64.deb"),
            declared_architecture: "amd64",
            binary_destination: DEB_DESTINATION,
            tmux_prerequisite: DEB_DEPENDS,
        },
        ArtifactSpec {
            lane: Lane::LinuxX86_64,
            kind: ArtifactKind::Rpm,
            name: format!("{PRODUCT}-{PRODUCT_VERSION}-1.x86_64.rpm"),
            declared_architecture: "x86_64",
            binary_destination: RPM_DESTINATION,
            tmux_prerequisite: RPM_REQUIRES,
        },
        ArtifactSpec {
            lane: Lane::LinuxAarch64,
            kind: ArtifactKind::TarGz,
            name: format!("{PRODUCT}-{PRODUCT_VERSION}-aarch64-linux.tar.gz"),
            declared_architecture: "aarch64",
            binary_destination: LINUX_TAR_DESTINATION,
            tmux_prerequisite: "tmux",
        },
        ArtifactSpec {
            lane: Lane::LinuxAarch64,
            kind: ArtifactKind::Deb,
            name: format!("{PRODUCT}_{PRODUCT_VERSION}_arm64.deb"),
            declared_architecture: "arm64",
            binary_destination: DEB_DESTINATION,
            tmux_prerequisite: DEB_DEPENDS,
        },
        ArtifactSpec {
            lane: Lane::LinuxAarch64,
            kind: ArtifactKind::Rpm,
            name: format!("{PRODUCT}-{PRODUCT_VERSION}-1.aarch64.rpm"),
            declared_architecture: "aarch64",
            binary_destination: RPM_DESTINATION,
            tmux_prerequisite: RPM_REQUIRES,
        },
        ArtifactSpec {
            lane: Lane::MacosAarch64,
            kind: ArtifactKind::TarGz,
            name: format!("{PRODUCT}-{PRODUCT_VERSION}-aarch64-macos.tar.gz"),
            declared_architecture: "aarch64",
            binary_destination: MACOS_DESTINATION,
            tmux_prerequisite: "tmux",
        },
        ArtifactSpec {
            lane: Lane::MacosAarch64,
            kind: ArtifactKind::Pkg,
            name: format!("{PRODUCT}-{PRODUCT_VERSION}-aarch64-macos.pkg"),
            declared_architecture: "arm64",
            binary_destination: MACOS_DESTINATION,
            tmux_prerequisite: "tmux",
        },
    ]
});

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeledMemberKind {
    Regular,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeledMember {
    pub path: &'static str,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub kind: ModeledMemberKind,
}

pub fn modeled_members(spec: &ArtifactSpec, source_date_epoch: u64) -> Vec<ModeledMember> {
    let binary_path = spec.binary_destination.trim_start_matches('/');
    match spec.kind {
        ArtifactKind::TarGz => vec![
            ModeledMember {
                path: EXECUTABLE_NAME,
                mode: EXECUTABLE_MODE,
                uid: ROOT_UID,
                gid: ROOT_GID,
                mtime: source_date_epoch,
                kind: ModeledMemberKind::Regular,
            },
            ModeledMember {
                path: "INSTALL.md",
                mode: DOCUMENT_MODE,
                uid: ROOT_UID,
                gid: ROOT_GID,
                mtime: source_date_epoch,
                kind: ModeledMemberKind::Regular,
            },
        ],
        ArtifactKind::Deb | ArtifactKind::Rpm | ArtifactKind::Pkg => vec![ModeledMember {
            path: binary_path,
            mode: EXECUTABLE_MODE,
            uid: ROOT_UID,
            gid: ROOT_GID,
            mtime: source_date_epoch,
            kind: ModeledMemberKind::Regular,
        }],
    }
}

pub fn install_instructions(lane: Lane) -> String {
    let destination = match lane {
        Lane::LinuxX86_64 | Lane::LinuxAarch64 => LINUX_TAR_DESTINATION,
        Lane::MacosAarch64 => MACOS_DESTINATION,
    };
    format!(
        "solstone-tmux requires tmux.\nInstall with: install -m 0755 solstone-tmux {destination}\n"
    )
}

pub fn artifacts_for_lane(lane: Lane) -> Vec<ArtifactSpec> {
    ARTIFACTS
        .iter()
        .filter(|artifact| artifact.lane == lane)
        .cloned()
        .collect()
}

pub fn checksummed_names() -> Vec<String> {
    let mut names = ARTIFACTS
        .iter()
        .map(|artifact| artifact.name.clone())
        .chain(Lane::ALL.into_iter().map(Lane::record_name))
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

pub fn complete_candidate_names() -> Vec<String> {
    let mut names = checksummed_names();
    names.extend([SHA256SUMS_NAME.to_owned(), SIGNATURE_NAME.to_owned()]);
    names.sort_unstable();
    names
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetRecord {
    pub schema_version: u32,
    pub product_version: String,
    pub source_commit: String,
    pub rust_target: String,
    pub rustc_vv: String,
    pub executable: ExecutableRecord,
    pub artifacts: Vec<ArtifactRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableRecord {
    pub name: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub name: String,
    pub sha256: String,
}

impl TargetRecord {
    pub fn validate(&self, lane: Lane, expected_artifacts: &[&str]) -> Result<(), ModelError> {
        if self.schema_version != 1 {
            return Err(ModelError::SchemaVersion);
        }
        if self.product_version != PRODUCT_VERSION {
            return Err(ModelError::ProductVersion);
        }
        if !is_lower_hex(&self.source_commit, 40) {
            return Err(ModelError::SourceCommit);
        }
        if self.rust_target != lane.rust_target() {
            return Err(ModelError::RustTarget);
        }
        if self.rustc_vv.is_empty()
            || !self.rustc_vv.ends_with('\n')
            || self.rustc_vv.contains('\r')
        {
            return Err(ModelError::RustcVersion);
        }
        if self.executable.name != EXECUTABLE_NAME {
            return Err(ModelError::ExecutableName);
        }
        if !is_lower_hex(&self.executable.sha256, 64) {
            return Err(ModelError::Digest);
        }
        let names = self
            .artifacts
            .iter()
            .map(|artifact| artifact.name.as_str())
            .collect::<Vec<_>>();
        if names.windows(2).any(|pair| pair[0] >= pair[1]) {
            if names.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(ModelError::ArtifactDuplicate);
            }
            return Err(ModelError::ArtifactOrder);
        }
        if self
            .artifacts
            .iter()
            .any(|artifact| !is_lower_hex(&artifact.sha256, 64))
        {
            return Err(ModelError::Digest);
        }
        let expected = expected_artifacts.iter().copied().collect::<BTreeSet<_>>();
        if names.iter().copied().collect::<BTreeSet<_>>() != expected {
            return Err(ModelError::ArtifactSet);
        }
        Ok(())
    }
}

pub fn render_sha256sums(digests: &BTreeMap<String, String>) -> Result<Vec<u8>, ModelError> {
    if digests.is_empty() {
        return Err(ModelError::ChecksumSet);
    }
    let mut output = Vec::new();
    for (name, digest) in digests {
        validate_checksum_name(name)?;
        if !is_lower_hex(digest, 64) {
            return Err(ModelError::Digest);
        }
        output.extend_from_slice(digest.as_bytes());
        output.extend_from_slice(b"  ");
        output.extend_from_slice(name.as_bytes());
        output.push(b'\n');
    }
    Ok(output)
}

pub fn parse_sha256sums(bytes: &[u8]) -> Result<BTreeMap<String, String>, ModelError> {
    if bytes.is_empty() || !bytes.ends_with(b"\n") || bytes.contains(&b'\r') {
        return Err(ModelError::ChecksumSyntax);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ModelError::ChecksumSyntax)?;
    let mut parsed = BTreeMap::new();
    let mut previous: Option<&str> = None;
    for line in text.strip_suffix('\n').unwrap_or(text).split('\n') {
        if line.len() < 68 || &line[64..66] != "  " {
            return Err(ModelError::ChecksumSyntax);
        }
        let digest = &line[..64];
        let name = &line[66..];
        if !is_lower_hex(digest, 64) {
            return Err(ModelError::ChecksumSyntax);
        }
        validate_checksum_name(name)?;
        if previous.is_some_and(|prior| prior >= name) {
            if previous == Some(name) {
                return Err(ModelError::ChecksumDuplicate);
            }
            return Err(ModelError::ChecksumOrder);
        }
        if parsed.insert(name.to_owned(), digest.to_owned()).is_some() {
            return Err(ModelError::ChecksumDuplicate);
        }
        previous = Some(name);
    }
    Ok(parsed)
}

fn validate_checksum_name(name: &str) -> Result<(), ModelError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('*')
        || name.starts_with('#')
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\n')
    {
        return Err(ModelError::ChecksumSyntax);
    }
    Ok(())
}

pub fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    SchemaVersion,
    ProductVersion,
    SourceCommit,
    RustTarget,
    RustcVersion,
    ExecutableName,
    Digest,
    ArtifactOrder,
    ArtifactDuplicate,
    ArtifactSet,
    ChecksumSyntax,
    ChecksumOrder,
    ChecksumDuplicate,
    ChecksumSet,
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SchemaVersion => "target record schema version is invalid",
            Self::ProductVersion => "target record product version is invalid",
            Self::SourceCommit => "target record source commit is not canonical",
            Self::RustTarget => "target record Rust target is invalid",
            Self::RustcVersion => "target record rustc output is not LF-normalized",
            Self::ExecutableName => "target record executable name is invalid",
            Self::Digest => "digest is not canonical lowercase SHA-256",
            Self::ArtifactOrder => "target record artifacts are not bytewise sorted",
            Self::ArtifactDuplicate => "target record contains a duplicate artifact",
            Self::ArtifactSet => "target record artifact set is incomplete or unlisted",
            Self::ChecksumSyntax => "SHA256SUMS syntax is invalid",
            Self::ChecksumOrder => "SHA256SUMS entries are not bytewise sorted",
            Self::ChecksumDuplicate => "SHA256SUMS contains a duplicate filename",
            Self::ChecksumSet => "SHA256SUMS file set is incomplete or unlisted",
        })
    }
}

impl std::error::Error for ModelError {}
