# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""HTTP upload client for solstone ingest server.

Extracted from solstone's observe/remote_client.py. Accepts Config
as constructor parameter instead of reading config internally.
"""

from __future__ import annotations

import json
import logging
import shutil
import subprocess
import time
from pathlib import Path
from typing import Any, NamedTuple

import requests

from .config import Config

logger = logging.getLogger(__name__)

UPLOAD_TIMEOUT = 300
EVENT_TIMEOUT = 30


class UploadResult(NamedTuple):
    success: bool
    duplicate: bool = False


class UploadClient:
    """HTTP client for uploading observer segments to the ingest server."""

    def __init__(self, config: Config):
        self._url = config.server_url.rstrip("/") if config.server_url else ""
        self._key = config.key
        self._stream = config.stream
        self._revoked = False
        self._session = requests.Session()
        self._retry_backoff = config.sync_retry_delays[:3] or [1, 5, 15]
        self._max_retries = min(config.sync_max_retries, 3)

    @property
    def is_revoked(self) -> bool:
        return self._revoked

    def _persist_key(self, config: Config, key: str) -> None:
        """Save auto-registered key back to config."""
        from .config import save_config

        config.key = key
        save_config(config)

    def ensure_registered(self, config: Config) -> bool:
        """Ensure the client has a valid key, auto-registering if needed.

        Tries sol CLI first (no server needed), falls back to HTTP.
        Returns True if a key is available.
        """
        if self._key:
            return True

        # Try sol CLI registration first
        name = self._stream or "solstone-tmux"
        sol = shutil.which("sol")
        if sol:
            try:
                result = subprocess.run(
                    [sol, "remote", "--json", "create", name],
                    capture_output=True, text=True, timeout=10,
                )
                if result.returncode == 0:
                    data = json.loads(result.stdout)
                    self._key = data["key"]
                    self._persist_key(config, self._key)
                    logger.info(f"CLI-registered as '{name}' (key: {self._key[:8]}...)")
                    return True
            except (subprocess.TimeoutExpired, json.JSONDecodeError, KeyError, OSError) as e:
                logger.debug(f"CLI registration failed: {e}")

        if not self._url:
            return False

        url = f"{self._url}/app/remote/api/create"

        for attempt, delay in enumerate(self._retry_backoff):
            try:
                resp = self._session.post(
                    url, json={"name": name}, timeout=EVENT_TIMEOUT
                )
                if resp.status_code == 200:
                    data = resp.json()
                    self._key = data["key"]
                    self._persist_key(config, self._key)
                    logger.info(f"Auto-registered as '{name}' (key: {self._key[:8]}...)")
                    return True
                elif resp.status_code == 403:
                    self._revoked = True
                    logger.error("Registration rejected (403)")
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
            return UploadResult(False)

        url = f"{self._url}/app/remote/ingest/{self._key}"

        for attempt, delay in enumerate(self._retry_backoff):
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
                    return UploadResult(False)

                data: dict[str, Any] = {"day": day, "segment": segment}
                if meta:
                    data["meta"] = json.dumps(meta)

                response = self._session.post(
                    url, data=data, files=files_data, timeout=UPLOAD_TIMEOUT
                )

                if response.status_code == 200:
                    resp_data = response.json()
                    is_duplicate = resp_data.get("status") == "duplicate"
                    return UploadResult(True, duplicate=is_duplicate)
                if response.status_code in (400, 401, 403):
                    if response.status_code == 403:
                        self._revoked = True
                    logger.error(
                        f"Upload rejected ({response.status_code}): {response.text}"
                    )
                    return UploadResult(False)

                logger.warning(
                    f"Upload attempt {attempt + 1} failed: "
                    f"{response.status_code} {response.text}"
                )
            except requests.RequestException as e:
                logger.warning(f"Upload attempt {attempt + 1} failed: {e}")
            finally:
                for fh in file_handles:
                    try:
                        fh.close()
                    except Exception:
                        pass

            if attempt < len(self._retry_backoff) - 1:
                time.sleep(delay)

        logger.error(f"Upload failed after {len(self._retry_backoff)} attempts: {day}/{segment}")
        return UploadResult(False)

    def get_server_segments(self, day: str) -> list[dict] | None:
        """Query server for segments on a given day.

        Returns list of segment dicts, or None on failure.
        """
        if self._revoked or not self._key or not self._url:
            return None

        url = f"{self._url}/app/remote/ingest/{self._key}/segments/{day}"
        params = {}
        if self._stream:
            params["stream"] = self._stream

        try:
            resp = self._session.get(url, params=params, timeout=EVENT_TIMEOUT)
            if resp.status_code == 200:
                return resp.json()
            if resp.status_code in (401, 403):
                if resp.status_code == 403:
                    self._revoked = True
                logger.error(f"Segments query rejected ({resp.status_code})")
                return None
            logger.warning(f"Segments query failed: {resp.status_code}")
            return None
        except requests.RequestException as e:
            logger.debug(f"Segments query failed: {e}")
            return None

    def relay_event(self, tract: str, event: str, **fields: Any) -> bool:
        """Fire-and-forget event relay."""
        if self._revoked or not self._key or not self._url:
            return False

        url = f"{self._url}/app/remote/ingest/{self._key}/event"
        payload = {"tract": tract, "event": event, **fields}
        try:
            resp = self._session.post(url, json=payload, timeout=EVENT_TIMEOUT)
            if resp.status_code == 200:
                return True
            if resp.status_code == 403:
                self._revoked = True
            return False
        except requests.RequestException:
            return False

    def stop(self) -> None:
        self._session.close()
