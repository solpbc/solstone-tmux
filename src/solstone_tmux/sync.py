# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Background sync service for uploading captured segments.

Modeled on solstone-macos's SyncService.swift. Runs as an asyncio
background task in the same event loop as capture. Walks cache days
newest-to-oldest, queries server for existing segments, uploads missing ones.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
import time
from datetime import datetime, timedelta
from pathlib import Path

from .config import Config
from .upload import (
    FAILURE_AUTH,
    FAILURE_CLIENT_CONTRACT,
    FAILURE_CONFIGURATION,
    FAILURE_TRANSIENT,
    UploadClient,
    UploadResult,
    classify_http_failure,
)

logger = logging.getLogger(__name__)

CIRCUIT_CLOSED = "closed"
CIRCUIT_OPEN = "open"
CIRCUIT_HALF_OPEN = "half_open"
CIRCUIT_PERMANENT = "permanent"
TRANSIENT_FAILURE_THRESHOLD = 3


class SyncService:
    """Background sync service that uploads completed segments to the server."""

    def __init__(self, config: Config, client: UploadClient):
        self._config = config
        self._client = client
        self._synced_days: set[str] = set()
        self._consecutive_failures = 0
        self._circuit_open = False
        self._circuit_state = CIRCUIT_CLOSED
        self._failure_class: str | None = None
        self._last_status_code: int | None = None
        self._last_exception_class: str | None = None
        self._last_operation: str | None = None
        self._next_probe_at: float | None = None
        self._required_action: str | None = None
        self._permanent_config_signature: tuple[str, str] | None = None
        first_probe_delay = (
            self._config.sync_retry_delays[0] if self._config.sync_retry_delays else 60
        )
        self._transient_probe_initial_delay = max(0, first_probe_delay)
        self._transient_probe_delay = self._transient_probe_initial_delay
        self._transient_probe_max_delay = max(self._transient_probe_initial_delay, 300)
        self._last_full_sync: float = 0
        self._running = True
        self._trigger = asyncio.Event()
        self._last_successful_sync: int | None = None
        self._recent_error_count = 0
        self._last_error_reason: str | None = None
        self._pending_queue_depth = 0

        # Load synced days cache
        self._load_synced_days()

    def _synced_days_path(self) -> Path:
        return self._config.state_dir / "synced_days.json"

    def _load_synced_days(self) -> None:
        path = self._synced_days_path()
        if not path.exists():
            return
        try:
            with open(path, encoding="utf-8") as f:
                data = json.load(f)
            self._synced_days = set(data) if isinstance(data, list) else set()
        except (json.JSONDecodeError, OSError):
            self._synced_days = set()

    def _save_synced_days(self) -> None:
        self._config.state_dir.mkdir(parents=True, exist_ok=True)
        path = self._synced_days_path()
        tmp = path.with_suffix(f".{os.getpid()}.tmp")
        try:
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(sorted(self._synced_days), f)
                f.write("\n")
            os.rename(str(tmp), str(path))
        except OSError as e:
            logger.warning(f"Failed to save synced days: {e}")

    def trigger(self) -> None:
        """Trigger a sync pass (called by observer on segment completion)."""
        self._trigger.set()

    def stop(self) -> None:
        """Stop the sync service."""
        self._running = False
        self._trigger.set()

    @property
    def is_connected(self) -> bool:
        """Whether sync is connected (server configured and circuit closed)."""
        return (
            bool(self._config.server_url)
            and not self._circuit_open
            and self._circuit_state != CIRCUIT_PERMANENT
        )

    def _record_sync_success(self) -> None:
        self._last_successful_sync = int(time.time() * 1000)
        self._recent_error_count = 0
        self._last_error_reason = None
        self._clear_circuit_state()

    def _record_sync_error(self, reason: str) -> None:
        self._recent_error_count = min(99, self._recent_error_count + 1)
        self._last_error_reason = (reason or "error")[:200]

    def health_snapshot(self) -> dict:
        return {
            "last_successful_sync": self._last_successful_sync,
            "pending_queue_depth": self._pending_queue_depth,
            "recent_error_count": self._recent_error_count,
            "last_error_reason": self._last_error_reason,
            "sync_state": self._sync_state_snapshot(),
        }

    def _sync_state_snapshot(self) -> dict:
        return {
            "circuit_state": self._circuit_state,
            "failure_class": self._failure_class,
            "last_status": self._last_status_code,
            "last_exception": self._last_exception_class,
            "last_operation": self._last_operation,
            "next_probe_at": self._next_probe_at,
            "required_action": self._required_action,
            "consecutive_transient_failures": self._consecutive_failures,
        }

    def _config_signature(self) -> tuple[str, str]:
        return (self._config.server_url or "", self._config.key or "")

    def _refresh_client_config(self) -> None:
        refresh = getattr(self._client, "refresh_config", None)
        if refresh:
            refresh(self._config)

    def _clear_circuit_state(self) -> None:
        self._consecutive_failures = 0
        self._circuit_open = False
        self._circuit_state = CIRCUIT_CLOSED
        self._failure_class = None
        self._last_status_code = None
        self._last_exception_class = None
        self._last_operation = None
        self._next_probe_at = None
        self._required_action = None
        self._permanent_config_signature = None
        self._transient_probe_delay = self._transient_probe_initial_delay

    def _clear_transient_failure_state(self) -> None:
        self._consecutive_failures = 0
        self._transient_probe_delay = self._transient_probe_initial_delay
        if self._failure_class == FAILURE_TRANSIENT:
            self._circuit_open = False
            self._circuit_state = CIRCUIT_CLOSED
            self._failure_class = None
            self._last_status_code = None
            self._last_exception_class = None
            self._last_operation = None
            self._next_probe_at = None
            self._required_action = None

    def _failure_result_for_client(self, fallback_reason: str) -> UploadResult:
        result = getattr(self._client, "last_failure", None)
        if isinstance(result, UploadResult):
            return result
        return UploadResult(
            False, reason=fallback_reason, failure_class=FAILURE_TRANSIENT
        )

    def _failure_class_for_result(self, result: UploadResult) -> str:
        if result.failure_class:
            return result.failure_class
        if result.reason in {"revoked"}:
            return FAILURE_AUTH
        if result.reason == "not_configured":
            return FAILURE_CONFIGURATION
        if result.reason and result.reason.startswith("http_"):
            try:
                return classify_http_failure(int(result.reason.removeprefix("http_")))
            except ValueError:
                pass
        return FAILURE_TRANSIENT

    def _status_code_for_result(self, result: UploadResult) -> int | None:
        if result.status_code is not None:
            return result.status_code
        if result.reason and result.reason.startswith("http_"):
            try:
                return int(result.reason.removeprefix("http_"))
            except ValueError:
                return None
        return None

    def _required_action_for_failure(self, failure_class: str) -> str | None:
        if failure_class == FAILURE_AUTH:
            return "refresh_observer_credentials"
        if failure_class == FAILURE_CONFIGURATION:
            return "configure_sync_server_or_key"
        if failure_class == FAILURE_CLIENT_CONTRACT:
            return "inspect_segment_payload_or_observer_contract"
        return None

    def _record_failure_state(self, result: UploadResult, operation: str) -> str:
        failure_class = self._failure_class_for_result(result)
        self._failure_class = failure_class
        self._last_status_code = self._status_code_for_result(result)
        self._last_exception_class = result.exception_class
        self._last_operation = operation
        self._required_action = self._required_action_for_failure(failure_class)
        return failure_class

    def _open_transient_circuit(
        self,
        result: UploadResult,
        operation: str,
        *,
        increase_backoff: bool = False,
    ) -> None:
        if increase_backoff:
            self._transient_probe_delay = min(
                max(1, self._transient_probe_delay * 2),
                self._transient_probe_max_delay,
            )
        self._record_failure_state(result, operation)
        self._failure_class = FAILURE_TRANSIENT
        self._circuit_open = True
        self._circuit_state = CIRCUIT_OPEN
        self._next_probe_at = time.time() + self._transient_probe_delay
        logger.error(
            "Circuit breaker OPEN: %d consecutive transient failures; "
            "next probe in %ss",
            self._consecutive_failures,
            self._transient_probe_delay,
        )

    def _open_permanent_circuit(self, result: UploadResult, operation: str) -> None:
        self._record_failure_state(result, operation)
        self._circuit_open = True
        self._circuit_state = CIRCUIT_PERMANENT
        self._next_probe_at = None
        self._permanent_config_signature = self._config_signature()
        logger.error(
            "Sync circuit permanent until config changes: %s",
            self._required_action or "operator_action_required",
        )

    def _handle_failure_result(self, result: UploadResult, operation: str) -> bool:
        failure_class = self._record_failure_state(result, operation)
        if failure_class == FAILURE_TRANSIENT:
            self._consecutive_failures += 1
            if self._consecutive_failures >= TRANSIENT_FAILURE_THRESHOLD:
                self._open_transient_circuit(result, operation)
                return True
            return False
        if failure_class in {FAILURE_AUTH, FAILURE_CONFIGURATION}:
            self._open_permanent_circuit(result, operation)
            return True
        return False

    async def _prepare_circuit_for_sync(self) -> bool:
        self._refresh_client_config()
        if self._circuit_state == CIRCUIT_PERMANENT:
            if self._config_signature() != self._permanent_config_signature:
                logger.info("Sync config changed; clearing permanent circuit")
                self._clear_circuit_state()
                return True
            logger.warning("Permanent sync circuit open — skipping sync")
            return False

        if self._circuit_open:
            if (
                self._circuit_state == CIRCUIT_OPEN
                and self._failure_class == FAILURE_TRANSIENT
            ):
                now = time.time()
                if self._next_probe_at is not None and now < self._next_probe_at:
                    logger.warning("Transient sync circuit open — waiting for probe")
                    return False
                return await self._probe_transient_circuit()

            logger.warning("Circuit breaker open — skipping sync")
            return False

        return True

    async def _probe_transient_circuit(self) -> bool:
        self._circuit_state = CIRCUIT_HALF_OPEN
        self._last_operation = "probe"
        today = datetime.now().strftime("%Y%m%d")
        server_segments = await asyncio.to_thread(
            self._client.get_server_segments, today
        )
        if server_segments is not None:
            logger.info("Transient sync circuit probe succeeded")
            self._clear_circuit_state()
            return True

        result = self._failure_result_for_client("probe_failed")
        failure_class = self._failure_class_for_result(result)
        if failure_class == FAILURE_TRANSIENT:
            self._open_transient_circuit(result, "probe", increase_backoff=True)
            return False
        if failure_class in {FAILURE_AUTH, FAILURE_CONFIGURATION}:
            self._open_permanent_circuit(result, "probe")
            return False

        self._record_failure_state(result, "probe")
        self._circuit_open = False
        self._circuit_state = CIRCUIT_CLOSED
        self._next_probe_at = None
        return False

    def _client_contract_failure_path(self, day: str, segment_dir: Path) -> Path:
        return (
            self._config.state_dir
            / "client_contract_failures"
            / day
            / segment_dir.parent.name
            / f"{segment_dir.name}.json"
        )

    def _has_client_contract_failure(self, day: str, segment_dir: Path) -> bool:
        return self._client_contract_failure_path(day, segment_dir).exists()

    def _mark_client_contract_failure(
        self, day: str, segment_dir: Path, result: UploadResult
    ) -> None:
        marker = self._client_contract_failure_path(day, segment_dir)
        marker.parent.mkdir(parents=True, exist_ok=True)
        data = {
            "day": day,
            "stream": segment_dir.parent.name,
            "segment": segment_dir.name,
            "segment_path": str(segment_dir),
            "reason": result.reason,
            "status_code": self._status_code_for_result(result),
            "recorded_at": int(time.time() * 1000),
        }
        tmp = marker.with_suffix(f".{os.getpid()}.tmp")
        try:
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(data, f, indent=2, sort_keys=True)
                f.write("\n")
            os.rename(str(tmp), str(marker))
        except OSError as e:
            logger.warning("Failed to record client/contract failure marker: %s", e)

    async def run(self) -> None:
        """Main sync loop — waits for triggers, then syncs."""
        while self._running:
            try:
                # Wait for trigger or periodic check (60s timeout)
                try:
                    await asyncio.wait_for(self._trigger.wait(), timeout=60)
                except asyncio.TimeoutError:
                    pass

                self._trigger.clear()

                if not self._running:
                    break

                if not await self._prepare_circuit_for_sync():
                    continue

                # Force full sync daily
                now = time.time()
                force_full = (now - self._last_full_sync) > 86400

                await self._sync(force_full=force_full)

                if force_full:
                    self._last_full_sync = now

            except Exception as e:
                logger.error(f"Sync error: {e}", exc_info=True)
                await asyncio.sleep(5)

    async def _sync(self, force_full: bool = False) -> None:
        """Walk days newest-to-oldest and upload missing segments."""
        if not await self._prepare_circuit_for_sync():
            return

        captures_dir = self._config.captures_dir
        if not captures_dir.exists():
            self._pending_queue_depth = 0
            return

        today = datetime.now().strftime("%Y%m%d")

        # Collect segments by day
        segments_by_day = self._collect_segments(captures_dir)
        if not segments_by_day:
            self._pending_queue_depth = 0
            return

        contacted = False
        query_failed = False
        upload_failed = False
        client_contract_pending = False
        fail_reason: str | None = None
        pending = 0

        for day in sorted(segments_by_day.keys(), reverse=True):
            if not self._running or self._circuit_open:
                break

            # Skip past days already fully synced (unless forcing)
            if day != today and day in self._synced_days and not force_full:
                continue

            local_segments = segments_by_day[day]

            # Query server for existing segments
            server_segments = await asyncio.to_thread(
                self._client.get_server_segments, day
            )
            if server_segments is None:
                logger.warning(f"Failed to query server for day {day}")
                failure = self._failure_result_for_client("server_unreachable")
                query_failed = True
                fail_reason = failure.reason or "server_unreachable"
                if (
                    self._handle_failure_result(failure, "query")
                    and self._circuit_state == CIRCUIT_OPEN
                ):
                    fail_reason = "circuit_open"
                pending += len(local_segments)
                if self._circuit_open:
                    break
                continue
            contacted = True
            self._clear_transient_failure_state()

            # Build lookup
            server_keys: set[str] = set()
            for seg in server_segments:
                server_keys.add(seg.get("key", ""))
                if "original_key" in seg:
                    server_keys.add(seg["original_key"])

            any_needed_upload = False

            for segment_dir in local_segments:
                if not self._running or self._circuit_open:
                    break

                segment_key = segment_dir.name
                if segment_key in server_keys:
                    continue

                any_needed_upload = True
                if self._has_client_contract_failure(day, segment_dir):
                    pending += 1
                    client_contract_pending = True
                    self._record_failure_state(
                        UploadResult(
                            False,
                            reason="client_contract_recorded",
                            failure_class=FAILURE_CLIENT_CONTRACT,
                        ),
                        "upload",
                    )
                    logger.warning(
                        "Skipping previously rejected client/contract segment: %s/%s",
                        day,
                        segment_key,
                    )
                    continue

                result = await self._upload_segment(day, segment_dir)

                if not result.success:
                    upload_failed = True
                    fail_reason = result.reason or "upload_failed"
                    pending += 1

                    failure_class = self._failure_class_for_result(result)
                    if failure_class == FAILURE_CLIENT_CONTRACT:
                        client_contract_pending = True
                        self._mark_client_contract_failure(day, segment_dir, result)

                    if (
                        self._handle_failure_result(result, "upload")
                        and self._circuit_state == CIRCUIT_OPEN
                    ):
                        fail_reason = "circuit_open"
                        break
                else:
                    self._clear_transient_failure_state()

            # Mark past days as synced if nothing needed upload
            if day != today and not any_needed_upload:
                self._synced_days.add(day)
                self._save_synced_days()

        self._pending_queue_depth = pending
        if upload_failed or query_failed:
            self._record_sync_error(fail_reason or "upload_failed")
        elif client_contract_pending:
            self._last_error_reason = self._last_error_reason or "client_contract"
        elif contacted:
            self._record_sync_success()
        else:
            self._record_sync_error("server_unreachable")

        # Cleanup old synced segments
        if not self._circuit_open and self._running:
            try:
                await self._cleanup_synced_segments()
            except Exception as e:
                logger.error(f"Cleanup error: {e}", exc_info=True)

    async def _cleanup_synced_segments(self) -> None:
        """Delete synced segments older than cache_retention_days.

        Triple-gated safety:
        1. Day must be in _synced_days (fully synced locally)
        2. Segment must be older than retention threshold (unless retention=0)
        3. Segment must be confirmed present on server (fresh query)
        """
        retention = self._config.cache_retention_days
        if retention < 0:
            return

        captures_dir = self._config.captures_dir
        if not captures_dir.exists():
            return

        today = datetime.now().strftime("%Y%m%d")
        if retention > 0:
            cutoff = (datetime.now() - timedelta(days=retention)).strftime("%Y%m%d")
        else:
            cutoff = today  # 0 means delete immediately — all days qualify

        deleted_total = 0

        for day_dir in sorted(captures_dir.iterdir()):
            if not day_dir.is_dir():
                continue

            day = day_dir.name

            if not self._running:
                break

            # Gate 1: day must be in synced_days
            if day not in self._synced_days:
                continue

            # Gate 2: day must be old enough (unless retention=0)
            if retention > 0 and day >= cutoff:
                continue

            # Don't clean today's segments
            if day == today:
                continue

            # Gate 3: fresh server confirmation
            server_segments = await asyncio.to_thread(
                self._client.get_server_segments, day
            )
            if server_segments is None:
                logger.warning("Cleanup: skipping day %s — server unreachable", day)
                continue

            server_keys: set[str] = set()
            for seg in server_segments:
                server_keys.add(seg.get("key", ""))
                if "original_key" in seg:
                    server_keys.add(seg["original_key"])

            deleted_day = 0

            for stream_dir in day_dir.iterdir():
                if not stream_dir.is_dir():
                    continue

                for seg_dir in sorted(stream_dir.iterdir()):
                    if not seg_dir.is_dir():
                        continue

                    name = seg_dir.name
                    # Never touch incomplete or failed
                    if name.endswith(".incomplete") or name.endswith(".failed"):
                        continue

                    if name not in server_keys:
                        logger.warning(
                            "Cleanup: keeping %s/%s — not confirmed on server",
                            day,
                            name,
                        )
                        continue

                    shutil.rmtree(seg_dir)
                    logger.info("Cleanup: deleted %s/%s", day, name)
                    deleted_day += 1

                # Remove empty stream dir
                if stream_dir.is_dir() and not any(stream_dir.iterdir()):
                    stream_dir.rmdir()

            # Remove empty day dir
            if day_dir.is_dir() and not any(day_dir.iterdir()):
                day_dir.rmdir()

            if deleted_day:
                deleted_total += deleted_day

        if deleted_total:
            logger.info("Cleanup: deleted %d segment(s) total", deleted_total)

    def _collect_segments(self, captures_dir: Path) -> dict[str, list[Path]]:
        """Collect completed segments grouped by day."""
        result: dict[str, list[Path]] = {}

        for day_dir in sorted(captures_dir.iterdir(), reverse=True):
            if not day_dir.is_dir():
                continue

            day = day_dir.name

            for stream_dir in day_dir.iterdir():
                if not stream_dir.is_dir():
                    continue

                segments = []
                for seg_dir in sorted(stream_dir.iterdir(), reverse=True):
                    if not seg_dir.is_dir():
                        continue
                    name = seg_dir.name
                    # Skip incomplete and failed
                    if name.endswith(".incomplete") or name.endswith(".failed"):
                        continue
                    segments.append(seg_dir)

                if segments:
                    result.setdefault(day, []).extend(segments)

        return result

    async def _upload_segment(self, day: str, segment_dir: Path) -> UploadResult:
        """Upload a single segment with retry logic."""
        segment_key = segment_dir.name
        files = [f for f in segment_dir.iterdir() if f.is_file()]
        if not files:
            return UploadResult(True)  # Nothing to upload

        retry_delays = self._config.sync_retry_delays or [0]
        max_retries = max(1, self._config.sync_max_retries)
        result = UploadResult(False, reason="upload_failed")

        for attempt in range(max_retries):
            result = await asyncio.to_thread(
                self._client.upload_segment, day, segment_key, files
            )

            if result.success:
                logger.info(f"Uploaded: {day}/{segment_key} ({len(files)} files)")
                return result

            # Non-retryable errors
            if self._client.is_revoked:
                logger.error("Client revoked — disabling sync")
                return UploadResult(False, reason="revoked", failure_class=FAILURE_AUTH)

            failure_class = self._failure_class_for_result(result)
            if failure_class in {
                FAILURE_AUTH,
                FAILURE_CONFIGURATION,
                FAILURE_CLIENT_CONTRACT,
            }:
                logger.error(
                    "Upload rejected without retry: %s/%s (%s)",
                    day,
                    segment_key,
                    result.reason or failure_class,
                )
                return result

            if attempt < max_retries - 1:
                delay = retry_delays[min(attempt, len(retry_delays) - 1)]
                logger.info(
                    f"Retrying {day}/{segment_key} in {delay}s (attempt {attempt + 2})"
                )
                await asyncio.sleep(delay)

        logger.error(f"Upload failed after {max_retries} attempts: {day}/{segment_key}")
        return result
