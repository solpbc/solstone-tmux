# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""HTTP upload client for solstone ingest server.

Extracted from solstone's observe/remote_client.py. Accepts Config
as constructor parameter instead of reading config internally.
"""

from __future__ import annotations

import json
import logging
import platform
import socket
import time
from pathlib import Path
from typing import Any, NamedTuple

import requests

from . import __version__
from .config import Config

logger = logging.getLogger(__name__)

UPLOAD_TIMEOUT = 300
EVENT_TIMEOUT = 30
OBSERVER_HEADER = "X-Solstone-Observer"
FAILURE_AUTH = "auth"
FAILURE_CLIENT_CONTRACT = "client_contract"
FAILURE_CONFIGURATION = "configuration"
FAILURE_TRANSIENT = "transient"


class UploadResult(NamedTuple):
    success: bool
    duplicate: bool = False
    reason: str | None = None
    status_code: int | None = None
    failure_class: str | None = None
    exception_class: str | None = None


def classify_http_failure(status_code: int) -> str:
    """Classify observer HTTP failures for sync circuit behavior."""
    if status_code in (401, 403):
        return FAILURE_AUTH
    if status_code in (408, 429) or status_code >= 500:
        return FAILURE_TRANSIENT
    if 400 <= status_code < 500:
        return FAILURE_CLIENT_CONTRACT
    return FAILURE_TRANSIENT


class UploadClient:
    """HTTP client for uploading observer segments to the ingest server."""

    def __init__(self, config: Config):
        self._url = config.server_url.rstrip("/") if config.server_url else ""
        self._key = config.key
        self._revoked = False
        self._session = requests.Session()
        self._retry_backoff = config.sync_retry_delays[:3] or [1, 5, 15]
        self._max_retries = max(1, min(config.sync_max_retries, 3))
        self._last_failure: UploadResult | None = None

    @property
    def is_revoked(self) -> bool:
        return self._revoked

    @property
    def is_registered(self) -> bool:
        return bool(self._key)

    @property
    def last_failure(self) -> UploadResult | None:
        """Most recent upload/query failure metadata."""
        return self._last_failure

    def refresh_config(self, config: Config) -> None:
        """Refresh cached connection credentials after config changes."""
        next_url = config.server_url.rstrip("/") if config.server_url else ""
        next_key = config.key
        if next_url != self._url or next_key != self._key:
            self._url = next_url
            self._key = next_key
            self._revoked = False
            self._last_failure = None

    def _record_failure(self, result: UploadResult) -> UploadResult:
        self._last_failure = result
        return result

    def _clear_failure(self) -> None:
        self._last_failure = None

    def _persist_registration(self, config: Config, key: str, name: str) -> None:
        """Save the minted key and journal-locked stream back to config."""
        from .config import save_config

        config.key = key
        config.stream = name
        save_config(config)

    def ensure_registered(self, config: Config) -> bool:
        """Ensure the client has a valid key, registering with the journal if needed.

        Registers HTTP-direct against /app/observer/register with a full
        descriptor; the journal mints the key and locks the stream identity.
        Returns True if a key is available.
        """
        if self._key:
            return True

        if not self._url:
            return False

        descriptor = {
            "platform": platform.system().lower(),
            "hostname": socket.gethostname(),
            "stream_type": "tmux",
            "version": __version__,
        }
        url = f"{self._url}/app/observer/register"

        for attempt, delay in enumerate(self._retry_backoff):
            try:
                resp = self._session.post(url, json=descriptor, timeout=EVENT_TIMEOUT)
                if resp.status_code == 200:
                    data = resp.json()
                    self._key = data["key"]
                    self._persist_registration(config, data["key"], data["name"])
                    logger.info(
                        f"Registered as '{data['name']}' (key: {self._key[:8]}...)"
                    )
                    return True
                elif resp.status_code == 403:
                    self._revoked = True
                    logger.error(
                        "Registration rejected — your journal must be reachable "
                        "directly on localhost"
                    )
                    return False
                else:
                    logger.warning(
                        f"Registration attempt {attempt + 1} failed: {resp.status_code}"
                    )
            except requests.RequestException as e:
                logger.warning(f"Registration attempt {attempt + 1} failed: {e}")
            if attempt < len(self._retry_backoff) - 1:
                time.sleep(delay)

        logger.error(f"Registration failed after {len(self._retry_backoff)} attempts")
        return False

    def upload_segment(
        self,
        day: str,
        segment: str,
        files: list[Path],
        meta: dict[str, Any] | None = None,
    ) -> UploadResult:
        """Upload a segment's files to the ingest server."""
        if self._revoked or not self._key or not self._url:
            return self._record_failure(
                UploadResult(
                    False,
                    reason="revoked" if self._revoked else "not_configured",
                    failure_class=(
                        FAILURE_AUTH if self._revoked else FAILURE_CONFIGURATION
                    ),
                )
            )

        url = f"{self._url}/app/observer/ingest"
        last_reason = "upload_failed"

        for attempt in range(self._max_retries):
            delay = self._retry_backoff[min(attempt, len(self._retry_backoff) - 1)]
            file_handles = []
            files_data = []
            try:
                for path in files:
                    if not path.exists():
                        logger.warning(f"File not found, skipping: {path}")
                        continue
                    fh = open(path, "rb")
                    file_handles.append(fh)
                    files_data.append(
                        ("files", (path.name, fh, "application/octet-stream"))
                    )

                if not files_data:
                    return self._record_failure(
                        UploadResult(
                            False,
                            reason="not_configured",
                            failure_class=FAILURE_CONFIGURATION,
                        )
                    )

                data: dict[str, Any] = {"day": day, "segment": segment}
                if meta:
                    data["meta"] = json.dumps(meta)

                response = self._session.post(
                    url,
                    data=data,
                    files=files_data,
                    headers={
                        OBSERVER_HEADER: self._key,
                        "Authorization": f"Bearer {self._key}",
                    },
                    timeout=UPLOAD_TIMEOUT,
                )

                if response.status_code == 200:
                    resp_data = response.json()
                    is_duplicate = resp_data.get("status") == "duplicate"
                    self._clear_failure()
                    return UploadResult(True, duplicate=is_duplicate)

                failure_class = classify_http_failure(response.status_code)
                result = UploadResult(
                    False,
                    reason=f"http_{response.status_code}",
                    status_code=response.status_code,
                    failure_class=failure_class,
                )
                if failure_class != FAILURE_TRANSIENT:
                    if response.status_code == 403:
                        self._revoked = True
                    logger.error(
                        f"Upload rejected ({response.status_code}): {response.text}"
                    )
                    return self._record_failure(result)

                last_reason = f"http_{response.status_code}"
                self._record_failure(result)
                logger.warning(
                    f"Upload attempt {attempt + 1} failed: "
                    f"{response.status_code} {response.text}"
                )
            except requests.RequestException as e:
                last_reason = type(e).__name__
                self._record_failure(
                    UploadResult(
                        False,
                        reason=last_reason,
                        failure_class=FAILURE_TRANSIENT,
                        exception_class=last_reason,
                    )
                )
                logger.warning(f"Upload attempt {attempt + 1} failed: {e}")
            finally:
                for fh in file_handles:
                    try:
                        fh.close()
                    except Exception:
                        pass

            if attempt < self._max_retries - 1:
                time.sleep(delay)

        logger.error(
            f"Upload failed after {self._max_retries} attempts: {day}/{segment}"
        )
        return self._record_failure(
            self._last_failure
            or UploadResult(False, reason=last_reason, failure_class=FAILURE_TRANSIENT)
        )

    def get_server_segments(self, day: str) -> list[dict] | None:
        """Query server for segments on a given day.

        Returns list of segment dicts, or None on failure.
        """
        if self._revoked or not self._key or not self._url:
            self._record_failure(
                UploadResult(
                    False,
                    reason="revoked" if self._revoked else "not_configured",
                    failure_class=(
                        FAILURE_AUTH if self._revoked else FAILURE_CONFIGURATION
                    ),
                )
            )
            return None

        url = f"{self._url}/app/observer/ingest/segments/{day}"

        try:
            resp = self._session.get(
                url,
                headers={
                    OBSERVER_HEADER: self._key,
                    "Authorization": f"Bearer {self._key}",
                },
                timeout=EVENT_TIMEOUT,
            )
            if resp.status_code == 200:
                self._clear_failure()
                return resp.json()
            failure_class = classify_http_failure(resp.status_code)
            if failure_class == FAILURE_AUTH:
                if resp.status_code == 403:
                    self._revoked = True
                logger.error(f"Segments query rejected ({resp.status_code})")
            else:
                logger.warning(f"Segments query failed: {resp.status_code}")
            self._record_failure(
                UploadResult(
                    False,
                    reason=f"http_{resp.status_code}",
                    status_code=resp.status_code,
                    failure_class=failure_class,
                )
            )
            return None
        except requests.RequestException as e:
            logger.debug(f"Segments query failed: {e}")
            self._record_failure(
                UploadResult(
                    False,
                    reason=type(e).__name__,
                    failure_class=FAILURE_TRANSIENT,
                    exception_class=type(e).__name__,
                )
            )
            return None

    def relay_event(self, tract: str, event: str, **fields: Any) -> bool:
        """Fire-and-forget event relay."""
        if self._revoked or not self._key or not self._url:
            return False

        url = f"{self._url}/app/observer/ingest/event"
        payload = {"tract": tract, "event": event, **fields}
        try:
            resp = self._session.post(
                url,
                json=payload,
                headers={
                    OBSERVER_HEADER: self._key,
                    "Authorization": f"Bearer {self._key}",
                },
                timeout=EVENT_TIMEOUT,
            )
            if resp.status_code == 200:
                return True
            if resp.status_code == 403:
                self._revoked = True
            return False
        except requests.RequestException:
            return False

    def stop(self) -> None:
        self._session.close()
