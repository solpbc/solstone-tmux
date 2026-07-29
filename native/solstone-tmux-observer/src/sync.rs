// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spl_transport::client::TokenPersistHook;
use spl_transport::credential::Credential;
use time::{Date, Month};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::clock::Clock;
use crate::config::RuntimeConfig;
use crate::health::{DiagnosticCode, HealthWriter, SyncFacts};
use crate::journal::{
    JournalClient, JournalError, JournalReasonCode, ListingFileStatus, LocalFile,
    RegistrationDescriptor, SegmentsEnvelope, UploadResult, UploadStatus, inventory_files,
};
use crate::name::{DerivedName, derive_component};
use crate::paths::PlatformKind;
use crate::private_link::{
    ObserverState, PrivateLinkBridge, load_credential, load_observer, persist_credential,
    persist_observer,
};
use crate::segment::SegmentClose;
use crate::storage::{
    atomic_write_bytes, open_directory_readonly, open_regular_readonly, open_regular_readonly_at,
    sync_directory,
};

const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(300),
];
const PERIODIC_SYNC_INTERVAL: Duration = Duration::from_secs(60);
pub const SEGMENTS_PER_PASS: usize = 8;
const STATUS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncActivity {
    Idle,
    Working,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusBeacon {
    pub name: String,
    pub uptime: u64,
    pub last_successful_sync: Option<i64>,
    pub pending_queue_depth: u64,
    pub recent_error_count: u32,
    pub last_error_reason: Option<DiagnosticCode>,
}

impl StatusBeacon {
    pub fn fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = serde_json::Map::new();
        fields.insert("name".to_owned(), self.name.clone().into());
        fields.insert("stream_type".to_owned(), "tmux".into());
        fields.insert("version".to_owned(), env!("CARGO_PKG_VERSION").into());
        fields.insert("uptime".to_owned(), self.uptime.into());
        fields.insert(
            "last_successful_sync".to_owned(),
            self.last_successful_sync
                .map_or(serde_json::Value::Null, Into::into),
        );
        fields.insert(
            "pending_queue_depth".to_owned(),
            self.pending_queue_depth.into(),
        );
        fields.insert(
            "recent_error_count".to_owned(),
            u64::from(self.recent_error_count).into(),
        );
        fields.insert(
            "last_error_reason".to_owned(),
            self.last_error_reason
                .map_or(serde_json::Value::Null, |code| code.as_str().into()),
        );
        fields
    }
}

#[derive(Clone, Eq, PartialEq)]
struct TokenUpdate {
    token: String,
    expires_at: i64,
}

struct TokenPersistence {
    config_root: PathBuf,
    credential: Arc<Mutex<Credential>>,
    pending: Arc<Mutex<Option<TokenUpdate>>>,
}

impl TokenPersistence {
    fn new(config_root: PathBuf, credential: Credential) -> (Self, TokenPersistHook) {
        let pending = Arc::new(Mutex::new(None));
        let hook_pending = Arc::clone(&pending);
        let hook: TokenPersistHook = Arc::new(move |token, expires_at| {
            let mut pending = match hook_pending.lock() {
                Ok(pending) => pending,
                Err(poisoned) => poisoned.into_inner(),
            };
            *pending = Some(TokenUpdate {
                token: token.to_owned(),
                expires_at,
            });
        });
        (
            Self {
                config_root,
                credential: Arc::new(Mutex::new(credential)),
                pending,
            },
            hook,
        )
    }

    async fn persist_pending(&self) -> Result<(), DiagnosticCode> {
        let update = {
            let pending = match self.pending.lock() {
                Ok(pending) => pending,
                Err(poisoned) => poisoned.into_inner(),
            };
            pending.clone()
        };
        let Some(update) = update else {
            return Ok(());
        };
        let mut updated_credential = {
            let credential = match self.credential.lock() {
                Ok(credential) => credential,
                Err(poisoned) => poisoned.into_inner(),
            };
            credential.clone()
        };
        updated_credential.device_token = Some(update.token.clone());
        updated_credential.device_token_expires_at = Some(update.expires_at);
        let config_root = self.config_root.clone();
        let persisted = updated_credential.clone();
        tokio::task::spawn_blocking(move || persist_credential(&config_root, &persisted))
            .await
            .map_err(|_| DiagnosticCode::PrivateStateIo)??;
        {
            let mut credential = match self.credential.lock() {
                Ok(credential) => credential,
                Err(poisoned) => poisoned.into_inner(),
            };
            *credential = updated_credential;
        }
        let mut pending = match self.pending.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        if pending.as_ref() == Some(&update) {
            *pending = None;
        }
        Ok(())
    }
}

pub struct RegistrationOwner {
    bridge: PrivateLinkBridge,
    journal: JournalClient,
    config_root: PathBuf,
    credential_instance_id: String,
    token_persistence: TokenPersistence,
}

impl RegistrationOwner {
    pub async fn start(
        credential: Credential,
        config_root: PathBuf,
    ) -> Result<Self, DiagnosticCode> {
        let credential_instance_id = credential.instance_id.clone();
        let (token_persistence, hook) =
            TokenPersistence::new(config_root.clone(), credential.clone());
        let bridge = PrivateLinkBridge::start(credential, Some(hook)).await?;
        let journal = match JournalClient::bootstrap(&bridge).await {
            Ok(journal) => journal,
            Err(code) => {
                bridge.shutdown().await;
                return Err(code);
            }
        };
        Ok(Self {
            bridge,
            journal,
            config_root,
            credential_instance_id,
            token_persistence,
        })
    }

    pub fn journal(&self) -> &JournalClient {
        &self.journal
    }

