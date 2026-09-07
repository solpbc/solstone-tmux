// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::cli;
use crate::journal::JournalClient;
use crate::journal_version::VersionRefreshState;
use crate::paths::PlatformKind;
use crate::sync::CredentialStore;

pub const METADATA_PROTOCOL_VERSION: u32 = 1;
const MAX_NAME_BYTES: usize = 80;
const MAX_FIELD_BYTES: usize = 64;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientsSelfReportedSnapshot {
    pub name: Option<String>,
    pub platform: Option<String>,
    pub device_type: Option<String>,
    pub app_id: Option<String>,
    pub app_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientsSelfJournalInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientsSelfGetResponse {
    pub protocol_version: u32,
    pub revision: u64,
    pub reported: Option<ClientsSelfReportedSnapshot>,
    pub owner_label: Option<String>,
    pub display_label: Option<String>,
    pub updated_at: Option<String>,
    pub journal: ClientsSelfJournalInfo,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientsSelfPutRequest {
    pub protocol_version: u32,
    pub expected_revision: u64,
    pub reported: ClientsSelfReportedSnapshot,
}

pub fn sanitize_field(value: &str, max_bytes: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return None;
    }
    if trimmed.len() > max_bytes {
        return None;
    }
    Some(trimmed.to_owned())
}

pub fn build_reported_snapshot<F>(
    hostname_source: F,
    platform: PlatformKind,
) -> ClientsSelfReportedSnapshot
where
    F: Fn() -> Option<String>,
{
    let raw_name = hostname_source();
    let name = raw_name
        .as_deref()
        .and_then(|h| sanitize_field(h, MAX_NAME_BYTES));
    let platform_name = sanitize_field(platform.pairing_platform(), MAX_FIELD_BYTES);
    let device_type = sanitize_field("terminal", MAX_FIELD_BYTES);
    let app_id = sanitize_field("solstone-tmux", MAX_FIELD_BYTES);
    let app_version = sanitize_field(&cli::version(), MAX_FIELD_BYTES);

    ClientsSelfReportedSnapshot {
        name,
        platform: platform_name,
        device_type,
        app_id,
        app_version,
    }
}

pub fn decode_get_response(bytes: &[u8]) -> Option<ClientsSelfGetResponse> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    let object = value.as_object()?;
    for key in [
        "reported",
        "owner_label",
        "display_label",
        "updated_at",
        "journal",
    ] {
        object.contains_key(key).then_some(())?;
    }
    if let Some(reported) = object
        .get("reported")
        .and_then(serde_json::Value::as_object)
    {
        for key in ["name", "platform", "device_type", "app_id", "app_version"] {
            reported.contains_key(key).then_some(())?;
        }
    } else if !object
        .get("reported")
        .is_some_and(serde_json::Value::is_null)
    {
        return None;
    }
    let journal = object.get("journal")?.as_object()?;
    for key in ["name", "version"] {
        journal.contains_key(key).then_some(())?;
    }
    let response = serde_json::from_slice::<ClientsSelfGetResponse>(bytes).ok()?;
    if response.protocol_version != METADATA_PROTOCOL_VERSION {
        return None;
    }
    Some(response)
}

async fn cache_journal_info(
    store: &Arc<CredentialStore>,
    version_refresh: VersionRefreshState,
    attempt: u64,
    name: Option<String>,
    version: String,
) {
    let _ = store
        .publish_journal_info(version_refresh, attempt, name, version)
        .await;
}

