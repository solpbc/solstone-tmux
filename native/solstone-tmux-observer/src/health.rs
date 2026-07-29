// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::instance_lock::{ExistingLock, InstanceLock, RunIdentity, inspect_existing};
use crate::storage::{atomic_write_bytes, open_regular_readonly};

pub const HEALTH_FILENAME: &str = "sync-health.json";
const HEALTH_SCHEMA_VERSION: u32 = 1;
const HEALTH_STALE_SECONDS: i64 = 180;
const HEALTH_FUTURE_TOLERANCE_SECONDS: i64 = 60;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    SetupInputInvalid,
    SetupUnavailable,
    PairingFailed,
    PrivateStateInvalid,
    PrivateStateIo,
    HealthSnapshotIo,
    BridgeUnavailable,
    JournalUnavailable,
    JournalTimeout,
    RegistrationFailed,
    JournalContractInvalid,
    LocalSegmentInvalid,
    RequestTooLarge,
    JournalRejected,
    SyncTaskExited,
    SyncTaskPanicked,
    SyncTaskCancelled,
    IndicatorUpdateFailed,
}

impl DiagnosticCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SetupInputInvalid => "setup_input_invalid",
            Self::SetupUnavailable => "setup_unavailable",
            Self::PairingFailed => "pairing_failed",
            Self::PrivateStateInvalid => "private_state_invalid",
            Self::PrivateStateIo => "private_state_io",
            Self::HealthSnapshotIo => "health_snapshot_io",
            Self::BridgeUnavailable => "bridge_unavailable",
            Self::JournalUnavailable => "journal_unavailable",
            Self::JournalTimeout => "journal_timeout",
            Self::RegistrationFailed => "registration_failed",
            Self::JournalContractInvalid => "journal_contract_invalid",
            Self::LocalSegmentInvalid => "local_segment_invalid",
            Self::RequestTooLarge => "request_too_large",
            Self::JournalRejected => "journal_rejected",
            Self::SyncTaskExited => "sync_task_exited",
            Self::SyncTaskPanicked => "sync_task_panicked",
            Self::SyncTaskCancelled => "sync_task_cancelled",
            Self::IndicatorUpdateFailed => "indicator_update_failed",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::SetupInputInvalid => "setup input is invalid",
            Self::SetupUnavailable => "setup is unavailable; stop the observer and retry",
            Self::PairingFailed => "private-link pairing failed; verify the link and retry setup",
            Self::PrivateStateInvalid => "private-link state is invalid",
            Self::PrivateStateIo => "private-link state could not be accessed",
            Self::HealthSnapshotIo => "sync health snapshot could not be written",
            Self::BridgeUnavailable => "private-link bridge is unavailable",
            Self::JournalUnavailable => "paired journal is unavailable",
            Self::JournalTimeout => "paired journal request timed out",
            Self::RegistrationFailed => "journal registration failed",
            Self::JournalContractInvalid => "journal response did not match the contract",
            Self::LocalSegmentInvalid => "local segment is invalid",
            Self::RequestTooLarge => "local request exceeds the bridge limit",
            Self::JournalRejected => "journal request was rejected",
            Self::SyncTaskExited => "sync task exited unexpectedly",
            Self::SyncTaskPanicked => "sync task failed: panic",
            Self::SyncTaskCancelled => "sync task failed: task was cancelled",
            Self::IndicatorUpdateFailed => "indicator update failed",
        }
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for DiagnosticCode {}

pub fn emit_diagnostic(code: DiagnosticCode) {
    eprintln!("solstone-tmux-observer: {}", code.message());
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Unpaired,
    Connected,
    Syncing,
    Offline,
    UpdateNeeded,
    Revoked,
}

impl HealthState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unpaired => "unpaired",
            Self::Connected => "connected",
            Self::Syncing => "syncing",
            Self::Offline => "offline",
            Self::UpdateNeeded => "update_needed",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncFacts {
    pub paired: bool,
    pub sync_in_progress: bool,
    pub pending_segments: u64,
    pub last_successful_contact_unix_seconds: Option<i64>,
    pub last_successful_sync_unix_seconds: Option<i64>,
    pub recent_error_count: u32,
    pub last_error_code: Option<DiagnosticCode>,
}

impl SyncFacts {
    pub fn successful_contact(&mut self, now_unix_seconds: i64) {
        self.last_successful_contact_unix_seconds = Some(now_unix_seconds);
        self.recent_error_count = 0;
        self.last_error_code = None;
    }

    pub fn successful_sync(&mut self, now_unix_seconds: i64) {
        self.successful_contact(now_unix_seconds);
        self.last_successful_sync_unix_seconds = Some(now_unix_seconds);
    }

    pub fn failed(&mut self, code: DiagnosticCode) {
        self.recent_error_count = self.recent_error_count.saturating_add(1);
        self.last_error_code = Some(code);
    }