    pub async fn ensure_registration(
        &self,
        descriptor: &RegistrationDescriptor,
    ) -> Result<(ObserverState, bool), JournalError> {
        self.token_persistence.persist_pending().await?;
        let config_root = self.config_root.clone();
        let instance_id = self.credential_instance_id.clone();
        let existing =
            tokio::task::spawn_blocking(move || load_observer(&config_root, &instance_id))
                .await
                .map_err(|_| DiagnosticCode::PrivateStateIo)??;
        if let Some(observer) = existing {
            self.journal.validate_observer(&observer)?;
            self.bridge.opener().set_registered(&observer)?;
            return Ok((observer, false));
        }

        let observer = self
            .journal
            .register(descriptor, &self.credential_instance_id)
            .await?;
        let config_root = self.config_root.clone();
        let persisted = observer.clone();
        tokio::task::spawn_blocking(move || persist_observer(&config_root, &persisted))
            .await
            .map_err(|_| DiagnosticCode::PrivateStateIo)??;
        self.bridge.opener().set_registered(&observer)?;
        Ok((observer, true))
    }

    pub async fn shutdown(self) -> Result<(), DiagnosticCode> {
        let persist_result = self.token_persistence.persist_pending().await;
        self.bridge.shutdown().await;
        persist_result
    }
}

pub fn fresh_listing_proves_custody(
    listing: &SegmentsEnvelope,
    submitted_key: &str,
    authoritative_key: &str,
    local_files: &[LocalFile],
) -> bool {
    if submitted_key.is_empty() || authoritative_key.is_empty() || local_files.is_empty() {
        return false;
    }
    let mut entries = listing.items.iter().filter(|entry| {
        entry.key == authoritative_key || entry.original_key.as_deref() == Some(submitted_key)
    });
    let Some(entry) = entries.next() else {
        return false;
    };
    if entries.next().is_some() {
        return false;
    }

    let mut remote_by_submitted_name = HashMap::new();
    for remote in &entry.files {
        let submitted_name = remote
            .submitted_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&remote.name);
        if submitted_name.is_empty()
            || remote_by_submitted_name
                .insert(submitted_name, remote)
                .is_some()
        {
            return false;
        }
    }

    let mut local_names = HashSet::new();
    local_files.iter().all(|local| {
        if local.name.is_empty()
            || !local_names.insert(local.name.as_str())
            || !valid_sha256(&local.sha256)
        {
            return false;
        }
        let Some(remote) = remote_by_submitted_name.get(local.name.as_str()) else {
            return false;
        };
        valid_sha256(&remote.sha256)
            && remote.sha256 == local.sha256
            && matches!(
                remote.status,
                ListingFileStatus::Present | ListingFileStatus::Processed
            )
    })
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct SegmentCandidate {
    day: String,
    stream: String,
    segment: String,
}

impl SegmentCandidate {
    pub fn new(
        day: impl Into<String>,
        stream: impl Into<String>,
        segment: impl Into<String>,
    ) -> Self {
        Self {
            day: day.into(),
            stream: stream.into(),
            segment: segment.into(),
        }
    }

    pub fn day(&self) -> &str {
        &self.day
    }

    pub fn stream(&self) -> &str {
        &self.stream
    }

