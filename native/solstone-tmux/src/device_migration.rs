// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::command::{CommandInvocation, CommandOperation, CommandRunner};
use crate::health::DiagnosticCode;
use crate::journal::JournalClient;
use crate::journal_version::VersionRefreshState;
use crate::journal_version::hex_encode;
use crate::pairing_answer::{ANSWER_FILENAME, AnswerRecord, acquire_answer_lock, read_answer_file};
use crate::paths::PlatformKind;
use crate::private_link::{PrivateLinkBridge, load_credential, persist_credential};
use crate::storage::{StorageError, atomic_write_bytes, open_regular_readonly};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use reqwest::{Method, StatusCode};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use spl_core::PairResponse;
use spl_transport::credential::{Credential, EndpointAddr};

pub const RECORD_FILENAME: &str = "device-migration.json";
const RECORD_VERSION: u32 = 1;
const MARKER_DOMAIN: &[u8] = b"solstone-tmux/device-migration/host-marker/v1\0";
const IOREG_TIMEOUT: Duration = Duration::from_secs(5);
const MIGRATION_PATH: &str = "/app/network/api/clients/self/migration";
const REKEY_PATH: &str = "/app/network/api/clients/self/rekey";
const CONTROL_BODY_LIMIT: usize = 64 * 1024;

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RekeyRequest<'a> {
    protocol_version: u32,
    operation_id: &'a str,
    csr: &'a str,
    device_label: &'a str,
    client_label: &'a str,
    platform: &'a str,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct DecisionRequest<'a> {
    protocol_version: u32,
    operation_id: &'a str,
    choice: &'static str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedRekeyRequest {
    protocol_version: u32,
    operation_id: String,
    csr: String,
    device_label: String,
    client_label: String,
    platform: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedDecisionRequest {
    protocol_version: u32,
    operation_id: String,
    choice: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RekeyResponse {
    protocol_version: u32,
    operation_id: String,
    state: String,
    previous_cid: String,
    cid: String,
    pairing: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationStateResponse {
    protocol_version: u32,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    rekey_operation_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    previous_cid: Option<String>,
    state: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    replaced_cid: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionResponse {
    protocol_version: u32,
    operation_id: String,
    state: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    previous_cid: Option<String>,
    cid: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    replaced_cid: Option<String>,
    display_label: String,
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationRecord {
    version: u32,
    pub phase: MigrationPhase,
    pub adopted_marker_digest: Option<String>,
    pub pending_marker_digest: Option<String>,
    pub source_cid: Option<String>,
    pub source_generation: Option<String>,
    pub source_credential: Option<Credential>,
    pub candidate_credential: Option<Credential>,
    pub rekey_operation_id: Option<String>,
    pub candidate_key_pem: Option<String>,
    pub csr_pem: Option<String>,
    pub rekey_request: Option<Vec<u8>>,
    pub rekey_reply: Option<Vec<u8>>,
    pub decision_id: Option<String>,
    pub decision_request: Option<Vec<u8>>,
    pub decision_reply: Option<Vec<u8>>,
    pub decision_state_reply: Option<Vec<u8>>,
    pub source_answer_bytes: Option<Vec<u8>>,
    pub target_answer_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    Unadopted,
    RekeyPending,
    RekeyVerified,
    DecisionPending,
    DecisionVerified,
    Publishing,
    Adopted,
}

impl Default for MigrationRecord {
    fn default() -> Self {
        Self {
            version: RECORD_VERSION,
            phase: MigrationPhase::Unadopted,
            adopted_marker_digest: None,
            pending_marker_digest: None,
            source_cid: None,
            source_generation: None,
            source_credential: None,
            candidate_credential: None,
            rekey_operation_id: None,
            candidate_key_pem: None,
            csr_pem: None,
            rekey_request: None,
            rekey_reply: None,
            decision_id: None,
            decision_request: None,
            decision_reply: None,
            decision_state_reply: None,
            source_answer_bytes: None,
            target_answer_bytes: None,
        }
    }
}

impl MigrationRecord {
    pub fn load(config_root: &Path) -> Result<Option<Self>, DiagnosticCode> {
        let path = config_root.join(RECORD_FILENAME);
        let mut file = match open_regular_readonly(&path) {
            Ok(file) => file,
            Err(StorageError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(StorageError::InvalidTarget(_)) => {
                return Err(DiagnosticCode::PrivateStateInvalid);
            }
            Err(StorageError::Io { source, .. })
                if source.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                return Err(DiagnosticCode::PrivateStateInvalid);
            }
            Err(_) => return Err(DiagnosticCode::PrivateStateIo),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| DiagnosticCode::PrivateStateIo)?;
        let record: Self =
            serde_json::from_slice(&bytes).map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
        if !record.has_valid_state() {
            return Err(DiagnosticCode::PrivateStateInvalid);
        }
        Ok(Some(record))
    }

    pub fn persist(&self, config_root: &Path) -> Result<(), DiagnosticCode> {
        if !self.has_valid_state() {
            return Err(DiagnosticCode::PrivateStateInvalid);
        }
        let bytes = serde_json::to_vec(self).map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
        match atomic_write_bytes(&config_root.join(RECORD_FILENAME), config_root, &bytes) {
            Ok(()) => Ok(()),
            Err(StorageError::InvalidTarget(_)) => Err(DiagnosticCode::PrivateStateInvalid),
            Err(_) => Err(DiagnosticCode::PrivateStateIo),
        }
    }

    pub fn is_transaction_pending(&self) -> bool {
        !matches!(
            self.phase,
            MigrationPhase::Unadopted | MigrationPhase::Adopted
        )
    }

    fn clear_transaction_material(&mut self) {
        self.pending_marker_digest = None;
        self.source_cid = None;
        self.source_generation = None;
        self.source_credential = None;
        self.candidate_credential = None;
        self.rekey_operation_id = None;
        self.candidate_key_pem = None;
        self.csr_pem = None;
        self.rekey_request = None;
        self.rekey_reply = None;
        self.decision_id = None;
        self.decision_request = None;
        self.decision_reply = None;
        self.decision_state_reply = None;
        self.source_answer_bytes = None;
        self.target_answer_bytes = None;
    }

    fn finish_adoption(&mut self, marker_digest: &str) {
        self.phase = MigrationPhase::Adopted;
        self.adopted_marker_digest = Some(marker_digest.to_owned());
        self.clear_transaction_material();
    }

    fn has_transaction_material(&self) -> bool {
        self.source_cid.is_some()
            || self.source_generation.is_some()
            || self.source_credential.is_some()
            || self.candidate_credential.is_some()
            || self.rekey_operation_id.is_some()
            || self.candidate_key_pem.is_some()
            || self.csr_pem.is_some()
            || self.rekey_request.is_some()
            || self.rekey_reply.is_some()
            || self.decision_id.is_some()
            || self.decision_request.is_some()
            || self.decision_reply.is_some()
            || self.decision_state_reply.is_some()
            || self.source_answer_bytes.is_some()
            || self.target_answer_bytes.is_some()
    }

    fn has_valid_state(&self) -> bool {
        if self.version != RECORD_VERSION
            || self
                .adopted_marker_digest
                .as_deref()
                .is_some_and(|value| !is_sha256_hex(value))
            || self
                .pending_marker_digest
                .as_deref()
                .is_some_and(|value| !is_sha256_hex(value))
            || self
                .source_generation
                .as_deref()
                .is_some_and(|value| !is_sha256_hex(value))
        {
            return false;
        }
        let no_rekey_transaction = self.source_cid.is_none()
            && self.source_generation.is_none()
            && self.source_credential.is_none()
            && self.candidate_credential.is_none()
            && self.rekey_operation_id.is_none()
            && self.candidate_key_pem.is_none()
            && self.csr_pem.is_none()
            && self.rekey_request.is_none()
            && self.rekey_reply.is_none()
            && self.decision_id.is_none()
            && self.decision_request.is_none()
            && self.decision_reply.is_none()
            && self.decision_state_reply.is_none()
            && self.source_answer_bytes.is_none()
            && self.target_answer_bytes.is_none();
        let no_decision = self.decision_id.is_none()
            && self.decision_request.is_none()
            && self.decision_reply.is_none()
            && self.decision_state_reply.is_none();
        let no_answer_transfer =
            self.source_answer_bytes.is_none() && self.target_answer_bytes.is_none();
        match self.phase {
            MigrationPhase::Unadopted => {
                self.adopted_marker_digest.is_none()
                    && self.pending_marker_digest.is_none()
                    && no_rekey_transaction
            }
            MigrationPhase::Adopted => {
                self.adopted_marker_digest.is_some() && self.pending_marker_digest.is_none()
            }
            MigrationPhase::RekeyPending
            | MigrationPhase::RekeyVerified
            | MigrationPhase::DecisionPending
            | MigrationPhase::DecisionVerified
            | MigrationPhase::Publishing => {
                let base = self.pending_marker_digest.is_some()
                    && self.source_cid.is_some()
                    && self.source_cid.as_deref().is_some_and(is_cid)
                    && self.source_generation.is_some()
                    && self.source_credential.is_some()
                    && self.rekey_operation_id.is_some()
                    && self.candidate_key_pem.is_some()
                    && self.csr_pem.is_some()
                    && self.rekey_request.is_some();
                if !base {
                    return false;
                }
                match self.phase {
                    MigrationPhase::RekeyPending => {
                        self.candidate_credential.is_none()
                            && self.rekey_reply.is_none()
                            && no_decision
                            && no_answer_transfer
                    }
                    MigrationPhase::RekeyVerified => {
                        self.candidate_credential.is_some()
                            && self.rekey_reply.is_some()
                            && no_decision
                            && no_answer_transfer
                    }
                    MigrationPhase::DecisionPending => {
                        self.candidate_credential.is_some()
                            && self.rekey_reply.is_some()
                            && self.decision_id.is_some()
                            && self.decision_request.is_some()
                            && self.decision_reply.is_none()
                            && self.decision_state_reply.is_none()
                            && no_answer_transfer
                    }
                    MigrationPhase::DecisionVerified => {
                        self.candidate_credential.is_some()
                            && self.rekey_reply.is_some()
                            && self.decision_id.is_some()
                            && self.decision_request.is_some()
                            && (self.decision_reply.is_some()
                                || self.decision_state_reply.is_some())
                            && no_answer_transfer
                    }
                    MigrationPhase::Publishing => {
                        self.candidate_credential.is_some()
                            && self.rekey_reply.is_some()
                            && self.decision_id.is_some()
                            && self.decision_request.is_some()
                            && (self.decision_reply.is_some()
                                || self.decision_state_reply.is_some())
                            && (self.source_answer_bytes.is_some()
                                == self.target_answer_bytes.is_some())
                    }
                    MigrationPhase::Unadopted | MigrationPhase::Adopted => unreachable!(),
                }
            }
        }
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_cid(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_sha256_hex)
}

pub fn grandfather_or_settle_if_idle(config_root: &Path) -> Result<(), DiagnosticCode> {
    if MigrationRecord::load(config_root)?.is_some_and(|record| record.is_transaction_pending()) {
        return Ok(());
    }
    crate::pairing_answer::grandfather_or_settle(config_root)
}

pub fn record_setup_baseline(
    config_root: &Path,
    marker_digest: Option<&str>,
) -> Result<(), DiagnosticCode> {
    if marker_digest.is_some_and(|value| !is_sha256_hex(value)) {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    let mut record = MigrationRecord::load(config_root)?.unwrap_or_default();
    if let Some(marker_digest) = marker_digest {
        record.finish_adoption(marker_digest);
    } else {
        record.phase = MigrationPhase::Unadopted;
        record.adopted_marker_digest = None;
        record.clear_transaction_material();
    }
    record.persist(config_root)
}

pub async fn recover_publication(config_root: &Path) -> Result<bool, DiagnosticCode> {
    let root = config_root.to_owned();
    let Some(mut record) = blocking(move || MigrationRecord::load(&root)).await? else {
        return Ok(false);
    };
    if record.phase != MigrationPhase::Publishing {
        return Ok(false);
    }
    let source = record
        .source_credential
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let validation_record = record.clone();
    let candidate = blocking(move || verify_publishing_record(&validation_record)).await?;
    let lock = acquire_answer_lock(config_root).await?;
    let root = config_root.to_owned();
    let current = blocking(move || load_credential(&root)).await?;
    if current.as_ref() != Some(&source) && current.as_ref() != Some(&candidate) {
        drop(lock);
        return Err(DiagnosticCode::PrivateStateIo);
    }
    if let Some(target) = record.target_answer_bytes.as_deref() {
        let root = config_root.to_owned();
        let current_bytes = blocking(move || read_answer_bytes(&root)).await?;
        if current_bytes.as_deref() == Some(target) {
            // The answer publication reached disk before the interrupted checkpoint.
        } else if current_bytes.as_deref() == record.source_answer_bytes.as_deref() {
            let root = config_root.to_owned();
            let bytes = target.to_vec();
            blocking(move || write_answer_bytes(&root, &bytes)).await?;
        } else {
            drop(lock);
            return Err(DiagnosticCode::PrivateStateIo);
        }
    }
    if current.as_ref() == Some(&source) {
        let root = config_root.to_owned();
        blocking(move || persist_credential(&root, &candidate)).await?;
    }
    let marker_digest = record
        .pending_marker_digest
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    record.finish_adoption(&marker_digest);
    let root = config_root.to_owned();
    blocking(move || record.persist(&root)).await?;
    drop(lock);
    Ok(true)
}

async fn supersede_stale_setup(
    config_root: &Path,
    record: &MigrationRecord,
    credential: &Credential,
    marker_digest: &str,
) -> Result<bool, DiagnosticCode> {
    if !record.is_transaction_pending()
        || !record
            .source_credential
            .as_ref()
            .is_some_and(|source| source != credential)
        || record
            .candidate_credential
            .as_ref()
            .is_some_and(|candidate| candidate == credential)
    {
        return Ok(false);
    }
    let answer_lock = acquire_answer_lock(config_root).await?;
    let root = config_root.to_owned();
    let current = blocking(move || load_credential(&root)).await?;
    if current.as_ref() != Some(credential) {
        drop(answer_lock);
        return Err(DiagnosticCode::PrivateStateIo);
    }
    let root = config_root.to_owned();
    let marker = marker_digest.to_owned();
    blocking(move || {
        crate::journal_version::clear_cached_version(&root);
        record_setup_baseline(&root, Some(&marker))
    })
    .await?;
    drop(answer_lock);
    Ok(true)
}

fn read_answer_bytes(config_root: &Path) -> Result<Option<Vec<u8>>, DiagnosticCode> {
    let path = config_root.join(ANSWER_FILENAME);
    let mut file = match open_regular_readonly(&path) {
        Ok(file) => file,
        Err(StorageError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(StorageError::InvalidTarget(_)) => return Err(DiagnosticCode::PrivateStateInvalid),
        Err(_) => return Err(DiagnosticCode::PrivateStateIo),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    Ok(Some(bytes))
}

fn write_answer_bytes(config_root: &Path, bytes: &[u8]) -> Result<(), DiagnosticCode> {
    match atomic_write_bytes(&config_root.join(ANSWER_FILENAME), config_root, bytes) {
        Ok(()) => Ok(()),
        Err(StorageError::InvalidTarget(_)) => Err(DiagnosticCode::PrivateStateInvalid),
        Err(_) => Err(DiagnosticCode::PrivateStateIo),
    }
}

async fn blocking<T, F>(work: F) -> Result<T, DiagnosticCode>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, DiagnosticCode> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| DiagnosticCode::PrivateStateIo)?
}

async fn save_record(config_root: &Path, record: &MigrationRecord) -> Result<(), DiagnosticCode> {
    let root = config_root.to_owned();
    let record = record.clone();
    blocking(move || record.persist(&root)).await
}

async fn current_credential(config_root: &Path) -> Result<Credential, DiagnosticCode> {
    let root = config_root.to_owned();
    blocking(move || load_credential(&root))
        .await?
        .ok_or(DiagnosticCode::PrivateStateIo)
}

#[allow(clippy::too_many_arguments)]
pub async fn migrate_if_needed(
    config_root: &Path,
    data_root: &Path,
    platform: PlatformKind,
    hostname: &str,
    marker_digest: &str,
    mut credential: Credential,
    identity: crate::instance_lock::RunIdentity,
    now_unix_seconds: i64,
) -> Result<Credential, DiagnosticCode> {
    if !is_sha256_hex(marker_digest) {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    let root = config_root.to_owned();
    let mut record = blocking(move || MigrationRecord::load(&root))
        .await?
        .unwrap_or_default();
    if record.phase == MigrationPhase::Unadopted
        && record.source_credential.is_none()
        && record.adopted_marker_digest.is_none()
        && record.pending_marker_digest.is_none()
    {
        let answer_lock = acquire_answer_lock(config_root).await?;
        let legacy_root = config_root.to_owned();
        blocking(move || grandfather_or_settle_if_idle(&legacy_root)).await?;
        drop(answer_lock);
    }
    if supersede_stale_setup(config_root, &record, &credential, marker_digest).await? {
        return Ok(credential);
    }
    if record.phase == MigrationPhase::Publishing
        && record.pending_marker_digest.as_deref() == Some(marker_digest)
    {
        recover_publication(config_root).await?;
        return current_credential(config_root).await;
    }
    if record.phase == MigrationPhase::Adopted && record.has_transaction_material() {
        record.clear_transaction_material();
        save_record(config_root, &record).await?;
    }
    if record.phase == MigrationPhase::Publishing {
        credential = restore_source_for_new_marker(config_root, &record, &credential).await?;
        let adopted_marker = record
            .adopted_marker_digest
            .clone()
            .ok_or(DiagnosticCode::PrivateStateInvalid)?;
        record.finish_adoption(&adopted_marker);
    }
    if record.phase == MigrationPhase::Unadopted {
        record.finish_adoption(marker_digest);
        save_record(config_root, &record).await?;
        return current_credential(config_root).await;
    }
    if record.phase == MigrationPhase::Adopted
        && record.adopted_marker_digest.as_deref() == Some(marker_digest)
    {
        return Ok(credential);
    }

    if record.phase == MigrationPhase::Adopted
        || record.pending_marker_digest.as_deref() != Some(marker_digest)
    {
        if record.phase != MigrationPhase::Adopted
            && record.source_credential.as_ref() != Some(&credential)
        {
            return Err(DiagnosticCode::PrivateStateIo);
        }
        let (source_cid, source_generation) =
            certificate_binding(credential.client_cert_pem.clone()).await?;
        record.clear_transaction_material();
        record.phase = MigrationPhase::Unadopted;
        record.pending_marker_digest = Some(marker_digest.to_owned());
        record.source_credential = Some(credential.clone());
        record.source_cid = Some(source_cid);
        record.source_generation = Some(source_generation);
        let (device_label, fields) = crate::private_link::pairing_ceremony_identity(
            platform,
            Ok::<String, ()>(hostname.to_owned()),
        );
        let device_label = truncate_label_to(&device_label, 80).to_owned();
        let client_label = fields
            .get("client_label")
            .and_then(Value::as_str)
            .unwrap_or(&device_label)
            .to_owned();
        let client_label = truncate_label_to(&client_label, 253).to_owned();
        let label = device_label.clone();
        let (key_pem, csr_pem, operation_id) = blocking(move || {
            generate_candidate(&label).map_err(|_| DiagnosticCode::PrivateStateIo)
        })
        .await?;
        let request = RekeyRequest {
            protocol_version: 1,
            operation_id: &operation_id,
            csr: &csr_pem,
            device_label: &device_label,
            client_label: &client_label,
            platform: platform.pairing_platform(),
        };
        record.rekey_request =
            Some(serde_json::to_vec(&request).map_err(|_| DiagnosticCode::PrivateStateInvalid)?);
        record.candidate_key_pem = Some(key_pem);
        record.csr_pem = Some(csr_pem);
        record.rekey_operation_id = Some(operation_id);
        record.phase = MigrationPhase::RekeyPending;
        // A confirmation rejection retires the current credential under the
        // answer lock only while no migration owns it, so a transaction starts
        // under the same lock and only for the credential still on disk.
        let answer_lock = acquire_answer_lock(config_root).await?;
        if current_credential(config_root).await? != credential {
            drop(answer_lock);
            return Err(DiagnosticCode::PrivateStateIo);
        }
        save_record(config_root, &record).await?;
        drop(answer_lock);
    }

    let source = record
        .source_credential
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    if source != credential {
        return Err(DiagnosticCode::PrivateStateIo);
    }
    let old_cid = record
        .source_cid
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let (actual_cid, expected_generation) =
        certificate_binding(source.client_cert_pem.clone()).await?;
    if actual_cid != old_cid
        || record.source_generation.as_deref() != Some(expected_generation.as_str())
    {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    let validation_record = record.clone();
    blocking(move || {
        validate_saved_rekey_request(&validation_record)?;
        if validation_record.decision_request.is_some() {
            validate_saved_decision_request(&validation_record)?;
        }
        Ok(())
    })
    .await?;
    let refresh = VersionRefreshState::new(
        config_root.to_owned(),
        data_root.to_owned(),
        source.instance_id.clone(),
        &source.ca_fp_prefix,
        identity,
    );
    if record.rekey_reply.is_none() {
        let bridge = PrivateLinkBridge::start(source.clone(), None, refresh.clone()).await?;
        let sent = send_rekey(
            &bridge,
            config_root,
            &mut record,
            &source,
            &old_cid,
            now_unix_seconds,
        )
        .await;
        bridge.shutdown().await;
        sent?;
    }

    let validation_record = record.clone();
    let candidate =
        blocking(move || verify_saved_candidate(&validation_record, now_unix_seconds)).await?;
    let expected_candidate_cid =
        certificate_cid_blocking(candidate.client_cert_pem.clone()).await?;
    if record.decision_request.is_none() {
        let key_material = record
            .candidate_key_pem
            .clone()
            .unwrap_or_default()
            .into_bytes();
        let decision_id =
            blocking(move || Ok(uuid_from_material(&key_material, b"decision"))).await?;
        let request = DecisionRequest {
            protocol_version: 1,
            operation_id: &decision_id,
            choice: "new_device",
        };
        record.decision_request =
            Some(serde_json::to_vec(&request).map_err(|_| DiagnosticCode::PrivateStateInvalid)?);
        record.decision_id = Some(decision_id);
        record.phase = MigrationPhase::DecisionPending;
        save_record(config_root, &record).await?;
    }
    let bridge = PrivateLinkBridge::start(candidate, None, refresh).await?;
    let decided = finalize_keep_both(
        &bridge,
        config_root,
        &mut record,
        &old_cid,
        &expected_candidate_cid,
    )
    .await;
    bridge.shutdown().await;
    decided?;
    if record.phase != MigrationPhase::DecisionVerified {
        record.phase = MigrationPhase::DecisionVerified;
        save_record(config_root, &record).await?;
    }
    publish_candidate(config_root, marker_digest, record).await?;
    current_credential(config_root).await
}

/// Sends the saved rekey bytes under the copied credential. Any failure leaves
/// the transaction pending so a later attempt replays the same request.
async fn send_rekey(
    bridge: &PrivateLinkBridge,
    config_root: &Path,
    record: &mut MigrationRecord,
    source: &Credential,
    old_cid: &str,
    now_unix_seconds: i64,
) -> Result<(), DiagnosticCode> {
    let client = JournalClient::bootstrap(bridge).await?;
    let body = record
        .rekey_request
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let (status, response_body) = control_request(&client, Method::POST, REKEY_PATH, Some(body))
        .await
        .ok_or(DiagnosticCode::JournalUnavailable)?;
    if status != StatusCode::CREATED && status != StatusCode::OK {
        return Err(DiagnosticCode::JournalUnavailable);
    }
    let response: RekeyResponse = serde_json::from_slice(&response_body)
        .map_err(|_| DiagnosticCode::JournalContractInvalid)?;
    let operation = record
        .rekey_operation_id
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    if response.protocol_version != 1
        || response.operation_id != operation
        || response.state != "pending"
        || response.previous_cid != old_cid
        || response.cid == old_cid
    {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let key_pem = record
        .candidate_key_pem
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let candidate_source = source.clone();
    let candidate = blocking(move || {
        candidate_credential(
            &response.pairing,
            &key_pem,
            &response.cid,
            &candidate_source,
            now_unix_seconds,
        )
    })
    .await?;
    record.candidate_credential = Some(candidate);
    record.rekey_reply = Some(response_body);
    record.phase = MigrationPhase::RekeyVerified;
    save_record(config_root, record).await
}

/// Records keep-both under the new credential, reconciling a lost decision
/// reply through the migration state. Anything unproven stays pending.
async fn finalize_keep_both(
    bridge: &PrivateLinkBridge,
    config_root: &Path,
    record: &mut MigrationRecord,
    old_cid: &str,
    candidate_cid: &str,
) -> Result<(), DiagnosticCode> {
    let client = JournalClient::bootstrap(bridge).await?;
    if let Some((state_bytes, state)) = get_migration_state(&client).await {
        if is_keep_both_state(&state, record, old_cid) {
            record.decision_state_reply = Some(state_bytes);
            return Ok(());
        }
        if !is_pending_state(&state, record, old_cid) {
            return Err(DiagnosticCode::JournalContractInvalid);
        }
    }
    let body = record
        .decision_request
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let mut invalid_put_reply = false;
    if let Some((status, response_body)) =
        control_request(&client, Method::PUT, MIGRATION_PATH, Some(&body)).await
        && status == StatusCode::OK
    {
        let decision_id = record.decision_id.as_deref().unwrap_or_default();
        match serde_json::from_slice::<DecisionResponse>(&response_body) {
            Ok(response)
                if response.protocol_version == 1
                    && response.operation_id == decision_id
                    && response.state == "new_device"
                    && response.previous_cid.as_deref() == Some(old_cid)
                    && response.cid == candidate_cid
                    && response.replaced_cid.is_none() =>
            {
                let _ = response.display_label;
                record.decision_reply = Some(response_body);
                record.phase = MigrationPhase::DecisionVerified;
                return save_record(config_root, record).await;
            }
            _ => invalid_put_reply = true,
        }
    }
    if let Some((state_bytes, state)) = get_migration_state(&client).await {
        if is_keep_both_state(&state, record, old_cid) {
            record.decision_state_reply = Some(state_bytes);
            return Ok(());
        }
        if !is_pending_state(&state, record, old_cid) {
            return Err(DiagnosticCode::JournalContractInvalid);
        }
    }
    Err(if invalid_put_reply {
        DiagnosticCode::JournalContractInvalid
    } else {
        DiagnosticCode::JournalUnavailable
    })
}

async fn restore_source_for_new_marker(
    config_root: &Path,
    record: &MigrationRecord,
    current: &Credential,
) -> Result<Credential, DiagnosticCode> {
    let source = record
        .source_credential
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let candidate = record
        .candidate_credential
        .as_ref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    if current != &source && current != candidate {
        return Err(DiagnosticCode::PrivateStateIo);
    }
    let lock = acquire_answer_lock(config_root).await?;
    let root = config_root.to_owned();
    let owner = blocking(move || load_credential(&root)).await?;
    if owner.as_ref() != Some(&source) && owner.as_ref() != Some(candidate) {
        drop(lock);
        return Err(DiagnosticCode::PrivateStateIo);
    }
    if let (Some(old_answer), Some(new_answer)) = (
        record.source_answer_bytes.as_deref(),
        record.target_answer_bytes.as_deref(),
    ) {
        let root = config_root.to_owned();
        let actual = blocking(move || read_answer_bytes(&root)).await?;
        if actual.as_deref() == Some(new_answer) {
            let root = config_root.to_owned();
            let bytes = old_answer.to_vec();
            blocking(move || write_answer_bytes(&root, &bytes)).await?;
        } else if actual.as_deref() != Some(old_answer) {
            drop(lock);
            return Err(DiagnosticCode::PrivateStateIo);
        }
    }
    if owner.as_ref() == Some(candidate) {
        let root = config_root.to_owned();
        let source_to_write = source.clone();
        blocking(move || persist_credential(&root, &source_to_write)).await?;
    }
    drop(lock);
    Ok(source)
}

async fn publish_candidate(
    config_root: &Path,
    marker_digest: &str,
    mut record: MigrationRecord,
) -> Result<(), DiagnosticCode> {
    let source = record
        .source_credential
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let validation_record = record.clone();
    let candidate = blocking(move || {
        let candidate = verify_saved_candidate(&validation_record, 0)?;
        verify_decision_proof(&validation_record, &candidate)?;
        Ok(candidate)
    })
    .await?;
    let source_generation = record
        .source_generation
        .clone()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let candidate_generation = pairing_generation(candidate.client_cert_pem.clone()).await?;
    let lock = acquire_answer_lock(config_root).await?;
    let root = config_root.to_owned();
    let current = blocking(move || load_credential(&root)).await?;
    if current.as_ref() != Some(&source) {
        drop(lock);
        return Err(DiagnosticCode::PrivateStateIo);
    }
    let root = config_root.to_owned();
    let answer =
        tokio::task::spawn_blocking(move || (read_answer_file(&root), read_answer_bytes(&root)))
            .await
            .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    let answer_bytes = answer.1?;
    let target_answer = answer
        .0
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .and_then(|answer| {
            confirmed_answer_transfer(answer, &source_generation, &candidate_generation)
        })
        .ok_or(())
        .ok();
    if let Some(target_answer) = target_answer {
        record.source_answer_bytes = answer_bytes;
        record.target_answer_bytes = Some(target_answer);
    } else {
        record.source_answer_bytes = None;
        record.target_answer_bytes = None;
    }
    record.phase = MigrationPhase::Publishing;
    let root = config_root.to_owned();
    let publication_record = record.clone();
    blocking(move || publication_record.persist(&root)).await?;

    let root = config_root.to_owned();
    let candidate_for_write = candidate.clone();
    blocking(move || persist_credential(&root, &candidate_for_write)).await?;
    if let Some(target) = record.target_answer_bytes.as_deref() {
        let root = config_root.to_owned();
        let bytes = target.to_vec();
        blocking(move || write_answer_bytes(&root, &bytes)).await?;
    }
    record.finish_adoption(marker_digest);
    let root = config_root.to_owned();
    blocking(move || record.persist(&root)).await?;
    drop(lock);
    Ok(())
}

fn confirmed_answer_transfer(
    answer: &AnswerRecord,
    source_generation: &str,
    candidate_generation: &str,
) -> Option<Vec<u8>> {
    if source_generation.is_empty() || answer.confirmed != source_generation {
        return None;
    }
    serde_json::to_vec(&AnswerRecord {
        confirmed: candidate_generation.to_owned(),
    })
    .ok()
}

async fn get_migration_state(client: &JournalClient) -> Option<(Vec<u8>, MigrationStateResponse)> {
    let (status, body) = control_request(client, Method::GET, MIGRATION_PATH, None).await?;
    if status != StatusCode::OK {
        return None;
    }
    let response = serde_json::from_slice(&body).ok()?;
    Some((body, response))
}

fn is_pending_state(
    state: &MigrationStateResponse,
    record: &MigrationRecord,
    old_cid: &str,
) -> bool {
    state.protocol_version == 1
        && state.state == "pending"
        && state.rekey_operation_id.as_deref() == record.rekey_operation_id.as_deref()
        && state.previous_cid.as_deref() == Some(old_cid)
        && state.replaced_cid.is_none()
}

fn is_keep_both_state(
    state: &MigrationStateResponse,
    record: &MigrationRecord,
    old_cid: &str,
) -> bool {
    is_keep_both_state_fields(
        state,
        record.rekey_operation_id.as_deref().unwrap_or_default(),
        old_cid,
    )
}

fn is_keep_both_state_fields(
    state: &MigrationStateResponse,
    operation_id: &str,
    old_cid: &str,
) -> bool {
    state.protocol_version == 1
        && state.state == "new_device"
        && state.rekey_operation_id.as_deref() == Some(operation_id)
        && state.previous_cid.as_deref() == Some(old_cid)
        && state.replaced_cid.is_none()
}

async fn control_request(
    client: &JournalClient,
    method: Method,
    path: &str,
    body: Option<&[u8]>,
) -> Option<(StatusCode, Vec<u8>)> {
    let mut request = client.request(method, path).ok()?;
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_vec());
    }
    let mut response = request.send().await.ok()?;
    if response
        .content_length()
        .is_some_and(|length| length > CONTROL_BODY_LIMIT as u64)
    {
        return None;
    }
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len().saturating_add(chunk.len()) > CONTROL_BODY_LIMIT {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some((status, body))
}

fn candidate_credential(
    value: &Value,
    key_pem: &str,
    response_cid: &str,
    source: &Credential,
    now_unix_seconds: i64,
) -> Result<Credential, DiagnosticCode> {
    let pair: PairResponse = serde_json::from_value(value.clone())
        .map_err(|_| DiagnosticCode::JournalContractInvalid)?;
    if pair.instance_id != source.instance_id {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let cert = spl_transport::tls::parse_certs(&pair.client_cert)
        .map_err(|_| DiagnosticCode::JournalContractInvalid)?
        .into_iter()
        .next()
        .ok_or(DiagnosticCode::JournalContractInvalid)?;
    let computed_cid = format!("sha256:{}", spl_core::ca::sha256_hex(cert.as_ref()));
    if response_cid != computed_cid || pair.fingerprint != computed_cid {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let key = KeyPair::from_pem(key_pem).map_err(|_| DiagnosticCode::JournalContractInvalid)?;
    let candidate_spki = key.public_key_der();
    let cert_spki = spl_core::ca::extract_spki_der(cert.as_ref())
        .map_err(|_| DiagnosticCode::JournalContractInvalid)?;
    if candidate_spki != cert_spki {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let old_cert = spl_transport::tls::parse_certs(&source.client_cert_pem)
        .map_err(|_| DiagnosticCode::PrivateStateInvalid)?
        .into_iter()
        .next()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    if spl_core::ca::extract_spki_der(old_cert.as_ref())
        .ok()
        .as_deref()
        == Some(&candidate_spki)
    {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let ca_certs = pair
        .ca_chain
        .iter()
        .flat_map(|pem| spl_transport::tls::parse_certs(pem).unwrap_or_default())
        .collect::<Vec<_>>();
    if ca_certs.is_empty()
        || !ca_certs.iter().any(|cert| {
            let digest = Sha256::digest(cert.as_ref());
            digest.starts_with(&source.ca_fp_prefix)
        })
    {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let endpoints = pair
        .local_endpoints
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let object = value.as_object()?;
            let host = object.get("ip")?.as_str()?;
            host.parse::<IpAddr>().ok()?;
            let port = u16::try_from(object.get("port")?.as_u64()?).ok()?;
            (port != 0).then(|| EndpointAddr {
                host: host.to_owned(),
                port,
            })
        })
        .collect::<Vec<_>>();
    let relay = pair.relay_access.as_ref().and_then(|value| {
        let access: spl_core::relay_access::RelayAccess =
            serde_json::from_value(value.clone()).ok()?;
        if access.status != "ready" || access.protocol_version != 2 {
            return None;
        }
        spl_transport::validate_relay_origin(&access.relay_origin).ok()?;
        let claims = access.claims(&pair.instance_id, now_unix_seconds)?;
        Some((access.relay_origin, access.device_token, claims.exp))
    });
    Ok(Credential {
        client_key_pem: key_pem.to_owned(),
        client_cert_pem: pair.client_cert,
        ca_chain_pem: pair.ca_chain,
        ca_fp_prefix: source.ca_fp_prefix.clone(),
        instance_id: pair.instance_id,
        home_label: pair.home_label,
        endpoints: if endpoints.is_empty() {
            source.endpoints.clone()
        } else {
            endpoints
        },
        home_attestation: pair.home_attestation,
        local_endpoints: pair.local_endpoints,
        relay_origin: relay.as_ref().map(|value| value.0.clone()),
        device_token: relay.as_ref().map(|value| value.1.clone()),
        device_token_expires_at: relay.map(|value| value.2),
    })
}

fn verify_saved_candidate(
    record: &MigrationRecord,
    now_unix_seconds: i64,
) -> Result<Credential, DiagnosticCode> {
    let source = record
        .source_credential
        .as_ref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let old_cid = record
        .source_cid
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let expected_generation = hex_encode(&crate::post_connect::compute_pairing_generation(
        &source.client_cert_pem,
    ));
    if certificate_cid(&source.client_cert_pem)? != old_cid
        || record.source_generation.as_deref() != Some(expected_generation.as_str())
    {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }

    validate_saved_rekey_request(record)?;
    let operation_id = record
        .rekey_operation_id
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;

    let response: RekeyResponse = serde_json::from_slice(
        record
            .rekey_reply
            .as_deref()
            .ok_or(DiagnosticCode::PrivateStateInvalid)?,
    )
    .map_err(|_| DiagnosticCode::JournalContractInvalid)?;
    if response.protocol_version != 1
        || response.operation_id != operation_id
        || response.state != "pending"
        || response.previous_cid != old_cid
        || response.cid == old_cid
    {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let key_pem = record
        .candidate_key_pem
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let verified = candidate_credential(
        &response.pairing,
        key_pem,
        &response.cid,
        source,
        now_unix_seconds,
    )?;
    let stored = record
        .candidate_credential
        .as_ref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    if stored.client_key_pem != verified.client_key_pem
        || stored.client_cert_pem != verified.client_cert_pem
        || stored.ca_chain_pem != verified.ca_chain_pem
        || stored.ca_fp_prefix != verified.ca_fp_prefix
        || stored.instance_id != verified.instance_id
    {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    Ok(stored.clone())
}

fn validate_saved_rekey_request(record: &MigrationRecord) -> Result<(), DiagnosticCode> {
    let operation_id = record
        .rekey_operation_id
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let csr = record
        .csr_pem
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let request: SavedRekeyRequest = serde_json::from_slice(
        record
            .rekey_request
            .as_deref()
            .ok_or(DiagnosticCode::PrivateStateInvalid)?,
    )
    .map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    if request.protocol_version != 1
        || !is_uuid(operation_id)
        || request.operation_id != operation_id
        || request.csr != csr
        || request.device_label.is_empty()
        || request.device_label.len() > 80
        || request.client_label.is_empty()
        || request.client_label.len() > 253
        || !matches!(request.platform.as_str(), "linux" | "macos")
    {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    Ok(())
}

fn validate_saved_decision_request(record: &MigrationRecord) -> Result<(), DiagnosticCode> {
    let decision_id = record
        .decision_id
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let request: SavedDecisionRequest = serde_json::from_slice(
        record
            .decision_request
            .as_deref()
            .ok_or(DiagnosticCode::PrivateStateInvalid)?,
    )
    .map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
    if request.protocol_version != 1
        || !is_uuid(decision_id)
        || request.operation_id != decision_id
        || request.choice != "new_device"
    {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    Ok(())
}

fn verify_decision_proof(
    record: &MigrationRecord,
    candidate: &Credential,
) -> Result<(), DiagnosticCode> {
    let source_cid = record
        .source_cid
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let rekey_operation_id = record
        .rekey_operation_id
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    validate_saved_decision_request(record)?;
    let decision_id = record
        .decision_id
        .as_deref()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    let candidate_cid = certificate_cid(&candidate.client_cert_pem)?;
    let mut has_proof = false;
    if let Some(bytes) = record.decision_reply.as_deref() {
        let response: DecisionResponse =
            serde_json::from_slice(bytes).map_err(|_| DiagnosticCode::JournalContractInvalid)?;
        if response.protocol_version != 1
            || response.operation_id != decision_id
            || response.state != "new_device"
            || response.previous_cid.as_deref() != Some(source_cid)
            || response.cid != candidate_cid
            || response.replaced_cid.is_some()
        {
            return Err(DiagnosticCode::JournalContractInvalid);
        }
        let _ = response.display_label;
        has_proof = true;
    }
    if let Some(bytes) = record.decision_state_reply.as_deref() {
        let state: MigrationStateResponse =
            serde_json::from_slice(bytes).map_err(|_| DiagnosticCode::JournalContractInvalid)?;
        if !is_keep_both_state_fields(&state, rekey_operation_id, source_cid) {
            return Err(DiagnosticCode::JournalContractInvalid);
        }
        has_proof = true;
    }
    if !has_proof {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    Ok(())
}

fn verify_publishing_record(record: &MigrationRecord) -> Result<Credential, DiagnosticCode> {
    if record.phase != MigrationPhase::Publishing {
        return Err(DiagnosticCode::PrivateStateInvalid);
    }
    let candidate = verify_saved_candidate(record, 0)?;
    verify_decision_proof(record, &candidate)?;
    if let (Some(source_bytes), Some(target_bytes)) = (
        record.source_answer_bytes.as_deref(),
        record.target_answer_bytes.as_deref(),
    ) {
        let source_answer: AnswerRecord = serde_json::from_slice(source_bytes)
            .map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
        let target_answer: AnswerRecord = serde_json::from_slice(target_bytes)
            .map_err(|_| DiagnosticCode::PrivateStateInvalid)?;
        let source_generation = record
            .source_generation
            .as_deref()
            .ok_or(DiagnosticCode::PrivateStateInvalid)?;
        let candidate_generation = hex_encode(&crate::post_connect::compute_pairing_generation(
            &candidate.client_cert_pem,
        ));
        if source_answer.confirmed != source_generation
            || target_answer.confirmed != candidate_generation
        {
            return Err(DiagnosticCode::PrivateStateInvalid);
        }
    }
    Ok(candidate)
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| value.as_bytes()[index] == b'-')
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

fn certificate_cid(cert_pem: &str) -> Result<String, DiagnosticCode> {
    let cert = spl_transport::tls::parse_certs(cert_pem)
        .map_err(|_| DiagnosticCode::PrivateStateInvalid)?
        .into_iter()
        .next()
        .ok_or(DiagnosticCode::PrivateStateInvalid)?;
    Ok(format!(
        "sha256:{}",
        spl_core::ca::sha256_hex(cert.as_ref())
    ))
}

async fn certificate_binding(cert_pem: String) -> Result<(String, String), DiagnosticCode> {
    tokio::task::spawn_blocking(move || {
        let cid = certificate_cid(&cert_pem)?;
        let generation = hex_encode(&crate::post_connect::compute_pairing_generation(&cert_pem));
        Ok((cid, generation))
    })
    .await
    .map_err(|_| DiagnosticCode::PrivateStateIo)?
}

async fn certificate_cid_blocking(cert_pem: String) -> Result<String, DiagnosticCode> {
    tokio::task::spawn_blocking(move || certificate_cid(&cert_pem))
        .await
        .map_err(|_| DiagnosticCode::PrivateStateIo)?
}

async fn pairing_generation(cert_pem: String) -> Result<String, DiagnosticCode> {
    tokio::task::spawn_blocking(move || {
        hex_encode(&crate::post_connect::compute_pairing_generation(&cert_pem))
    })
    .await
    .map_err(|_| DiagnosticCode::PrivateStateIo)
}

pub async fn host_marker_digest(
    platform: PlatformKind,
    runner: &dyn CommandRunner,
) -> Result<String, ()> {
    host_marker_digest_with(platform, runner, None).await
}

pub async fn host_marker_digest_with(
    platform: PlatformKind,
    runner: &dyn CommandRunner,
    linux_machine_id_fixture: Option<Vec<u8>>,
) -> Result<String, ()> {
    let marker = match platform {
        PlatformKind::Linux => match linux_machine_id_fixture {
            Some(bytes) => parse_machine_id(&bytes).ok_or(())?,
            None => tokio::task::spawn_blocking(read_linux_machine_id)
                .await
                .map_err(|_| ())??,
        },
        PlatformKind::Macos => {
            let output = runner
                .run(CommandInvocation {
                    operation: CommandOperation::HostFact,
                    executable: PathBuf::from("/usr/sbin/ioreg"),
                    args: ["-rd1", "-c", "IOPlatformExpertDevice"]
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    timeout: IOREG_TIMEOUT,
                })
                .await
                .map_err(|_| ())?;
            if output.status != 0 {
                return Err(());
            }
            parse_ioreg_platform_uuid(&output.stdout).ok_or(())?
        }
    };
    tokio::task::spawn_blocking(move || {
        let mut hasher = Sha256::new();
        hasher.update(MARKER_DOMAIN);
        hasher.update(platform.pairing_platform().as_bytes());
        hasher.update([0]);
        hasher.update(marker);
        hex_encode(&hasher.finalize())
    })
    .await
    .map_err(|_| ())
}

fn read_linux_machine_id() -> Result<[u8; 16], ()> {
    let mut file = open_regular_readonly(Path::new("/etc/machine-id")).map_err(|_| ())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| ())?;
    parse_machine_id(&bytes).ok_or(())
}

pub fn parse_machine_id(bytes: &[u8]) -> Option<[u8; 16]> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.len() != 32 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut output = [0; 16];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = (hex_nibble(bytes[index * 2])? << 4) | hex_nibble(bytes[index * 2 + 1])?;
    }
    Some(output)
}

pub fn parse_ioreg_platform_uuid(output: &[u8]) -> Option<[u8; 16]> {
    let text = std::str::from_utf8(output).ok()?;
    let matches = text
        .lines()
        .filter(|line| line.contains("IOPlatformUUID"))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return None;
    }
    let line = matches[0].trim();
    let value = line
        .strip_prefix("\"IOPlatformUUID\" = \"")?
        .strip_suffix('"')?;
    parse_uuid(value)
}

fn parse_uuid(value: &str) -> Option<[u8; 16]> {
    if value.len() != 36
        || ![8, 13, 18, 23]
            .into_iter()
            .all(|index| value.as_bytes()[index] == b'-')
    {
        return None;
    }
    let digits = value
        .bytes()
        .filter(|byte| *byte != b'-')
        .collect::<Vec<_>>();
    let mut output = [0; 16];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = (hex_nibble(digits[index * 2])? << 4) | hex_nibble(digits[index * 2 + 1])?;
    }
    Some(output)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn generate_candidate(device_label: &str) -> Result<(String, String, String), DiagnosticCode> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    let mut params =
        CertificateParams::new(Vec::<String>::new()).map_err(|_| DiagnosticCode::PrivateStateIo)?;
    let mut distinguished = DistinguishedName::new();
    distinguished.push(DnType::CommonName, truncate_label(device_label));
    params.distinguished_name = distinguished;
    let csr = params
        .serialize_request(&key)
        .map_err(|_| DiagnosticCode::PrivateStateIo)?;
    let csr_pem = csr.pem().map_err(|_| DiagnosticCode::PrivateStateIo)?;
    let key_pem = key.serialize_pem();
    let operation_id = uuid_from_material(key_pem.as_bytes(), b"rekey");
    Ok((key_pem, csr_pem, operation_id))
}

pub fn uuid_from_material(material: &[u8], domain: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"solstone-tmux/device-migration/operation/v1\0");
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(material);
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn truncate_label(label: &str) -> &str {
    truncate_label_to(label, 64)
}

fn truncate_label_to(label: &str, limit: usize) -> &str {
    let mut end = label.len().min(limit);
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    &label[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::pairing_answer::write_answer_file;
    use crate::paths::ensure_private_directory;
    use crate::private_link::{CREDENTIALS_FILENAME, load_credential};
    use crate::storage::{AtomicWriteFault, set_atomic_write_fault_for_path};
    use spl_transport::credential::Credential;
    use spl_transport::credential::EndpointAddr;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = PathBuf::from(format!(
                "/var/tmp/solstone-tmux-device-migration-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).expect("create isolated migration root");
            ensure_private_directory(&root).expect("secure isolated migration root");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_credential(certificate: &str) -> Credential {
        Credential {
            client_key_pem: "private-key".to_owned(),
            client_cert_pem: certificate.to_owned(),
            ca_chain_pem: vec!["ca-certificate".to_owned()],
            ca_fp_prefix: vec![1, 2, 3, 4],
            instance_id: "fixture-instance".to_owned(),
            home_label: "fixture-home".to_owned(),
            endpoints: vec![EndpointAddr {
                host: "127.0.0.1".to_owned(),
                port: 7657,
            }],
            home_attestation: None,
            local_endpoints: None,
            relay_origin: None,
            device_token: None,
            device_token_expires_at: None,
        }
    }

    fn publication_fixture(marker_digest: &str) -> (Credential, Credential, MigrationRecord) {
        use rcgen::{BasicConstraints, IsCa, KeyUsagePurpose};

        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("CA key");
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        let ca = ca_params.self_signed(&ca_key).expect("self-sign CA");
        let ca_pem = ca.pem();
        let ca_fingerprint = Sha256::digest(ca.der().as_ref());
        let old_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("source key");
        let old_cert = CertificateParams::new(Vec::<String>::new())
            .expect("source cert params")
            .signed_by(&old_key, &ca, &ca_key)
            .expect("sign source cert");
        let candidate_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("candidate key");
        let candidate_cert = CertificateParams::new(Vec::<String>::new())
            .expect("candidate cert params")
            .signed_by(&candidate_key, &ca, &ca_key)
            .expect("sign candidate cert");
        let candidate_key_pem = candidate_key.serialize_pem();
        let csr = CertificateParams::new(Vec::<String>::new())
            .expect("CSR params")
            .serialize_request(&candidate_key)
            .expect("serialize CSR")
            .pem()
            .expect("encode CSR");
        let source_cid = format!("sha256:{}", spl_core::ca::sha256_hex(old_cert.der()));
        let candidate_cid = format!("sha256:{}", spl_core::ca::sha256_hex(candidate_cert.der()));
        let source = Credential {
            client_key_pem: old_key.serialize_pem(),
            client_cert_pem: old_cert.pem(),
            ca_chain_pem: vec![ca_pem.clone()],
            ca_fp_prefix: ca_fingerprint[..16].to_vec(),
            instance_id: "fixture-instance".to_owned(),
            home_label: "fixture-home".to_owned(),
            endpoints: vec![EndpointAddr {
                host: "127.0.0.1".to_owned(),
                port: 7657,
            }],
            home_attestation: None,
            local_endpoints: None,
            relay_origin: None,
            device_token: None,
            device_token_expires_at: None,
        };
        let source_generation = hex_encode(&crate::post_connect::compute_pairing_generation(
            &source.client_cert_pem,
        ));
        let rekey_operation_id = uuid_from_material(candidate_key_pem.as_bytes(), b"rekey");
        let decision_id = uuid_from_material(candidate_key_pem.as_bytes(), b"decision");
        let rekey_request = RekeyRequest {
            protocol_version: 1,
            operation_id: &rekey_operation_id,
            csr: &csr,
            device_label: "machine",
            client_label: "machine",
            platform: "linux",
        };
        let rekey_request_bytes =
            serde_json::to_vec(&rekey_request).expect("serialize rekey request");
        let rekey_reply = serde_json::json!({
            "protocol_version": 1,
            "operation_id": rekey_operation_id,
            "state": "pending",
            "previous_cid": source_cid,
            "cid": candidate_cid,
            "pairing": {
                "client_cert": candidate_cert.pem(),
                "ca_chain": [ca_pem],
                "instance_id": source.instance_id,
                "home_label": "fixture candidate",
                "fingerprint": candidate_cid,
                "home_attestation": null
            }
        });
        let pairing: Value = rekey_reply["pairing"].clone();
        let candidate = candidate_credential(
            &pairing,
            &candidate_key_pem,
            &candidate_cid,
            &source,
            1_800_000_000,
        )
        .expect("verify candidate fixture");
        let decision_request = DecisionRequest {
            protocol_version: 1,
            operation_id: &decision_id,
            choice: "new_device",
        };
        let decision_request_bytes =
            serde_json::to_vec(&decision_request).expect("serialize decision request");
        let decision_reply = serde_json::json!({
            "protocol_version": 1,
            "operation_id": decision_id,
            "state": "new_device",
            "previous_cid": source_cid,
            "cid": candidate_cid,
            "replaced_cid": null,
            "display_label": "fixture"
        });
        let record = MigrationRecord {
            phase: MigrationPhase::DecisionVerified,
            pending_marker_digest: Some(marker_digest.to_owned()),
            source_cid: Some(source_cid),
            source_generation: Some(source_generation),
            source_credential: Some(source.clone()),
            candidate_credential: Some(candidate.clone()),
            rekey_operation_id: Some(rekey_operation_id),
            candidate_key_pem: Some(candidate_key_pem),
            csr_pem: Some(csr),
            rekey_request: Some(rekey_request_bytes),
            rekey_reply: Some(serde_json::to_vec(&rekey_reply).expect("serialize reply")),
            decision_id: Some(decision_id),
            decision_request: Some(decision_request_bytes),
            decision_reply: Some(
                serde_json::to_vec(&decision_reply).expect("serialize decision reply"),
            ),
            ..MigrationRecord::default()
        };
        (source, candidate, record)
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create runtime")
    }

    #[derive(Clone)]
    struct MarkerRunner {
        result: Result<crate::command::CommandOutput, String>,
        calls: Arc<Mutex<Vec<CommandInvocation>>>,
    }

    impl CommandRunner for MarkerRunner {
        fn run<'a>(
            &'a self,
            invocation: CommandInvocation,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            crate::command::CommandOutput,
                            crate::command::CommandError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.calls.lock().expect("calls lock").push(invocation);
            let result = self.result.clone().map_err(|message| {
                crate::command::CommandError::Spawn(std::io::Error::other(message))
            });
            Box::pin(async move { result })
        }
    }

    #[test]
    fn contract_vectors_pass_typed_request_and_reply_validation() {
        let document: Value = serde_json::from_slice(include_bytes!(
            "../vendor/device-migration-contract/contracts/v1.vectors.json"
        ))
        .expect("parse vendored vectors");
        let vectors = &document["vectors"];
        let edited = |value: &Value, field: &str, replacement: Option<Value>| {
            let mut value = value.clone();
            let object = value.as_object_mut().expect("vector object");
            match replacement {
                Some(replacement) => object.insert(field.to_owned(), replacement),
                None => object.remove(field),
            };
            value
        };
        let accepts = |kind: &str, value: &Value| {
            let bytes = serde_json::to_vec(value).expect("serialize vector");
            match kind {
                "rekey_request" => serde_json::from_slice::<SavedRekeyRequest>(&bytes).is_ok(),
                "rekey_reply" => {
                    serde_json::from_slice::<RekeyResponse>(&bytes).is_ok_and(|reply| {
                        serde_json::from_value::<PairResponse>(reply.pairing).is_ok()
                    })
                }
                "state" => serde_json::from_slice::<MigrationStateResponse>(&bytes).is_ok(),
                "decision_request" => {
                    serde_json::from_slice::<SavedDecisionRequest>(&bytes).is_ok()
                }
                "decision_reply" => serde_json::from_slice::<DecisionResponse>(&bytes).is_ok(),
                _ => unreachable!(),
            }
        };
        let rekey_request = &vectors["rekey_request"];
        let first_state = &vectors["migration_states"][0];
        let created = &vectors["rekey_created_201"]["body"];
        let mut cases = vec![
            ("rekey_request", rekey_request.clone(), true),
            (
                "rekey_request",
                edited(rekey_request, "replaces_cid", Some(Value::Null)),
                false,
            ),
            ("rekey_request", edited(rekey_request, "csr", None), false),
            ("rekey_reply", edited(created, "cid", None), false),
            (
                "rekey_reply",
                edited(created, "extra", Some(Value::Bool(true))),
                false,
            ),
            ("state", edited(first_state, "replaced_cid", None), false),
            ("decision_reply", vectors["decision_response"].clone(), true),
            (
                "decision_reply",
                edited(&vectors["decision_response"], "cid", None),
                false,
            ),
            // This client only keeps both devices, so a replacement is never valid.
            (
                "decision_request",
                vectors["replace_request"].clone(),
                false,
            ),
        ];
        for name in [
            "rekey_created_201",
            "rekey_replay_200",
            "rekey_without_network_metadata",
        ] {
            cases.push(("rekey_reply", vectors[name]["body"].clone(), true));
        }
        for state in vectors["migration_states"]
            .as_array()
            .expect("state vectors")
        {
            cases.push(("state", state.clone(), true));
        }
        for (kind, value, valid) in cases {
            assert_eq!(accepts(kind, &value), valid, "{kind}: {value}");
        }

        let field = |name: &str| rekey_request[name].as_str().expect("request field");
        let request = RekeyRequest {
            protocol_version: 1,
            operation_id: field("operation_id"),
            csr: field("csr"),
            device_label: field("device_label"),
            client_label: field("client_label"),
            platform: field("platform"),
        };
        assert_eq!(
            &serde_json::to_value(&request).expect("serialize"),
            rekey_request
        );
        let replacement = &vectors["replace_request"];
        let decision = DecisionRequest {
            protocol_version: 1,
            operation_id: replacement["operation_id"].as_str().expect("operation id"),
            choice: "new_device",
        };
        let keep_both = edited(
            &edited(replacement, "replaces_cid", None),
            "choice",
            Some(Value::from("new_device")),
        );
        assert_eq!(
            serde_json::to_value(&decision).expect("serialize"),
            keep_both
        );
        assert!(accepts("decision_request", &keep_both));
    }

    #[test]
    fn linux_marker_accepts_only_exact_hex_id() {
        assert_eq!(
            parse_machine_id(b"00112233445566778899aabbccddeeff\n").unwrap()[0],
            0
        );
        assert!(parse_machine_id(b"00112233445566778899aabbccddeefg\n").is_none());
        assert!(parse_machine_id(b" 00112233445566778899aabbccddeeff\n").is_none());
        assert!(parse_machine_id(b"00112233445566778899aabbccddeeff\n\n").is_none());
    }

    #[test]
    fn macos_marker_requires_one_exact_uuid_property() {
        let valid = b"    \"IOPlatformUUID\" = \"00112233-4455-6677-8899-aabbccddeeff\"\n";
        assert!(parse_ioreg_platform_uuid(valid).is_some());
        assert!(parse_ioreg_platform_uuid(b"other property\n").is_none());
        assert!(parse_ioreg_platform_uuid(
            b"\"IOPlatformUUID\" = \"00112233-4455-6677-8899-aabbccddeeff\"\n\"IOPlatformUUID\" = \"00112233-4455-6677-8899-aabbccddeeff\"\n"
        )
        .is_none());
        assert!(parse_ioreg_platform_uuid(b"\"IOPlatformUUID\" = \"bad\"\n").is_none());
        assert!(parse_ioreg_platform_uuid(b"IOPlatformUUID <class>\n").is_none());
    }

    #[test]
    fn marker_provider_uses_only_injected_linux_and_macos_facts() {
        let runtime = test_runtime();
        let linux_runner = MarkerRunner {
            result: Err("Linux must not invoke a command".to_owned()),
            calls: Arc::default(),
        };
        let linux = runtime
            .block_on(host_marker_digest_with(
                PlatformKind::Linux,
                &linux_runner,
                Some(b"00112233445566778899aabbccddeeff\n".to_vec()),
            ))
            .expect("injected Linux marker");
        assert_eq!(
            linux,
            "094b936094c83e3ad2e9f771be0541387cb55f7a6cc91713b51559f7a4b7929a"
        );
        assert!(linux_runner.calls.lock().expect("calls").is_empty());
        assert!(
            runtime
                .block_on(host_marker_digest_with(
                    PlatformKind::Linux,
                    &linux_runner,
                    Some(Vec::new()),
                ))
                .is_err()
        );

        let calls: Arc<Mutex<Vec<CommandInvocation>>> = Arc::default();
        let mac_runner = MarkerRunner {
            result: Ok(crate::command::CommandOutput {
                stdout: b"    \"IOPlatformUUID\" = \"00112233-4455-6677-8899-aabbccddeeff\"\n"
                    .to_vec(),
                stderr: Vec::new(),
                status: 0,
            }),
            calls: calls.clone(),
        };
        let macos = runtime
            .block_on(host_marker_digest_with(
                PlatformKind::Macos,
                &mac_runner,
                None,
            ))
            .expect("sanitized macOS marker");
        assert_eq!(
            macos,
            "aa4cde633064d694ef114cf652a52e903073e86882d6065a1730873202e6e2b2"
        );
        let invocations = calls.lock().expect("calls");
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].operation, CommandOperation::HostFact);
        assert_eq!(invocations[0].executable, PathBuf::from("/usr/sbin/ioreg"));
        assert_eq!(
            invocations[0].args,
            ["-rd1", "-c", "IOPlatformExpertDevice"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
        drop(invocations);

        let unavailable = MarkerRunner {
            result: Err("injected unavailable probe".to_owned()),
            calls: Arc::default(),
        };
        assert!(
            runtime
                .block_on(host_marker_digest_with(
                    PlatformKind::Macos,
                    &unavailable,
                    None
                ))
                .is_err()
        );
    }

    #[test]
    fn initial_adoption_crash_restarts_as_initial_without_a_move() {
        let root = TempRoot::new();
        let credential = test_credential("initial-certificate");
        crate::private_link::persist_credential(root.path(), &credential)
            .expect("persist fixture credential");
        MigrationRecord::default()
            .persist(root.path())
            .expect("persist pre-adoption state");

        assert_eq!(
            test_runtime().block_on(migrate_if_needed(
                root.path(),
                root.path(),
                PlatformKind::Linux,
                "machine",
                "malformed-marker",
                credential.clone(),
                crate::instance_lock::RunIdentity {
                    run_id: "test-run".to_owned(),
                    lock_inode: 1,
                },
                1_800_000_000,
            )),
            Err(DiagnosticCode::PrivateStateInvalid)
        );
        assert_eq!(
            record_setup_baseline(root.path(), Some("missing-marker")),
            Err(DiagnosticCode::PrivateStateInvalid)
        );

        let result = test_runtime().block_on(migrate_if_needed(
            root.path(),
            root.path(),
            PlatformKind::Linux,
            "machine",
            &"a".repeat(64),
            credential.clone(),
            crate::instance_lock::RunIdentity {
                run_id: "test-run".to_owned(),
                lock_inode: 1,
            },
            1_800_000_000,
        ));
        assert_eq!(result.expect("initial adoption"), credential);
        let adopted = MigrationRecord::load(root.path())
            .expect("load adopted record")
            .expect("record exists");
        assert_eq!(adopted.phase, MigrationPhase::Adopted);
        assert_eq!(
            adopted.adopted_marker_digest.as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert_eq!(
            load_credential(root.path()).expect("load credential"),
            Some(test_credential("initial-certificate"))
        );
    }

    #[test]
    fn walk_away_pause_survives_baseline_adoption_and_restart() {
        let root = TempRoot::new();
        let credential = test_credential("walk-away-certificate");
        crate::private_link::persist_credential(root.path(), &credential)
            .expect("persist held credential");
        write_answer_file(root.path(), "").expect("persist walk-away pause");
        record_setup_baseline(root.path(), Some(&"a".repeat(64))).expect("persist setup baseline");

        let result = test_runtime().block_on(migrate_if_needed(
            root.path(),
            root.path(),
            PlatformKind::Linux,
            "machine",
            &"a".repeat(64),
            credential.clone(),
            crate::instance_lock::RunIdentity {
                run_id: "test-run".to_owned(),
                lock_inode: 1,
            },
            1_800_000_000,
        ));
        assert_eq!(result.expect("same-machine restart"), credential);
        assert_eq!(
            read_answer_file(root.path())
                .expect("read pause")
                .expect("pause remains present")
                .confirmed,
            ""
        );
    }

    #[test]
    fn pending_migration_does_not_grandfather_an_absent_answer() {
        let root = TempRoot::new();
        let (source, _candidate, record) = publication_fixture(&"a".repeat(64));
        crate::private_link::persist_credential(root.path(), &source)
            .expect("persist migration source");
        record
            .persist(root.path())
            .expect("persist pending migration");
        assert!(
            read_answer_file(root.path())
                .expect("read absent answer")
                .is_none()
        );

        grandfather_or_settle_if_idle(root.path()).expect("preserve migration snapshot");
        assert!(
            read_answer_file(root.path())
                .expect("read preserved absent answer")
                .is_none()
        );
    }

    #[test]
    fn setup_without_marker_supersedes_pending_migration_without_adopting() {
        let root = TempRoot::new();
        let source = test_credential("superseded-migration-source");
        let operation_id = uuid_from_material(b"pending", b"rekey");
        let record = MigrationRecord {
            phase: MigrationPhase::RekeyPending,
            pending_marker_digest: Some("a".repeat(64)),
            source_cid: Some(format!("sha256:{}", "b".repeat(64))),
            source_generation: Some("c".repeat(64)),
            source_credential: Some(source),
            rekey_operation_id: Some(operation_id.clone()),
            candidate_key_pem: Some("candidate-key".to_owned()),
            csr_pem: Some("candidate-csr".to_owned()),
            rekey_request: Some(
                serde_json::to_vec(&serde_json::json!({
                    "protocol_version": 1,
                    "operation_id": operation_id,
                    "csr": "candidate-csr",
                    "device_label": "machine",
                    "client_label": "machine",
                    "platform": "linux"
                }))
                .expect("serialize pending request"),
            ),
            ..MigrationRecord::default()
        };
        record
            .persist(root.path())
            .expect("persist old pending migration");

        record_setup_baseline(root.path(), None).expect("reset setup ownership");
        let reset = MigrationRecord::load(root.path())
            .expect("load reset state")
            .expect("migration record remains");
        assert_eq!(reset.phase, MigrationPhase::Unadopted);
        assert!(reset.adopted_marker_digest.is_none());
        assert!(reset.pending_marker_digest.is_none());
        assert!(reset.source_credential.is_none());
        assert!(reset.candidate_key_pem.is_none());
        assert!(reset.rekey_request.is_none());
    }

    #[test]
    fn restarted_sync_supersedes_a_transaction_after_fresh_setup() {
        let root = TempRoot::new();
        let (_source, _candidate, record) = publication_fixture(&"a".repeat(64));
        let fresh_setup = test_credential("fresh-setup-after-interruption");
        crate::private_link::persist_credential(root.path(), &fresh_setup)
            .expect("persist fresh setup credential");
        write_answer_file(root.path(), "").expect("preserve fresh setup pause");
        record
            .persist(root.path())
            .expect("persist stopped migration");

        assert!(
            test_runtime()
                .block_on(supersede_stale_setup(
                    root.path(),
                    &record,
                    &fresh_setup,
                    &"b".repeat(64),
                ))
                .expect("recognize completed setup owner")
        );
        assert_eq!(
            load_credential(root.path()).expect("load current setup credential"),
            Some(fresh_setup)
        );
        assert_eq!(
            read_answer_file(root.path())
                .expect("read preserved setup answer")
                .expect("answer remains present")
                .confirmed,
            ""
        );
        let reset = MigrationRecord::load(root.path())
            .expect("load reset migration")
            .expect("record remains");
        assert_eq!(reset.phase, MigrationPhase::Adopted);
        assert_eq!(
            reset.adopted_marker_digest.as_deref(),
            Some("b".repeat(64).as_str())
        );
        assert!(reset.source_credential.is_none());
        assert!(reset.rekey_request.is_none());
    }

    #[test]
    fn migration_record_refuses_symlinks_and_preserves_old_state_on_write_failure() {
        use std::os::unix::fs::symlink;

        let _fault_guard = crate::storage::ATOMIC_FAULT_TEST_LOCK
            .lock()
            .expect("fault test lock");
        let root = TempRoot::new();
        let record = MigrationRecord::default();
        let mut invalid = record.clone();
        invalid.phase = MigrationPhase::Publishing;
        assert_eq!(
            invalid.persist(root.path()),
            Err(DiagnosticCode::PrivateStateInvalid)
        );
        record.persist(root.path()).expect("persist initial record");
        let record_path = root.path().join(RECORD_FILENAME);
        let original = fs::read(&record_path).expect("read original record");
        set_atomic_write_fault_for_path(&record_path, Some(AtomicWriteFault::FailBeforeRename));
        assert_eq!(
            record.persist(root.path()),
            Err(DiagnosticCode::PrivateStateIo)
        );
        set_atomic_write_fault_for_path(&record_path, None);
        assert_eq!(
            fs::read(&record_path).expect("old record remains"),
            original
        );

        fs::remove_file(&record_path).expect("remove migration record");
        let target = root.path().join("target.json");
        fs::write(&target, original).expect("write symlink target");
        symlink(&target, &record_path).expect("create state symlink");
        assert_eq!(
            MigrationRecord::load(root.path()),
            Err(DiagnosticCode::PrivateStateInvalid)
        );
    }

    #[test]
    fn changed_marker_transfers_only_a_proven_confirmation() {
        for (label, answer, transfer) in [
            ("confirmed", Some(r#"{"confirmed":"SOURCE"}"#), true),
            ("awaiting", Some(r#"{"confirmed":""}"#), false),
            ("rejected", Some(r#"{"confirmed":""}"#), false),
            (
                "mismatched",
                Some(
                    r#"{"confirmed":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
                ),
                false,
            ),
            ("malformed", Some("not-json"), false),
            ("absent", None, false),
        ] {
            let root = TempRoot::new();
            let (source, candidate, record) = publication_fixture(&"c".repeat(64));
            let source_generation = hex_encode(&crate::post_connect::compute_pairing_generation(
                &source.client_cert_pem,
            ));
            let candidate_generation = hex_encode(
                &crate::post_connect::compute_pairing_generation(&candidate.client_cert_pem),
            );
            let source_answer = format!(r#"{{"confirmed":"{source_generation}"}}"#);
            crate::private_link::persist_credential(root.path(), &source)
                .expect("persist source credential");
            if let Some(answer) = answer {
                let answer = if answer.contains("SOURCE") {
                    source_answer.as_str()
                } else {
                    answer
                };
                fs::write(root.path().join(ANSWER_FILENAME), answer)
                    .expect("write source answer fixture");
            }
            test_runtime()
                .block_on(publish_candidate(root.path(), &"c".repeat(64), record))
                .expect("publish candidate");
            let loaded = load_credential(root.path())
                .expect("load published credential")
                .expect("published credential exists");
            assert_eq!(loaded, candidate, "{label}");
            let answer_record = crate::pairing_answer::read_answer_file(root.path());
            if transfer {
                assert_eq!(
                    answer_record
                        .expect("valid transferred answer")
                        .unwrap()
                        .confirmed,
                    candidate_generation,
                    "{label}"
                );
            } else if label == "absent" {
                assert!(answer_record.expect("absent remains absent").is_none());
            } else if label == "malformed" {
                assert!(answer_record.is_err());
            } else {
                assert_eq!(
                    answer_record
                        .expect("valid non-confirmation")
                        .unwrap()
                        .confirmed,
                    if label == "mismatched" {
                        "b".repeat(64)
                    } else {
                        String::new()
                    },
                    "{label}"
                );
            }
            let adopted = MigrationRecord::load(root.path())
                .expect("load record")
                .expect("record exists");
            assert_eq!(adopted.phase, MigrationPhase::Adopted);
            assert!(!adopted.has_transaction_material(), "{label}");
            assert!(adopted.source_credential.is_none(), "{label}");
            assert!(adopted.candidate_credential.is_none(), "{label}");
            assert!(adopted.candidate_key_pem.is_none(), "{label}");
            assert_eq!(
                adopted.adopted_marker_digest.as_deref(),
                Some("c".repeat(64).as_str())
            );
        }
    }

    #[test]
    fn adopted_transition_persists_no_source_or_candidate_secret_material() {
        let root = TempRoot::new();
        let marker = "c".repeat(64);
        let (source, candidate, record) = publication_fixture(&marker);
        let source_generation = hex_encode(&crate::post_connect::compute_pairing_generation(
            &source.client_cert_pem,
        ));
        crate::private_link::persist_credential(root.path(), &source)
            .expect("persist source credential");
        write_answer_file(root.path(), &source_generation).expect("confirm source credential");

        test_runtime()
            .block_on(publish_candidate(root.path(), &marker, record))
            .expect("complete migration");

        let adopted = MigrationRecord::load(root.path())
            .expect("load durable adopted state")
            .expect("adopted state exists");
        assert_eq!(adopted.phase, MigrationPhase::Adopted);
        assert!(!adopted.has_transaction_material());
        assert!(adopted.source_credential.is_none());
        assert!(adopted.candidate_credential.is_none());
        assert!(adopted.candidate_key_pem.is_none());
        assert!(adopted.csr_pem.is_none());
        assert_eq!(
            load_credential(root.path()).expect("load new credential"),
            Some(candidate)
        );
    }

    #[test]
    fn interrupted_credential_and_answer_publications_recover_confirmation() {
        let _fault_guard = crate::storage::ATOMIC_FAULT_TEST_LOCK
            .lock()
            .expect("fault test lock");
        for (label, target, fault) in [
            (
                "credential-after-rename",
                CREDENTIALS_FILENAME,
                AtomicWriteFault::FailAfterRename,
            ),
            (
                "answer-before-rename",
                ANSWER_FILENAME,
                AtomicWriteFault::FailBeforeRename,
            ),
        ] {
            let root = TempRoot::new();
            let (source, candidate, record) = publication_fixture(&"d".repeat(64));
            let source_generation = hex_encode(&crate::post_connect::compute_pairing_generation(
                &source.client_cert_pem,
            ));
            let candidate_generation = hex_encode(
                &crate::post_connect::compute_pairing_generation(&candidate.client_cert_pem),
            );
            crate::private_link::persist_credential(root.path(), &source)
                .expect("persist source credential");
            write_answer_file(root.path(), &source_generation).expect("write confirmation");
            set_atomic_write_fault_for_path(&root.path().join(target), Some(fault));
            assert!(
                test_runtime()
                    .block_on(publish_candidate(root.path(), &"d".repeat(64), record))
                    .is_err(),
                "{label}"
            );
            set_atomic_write_fault_for_path(&root.path().join(target), None);

            let interrupted = MigrationRecord::load(root.path())
                .expect("load interrupted transaction")
                .expect("transaction exists");
            assert_eq!(interrupted.phase, MigrationPhase::Publishing, "{label}");
            assert!(
                test_runtime()
                    .block_on(recover_publication(root.path()))
                    .expect("recover publication")
            );
            assert_eq!(
                load_credential(root.path()).expect("load candidate"),
                Some(candidate),
                "{label}"
            );
            assert_eq!(
                crate::pairing_answer::read_answer_file(root.path())
                    .expect("read recovered answer")
                    .expect("answer exists")
                    .confirmed,
                candidate_generation,
                "{label}"
            );
        }
    }

    #[test]
    fn setup_or_rejection_owner_change_prevents_stale_candidate_publication() {
        let root = TempRoot::new();
        let (_source, _candidate, record) = publication_fixture(&"e".repeat(64));
        let replacement = test_credential("fresh-setup-certificate");
        crate::private_link::persist_credential(root.path(), &replacement)
            .expect("persist fresh setup credential");
        write_answer_file(root.path(), "").expect("write awaiting answer");
        assert_eq!(
            test_runtime().block_on(publish_candidate(root.path(), &"e".repeat(64), record)),
            Err(DiagnosticCode::PrivateStateIo)
        );
        assert_eq!(
            load_credential(root.path()).expect("load replacement"),
            Some(replacement)
        );

        let confirmed_root = TempRoot::new();
        let (source, _candidate, record) = publication_fixture(&"f".repeat(64));
        let source_generation = hex_encode(&crate::post_connect::compute_pairing_generation(
            &source.client_cert_pem,
        ));
        crate::private_link::persist_credential(confirmed_root.path(), &source)
            .expect("persist source before prompt confirmation");
        write_answer_file(confirmed_root.path(), &source_generation)
            .expect("concurrent prompt confirmed source");
        crate::pairing_answer::delete_credential_file(confirmed_root.path())
            .expect("completed concurrent rejection");
        assert_eq!(
            test_runtime().block_on(publish_candidate(
                confirmed_root.path(),
                &"f".repeat(64),
                record
            )),
            Err(DiagnosticCode::PrivateStateIo)
        );
        assert_eq!(
            load_credential(confirmed_root.path()).expect("credential stays absent"),
            None
        );
        assert_eq!(
            read_answer_file(confirmed_root.path())
                .expect("confirmed source answer remains")
                .expect("answer exists")
                .confirmed,
            source_generation
        );

        let rejected_root = TempRoot::new();
        let source = test_credential("source-certificate");
        let candidate = test_credential("candidate-certificate");
        crate::private_link::persist_credential(rejected_root.path(), &source)
            .expect("persist source before rejection");
        let record = MigrationRecord {
            phase: MigrationPhase::Publishing,
            pending_marker_digest: Some("f".repeat(64)),
            source_credential: Some(source.clone()),
            candidate_credential: Some(candidate.clone()),
            ..MigrationRecord::default()
        };
        crate::pairing_answer::delete_credential_file(rejected_root.path())
            .expect("completed concurrent rejection");
        assert_eq!(
            test_runtime().block_on(restore_source_for_new_marker(
                rejected_root.path(),
                &record,
                &candidate,
            )),
            Err(DiagnosticCode::PrivateStateIo)
        );
        assert_eq!(
            load_credential(rejected_root.path()).expect("credential stays absent"),
            None
        );
    }

    #[test]
    fn operation_ids_are_stable_uuid_v4_values_for_saved_material() {
        let first = uuid_from_material(b"candidate", b"rekey");
        assert_eq!(first, uuid_from_material(b"candidate", b"rekey"));
        assert_ne!(first, uuid_from_material(b"candidate", b"decision"));
        assert_eq!(first.as_bytes()[14], b'4');
    }

    #[test]
    fn persisted_reply_and_answer_provenance_are_revalidated_before_recovery() {
        let (source, _candidate, record) = publication_fixture(&"a".repeat(64));
        assert!(verify_saved_candidate(&record, 1_800_000_000).is_ok());

        let mut wrong_operation = record.clone();
        let mut reply: Value = serde_json::from_slice(
            wrong_operation
                .rekey_reply
                .as_deref()
                .expect("saved rekey reply"),
        )
        .expect("parse saved reply");
        reply["operation_id"] = Value::String(uuid_from_material(b"other", b"rekey"));
        wrong_operation.rekey_reply = Some(serde_json::to_vec(&reply).expect("serialize reply"));
        assert!(verify_saved_candidate(&wrong_operation, 1_800_000_000).is_err());
        for (field, value) in [
            ("protocol_version", Value::from(2)),
            ("state", Value::String("new_device".to_owned())),
            (
                "previous_cid",
                Value::String(format!("sha256:{}", "f".repeat(64))),
            ),
            ("cid", Value::String(format!("sha256:{}", "e".repeat(64)))),
        ] {
            let mut invalid_reply = record.clone();
            let mut reply: Value = serde_json::from_slice(
                invalid_reply
                    .rekey_reply
                    .as_deref()
                    .expect("saved rekey reply"),
            )
            .expect("parse saved rekey reply");
            reply[field] = value;
            invalid_reply.rekey_reply =
                Some(serde_json::to_vec(&reply).expect("serialize invalid rekey reply"));
            assert!(
                verify_saved_candidate(&invalid_reply, 1_800_000_000).is_err(),
                "{field}"
            );
        }

        let candidate = verify_saved_candidate(&record, 1_800_000_000).expect("candidate proof");
        let mut state_proof = record.clone();
        state_proof.decision_reply = None;
        state_proof.decision_state_reply = Some(
            serde_json::to_vec(&serde_json::json!({
                "protocol_version": 1,
                "rekey_operation_id": record.rekey_operation_id,
                "previous_cid": record.source_cid,
                "state": "new_device",
                "replaced_cid": null
            }))
            .expect("serialize GET proof"),
        );
        assert!(verify_decision_proof(&state_proof, &candidate).is_ok());
        let mut bad_state = state_proof.clone();
        let mut reply: Value = serde_json::from_slice(
            bad_state
                .decision_state_reply
                .as_deref()
                .expect("saved GET proof"),
        )
        .expect("parse GET proof");
        reply["replaced_cid"] = Value::String("sha256:replaced".to_owned());
        bad_state.decision_state_reply =
            Some(serde_json::to_vec(&reply).expect("serialize wrong proof"));
        assert!(verify_decision_proof(&bad_state, &candidate).is_err());
        let mut pending_state = state_proof.clone();
        let mut reply: Value = serde_json::from_slice(
            pending_state
                .decision_state_reply
                .as_deref()
                .expect("saved GET proof"),
        )
        .expect("parse GET proof");
        reply["state"] = Value::String("pending".to_owned());
        pending_state.decision_state_reply =
            Some(serde_json::to_vec(&reply).expect("serialize pending state"));
        assert!(verify_decision_proof(&pending_state, &candidate).is_err());
        for (field, value) in [
            ("protocol_version", Value::from(2)),
            (
                "rekey_operation_id",
                Value::String(uuid_from_material(b"other", b"rekey")),
            ),
            (
                "previous_cid",
                Value::String(format!("sha256:{}", "f".repeat(64))),
            ),
        ] {
            let mut invalid_state = state_proof.clone();
            let mut reply: Value = serde_json::from_slice(
                invalid_state
                    .decision_state_reply
                    .as_deref()
                    .expect("saved GET proof"),
            )
            .expect("parse GET proof");
            reply[field] = value;
            invalid_state.decision_state_reply =
                Some(serde_json::to_vec(&reply).expect("serialize invalid GET proof"));
            assert!(
                verify_decision_proof(&invalid_state, &candidate).is_err(),
                "{field}"
            );
        }

        let mut wrong_decision_cid = record.clone();
        let mut reply: Value = serde_json::from_slice(
            wrong_decision_cid
                .decision_reply
                .as_deref()
                .expect("saved decision reply"),
        )
        .expect("parse decision reply");
        reply["cid"] = Value::String("sha256:wrong-candidate".to_owned());
        wrong_decision_cid.decision_reply =
            Some(serde_json::to_vec(&reply).expect("serialize wrong decision CID"));
        assert!(verify_decision_proof(&wrong_decision_cid, &candidate).is_err());
        for (field, value) in [
            ("protocol_version", Value::from(2)),
            (
                "operation_id",
                Value::String(uuid_from_material(b"other", b"decision")),
            ),
            ("state", Value::String("pending".to_owned())),
            (
                "previous_cid",
                Value::String(format!("sha256:{}", "f".repeat(64))),
            ),
            (
                "replaced_cid",
                Value::String(format!("sha256:{}", "d".repeat(64))),
            ),
        ] {
            let mut invalid_decision = record.clone();
            let mut reply: Value = serde_json::from_slice(
                invalid_decision
                    .decision_reply
                    .as_deref()
                    .expect("saved decision reply"),
            )
            .expect("parse decision reply");
            reply[field] = value;
            invalid_decision.decision_reply =
                Some(serde_json::to_vec(&reply).expect("serialize invalid decision"));
            assert!(
                verify_decision_proof(&invalid_decision, &candidate).is_err(),
                "{field}"
            );
        }

        let source_generation = record.source_generation.clone().expect("source binding");
        let candidate_generation = hex_encode(&crate::post_connect::compute_pairing_generation(
            &candidate.client_cert_pem,
        ));
        let mut publishing = record;
        publishing.phase = MigrationPhase::Publishing;
        publishing.source_answer_bytes = Some(
            serde_json::to_vec(&AnswerRecord {
                confirmed: source_generation.clone(),
            })
            .expect("serialize source confirmation"),
        );
        publishing.target_answer_bytes = Some(
            serde_json::to_vec(&AnswerRecord {
                confirmed: candidate_generation,
            })
            .expect("serialize candidate confirmation"),
        );
        assert_eq!(
            verify_publishing_record(&publishing).expect("valid publication proof"),
            candidate
        );
        let mut wrong_source = publishing.clone();
        wrong_source.source_answer_bytes = Some(
            serde_json::to_vec(&AnswerRecord {
                confirmed: String::new(),
            })
            .expect("serialize wrong source binding"),
        );
        assert!(verify_publishing_record(&wrong_source).is_err());
        let mut wrong_target = publishing;
        wrong_target.target_answer_bytes = Some(
            serde_json::to_vec(&AnswerRecord {
                confirmed: source_generation,
            })
            .expect("serialize wrong candidate binding"),
        );
        assert!(verify_publishing_record(&wrong_target).is_err());
        assert_ne!(source.client_cert_pem, candidate.client_cert_pem);
    }

    #[test]
    fn verified_pairing_requires_current_key_certificate_fingerprint_ca_and_instance() {
        use rcgen::{BasicConstraints, IsCa, KeyUsagePurpose};

        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("CA key");
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        let ca = ca_params.self_signed(&ca_key).expect("self-sign CA");
        let old_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("old key");
        let old_cert = CertificateParams::new(Vec::<String>::new())
            .expect("old cert params")
            .signed_by(&old_key, &ca, &ca_key)
            .expect("sign old cert");
        let candidate_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("candidate key");
        let candidate_cert = CertificateParams::new(Vec::<String>::new())
            .expect("candidate cert params")
            .signed_by(&candidate_key, &ca, &ca_key)
            .expect("sign candidate cert");
        let cid = format!("sha256:{}", spl_core::ca::sha256_hex(candidate_cert.der()));
        let ca_digest = Sha256::digest(ca.der().as_ref());
        let source = Credential {
            ca_fp_prefix: ca_digest[..16].to_vec(),
            ..test_credential(&old_cert.pem())
        };
        let pairing = serde_json::json!({
            "client_cert": candidate_cert.pem(),
            "ca_chain": [ca.pem()],
            "instance_id": source.instance_id,
            "home_label": "fixture home",
            "fingerprint": cid,
            "home_attestation": null
        });
        let key_pem = candidate_key.serialize_pem();
        assert!(candidate_credential(&pairing, &key_pem, &cid, &source, 1_800_000_000).is_ok());

        let wrong_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("wrong key");
        assert!(
            candidate_credential(
                &pairing,
                &wrong_key.serialize_pem(),
                &cid,
                &source,
                1_800_000_000
            )
            .is_err()
        );
        assert!(
            candidate_credential(
                &pairing,
                &key_pem,
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                &source,
                1_800_000_000
            )
            .is_err()
        );

        let mut wrong_fingerprint = pairing.clone();
        wrong_fingerprint["fingerprint"] = Value::String(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        );
        assert!(
            candidate_credential(&wrong_fingerprint, &key_pem, &cid, &source, 1_800_000_000)
                .is_err()
        );

        let other_ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("other CA key");
        let other_ca = CertificateParams::new(Vec::<String>::new())
            .expect("other CA params")
            .self_signed(&other_ca_key)
            .expect("self-sign other CA");
        let mut wrong_ca = pairing.clone();
        wrong_ca["ca_chain"] = serde_json::json!([other_ca.pem()]);
        assert!(candidate_credential(&wrong_ca, &key_pem, &cid, &source, 1_800_000_000).is_err());

        let mut wrong_instance = pairing;
        wrong_instance["instance_id"] = Value::String("different-journal".to_owned());
        assert!(
            candidate_credential(&wrong_instance, &key_pem, &cid, &source, 1_800_000_000).is_err()
        );
    }
}
