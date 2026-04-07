# AGENTS.md

Development guidelines for solstone-tmux, a standalone tmux terminal observer for solstone.

## Project Overview

solstone-tmux is a companion app that runs alongside the main solstone journal. It captures all active tmux terminal sessions every 5 seconds, accumulates captures into 5-minute segments in a local cache, and syncs completed segments to the solstone ingest API. Pure Python, no system dependencies beyond tmux itself. Works offline -- segments sync when the server becomes available. Recovers incomplete segments on startup after crashes.

This is a **solstone observer** -- a standalone capture agent that feeds data into a solstone journal. It follows the same patterns as solstone-macos (the macOS screen/audio observer) but captures terminal content instead of screen recordings.

## Source Layout

```
src/solstone_tmux/
    __init__.py         Package init, version
    cli.py              CLI entry point (run, setup, install-service, status)
    config.py           Config loading/persistence (~/.local/share/solstone-tmux/)
    capture.py          Tmux capture library (polls sessions, panes, deduplication)
    observer.py         Main capture loop with segment rotation
    streams.py          Stream name derivation (hostname.tmux convention)
    sync.py             Background sync service (uploads segments to server)
    upload.py           HTTP upload client for solstone ingest API
    recovery.py         Crash recovery for orphaned .incomplete segments
tests/
    test_capture.py     Capture result serialization, hashing, JSONL writing
    test_config.py      Config round-trip, defaults, permissions
    test_streams.py     Stream name derivation and hostname stripping
    test_sync.py        Recovery and segment collection logic
contrib/
    solstone-tmux.service   Reference systemd unit file
```

## Build and Test Commands

```bash
make install        # Create venv, install package in editable mode with dev deps
make test           # Run all tests with pytest
make test-only TEST=tests/test_capture.py   # Run a specific test file
make test-only TEST="-k test_function_name" # Run tests matching a pattern
make format         # Auto-format and lint with ruff
make ci             # Full CI: format check + lint + tests
make clean          # Remove build artifacts and caches
make uninstall      # Remove venv and all artifacts
make clean-install  # Clean everything and reinstall from scratch
```

## Development Principles

- **Pure Python, minimal dependencies.** Runtime dependency is `requests` only. No frameworks, no heavy libraries. Keep it lean.
- **Stdlib over libraries.** Use `subprocess` for tmux interaction, `asyncio` for the event loop, `dataclasses` for data structures.
- **Atomic writes.** Write to `.tmp` then `os.rename()` for config and state persistence.
- **Offline-first.** Captures always write to local cache. Sync is best-effort with retry and circuit breaker.
- **Crash recovery.** `.incomplete` segment directories get recovered on startup. `.failed` directories are quarantined.
- **Test everything, mock external state.** Tests must never call real tmux or real HTTP endpoints. Use `tmp_path`, monkeypatch, and fixtures to isolate completely.

## File Headers

All Python source files must include this header as the first two lines:

```python
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
```

Add this to new `.py` files in `src/solstone_tmux/` and `tests/`. Do not add headers to TOML, Makefile, or markdown files.

## Architecture Notes

### Capture Loop

The observer (`observer.py`) runs a single `asyncio` event loop. Every 5 seconds it polls tmux for active sessions, captures changed panes, and accumulates captures in memory. Every 5 minutes (configurable), it finalizes the segment: writes JSONL files to disk and triggers the sync service.

### Segment Format

Segments live under `~/.local/share/solstone-tmux/captures/YYYYMMDD/stream/HHMMSS_DDD/` where `DDD` is duration in seconds. During recording, the directory has a `.incomplete` suffix. Each segment contains one JSONL file per tmux session (`tmux_{session}_screen.jsonl`).

### Sync Service

The `SyncService` runs as a background `asyncio` task. It walks cached days newest-to-oldest, queries the server for existing segments, and uploads missing ones. A circuit breaker opens after 3 consecutive failures.

### Registration

Observer registration tries `sol observer create` via CLI first (works without a running server if sol is on PATH), falling back to HTTP registration at the server's `/app/observer/api/create` endpoint.

## Config

Config file: `~/.local/share/solstone-tmux/config/config.json`

```json
{
  "server_url": "http://localhost:5015",
  "key": "<observer-api-key>",
  "stream": "<hostname>.tmux",
  "capture_interval": 5,
  "segment_interval": 300
}
```

## License

AGPL-3.0-only. Copyright (c) 2026 sol pbc.