    pub fn segment(&self) -> &str {
        &self.segment
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionOutcome {
    Disabled,
    Ineligible,
    Retained,
    Deleted,
}

#[derive(Clone, Eq, PartialEq)]
struct FileIdentity {
    name: String,
    device: u64,
    inode: u64,
    size: u64,
}

pub async fn delete_custodied_segment(
    captures_root: &Path,
    configured_stream: &DerivedName,
    today: Date,
    retention_days: i64,
    candidate: &SegmentCandidate,
    authoritative_key: &str,
    listing: &SegmentsEnvelope,
) -> RetentionOutcome {
    delete_custodied_segment_with_hook_inner(
        captures_root,
        configured_stream,
        today,
        retention_days,
        candidate,
        (authoritative_key, listing),
        None,
    )
    .await
}

#[doc(hidden)]
pub async fn delete_custodied_segment_with_hook(
    captures_root: &Path,
    configured_stream: &DerivedName,
    today: Date,
    retention_days: i64,
    candidate: &SegmentCandidate,
    custody: (&str, &SegmentsEnvelope),
    delete_hook: Arc<dyn Fn(usize) + Send + Sync>,
) -> RetentionOutcome {
    delete_custodied_segment_with_hook_inner(
        captures_root,
        configured_stream,
        today,
        retention_days,
        candidate,
        custody,
        Some(delete_hook),
    )
    .await
}

async fn delete_custodied_segment_with_hook_inner(
    captures_root: &Path,
    configured_stream: &DerivedName,
    today: Date,
    retention_days: i64,
    candidate: &SegmentCandidate,
    custody: (&str, &SegmentsEnvelope),
    delete_hook: Option<Arc<dyn Fn(usize) + Send + Sync>>,
) -> RetentionOutcome {
    let (authoritative_key, listing) = custody;
    if retention_days < 0 {
        return RetentionOutcome::Disabled;
    }
    if !retention_eligible(candidate.day(), today, retention_days)
        || candidate.stream() != configured_stream.as_str()
    {
        return RetentionOutcome::Ineligible;
    }

    let root = captures_root.to_owned();
    let target = candidate.clone();
    let paths =
        match tokio::task::spawn_blocking(move || resolve_segment_files(&root, &target)).await {
            Ok(Some(paths)) => paths,
            _ => return RetentionOutcome::Retained,
        };
    let first_inventory = match inventory_files(paths.clone()).await {
        Ok(inventory) => inventory,
        Err(_) => return RetentionOutcome::Retained,
    };
    if !fresh_listing_proves_custody(
        listing,
        candidate.segment(),
        authoritative_key,
        &first_inventory,
    ) {
        return RetentionOutcome::Retained;
    }
    let first_paths = paths.clone();
    let expected_paths = paths;
    let first_identities =
        match tokio::task::spawn_blocking(move || file_identities(&first_paths)).await {
            Ok(Some(identities)) => identities,
            _ => return RetentionOutcome::Retained,
        };

    let root = captures_root.to_owned();
    let target = candidate.clone();
    let second_paths =
        match tokio::task::spawn_blocking(move || resolve_segment_files(&root, &target)).await {
            Ok(Some(paths)) if paths == expected_paths => paths,
            _ => return RetentionOutcome::Retained,
        };
    let second_inventory = match inventory_files(second_paths.clone()).await {
        Ok(inventory) if inventory == first_inventory => inventory,
        _ => return RetentionOutcome::Retained,
    };
    let identities_path = second_paths.clone();
    let second_identities =
        match tokio::task::spawn_blocking(move || file_identities(&identities_path)).await {
            Ok(Some(identities)) if identities == first_identities => identities,
            _ => return RetentionOutcome::Retained,
        };
    if !fresh_listing_proves_custody(
        listing,
        candidate.segment(),
        authoritative_key,
        &second_inventory,
    ) {
        return RetentionOutcome::Retained;
    }

    let root = captures_root.to_owned();
    let target = candidate.clone();
    match tokio::task::spawn_blocking(move || {
        delete_revalidated_segment(
            &root,
            &target,
            &second_paths,
            &second_identities,
            delete_hook.as_deref(),
        )
    })
    .await
    {
        Ok(true) => RetentionOutcome::Deleted,
        _ => RetentionOutcome::Retained,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncFailureClass {
    Direct,
    Relay,
    Auth,
    Timeout,
    Contract,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncOperationError {
    RetainCandidate(DiagnosticCode),
    EndPass(SyncFailureClass),
    EndPassDiagnostic(SyncFailureClass, DiagnosticCode),
}

impl fmt::Display for SyncOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let diagnostic = match self {
            Self::RetainCandidate(code) | Self::EndPassDiagnostic(_, code) => *code,
            Self::EndPass(failure) => diagnostic_for_failure(*failure),
        };
        formatter.write_str(diagnostic.message())
    }
}

impl std::error::Error for SyncOperationError {}

pub trait SyncJournal: Send {
    fn observer_name(&self) -> Option<&str>;

    fn ensure_registered<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SyncOperationError>> + Send + 'a>>;

    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>>;

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>;

    fn status_event<'a>(
        &'a mut self,
        beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>>;
}

#[derive(Clone, Default)]
pub struct SyncWake {
    notify: Arc<Notify>,
}

impl SyncWake {
    pub fn segment_closed(&self, close: &SegmentClose) {
        if matches!(close, SegmentClose::Finalized(_)) {
            self.notify.notify_one();
        }
    }

    pub async fn wait(&self) {
        self.notify.notified().await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncPassSummary {
    pub attempted: usize,
    pub contacted: bool,
    pub custodied: usize,
    pub more_work: bool,
    pub failure: Option<SyncFailureClass>,
    pub diagnostic: Option<DiagnosticCode>,
}

impl SyncPassSummary {
    fn empty() -> Self {
        Self {
            attempted: 0,
            contacted: false,
            custodied: 0,
            more_work: false,
            failure: None,
            diagnostic: None,
        }
    }
}

struct Backoff {
    next_delay: usize,
    deadline: Option<Instant>,
}

impl Backoff {
    fn new() -> Self {
        Self {
            next_delay: 0,
            deadline: None,
        }
    }

    fn successful_operation(&mut self) {
        self.next_delay = 0;
        self.deadline = None;
    }

    fn failed_operation(&mut self) {
        let delay = RETRY_DELAYS[self.next_delay];
        self.next_delay = (self.next_delay + 1).min(RETRY_DELAYS.len() - 1);
        self.deadline = Some(Instant::now() + delay);
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn deadline_reached(&mut self) {
        self.deadline = None;
    }
}

pub struct SyncScheduler {
    captures_root: PathBuf,
    stream: DerivedName,
    retention_days: i64,
    clock: Arc<dyn Clock>,
    wake: SyncWake,
    cursor: Option<SegmentCandidate>,
    remaining_in_sweep: usize,
    backoff: Backoff,
    activity: Option<watch::Sender<SyncActivity>>,
    health: Option<HealthWriter>,
    facts: SyncFacts,
    started_at: Duration,
    last_status_event: Option<Duration>,
}

impl SyncScheduler {
    pub fn new(
        captures_root: PathBuf,
        stream: DerivedName,
        retention_days: i64,
        clock: Arc<dyn Clock>,
        wake: SyncWake,
    ) -> Self {
        let started_at = clock.monotonic_now();
        Self {
            captures_root,
            stream,
            retention_days,
            clock,
            wake,
            cursor: None,
            remaining_in_sweep: 0,
            backoff: Backoff::new(),
            activity: None,
            health: None,
            facts: SyncFacts::default(),
            started_at,
            last_status_event: None,
        }
    }

    pub fn with_observability(
        mut self,
        activity: watch::Sender<SyncActivity>,
        health: HealthWriter,
    ) -> Self {
        self.activity = Some(activity);
        self.health = Some(health);
        self
    }

    pub fn with_activity(mut self, activity: watch::Sender<SyncActivity>) -> Self {
        self.activity = Some(activity);
        self
    }

    pub async fn run<S>(&mut self, journal: &mut dyn SyncJournal, shutdown: S)
    where
        S: Future<Output = ()> + Send,
    {
        tokio::pin!(shutdown);
        let mut periodic = tokio::time::interval_at(
            Instant::now() + PERIODIC_SYNC_INTERVAL,
            PERIODIC_SYNC_INTERVAL,
        );
        periodic.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut requested = true;

        loop {
            if requested {
                if let Some(deadline) = self.backoff.deadline() {
                    if Instant::now() >= deadline {
                        self.backoff.deadline_reached();
                    } else {
                        tokio::select! {
                            biased;
                            () = &mut shutdown => return,
                            () = self.wake.wait() => {},
                            _ = periodic.tick() => {
                                self.write_health().await;
                            },
                            () = tokio::time::sleep_until(deadline) => {
                                self.backoff.deadline_reached();
                            }
                        }
                        continue;
                    }
                }

                let mut summary = self.run_pass(journal).await;
                self.update_facts(&summary);
                if summary.failure.is_none()
                    && summary.diagnostic.is_none()
                    && self.status_event_due()
                    && journal.observer_name().is_some()
                {
                    let beacon = self.status_beacon(journal.observer_name());
                    match journal.status_event(&beacon).await {
                        Ok(()) => {
                            summary.contacted = true;
                            self.facts
                                .successful_contact(self.clock.wall_now().unix_timestamp());
                            self.backoff.successful_operation();
                        }
                        Err(error) => {
                            summary = self.end_pass(summary, error);
                            self.update_facts(&summary);
                        }
                    }
                    self.last_status_event = Some(self.clock.monotonic_now());
                }
                self.write_health().await;
                requested = summary.failure.is_some() || summary.more_work;
                if requested {
                    tokio::task::yield_now().await;
                }
                continue;
            }

            tokio::select! {
                biased;
                () = &mut shutdown => return,
                () = self.wake.wait() => requested = true,
                _ = periodic.tick() => requested = true,
            }
        }
    }

    pub async fn run_pass(&mut self, journal: &mut dyn SyncJournal) -> SyncPassSummary {
        let mut summary = SyncPassSummary::empty();
        match journal.ensure_registered().await {
            Ok(contacted) => {
                if contacted {
                    summary.contacted = true;
                    self.backoff.successful_operation();
                }
            }
            Err(error) => return self.end_pass(summary, error),
        }

        let captures_root = self.captures_root.clone();
        let stream = self.stream.clone();
        let candidates =
            match tokio::task::spawn_blocking(move || scan_candidates(&captures_root, &stream))
                .await
            {
                Ok(Ok(candidates)) => candidates,
                _ => {
                    self.remaining_in_sweep = 0;
                    return self.end_pass(
                        summary,
                        SyncOperationError::EndPassDiagnostic(
                            SyncFailureClass::Contract,
                            DiagnosticCode::LocalSegmentInvalid,
                        ),
                    );
                }
            };
        self.facts.pending_segments = u64::try_from(candidates.len()).unwrap_or(u64::MAX);
        if candidates.is_empty() {
            self.remaining_in_sweep = 0;
            let today = local_today(self.clock.as_ref());
            match journal.segments(&format_day(today)).await {
                Ok(_) => {
                    summary.contacted = true;
                    self.backoff.successful_operation();
                }
                Err(error) => return self.end_pass(summary, error),
            }
            return summary;
        }

        let ordered = candidates_after_cursor(candidates, self.cursor.as_ref());
        if self.remaining_in_sweep == 0 {
            self.remaining_in_sweep = ordered.len();
        } else {
            self.remaining_in_sweep = self.remaining_in_sweep.min(ordered.len());
        }
        let candidate_limit = SEGMENTS_PER_PASS.min(self.remaining_in_sweep);
        self.facts.sync_in_progress = true;
        self.write_health().await;
        let _activity = ActivityGuard::new(self.activity.as_ref());
        for candidate in ordered.into_iter().take(candidate_limit) {
            self.cursor = Some(candidate.clone());
            summary.attempted += 1;
            self.remaining_in_sweep = self.remaining_in_sweep.saturating_sub(1);
            let root = self.captures_root.clone();
            let target = candidate.clone();
            let files =
                match tokio::task::spawn_blocking(move || resolve_segment_files(&root, &target))
                    .await
                {
                    Ok(Some(files)) => files,
                    _ => {
                        summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                        continue;
                    }
                };
            let custody_paths = files.clone();
            let upload = match journal.upload(&candidate, files).await {
                Ok(upload) => {
                    summary.contacted = true;
                    self.backoff.successful_operation();
                    upload
                }
                Err(SyncOperationError::RetainCandidate(code)) => {
                    summary.diagnostic = Some(code);
                    continue;
                }
                Err(error) => {
                    summary.more_work = self.remaining_in_sweep > 0;
                    return self.end_pass(summary, error);
                }
            };
            if matches!(upload.status, UploadStatus::Conflict | UploadStatus::Failed) {
                summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                continue;
            }
            let Some(authoritative_key) = upload.authoritative_key else {
                summary.more_work = self.remaining_in_sweep > 0;
                return self.end_pass(
                    summary,
                    SyncOperationError::EndPass(SyncFailureClass::Contract),
                );
            };
            let listing = match journal.segments(candidate.day()).await {
                Ok(listing) => {
                    summary.contacted = true;
                    self.backoff.successful_operation();
                    listing
                }
                Err(error) => {
                    summary.more_work = self.remaining_in_sweep > 0;
                    return self.end_pass(summary, error);
                }
            };
            let local_files = match inventory_files(custody_paths).await {
                Ok(files) => files,
                Err(_) => {
                    summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                    continue;
                }
            };
            if !fresh_listing_proves_custody(
                &listing,
                candidate.segment(),
                &authoritative_key,
                &local_files,
            ) {
                summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                continue;
            }
            summary.custodied += 1;
            match delete_custodied_segment(
                &self.captures_root,
                &self.stream,
                local_today(self.clock.as_ref()),
                self.retention_days,
                &candidate,
                &authoritative_key,
                &listing,
            )
            .await
            {
                RetentionOutcome::Deleted => {
                    self.facts.pending_segments = self.facts.pending_segments.saturating_sub(1);
                }
                RetentionOutcome::Retained => {
                    summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                }
                RetentionOutcome::Disabled | RetentionOutcome::Ineligible => {}
            }
        }
        summary.more_work = self.remaining_in_sweep > 0;
        summary
    }

    fn update_facts(&mut self, summary: &SyncPassSummary) {
        let now = self.clock.wall_now().unix_timestamp();
        if summary.custodied > 0 {
            self.facts.successful_sync(now);
        } else if summary.contacted {
            self.facts.successful_contact(now);
        }
        if let Some(code) = summary.diagnostic {
            self.facts.failed(code);
        } else if let Some(failure) = summary.failure {
            self.facts.failed(diagnostic_for_failure(failure));
        }
        self.facts.sync_in_progress = false;
    }

    fn status_event_due(&self) -> bool {
        self.last_status_event.is_none_or(|last| {
            self.clock.monotonic_now().saturating_sub(last) >= STATUS_HEARTBEAT_INTERVAL
        })
    }

    fn status_beacon(&self, observer_name: Option<&str>) -> StatusBeacon {
        StatusBeacon {
            name: observer_name.unwrap_or_default().to_owned(),
            uptime: self
                .clock
                .monotonic_now()
                .saturating_sub(self.started_at)
                .as_secs(),
            last_successful_sync: self.facts.last_successful_sync_unix_seconds,
            pending_queue_depth: self.facts.pending_segments,
            recent_error_count: self.facts.recent_error_count,
            last_error_reason: self.facts.last_error_code,
        }
    }

    async fn write_health(&self) {
        if let Some(health) = &self.health {
            let _ = health
                .write(&self.facts, self.clock.wall_now().unix_timestamp())
                .await;
        }
    }

    fn end_pass(
        &mut self,
        mut summary: SyncPassSummary,
        error: SyncOperationError,
    ) -> SyncPassSummary {
        let failure = match error {
            SyncOperationError::RetainCandidate(code) => {
                summary.diagnostic = Some(code);
                SyncFailureClass::Contract
            }
            SyncOperationError::EndPass(failure) => failure,
            SyncOperationError::EndPassDiagnostic(failure, code) => {
                summary.diagnostic = Some(code);
                failure
            }
        };
        self.backoff.failed_operation();
        summary.failure = Some(failure);
        summary
    }
}

struct ActivityGuard {
    sender: Option<watch::Sender<SyncActivity>>,
}

impl ActivityGuard {
    fn new(sender: Option<&watch::Sender<SyncActivity>>) -> Self {
        let sender = sender.cloned();
        if let Some(sender) = &sender {
            sender.send_replace(SyncActivity::Working);
        }
        Self { sender }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            sender.send_replace(SyncActivity::Idle);
        }
    }
}

struct ProductionJournal {
    credential: Credential,
    config_root: PathBuf,
    descriptor: RegistrationDescriptor,
    owner: Option<RegistrationOwner>,
    observer: Option<ObserverState>,
}

impl ProductionJournal {
    fn new(
        credential: Credential,
        config_root: PathBuf,
        platform: PlatformKind,
        hostname: String,
    ) -> Self {
        Self {
            credential,
            config_root,
            descriptor: RegistrationDescriptor {
                platform: match platform {
                    PlatformKind::Linux => "linux",
                    PlatformKind::Macos => "macos",
                }
                .to_owned(),
                hostname,
            },
            owner: None,
            observer: None,
        }
    }
}

impl SyncJournal for ProductionJournal {
    fn observer_name(&self) -> Option<&str> {
        self.observer
            .as_ref()
            .map(|observer| observer.name.as_str())
    }

    fn ensure_registered<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            if self.owner.is_none() {
                self.owner = Some(
                    RegistrationOwner::start(self.credential.clone(), self.config_root.clone())
                        .await
                        .map_err(map_diagnostic)?,
                );
            }
            let (observer, contacted) = self
                .owner
                .as_ref()
                .ok_or(SyncOperationError::EndPass(SyncFailureClass::Contract))?
                .ensure_registration(&self.descriptor)
                .await
                .map_err(map_journal_error)?;
            self.observer = Some(observer);
            Ok(contacted)
        })
    }

    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            let owner = self
                .owner
                .as_ref()
                .ok_or(SyncOperationError::EndPass(SyncFailureClass::Contract))?;
            let observer = self
                .observer
                .as_ref()
                .ok_or(SyncOperationError::EndPass(SyncFailureClass::Contract))?;
            owner
                .journal()
                .ingest_upload(
                    &observer.ingest_url,
                    candidate.day(),
                    candidate.segment(),
                    files,
                )
                .await
                .map_err(map_journal_error)
        })
    }

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.owner
                .as_ref()
                .ok_or(SyncOperationError::EndPass(SyncFailureClass::Contract))?
                .journal()
                .ingest_segments(day)
                .await
                .map_err(map_journal_error)
        })
    }

    fn status_event<'a>(
        &'a mut self,
        beacon: &'a StatusBeacon,
    ) -> Pin<Box<dyn Future<Output = Result<(), SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.owner
                .as_ref()
                .ok_or(SyncOperationError::EndPass(SyncFailureClass::Contract))?
                .journal()
                .ingest_event("observe", "status", beacon.fields())
                .await
                .map_err(map_journal_error)
        })
    }
}

