# Releasing solstone-tmux

Native releases are built from one exact clean commit in three independent
candidate lanes, then validated and published once as a complete aggregate.
No individual lane can publish.

Builds and gates run locally on native release machines. GitHub Actions and
other GitHub workflows are not part of the release rail. The release is
published to the release origin, `https://updates.solstone.app`, which is where
the install documentation points once the first `release` publish lands; GitHub
Releases is an optional mirror carrying the tag, the release notes, and a copy of
the same bytes.

## Prerequisites

- The source commit is clean, full lowercase 40-hex, and equals `HEAD`.
- `Cargo.toml`, `Cargo.lock`, the source-bound executable, target records,
  release tag, title, and `CHANGELOG.md` agree on version and source.
- `cargo-deny` is exactly version `0.20.2`.
- The committed file `packaging/keys/solstone-tmux-release.pub` contains the
  real release public key.
- The minisign private key remains outside the repository.

## 0. Committed release public key

The committed minisign release public key at
`packaging/keys/solstone-tmux-release.pub` has ID `365708FAD9F80092`. All three
candidate lanes build only from a clean `HEAD` that already contains it.

## 1. Linux candidate lanes

Run each lane on an internal native Linux build machine: one x86_64 and one
aarch64. If an internal lane is unavailable, use a disposable same-architecture
cloud instance. Start from the same exact clean commit on both machines and run:

```sh
make release-linux \
  SOURCE_COMMIT=<exact-commit> \
  OUTPUT_DIRECTORY=/absolute/path/to/new-candidate
```

The local release entrypoint derives the lane from the native Rust host, fetches
the complete locked dependency graph, runs `make ci`, builds the source-bound
executable and all three package formats, validates architecture, static linkage,
source binding, package model, target record, and checksums, then proves normal
foreground operation with real tmux in isolated roots. It produces three
packages and one target record in the requested directory and performs no
upload or remote mutation.

On disposable target-matching environments, install and smoke the tar, deb, and
RPM candidates with their real package tools. Structural parsing in Rust does
not replace package-manager installation proof.

## 2. macOS candidate lane

Run `packaging/macos/build-candidate.sh` on a disposable Apple-silicon machine.
Pass the configured macOS target, exact commit, version, tag, Developer ID
Application identity, Developer ID Installer identity, notary profile, and a
new absolute output directory. Set `SOLSTONE_TMUX_SCRATCH_HOST=1` only on that
disposable machine. The notary profile is read from the dedicated signing
keychain at `~/Library/Keychains/sol-signing.keychain-db`; override that
absolute path with `SOLSTONE_TMUX_NOTARY_KEYCHAIN` when the operator machine
uses a different dedicated keychain. Unlock the keychain in the operator
session before starting the lane.

The script refuses dirty or inconsistent source, runs guards and the locked
gate, builds with the 14.0 deployment floor, signs and verifies the executable,
constructs the signed-binary tarball, builds and product-signs the script-free
pkg, notarizes and staples the pkg, verifies every payload and hash, installs
the exact package, proves its LaunchAgent loads and unloads, foreground-smokes
the byte-identical signed executable against isolated tmux and owner state,
cleans up, and emits the macOS target record.

The tarball claim is limited to containing a Developer-ID-signed binary. Only
the pkg is notarized and stapled. This lane finalizes its candidate and stops;
it does not aggregate or publish.

## 3. Collect and validate

Collect both Linux lane candidates and the macOS candidate into one private
directory. Before signing it contains exactly eight packages and three
target records.

For an already signed 13-file candidate, run:

```sh
make validate-release \
  CANDIDATE_DIRECTORY=/absolute/path/to/signed-candidate
```

This invokes the complete-set validator. It checks the eight packages, three
target records, sorted `SHA256SUMS`, and detached minisign signature. The source
commit is read from executable output and bytes before metadata is trusted.
Run aggregate validation and publication on a platform represented by one of
the three candidate lanes: Linux x86_64, Linux aarch64, or macOS aarch64. The
validator executes `--version` only from the lane matching that machine and
uses fixed-byte source-commit checks for the other two lanes.

The variable contract is:

- `CANDIDATE_DIRECTORY`: absolute path to the complete signed candidate.

## 4. Sign and validate without publication

When a release-validation lane needs canonical aggregate-signing proof without
touching a GitHub tag, draft, release, asset, or release-state API, use:

