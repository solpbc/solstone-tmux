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
/// How long one answer from the journal keeps "connected": a periodic sweep, its status
/// check's timeout, and a second of slack. A failed check alone does not drop
/// connected, but the lease can expire before the next retry answers.
pub const CONNECTED_LEASE_SECONDS: i64 = crate::sync::PERIODIC_SYNC_INTERVAL.as_secs() as i64
    + crate::journal::SYSTEM_STATUS_TIMEOUT.as_secs() as i64
    + 1;
/// How long the device tries to reach the journal before it says offline instead of
/// connecting.
pub const OFFLINE_AFTER_SECONDS: i64 = 30;

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
    JournalResponseTooLarge,
    JournalContractInvalid,
    ConfiguredStreamMismatch,
    LocalSegmentInvalid,
    RequestTooLarge,
    JournalRejected,
    JournalRevoked,
    SyncTaskExited,
    SyncTaskPanicked,
    SyncTaskCancelled,
    SyncTaskTimedOut,
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
            Self::JournalResponseTooLarge => "journal_response_too_large",
            Self::JournalContractInvalid => "journal_contract_invalid",
            Self::ConfiguredStreamMismatch => "configured_stream_mismatch",
            Self::LocalSegmentInvalid => "local_segment_invalid",
            Self::RequestTooLarge => "request_too_large",
            Self::JournalRejected => "journal_rejected",
            Self::JournalRevoked => "journal_revoked",
            Self::SyncTaskExited => "sync_task_exited",
            Self::SyncTaskPanicked => "sync_task_panicked",
            Self::SyncTaskCancelled => "sync_task_cancelled",
            Self::SyncTaskTimedOut => "sync_task_timed_out",
            Self::IndicatorUpdateFailed => "indicator_update_failed",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::SetupInputInvalid => "setup input is invalid",
            Self::SetupUnavailable => "setup is unavailable; stop solstone-tmux and try again",
            Self::PairingFailed => {
                "private network pairing failed; check the pairing link and try setup again"
            }
            Self::PrivateStateInvalid => "private network state is invalid",
            Self::PrivateStateIo => "private network state could not be accessed",
            Self::HealthSnapshotIo => "sync health snapshot could not be written",
            Self::BridgeUnavailable => "the private network is unavailable",
            Self::JournalUnavailable => "paired journal is unavailable",
            Self::JournalTimeout => "paired journal request timed out",
            Self::JournalResponseTooLarge => "paired journal response exceeds the supported size",
            Self::JournalContractInvalid => "journal response did not match the contract",
            Self::ConfiguredStreamMismatch => {
                "set stream to the hostname-derived tmux name and restart"
            }
            Self::LocalSegmentInvalid => "local segment is invalid",
            Self::RequestTooLarge => "local request exceeds the bridge limit",
            Self::JournalRejected => "journal request was rejected",
            Self::JournalRevoked => {
                "this device can't connect to your journal with its saved pairing; run setup with a new pairing link"
            }
            Self::SyncTaskExited => "sync task exited unexpectedly",
            Self::SyncTaskPanicked => "sync task failed: panic",
            Self::SyncTaskCancelled => "sync task failed: task was cancelled",
            Self::SyncTaskTimedOut => "sync task did not stop within the shutdown deadline",
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
    crate::stderr_log::line(code.message());
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Unpaired,
    Held,
    Connected,
    Syncing,
    Connecting,
    Offline,
    UpdateNeeded,
    Revoked,
}

impl HealthState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unpaired => "unpaired",
            Self::Held => "held",
            Self::Connected => "connected",
            Self::Syncing => "syncing",
            Self::Connecting => "connecting",
            Self::Offline => "offline",
            Self::UpdateNeeded => "update_needed",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncFacts {
    pub paired: bool,
    pub held: bool,
    pub sync_in_progress: bool,
    pub pending_segments: u64,
    pub last_successful_contact_unix_seconds: Option<i64>,
    pub last_successful_sync_unix_seconds: Option<i64>,
    pub recent_error_count: u32,
    pub last_error_code: Option<DiagnosticCode>,
    /// When the current attempt to reach the journal began: start, or a wake. Offline
    /// counts from the later of this and the journal's last answer.
    pub episode_started_unix_seconds: Option<i64>,
    /// Segments kept here because the journal turned each one away with an answer. A refusal is the
    /// journal answering, so these never decide the connection state.
    pub held_segments: u64,
    /// The newest held segment and the journal's answer, as `day/segment status:reason`.
    pub held_newest: Option<String>,
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

    /// The one rule for what the sync health says. Connected and syncing need an answer
    /// from the journal within the lease. Inside it, a timed-out request changes nothing;
    /// any other failure shows at once.
    pub fn state_at(&self, now_unix_seconds: i64) -> HealthState {
        let heard = self
            .last_successful_contact_unix_seconds
            .is_some_and(|contact| within_lease(contact, now_unix_seconds));
        let link_up = self.paired
            && heard
            && matches!(
                self.last_error_code,
                None | Some(DiagnosticCode::JournalTimeout)
            );
        if link_up && self.sync_in_progress {
            return HealthState::Syncing;
        }
        if matches!(
            self.last_error_code,
            Some(
                DiagnosticCode::JournalContractInvalid
                    | DiagnosticCode::ConfiguredStreamMismatch
                    | DiagnosticCode::PrivateStateInvalid
                    | DiagnosticCode::PrivateStateIo
                    | DiagnosticCode::HealthSnapshotIo
            )
        ) {
            return HealthState::UpdateNeeded;
        }
        if self.held {
            return HealthState::Held;
        }
        if !self.paired {
            return HealthState::Unpaired;
        }
        if self.last_error_code == Some(DiagnosticCode::JournalRevoked) {
            return HealthState::Revoked;
        }
        if link_up {
            return HealthState::Connected;
        }
        let unreached_since = self
            .last_successful_contact_unix_seconds
            .max(self.episode_started_unix_seconds);
        match unreached_since {
            Some(since) if now_unix_seconds.saturating_sub(since) < OFFLINE_AFTER_SECONDS => {
                HealthState::Connecting
            }
            _ => HealthState::Offline,
        }
    }
}