pub struct SyncTask {
    pub config_root: PathBuf,
    pub data_root: PathBuf,
    pub config: RuntimeConfig,
    pub platform: PlatformKind,
    pub hostname: String,
    pub clock: Arc<dyn Clock>,
    pub wake: SyncWake,
    pub activity: watch::Sender<SyncActivity>,
    pub health: HealthWriter,
}

impl SyncTask {
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), DiagnosticCode> {
        let load_root = self.config_root.clone();
        let loaded = tokio::task::spawn_blocking(move || load_credential(&load_root))
            .await
            .map_err(|_| DiagnosticCode::PrivateStateIo)?;
        let credential = match loaded {
            Ok(Some(credential)) => credential,
            Ok(None) => {
                let facts = SyncFacts::default();
                refresh_waiting_health(&self.health, &facts, self.clock.as_ref(), &mut shutdown)
                    .await;
                return Ok(());
            }
            Err(code) => {
                let mut facts = SyncFacts::default();
                facts.failed(code);
                refresh_waiting_health(&self.health, &facts, self.clock.as_ref(), &mut shutdown)
                    .await;
                return Ok(());
            }
        };
        let mut journal =
            ProductionJournal::new(credential, self.config_root, self.platform, self.hostname);
        let mut scheduler = SyncScheduler::new(
            self.data_root.join("captures"),
            self.config.stream,
            self.config.cache_retention_days,
            self.clock,
            self.wake,
        )
        .with_observability(self.activity, self.health);
        scheduler.facts.paired = true;
        scheduler
            .run(&mut journal, wait_for_shutdown(&mut shutdown))
            .await;
        if let Some(owner) = journal.owner
            && let Err(code) = owner.shutdown().await
        {
            scheduler.facts.failed(code);
            scheduler.write_health().await;
            return Err(code);
        }
        Ok(())
    }
}

