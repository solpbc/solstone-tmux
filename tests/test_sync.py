# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from solstone_tmux.config import Config
from solstone_tmux.recovery import recover_incomplete_segments
from solstone_tmux.sync import SyncService
from solstone_tmux.upload import UploadClient


class TestRecovery:
    """Test crash recovery for incomplete segments."""

    def _make_incomplete(
        self,
        captures_dir: Path,
        day: str,
        stream: str,
        time_prefix: str,
        age: int = 300,
    ) -> Path:
        """Create an incomplete segment directory with a dummy file."""
        import os
        import time

        seg_dir = captures_dir / day / stream / f"{time_prefix}.incomplete"
        seg_dir.mkdir(parents=True)
        (seg_dir / "tmux_main_screen.jsonl").write_text('{"frame_id": 1}\n')

        # Set timestamps to simulate age
        old_time = time.time() - age
        os.utime(seg_dir, (old_time, old_time))
        return seg_dir

    def test_recovers_old_incomplete(self, tmp_path: Path):
        captures_dir = tmp_path / "captures"
        self._make_incomplete(
            captures_dir, "20260403", "archon.tmux", "140000", age=300
        )

        recovered = recover_incomplete_segments(captures_dir)
        assert recovered == 1

        # Should be renamed to HHMMSS_DDD
        stream_dir = captures_dir / "20260403" / "archon.tmux"
        dirs = [d.name for d in stream_dir.iterdir() if d.is_dir()]
        assert len(dirs) == 1
        assert dirs[0].startswith("140000_")
        assert not dirs[0].endswith(".incomplete")

    def test_skips_recent_incomplete(self, tmp_path: Path):
        captures_dir = tmp_path / "captures"
        seg_dir = captures_dir / "20260403" / "archon.tmux" / "140000.incomplete"
        seg_dir.mkdir(parents=True)
        (seg_dir / "test.jsonl").write_text("{}\n")
        # Don't age it — it's recent

        recovered = recover_incomplete_segments(captures_dir)
        assert recovered == 0
        assert seg_dir.exists()

    def test_marks_empty_as_failed(self, tmp_path: Path):
        captures_dir = tmp_path / "captures"
        import os
        import time

        seg_dir = captures_dir / "20260403" / "archon.tmux" / "140000.incomplete"
        seg_dir.mkdir(parents=True)
        # No files inside — should fail
        old_time = time.time() - 300
        os.utime(seg_dir, (old_time, old_time))

        recovered = recover_incomplete_segments(captures_dir)
        assert recovered == 0

        # Should be renamed to .failed
        failed_dir = captures_dir / "20260403" / "archon.tmux" / "140000.failed"
        assert failed_dir.exists()

    def test_no_captures_dir(self, tmp_path: Path):
        assert recover_incomplete_segments(tmp_path / "nonexistent") == 0


class TestSyncServiceCollect:
    """Test segment collection logic."""

    def test_skips_incomplete_and_failed(self, tmp_path: Path):
        from solstone_tmux.sync import SyncService
        from solstone_tmux.upload import UploadClient

        config = Config(base_dir=tmp_path)
        config.ensure_dirs()

        captures = config.captures_dir
        stream_dir = captures / "20260403" / "archon.tmux"
        stream_dir.mkdir(parents=True)

        # Create various segment dirs
        (stream_dir / "140000_300").mkdir()
        (stream_dir / "140000_300" / "test.jsonl").write_text("{}\n")
        (stream_dir / "145000.incomplete").mkdir()
        (stream_dir / "143000.failed").mkdir()
        (stream_dir / "150000_300").mkdir()
        (stream_dir / "150000_300" / "test.jsonl").write_text("{}\n")

        client = UploadClient(config)
        sync = SyncService(config, client)

        segments = sync._collect_segments(captures)
        assert "20260403" in segments
        names = [s.name for s in segments["20260403"]]
        assert "140000_300" in names
        assert "150000_300" in names
        assert "145000.incomplete" not in names
        assert "143000.failed" not in names


