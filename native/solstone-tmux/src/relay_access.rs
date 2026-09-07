// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use spl_core::relay_access::{RelayAccess, negotiated_claims};
use spl_transport::{same_relay_origin, validate_relay_origin};

use crate::clock::Clock;
use crate::journal::JournalClient;
use crate::private_link::PrivateLinkOpener;
use crate::sync::CredentialStore;

pub const RELAY_PROTOCOL_VERSION: u8 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RelayAccessNotConfigured {
    pub protocol_version: u8,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum RelayAccessResponse {
    Ready(RelayAccess),
    NotConfigured(RelayAccessNotConfigured),
}

/// Fetch and apply a capability only after the shared SPL validation succeeds.
/// Non-200 and malformed optional responses deliberately preserve the useful
/// credential already installed in the opener.
pub async fn run_relay_access_job(
    client: &JournalClient,
    store: &Arc<CredentialStore>,
    opener: &Arc<PrivateLinkOpener>,
    lane_attempt_id: u64,
    clock: &dyn Clock,
    timeout: Duration,
) -> Result<(), ()> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::time::timeout_at(deadline, async {
        // A Ready write which landed after a reported durability error remains
        // pending until this owner retry confirms it. This is optional work: a
        // retry failure must not turn access acquisition into PrivateStateIo.
        let _ = store.persist_pending().await;

        let attempt = store.capture_access_attempt_until(lane_attempt_id, deadline);

        let (status, body) = client
            .get_relay_access(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await
            .map_err(|_| ())?;
        if status != StatusCode::OK {
            return Err(());
        }

        let response = serde_json::from_slice::<RelayAccessResponse>(&body).map_err(|_| ())?;
        match response {
            RelayAccessResponse::Ready(ready) => {
                if ready.status != "ready" || ready.protocol_version != RELAY_PROTOCOL_VERSION {
                    return Err(());
                }
                let paired_instance_id = store.instance_id();
                if ready.instance_id != paired_instance_id {
                    return Err(());
                }
                validate_relay_origin(&ready.relay_origin).map_err(|_| ())?;
                // Capture time only after the response is complete, so expiry
                // during a blocked request cannot be admitted.
                let now_unix_seconds = clock.wall_now().unix_timestamp();
                let claims = negotiated_claims(
                    ready.protocol_version,
                    &ready.device_token,
                    &ready.expires_at,
                    &paired_instance_id,
                    now_unix_seconds,
                )
                .ok_or(())?;

                let current = store.live_credential();
                let same_origin = current
                    .relay_origin
                    .as_deref()
                    .map(|origin| same_relay_origin(origin, &ready.relay_origin).map_err(|_| ()))
                    .transpose()?
                    .unwrap_or(false);
                if same_origin
                    && current.device_token.as_deref() == Some(ready.device_token.as_str())
                    && current.device_token_expires_at == Some(claims.exp)
                {
                    return Ok(());
                }

                store
                    .submit_ready(
                        Arc::clone(opener),
                        attempt,
                        ready.relay_origin,
                        ready.device_token,
                        claims.exp,
                    )
                    .await
                    .map_err(|_| ())?;
                Ok(())
            }
            RelayAccessResponse::NotConfigured(response) => {
                if response.status != "not_configured"
                    || response.protocol_version != RELAY_PROTOCOL_VERSION
                {
                    return Err(());
                }
                store.submit_disable(Arc::clone(opener), attempt).await
            }
        }
    })
    .await
    .unwrap_or(Err(()))
}