async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn refresh_waiting_health(
    health: &HealthWriter,
    facts: &SyncFacts,
    clock: &dyn Clock,
    shutdown: &mut watch::Receiver<bool>,
) {
    let mut heartbeat = tokio::time::interval(STATUS_HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => return,
            _ = heartbeat.tick() => {
                let _ = health.write(facts, clock.wall_now().unix_timestamp()).await;
            }
        }
    }
}

fn map_diagnostic(code: DiagnosticCode) -> SyncOperationError {
    match code {
        DiagnosticCode::JournalTimeout => SyncOperationError::EndPassDiagnostic(
            SyncFailureClass::Timeout,
            DiagnosticCode::JournalTimeout,
        ),
        DiagnosticCode::JournalContractInvalid
        | DiagnosticCode::PrivateStateInvalid
        | DiagnosticCode::PrivateStateIo => {
            SyncOperationError::EndPassDiagnostic(SyncFailureClass::Contract, code)
        }
        _ => SyncOperationError::EndPassDiagnostic(SyncFailureClass::Direct, code),
    }
}

fn map_journal_error(error: JournalError) -> SyncOperationError {
    let diagnostic = error.diagnostic();
    match diagnostic {
        DiagnosticCode::RequestTooLarge | DiagnosticCode::LocalSegmentInvalid => {
            SyncOperationError::RetainCandidate(diagnostic)
        }
        DiagnosticCode::JournalTimeout => SyncOperationError::EndPassDiagnostic(
            SyncFailureClass::Timeout,
            DiagnosticCode::JournalTimeout,
        ),
        DiagnosticCode::JournalRejected => match error.reason_code() {
            Some(
                JournalReasonCode::AuthKeyInvalid
                | JournalReasonCode::AuthRequired
                | JournalReasonCode::PlRevoked,
            ) => SyncOperationError::EndPassDiagnostic(
                SyncFailureClass::Auth,
                DiagnosticCode::JournalRejected,
            ),
            Some(
                JournalReasonCode::IngestContractInvalid
                | JournalReasonCode::IngestNoFiles
                | JournalReasonCode::IngestSidecarConflict,
            ) => SyncOperationError::RetainCandidate(DiagnosticCode::LocalSegmentInvalid),
            Some(JournalReasonCode::IngestStorageFailed) => SyncOperationError::EndPassDiagnostic(
                SyncFailureClass::Direct,
                DiagnosticCode::JournalRejected,
            ),
            _ => SyncOperationError::EndPassDiagnostic(
                SyncFailureClass::Contract,
                DiagnosticCode::JournalRejected,
            ),
        },
        DiagnosticCode::JournalContractInvalid
        | DiagnosticCode::PrivateStateInvalid
        | DiagnosticCode::PrivateStateIo => {
            SyncOperationError::EndPassDiagnostic(SyncFailureClass::Contract, diagnostic)
        }
        _ => SyncOperationError::EndPassDiagnostic(SyncFailureClass::Direct, diagnostic),
    }
}

