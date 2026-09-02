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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spl_transport::client::TokenPersistHook;
use spl_transport::credential::Credential;
use time::{Date, Month};
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::Instant;

use crate::clock::Clock;
use crate::config::{RuntimeConfig, default_stream};
use crate::health::{DiagnosticCode, HealthWriter, SyncFacts};
use crate::journal::{
    IngestDayManifest, IngestManifest, JournalClient, JournalError, JournalReasonCode,
    ListingFileStatus, LocalFile, SegmentsEnvelope, UploadResult, UploadStatus, inventory_files,
    stream_sha256_hex,
};
use crate::name::{DerivedName, derive_component};
use crate::private_link::{PrivateLinkBridge, load_credential, persist_credential};
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
const CANDIDATES_PER_BATCH: usize = 8;
const HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncActivity {
    Idle,
    Working,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncInstrumentationSnapshot {
    pub candidate_scans: usize,
    pub batches: usize,
    pub batch_yields: usize,
    pub hashed_files: usize,
    pub hashed_bytes: u64,
}

#[derive(Clone, Default)]
pub struct SyncInstrumentation {
    candidate_scans: Arc<AtomicUsize>,
    batches: Arc<AtomicUsize>,
    batch_yields: Arc<AtomicUsize>,
    hashed_files: Arc<AtomicUsize>,
    hashed_bytes: Arc<AtomicUsize>,
}

impl SyncInstrumentation {
    pub fn snapshot(&self) -> SyncInstrumentationSnapshot {
        SyncInstrumentationSnapshot {
            candidate_scans: self.candidate_scans.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            batch_yields: self.batch_yields.load(Ordering::Relaxed),
            hashed_files: self.hashed_files.load(Ordering::Relaxed),
            hashed_bytes: self.hashed_bytes.load(Ordering::Relaxed) as u64,
        }
    }

    pub(crate) fn candidate_scan(&self) {
        self.candidate_scans.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch(&self) {
        self.batches.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn batch_yield(&self) {
        self.batch_yields.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn hashed_file(&self, bytes: u64) {
        self.hashed_files.fetch_add(1, Ordering::Relaxed);
        self.hashed_bytes.fetch_add(
            usize::try_from(bytes).unwrap_or(usize::MAX),
            Ordering::Relaxed,
        );
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

pub struct JournalSession {
    bridge: PrivateLinkBridge,
    journal: JournalClient,
    token_persistence: TokenPersistence,
}

impl JournalSession {
    pub async fn start(
        credential: Credential,
        config_root: PathBuf,
    ) -> Result<Self, DiagnosticCode> {
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
            token_persistence,
        })
    }

    pub fn journal(&self) -> &JournalClient {
        &self.journal
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
            && remote.size == local.size
            && matches!(
                remote.status,
                ListingFileStatus::Present | ListingFileStatus::Processed
            )
    })
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

pub struct RetentionFence {
    accepting: AsyncMutex<bool>,
    irreversible: Arc<Semaphore>,
}

impl RetentionFence {
    pub fn new() -> Self {
        Self {
            accepting: AsyncMutex::new(true),
            irreversible: Arc::new(Semaphore::new(1)),
        }
    }

    async fn begin_irreversible(&self) -> Option<OwnedSemaphorePermit> {
        let accepting = self.accepting.lock().await;
        if !*accepting {
            return None;
        }
        self.irreversible.clone().acquire_owned().await.ok()
    }

    pub async fn close_and_drain(&self) {
        let mut accepting = self.accepting.lock().await;
        *accepting = false;
        let _permit = self.irreversible.clone().acquire_owned().await;
    }
}

impl Default for RetentionFence {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Eq, PartialEq)]
struct FileIdentity {
    name: String,
    device: u64,
    inode: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

#[allow(clippy::too_many_arguments)]
pub async fn delete_custodied_segment(
    captures_root: &Path,
    configured_stream: &DerivedName,
    today: Date,
    retention_days: i64,
    candidate: &SegmentCandidate,
    authoritative_key: &str,
    listing: &SegmentsEnvelope,
    instrumentation: SyncInstrumentation,
    fence: Arc<RetentionFence>,
) -> RetentionOutcome {
    delete_custodied_segment_with_hook_inner(
        captures_root,
        configured_stream,
        today,
        retention_days,
        candidate,
        (authoritative_key, listing),
        None,
        instrumentation,
        fence,
    )
    .await
}

#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn delete_custodied_segment_with_hook(
    captures_root: &Path,
    configured_stream: &DerivedName,
    today: Date,
    retention_days: i64,
    candidate: &SegmentCandidate,
    custody: (&str, &SegmentsEnvelope),
    delete_hook: Arc<dyn Fn(usize) + Send + Sync>,
    instrumentation: SyncInstrumentation,
    fence: Arc<RetentionFence>,
) -> RetentionOutcome {
    delete_custodied_segment_with_hook_inner(
        captures_root,
        configured_stream,
        today,
        retention_days,
        candidate,
        custody,
        Some(delete_hook),
        instrumentation,
        fence,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn delete_custodied_segment_with_hook_inner(
    captures_root: &Path,
    configured_stream: &DerivedName,
    today: Date,
    retention_days: i64,
    candidate: &SegmentCandidate,
    custody: (&str, &SegmentsEnvelope),
    delete_hook: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    instrumentation: SyncInstrumentation,
    fence: Arc<RetentionFence>,
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
            Ok(ResolvedSegmentFiles::Found(paths)) => paths,
            _ => return RetentionOutcome::Retained,
        };
    let first_inventory = match inventory_files(paths.clone(), Some(instrumentation.clone())).await
    {
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
            Ok(ResolvedSegmentFiles::Found(paths)) if paths == expected_paths => paths,
            _ => return RetentionOutcome::Retained,
        };
    let second_inventory = match inventory_files(second_paths.clone(), Some(instrumentation)).await
    {
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
    let Some(permit) = fence.begin_irreversible().await else {
        return RetentionOutcome::Retained;
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        delete_revalidated_segment(
            &root,
            &target,
            &second_paths,
            &second_identities,
            &second_inventory,
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
    EndSweep(SyncFailureClass),
    EndSweepDiagnostic(SyncFailureClass, DiagnosticCode),
}

impl fmt::Display for SyncOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let diagnostic = match self {
            Self::RetainCandidate(code) | Self::EndSweepDiagnostic(_, code) => *code,
            Self::EndSweep(failure) => diagnostic_for_failure(*failure),
        };
        formatter.write_str(diagnostic.message())
    }
}

impl std::error::Error for SyncOperationError {}

pub trait SyncJournal: Send {
    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>>;

    fn manifest<'a>(
        &'a mut self,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestManifest, SyncOperationError>> + Send + 'a>>;

    fn manifest_day<'a>(
        &'a mut self,
        day: &'a str,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestDayManifest, SyncOperationError>> + Send + 'a>>;

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>;
}

#[derive(Clone, Default)]
pub struct SyncWake {
    notify: Arc<Notify>,
    pending: Arc<AtomicBool>,
}

impl SyncWake {
    pub fn segment_closed(&self, close: &SegmentClose) {
        if matches!(close, SegmentClose::Finalized(_)) {
            self.pending.store(true, Ordering::Release);
            self.notify.notify_one();
        }
    }

    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.pending.swap(false, Ordering::AcqRel) {
                return;
            }
            notified.await;
        }
    }

    fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncSweepSummary {
    pub attempted: usize,
    pub contacted: bool,
    pub custodied: usize,
    pub cancelled: bool,
    pub failure: Option<SyncFailureClass>,
    pub diagnostic: Option<DiagnosticCode>,
}

impl SyncSweepSummary {
    fn empty() -> Self {
        Self {
            attempted: 0,
            contacted: false,
            custodied: 0,
            cancelled: false,
            failure: None,
            diagnostic: None,
        }
    }
}

struct CachedInventory {
    names: Vec<String>,
    identities: Vec<FileIdentity>,
    inventory: Vec<LocalFile>,
}

struct Reconciliation {
    root: IngestManifest,
    day: IngestDayManifest,
    listing: SegmentsEnvelope,
}

impl Reconciliation {
    fn proves(
        &self,
        requested_day: &str,
        submitted_key: &str,
        authoritative_key: &str,
        local_files: &[LocalFile],
    ) -> bool {
        self.root.days.contains_key(requested_day)
            && self.day.day == requested_day
            && self.day.segments.contains_key(authoritative_key)
            && fresh_listing_proves_custody(
                &self.listing,
                submitted_key,
                authoritative_key,
                local_files,
            )
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
    source: String,
    retention_days: i64,
    clock: Arc<dyn Clock>,
    wake: SyncWake,
    inventories: HashMap<SegmentCandidate, CachedInventory>,
    instrumentation: SyncInstrumentation,
    backoff: Backoff,
    activity: Option<watch::Sender<SyncActivity>>,
    health: Option<HealthWriter>,
    facts: SyncFacts,
    retention_fence: Arc<RetentionFence>,
}

impl SyncScheduler {
    pub fn new(
        captures_root: PathBuf,
        stream: DerivedName,
        source: String,
        retention_days: i64,
        clock: Arc<dyn Clock>,
        wake: SyncWake,
    ) -> Self {
        Self {
            captures_root,
            stream,
            source,
            retention_days,
            clock,
            wake,
            inventories: HashMap::new(),
            instrumentation: SyncInstrumentation::default(),
            backoff: Backoff::new(),
            activity: None,
            health: None,
            facts: SyncFacts::default(),
            retention_fence: Arc::new(RetentionFence::new()),
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

    pub fn with_retention_fence(mut self, fence: Arc<RetentionFence>) -> Self {
        self.retention_fence = fence;
        self
    }

    pub fn instrumentation(&self) -> SyncInstrumentationSnapshot {
        self.instrumentation.snapshot()
    }

    pub fn cached_inventories(&self) -> usize {
        self.inventories.len()
    }

    pub async fn run<S>(&mut self, journal: &mut dyn SyncJournal, shutdown_future: S)
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let (stop, receiver) = watch::channel(false);
        tokio::spawn(async move {
            shutdown_future.await;
            stop.send_replace(true);
        });
        self.run_with_shutdown(journal, receiver).await;
    }

    pub async fn run_with_shutdown(
        &mut self,
        journal: &mut dyn SyncJournal,
        mut shutdown: watch::Receiver<bool>,
    ) {
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
                            () = wait_for_shutdown(&mut shutdown) => return,
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

                let summary = self.run_sweep(journal, shutdown.clone()).await;
                if summary.cancelled {
                    return;
                }
                self.update_facts(&summary);
                self.write_health().await;
                requested = summary.failure.is_some();
                if self.wake.take_pending() {
                    requested = true;
                }
                if requested {
                    tokio::task::yield_now().await;
                }
                continue;
            }

            tokio::select! {
                biased;
                () = wait_for_shutdown(&mut shutdown) => return,
                () = self.wake.wait() => requested = true,
                _ = periodic.tick() => requested = true,
            }
        }
    }

    pub async fn run_sweep(
        &mut self,
        journal: &mut dyn SyncJournal,
        mut shutdown: watch::Receiver<bool>,
    ) -> SyncSweepSummary {
        let mut summary = SyncSweepSummary::empty();
        let captures_root = self.captures_root.clone();
        let stream = self.stream.clone();
        let instrumentation = self.instrumentation.clone();
        let candidates = match tokio::task::spawn_blocking(move || {
            instrumentation.candidate_scan();
            scan_candidates(&captures_root, &stream)
        })
        .await
        {
            Ok(Ok(candidates)) => candidates,
            _ => {
                self.facts.pending_segments = 0;
                return self.end_sweep(
                    summary,
                    SyncOperationError::EndSweepDiagnostic(
                        SyncFailureClass::Contract,
                        DiagnosticCode::LocalSegmentInvalid,
                    ),
                );
            }
        };
        let snapshot = candidates.iter().cloned().collect::<HashSet<_>>();
        self.inventories
            .retain(|candidate, _| snapshot.contains(candidate));
        if candidates.is_empty() {
            self.facts.pending_segments = 0;
            match cancellable(&mut shutdown, journal.manifest(&self.source)).await {
                Err(()) => return self.cancelled_sweep(summary).await,
                Ok(Ok(_)) => {
                    summary.contacted = true;
                    self.backoff.successful_operation();
                }
                Ok(Err(error)) => return self.end_sweep(summary, error),
            }
            return summary;
        }
        self.facts.pending_segments = u64::try_from(candidates.len()).unwrap_or(u64::MAX);
        let today = local_today(self.clock.as_ref());
        let mut reconciliations_by_day = HashMap::new();
        self.facts.sync_in_progress = true;
        self.write_health().await;
        let mut activity = None;
        for (batch_index, batch) in candidates.chunks(CANDIDATES_PER_BATCH).enumerate() {
            self.instrumentation.batch();
            let mut batch_fresh_listing_days = HashSet::new();
            for candidate in batch {
                if shutdown_requested(&mut shutdown) {
                    return self.cancelled_sweep_with_activity(summary, activity).await;
                }
                summary.attempted += 1;
                let root = self.captures_root.clone();
                let target = candidate.clone();
                let files = match tokio::task::spawn_blocking(move || {
                    resolve_segment_files(&root, &target)
                })
                .await
                {
                    Ok(ResolvedSegmentFiles::Found(files)) => files,
                    Ok(ResolvedSegmentFiles::Missing) => {
                        self.inventories.remove(candidate);
                        self.facts.pending_segments = self.facts.pending_segments.saturating_sub(1);
                        continue;
                    }
                    Ok(ResolvedSegmentFiles::Invalid) | Err(_) => {
                        summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                        continue;
                    }
                };
                let local_files = match self.inventory_for(candidate, &files).await {
                    Ok(files) => files,
                    Err(()) => {
                        summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                        continue;
                    }
                };
                let deletion_eligible = self.retention_days >= 0
                    && retention_eligible(candidate.day(), today, self.retention_days);
                let cached_proves_custody = reconciliations_by_day
                    .get(candidate.day())
                    .is_some_and(|reconciliation: &Reconciliation| {
                        reconciliation.proves(
                            candidate.day(),
                            candidate.segment(),
                            candidate.segment(),
                            &local_files,
                        )
                    });
                if cached_proves_custody {
                    summary.custodied += 1;
                    summary.contacted = true;
                    self.backoff.successful_operation();
                    self.facts.pending_segments = self.facts.pending_segments.saturating_sub(1);
                    if deletion_eligible
                        && batch_fresh_listing_days.contains(candidate.day())
                        && let Some(reconciliation) = reconciliations_by_day.get(candidate.day())
                    {
                        match delete_custodied_segment(
                            &self.captures_root,
                            &self.stream,
                            today,
                            self.retention_days,
                            candidate,
                            candidate.segment(),
                            &reconciliation.listing,
                            self.instrumentation.clone(),
                            Arc::clone(&self.retention_fence),
                        )
                        .await
                        {
                            RetentionOutcome::Retained => {
                                summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                            }
                            RetentionOutcome::Deleted => {
                                self.inventories.remove(candidate);
                            }
                            RetentionOutcome::Disabled | RetentionOutcome::Ineligible => {}
                        }
                    }
                    continue;
                }
                reconciliations_by_day.remove(candidate.day());
                batch_fresh_listing_days.remove(candidate.day());
                if activity.is_none() {
                    activity = Some(ActivityGuard::new(self.activity.as_ref()));
                }
                let upload = match cancellable(
                    &mut shutdown,
                    journal.upload(candidate, files, &self.source),
                )
                .await
                {
                    Err(()) => return self.cancelled_sweep_with_activity(summary, activity).await,
                    Ok(Ok(upload)) => {
                        summary.contacted = true;
                        self.backoff.successful_operation();
                        upload
                    }
                    Ok(Err(SyncOperationError::RetainCandidate(code))) => {
                        summary.diagnostic = Some(code);
                        continue;
                    }
                    Ok(Err(error)) => {
                        drop(activity);
                        return self.end_sweep(summary, error);
                    }
                };
                if matches!(upload.status, UploadStatus::Conflict | UploadStatus::Failed) {
                    summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                    continue;
                }
                let Some(authoritative_key) = upload.authoritative_key else {
                    drop(activity);
                    return self.end_sweep(
                        summary,
                        SyncOperationError::EndSweep(SyncFailureClass::Contract),
                    );
                };
                let root_manifest = match cancellable(&mut shutdown, journal.manifest(&self.source))
                    .await
                {
                    Err(()) => return self.cancelled_sweep_with_activity(summary, activity).await,
                    Ok(Ok(manifest)) => {
                        summary.contacted = true;
                        self.backoff.successful_operation();
                        manifest
                    }
                    Ok(Err(SyncOperationError::RetainCandidate(code))) => {
                        summary.diagnostic = Some(code);
                        continue;
                    }
                    Ok(Err(error)) => {
                        drop(activity);
                        return self.end_sweep(summary, error);
                    }
                };
                if !root_manifest.days.contains_key(candidate.day()) {
                    summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                    continue;
                }
                let day_manifest = match cancellable(
                    &mut shutdown,
                    journal.manifest_day(candidate.day(), &self.source),
                )
                .await
                {
                    Err(()) => {
                        return self.cancelled_sweep_with_activity(summary, activity).await;
                    }
                    Ok(Ok(manifest)) => {
                        summary.contacted = true;
                        self.backoff.successful_operation();
                        manifest
                    }
                    Ok(Err(SyncOperationError::RetainCandidate(code))) => {
                        summary.diagnostic = Some(code);
                        continue;
                    }
                    Ok(Err(error)) => {
                        drop(activity);
                        return self.end_sweep(summary, error);
                    }
                };
                if day_manifest.day != candidate.day()
                    || !day_manifest.segments.contains_key(&authoritative_key)
                {
                    summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                    continue;
                }
                let listing = match cancellable(
                    &mut shutdown,
                    journal.segments(candidate.day(), &self.source),
                )
                .await
                {
                    Err(()) => return self.cancelled_sweep_with_activity(summary, activity).await,
                    Ok(Ok(listing)) => {
                        summary.contacted = true;
                        self.backoff.successful_operation();
                        listing
                    }
                    Ok(Err(SyncOperationError::RetainCandidate(code))) => {
                        summary.diagnostic = Some(code);
                        continue;
                    }
                    Ok(Err(error)) => {
                        drop(activity);
                        return self.end_sweep(summary, error);
                    }
                };
                let reconciliation = Reconciliation {
                    root: root_manifest,
                    day: day_manifest,
                    listing: listing.clone(),
                };
                if !reconciliation.proves(
                    candidate.day(),
                    candidate.segment(),
                    &authoritative_key,
                    &local_files,
                ) {
                    summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                    continue;
                }
                reconciliations_by_day.insert(candidate.day().to_owned(), reconciliation);
                batch_fresh_listing_days.insert(candidate.day().to_owned());
                summary.custodied += 1;
                self.facts.pending_segments = self.facts.pending_segments.saturating_sub(1);
                match delete_custodied_segment(
                    &self.captures_root,
                    &self.stream,
                    today,
                    self.retention_days,
                    candidate,
                    &authoritative_key,
                    &listing,
                    self.instrumentation.clone(),
                    Arc::clone(&self.retention_fence),
                )
                .await
                {
                    RetentionOutcome::Deleted => {
                        self.inventories.remove(candidate);
                    }
                    RetentionOutcome::Retained => {
                        summary.diagnostic = Some(DiagnosticCode::LocalSegmentInvalid);
                    }
                    RetentionOutcome::Disabled | RetentionOutcome::Ineligible => {}
                }
            }
            if batch_index + 1 != candidates.chunks(CANDIDATES_PER_BATCH).len() {
                self.yield_between_batches().await;
                if shutdown_requested(&mut shutdown) {
                    return self.cancelled_sweep_with_activity(summary, activity).await;
                }
            }
        }
        drop(activity);
        summary
    }

    async fn yield_between_batches(&self) {
        tokio::task::yield_now().await;
        self.instrumentation.batch_yield();
    }

    async fn inventory_for(
        &mut self,
        candidate: &SegmentCandidate,
        paths: &[PathBuf],
    ) -> Result<Vec<LocalFile>, ()> {
        let paths = paths.to_vec();
        let names = paths
            .iter()
            .map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(())?;
        let identity_paths = paths.clone();
        let identities = tokio::task::spawn_blocking(move || file_identities(&identity_paths))
            .await
            .ok()
            .flatten()
            .ok_or(())?;
        if let Some(cached) = self.inventories.get(candidate)
            && cached.names == names
            && cached.identities == identities
        {
            return Ok(cached.inventory.clone());
        }
        let inventory = inventory_files(paths, Some(self.instrumentation.clone()))
            .await
            .map_err(|_| ())?;
        // Timestamps are an invalidation heuristic, not byte proof. A missed invalidation can
        // only skip an upload: retention always re-inventories from disk before deletion.
        self.inventories.insert(
            candidate.clone(),
            CachedInventory {
                names,
                identities,
                inventory: inventory.clone(),
            },
        );
        Ok(inventory)
    }

    async fn cancelled_sweep(&mut self, summary: SyncSweepSummary) -> SyncSweepSummary {
        self.cancelled_sweep_with_activity(summary, None).await
    }

    async fn cancelled_sweep_with_activity(
        &mut self,
        mut summary: SyncSweepSummary,
        activity: Option<ActivityGuard>,
    ) -> SyncSweepSummary {
        drop(activity);
        summary.cancelled = true;
        self.facts.sync_in_progress = false;
        self.write_health().await;
        summary
    }

    fn update_facts(&mut self, summary: &SyncSweepSummary) {
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

    async fn write_health(&self) {
        if let Some(health) = &self.health {
            let _ = health
                .write(&self.facts, self.clock.wall_now().unix_timestamp())
                .await;
        }
    }

    fn end_sweep(
        &mut self,
        mut summary: SyncSweepSummary,
        error: SyncOperationError,
    ) -> SyncSweepSummary {
        let failure = match error {
            SyncOperationError::RetainCandidate(code) => {
                summary.diagnostic = Some(code);
                SyncFailureClass::Contract
            }
            SyncOperationError::EndSweep(failure) => failure,
            SyncOperationError::EndSweepDiagnostic(failure, code) => {
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

impl SyncJournal for JournalSession {
    fn upload<'a>(
        &'a mut self,
        candidate: &'a SegmentCandidate,
        files: Vec<PathBuf>,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UploadResult, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.journal
                .ingest_upload(candidate.day(), candidate.segment(), files, source)
                .await
                .map_err(map_journal_error)
        })
    }

    fn manifest<'a>(
        &'a mut self,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestManifest, SyncOperationError>> + Send + 'a>> {
        Box::pin(async move {
            self.journal
                .ingest_manifest(source)
                .await
                .map_err(map_journal_error)
        })
    }

    fn manifest_day<'a>(
        &'a mut self,
        day: &'a str,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<IngestDayManifest, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.journal
                .ingest_manifest_day(day, source)
                .await
                .map_err(map_journal_error)
        })
    }

    fn segments<'a>(
        &'a mut self,
        day: &'a str,
        source: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SegmentsEnvelope, SyncOperationError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.journal
                .ingest_segments(day, source)
                .await
                .map_err(map_journal_error)
        })
    }
}