    pub fn state(&self) -> HealthState {
        if self.paired && self.sync_in_progress {
            return HealthState::Syncing;
        }
        if matches!(
            self.last_error_code,
            Some(
                DiagnosticCode::JournalContractInvalid
                    | DiagnosticCode::PrivateStateInvalid
                    | DiagnosticCode::PrivateStateIo
                    | DiagnosticCode::HealthSnapshotIo
            )
        ) {
            return HealthState::UpdateNeeded;
        }
        if !self.paired {
            return HealthState::Unpaired;
        }
        match self.last_error_code {
            Some(DiagnosticCode::JournalRejected) => HealthState::Revoked,
            Some(_) => HealthState::Offline,
            None if self.last_successful_contact_unix_seconds.is_some() => HealthState::Connected,
            None => HealthState::Offline,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HealthSnapshot {
    schema_version: u32,
    run_id: String,
    lock_inode: u64,
    updated_at_unix_seconds: i64,
    state: HealthState,
    paired: bool,
    sync_in_progress: bool,
    pending_segments: u64,
    last_successful_contact_unix_seconds: Option<i64>,
    last_successful_sync_unix_seconds: Option<i64>,
    recent_error_count: u32,
    last_error_code: Option<DiagnosticCode>,
}

pub struct HealthWriter {
    data_root: PathBuf,
    identity: RunIdentity,
}

impl HealthWriter {
    pub fn new(data_root: PathBuf, instance_lock: &InstanceLock) -> Self {
        Self {
            data_root,
            identity: instance_lock.identity().clone(),
        }
    }

    pub async fn write(
        &self,
        facts: &SyncFacts,
        now_unix_seconds: i64,
    ) -> Result<(), DiagnosticCode> {
        let snapshot = HealthSnapshot {
            schema_version: HEALTH_SCHEMA_VERSION,
            run_id: self.identity.run_id.clone(),
            lock_inode: self.identity.lock_inode,
            updated_at_unix_seconds: now_unix_seconds,
            state: facts.state(),
            paired: facts.paired,
            sync_in_progress: facts.sync_in_progress,
            pending_segments: facts.pending_segments,
            last_successful_contact_unix_seconds: facts.last_successful_contact_unix_seconds,
            last_successful_sync_unix_seconds: facts.last_successful_sync_unix_seconds,
            recent_error_count: facts.recent_error_count,
            last_error_code: facts.last_error_code,
        };
        let bytes = serde_json::to_vec(&snapshot).map_err(|_| DiagnosticCode::HealthSnapshotIo)?;
        let parent = self.data_root.clone();
        let path = parent.join(HEALTH_FILENAME);
        tokio::task::spawn_blocking(move || atomic_write_bytes(&path, &parent, &bytes))
            .await
            .map_err(|_| DiagnosticCode::HealthSnapshotIo)?
            .map_err(|_| DiagnosticCode::HealthSnapshotIo)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusHealth {
    Unknown,
    Stale,
    Live(HealthState),
}

impl StatusHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Stale => "stale",
            Self::Live(state) => state.as_str(),
        }
    }
}

pub fn read_status_health(data_root: &Path, now_unix_seconds: i64) -> StatusHealth {
    match fs::symlink_metadata(data_root) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
        _ => return StatusHealth::Unknown,
    }
    let snapshot = match read_snapshot(data_root) {
        Some(snapshot) => snapshot,
        None => return StatusHealth::Unknown,
    };
    if !valid_snapshot(&snapshot) {
        return StatusHealth::Unknown;
    }
    let identity = match inspect_existing(data_root) {
        ExistingLock::Locked(identity) => identity,
        ExistingLock::Unlocked(_) => return StatusHealth::Stale,
        ExistingLock::MissingOrInvalid => return StatusHealth::Unknown,
    };
    if snapshot.run_id != identity.run_id
        || snapshot.lock_inode != identity.lock_inode
        || snapshot.updated_at_unix_seconds > now_unix_seconds + HEALTH_FUTURE_TOLERANCE_SECONDS
        || now_unix_seconds.saturating_sub(snapshot.updated_at_unix_seconds) > HEALTH_STALE_SECONDS
    {
        return StatusHealth::Stale;
    }
    StatusHealth::Live(snapshot.state)
}

fn read_snapshot(data_root: &Path) -> Option<HealthSnapshot> {
    let mut file = open_regular_readonly(&data_root.join(HEALTH_FILENAME)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn valid_snapshot(snapshot: &HealthSnapshot) -> bool {
    if snapshot.schema_version != HEALTH_SCHEMA_VERSION
        || snapshot.run_id.len() != 32
        || !snapshot
            .run_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    match snapshot.state {
        HealthState::Unpaired => !snapshot.paired && !snapshot.sync_in_progress,
        HealthState::Syncing => snapshot.paired && snapshot.sync_in_progress,
        HealthState::Connected => {
            snapshot.paired
                && !snapshot.sync_in_progress
                && snapshot.last_successful_contact_unix_seconds.is_some()
                && snapshot.recent_error_count == 0
                && snapshot.last_error_code.is_none()
        }
        HealthState::UpdateNeeded => !snapshot.sync_in_progress,
        HealthState::Offline | HealthState::Revoked => {
            snapshot.paired && !snapshot.sync_in_progress
        }
    }
}