fn diagnostic_for_failure(failure: SyncFailureClass) -> DiagnosticCode {
    match failure {
        SyncFailureClass::Auth => DiagnosticCode::JournalRejected,
        SyncFailureClass::Timeout => DiagnosticCode::JournalTimeout,
        SyncFailureClass::Contract => DiagnosticCode::JournalContractInvalid,
        SyncFailureClass::Direct | SyncFailureClass::Relay => DiagnosticCode::JournalUnavailable,
    }
}

fn scan_candidates(
    captures_root: &Path,
    stream: &DerivedName,
) -> Result<Vec<SegmentCandidate>, ()> {
    match fs::symlink_metadata(captures_root) {
        Ok(metadata) if is_plain_directory(&metadata) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        _ => return Err(()),
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(captures_root).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let day = entry.file_name().into_string().map_err(|_| ())?;
        if parse_day(&day).is_none() {
            continue;
        }
        if !plain_directory_entry(&entry) {
            return Err(());
        }
        let derived_day = derive_component(&day).map_err(|_| ())?;
        let day_path = derived_day.join_checked(captures_root).map_err(|_| ())?;
        if day_path != entry.path() {
            return Err(());
        }
        let stream_path = stream.join_checked(&day_path).map_err(|_| ())?;
        match fs::symlink_metadata(&stream_path) {
            Ok(metadata) if is_plain_directory(&metadata) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            _ => return Err(()),
        }
        for segment_entry in fs::read_dir(&stream_path).map_err(|_| ())? {
            let segment_entry = segment_entry.map_err(|_| ())?;
            let segment = segment_entry.file_name().into_string().map_err(|_| ())?;
            if !valid_segment_name(&segment) {
                continue;
            }
            if !plain_directory_entry(&segment_entry) {
                return Err(());
            }
            candidates.push(SegmentCandidate::new(day.clone(), stream.as_str(), segment));
        }
    }
    candidates.sort_by(|left, right| right.cmp(left));
    Ok(candidates)
}

