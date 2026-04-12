# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Configuration loading and persistence for solstone-tmux.

Config lives at ~/.local/share/solstone-tmux/config/config.json.
Captures go to ~/.local/share/solstone-tmux/captures/.
"""

from __future__ import annotations

import json
import logging
import os
import stat
from dataclasses import dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)

DEFAULT_BASE_DIR = Path.home() / ".local" / "share" / "solstone-tmux"
DEFAULT_CAPTURE_INTERVAL = 5
DEFAULT_SEGMENT_INTERVAL = 300
DEFAULT_SYNC_RETRY_DELAYS = [5, 30, 120, 300]
DEFAULT_SYNC_MAX_RETRIES = 10


@dataclass
class Config:
    """Configuration for the tmux observer."""

    server_url: str = ""
    key: str = ""
    stream: str = ""
    capture_interval: int = DEFAULT_CAPTURE_INTERVAL
    segment_interval: int = DEFAULT_SEGMENT_INTERVAL
    sync_retry_delays: list[int] = field(
        default_factory=lambda: list(DEFAULT_SYNC_RETRY_DELAYS)
    )
    sync_max_retries: int = DEFAULT_SYNC_MAX_RETRIES
    cache_retention_days: int = 7
    base_dir: Path = DEFAULT_BASE_DIR

    @property
    def captures_dir(self) -> Path:
        return self.base_dir / "captures"

    @property
    def config_dir(self) -> Path:
        return self.base_dir / "config"

    @property
    def state_dir(self) -> Path:
        return self.base_dir / "state"

    @property
    def config_path(self) -> Path:
        return self.config_dir / "config.json"

    def ensure_dirs(self) -> None:
        """Create all required directories."""
        self.captures_dir.mkdir(parents=True, exist_ok=True)
        self.config_dir.mkdir(parents=True, exist_ok=True)
        self.state_dir.mkdir(parents=True, exist_ok=True)


def load_config(base_dir: Path | None = None) -> Config:
    """Load config from disk, returning defaults if not found."""
    config = Config()
    if base_dir:
        config.base_dir = base_dir

    config_path = config.config_path
    if not config_path.exists():
        return config

    try:
        with open(config_path, encoding="utf-8") as f:
            data = json.load(f)
    except (json.JSONDecodeError, OSError) as e:
        logger.warning(f"Failed to load config from {config_path}: {e}")
        return config

    config.server_url = data.get("server_url", "")
    config.key = data.get("key", "")
    config.stream = data.get("stream", "")
    config.capture_interval = data.get("capture_interval", DEFAULT_CAPTURE_INTERVAL)
    config.segment_interval = data.get("segment_interval", DEFAULT_SEGMENT_INTERVAL)
    if "sync_retry_delays" in data:
        config.sync_retry_delays = data["sync_retry_delays"]
    if "sync_max_retries" in data:
        config.sync_max_retries = data["sync_max_retries"]
    if "cache_retention_days" in data:
        try:
            config.cache_retention_days = int(data["cache_retention_days"])
        except (ValueError, TypeError):
            pass

    return config


def save_config(config: Config) -> None:
    """Save config to disk with user-only permissions."""
    config.ensure_dirs()

    data = {
        "server_url": config.server_url,
        "key": config.key,
        "stream": config.stream,
        "capture_interval": config.capture_interval,
        "segment_interval": config.segment_interval,
        "sync_retry_delays": config.sync_retry_delays,
        "sync_max_retries": config.sync_max_retries,
        "cache_retention_days": config.cache_retention_days,
    }

    config_path = config.config_path
    tmp_path = config_path.with_suffix(f".{os.getpid()}.tmp")

    with open(tmp_path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)
        f.write("\n")

    # Set user-only read/write before moving into place
    os.chmod(tmp_path, stat.S_IRUSR | stat.S_IWUSR)
    os.rename(str(tmp_path), str(config_path))
    logger.info(f"Config saved to {config_path}")
