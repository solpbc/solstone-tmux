# AGENTS.md

Development guidelines for solstone-tmux, the native tmux observer for
solstone.

## Product overview

solstone-tmux experiences tmux sessions along with the owner. Every five
seconds it reads changed active panes, accumulates observations into local
segments, and syncs completed segments to the paired journal. Local observation
continues while pairing or sync is unavailable, and incomplete segments recover
after a restart.

The product is one Rust crate and one executable. Linux and macOS share capture,
serialization, storage, recovery, registration, sync, health, and supervision
logic. Platform-specific code is limited to path and service lifecycle policy.

## Source layout

```text
Cargo.toml                         Rust workspace with one member
Cargo.lock                         Locked dependency graph
rust-toolchain.toml                Toolchain and authoritative target list
deny.toml                          License, source, ban, and target policy
native/solstone-tmux/
    Cargo.toml                     Product crate and sole binary
    contracts/
        observer-client-import.json
                                   Vendored-contract provenance
    vendor/observer-client-contract/
                                   Journal observer-client authority bundle
    src/
        main.rs                    Process startup and command dispatch
        lib.rs                     Shared crate surface
        cli.rs                     Command parser, help, and source-bound version
        clock.rs                   Wall and monotonic clock seam
        command.rs                 Bounded argv command seam
        config.rs                  Native config and hostname-derived defaults
        health.rs                  Closed diagnostics and health snapshots
        indicator.rs               Owned tmux status indicator
        instance_lock.rs           Exclusive data-root lock
        journal.rs                 Journal observer-client protocol
        migration.rs               One-time Linux settings adoption
        model.rs                   Capture domain model
        name.rs                    Injective filename-safe names
        observer.rs                Poll and shutdown lifecycle
        paths.rs, paths/           Linux and macOS path policy
        private_link.rs            Pairing, credentials, bridge, observer state
        recovery.rs                Incomplete-segment recovery
        segment.rs                 Rotation and finalization
        serialize.rs               JSONL serialization
        service.rs, service/       systemd-user and launchd lifecycle
        storage.rs                 Durable append and atomic writes
        sync.rs                    Bounded sync, custody, and retention
        tmux.rs                    Tmux grammar and transactions
    tests/
        data/                      Golden, tmux, launchd, migration, signature data
        support/                   Shared private-link and package test support
        *.rs                       Integration and contract tests
packaging/
    keys/                          Pinned release verification key
    linux/build-candidate.sh       Deterministic tar, deb, and RPM construction
    linux/build-release-lane.sh    Native Linux build, gate, and tmux proof
    macos/build-candidate.sh       Signed and notarized operator candidate flow
    publish-release.sh             Aggregate immutable publisher
scripts/
    check-rust-guards.sh           Repository policy guard
    extract_changelog.sh           Exact release-note extraction
    rust-targets.sh                Target parser for rust-toolchain.toml
    spl-pin.sh                     SPL pin, inheritance, and lock guard
```

## Build and test commands

```bash
make build
make test
make test-only TEST=<filter>
make format
make ci
make clean

make install-service
make uninstall-service
make service-status
make service-logs
```

Every Cargo command that resolves dependencies uses `--locked`. The target list
exists only in `rust-toolchain.toml`; local build scripts consume that authority
instead of defining a second list.

`make ci` runs:

1. Repository guards.
2. Rust formatting check.
3. Clippy for the workspace and all targets with warnings denied.
4. Locked workspace tests.
5. Exact cargo-deny version check.
6. Offline license, source, and ban policy across configured graph targets.
7. Dependency-graph evidence output.
8. Locked host-only workspace check and host evidence output.

The dependency evidence proves graph resolution only. The host evidence proves
type checking only; neither is a linked or native-artifact claim.

## Dependency policy

The workspace pins `spl-core` and `spl-transport` to one exact Git revision, and
the native crate inherits both declarations. Only the declared crates.io
registry and SPL Git source are allowed. The committed lockfile is
authoritative. Packaging parsers are dev-dependencies and do not enter the
shipped executable graph.

`cargo-deny` must be exactly version `0.20.2`. Its offline gate covers licenses,
sources, and bans. Advisories are excluded because that database requires
network access.

## Development principles

- Keep one process, one crate, one executable, and one service per platform.
- Add no compatibility shim or optional mechanism unless the contract requires
  it.
- Preserve local observation when pairing or sync fails.
- Retain segments until fresh Journal custody proves every submitted file.
- Route blocking filesystem and hashing work through Tokio's blocking pool.
- Use owner-only directories and regular-file, symlink-refusing state access.
- Use `storage::atomic_write_bytes` and directory sync for durable state.
- Keep platform adaptation in `paths` and `service`.
- Tests never touch real tmux, systemd, launchd, Apple signing tools, GitHub, or
  external network services.
