# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import json
from pathlib import Path

from solstone_tmux.config import Config
from solstone_tmux.recovery import recover_incomplete_segments


class TestRecovery:
    """Test crash recovery for incomplete segments."""

    def _make_incomplete(
        self, captures_dir: Path, day: str, stream: str, time_prefix: str, age: int = 300
    ) -> Path:
        """Create an incomplete segment directory with a dummy file."""
        import os
        import time

        seg_dir = captures_dir / day / stream / f"{time_prefix}.incomplete"
        seg_dir.mkdir(parents=True)
        (seg_dir / f"tmux_main_screen.jsonl").write_text('{"frame_id": 1}\n')

        # Set timestamps to simulate age
        old_time = time.time() - age
        os.utime(seg_dir, (old_time, old_time))
        return seg_dir

    def test_recovers_old_incomplete(self, tmp_path: Path):
        captures_dir = tmp_path / "captures"
        self._make_incomplete(captures_dir, "20260403", "archon.tmux", "140000", age=300)

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