fn candidates_after_cursor(
    candidates: Vec<SegmentCandidate>,
    cursor: Option<&SegmentCandidate>,
) -> Vec<SegmentCandidate> {
    let Some(cursor) = cursor else {
        return candidates;
    };
    let start = candidates
        .iter()
        .position(|candidate| candidate == cursor)
        .map(|index| (index + 1) % candidates.len())
        .or_else(|| candidates.iter().position(|candidate| candidate < cursor))
        .unwrap_or(0);
    candidates[start..]
        .iter()
        .chain(&candidates[..start])
        .cloned()
        .collect()
}

fn resolve_segment_files(
    captures_root: &Path,
    candidate: &SegmentCandidate,
) -> Option<Vec<PathBuf>> {
    let root_metadata = fs::symlink_metadata(captures_root).ok()?;
    if !is_plain_directory(&root_metadata) || parse_day(candidate.day()).is_none() {
        return None;
    }
    let day = exact_component(candidate.day())?;
    let stream = exact_component(candidate.stream())?;
    let segment = exact_component(candidate.segment())?;
    if !valid_segment_name(candidate.segment()) {
        return None;
    }
    let day_path = day.join_checked(captures_root).ok()?;
    let stream_path = stream.join_checked(&day_path).ok()?;
    let segment_path = segment.join_checked(&stream_path).ok()?;
    for directory in [&day_path, &stream_path, &segment_path] {
        let metadata = fs::symlink_metadata(directory).ok()?;
        if !is_plain_directory(&metadata) {
            return None;
        }
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(&segment_path).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().into_string().ok()?;
        if !valid_capture_filename(&name) || !plain_regular_entry(&entry) {
            return None;
        }
        let component = exact_component(&name)?;
        let path = component.join_checked(&segment_path).ok()?;
        if path != entry.path() {
            return None;
        }
        files.push(path);
    }
    if files.is_empty() {
        return None;
    }
    files.sort();
    Some(files)
}

fn file_identities(paths: &[PathBuf]) -> Option<Vec<FileIdentity>> {
    paths
        .iter()
        .map(|path| {
            let file = open_regular_readonly(path).ok()?;
            let metadata = file.metadata().ok()?;
            Some(FileIdentity {
                name: path.file_name()?.to_str()?.to_owned(),
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
            })
        })
        .collect()
}

fn delete_revalidated_segment(
    captures_root: &Path,
    candidate: &SegmentCandidate,
    expected_paths: &[PathBuf],
    expected_identities: &[FileIdentity],
    delete_hook: Option<&(dyn Fn(usize) + Send + Sync)>,
) -> bool {
    let Some(paths) = resolve_segment_files(captures_root, candidate) else {
        return false;
    };
    if paths != expected_paths || file_identities(&paths).as_deref() != Some(expected_identities) {
        return false;
    }
    let Some(segment_path) = expected_paths.first().and_then(|path| path.parent()) else {
        return false;
    };
    let Some(stream_path) = segment_path.parent() else {
        return false;
    };
    let Ok(segment_directory) = open_directory_readonly(segment_path) else {
        return false;
    };
    let Ok(stream_directory) = open_directory_readonly(stream_path) else {
        return false;
    };
    let mut retained_files = Vec::with_capacity(paths.len());
    for (path, expected) in paths.iter().zip(expected_identities) {
        let Ok(file) = open_regular_readonly_at(&segment_directory, &expected.name, path) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        if !metadata_matches(&metadata, expected) {
            return false;
        }
        retained_files.push(file);
    }
    for (index, (path, expected)) in paths.iter().zip(expected_identities).enumerate() {
        if let Some(hook) = delete_hook {
            hook(index);
        }
        let current = open_regular_readonly_at(&segment_directory, &expected.name, path);
        let current_matches = current
            .as_ref()
            .ok()
            .and_then(|file| file.metadata().ok())
            .is_some_and(|metadata| metadata_matches(&metadata, expected));
        if !current_matches
            || rustix::fs::unlinkat(
                &segment_directory,
                expected.name.as_str(),
                rustix::fs::AtFlags::empty(),
            )
            .is_err()
        {
            let _ = restore_expected_files(
                &paths,
                expected_identities,
                segment_path,
                &segment_directory,
                &mut retained_files,
            );
            return false;
        }
    }
    if rustix::fs::unlinkat(
        &stream_directory,
        candidate.segment(),
        rustix::fs::AtFlags::REMOVEDIR,
    )
    .is_err()
    {
        let _ = restore_expected_files(
            &paths,
            expected_identities,
            segment_path,
            &segment_directory,
            &mut retained_files,
        );
        return false;
    }
    let _ = sync_directory(stream_path);
    true
}