pub async fn run_metadata_job<F>(
    client: &JournalClient,
    store: &Arc<CredentialStore>,
    version_refresh: &VersionRefreshState,
    hostname_source: F,
    platform: PlatformKind,
    timeout: Duration,
) -> Result<(), ()>
where
    F: Fn() -> Option<String>,
{
    let attempt = version_refresh.capture_metadata_attempt();
    let (status, body) = client.get_clients_self(timeout).await.map_err(|_| ())?;
    if !version_refresh.metadata_attempt_is_current(attempt) {
        return Ok(());
    }
    if status == StatusCode::NOT_FOUND {
        // Legacy status probing is a metadata-lane fallback, never a third
        // redial-triggered job.
        if let Ok(version) = client.system_status().await
            && !version.trim().is_empty()
        {
            cache_journal_info(store, version_refresh.clone(), attempt, None, version).await;
        }
        return Ok(());
    }
    if status != StatusCode::OK {
        return Err(());
    }
    let get_response = decode_get_response(&body).ok_or(())?;
    if !get_response.journal.version.is_empty() {
        cache_journal_info(
            store,
            version_refresh.clone(),
            attempt,
            Some(get_response.journal.name.clone()),
            get_response.journal.version.clone(),
        )
        .await;
    }

    let current_snapshot = build_reported_snapshot(&hostname_source, platform);
    if get_response.reported.as_ref() == Some(&current_snapshot) {
        return Ok(());
    }
    if !version_refresh.metadata_attempt_is_current(attempt) {
        return Ok(());
    }

    let put_req = ClientsSelfPutRequest {
        protocol_version: METADATA_PROTOCOL_VERSION,
        expected_revision: get_response.revision,
        reported: current_snapshot,
    };
    let put_bytes = serde_json::to_vec(&put_req).map_err(|_| ())?;
    let (put_status, put_body) = client
        .put_clients_self(put_bytes, timeout)
        .await
        .map_err(|_| ())?;
    if put_status == StatusCode::OK {
        // PUT is the same complete resource as GET. A malformed successful
        // response must not roll back the already validated GET cache.
        let put_response = decode_get_response(&put_body).ok_or(())?;
        if version_refresh.metadata_attempt_is_current(attempt)
            && !put_response.journal.version.is_empty()
        {
            cache_journal_info(
                store,
                version_refresh.clone(),
                attempt,
                Some(put_response.journal.name),
                put_response.journal.version,
            )
            .await;
        }
        return Ok(());
    }
    if put_status == StatusCode::CONFLICT {
        // 409 Conflict: reread GET, retry at most once with newest local snapshot
        let (retry_get_status, retry_get_body) =
            client.get_clients_self(timeout).await.map_err(|_| ())?;
        if retry_get_status != StatusCode::OK {
            return Err(());
        }
        let retry_get_response = decode_get_response(&retry_get_body).ok_or(())?;
        if !retry_get_response.journal.version.is_empty() {
            cache_journal_info(
                store,
                version_refresh.clone(),
                attempt,
                Some(retry_get_response.journal.name.clone()),
                retry_get_response.journal.version.clone(),
            )
            .await;
        }
        let newest_snapshot = build_reported_snapshot(&hostname_source, platform);
        if !version_refresh.metadata_attempt_is_current(attempt) {
            return Ok(());
        }
        if retry_get_response.reported.as_ref() == Some(&newest_snapshot) {
            return Ok(());
        }
        let retry_put_req = ClientsSelfPutRequest {
            protocol_version: METADATA_PROTOCOL_VERSION,
            expected_revision: retry_get_response.revision,
            reported: newest_snapshot,
        };
        let retry_put_bytes = serde_json::to_vec(&retry_put_req).map_err(|_| ())?;
        let (final_status, final_body) = client
            .put_clients_self(retry_put_bytes, timeout)
            .await
            .map_err(|_| ())?;
        if final_status == StatusCode::OK {
            let final_response = decode_get_response(&final_body).ok_or(())?;
            if version_refresh.metadata_attempt_is_current(attempt)
                && !final_response.journal.version.is_empty()
            {
                cache_journal_info(
                    store,
                    version_refresh.clone(),
                    attempt,
                    Some(final_response.journal.name),
                    final_response.journal.version,
                )
                .await;
            }
            return Ok(());
        }
        return Err(());
    }

    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_field_enforces_bounds_and_controls() {
        assert_eq!(
            sanitize_field("  my-host  ", 80),
            Some("my-host".to_owned())
        );
        assert_eq!(sanitize_field("   ", 80), None);
        assert_eq!(sanitize_field("host\x00name", 80), None);
        assert_eq!(sanitize_field("host\nname", 80), None);
        assert_eq!(sanitize_field("host\rname", 80), None);
        assert_eq!(sanitize_field("a".repeat(81).as_str(), 80), None);
        assert_eq!(
            sanitize_field("a".repeat(80).as_str(), 80),
            Some("a".repeat(80))
        );
    }

    #[test]
    fn sanitize_multibyte_counts_utf8_bytes() {
        // '€' is 3 UTF-8 bytes: [0xe2, 0x82, 0xac]
        let euro = "€";
        assert_eq!(euro.len(), 3);
        assert_eq!(sanitize_field(euro, 3), Some("€".to_owned()));
        assert_eq!(sanitize_field(euro, 2), None);
    }

    #[test]
    fn build_snapshot_invalid_name_nulls_without_tmux_fallback() {
        let snapshot = build_reported_snapshot(|| None, PlatformKind::Linux);
        assert_eq!(snapshot.name, None);
        assert_eq!(snapshot.platform, Some("linux".to_owned()));
        assert_eq!(snapshot.device_type, Some("terminal".to_owned()));
        assert_eq!(snapshot.app_id, Some("solstone-tmux".to_owned()));
        assert!(snapshot.app_version.is_some());
    }

    #[test]
    fn decode_get_response_validates_protocol_version() {
        let valid_json = r#"{
            "protocol_version": 1,
            "revision": 4,
            "reported": null,
            "owner_label": "prod-server",
            "display_label": "prod",
            "updated_at": "2026-09-07T12:00:00Z",
            "journal": {
                "name": "journal-main",
                "version": "2026.9.1"
            }
        }"#;
        let decoded = decode_get_response(valid_json.as_bytes()).expect("decode valid GET");
        assert_eq!(decoded.protocol_version, 1);
        assert_eq!(decoded.revision, 4);
        assert_eq!(decoded.owner_label, Some("prod-server".to_owned()));
        assert_eq!(decoded.journal.version, "2026.9.1");

        let invalid_version = r#"{
            "protocol_version": 2,
            "revision": 4,
            "journal": {"version": "2026.9.1"}
        }"#;
        assert!(decode_get_response(invalid_version.as_bytes()).is_none());
    }
}
