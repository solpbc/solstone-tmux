# AGENTS.md

Development guidelines for solstone-tmux, a standalone tmux terminal observer for solstone.

## Project Overview

solstone-tmux is one of the owner's observers. It experiences tmux sessions along with the owner — every 5 seconds it takes in what's on each active pane, accumulating observations into 5-minute segments in a local cache, and syncing completed segments to the solstone ingest API. The installed product is the Python package and has no system dependency beyond tmux itself. It works offline -- segments sync when the server becomes available -- and recovers incomplete segments on startup after crashes.

This is a **solstone observer** -- a standalone companion that feeds observations into a solstone journal. It follows the same patterns as solstone-macos (the macOS screen/audio observer) but experiences terminal content instead of screen and audio.

The repository also contains a transitional Rust local observation plane side-by-side
with Python. It is not the installed product, is not wired into the repository service
install targets, and adds a private-link networking plane: stdin-only pairing, one
capability-gated loopback bridge over pinned SPL transport, a Journal observer-client
v2 client, sequential bounded sync with SHA-256 custody checks, retention, and
lock-bound health diagnostics.

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
    contracts/
        observer-client-import.json  Vendored-contract provenance
    vendor/
        observer-client-contract/   Journal observer-client authority bundle
    src/
        lib.rs                     Shared native crate surface
        main.rs                    Process startup and command dispatch
        cli.rs                     Five-command argument parser, including setup
        clock.rs                   Wall/monotonic clock seam
        command.rs                 Bounded argv command seam
        config.rs                  Native runtime config, owner-only permissions, hostname defaults
        private_link.rs             Pairing, private state, SPL carrier, bridge lifecycle
        journal.rs                  Journal v2 client and streaming multipart
        sync.rs                     Bounded scheduler, custody, and retention
        health.rs                   Closed diagnostics and lock-bound health snapshot
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
        support/private_link_peer.rs  Shared loopback SPL peer test harness
        observer_contract.rs       Offline vendored-contract provenance gate
        private_state.rs           Pairing and private-state behavior
        bridge_composition.rs      Real private-link bridge composition and streaming
        journal_contract.rs        Fixture-driven Journal wire contract
        sync_custody.rs            Exact all-file custody predicate
        retention.rs               Fail-safe retention behavior
        sync_scheduler.rs          Controlled-time bounded sync and backoff
        health_status.rs           Snapshot liveness and non-mutating status
        supervision.rs             Sync failure and shutdown ordering
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
make ci             # Full ordered gate, graph projection, and host compile evidence
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
then one host-only `cargo check --locked --workspace --all-targets`. Every Cargo
invocation that resolves dependencies uses `--locked`. The target list exists only in
`rust-toolchain.toml`; `scripts/rust-targets.sh` parses it for CI and drift checks.
`cargo-deny --version` must be exactly `cargo-deny 0.20.2` or CI stops with an
installation message. Advisories are deliberately excluded because cargo-deny's
advisory database requires network access, while this CI policy is deterministic and
offline.

The two Rust evidence blocks emitted by `make ci` have distinct meanings:

```text
Rust dependency graph evidence (cargo deny --offline --locked with deny.toml [graph].targets; resolution only; no compile, link, runnable, or native-artifact claim):
<each configured target>: PASS — dependency graph resolves; resolution only; no compile, link, runnable, or native-artifact claim

Rust host compile evidence (cargo check --locked --workspace --all-targets; host only; no executable linked; no native-artifact claim):
<configured host target>: PASS — host cargo check; no executable linked; no native-artifact claim
```

The dependency block proves offline graph resolution for every configured target; it
does not claim compilation, linking, a runnable result, or a native artifact. The host
block proves type checking only on the current host and makes no linked or native
artifact claim.

### Native dependency policy

The native graph pins `spl-core` and `spl-transport` to an exact Git revision and
allows only the declared crates.io registry and SPL Git source. In addition to the
project's AGPL, Apache, MIT, and Unicode licenses, `deny.toml` permits BSD-3-Clause for
`subtle`, ISC for the ring/rustls family, and CDLA-Permissive-2.0 for
`webpki-roots`. Its `[graph].targets` projects dependency resolution across every
configured Rust target. The repository guard requires that ordered projection to
match `scripts/rust-targets.sh`, whose sole authority remains `rust-toolchain.toml`;
compilation remains host-only.

## Development Principles