pub struct SyncTask {
    pub config_root: PathBuf,
    pub data_root: PathBuf,
    pub config: RuntimeConfig,
    pub hostname: String,
    pub clock: Arc<dyn Clock>,
    pub wake: SyncWake,
    pub activity: watch::Sender<SyncActivity>,
    pub health: HealthWriter,
    pub retention_fence: Arc<RetentionFence>,
}

impl SyncTask {
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), DiagnosticCode> {
        let SyncTask {
            config_root,
            data_root,
            config,
            hostname,
            clock,
            wake,
            activity,
            health,
            retention_fence,
        } = self;
        let load_root = config_root.clone();
        let loaded = tokio::task::spawn_blocking(move || load_credential(&load_root))
            .await
            .map_err(|_| DiagnosticCode::PrivateStateIo)?;
        let credential = match loaded {
            Ok(Some(credential)) => credential,
            Ok(None) => {
                let facts = SyncFacts::default();
                refresh_waiting_health(&health, &facts, clock.as_ref(), &mut shutdown).await;
                return Ok(());
            }
            Err(code) => {
                let mut facts = SyncFacts::default();
                facts.failed(code);
                refresh_waiting_health(&health, &facts, clock.as_ref(), &mut shutdown).await;
                return Ok(());
            }
        };
        let expected_stream = default_stream(&hostname)
            .ok()
            .and_then(|stream| derive_component(&stream).ok())
            .ok_or(DiagnosticCode::ConfiguredStreamMismatch)?;
        if config.stream != expected_stream {
            let mut facts = SyncFacts::default();
            facts.failed(DiagnosticCode::ConfiguredStreamMismatch);
            refresh_waiting_health(&health, &facts, clock.as_ref(), &mut shutdown).await;
            return Ok(());
        }
        let mut reconnect = Backoff::new();
        let mut reconnect_facts = SyncFacts {
            paired: true,
            ..SyncFacts::default()
        };
        loop {
            if shutdown_requested(&mut shutdown) {
                return Ok(());
            }
            let mut journal =
                match JournalSession::start(credential.clone(), config_root.clone()).await {
                    Ok(journal) => journal,
                    Err(code) => {
                        reconnect_facts.failed(code);
                        let _ = health
                            .write(&reconnect_facts, clock.wall_now().unix_timestamp())
                            .await;
                        reconnect.failed_operation();
                        let deadline = reconnect
                            .deadline()
                            .expect("failed reconnect has a retry deadline");
                        if !wait_for_retry_or_shutdown(&mut shutdown, deadline).await {
                            return Ok(());
                        }
                        reconnect.deadline_reached();
                        continue;
                    }
                };
            let mut scheduler = SyncScheduler::new(
                data_root.join("captures"),
                config.stream,
                config.source,
                config.cache_retention_days,
                clock,
                wake,
            )
            .with_observability(activity, health)
            .with_retention_fence(retention_fence);
            scheduler.facts.paired = true;
            scheduler
                .run_with_shutdown(&mut journal, shutdown.clone())
                .await;
            if let Err(code) = journal.shutdown().await {
                scheduler.facts.failed(code);
                scheduler.write_health().await;
                return Err(code);
            }
            return Ok(());
        }
    }
}

