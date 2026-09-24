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
use reqwest::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
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
use crate::private_link::{MAX_REQUEST_BODY_BYTES, PrivateLinkBridge};
use crate::storage::open_regular_readonly;
use crate::sync::SyncInstrumentation;

pub const INGEST_PATH: &str = "/app/devices/ingest";
pub const SYSTEM_STATUS_PATH: &str = "/api/system/status";
pub const CLIENTS_SELF_PATH: &str = "/app/network/api/clients/self";
pub const RELAY_ACCESS_PATH: &str = "/app/network/api/relay/access";
pub const OPTIONAL_JOB_TIMEOUT: Duration = Duration::from_secs(15);
pub const OPTIONAL_RESPONSE_BODY_BYTES: usize = 64 * 1024;
const SYSTEM_STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const LOOPBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FILE_STAGE_CAPACITY: usize = UPLOAD_BODY_STAGE_CAPACITY / RECOMMENDED_CHUNK;
const MAX_RESPONSE_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MULTIPART_PART_BYTES: u64 = 64 * 1024 * 1024;

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
    ContentConflict,
    FeatureUnavailable,
    IngestContractInvalid,
    IngestNoFiles,
    IngestSidecarConflict,
    IngestStorageFailed,
    InvalidDay,
    InvalidSegmentOrStream,
    LocalRequestOnly,
    MissingRequiredField,
    LinkedDeviceRequired,
    PlRevoked,
    ProtocolVersionFuture,
    ProtocolVersionLegacy,
    SegmentRemoved,
    SettingsOperationFailed,
}

impl JournalReasonCode {
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::AuthKeyInvalid => "auth_key_invalid",
            Self::AuthRequired => "auth_required",
            Self::ContentConflict => "content_conflict",
            Self::FeatureUnavailable => "feature_unavailable",
            Self::IngestContractInvalid => "ingest_contract_invalid",
            Self::IngestNoFiles => "ingest_no_files",
            Self::IngestSidecarConflict => "ingest_sidecar_conflict",
            Self::IngestStorageFailed => "ingest_storage_failed",
            Self::InvalidDay => "invalid_day",
            Self::InvalidSegmentOrStream => "invalid_segment_or_stream",
            Self::LocalRequestOnly => "local_request_only",
            Self::MissingRequiredField => "missing_required_field",
            Self::LinkedDeviceRequired => "linked_device_required",
            Self::PlRevoked => "pl_revoked",
            Self::ProtocolVersionFuture => "protocol_version_future",
            Self::ProtocolVersionLegacy => "protocol_version_legacy",
            Self::SegmentRemoved => "segment_removed",
            Self::SettingsOperationFailed => "settings_operation_failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auth_key_invalid" => Some(Self::AuthKeyInvalid),
            "auth_required" => Some(Self::AuthRequired),
            "content_conflict" => Some(Self::ContentConflict),
            "feature_unavailable" => Some(Self::FeatureUnavailable),
            "ingest_contract_invalid" => Some(Self::IngestContractInvalid),
            "ingest_no_files" => Some(Self::IngestNoFiles),
            "ingest_sidecar_conflict" => Some(Self::IngestSidecarConflict),
            "ingest_storage_failed" => Some(Self::IngestStorageFailed),
            "invalid_day" => Some(Self::InvalidDay),
            "invalid_segment_or_stream" => Some(Self::InvalidSegmentOrStream),
            "local_request_only" => Some(Self::LocalRequestOnly),
            "missing_required_field" => Some(Self::MissingRequiredField),
            "linked_device_required" => Some(Self::LinkedDeviceRequired),
            "pl_revoked" => Some(Self::PlRevoked),
            "protocol_version_future" => Some(Self::ProtocolVersionFuture),
            "protocol_version_legacy" => Some(Self::ProtocolVersionLegacy),
            "segment_removed" => Some(Self::SegmentRemoved),
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
    http_status: Option<u16>,
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

    pub fn http_status(self) -> Option<u16> {
        self.http_status
    }