```sh
make sign-validate-release \
  SOURCE_COMMIT=<exact-commit> \
  CANDIDATE_DIRECTORY=/absolute/path/to/unsigned-candidate \
  MINISIGN_SECRET_KEY=/absolute/path/to/out-of-tree-key \
  SIGNED_CANDIDATE_DIRECTORY=/absolute/path/to/new-signed-candidate
```

This validates the unsigned aggregate, writes bytewise-sorted `SHA256SUMS`,
signs and verifies it with the pinned key, validates the exact 13-file signed
aggregate, and moves it to the new destination. It does not invoke `gh`, inspect
GitHub, create a tag, or mutate a release surface. The destination must be a new
absolute directory outside both the source tree and the unsigned candidate.

The signing binary remains pinned to minisign 0.11. Set `MINISIGN_BIN` to an
absolute executable path only when using a disposable lane-local copy of that
pinned binary; never change a shared host toolchain just to run this proof.

## 5. Publish the aggregate

Run publication only after all three lane candidates have been collected. The
release origin comes first and stands alone; the GitHub mirror is optional and
comes after.

### The release origin

Take the signed thirteen-file candidate from section 4 and publish it:

```sh
make publish-origin \
  LANE=release \
  CANDIDATE_DIRECTORY=/absolute/path/to/signed-candidate
```

Objects land at
`https://updates.solstone.app/solstone-tmux/<lane>/<version>/<filename>`, and
`<lane>/latest` (one line, `version=<version>`, the pointer the documentation
tells people to read) is written only after every file in the set is published.
The published key is at `https://updates.solstone.app/solstone-tmux/minisign.pub`
and is the same key this repository pins at
`packaging/keys/solstone-tmux-release.pub`. Nothing else places that object, so
the publisher owns it: it writes the key when the origin has none, and refuses if
the two ever disagree.

Lanes are exactly `release`, `staging`, and `dev`. Versioned objects on `release`
and `staging` are immutable: the publisher refuses to overwrite one, republishing
identical bytes is a no-op, and adding a missing filename under an existing
version is allowed. `dev` may be overwritten. `latest` never moves backwards.
No R2 bucket lock rule covers these prefixes today (`wrangler r2 bucket lock
list solstone-updates` reports none), so the store enforces none of that and
`wrangler r2 object put` overwrites silently. The publisher enforces
it instead, by reading what is already there before it writes.

The origin publish never contacts GitHub and does not require `gh`. A GitHub
outage cannot delay or fail it.

Before publishing, the publisher runs the same verification the documentation
asks a reader to run: the exact file set, the `SHA256SUMS` signature under the
pinned key, and every listed digest. The `release` lane adds a clean checkout at
the candidate's exact source commit, agreement with the shipping package version,
and the repository's own aggregate validation. The `staging` and `dev` proof
lanes deliberately do not require the publishing checkout to sit at the
candidate's commit; that is what lets a retained candidate be republished for
proof without re-cutting it.

### The GitHub mirror

```sh
make publish-release \
  SOURCE_COMMIT=<exact-commit> \
  CANDIDATE_DIRECTORY=/absolute/path/to/unsigned-candidate \
  MINISIGN_SECRET_KEY=/absolute/path/to/out-of-tree-key
```

Run this after the origin publish, never before it. The variable contract is:

- `SOURCE_COMMIT`: full lowercase 40-hex commit equal to clean `HEAD`.
- `CANDIDATE_DIRECTORY`: absolute path to the exact unsigned 11-file aggregate.
- `MINISIGN_SECRET_KEY`: absolute path to the out-of-tree minisign private key.

`packaging/publish-release.sh` validates the unsigned aggregate, generates
bytewise-sorted `SHA256SUMS`, signs it, verifies the detached signature with the
pinned public key, validates the complete signed set, then evaluates remote tag
and release state. It signs the same bytes with the same key and the same
comments as section 4, and minisign signing is deterministic, so the mirrored
`SHA256SUMS.minisig` is byte-identical to the one on the origin. Confirm that
rather than assuming it: the mirrored assets must match the origin objects byte
for byte, and a difference is a hard stop.

Exact pushed tags are reused. An exact draft receives only missing assets.
Existing asset equality is established by downloading and hashing bytes.
An exact published release is an idempotent success. Any differing or ambiguous
tag, release, metadata, or asset state is immutable red: the publisher never
moves, replaces, repairs, or deletes it.

GitHub does not build, validate, approve, or define the release. Skipping the
mirror leaves a complete, correct release. The install instructions move to the
origin with the first `release` publish; until that happens `INSTALL.md` and
`README.md` still name GitHub, and that is correct, because the `release` lane
is empty.
