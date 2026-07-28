# AGENTS.md

Development guidelines for solstone-tmux, a standalone tmux terminal observer for solstone.

## Project Overview

solstone-tmux is one of the owner's observers. It experiences tmux sessions along with the owner — every 5 seconds it takes in what's on each active pane, accumulating observations into 5-minute segments in a local cache, and syncing completed segments to the solstone ingest API. The installed product is the Python package and has no system dependency beyond tmux itself. It works offline -- segments sync when the server becomes available -- and recovers incomplete segments on startup after crashes.

This is a **solstone observer** -- a standalone companion that feeds observations into a solstone journal. It follows the same patterns as solstone-macos (the macOS screen/audio observer) but experiences terminal content instead of screen and audio.

The repository also contains a transitional Rust local observation plane side-by-side
with Python. It is not the installed product, is not wired into the repository service
install targets, and has no networking capability.

## Source Layout

```
Cargo.toml                         Virtual Rust workspace
Cargo.lock                         Locked native dependency graph
rust-toolchain.toml                Rust version, components, authoritative target list
src/solstone_tmux/
    __init__.py                    Package init, version
    cli.py                         Installed CLI (run, setup, install-service, status)
    config.py                      Config loading/persistence
    capture.py                     Tmux capture library
    observer.py                    Installed capture loop and segment rotation
    indicator.py                   Tmux status-left indicator
    streams.py                     Stream name derivation
    sync.py                        Background sync service
    upload.py                      HTTP upload client
    recovery.py                    Python crash recovery
    install_guard.py               Installed-service repository guard
    contrib/
        solstone-tmux.service.in   Installed Python systemd unit template
native/solstone-tmux-observer/     Transitional native local plane (not installed product)
    Cargo.toml
    src/
        lib.rs                     Shared native crate surface
        main.rs                    Process startup and command dispatch
        cli.rs                     Four-command argument parser
        clock.rs                   Wall/monotonic clock seam
        command.rs                 Bounded argv command seam
        config.rs                  Read-only native runtime config + hostname defaults
        model.rs                   Shared capture domain model
        tmux.rs                    Tmux grammar and complete transactions
        serialize.rs               Python-compatible JSONL serialization
        name.rs                    Injective safe filename derivation
        paths.rs, paths/           Shared path seam + Linux/macOS policies
        storage.rs                 Durable append and atomic writes
        segment.rs                 Segment state, dedup, rotation, finalization
        recovery.rs                Locked native recovery
        instance_lock.rs           Exclusive data-root lock
        indicator.rs               Owned tmux indicator state
        observer.rs                Tokio poll and shutdown lifecycle
        service.rs, service/       Runtime-selected systemd/launchd backends
    tests/
        data/tmux/                 Verbatim observed tmux stdout fixtures
        data/golden/               Verbatim Python-authored JSONL fixtures
        data/launchd/              Verbatim-realistic launchctl print stdout fixtures
        *.rs                       Native integration tests
tests/
    test_capture.py                Capture serialization, hashing, JSONL writing
    test_cli.py                    Installed CLI behavior
    test_config.py                 Config round-trip, defaults, permissions
    test_indicator.py              Python indicator behavior
    test_install_guard.py          Installed-service guard
    test_observer.py               Python observer lifecycle
    test_release.py                Release helpers
    test_streams.py                Stream name derivation
    test_sync.py                   Segment collection and sync behavior
    test_upload.py                 Upload client behavior
```

## Build and Test Commands

```bash
make install        # Install Python dev env and build the locked Rust workspace
make test           # Run Python and locked Rust workspace tests
make test-only TEST=tests/test_capture.py   # Run a specific test file
make test-only TEST="-k test_function_name" # Run tests matching a pattern
make format         # Format/lint Python and format the Rust workspace
make ci             # Full ordered Python + Rust offline gate and target matrix
make clean          # Remove Python and Rust build artifacts and caches
make install-service # Smart install or upgrade of the systemd service (guard-checked)
make uninstall-service # Remove the installed service and pipx package (guard-checked)
make clean-install  # Clean everything and reinstall from scratch

# Narrow checks
.venv/bin/pytest tests/test_capture.py -q
cargo test --locked -p solstone-tmux-observer --test tmux_adapter
cargo test --locked -p solstone-tmux-observer test_name
```