- **Installed Python product.** Its runtime dependency remains `requests` only. Prefer `subprocess`, `asyncio`, `dataclasses`, and other stdlib facilities over convenience dependencies.
- **Transitional native plane.** It runs as one Tokio process, uses revision-pinned SPL transport through one capability-gated loopback bridge, and performs sequential bounded Journal sync. Capture/serialization/segment/recovery behavior stays shared across platforms, platform adaptation remains limited to paths and service lifecycle, and missing credentials never stop local capture or rotation.
- **No unsafe application code.** Every Rust crate root has `#![forbid(unsafe_code)]`; safe dependency APIs may encapsulate platform primitives.
- **KISS and locked dependencies.** Add no framework or optional mechanism the contract does not require. Commit `Cargo.lock` and use `--locked`.
- **Atomic writes.** Write to `.tmp` then `os.rename()` for config and state persistence.
- **Offline-first.** Captures always write to local cache. Python sync retains its circuit breaker; native sync uses its single bounded backoff owner and retains data until custody is proven.
- **Crash recovery.** `.incomplete` segment directories get recovered on startup. `.failed` directories are quarantined.
- **Test everything, isolate external state.** Tests must never call real tmux, systemd, launchd, or external HTTP services. Use isolated paths and injected environment, clock, command, storage, and shutdown seams; private-link tests use the shared in-process loopback peer.

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
Blocking sync preparation—including file reads, hashing, scans, deletion, and atomic
writes—runs through Tokio's blocking pool, while known-length multipart bodies stream
in bounded chunks. The current-thread runtime therefore remains available for the
five-second capture cadence during a slow upload. The native plane does not read or
migrate the installed Python product's config.

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

Native private-link sync runs inside the existing native observer process. Shutdown
stops and joins sync and its bridge before releasing the instance lock; it does not
install, start, stop, or otherwise manage a separate sync service or the Python unit.

The launchd plist retains Apple's standard
`http://www.apple.com/DTDs/PropertyList-1.0.dtd` document-type identifier. It is emitted
as plist syntax and is never fetched by the service lifecycle code.

The launchd plist sets `LC_ALL=UTF-8` for the observer process so tmux option values
round-trip byte-exactly.

### Sync Service

The installed Python `SyncService` runs as a background `asyncio` task. It walks cached
days newest-to-oldest, queries the server for existing segments, and uploads missing
ones. Its circuit breaker opens after 3 consecutive failures, and its existing synced
days ledger remains specific to that installed product.

`SyncService` keeps the sync-related health facts in memory. The observer rides an 8-field diagnostics beacon on each `observe.status` event: `name`, `stream_type`, `version`, `uptime`, `last_successful_sync`, `pending_queue_depth`, `recent_error_count`, and `last_error_reason`. The beacon excludes observed-user data. Journal-side health may independently report ingest rejections separate from this observer-side beacon.

The native sync task rescans the filesystem on startup, finalization, and periodic
wakeups, processes at most eight candidates sequentially per pass, and owns one
5/30/120/300-second backoff sequence. Upload success alone never authorizes deletion:
a fresh Journal listing must prove every local file's submitted name, SHA-256 digest,
and held status through the authoritative key or original key. The native plane has no
circuit breaker or synced-days ledger.

### Registration

On first run the installed Python observer self-registers over HTTP directly to the
journal's `/app/observer/register` endpoint, sending a descriptor (`platform`,
`hostname`, `stream_type`, `version`). The journal returns a handle, which is cached in
the config and presented as an `Authorization: Bearer` token on every later upload.
Registration runs once — a cached handle is reused.

Native `setup` reads one private-link pairing link from stdin, pairs, and atomically
persists only the credential. Native `run` performs Journal registration as ordinary
best-effort sync work, persists observer state separately, and binds that state to
`Credential.instance_id`; changing credentials makes the stored observer registration
stale and forces re-registration without blocking capture.

## Config

Installed Python config file: `~/.local/share/solstone-tmux/config/config.json`

```json
{
  "server_url": "http://localhost:5015",
  "key": "<observer-api-key>",
  "stream": "<hostname>.tmux",
  "capture_interval": 5,
  "segment_interval": 300
}
```

The transitional native plane uses its separately resolved
`<config_root>/config.json`. Its capture and segment intervals and stream setting are
independent of the installed product. `cache_retention_days` defaults to `7`: a
negative value disables retention traversal, zero permits deletion of every
custody-proven day older than today, and a positive value preserves days on or after
the local-day cutoff. Today is always retained.

Native private-link state is separate from runtime config:
`<config_root>/credentials.json` holds paired SPL credentials, while
`<config_root>/observer.json` holds the Journal registration and its credential
instance binding. Both are owner-only, symlink-refusing, atomic state files. Do not
include their values in diagnostics or documentation examples.

## Vendored observer contract

`native/solstone-tmux-observer/vendor/observer-client-contract/` contains the
byte-exact Journal observer-client authority bundle. Its authority manifest is the
only source for the bundle's file digests. The adjacent
`contracts/observer-client-import.json` records the authority repository revision,
bundle version, manifest digest, and vendored root. The offline
`observer_contract` Rust test verifies provenance first, then requires exactly the
manifest-listed files and verifies their bytes; missing, extra, renamed, or modified
authority material fails the gate.

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