fn within_lease(contact: i64, now_unix_seconds: i64) -> bool {
    now_unix_seconds.saturating_sub(contact) <= CONNECTED_LEASE_SECONDS
}

/// A snapshot is read after it was written. What it says still has to hold now: connected
/// and syncing end with the lease, and connecting turns offline once it has lasted.
fn state_now(snapshot: &HealthSnapshot, now_unix_seconds: i64) -> HealthState {
    match snapshot.state {
        HealthState::Connected | HealthState::Syncing
            if !snapshot
                .last_successful_contact_unix_seconds
                .is_some_and(|contact| within_lease(contact, now_unix_seconds)) =>
        {
            HealthState::Offline
        }
        HealthState::Connecting
            if now_unix_seconds.saturating_sub(snapshot.updated_at_unix_seconds)
                >= OFFLINE_AFTER_SECONDS =>
        {
            HealthState::Offline
        }
        state => state,
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
    /// The version of the running process. A snapshot without it was written by a
    /// release older than the one that added it.
    #[serde(default)]
    app_version: Option<String>,
    #[serde(default)]
    held_segments: u64,
    #[serde(default)]
    held_newest: Option<String>,
}

#[derive(Clone)]
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
            state: facts.state_at(now_unix_seconds),
            paired: facts.paired,
            sync_in_progress: facts.sync_in_progress,
            pending_segments: facts.pending_segments,
            last_successful_contact_unix_seconds: facts.last_successful_contact_unix_seconds,
            last_successful_sync_unix_seconds: facts.last_successful_sync_unix_seconds,
            recent_error_count: facts.recent_error_count,
            last_error_code: facts.last_error_code,
            app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            held_segments: facts.held_segments,
            held_newest: facts.held_newest.clone(),
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
    read_status_report(data_root, now_unix_seconds).health
}

/// What the running process says about itself, beyond its health state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunningReport {
    /// The running version, or `None` when it predates reporting one.
    pub app_version: Option<String>,
    pub held_segments: u64,
    pub held_newest: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusReport {
    pub health: StatusHealth,
    /// Present only when the health is live: a stale snapshot says nothing about
    /// what is running now.
    pub running: Option<RunningReport>,
}

pub fn read_status_report(data_root: &Path, now_unix_seconds: i64) -> StatusReport {
    let only = |health| StatusReport {
        health,
        running: None,
    };
    match fs::symlink_metadata(data_root) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
        _ => return only(StatusHealth::Unknown),
    }
    let snapshot = match read_snapshot(data_root) {
        Some(snapshot) => snapshot,
        None => return only(StatusHealth::Unknown),
    };
    if !valid_snapshot(&snapshot) {
        return only(StatusHealth::Unknown);
    }
    let identity = match inspect_existing(data_root) {
        ExistingLock::Locked(identity) => identity,
        ExistingLock::Unlocked(_) => return only(StatusHealth::Stale),
        ExistingLock::MissingOrInvalid => return only(StatusHealth::Unknown),
    };
    if snapshot.run_id != identity.run_id
        || snapshot.lock_inode != identity.lock_inode
        || snapshot.updated_at_unix_seconds > now_unix_seconds + HEALTH_FUTURE_TOLERANCE_SECONDS
        || now_unix_seconds.saturating_sub(snapshot.updated_at_unix_seconds) > HEALTH_STALE_SECONDS
    {
        return only(StatusHealth::Stale);
    }
    StatusReport {
        health: StatusHealth::Live(state_now(&snapshot, now_unix_seconds)),
        running: Some(RunningReport {
            app_version: snapshot.app_version,
            held_segments: snapshot.held_segments,
            held_newest: snapshot.held_newest,
        }),
    }
}

fn read_snapshot(data_root: &Path) -> Option<HealthSnapshot> {
    let mut file = open_regular_readonly(&data_root.join(HEALTH_FILENAME)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn valid_snapshot(snapshot: &HealthSnapshot) -> bool {
    let short_text = |text: &Option<String>| {
        text.as_ref().is_none_or(|text| {
            !text.is_empty()
                && text.len() <= 160
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        })
    };
    if !short_text(&snapshot.app_version) || !short_text(&snapshot.held_newest) {
        return false;
    }
    if snapshot.schema_version != HEALTH_SCHEMA_VERSION
        || snapshot.run_id.len() != 32
        || !snapshot
            .run_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    // A retry sweep can be sending while the link reads down, so only the up states and
    // the ones that never send pin the sending flag.
    match snapshot.state {
        HealthState::Unpaired => !snapshot.paired && !snapshot.sync_in_progress,
        HealthState::Held => snapshot.paired && !snapshot.sync_in_progress,
        HealthState::Syncing => {
            snapshot.paired
                && snapshot.sync_in_progress
                && snapshot.last_successful_contact_unix_seconds.is_some()
        }
        HealthState::Connected => {
            snapshot.paired
                && !snapshot.sync_in_progress
                && snapshot.last_successful_contact_unix_seconds.is_some()
                && matches!(
                    snapshot.last_error_code,
                    None | Some(DiagnosticCode::JournalTimeout)
                )
        }
        HealthState::UpdateNeeded => true,
        HealthState::Connecting | HealthState::Offline | HealthState::Revoked => snapshot.paired,
    }
}