fn restore_expected_files(
    paths: &[PathBuf],
    expected_identities: &[FileIdentity],
    segment_path: &Path,
    segment_directory: &fs::File,
    retained_files: &mut [fs::File],
) -> bool {
    let mut restored = true;
    for (index, ((path, expected), file)) in paths
        .iter()
        .zip(expected_identities)
        .zip(retained_files)
        .enumerate()
    {
        let current_matches = open_regular_readonly_at(segment_directory, &expected.name, path)
            .ok()
            .and_then(|current| current.metadata().ok())
            .is_some_and(|metadata| metadata_matches(&metadata, expected));
        if current_matches {
            continue;
        }
        match rustix::fs::statat(
            segment_directory,
            expected.name.as_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(_) => {
                let preserved_name = format!(".retention-conflict-{index}");
                if rustix::fs::statat(
                    segment_directory,
                    preserved_name.as_str(),
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .is_ok()
                    || rustix::fs::renameat(
                        segment_directory,
                        expected.name.as_str(),
                        segment_directory,
                        preserved_name.as_str(),
                    )
                    .is_err()
                {
                    restored = false;
                    continue;
                }
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(_) => {
                restored = false;
                continue;
            }
        }
        if file.seek(SeekFrom::Start(0)).is_err() {
            restored = false;
            continue;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err()
            || atomic_write_bytes(path, segment_path, &bytes).is_err()
        {
            restored = false;
        }
    }
    restored
}

fn metadata_matches(metadata: &fs::Metadata, expected: &FileIdentity) -> bool {
    metadata.dev() == expected.device
        && metadata.ino() == expected.inode
        && metadata.len() == expected.size
}

fn retention_eligible(day: &str, today: Date, retention_days: i64) -> bool {
    let Some(day) = parse_day(day) else {
        return false;
    };
    if day == today {
        return false;
    }
    let cutoff = if retention_days == 0 {
        today
    } else {
        let Some(seconds) = retention_days.checked_mul(86_400) else {
            return false;
        };
        let Some(cutoff) = today.checked_sub(time::Duration::seconds(seconds)) else {
            return false;
        };
        cutoff
    };
    day < cutoff
}

fn exact_component(value: &str) -> Option<DerivedName> {
    derive_component(value)
        .ok()
        .filter(|component| component.as_str() == value)
}

fn parse_day(value: &str) -> Option<Date> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year = value[0..4].parse().ok()?;
    let month = Month::try_from(value[4..6].parse::<u8>().ok()?).ok()?;
    let day = value[6..8].parse().ok()?;
    Date::from_calendar_date(year, month, day).ok()
}

fn format_day(day: Date) -> String {
    format!(
        "{:04}{:02}{:02}",
        day.year(),
        u8::from(day.month()),
        day.day()
    )
}

fn local_today(clock: &dyn Clock) -> Date {
    clock.wall_now().to_offset(clock.local_offset()).date()
}

fn valid_segment_name(value: &str) -> bool {
    let Some((time, duration)) = value.split_once('_') else {
        return false;
    };
    if time.len() != 6
        || duration.len() < 3
        || !time.bytes().all(|byte| byte.is_ascii_digit())
        || !duration.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let hour = time[0..2].parse::<u8>().ok();
    let minute = time[2..4].parse::<u8>().ok();
    let second = time[4..6].parse::<u8>().ok();
    matches!(
        (hour, minute, second),
        (Some(hour), Some(minute), Some(second))
            if time::Time::from_hms(hour, minute, second).is_ok()
    )
}

fn valid_capture_filename(value: &str) -> bool {
    let Some(session) = value
        .strip_prefix("tmux_")
        .and_then(|value| value.strip_suffix("_screen.jsonl"))
    else {
        return false;
    };
    exact_component(value).is_some() && exact_component(session).is_some()
}

fn plain_directory_entry(entry: &fs::DirEntry) -> bool {
    let Ok(file_type) = entry.file_type() else {
        return false;
    };
    let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
        return false;
    };
    !file_type.is_symlink()
        && file_type.is_dir()
        && !metadata.file_type().is_symlink()
        && metadata.is_dir()
}

fn plain_regular_entry(entry: &fs::DirEntry) -> bool {
    let Ok(file_type) = entry.file_type() else {
        return false;
    };
    let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
        return false;
    };
    !file_type.is_symlink()
        && file_type.is_file()
        && !metadata.file_type().is_symlink()
        && metadata.is_file()
}

fn is_plain_directory(metadata: &fs::Metadata) -> bool {
    !metadata.file_type().is_symlink() && metadata.is_dir()
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use spl_transport::credential::{Credential, EndpointAddr};

    use super::{SyncFailureClass, SyncOperationError, TokenPersistence, map_diagnostic};
    use crate::health::DiagnosticCode;
    use crate::private_link::{CREDENTIALS_FILENAME, load_credential};

    #[test]
    fn failed_token_persistence_is_reported_and_retried() {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime")
            .block_on(async {
                let suffix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let root = std::env::temp_dir().join(format!(
                    "solstone-token-persistence-{}-{suffix}",
                    std::process::id()
                ));
                fs::create_dir(&root).expect("create token test root");
                fs::create_dir(root.join(CREDENTIALS_FILENAME))
                    .expect("create invalid credential target");
                let (persistence, hook) = TokenPersistence::new(root.clone(), credential());
                hook("refreshed-token", 1_900_000_000);

                let code = persistence
                    .persist_pending()
                    .await
                    .expect_err("invalid credential target was accepted");
                assert!(matches!(
                    map_diagnostic(code),
                    SyncOperationError::EndPassDiagnostic(
                        SyncFailureClass::Contract,
                        DiagnosticCode::PrivateStateInvalid
                    )
                ));

                fs::remove_dir(root.join(CREDENTIALS_FILENAME))
                    .expect("remove invalid credential target");
                persistence
                    .persist_pending()
                    .await
                    .expect("retry pending token persistence");
                let loaded = load_credential(&root)
                    .expect("load persisted credential")
                    .expect("persisted credential exists");
                assert!(loaded.device_token.is_some());
                assert!(loaded.device_token_expires_at.is_some());
                fs::remove_dir_all(root).expect("remove token test root");
            });
    }

    fn credential() -> Credential {
        Credential {
            client_key_pem: "test-key".to_owned(),
            client_cert_pem: "test-cert".to_owned(),
            ca_chain_pem: vec!["test-ca".to_owned()],
            ca_fp_prefix: vec![1, 2, 3, 4],
            instance_id: "test-instance".to_owned(),
            home_label: "test-home".to_owned(),
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
}
