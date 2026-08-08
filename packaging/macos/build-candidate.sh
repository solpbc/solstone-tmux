#!/usr/bin/env bash
set -euo pipefail

umask 077

operator_exec() {
    if [[ -n "${SOLSTONE_TMUX_OPERATOR_DISPATCHER:-}" ]]; then
        "$SOLSTONE_TMUX_OPERATOR_DISPATCHER" "$@"
    else
        "$@"
    fi
}

if (($# != 8)); then
    echo "usage: build-candidate.sh <rust-target> <source-commit> <version> <tag> <application-identity> <installer-identity> <notary-profile> <output-directory>" >&2
    exit 2
fi

rust_target="$1"
source_commit="$2"
product_version="$3"
release_tag="$4"
application_identity="$5"
installer_identity="$6"
notary_profile="$7"
output_directory="$8"
script_dir="${BASH_SOURCE[0]%/*}"
repo_root="$(cd "$script_dir/../.." && pwd)"
notary_keychain="${SOLSTONE_TMUX_NOTARY_KEYCHAIN:-$HOME/Library/Keychains/sol-signing.keychain-db}"

if [[ "${SOLSTONE_TMUX_SCRATCH_HOST:-}" != "1" ]]; then
    echo "macOS candidate installation requires an explicitly disposable scratch host" >&2
    exit 1
fi
if [[ ! "$source_commit" =~ ^[0-9a-f]{40}$ ]]; then
    echo "source commit must be lowercase 40-hex" >&2
    exit 1
fi
if [[ ! "$product_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "product version must be semantic version digits" >&2
    exit 1
fi
if [[ "$release_tag" != "v$product_version" ]]; then
    echo "release tag must exactly match the product version" >&2
    exit 1
fi
if [[ -z "$application_identity" || -z "$installer_identity" || -z "$notary_profile" ]]; then
    echo "signing identities and notary profile are required" >&2
    exit 1
fi
if [[ "$notary_keychain" != /* ]]; then
    echo "notary keychain must be an absolute path" >&2
    exit 1
fi
if [[ "$output_directory" != /* ]]; then
    echo "candidate output must be an absolute path" >&2
    exit 1
fi
if [[ -e "$output_directory" || -L "$output_directory" ]]; then
    echo "candidate output already exists: $output_directory" >&2
    exit 1
fi
output_parent="${output_directory%/*}"
if [[ ! -d "$output_parent" ]]; then
    echo "candidate output parent does not exist: $output_parent" >&2
    exit 1
fi
if [[ -n "${SOLSTONE_TMUX_OPERATOR_DISPATCHER:-}" &&
    ( "$SOLSTONE_TMUX_OPERATOR_DISPATCHER" != /* ||
      ! -x "$SOLSTONE_TMUX_OPERATOR_DISPATCHER" ) ]]; then
    echo "operator dispatcher must be an absolute executable path" >&2
    exit 1
fi

required_tools=(
    bash cargo cargo-deny chmod codesign cmp date env find git grep gzip id install
    installer jq kill launchctl lipo mkdir mktemp mv nohup otool pkgbuild
    pkgutil productsign rm rustc rustup script security sed seq shasum sleep spctl stat
    sudo tar test tmux touch xcrun
)
for tool in "${required_tools[@]}"; do
    # $1 belongs to the dispatched shell.
    # shellcheck disable=SC2016
    if ! operator_exec sh -c 'command -v "$1" >/dev/null 2>&1' sh "$tool"; then
        echo "required macOS candidate tool is unavailable: $tool" >&2
        exit 1
    fi
done

configured_targets=()
while IFS= read -r configured_target; do
    configured_targets+=("$configured_target")
done < <(operator_exec "$repo_root/scripts/rust-targets.sh")
target_found=false
for configured_target in "${configured_targets[@]}"; do
    if [[ "$rust_target" == "$configured_target" ]]; then
        target_found=true
        break
    fi
done
if ! $target_found; then
    echo "Rust target is not configured: $rust_target" >&2
    exit 1
fi
case "$rust_target" in
    aarch64-*-darwin) ;;
    *)
        echo "macOS candidate packaging does not support target: $rust_target" >&2
        exit 1
        ;;
esac

if ! dirty_tree="$(
    operator_exec git -C "$repo_root" status --porcelain --untracked-files=all
)"; then
    echo "could not inspect the macOS candidate source tree" >&2
    exit 1
fi
if [[ -n "$dirty_tree" ]]; then
    echo "macOS candidate source tree must be clean" >&2
    exit 1
fi
if [[ "$(operator_exec git -C "$repo_root" rev-parse HEAD)" != "$source_commit" ]]; then
    echo "source commit must equal the checked-out HEAD" >&2
    exit 1
fi
if [[ "$(operator_exec git -C "$repo_root" rev-parse "$release_tag^{commit}")" != "$source_commit" ]]; then
    echo "release tag must resolve to the exact source commit" >&2
    exit 1
fi
metadata="$(operator_exec cargo metadata --locked --no-deps --format-version 1)"
manifest_version="$(
    operator_exec jq -er \
        '.packages[] | select(.name == "solstone-tmux") | .version' \
        <<<"$metadata"
)"
if [[ "$manifest_version" != "$product_version" ]]; then
    echo "product version does not match Cargo.toml" >&2
    exit 1
fi
if [[ "$(operator_exec cargo-deny --version)" != "cargo-deny 0.20.2" ]]; then
    echo "cargo-deny 0.20.2 is required" >&2
    exit 1
fi
installed_targets="$(operator_exec rustup target list --installed)"
if ! operator_exec grep -Fx "$rust_target" <<<"$installed_targets" >/dev/null; then
    echo "configured macOS Rust target is not installed" >&2
    exit 1
fi
application_identities="$(operator_exec security find-identity -v -p codesigning)"
if ! operator_exec grep -F "$application_identity" <<<"$application_identities" >/dev/null; then
    echo "Developer ID Application identity is unavailable" >&2
    exit 1
fi
installer_identities="$(operator_exec security find-identity -v -p basic)"
if ! operator_exec grep -F "$installer_identity" <<<"$installer_identities" >/dev/null; then
    echo "Developer ID Installer identity is unavailable" >&2
    exit 1
fi
operator_exec xcrun notarytool history \
    --keychain-profile "$notary_profile" \
    --keychain "$notary_keychain" \
    >/dev/null

# Both resolved before the scratch root exists, so a failure here cannot leak a
# scratch directory: the cleanup trap is not installed until further down.
# See the DANGER block below for why unsetting TMUX is load-bearing.
unset TMUX TMUX_PANE
user_id="$(operator_exec id -u)"

scratch_root="$(operator_exec mktemp -d "$output_parent/.solstone-tmux-macos.XXXXXX")"
candidate_root="$scratch_root/candidate"
stage_root="$scratch_root/stage"
scratch_home="$scratch_root/home"
scratch_tmux="$scratch_root/tmux"

###############################################################################
# !!!  DANGER  —  READ THIS BEFORE TOUCHING ANY tmux CALL IN THIS SCRIPT  !!!
#
# Every tmux invocation in this script MUST carry an explicit
# -S "$scratch_tmux_socket". Not TMUX_TMPDIR. Not a bare `tmux`. -S.
#
# TMUX_TMPDIR IS NOT ISOLATION. When $TMUX is set in the environment — which it
# is any time this script runs inside a tmux pane, and it always does: the
# release playbook drives it as
#     ssh -tt pro5e.local 'tmux-run hopper:solstone-tmux-NNN ... build-candidate.sh ...'
# and `tmux-run` executes it inside a tmux session — tmux reads its socket path
# out of $TMUX and ignores TMUX_TMPDIR completely. In that context
#
#     TMUX_TMPDIR=/some/scratch tmux kill-server
#
# is a plain `tmux kill-server` against the scratch host's REAL server —
# including the very `tmux-run` session this script is running inside.
#
# The Linux twin of this bug (packaging/linux/build-release-lane.sh, same
# TMUX_TMPDIR-as-isolation mistake) killed the extro box's live tmux server
# twice on 2026-08-08, taking down every pane, the hub daemon and every running
# lane. That script's DANGER block carries the full incident. This file had the
# identical defect at five call sites; do not reintroduce it.
#
# -S (and -L) are the ONLY forms that override $TMUX. Both halves below are
# load-bearing; removing either one re-arms the failure:
#
#   1. -S pins every tmux CLI call in this script to the scratch socket.
#   2. TMUX is unset so the observer binary lands on the scratch server too.
#      The observer deliberately passes no socket flag — asserted by
#      native/solstone-tmux/tests/tmux_adapter.rs — so it resolves
#      $TMUX -> TMUX_TMPDIR -> /tmp. TMUX_TMPDIR stays exported for the
#      solstone-tmux binary invocations for exactly that reason; it is the
#      observer's resolution root, never the tmux CLI's isolation.
###############################################################################
# The path tmux itself derives from TMUX_TMPDIR, pinned explicitly so the CLI
# calls and the observer's own resolution agree on one server. TMUX is unset and
# user_id resolved above, before the scratch root is created.
scratch_tmux_socket="$scratch_tmux/tmux-$user_id/default"

client_pid_file="$scratch_root/tmux-client.pid"
client_pid=""
observer_pid=""
service_installed=false
package_installed=false

cleanup() {
    set +e
    if [[ -n "$observer_pid" ]]; then
        SOLSTONE_TMUX_OPERATOR_CLEANUP=1 \
            operator_exec kill -TERM "$observer_pid" >/dev/null 2>&1
        wait "$observer_pid" >/dev/null 2>&1
    fi
    if $service_installed; then
        SOLSTONE_TMUX_OPERATOR_CLEANUP=1 \
            HOME="$scratch_home" \
            TMUX_TMPDIR="$scratch_tmux" \
            operator_exec /usr/local/bin/solstone-tmux uninstall-service >/dev/null 2>&1
    fi
    if [[ -z "$client_pid" && -f "$client_pid_file" ]]; then
        client_pid="$(<"$client_pid_file")"
    fi
    if [[ -n "$client_pid" ]]; then
        SOLSTONE_TMUX_OPERATOR_CLEANUP=1 operator_exec kill -TERM "$client_pid" >/dev/null 2>&1
    fi
    # -S, always. See the DANGER block above.
    SOLSTONE_TMUX_OPERATOR_CLEANUP=1 \
        operator_exec tmux -S "$scratch_tmux_socket" kill-server >/dev/null 2>&1
    if $package_installed; then
        SOLSTONE_TMUX_OPERATOR_CLEANUP=1 \
            operator_exec sudo rm -f /usr/local/bin/solstone-tmux >/dev/null 2>&1
        SOLSTONE_TMUX_OPERATOR_CLEANUP=1 \
            operator_exec sudo pkgutil --forget com.solstone.tmux >/dev/null 2>&1
    fi
    SOLSTONE_TMUX_OPERATOR_CLEANUP=1 operator_exec rm -rf "$scratch_root"
}
trap cleanup EXIT

operator_exec mkdir -m 0700 \
    "$candidate_root" \
    "$stage_root" \
    "$scratch_home" \
    "$scratch_tmux"
# tmux creates this directory itself under default resolution, but -S binds the
# socket at a literal path and creates no parents. The observer reaches the same
# socket the other way, through TMUX_TMPDIR, so the layout has to match.
operator_exec mkdir -m 0700 "$scratch_tmux/tmux-$user_id"
if operator_exec test -e /usr/local/bin/solstone-tmux; then
    echo "scratch host already has /usr/local/bin/solstone-tmux" >&2
    exit 1
else
    installed_status="$?"
    if [[ "$installed_status" != "1" ]]; then
        echo "could not prove the native executable is absent on the scratch host" >&2
        exit 1
    fi
fi
if operator_exec launchctl print "gui/$user_id/com.solstone.tmux" >/dev/null 2>&1; then
    echo "scratch host already has com.solstone.tmux loaded" >&2
    exit 1
else
    launchd_status="$?"
    if [[ "$launchd_status" != "113" ]]; then
        echo "could not prove com.solstone.tmux is absent on the scratch host" >&2
        exit 1
    fi
fi
# -S, always. See the DANGER block above. Without it this preflight pair would
# create a session on, and then kill, the scratch host's live tmux server —
# including the tmux-run session this script is executing inside.
operator_exec tmux -S "$scratch_tmux_socket" -f /dev/null new-session -d -s preflight
operator_exec tmux -S "$scratch_tmux_socket" kill-server

operator_exec bash "$repo_root/scripts/check-rust-guards.sh"
operator_exec cargo fetch --locked
operator_exec cargo fmt --all --check
operator_exec cargo clippy --locked --workspace --all-targets -- -D warnings
operator_exec cargo test --locked --workspace
operator_exec cargo deny --offline --locked check licenses sources bans
operator_exec cargo check --locked --workspace --all-targets

# Exact derivation: git show -s --format=%ct "$source_commit"
SOURCE_DATE_EPOCH="$(
    operator_exec git -C "$repo_root" show -s --format=%ct "$source_commit"
)"
if [[ ! "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]]; then
    echo "source commit timestamp is invalid" >&2
    exit 1
fi
export SOURCE_DATE_EPOCH
if ! normalized_timestamp="$(
    operator_exec date -u -r "$SOURCE_DATE_EPOCH" +%Y%m%d%H%M.%S
)"; then
    echo "could not normalize the source commit timestamp" >&2
    exit 1
fi
if [[ ! "$normalized_timestamp" =~ ^[0-9]{12}\.[0-9]{2}$ ]]; then
    echo "normalized source commit timestamp is invalid" >&2
    exit 1
fi
operator_exec env \
    SOLSTONE_TMUX_SOURCE_COMMIT="$source_commit" \
    MACOSX_DEPLOYMENT_TARGET=14.0 \
    cargo build --locked --release --target "$rust_target"
source_executable="$repo_root/target/$rust_target/release/solstone-tmux"
if [[ "$(operator_exec lipo -archs "$source_executable")" != "arm64" ]]; then
    echo "candidate executable architecture is not arm64" >&2
    exit 1
fi
load_commands="$(operator_exec otool -l "$source_executable")"
deployment_targets="$(
    operator_exec sed -n 's/^[[:space:]]*minos[[:space:]]*//p' <<<"$load_commands"
)"
if [[ "$deployment_targets" != "14.0" ]]; then
    echo "candidate executable deployment target is not exactly 14.0" >&2
    exit 1
fi
version_output="$(operator_exec "$source_executable" --version)"
if [[ "$version_output" != "solstone-tmux $product_version (source $source_commit)" ]]; then
    echo "candidate executable is not bound to the requested source" >&2
    exit 1
fi
if ! operator_exec grep -aFq "$source_commit" "$source_executable"; then
    echo "source commit is not present in executable bytes" >&2
    exit 1
fi
unsigned_digest_line="$(operator_exec shasum -a 256 "$source_executable")"
unsigned_hash="$(operator_exec sed 's/[[:space:]].*$//' <<<"$unsigned_digest_line")"

signed_executable="$stage_root/solstone-tmux"
operator_exec install -m 0755 "$source_executable" "$signed_executable"
operator_exec codesign \
    --force \
    --sign "$application_identity" \
    --options runtime \
    --timestamp \
    "$signed_executable"
operator_exec codesign --verify --strict --verbose=2 "$signed_executable"
signed_digest_line="$(operator_exec shasum -a 256 "$signed_executable")"
signed_hash="$(operator_exec sed 's/[[:space:]].*$//' <<<"$signed_digest_line")"
if [[ "$unsigned_hash" == "$signed_hash" ]]; then
    echo "codesign did not change the executable bytes" >&2
    exit 1
fi

tar_stage="$stage_root/tar"
operator_exec mkdir -m 0700 "$tar_stage"
operator_exec install -m 0755 "$signed_executable" "$tar_stage/solstone-tmux"
{
    printf '%s\n' "solstone-tmux requires tmux."
    printf '%s\n' "Install with: install -m 0755 solstone-tmux /usr/local/bin/solstone-tmux"
} >"$tar_stage/INSTALL.md"
operator_exec chmod 0644 "$tar_stage/INSTALL.md"
operator_exec env TZ=UTC touch -t "$normalized_timestamp" \
    "$tar_stage/INSTALL.md" \
    "$tar_stage/solstone-tmux"
tar_name="solstone-tmux-$product_version-aarch64-macos.tar.gz"
operator_exec tar \
    --format=ustar \
    --uid=0 \
    --gid=0 \
    --uname=root \
    --gname=wheel \
    --numeric-owner \
    -C "$tar_stage" \
    -cf "$scratch_root/macos.tar" \
    INSTALL.md solstone-tmux
operator_exec gzip -n -9 <"$scratch_root/macos.tar" >"$candidate_root/$tar_name"
operator_exec chmod 0644 "$candidate_root/$tar_name"

pkg_stage="$stage_root/pkg"
operator_exec mkdir -m 0755 \
    "$pkg_stage" \
    "$pkg_stage/usr" \
    "$pkg_stage/usr/local" \
    "$pkg_stage/usr/local/bin"
operator_exec install -m 0755 "$signed_executable" "$pkg_stage/usr/local/bin/solstone-tmux"
operator_exec env TZ=UTC touch -t "$normalized_timestamp" \
    "$pkg_stage/usr/local/bin/solstone-tmux"
component_pkg="$scratch_root/solstone-tmux-component.pkg"
operator_exec pkgbuild \
    --root "$pkg_stage" \
    --ownership recommended \
    --identifier com.solstone.tmux \
    --version "$product_version" \
    --install-location / \
    "$component_pkg"
pkg_name="solstone-tmux-$product_version-aarch64-macos.pkg"
operator_exec productsign \
    --sign "$installer_identity" \
    --timestamp \
    "$component_pkg" \
    "$candidate_root/$pkg_name"
operator_exec xcrun notarytool submit \
    "$candidate_root/$pkg_name" \
    --keychain-profile "$notary_profile" \
    --keychain "$notary_keychain" \
    --wait
operator_exec xcrun stapler staple "$candidate_root/$pkg_name"

operator_exec pkgutil --check-signature "$candidate_root/$pkg_name"
operator_exec spctl --assess --type install --verbose=2 "$candidate_root/$pkg_name"
operator_exec xcrun stapler validate "$candidate_root/$pkg_name"
expanded_pkg="$stage_root/expanded-pkg"
operator_exec pkgutil --expand-full "$candidate_root/$pkg_name" "$expanded_pkg"
script_directories="$(
    operator_exec find "$expanded_pkg" -type d -name Scripts -print -quit
)"
if operator_exec grep -q . <<<"$script_directories"; then
    echo "macOS package unexpectedly contains maintainer scripts" >&2
    exit 1
else
    scripts_status="$?"
    if [[ "$scripts_status" != "1" ]]; then
        echo "could not prove the macOS package has no maintainer scripts" >&2
        exit 1
    fi
fi
payload_list="$(operator_exec pkgutil --payload-files "$candidate_root/$pkg_name")"
normalized_payload_list="$(operator_exec sed 's#^\./##' <<<"$payload_list")"
payload_binary_count="$(
    operator_exec grep -Fxc 'usr/local/bin/solstone-tmux' <<<"$normalized_payload_list"
)"
if [[ "$payload_binary_count" != "1" ]]; then
    echo "macOS package payload does not list the executable exactly once" >&2
    exit 1
fi
if ! payload_regular_output="$(
    operator_exec find "$expanded_pkg" -type f -path '*/Payload/*' -print
)"; then
    echo "could not inspect regular files in the macOS package payload" >&2
    exit 1
fi
payload_regular_files=()
while IFS= read -r payload_regular_file; do
    payload_regular_files+=("$payload_regular_file")
done <<<"$payload_regular_output"
if ((${#payload_regular_files[@]} != 1)) ||
    [[ "${payload_regular_files[0]}" != */Payload/usr/local/bin/solstone-tmux ]]; then
    echo "macOS package regular-file payload is not exact" >&2
    exit 1
fi
expanded_binary="${payload_regular_files[0]}"
if ! payload_link_output="$(
    operator_exec find "$expanded_pkg" -type l -path '*/Payload/*' -print -quit
)"; then
    echo "could not inspect links in the macOS package payload" >&2
    exit 1
fi
if [[ -n "$payload_link_output" ]]; then
    echo "macOS package payload contains a link" >&2
    exit 1
fi
operator_exec cmp "$expanded_binary" "$signed_executable"
if [[ "$(operator_exec stat -f '%Lp' "$expanded_binary")" != "755" ]]; then
    echo "macOS package executable mode is not 0755" >&2
    exit 1
fi
tar_list="$(operator_exec tar -tzf "$candidate_root/$tar_name")"
if [[ "$tar_list" != $'INSTALL.md\nsolstone-tmux' ]]; then
    echo "macOS tar payload list is not exact" >&2
    exit 1
fi
tar_extract="$stage_root/tar-extract"
operator_exec mkdir -m 0700 "$tar_extract"
operator_exec tar -xzf "$candidate_root/$tar_name" -C "$tar_extract"
operator_exec cmp "$tar_extract/solstone-tmux" "$signed_executable"
operator_exec codesign --verify --strict --verbose=2 "$tar_extract/solstone-tmux"
if ! operator_exec grep -aFq "$source_commit" "$tar_extract/solstone-tmux"; then
    echo "source commit is absent from the packaged executable" >&2
    exit 1
fi

package_installed=true
operator_exec sudo installer -pkg "$candidate_root/$pkg_name" -target /
operator_exec cmp /usr/local/bin/solstone-tmux "$signed_executable"
installed_version="$(operator_exec /usr/local/bin/solstone-tmux --version)"
operator_exec grep -Fx "solstone-tmux $product_version (source $source_commit)" \
    <<<"$installed_version"

export TERM=xterm-256color
# -S, always. See the DANGER block above.
operator_exec tmux -S "$scratch_tmux_socket" -f /dev/null new-session -d -s candidate \
    "while :; do printf 'durable candidate observation\\n'; sleep 1; done"
# $1, $2, $3, and $! belong to the dispatched shell.
# shellcheck disable=SC2016
operator_exec sh -c \
    'nohup script -q /dev/null tmux -S "$3" attach-session -t candidate >"$1" 2>&1 </dev/null & echo $! >"$2"' \
    sh "$scratch_root/tmux-client.log" "$client_pid_file" "$scratch_tmux_socket"
client_pid="$(<"$client_pid_file")"
if [[ ! "$client_pid" =~ ^[1-9][0-9]*$ ]]; then
    echo "scratch tmux client did not report a valid pid" >&2
    exit 1
fi
service_installed=true
HOME="$scratch_home" \
TMUX_TMPDIR="$scratch_tmux" \
    operator_exec /usr/local/bin/solstone-tmux install-service
operator_exec launchctl print "gui/$user_id/com.solstone.tmux" >"$scratch_root/launchd-print.txt"
operator_exec grep -Eq '^[[:space:]]*pid = [1-9][0-9]*$' "$scratch_root/launchd-print.txt"
HOME="$scratch_home" \
TMUX_TMPDIR="$scratch_tmux" \
    "$signed_executable" run \
    >"$scratch_root/observer.stdout" 2>"$scratch_root/observer.stderr" &
observer_pid="$!"
# Loop variables and $1 belong to the dispatched shell.
# shellcheck disable=SC2016
operator_exec sh -c \
    'for ignored in $(seq 1 45); do
        if find "$1/Library/Application Support/solstone-tmux/captures" \
            -type f -name "*.jsonl" -size +0c -print -quit 2>/dev/null |
            grep -q .
        then
            exit 0
        fi
        sleep 1
     done
     exit 1' \
    sh "$scratch_home"
operator_exec kill -TERM "$observer_pid"
wait "$observer_pid"
observer_pid=""
incomplete_output="$(
    operator_exec find "$scratch_home/Library/Application Support/solstone-tmux/captures" \
        \( -name '*.incomplete' -o -name '*.incomplete.meta' \) -print -quit
)"
if [[ -n "$incomplete_output" ]]; then
    echo "foreground macOS observer did not shut down cleanly" >&2
    exit 1
fi
HOME="$scratch_home" \
TMUX_TMPDIR="$scratch_tmux" \
    operator_exec /usr/local/bin/solstone-tmux uninstall-service
if operator_exec launchctl print "gui/$user_id/com.solstone.tmux" >/dev/null 2>&1; then
    echo "owned launchd service remains loaded after uninstall" >&2
    exit 1
else
    launchd_status="$?"
    if [[ "$launchd_status" != "113" ]]; then
        echo "could not prove owned launchd service was removed" >&2
        exit 1
    fi
fi
service_installed=false
operator_exec kill -TERM "$client_pid"
client_pid=""
# -S, always. See the DANGER block above.
operator_exec tmux -S "$scratch_tmux_socket" kill-server
operator_exec sudo rm -f /usr/local/bin/solstone-tmux
operator_exec sudo pkgutil --forget com.solstone.tmux
package_installed=false

rustc_vv_file="$scratch_root/rustc-vv.txt"
operator_exec rustc -vV >"$rustc_vv_file"
record_name="solstone-tmux-$product_version-$rust_target.target.json"
artifacts_json="$scratch_root/artifacts.json"
: >"$artifacts_json"
for artifact_name in "$pkg_name" "$tar_name"; do
    artifact_digest_line="$(operator_exec shasum -a 256 "$candidate_root/$artifact_name")"
    artifact_hash="$(operator_exec sed 's/[[:space:]].*$//' <<<"$artifact_digest_line")"
    # The variables in this filter are expanded by jq.
    # shellcheck disable=SC2016
    operator_exec jq -cn \
        --arg name "$artifact_name" \
        --arg sha256 "$artifact_hash" \
        '{name: $name, sha256: $sha256}' >>"$artifacts_json"
done
operator_exec jq -s 'sort_by(.name)' "$artifacts_json" >"$scratch_root/artifacts-array.json"
# The variables in this filter are expanded by jq.
# shellcheck disable=SC2016
operator_exec jq -n \
    --arg product_version "$product_version" \
    --arg source_commit "$source_commit" \
    --arg rust_target "$rust_target" \
    --rawfile rustc_vv "$rustc_vv_file" \
    --arg executable_name solstone-tmux \
    --arg executable_sha256 "$signed_hash" \
    --slurpfile artifacts "$scratch_root/artifacts-array.json" \
    '{
        schema_version: 1,
        product_version: $product_version,
        source_commit: $source_commit,
        rust_target: $rust_target,
        rustc_vv: $rustc_vv,
        executable: {
            name: $executable_name,
            sha256: $executable_sha256
        },
        artifacts: $artifacts[0]
    }' >"$candidate_root/$record_name"
operator_exec chmod 0644 "$candidate_root/$record_name"

operator_exec mv "$candidate_root" "$output_directory"
trap - EXIT
SOLSTONE_TMUX_OPERATOR_CLEANUP=1 operator_exec rm -rf "$scratch_root" || true
printf '%s\n' "$output_directory"