- Use existing environment, clock, command, storage, shutdown, dispatcher, and
  loopback peer seams.

## File headers

Every Rust source and test file starts with:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
```

Both crate roots keep `#![forbid(unsafe_code)]` on line 4. Application code does
not use the bare `unsafe` token.

## Architecture notes

### Startup and capture

`run` resolves platform roots, acquires the data-root and private-state locks,
adopts eligible Linux settings, loads native config, recovers configured
streams, opens a segment, optionally installs the owned tmux indicator, and
starts capture and sync supervision.

Each poll builds complete per-session tmux transactions, serializes compatible
JSONL envelopes, and appends only changed observations. Rotation uses monotonic
time; paths and envelope timestamps use wall time.

### Paths and state

Linux uses:

- data: `${XDG_DATA_HOME:-$HOME/.local/share}/solstone-tmux`
- config: `${XDG_CONFIG_HOME:-$HOME/.config}/solstone-tmux`

macOS uses:

- data and config: `$HOME/Library/Application Support/solstone-tmux`

The data root owns `captures/`, the process lock, and `sync-health.json`. The
config root owns `config.json`, `credentials.json`, `observer.json`, the
private-state lock, and resolved local service state. Config and private state
are separate.

### Segments and recovery

Segments live below `captures/YYYYMMDD/<stream>/`. Active directories end in
`.incomplete`; unrecoverable directories end in `.failed`. Rust recovery
metadata is a sibling file, never a member of the segment directory. Derived
stream and session components are injective across slash, whitespace,
case-folding, and Unicode-normalization aliases.

### Pairing and registration

`setup` reads one pairing link from stdin and persists only the SPL credential.
`run` performs Journal registration as best-effort sync work. Observer state is
bound to both the credential instance and expected stream name; either mismatch
causes idempotent re-registration.

Before any network action, registration requires the configured stream to equal
the descriptor-derived name. The returned Journal name is checked again before
observer state is persisted.

### Sync and custody

The sync task rescans on startup, segment finalization, and periodic wakeups. It
processes at most eight candidates sequentially per pass and owns one bounded
backoff sequence. Upload success alone never permits deletion: a fresh Journal
listing must prove filename, SHA-256 digest, and held status for every local
file.

### Service lifecycle

Linux installs one marker-owned systemd user unit,
`solstone-tmux.service`. macOS installs one marker-owned launchd job,
`com.solstone.tmux`. Service rendering records the canonical executable that
was invoked, so package-specific install prefixes are not hardcoded.

Install and uninstall refuse unowned or malformed artifacts. In particular,
native service management does not adopt, replace, or remove a previous
Python-written unit at the colliding Linux path.

## Config

Native `config.json` accepts:

```json
{
  "stream": "machine.tmux",
  "capture_interval": 5,
  "segment_interval": 300,
  "cache_retention_days": 7,
  "status_indicator": true
}
```

Unknown fields are rejected. Missing fields use defaults. Capture and segment
intervals must be greater than zero. A missing stream derives from the system
hostname.

On Linux only, when native config is absent, startup reads the single previous
settings file under the data root and imports exactly `stream`,
`capture_interval`, `segment_interval`, `cache_retention_days`, and
`status_indicator`. It never imports credentials or traverses `captures/`.

## Vendored observer contract

`native/solstone-tmux/vendor/observer-client-contract/` contains the byte-exact
Journal observer-client authority bundle. Its manifest is the only source for
bundle digests. The adjacent import record pins authority revision, bundle
version, manifest digest, and vendored root.

The offline `observer_contract` test verifies provenance first, requires exactly
the manifest-listed files, and checks every byte. Do not edit, rename, add, or
remove vendored material without an explicit authority import.

## Brand canon

- Internal code terms such as capture loop, `capture_interval`, and `captures/`
  remain correct in engineering contexts.
- Diagnostics and documentation never expose credentials, observed pane
  content, or tmux session names.

## Releasing

Native releases use two native Linux build machines, one macOS operator machine,
and one aggregate publisher. GitHub workflows are not part of the build or
validation rail. Individual lanes cannot tag, release, sign the aggregate, or
publish.

See [RELEASING.md](RELEASING.md) for candidate construction, platform proof,
aggregate validation, signing, and immutable publication. Release credentials
remain outside the repository.

## License

AGPL-3.0-only. Copyright (c) 2026 sol pbc.
