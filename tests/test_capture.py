# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import json
from pathlib import Path

from solstone_tmux.capture import (
    CaptureResult,
    PaneInfo,
    TmuxCapture,
    WindowInfo,
    write_captures_jsonl,
)


class TestTmuxCapture:
    def test_result_to_dict(self):
        capture = TmuxCapture()
        result = CaptureResult(
            session="main",
            window=WindowInfo(id="@0", index=0, name="bash", active=True),
            windows=[WindowInfo(id="@0", index=0, name="bash", active=True)],
            panes=[
                PaneInfo(
                    id="%0",
                    index=0,
                    left=0,
                    top=0,
                    width=80,
                    height=24,
                    active=True,
                    content="$ ls\nfile1.txt\nfile2.txt",
                )
            ],
        )

        d = capture.result_to_dict(result, capture_id=1, relative_ts=5.0)

        assert d["frame_id"] == 1
        assert d["timestamp"] == 5.0
        assert d["analysis"]["primary"] == "tmux"
        assert d["content"]["tmux"]["session"] == "main"
        assert len(d["content"]["tmux"]["panes"]) == 1
        assert d["content"]["tmux"]["panes"][0]["width"] == 80

    def test_compute_hash_stable(self):
        capture = TmuxCapture()
        result = CaptureResult(
            session="main",
            window=WindowInfo(id="@0", index=0, name="bash", active=True),
            windows=[],
            panes=[
                PaneInfo(
                    id="%0", index=0, left=0, top=0,
                    width=80, height=24, active=True, content="hello",
                )
            ],
        )
        h1 = capture.compute_hash(result)
        h2 = capture.compute_hash(result)
        assert h1 == h2

    def test_compute_hash_changes(self):
        capture = TmuxCapture()
        result1 = CaptureResult(
            session="main",
            window=WindowInfo(id="@0", index=0, name="bash", active=True),
            windows=[],
            panes=[
                PaneInfo(
                    id="%0", index=0, left=0, top=0,
                    width=80, height=24, active=True, content="hello",
                )
            ],
        )
        result2 = CaptureResult(
            session="main",
            window=WindowInfo(id="@0", index=0, name="bash", active=True),
            windows=[],
            panes=[
                PaneInfo(
                    id="%0", index=0, left=0, top=0,
                    width=80, height=24, active=True, content="world",
                )
            ],
        )
        assert capture.compute_hash(result1) != capture.compute_hash(result2)


class TestWriteCapturesJsonl:
    def test_write_groups_by_session(self, tmp_path: Path):
        captures = [
            {
                "frame_id": 1,
                "timestamp": 0.0,
                "content": {"tmux": {"session": "main"}},
            },
            {
                "frame_id": 2,
                "timestamp": 1.0,
                "content": {"tmux": {"session": "work"}},
            },
            {
                "frame_id": 3,
                "timestamp": 2.0,
                "content": {"tmux": {"session": "main"}},
            },
        ]

        files = write_captures_jsonl(captures, tmp_path)
        assert len(files) == 2
        assert "tmux_main_screen.jsonl" in files
        assert "tmux_work_screen.jsonl" in files

        # Check main has 2 entries
        main_file = tmp_path / "tmux_main_screen.jsonl"
        lines = main_file.read_text().strip().split("\n")
        assert len(lines) == 2
        assert json.loads(lines[0])["frame_id"] == 1
        assert json.loads(lines[1])["frame_id"] == 3

    def test_write_empty(self, tmp_path: Path):
        assert write_captures_jsonl([], tmp_path) == []

    def test_sanitizes_session_name(self, tmp_path: Path):
        captures = [
            {
                "frame_id": 1,
                "timestamp": 0.0,
                "content": {"tmux": {"session": "my/session name"}},
            },
        ]
        files = write_captures_jsonl(captures, tmp_path)
        assert files == ["tmux_my_session_name_screen.jsonl"]
