// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING};
use reqwest::multipart::{Form, Part};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spl_core::frame::RECOMMENDED_CHUNK;
use spl_core::mux::UPLOAD_BODY_STAGE_CAPACITY;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::health::DiagnosticCode;
use crate::name::derive_component;
use crate::private_link::{
    MAX_REQUEST_BODY_BYTES, ObserverState, PrivateLinkBridge, contains_invalid_header_value,
};
use crate::storage::open_regular_readonly;
use crate::sync::SyncInstrumentation;

const REGISTER_PATH: &str = "/app/observer/register";
const SEGMENTS_PATH: &str = "/app/observer/ingest/segments";
const EVENT_PATH: &str = "/app/observer/ingest/event";
const LOOPBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FILE_STAGE_CAPACITY: usize = UPLOAD_BODY_STAGE_CAPACITY / RECOMMENDED_CHUNK;
const MAX_RESPONSE_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct RegistrationDescriptor {
    pub platform: String,
    pub hostname: String,
}

#[derive(Serialize)]
struct RegistrationRequest<'a> {
    platform: &'a str,
    hostname: &'a str,
    stream_type: &'static str,
    version: &'static str,
}

#[derive(Deserialize)]
struct RegistrationResponse {
    key: String,
    prefix: String,
    name: String,
    ingest_url: String,
    protocol_version: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalStatusClass {
    Client,
    Server,
    Unexpected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalReasonCode {
    AuthKeyInvalid,
    AuthRequired,
    FeatureUnavailable,
    IngestContractInvalid,
    IngestNoFiles,
    IngestSidecarConflict,
    IngestStorageFailed,
    InvalidDay,
    InvalidSegmentOrStream,
    LocalRequestOnly,
    MissingRequiredField,
    PlRevoked,
    SettingsOperationFailed,
}

impl JournalReasonCode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auth_key_invalid" => Some(Self::AuthKeyInvalid),
            "auth_required" => Some(Self::AuthRequired),
            "feature_unavailable" => Some(Self::FeatureUnavailable),
            "ingest_contract_invalid" => Some(Self::IngestContractInvalid),
            "ingest_no_files" => Some(Self::IngestNoFiles),
            "ingest_sidecar_conflict" => Some(Self::IngestSidecarConflict),
            "ingest_storage_failed" => Some(Self::IngestStorageFailed),
            "invalid_day" => Some(Self::InvalidDay),
            "invalid_segment_or_stream" => Some(Self::InvalidSegmentOrStream),
            "local_request_only" => Some(Self::LocalRequestOnly),
            "missing_required_field" => Some(Self::MissingRequiredField),
            "pl_revoked" => Some(Self::PlRevoked),
            "settings_operation_failed" => Some(Self::SettingsOperationFailed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalError {
    diagnostic: DiagnosticCode,
    status_class: Option<JournalStatusClass>,
    reason_code: Option<JournalReasonCode>,
}

impl JournalError {
    pub fn diagnostic(self) -> DiagnosticCode {
        self.diagnostic
    }

    pub fn status_class(self) -> Option<JournalStatusClass> {
        self.status_class
    }

    pub fn reason_code(self) -> Option<JournalReasonCode> {
        self.reason_code
    }

    fn local(diagnostic: DiagnosticCode) -> Self {
        Self {
            diagnostic,
            status_class: None,
            reason_code: None,
        }
    }
}

impl From<DiagnosticCode> for JournalError {
    fn from(diagnostic: DiagnosticCode) -> Self {
        Self::local(diagnostic)
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.diagnostic.message())
    }
}

impl std::error::Error for JournalError {}

#[derive(Deserialize)]
struct ErrorResponse {
    #[serde(rename = "error")]
    _error: String,
    reason_code: String,
    #[serde(rename = "detail")]
    _detail: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum UploadStatus {
    Ok,
    Duplicate,
    Collision,
    Conflict,
    Failed,
}

#[derive(Deserialize)]
struct UploadResponse {
    status: UploadStatus,
    #[serde(default)]
    segment: Option<String>,
    #[serde(default)]
    existing_segment: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadResult {
    pub status: UploadStatus,
    pub authoritative_key: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum ListingFileStatus {
    Present,
    Missing,
    Processed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SegmentFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub status: ListingFileStatus,
    #[serde(default)]
    pub submitted_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SegmentItem {
    pub key: String,
    pub observed: bool,
    pub files: Vec<SegmentFile>,
    #[serde(default)]
    pub original_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SegmentsEnvelope {
    pub items: Vec<SegmentItem>,
    pub total: usize,
    pub protocol_version: u64,
}

#[derive(Deserialize)]
struct EventResponse {
    status: String,
}

struct PreparedFile {
    descriptor: LocalFile,
    file: File,
}

#[derive(Default)]
struct ProducerStart {
    started: Mutex<bool>,
    ready: Condvar,
}

impl ProducerStart {
    fn release(&self) {
        *lock(&self.started) = true;
        self.ready.notify_one();
    }

    fn wait(&self) {
        let mut started = lock(&self.started);
        while !*started {
            started = match self.ready.wait(started) {
                Ok(started) => started,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }
}

#[derive(Default)]
struct UploadStageState {
    chunks: usize,
    bytes: usize,
}

#[derive(Default)]
struct UploadStage {
    state: Mutex<UploadStageState>,
    available: Condvar,
    high_water_bytes: AtomicUsize,
}

impl UploadStage {
    fn reserve(self: &Arc<Self>) -> UploadReservation {
        let mut state = lock(&self.state);
        while state.chunks >= FILE_STAGE_CAPACITY {
            state = match self.available.wait(state) {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
        state.chunks += 1;
        UploadReservation {
            stage: Arc::clone(self),
            bytes: 0,
        }
    }

    fn high_water_bytes(&self) -> usize {
        self.high_water_bytes.load(Ordering::Relaxed)
    }
}

struct UploadReservation {
    stage: Arc<UploadStage>,
    bytes: usize,
}

impl UploadReservation {
    fn record_bytes(&mut self, bytes: usize) {
        self.bytes = bytes;
        let mut state = lock(&self.stage.state);
        state.bytes += bytes;
        self.stage
            .high_water_bytes
            .fetch_max(state.bytes, Ordering::Relaxed);
    }
}

impl Drop for UploadReservation {
    fn drop(&mut self) {
        let mut state = lock(&self.stage.state);
        state.chunks -= 1;
        state.bytes -= self.bytes;
        drop(state);
        self.stage.available.notify_one();
    }
}

struct StagedChunk {
    bytes: Vec<u8>,
    reservation: UploadReservation,
}

struct FileChunkStream {
    receiver: mpsc::Receiver<Result<StagedChunk, io::Error>>,
    start: Option<Arc<ProducerStart>>,
    active: Option<UploadReservation>,
}

impl Stream for FileChunkStream {
    type Item = Result<Vec<u8>, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.active.take();
        if let Some(start) = self.start.take() {
            start.release();
        }
        match self.receiver.poll_recv(context) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.active = Some(chunk.reservation);
                Poll::Ready(Some(Ok(chunk.bytes)))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for FileChunkStream {
    fn drop(&mut self) {
        if let Some(start) = self.start.take() {
            start.release();
        }
    }
}

#[derive(Clone)]
pub struct JournalClient {
    client: reqwest::Client,
    origin: Url,
    upload_stage: Arc<UploadStage>,
}

impl JournalClient {
    pub async fn bootstrap(bridge: &PrivateLinkBridge) -> Result<Self, DiagnosticCode> {
        let origin =
            Url::parse(&bridge.loopback_origin()).map_err(|_| DiagnosticCode::BridgeUnavailable)?;
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(LOOPBACK_CONNECT_TIMEOUT)
            .build()
            .map_err(|_| DiagnosticCode::BridgeUnavailable)?;
        let bootstrap_url = bridge.bootstrap_url()?;
        let response = client
            .get(bootstrap_url)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| request_diagnostic(&error, DiagnosticCode::JournalUnavailable))?;
        if response.status() != StatusCode::FOUND {
            return Err(DiagnosticCode::JournalContractInvalid);
        }
        Ok(Self {
            client,
            origin,
            upload_stage: Arc::new(UploadStage::default()),
        })
    }

    pub fn upload_stage_high_water_bytes(&self) -> usize {
        self.upload_stage.high_water_bytes()
    }

    pub fn request(
        &self,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, DiagnosticCode> {
        let url = confine_path(&self.origin, path)?;
        Ok(self.client.request(method, url).timeout(REQUEST_TIMEOUT))
    }

    pub async fn register(
        &self,
        descriptor: &RegistrationDescriptor,
        credential_instance_id: &str,
        expected_name: &str,
    ) -> Result<ObserverState, JournalError> {
        let request = RegistrationRequest {
            platform: &descriptor.platform,
            hostname: &descriptor.hostname,
            stream_type: "tmux",
            version: env!("CARGO_PKG_VERSION"),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
        let response = self
            .request(Method::POST, REGISTER_PATH)?
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| {
                JournalError::local(request_diagnostic(
                    &error,
                    DiagnosticCode::RegistrationFailed,
                ))
            })?;
        let status = response.status();
        let body = collect_response_body(response).await?;
        if status != StatusCode::OK {
            return Err(classify_error_response(status.as_u16(), &body));
        }
        decode_registration_response(&body, credential_instance_id, expected_name)
    }

    pub fn validate_observer(&self, observer: &ObserverState) -> Result<(), DiagnosticCode> {
        let response = RegistrationResponse {
            key: observer.key.clone(),
            prefix: observer.prefix.clone(),
            name: observer.name.clone(),
            ingest_url: observer.ingest_url.clone(),
            protocol_version: observer.protocol_version,
        };
        validate_registration(&response)?;
        confine_path(&self.origin, &observer.ingest_url)?;
        Ok(())
    }

    pub async fn ingest_upload(
        &self,
        ingest_path: &str,
        day: &str,
        segment: &str,
        paths: Vec<PathBuf>,
    ) -> Result<UploadResult, JournalError> {
        if !valid_day(day) || !valid_component(segment) || paths.is_empty() {
            return Err(JournalError::local(DiagnosticCode::LocalSegmentInvalid));
        }
        let prepared = tokio::task::spawn_blocking(move || prepare_files(paths, None))
            .await
            .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))??;
        let mut producers = Vec::with_capacity(prepared.len());
        let mut producer_starts = Vec::with_capacity(prepared.len());
        let mut form = Form::new()
            .text("day", day.to_owned())
            .text("segment", segment.to_owned());
        for prepared_file in prepared {
            let PreparedFile { descriptor, file } = prepared_file;
            let (sender, receiver) = mpsc::channel(FILE_STAGE_CAPACITY);
            let start = Arc::new(ProducerStart::default());
            let body = reqwest::Body::wrap_stream(FileChunkStream {
                receiver,
                start: Some(Arc::clone(&start)),
                active: None,
            });
            let part = Part::stream_with_length(body, descriptor.size)
                .file_name(descriptor.name.clone())
                .mime_str("application/octet-stream")
                .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
            form = form.part("files", part);
            producer_starts.push(Arc::clone(&start));
            producers.push((file, sender, start));
        }

        let request = self
            .request(Method::POST, ingest_path)?
            .multipart(form)
            .build()
            .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
        validate_multipart_request(&request)?;
        let producer_tasks = producers
            .into_iter()
            .map(|(file, sender, start)| {
                spawn_file_producer(file, sender, start, Arc::clone(&self.upload_stage))
            })
            .collect::<Vec<_>>();
        let response = self.client.execute(request).await.map_err(|error| {
            JournalError::local(request_diagnostic(
                &error,
                DiagnosticCode::JournalUnavailable,
            ))
        });
        for start in producer_starts {
            start.release();
        }
        for task in producer_tasks {
            let _ = task.await;
        }
        let response = response?;
        let status = response.status();
        let body = collect_response_body(response).await?;
        if status != StatusCode::OK {
            return Err(classify_error_response(status.as_u16(), &body));
        }
        decode_upload_response(&body)
    }

    pub async fn ingest_segments(&self, day: &str) -> Result<SegmentsEnvelope, JournalError> {
        if !valid_day(day) {
            return Err(JournalError::local(DiagnosticCode::LocalSegmentInvalid));
        }
        let path = format!("{SEGMENTS_PATH}/{day}");
        let response = self
            .request(Method::GET, &path)?
            .send()
            .await
            .map_err(|error| {
                JournalError::local(request_diagnostic(
                    &error,
                    DiagnosticCode::JournalUnavailable,
                ))
            })?;
        let status = response.status();
        let body = collect_response_body(response).await?;
        if status != StatusCode::OK {
            return Err(classify_error_response(status.as_u16(), &body));
        }
        decode_segments_response(&body)
    }

    pub async fn ingest_event(
        &self,
        tract: &str,
        event: &str,
        mut fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), JournalError> {
        if tract.is_empty() || event.is_empty() {
            return Err(JournalError::local(DiagnosticCode::LocalSegmentInvalid));
        }
        fields.insert(
            "tract".to_owned(),
            serde_json::Value::String(tract.to_owned()),
        );
        fields.insert(
            "event".to_owned(),
            serde_json::Value::String(event.to_owned()),
        );
        let body = serde_json::to_vec(&fields)
            .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
        let response = self
            .request(Method::POST, EVENT_PATH)?
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| {
                JournalError::local(request_diagnostic(
                    &error,
                    DiagnosticCode::JournalUnavailable,
                ))
            })?;
        let status = response.status();
        let body = collect_response_body(response).await?;
        if status != StatusCode::OK {
            return Err(classify_error_response(status.as_u16(), &body));
        }
        decode_event_response(&body)
    }
}

async fn collect_response_body(mut response: reqwest::Response) -> Result<Vec<u8>, JournalError> {
    let declared_length = response.content_length().or_else(|| {
        response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    });
    if declared_length.is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64) {
        return Err(JournalError::local(DiagnosticCode::JournalResponseTooLarge));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        JournalError::local(request_diagnostic(
            &error,
            DiagnosticCode::JournalContractInvalid,
        ))
    })? {
        let Some(length) = body.len().checked_add(chunk.len()) else {
            return Err(JournalError::local(DiagnosticCode::JournalResponseTooLarge));
        };
        if length > MAX_RESPONSE_BODY_BYTES {
            return Err(JournalError::local(DiagnosticCode::JournalResponseTooLarge));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub fn decode_registration_response(
    body: &[u8],
    credential_instance_id: &str,
    expected_name: &str,
) -> Result<ObserverState, JournalError> {
    let response = serde_json::from_slice::<RegistrationResponse>(body)
        .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
    validate_registration(&response).map_err(JournalError::local)?;
    // Journal registration-name behavior pinned at dbe1b0fb316fe127fa8ea55dd2aeb605546c5351.
    if response.name != expected_name {
        return Err(JournalError::local(
            DiagnosticCode::RegistrationNameMismatch,
        ));
    }
    Ok(ObserverState {
        credential_instance_id: credential_instance_id.to_owned(),
        key: response.key,
        prefix: response.prefix,
        name: response.name,
        ingest_url: response.ingest_url,
        protocol_version: response.protocol_version,
    })
}

pub fn decode_upload_response(body: &[u8]) -> Result<UploadResult, JournalError> {
    let response = serde_json::from_slice::<UploadResponse>(body)
        .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
    let authoritative_key = match response.status {
        UploadStatus::Ok | UploadStatus::Collision => required_key(response.segment)?,
        UploadStatus::Duplicate => required_key(response.existing_segment)?,
        UploadStatus::Conflict | UploadStatus::Failed => None,
    };
    Ok(UploadResult {
        status: response.status,
        authoritative_key,
    })
}

pub fn decode_segments_response(body: &[u8]) -> Result<SegmentsEnvelope, JournalError> {
    let response = serde_json::from_slice::<SegmentsEnvelope>(body)
        .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
    if response.protocol_version != 2
        || response.total != response.items.len()
        || response
            .items
            .iter()
            .any(|item| item.key.is_empty() || item.files.iter().any(|file| file.name.is_empty()))
    {
        return Err(JournalError::local(DiagnosticCode::JournalContractInvalid));
    }
    Ok(response)
}

pub fn decode_event_response(body: &[u8]) -> Result<(), JournalError> {
    let response = serde_json::from_slice::<EventResponse>(body)
        .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
    if response.status != "ok" {
        return Err(JournalError::local(DiagnosticCode::JournalContractInvalid));
    }
    Ok(())
}

pub fn classify_error_response(status: u16, body: &[u8]) -> JournalError {
    let status_class = match status {
        400..=499 => JournalStatusClass::Client,
        500..=599 => JournalStatusClass::Server,
        _ => JournalStatusClass::Unexpected,
    };
    let reason_code = serde_json::from_slice::<ErrorResponse>(body)
        .ok()
        .and_then(|response| JournalReasonCode::parse(&response.reason_code));
    JournalError {
        diagnostic: DiagnosticCode::JournalRejected,
        status_class: Some(status_class),
        reason_code,
    }
}

pub async fn inventory_files(
    paths: Vec<PathBuf>,
    instrumentation: Option<SyncInstrumentation>,
) -> Result<Vec<LocalFile>, JournalError> {
    let prepared =
        tokio::task::spawn_blocking(move || prepare_files(paths, instrumentation.as_ref()))
            .await
            .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))??;
    Ok(prepared
        .into_iter()
        .map(|prepared| prepared.descriptor)
        .collect())
}

fn request_diagnostic(error: &reqwest::Error, fallback: DiagnosticCode) -> DiagnosticCode {
    if error.is_timeout() {
        DiagnosticCode::JournalTimeout
    } else {
        fallback
    }
}

fn validate_registration(response: &RegistrationResponse) -> Result<(), DiagnosticCode> {
    if response.key.is_empty()
        || contains_invalid_header_value(&response.key)
        || response.prefix.is_empty()
        || response.name.is_empty()
        || response.protocol_version != 2
    {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let origin =
        Url::parse("http://127.0.0.1").map_err(|_| DiagnosticCode::JournalContractInvalid)?;
    confine_path(&origin, &response.ingest_url)?;
    Ok(())
}

fn confine_path(origin: &Url, path: &str) -> Result<Url, DiagnosticCode> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path
            .chars()
            .any(|character| matches!(character, '?' | '#' | '\\' | '\r' | '\n'))
    {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    let url = origin
        .join(path)
        .map_err(|_| DiagnosticCode::JournalContractInvalid)?;
    if url.scheme() != origin.scheme()
        || url.host_str() != origin.host_str()
        || url.port_or_known_default() != origin.port_or_known_default()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(DiagnosticCode::JournalContractInvalid);
    }
    Ok(url)
}

fn prepare_files(
    paths: Vec<PathBuf>,
    instrumentation: Option<&SyncInstrumentation>,
) -> Result<Vec<PreparedFile>, JournalError> {
    if paths.is_empty() {
        return Err(JournalError::local(DiagnosticCode::LocalSegmentInvalid));
    }
    paths
        .into_iter()
        .map(|path| prepare_file(&path, instrumentation))
        .collect()
}

fn prepare_file(
    path: &Path,
    instrumentation: Option<&SyncInstrumentation>,
) -> Result<PreparedFile, JournalError> {
    let parent = path
        .parent()
        .ok_or_else(|| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
    let derived = derive_component(name)
        .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
    if derived.as_str() != name
        || derived
            .join_checked(parent)
            .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?
            != path
    {
        return Err(JournalError::local(DiagnosticCode::LocalSegmentInvalid));
    }
    let mut file = open_regular_readonly(path)
        .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
    let size = file
        .metadata()
        .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?
        .len();
    let mut digest = Sha256::new();
    let mut buffer = [0u8; RECOMMENDED_CHUNK];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
    if let Some(instrumentation) = instrumentation {
        instrumentation.hashed_file(size);
    }
    Ok(PreparedFile {
        descriptor: LocalFile {
            name: name.to_owned(),
            size,
            sha256: format!("{:x}", digest.finalize()),
        },
        file,
    })
}

fn spawn_file_producer(
    mut file: File,
    sender: mpsc::Sender<Result<StagedChunk, io::Error>>,
    start: Arc<ProducerStart>,
    stage: Arc<UploadStage>,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        start.wait();
        let sender = sender;
        loop {
            let mut reservation = stage.reserve();
            let mut chunk = vec![0u8; RECOMMENDED_CHUNK];
            let count = match file.read(&mut chunk) {
                Ok(count) => count,
                Err(error) => {
                    let _ = sender.blocking_send(Err(io::Error::new(
                        error.kind(),
                        "local file stream failed",
                    )));
                    return;
                }
            };
            if count == 0 {
                return;
            }
            chunk.truncate(count);
            reservation.record_bytes(count);
            if sender
                .blocking_send(Ok(StagedChunk {
                    bytes: chunk,
                    reservation,
                }))
                .is_err()
            {
                return;
            }
        }
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn validate_multipart_request(request: &reqwest::Request) -> Result<u64, JournalError> {
    if request.headers().contains_key(TRANSFER_ENCODING) {
        return Err(JournalError::local(DiagnosticCode::JournalContractInvalid));
    }
    let mut lengths = request.headers().get_all(CONTENT_LENGTH).iter();
    let length = lengths
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
    if lengths.next().is_some() {
        return Err(JournalError::local(DiagnosticCode::JournalContractInvalid));
    }
    if length > MAX_REQUEST_BODY_BYTES as u64 {
        return Err(JournalError::local(DiagnosticCode::RequestTooLarge));
    }
    Ok(length)
}

fn required_key(value: Option<String>) -> Result<Option<String>, JournalError> {
    match value {
        Some(value) if valid_component(&value) => Ok(Some(value)),
        _ => Err(JournalError::local(DiagnosticCode::JournalContractInvalid)),
    }
}

fn valid_day(day: &str) -> bool {
    day.len() == 8 && day.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_component(value: &str) -> bool {
    derive_component(value).is_ok_and(|derived| derived.as_str() == value)
}
