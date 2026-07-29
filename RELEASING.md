# Releasing solstone-tmux

Native releases are built from one exact clean commit in three independent
candidate lanes, then validated and published once as a complete aggregate.
No individual lane can publish.

## Prerequisites

- The source commit is clean, full lowercase 40-hex, and equals `HEAD`.
- `Cargo.toml`, `Cargo.lock`, the source-bound executable, target records,
  release tag, title, and `CHANGELOG.md` agree on version and source.
- `cargo-deny` is exactly version `0.20.2`.
- The committed file
  `packaging/keys/solstone-tmux-release.pub` is initially a labelled
  placeholder. VPE supplies the real release public key in the release-calling
  session before validation or publication.
- The minisign private key remains outside the repository.

## 1. Linux candidate lanes

Manually dispatch `.github/workflows/native-candidate.yml` with
`source_commit` set to the exact commit. Its ordered matrix uses native Linux
runners for x86_64 and aarch64, runs the repository and locked Rust gates,
builds one source-bound executable per lane, and constructs tar, deb, and RPM
packages from those immutable bytes.

Each lane validates architecture, the GLIBC floor, source binding, package
model, target record, checksums, and smoke installation. The private workflow
artifact contains that lane's three packages and target record.

RPM structural proof does not happen in Rust. It uses the real `rpm` CLI inside
the digest-pinned, same-architecture Fedora container to query payload,
metadata, dependencies, and script absence; install the exact package; compare
installed executable bytes; smoke it; and uninstall it. An operator machine
without RPM tooling can verify digest and complete-set membership only and
cannot claim real RPM-byte inspection. The mandatory Fedora proof is never
downgraded.

For a local Linux construction where all required tools are present:

```sh
make package-linux \
  RUST_TARGET=<configured-linux-target> \
  SOURCE_COMMIT=<exact-commit> \
  OUTPUT_DIRECTORY=/absolute/path/to/new-candidate
```

`PACKAGE_FORMATS` defaults to `tar,deb,rpm`. The packager refuses the complete
request before producing output if any required tool is unavailable.

## 2. macOS candidate lane

Run `packaging/macos/build-candidate.sh` on a disposable Apple-silicon machine.
Pass the configured macOS target, exact commit, version, tag, Developer ID
Application identity, Developer ID Installer identity, notary profile, and a
new absolute output directory. Set `SOLSTONE_TMUX_SCRATCH_HOST=1` only on that
disposable machine.

The script refuses dirty or inconsistent source, runs guards and the locked
gate, builds with the 14.0 deployment floor, signs and verifies the executable,
constructs the signed-binary tarball, builds and product-signs the script-free
pkg, notarizes and staples the pkg, verifies every payload and hash, installs
and smokes the exact package against isolated tmux and launchd state, cleans up,
and emits the macOS target record.

The tarball claim is limited to containing a Developer-ID-signed binary. Only
the pkg is notarized and stapled. This lane finalizes its candidate and stops;
it does not aggregate or publish.

## 3. Collect and validate

Collect both private Linux lane candidates and the macOS candidate into one
private directory. Before signing it contains exactly eight packages and three
target records.

For an already signed 13-file candidate, run:

```sh
make validate-release \
  CANDIDATE_DIRECTORY=/absolute/path/to/signed-candidate
```

This invokes the complete-set validator. It checks the eight packages, three
target records, sorted `SHA256SUMS`, and detached minisign signature. The source
commit is read from executable output and bytes before metadata is trusted.

The variable contract is:

- `CANDIDATE_DIRECTORY`: absolute path to the complete signed candidate.

Validation fails closed while the committed public key is still the labelled
placeholder.

## 4. Sign and publish the aggregate

Run publication only after all three lane candidates have been collected:

```sh
make publish-release \
  SOURCE_COMMIT=<exact-commit> \
  CANDIDATE_DIRECTORY=/absolute/path/to/unsigned-candidate \
  MINISIGN_SECRET_KEY=/absolute/path/to/out-of-tree-key
```

The variable contract is:

- `SOURCE_COMMIT`: full lowercase 40-hex commit equal to clean `HEAD`.
- `CANDIDATE_DIRECTORY`: absolute path to the exact unsigned 11-file aggregate.
- `MINISIGN_SECRET_KEY`: absolute path to the out-of-tree minisign private key.

The sole publisher implementation,
`packaging/publish-release.sh`, validates the unsigned aggregate, generates
bytewise-sorted `SHA256SUMS`, signs it, verifies the detached signature with the
pinned public key, validates the complete signed set, then evaluates remote tag
and release state.

Exact pushed tags are reused. An exact draft receives only missing assets.
Existing asset equality is established by downloading and hashing bytes.
An exact published release is an idempotent success. Any differing or ambiguous
tag, release, metadata, or asset state is immutable red: the publisher never
moves, replaces, repairs, or deletes it.