`make ci` runs, in order: repository guards; Python format, lint, and tests;
Rust format, clippy, and tests; the offline Rust license/source/ban policy;
then `cargo check --locked --workspace --all-targets --target <target>` for every
target read from `rust-toolchain.toml`. Every Cargo invocation that resolves
dependencies uses `--locked`. The target list exists only in `rust-toolchain.toml`;
`scripts/rust-targets.sh` parses it for CI and drift checks.
`cargo-deny --version` must be exactly `cargo-deny 0.20.2` or CI stops with an
installation message. Advisories are deliberately excluded because cargo-deny's
advisory database requires network access, while this CI policy is deterministic and
offline.

The matrix emitted by `make ci` has this meaning:

```text
Rust target evidence (cargo check --locked --workspace --all-targets --target <target>; no linked/native artifact claim):
<host target from rust-toolchain.toml>: PASS — host cargo check; no executable linked
<non-host target from rust-toolchain.toml>: PASS — cross-target type/check only; no native binary produced
```

Every non-host row is `cargo check` evidence only. It proves no linked or runnable
native artifact.

## Development Principles

- **Installed Python product.** Its runtime dependency remains `requests` only. Prefer `subprocess`, `asyncio`, `dataclasses`, and other stdlib facilities over convenience dependencies.
- **Transitional native plane.** It has no networking or HTTP/TLS dependencies, runs as one Tokio process, keeps capture/serialization/segment/recovery behavior shared across platforms, and uses platform adaptation only for paths and service lifecycle.
- **No unsafe application code.** Every Rust crate root has `#![forbid(unsafe_code)]`; safe dependency APIs may encapsulate platform primitives.
- **KISS and locked dependencies.** Add no framework or optional mechanism the contract does not require. Commit `Cargo.lock` and use `--locked`.
- **Atomic writes.** Write to `.tmp` then `os.rename()` for config and state persistence.
- **Offline-first.** Captures always write to local cache. Sync is best-effort with retry and circuit breaker.
- **Crash recovery.** `.incomplete` segment directories get recovered on startup. `.failed` directories are quarantined.
- **Test everything, mock external state.** Tests must never call real tmux, systemd, launchd, or HTTP endpoints. Use isolated paths and injected environment, clock, command, storage, and shutdown seams.

## File Headers

All Python source files must include this header as the first two lines:

```python
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
```

Add this to new `.py` files in `src/solstone_tmux/` and `tests/`.

Every Rust source and test file must start with:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
```

Rust crate roots must also contain `#![forbid(unsafe_code)]`. Do not add headers to
TOML, Makefile, or markdown files.

## Architecture Notes

### Capture Loop

The installed observer (`observer.py`) runs a single `asyncio` event loop. Every 5
seconds it polls tmux for active sessions, captures changed panes, and accumulates
captures in memory. Every 5 minutes (configurable), it finalizes the segment, writes
JSONL files, and triggers the sync service.

The transitional native local plane uses one Tokio process. It constructs complete
per-session tmux transactions, serializes Python-compatible envelopes, durably appends
changed sessions, rotates on monotonic time, and supervises shutdown. Its shared
capture, segment, durability, and recovery code is identical on Linux and macOS. This
does not make it the installed observer. Its `run` command acquires the data-root lock,
loads the separate native `<config_root>/config.json`, recovers configured streams,
opens a segment, installs the owned tmux indicator, and enters the supervised poll loop.
It does not read or migrate the installed Python product's config.

### Tmux grammar compatibility

The native adapter parses `client_activity` from the last space in each
`list-clients` row, so session names containing spaces remain intact. Python currently
splits from the left in `capture.py` and silently drops those rows when the remainder
is not an integer. That divergence is intentional: Rust follows the real tmux grammar,
and changing Python is out of scope for the transitional local plane.

### Segment Format

Segments live under
`~/.local/share/solstone-tmux/captures/YYYYMMDD/stream/HHMMSS_DDD/`, where `DDD`
is duration in seconds. During capture, the directory has a `.incomplete` suffix.
Each segment contains one JSONL file per tmux session
(`tmux_{session}_screen.jsonl`).

The native plane keeps Rust-only recovery state in a sibling
`HHMMSS.incomplete.meta` regular file in the stream directory, never inside the
segment directory. Python ignores that sibling file. Native stream and session
identities use the same safe injective filename derivation, so slash/space,
case-folding, and Unicode-normalization aliases cannot share a file. The canonical
session `main` still produces `tmux_main_screen.jsonl`.