async fn wait_for_retry_or_shutdown(
    shutdown: &mut watch::Receiver<bool>,
    deadline: Instant,
) -> bool {
    tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => false,
        () = tokio::time::sleep_until(deadline) => true,
    }
}

async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

fn shutdown_requested(receiver: &mut watch::Receiver<bool>) -> bool {
    *receiver.borrow_and_update()
}

async fn cancellable<T>(
    shutdown: &mut watch::Receiver<bool>,
    operation: impl Future<Output = T>,
) -> Result<T, ()> {
    tokio::select! {
        biased;
        () = wait_for_shutdown(shutdown) => Err(()),
        value = operation => Ok(value),
    }
}

async fn refresh_waiting_health(
    health: &HealthWriter,
    facts: &SyncFacts,
    clock: &dyn Clock,
    shutdown: &mut watch::Receiver<bool>,
) {
    let mut heartbeat = tokio::time::interval(HEALTH_REFRESH_INTERVAL);
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

fn map_journal_error(error: JournalError) -> SyncOperationError {
    let diagnostic = error.diagnostic();
    match diagnostic {
        DiagnosticCode::RequestTooLarge | DiagnosticCode::LocalSegmentInvalid => {
            SyncOperationError::RetainCandidate(diagnostic)
        }
        DiagnosticCode::JournalTimeout => SyncOperationError::EndSweepDiagnostic(
            SyncFailureClass::Timeout,
            DiagnosticCode::JournalTimeout,
        ),
        DiagnosticCode::JournalRejected => match error.reason_code() {
            Some(
                JournalReasonCode::LinkedDeviceRequired
                | JournalReasonCode::ProtocolVersionLegacy
                | JournalReasonCode::ProtocolVersionFuture,
            ) => SyncOperationError::RetainCandidate(DiagnosticCode::JournalRejected),
            Some(
                JournalReasonCode::AuthKeyInvalid
                | JournalReasonCode::AuthRequired
                | JournalReasonCode::PlRevoked,
            ) => SyncOperationError::EndSweepDiagnostic(
                SyncFailureClass::Auth,
                DiagnosticCode::JournalRejected,
            ),
            Some(
                JournalReasonCode::IngestContractInvalid
                | JournalReasonCode::IngestNoFiles
                | JournalReasonCode::IngestSidecarConflict,
            ) => SyncOperationError::RetainCandidate(DiagnosticCode::LocalSegmentInvalid),
            Some(JournalReasonCode::IngestStorageFailed) => SyncOperationError::EndSweepDiagnostic(
                SyncFailureClass::Direct,
                DiagnosticCode::JournalRejected,
            ),
            _ => SyncOperationError::EndSweepDiagnostic(
                SyncFailureClass::Contract,
                DiagnosticCode::JournalRejected,
            ),
        },
        DiagnosticCode::JournalContractInvalid
        | DiagnosticCode::JournalResponseTooLarge
        | DiagnosticCode::ConfiguredStreamMismatch
        | DiagnosticCode::PrivateStateInvalid
        | DiagnosticCode::PrivateStateIo => {
            SyncOperationError::EndSweepDiagnostic(SyncFailureClass::Contract, diagnostic)
        }
        _ => SyncOperationError::EndSweepDiagnostic(SyncFailureClass::Direct, diagnostic),
    }
}

#[cfg(test)]
fn map_diagnostic(code: DiagnosticCode) -> SyncOperationError {
    match code {
        DiagnosticCode::JournalTimeout => SyncOperationError::EndSweepDiagnostic(
            SyncFailureClass::Timeout,
            DiagnosticCode::JournalTimeout,
        ),
        DiagnosticCode::JournalContractInvalid
        | DiagnosticCode::JournalResponseTooLarge
        | DiagnosticCode::ConfiguredStreamMismatch
        | DiagnosticCode::PrivateStateInvalid
        | DiagnosticCode::PrivateStateIo => {
            SyncOperationError::EndSweepDiagnostic(SyncFailureClass::Contract, code)
        }
        _ => SyncOperationError::EndSweepDiagnostic(SyncFailureClass::Direct, code),
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

enum ResolvedSegmentFiles {
    Found(Vec<PathBuf>),
    Missing,
    Invalid,
}

fn resolve_segment_files(
    captures_root: &Path,
    candidate: &SegmentCandidate,
) -> ResolvedSegmentFiles {
    let Ok(root_metadata) = fs::symlink_metadata(captures_root) else {
        return ResolvedSegmentFiles::Missing;
    };
    if !is_plain_directory(&root_metadata) || parse_day(candidate.day()).is_none() {
        return ResolvedSegmentFiles::Invalid;
    }
    let Some(day) = exact_component(candidate.day()) else {
        return ResolvedSegmentFiles::Invalid;
    };
    let Some(stream) = exact_component(candidate.stream()) else {
        return ResolvedSegmentFiles::Invalid;
    };
    let Some(segment) = exact_component(candidate.segment()) else {
        return ResolvedSegmentFiles::Invalid;
    };
    if !valid_segment_name(candidate.segment()) {
        return ResolvedSegmentFiles::Invalid;
    }
    let Ok(day_path) = day.join_checked(captures_root) else {
        return ResolvedSegmentFiles::Invalid;
    };
    let Ok(stream_path) = stream.join_checked(&day_path) else {
        return ResolvedSegmentFiles::Invalid;
    };
    let Ok(segment_path) = segment.join_checked(&stream_path) else {
        return ResolvedSegmentFiles::Invalid;
    };
    for directory in [&day_path, &stream_path, &segment_path] {
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ResolvedSegmentFiles::Missing;
            }
            Err(_) => return ResolvedSegmentFiles::Invalid,
        };
        if !is_plain_directory(&metadata) {
            return ResolvedSegmentFiles::Invalid;
        }
    }

    let mut files = Vec::new();
    let entries = match fs::read_dir(&segment_path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ResolvedSegmentFiles::Missing;
        }
        Err(_) => return ResolvedSegmentFiles::Invalid,
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return ResolvedSegmentFiles::Invalid;
        };
        let Ok(name) = entry.file_name().into_string() else {
            return ResolvedSegmentFiles::Invalid;
        };
        if !valid_capture_filename(&name) || !plain_regular_entry(&entry) {
            return ResolvedSegmentFiles::Invalid;
        }
        let Some(component) = exact_component(&name) else {
            return ResolvedSegmentFiles::Invalid;
        };
        let Ok(path) = component.join_checked(&segment_path) else {
            return ResolvedSegmentFiles::Invalid;
        };
        if path != entry.path() {
            return ResolvedSegmentFiles::Invalid;
        }
        files.push(path);
    }
    if files.is_empty() {
        return ResolvedSegmentFiles::Invalid;
    }
    files.sort();
    ResolvedSegmentFiles::Found(files)
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
                mtime: metadata.mtime(),
                mtime_nsec: metadata.mtime_nsec(),
                ctime: metadata.ctime(),
                ctime_nsec: metadata.ctime_nsec(),
            })
        })
        .collect()
}