    fn local(diagnostic: DiagnosticCode) -> Self {
        Self {
            diagnostic,
            status_class: None,
            reason_code: None,
            http_status: None,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptFault {
    DescriptorsAbsent,
    MalformedDescriptor,
    DuplicateSubmitted,
    MissingDescriptor,
    ExtraDescriptor,
    Sha256Mismatch,
    Sha256Encoding,
    SizeMismatch,
    UnknownDisposition,
    MissingDisposition,
    ReceivedNotWritten,
    UploadNotAcknowledged,
}

impl ReceiptFault {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DescriptorsAbsent => "descriptors_absent",
            Self::MalformedDescriptor => "malformed_descriptor",
            Self::DuplicateSubmitted => "duplicate_submitted",
            Self::MissingDescriptor => "missing_descriptor",
            Self::ExtraDescriptor => "extra_descriptor",
            Self::Sha256Mismatch => "sha256_mismatch",
            Self::Sha256Encoding => "sha256_encoding",
            Self::SizeMismatch => "size_mismatch",
            Self::UnknownDisposition => "unknown_disposition",
            Self::MissingDisposition => "missing_disposition",
            Self::ReceivedNotWritten => "received_not_written",
            Self::UploadNotAcknowledged => "upload_not_acknowledged",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgedFile {
    pub submitted: String,
    pub written: String,
    pub size: u64,
    pub sha256: String,
    pub disposition: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Receipt {
    Absent,
    Invalid(ReceiptFault),
    Valid(Vec<AcknowledgedFile>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDescriptor {
    pub submitted: String,
    pub written: String,
    pub size: u64,
    pub sha256: String,
    pub disposition: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadResult {
    pub status: UploadStatus,
    pub authoritative_key: Option<String>,
    pub descriptors: Option<Result<Vec<ParsedDescriptor>, ReceiptFault>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

struct PreparedFile {
    descriptor: LocalFile,
    file: File,
}

#[derive(Serialize)]
struct UploadEnvelope {
    day: String,
    segment: String,
    source: String,
    files: Vec<UploadEnvelopeFile>,
}

#[derive(Serialize)]
struct UploadEnvelopeFile {
    submitted: String,
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
        if let Ok(bootstrap_url) = bridge.bootstrap_url() {
            let response = client
                .get(bootstrap_url)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(|error| request_diagnostic(&error, DiagnosticCode::JournalUnavailable))?;
            if response.status() != StatusCode::FOUND {
                return Err(DiagnosticCode::JournalContractInvalid);
            }
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

    fn ingest_request(
        &self,
        method: Method,
        path: &str,
        source: &str,
    ) -> Result<reqwest::RequestBuilder, DiagnosticCode> {
        Ok(self.request(method, path)?.query(&[("source", source)]))
    }

    pub async fn ingest_upload(
        &self,
        day: &str,
        segment: &str,
        paths: Vec<PathBuf>,
        source: &str,
    ) -> Result<UploadResult, JournalError> {
        if !valid_day(day) || !valid_component(segment) || paths.is_empty() {
            return Err(JournalError::local(DiagnosticCode::LocalSegmentInvalid));
        }
        let prepared = tokio::task::spawn_blocking(move || prepare_files(paths, None))
            .await
            .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))??;
        let mut envelope = UploadEnvelope {
            day: day.to_owned(),
            segment: segment.to_owned(),
            source: source.to_owned(),
            files: Vec::with_capacity(prepared.len()),
        };
        let mut upload_files = Vec::with_capacity(prepared.len());
        for prepared_file in prepared {
            if prepared_file.descriptor.size > MAX_MULTIPART_PART_BYTES {
                return Err(JournalError::local(DiagnosticCode::RequestTooLarge));
            }
            envelope.files.push(UploadEnvelopeFile {
                submitted: prepared_file.descriptor.name.clone(),
            });
            upload_files.push(prepared_file);
        }
        let envelope = serde_json::to_vec(&envelope)
            .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
        if envelope.len() > MAX_MULTIPART_PART_BYTES as usize {
            return Err(JournalError::local(DiagnosticCode::RequestTooLarge));
        }
        let mut producers = Vec::with_capacity(upload_files.len());
        let mut producer_starts = Vec::with_capacity(upload_files.len());
        let envelope_part = Part::bytes(envelope)
            .mime_str("application/json")
            .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
        let mut form = Form::new().part("envelope", envelope_part);
        for prepared_file in upload_files {
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
            .ingest_request(Method::POST, INGEST_PATH, source)?
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

    pub async fn system_status(&self) -> Result<String, JournalError> {
        self.system_status_with_limits(SYSTEM_STATUS_TIMEOUT, MAX_RESPONSE_BODY_BYTES)
            .await
    }

    pub(crate) async fn optional_system_status(
        &self,
        timeout: Duration,
    ) -> Result<String, JournalError> {
        self.system_status_with_limits(timeout, OPTIONAL_RESPONSE_BODY_BYTES)
            .await
    }

    async fn system_status_with_limits(
        &self,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<String, JournalError> {
        let response = self
            .request(Method::GET, SYSTEM_STATUS_PATH)?
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| {
                JournalError::local(request_diagnostic(
                    &error,
                    DiagnosticCode::JournalUnavailable,
                ))
            })?;
        let status = response.status();
        let body = collect_response_body_limited(response, max_bytes).await?;
        if status != StatusCode::OK {
            return Err(classify_error_response(status.as_u16(), &body));
        }
        decode_system_status_response(&body)
    }

    pub async fn get_clients_self(
        &self,
        timeout: Duration,
    ) -> Result<(StatusCode, Vec<u8>), JournalError> {
        let response = self
            .request(Method::GET, CLIENTS_SELF_PATH)?
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| {
                JournalError::local(request_diagnostic(
                    &error,
                    DiagnosticCode::JournalUnavailable,
                ))
            })?;
        let status = response.status();
        let body = collect_response_body_limited(response, OPTIONAL_RESPONSE_BODY_BYTES).await?;
        Ok((status, body))
    }

    pub async fn put_clients_self(
        &self,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<(StatusCode, Vec<u8>), JournalError> {
        let response = self
            .request(Method::PUT, CLIENTS_SELF_PATH)?
            .header("content-type", "application/json")
            .body(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| {
                JournalError::local(request_diagnostic(
                    &error,
                    DiagnosticCode::JournalUnavailable,
                ))
            })?;
        let status = response.status();
        let body = collect_response_body_limited(response, OPTIONAL_RESPONSE_BODY_BYTES).await?;
        Ok((status, body))
    }

    pub async fn get_relay_access(
        &self,
        timeout: Duration,
    ) -> Result<(StatusCode, Vec<u8>), JournalError> {
        let response = self
            .request(Method::GET, RELAY_ACCESS_PATH)?
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| {
                JournalError::local(request_diagnostic(
                    &error,
                    DiagnosticCode::JournalUnavailable,
                ))
            })?;
        let status = response.status();
        let body = collect_response_body_limited(response, OPTIONAL_RESPONSE_BODY_BYTES).await?;
        Ok((status, body))
    }
}

async fn collect_response_body(response: reqwest::Response) -> Result<Vec<u8>, JournalError> {
    collect_response_body_limited(response, MAX_RESPONSE_BODY_BYTES).await
}

pub(crate) async fn collect_response_body_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, JournalError> {
    let declared_length = response.content_length().or_else(|| {
        response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    });
    if declared_length.is_some_and(|length| length > max_bytes as u64) {
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
        if length > max_bytes {
            return Err(JournalError::local(DiagnosticCode::JournalResponseTooLarge));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct UploadResponse {
    status: UploadStatus,
    #[serde(default)]
    segment: Option<String>,
    #[serde(default)]
    existing_segment: Option<String>,
    #[serde(default)]
    file_descriptors: Option<serde_json::Value>,
}

fn parse_descriptor_shape(val: &serde_json::Value) -> Result<Vec<ParsedDescriptor>, ReceiptFault> {
    let items = val.as_array().ok_or(ReceiptFault::MalformedDescriptor)?;
    let mut descriptors = Vec::with_capacity(items.len());
    for item in items {
        let obj = item.as_object().ok_or(ReceiptFault::MalformedDescriptor)?;
        let submitted = obj
            .get("submitted")
            .and_then(|v| v.as_str())
            .ok_or(ReceiptFault::MalformedDescriptor)?;
        let written = obj
            .get("written")
            .and_then(|v| v.as_str())
            .ok_or(ReceiptFault::MalformedDescriptor)?;
        let size = obj
            .get("size")
            .and_then(|v| v.as_u64())
            .ok_or(ReceiptFault::MalformedDescriptor)?;
        let sha256_val = obj
            .get("sha256")
            .and_then(|v| v.as_str())
            .ok_or(ReceiptFault::MalformedDescriptor)?;

        if sha256_val.len() != 64 || !sha256_val.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ReceiptFault::MalformedDescriptor);
        }
        if sha256_val.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(ReceiptFault::Sha256Encoding);
        }

        let disposition_val = obj
            .get("disposition")
            .ok_or(ReceiptFault::MissingDisposition)?;
        let disposition = disposition_val
            .as_str()
            .ok_or(ReceiptFault::MalformedDescriptor)?;
        if disposition != "written"
            && disposition != "already_held"
            && disposition != "received_not_written"
        {
            return Err(ReceiptFault::UnknownDisposition);
        }

        descriptors.push(ParsedDescriptor {
            submitted: submitted.to_owned(),
            written: written.to_owned(),
            size,
            sha256: sha256_val.to_owned(),
            disposition: disposition.to_owned(),
        });
    }
    Ok(descriptors)
}

pub fn decode_upload_response(body: &[u8]) -> Result<UploadResult, JournalError> {
    let response = serde_json::from_slice::<UploadResponse>(body)
        .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
    let authoritative_key = match response.status {
        UploadStatus::Ok | UploadStatus::Collision => required_key(response.segment)?,
        UploadStatus::Duplicate => required_key(response.existing_segment)?,
        UploadStatus::Conflict | UploadStatus::Failed => None,
    };
    let descriptors = response.file_descriptors.and_then(|val| {
        if val.is_null() {
            return None;
        }
        Some(parse_descriptor_shape(&val))
    });
    Ok(UploadResult {
        status: response.status,
        authoritative_key,
        descriptors,
    })
}

pub fn assess_receipt(result: &UploadResult, local_files: &[LocalFile]) -> Receipt {
    if matches!(result.status, UploadStatus::Conflict | UploadStatus::Failed) {
        return Receipt::Invalid(ReceiptFault::UploadNotAcknowledged);
    }
    let Some(descriptors_res) = &result.descriptors else {
        return Receipt::Absent;
    };
    let descriptors = match descriptors_res {
        Ok(descriptors) => descriptors,
        Err(fault) => return Receipt::Invalid(*fault),
    };

    let mut seen_submitted = std::collections::HashSet::new();
    for d in descriptors {
        if !seen_submitted.insert(d.submitted.as_str()) {
            return Receipt::Invalid(ReceiptFault::DuplicateSubmitted);
        }
    }

    if descriptors
        .iter()
        .any(|d| d.disposition == "received_not_written")
    {
        return Receipt::Invalid(ReceiptFault::ReceivedNotWritten);
    }

    let local_by_name: std::collections::HashMap<&str, &LocalFile> =
        local_files.iter().map(|f| (f.name.as_str(), f)).collect();

    if descriptors.len() > local_files.len() {
        return Receipt::Invalid(ReceiptFault::ExtraDescriptor);
    }
    if descriptors.len() < local_files.len() {
        return Receipt::Invalid(ReceiptFault::MissingDescriptor);
    }

    for d in descriptors {
        if !local_by_name.contains_key(d.submitted.as_str()) {
            return Receipt::Invalid(ReceiptFault::ExtraDescriptor);
        }
    }
    for local in local_files {
        if !seen_submitted.contains(local.name.as_str()) {
            return Receipt::Invalid(ReceiptFault::MissingDescriptor);
        }
    }

    let mut acknowledged = Vec::with_capacity(descriptors.len());
    for d in descriptors {
        let local = local_by_name[d.submitted.as_str()];
        if d.size != local.size {
            return Receipt::Invalid(ReceiptFault::SizeMismatch);
        }
        if d.sha256 != local.sha256 {
            return Receipt::Invalid(ReceiptFault::Sha256Mismatch);
        }
        acknowledged.push(AcknowledgedFile {
            submitted: d.submitted.clone(),
            written: d.written.clone(),
            size: d.size,
            sha256: d.sha256.clone(),
            disposition: d.disposition.clone(),
        });
    }

    Receipt::Valid(acknowledged)
}

pub fn decode_system_status_response(body: &[u8]) -> Result<String, JournalError> {
    #[derive(Deserialize)]
    struct Response {
        ok: bool,
        version: VersionField,
    }
    #[derive(Deserialize)]
    struct VersionField {
        current: String,
    }
    let response = serde_json::from_slice::<Response>(body)
        .map_err(|_| JournalError::local(DiagnosticCode::JournalContractInvalid))?;
    if !response.ok || response.version.current.is_empty() {
        return Err(JournalError::local(DiagnosticCode::JournalContractInvalid));
    }
    Ok(response.version.current)
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
    // A server error with no recognized reason is the journal or the bridge failing to answer
    // (including the bridge's own 502), not the journal refusing this device.
    let diagnostic = if reason_code.is_none() && status_class == JournalStatusClass::Server {
        DiagnosticCode::JournalUnavailable
    } else {
        DiagnosticCode::JournalRejected
    };
    JournalError {
        diagnostic,
        status_class: Some(status_class),
        reason_code,
        http_status: Some(status),
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

pub(crate) fn stream_sha256_hex(file: &mut File) -> io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; RECOMMENDED_CHUNK];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
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
    let sha256 = stream_sha256_hex(&mut file)
        .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| JournalError::local(DiagnosticCode::LocalSegmentInvalid))?;
    if let Some(instrumentation) = instrumentation {
        instrumentation.hashed_file(size);
    }
    Ok(PreparedFile {
        descriptor: LocalFile {
            name: name.to_owned(),
            size,
            sha256,
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
