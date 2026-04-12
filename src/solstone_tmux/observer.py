# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Standalone tmux terminal capture observer.

Continuously polls all active tmux sessions and captures terminal content,
creating 5-minute segments in a local cache directory. The sync service
handles all network operations — the observer only writes locally.
"""

import asyncio
import datetime
import logging
import os
import platform
import signal
import socket
import time
from pathlib import Path

from .capture import TmuxCapture, write_captures_jsonl
from .config import Config
from .streams import stream_name
from .sync import SyncService
from .upload import UploadClient

logger = logging.getLogger(__name__)

HOST = socket.gethostname()
PLATFORM = platform.system().lower()


def _get_timestamp_parts(timestamp: float | None = None) -> tuple[str, str]:
    """Get date and time parts from timestamp."""
    if timestamp is None:
        timestamp = time.time()
    dt = datetime.datetime.fromtimestamp(timestamp)
    return dt.strftime("%Y%m%d"), dt.strftime("%H%M%S")


class TmuxObserver:
    def __init__(self, config: Config):
        self.config = config
        self.interval = config.segment_interval
        self.capture_interval = config.capture_interval
        self.tmux_capture = TmuxCapture()
        self.running = True
        self.stream = config.stream
        self._client: UploadClient | None = None
        self._sync: SyncService | None = None
        self.start_at = time.time()
        self.start_at_mono = time.monotonic()
        self.segment_dir: Path | None = None
        self.captures: list[dict] = []
        self.capture_id = 0
        self.sessions_seen: set[str] = set()
        self.last_capture_time: float = 0

    def setup(self) -> bool:
        """Initialize tmux availability and upload client."""
        if not self.tmux_capture.is_available():
            logger.error("Tmux not available")
            return False

        if not self.stream:
            try:
                self.stream = stream_name(host=HOST, qualifier="tmux")
                self.config.stream = self.stream
            except ValueError as e:
                logger.error(f"Failed to derive stream name: {e}")
                return False

        self._client = UploadClient(self.config)
        if self.config.server_url:
            self._client.ensure_registered(self.config)

        self._sync = SyncService(self.config, self._client)
        logger.info(f"Observer initialized: stream={self.stream}")
        return True

    def capture(self):
        """Poll tmux and accumulate captures."""
        now = time.time()
        if now - self.last_capture_time < self.capture_interval:
            return

        active_sessions = self.tmux_capture.get_active_sessions(self.capture_interval)
        if not active_sessions:
            return

        self.last_capture_time = now

        for session_info in active_sessions:
            session = session_info["session"]
            self.sessions_seen.add(session)

            result = self.tmux_capture.capture_changed(session)
            if not result:
                continue

            self.capture_id += 1
            relative_ts = now - self.start_at
            capture_dict = self.tmux_capture.result_to_dict(
                result, self.capture_id, relative_ts
            )
            self.captures.append(capture_dict)
            logger.debug(f"Captured tmux session {session}: {len(result.panes)} panes")

    def _reset_capture_state(self):
        """Reset per-segment capture tracking."""
        self.captures = []
        self.capture_id = 0
        self.sessions_seen = set()
        self.tmux_capture.reset_hashes()
        self.last_capture_time = 0

    def _remove_empty_segment(self):
        """Remove an empty segment directory."""
        if self.segment_dir and self.segment_dir.exists():
            try:
                os.rmdir(self.segment_dir)
            except OSError:
                pass

    def finalize_segment(self):
        """Write captures to disk and trigger sync."""
        if not self.captures or not self.segment_dir:
            self._remove_empty_segment()
            self._reset_capture_state()
            return

        write_captures_jsonl(self.captures, self.segment_dir)

        # Rename from .incomplete to final HHMMSS_DDD format
        date_part, time_part = _get_timestamp_parts(self.start_at)
        duration = int(time.time() - self.start_at)
        segment_key = f"{time_part}_{duration}"
        final_dir = self.segment_dir.parent / segment_key

        try:
            os.rename(str(self.segment_dir), str(final_dir))
            logger.info(f"Segment finalized: {segment_key}")
        except OSError as e:
            logger.error(f"Failed to finalize segment: {e}")

        # Trigger sync
        if self._sync:
            self._sync.trigger()

        self._reset_capture_state()

    def _start_segment(self):
        """Start a new segment with .incomplete directory."""
        self.start_at = time.time()
        self.start_at_mono = time.monotonic()

        date_part, time_part = _get_timestamp_parts(self.start_at)
        captures_dir = self.config.captures_dir

        # Create YYYYMMDD/stream/HHMMSS.incomplete/
        segment_dir = captures_dir / date_part / self.stream / f"{time_part}.incomplete"
        segment_dir.mkdir(parents=True, exist_ok=True)
        self.segment_dir = segment_dir

    def emit_status(self):
        """Emit observe.status with current tmux capture state (fire-and-forget)."""
        if not self._client:
            return

        elapsed = int(time.monotonic() - self.start_at_mono)
        tmux_info = {
            "capturing": True,
            "captures": len(self.captures),
            "sessions": sorted(self.sessions_seen),
            "window_elapsed_seconds": elapsed,
        }
        self._client.relay_event(
            "observe",
            "status",
            mode="tmux",
            tmux=tmux_info,
            host=HOST,
            platform=PLATFORM,
            stream=self.stream,
        )

    async def main_loop(self):
        """Run the capture loop with background sync."""
        # Start sync service as background task
        sync_task = None
        if self._sync:
            sync_task = asyncio.create_task(self._sync.run())

        self._start_segment()

        try:
            while self.running:
                await asyncio.sleep(5)
                self.capture()

                elapsed = time.monotonic() - self.start_at_mono
                if elapsed >= self.interval:
                    self.finalize_segment()
                    self._start_segment()

                self.emit_status()
        finally:
            await self.shutdown()
            if sync_task:
                if self._sync:
                    self._sync.stop()
                sync_task.cancel()
                try:
                    await sync_task
                except asyncio.CancelledError:
                    pass

    async def shutdown(self):
        """Finalize the current segment and stop."""
        self.finalize_segment()
        self.segment_dir = None
        if self._client:
            self._client.stop()
            self._client = None


async def async_run(config: Config) -> int:
    """Async entry point for the observer."""
    observer = TmuxObserver(config)

    loop = asyncio.get_running_loop()

    def signal_handler():
        logger.info("Received shutdown signal")
        observer.running = False

    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, signal_handler)

    if not observer.setup():
        logger.error("Tmux observer setup failed")
        return 1

    try:
        await observer.main_loop()
    except RuntimeError as e:
        logger.error(f"Tmux observer runtime error: {e}")
        return 1
    except Exception as e:
        logger.error(f"Tmux observer error: {e}", exc_info=True)
        return 1

    return 0