### Service lifecycle

The installed Python service remains `solstone-tmux.service` and continues to be
managed only by the repository's existing pipx-oriented service targets. The
transitional native backends use
`solstone-tmux-observer.service` on systemd-user and
`com.solstone.tmux-observer` on launchd. Native code must never replace, stop,
unload, or otherwise control the live Python unit.

The launchd plist retains Apple's standard
`http://www.apple.com/DTDs/PropertyList-1.0.dtd` document-type identifier. It is emitted
as plist syntax and is never fetched; the native crate has no networking code.

### Sync Service

The `SyncService` runs as a background `asyncio` task. It walks cached days newest-to-oldest, queries the server for existing segments, and uploads missing ones. A circuit breaker opens after 3 consecutive failures.

`SyncService` keeps the sync-related health facts in memory. The observer rides an 8-field diagnostics beacon on each `observe.status` event: `name`, `stream_type`, `version`, `uptime`, `last_successful_sync`, `pending_queue_depth`, `recent_error_count`, and `last_error_reason`. The beacon excludes observed-user data. Journal-side health may independently report ingest rejections separate from this observer-side beacon.

### Registration

On first run the observer self-registers over HTTP directly to the journal's `/app/observer/register` endpoint, sending a descriptor (`platform`, `hostname`, `stream_type`, `version`). The journal returns a handle, which is cached in the config and presented as an `Authorization: Bearer` token on every later upload. Registration runs once — a cached handle is reused.

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

## Brand canon

- **solstone-tmux is an observer.** Owner-facing, `solstone = observers + your journal` — sol is the keeper who lives in and tends your journal, not a separately enumerated part. In the architecture/engineering register the same system is `observers + sol agent + journal`, where the keeper runs as `sol agent`. This repo implements one of those observers.
- **Use co-experience language in branded prose.** In README, INSTALL, onboarding text, settings copy, and error messages, describe solstone-tmux as something that experiences tmux sessions along with the owner. Never describe it as watching, capturing, recording, monitoring, or tracking the owner.
- **Keep code language in code-only contexts.** Internal architecture terms such as the `Capture Loop` heading, the `capture.py` module, the `~/.local/share/solstone-tmux/captures/` on-disk path, and the `capture_interval` config key are canon-permitted here and must not be renamed just to match branded prose.
- **Edit with the surface in mind.** If the owner sees the string, follow the canon. If the text is naming code, pipelines, modules, or storage artifacts for engineers, the existing internal vocabulary stays.

## Releasing

solstone-tmux is released to PyPI via an operator-driven script. There is no
CI/CD: every cut is hand-run from a clean tree.

Tokens live in the operator's vault — never in the repo. Export the appropriate
token before running:

- `PYPI_TOKEN` for production (`make release`)
- `TESTPYPI_TOKEN` for dry-run uploads to TestPyPI (`make release-test`)

Cut steps (operator):

1. Bump `version = "x.y.z"` in `pyproject.toml` and the matching `__version__`
   in `src/solstone_tmux/__init__.py`.
2. Add a new `## [x.y.z] - YYYY-MM-DD` block to `CHANGELOG.md`. Mirror the
   existing `0.1.0` block as the template — plain owner-facing voice; no
   surveillance verbs (see `## Brand canon`).
3. Commit the version bump + changelog on a clean tree.
4. `TESTPYPI_TOKEN=… make release-test` — uploads to TestPyPI only. No tag,
   no GitHub Release. Use this to sanity-check the artifacts.
5. `PYPI_TOKEN=… make release` — builds, uploads to PyPI, creates `vX.Y.Z`
   tag, pushes the tag, and creates a GitHub Release with the sdist + wheel
   attached and the matching CHANGELOG block as release notes.

If `gh release create` fails after the PyPI upload, the script prints the
exact `gh release create …` command to re-run manually. PyPI versions are
immutable, so do not re-bump on failure — just complete the GitHub side.

The `scripts/extract_changelog.sh` helper pulls a single version block out of
`CHANGELOG.md`. It is unit-tested in `tests/test_release.py`.

## License

AGPL-3.0-only. Copyright (c) 2026 sol pbc.
