# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import json
from pathlib import Path

from solstone_tmux.config import Config, load_config, save_config


class TestConfig:
    def test_defaults(self):
        config = Config()
        assert config.server_url == ""
        assert config.key == ""
        assert config.capture_interval == 5
        assert config.segment_interval == 300

    def test_captures_dir(self):
        config = Config()
        assert config.captures_dir == config.base_dir / "captures"

    def test_round_trip(self, tmp_path: Path):
        config = Config(base_dir=tmp_path)
        config.server_url = "https://example.com"
        config.key = "test-key-123"
        config.stream = "archon.tmux"
        config.capture_interval = 10

        save_config(config)

        loaded = load_config(tmp_path)
        assert loaded.server_url == "https://example.com"
        assert loaded.key == "test-key-123"
        assert loaded.stream == "archon.tmux"
        assert loaded.capture_interval == 10

    def test_load_missing(self, tmp_path: Path):
        config = load_config(tmp_path)
        assert config.server_url == ""
        assert config.key == ""

    def test_load_corrupt(self, tmp_path: Path):
        config_dir = tmp_path / "config"
        config_dir.mkdir(parents=True)
        (config_dir / "config.json").write_text("not json!")

        config = load_config(tmp_path)
        assert config.server_url == ""

    def test_permissions(self, tmp_path: Path):
        config = Config(base_dir=tmp_path)
        config.server_url = "https://example.com"
        config.key = "secret"
        save_config(config)

        mode = config.config_path.stat().st_mode & 0o777
        assert mode == 0o600