fn custodied_digest<'a>(inventory: &'a [LocalFile], name: &str) -> Option<&'a str> {
    let mut matches = inventory.iter().filter(|file| file.name == name);
    let file = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(file.sha256.as_str())
    }
}

fn delete_revalidated_segment(
    captures_root: &Path,
    candidate: &SegmentCandidate,
    expected_paths: &[PathBuf],
    expected_identities: &[FileIdentity],
    expected_inventory: &[LocalFile],
    delete_hook: Option<&(dyn Fn(usize) + Send + Sync)>,
) -> bool {
    let ResolvedSegmentFiles::Found(paths) = resolve_segment_files(captures_root, candidate) else {
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
        let mut current = open_regular_readonly_at(&segment_directory, &expected.name, path);
        let current_matches = current
            .as_ref()
            .ok()
            .and_then(|file| file.metadata().ok())
            .is_some_and(|metadata| metadata_matches(&metadata, expected));
        let current_proves_custody = current_matches
            && custodied_digest(expected_inventory, &expected.name).is_some_and(
                |expected_digest| {
                    current.as_mut().ok().is_some_and(|file| {
                        stream_sha256_hex(file).is_ok_and(|digest| digest == expected_digest)
                    })
                },
            );
        if !current_proves_custody
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
                    SyncOperationError::EndSweepDiagnostic(
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