class TestCleanupSyncedSegments:
    """Test cache retention cleanup of synced segments."""

    def _make_sync(self, tmp_path: Path, retention: int = 7) -> SyncService:
        config = Config(base_dir=tmp_path)
        config.cache_retention_days = retention
        config.ensure_dirs()
        client = UploadClient(config)
        return SyncService(config, client)

    def _create_segment(
        self, captures_dir: Path, day: str, stream: str, name: str
    ) -> Path:
        seg_dir = captures_dir / day / stream / name
        seg_dir.mkdir(parents=True, exist_ok=True)
        (seg_dir / "test.jsonl").write_text("{}\n")
        return seg_dir

    @pytest.mark.asyncio
    async def test_deletes_old_synced_confirmed(self, tmp_path: Path):
        """Segments in synced_days + confirmed on server + old enough -> deleted."""
        sync = self._make_sync(tmp_path, retention=7)
        captures = sync._config.captures_dir

        self._create_segment(captures, "20260101", "archon.tmux", "120000_300")
        sync._synced_days.add("20260101")

        server_response = [{"key": "120000_300"}]
        with patch(
            "asyncio.to_thread", new_callable=AsyncMock, return_value=server_response
        ):
            await sync._cleanup_synced_segments()

        assert not (captures / "20260101" / "archon.tmux" / "120000_300").exists()

    @pytest.mark.asyncio
    async def test_keeps_unconfirmed_on_server(self, tmp_path: Path):
        """Segments in synced_days + NOT on server -> not deleted."""
        sync = self._make_sync(tmp_path, retention=7)
        captures = sync._config.captures_dir

        self._create_segment(captures, "20260101", "archon.tmux", "120000_300")
        sync._synced_days.add("20260101")

        server_response = [{"key": "999999_300"}]
        with patch(
            "asyncio.to_thread", new_callable=AsyncMock, return_value=server_response
        ):
            await sync._cleanup_synced_segments()

        assert (captures / "20260101" / "archon.tmux" / "120000_300").exists()

    @pytest.mark.asyncio
    async def test_keeps_segments_not_in_synced_days(self, tmp_path: Path):
        """Segments NOT in synced_days -> not deleted."""
        sync = self._make_sync(tmp_path, retention=7)
        captures = sync._config.captures_dir

        self._create_segment(captures, "20260101", "archon.tmux", "120000_300")

        with patch("asyncio.to_thread", new_callable=AsyncMock) as mock_thread:
            await sync._cleanup_synced_segments()

        assert (captures / "20260101" / "archon.tmux" / "120000_300").exists()
        mock_thread.assert_not_called()

    @pytest.mark.asyncio
    async def test_keeps_when_server_unreachable(self, tmp_path: Path):
        """Server unreachable (returns None) -> nothing deleted."""
        sync = self._make_sync(tmp_path, retention=7)
        captures = sync._config.captures_dir

        self._create_segment(captures, "20260101", "archon.tmux", "120000_300")
        sync._synced_days.add("20260101")

        with patch("asyncio.to_thread", new_callable=AsyncMock, return_value=None):
            await sync._cleanup_synced_segments()

        assert (captures / "20260101" / "archon.tmux" / "120000_300").exists()

    @pytest.mark.asyncio
    async def test_never_touches_incomplete_or_failed(self, tmp_path: Path):
        """.incomplete and .failed segments are never deleted."""
        sync = self._make_sync(tmp_path, retention=7)
        captures = sync._config.captures_dir

        self._create_segment(captures, "20260101", "archon.tmux", "120000.incomplete")
        self._create_segment(captures, "20260101", "archon.tmux", "130000.failed")
        self._create_segment(captures, "20260101", "archon.tmux", "140000_300")
        sync._synced_days.add("20260101")

        server_response = [{"key": "140000_300"}]
        with patch(
            "asyncio.to_thread", new_callable=AsyncMock, return_value=server_response
        ):
            await sync._cleanup_synced_segments()

        assert (captures / "20260101" / "archon.tmux" / "120000.incomplete").exists()
        assert (captures / "20260101" / "archon.tmux" / "130000.failed").exists()
        assert not (captures / "20260101" / "archon.tmux" / "140000_300").exists()

    @pytest.mark.asyncio
    async def test_retention_negative_one_keeps_forever(self, tmp_path: Path):
        """cache_retention_days = -1 -> nothing deleted."""
        sync = self._make_sync(tmp_path, retention=-1)
        captures = sync._config.captures_dir

        self._create_segment(captures, "20260101", "archon.tmux", "120000_300")
        sync._synced_days.add("20260101")

        with patch("asyncio.to_thread", new_callable=AsyncMock) as mock_thread:
            await sync._cleanup_synced_segments()

        assert (captures / "20260101" / "archon.tmux" / "120000_300").exists()
        mock_thread.assert_not_called()

    @pytest.mark.asyncio
    async def test_retention_zero_deletes_immediately(self, tmp_path: Path):
        """cache_retention_days = 0 -> deletes immediately (no age check)."""
        sync = self._make_sync(tmp_path, retention=0)
        captures = sync._config.captures_dir

        from datetime import datetime, timedelta

        yesterday = (datetime.now() - timedelta(days=1)).strftime("%Y%m%d")

        self._create_segment(captures, yesterday, "archon.tmux", "120000_300")
        sync._synced_days.add(yesterday)

        server_response = [{"key": "120000_300"}]
        with patch(
            "asyncio.to_thread", new_callable=AsyncMock, return_value=server_response
        ):
            await sync._cleanup_synced_segments()

        assert not (captures / yesterday / "archon.tmux" / "120000_300").exists()

    @pytest.mark.asyncio
    async def test_never_cleans_today(self, tmp_path: Path):
        """Today's segments are never cleaned, even with retention=0."""
        sync = self._make_sync(tmp_path, retention=0)
        captures = sync._config.captures_dir

        from datetime import datetime

        today = datetime.now().strftime("%Y%m%d")

        self._create_segment(captures, today, "archon.tmux", "120000_300")
        sync._synced_days.add(today)

        with patch("asyncio.to_thread", new_callable=AsyncMock) as mock_thread:
            await sync._cleanup_synced_segments()

        assert (captures / today / "archon.tmux" / "120000_300").exists()
        mock_thread.assert_not_called()

    @pytest.mark.asyncio
    async def test_cleans_empty_dirs(self, tmp_path: Path):
        """Empty stream and day dirs are removed after segment deletion."""
        sync = self._make_sync(tmp_path, retention=7)
        captures = sync._config.captures_dir

        self._create_segment(captures, "20260101", "archon.tmux", "120000_300")
        sync._synced_days.add("20260101")

        server_response = [{"key": "120000_300"}]
        with patch(
            "asyncio.to_thread", new_callable=AsyncMock, return_value=server_response
        ):
            await sync._cleanup_synced_segments()

        assert not (captures / "20260101" / "archon.tmux").exists()
        assert not (captures / "20260101").exists()

    @pytest.mark.asyncio
    async def test_original_key_lookup(self, tmp_path: Path):
        """Server segment with original_key should match local segment."""
        sync = self._make_sync(tmp_path, retention=7)
        captures = sync._config.captures_dir

        self._create_segment(captures, "20260101", "archon.tmux", "120000_300")
        sync._synced_days.add("20260101")

        server_response = [{"key": "renamed_key", "original_key": "120000_300"}]
        with patch(
            "asyncio.to_thread", new_callable=AsyncMock, return_value=server_response
        ):
            await sync._cleanup_synced_segments()

        assert not (captures / "20260101" / "archon.tmux" / "120000_300").exists()
