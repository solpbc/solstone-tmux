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
import time
import shutil
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any

from .config import Config
from .upload import UploadClient

logger = logging.getLogger(__name__)


class SyncService:
    """Background sync service that uploads completed segments to the server."""

    def __init__(self, config: Config, client: UploadClient):
        self._config = config
        self._client = client
        self._synced_days: set[str] = set()
        self._consecutive_failures = 0
        self._circuit_open = False
        self._last_full_sync: float = 0
        self._running = True
        self._trigger = asyncio.Event()

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

                if self._circuit_open:
                    logger.warning("Circuit breaker open — skipping sync")
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
        captures_dir = self._config.captures_dir
        if not captures_dir.exists():
            return

        today = datetime.now().strftime("%Y%m%d")

        # Collect segments by day
        segments_by_day = self._collect_segments(captures_dir)
        if not segments_by_day:
            return

        for day in sorted(segments_by_day.keys(), reverse=True):
            if not self._running:
                break

            if self._circuit_open:
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
                continue

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
                success = await self._upload_segment(day, segment_dir)

                if not success:
                    self._consecutive_failures += 1
                    if self._consecutive_failures >= 3:
                        self._circuit_open = True
                        logger.error(
                            "Circuit breaker OPEN: 3 consecutive failures across segments"
                        )
                        break
                else:
                    self._consecutive_failures = 0

            # Mark past days as synced if nothing needed upload
            if day != today and not any_needed_upload:
                self._synced_days.add(day)
                self._save_synced_days()

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

    async def _upload_segment(self, day: str, segment_dir: Path) -> bool:
        """Upload a single segment with retry logic."""
        segment_key = segment_dir.name
        files = [f for f in segment_dir.iterdir() if f.is_file()]
        if not files:
            return True  # Nothing to upload

        meta: dict[str, Any] = {"stream": self._config.stream}

        retry_delays = self._config.sync_retry_delays
        max_retries = self._config.sync_max_retries

        for attempt in range(max_retries):
            result = await asyncio.to_thread(
                self._client.upload_segment, day, segment_key, files, meta
            )

            if result.success:
                logger.info(f"Uploaded: {day}/{segment_key} ({len(files)} files)")
                return True

            # Non-retryable errors
            if self._client.is_revoked:
                logger.error("Client revoked — disabling sync")
                self._circuit_open = True
                return False

            if attempt < max_retries - 1:
                delay = retry_delays[min(attempt, len(retry_delays) - 1)]
                logger.info(
                    f"Retrying {day}/{segment_key} in {delay}s (attempt {attempt + 2})"
                )
                await asyncio.sleep(delay)

        logger.error(f"Upload failed after {max_retries} attempts: {day}/{segment_key}")
        return False
