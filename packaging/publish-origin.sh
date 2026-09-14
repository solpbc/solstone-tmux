#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

# Publishes an exact signed release candidate to the release origin,
# https://updates.solstone.app/solstone-tmux/{lane}/{version}/{filename},
# writing the {lane}/latest pointer only after every artifact is published.
#
# The origin is where owners fetch bytes. This script never contacts GitHub and
# never requires gh: a GitHub outage cannot stop or delay an origin publish.

set -euo pipefail

umask 077
export LC_ALL=C

PRODUCT="solstone-tmux"
BUCKET="${SOLSTONE_ORIGIN_BUCKET:-solstone-updates}"
ORIGIN_URL="https://updates.solstone.app"
SHA256SUMS_NAME="SHA256SUMS"
SIGNATURE_NAME="SHA256SUMS.minisig"

die() {
    printf 'release origin publisher: %s\n' "$1" >&2
    exit 1
}

usage() {
    echo "usage: publish-origin.sh --lane <release|staging|dev> --candidate-dir <signed-candidate-directory> [--dry-run]" >&2
    exit 2
}

lane=""
candidate_directory=""
dry_run=false
while (($# > 0)); do
    case "$1" in
        --lane)
            (($# >= 2)) || usage
            lane="$2"
            shift 2
            ;;
        --candidate-dir)
            (($# >= 2)) || usage
            candidate_directory="$2"
            shift 2
            ;;
        --dry-run)
            dry_run=true
            shift
            ;;
        *)
            usage
            ;;
    esac
done
[[ -n "$lane" && -n "$candidate_directory" ]] || usage

# Lane vocabulary is the journal's: exactly release, staging, dev.
case "$lane" in
    release | staging | dev) ;;
    *) die "lane-invalid: $lane" ;;
esac

required_tools=(awk find git jq mkdir mktemp realpath rm sha256sum sort minisign)
$dry_run || required_tools+=(wrangler)
for tool in "${required_tools[@]}"; do
    command -v "$tool" >/dev/null 2>&1 ||
        die "required release tool is unavailable: $tool"
done

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" ||
    die "current directory is not a Git worktree"
repo_root="$(realpath "$repo_root")"
public_key="$repo_root/packaging/keys/$PRODUCT-release.pub"
[[ -f "$public_key" && ! -L "$public_key" ]] ||
    die "release public key must be a regular file"

[[ -d "$candidate_directory" && ! -L "$candidate_directory" ]] ||
    die "signed candidate must be a real directory"
candidate_directory="$(realpath "$candidate_directory")" ||
    die "could not resolve the signed candidate directory"

mapfile -t configured_targets < <("$repo_root/scripts/rust-targets.sh")
((${#configured_targets[@]} == 3)) ||
    die "candidate-set-invalid: expected exactly three configured Rust targets"

# The version is the candidate's own, read from its target records; the release
# lane additionally requires it to equal the shipping package version.
mapfile -d '' -t records < <(
    find "$candidate_directory" -mindepth 1 -maxdepth 1 -type f \
        -name "$PRODUCT-*.target.json" -print0
)
((${#records[@]} == 3)) ||
    die "candidate-set-invalid: signed candidate must contain exactly three target records"

version=""
source_commit=""
for record in "${records[@]}"; do
    record_version="$(jq -er '.product_version' "$record")" ||
        die "candidate-set-invalid: target record product version is unavailable"
    record_commit="$(jq -er '.source_commit' "$record")" ||
        die "candidate-set-invalid: target record source commit is unavailable"
    if [[ -z "$version" ]]; then
        version="$record_version"
        source_commit="$record_commit"
    fi
    [[ "$record_version" == "$version" ]] ||
        die "candidate-set-invalid: target records disagree on the product version"
    [[ "$record_commit" == "$source_commit" ]] ||
        die "candidate-set-invalid: target records disagree on the source commit"
done
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "candidate-set-invalid: release version must be strict SemVer"
[[ "$source_commit" =~ ^[0-9a-f]{40}$ ]] ||
    die "candidate-set-invalid: source commit must be lowercase 40-hex"

record_names=()
for target in "${configured_targets[@]}"; do
    record_names+=("$PRODUCT-$version-$target.target.json")
done

artifact_names=()
for record_index in "${!record_names[@]}"; do
    record_name="${record_names[$record_index]}"
    record="$candidate_directory/$record_name"
    [[ -f "$record" && ! -L "$record" ]] ||
        die "candidate-set-invalid: candidate target records are incomplete"
    jq -e --arg target "${configured_targets[$record_index]}" \
        '.rust_target == $target' \
        "$record" >/dev/null ||
        die "candidate-set-invalid: target record does not describe its own target"
    record_artifact_text="$(
        jq -er '
            if (.artifacts | type) != "array" or (.artifacts | length) == 0 then
                error("target record has no artifacts")
            else
                .artifacts[].name
            end
        ' "$record"
    )" || die "candidate-set-invalid: candidate target record artifact set is invalid"
    mapfile -t record_artifacts <<<"$record_artifact_text"
    for name in "${record_artifacts[@]}"; do
        [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] ||
            die "candidate-set-invalid: candidate target record contains an invalid artifact name"
        artifact_names+=("$name")
    done
done
mapfile -t checksummed_names < <(
    printf '%s\n' "${record_names[@]}" "${artifact_names[@]}" | sort
)
if [[ -n "$(printf '%s\n' "${checksummed_names[@]}" | uniq -d)" ]]; then
    die "candidate-set-invalid: candidate target records contain duplicate artifacts"
fi
mapfile -t expected_names < <(
    printf '%s\n' "${checksummed_names[@]}" "$SHA256SUMS_NAME" "$SIGNATURE_NAME" | sort
)

actual_names=()
while IFS= read -r -d '' path; do
    [[ -f "$path" && ! -L "$path" ]] ||
        die "candidate-set-invalid: candidate entries must be regular files"
    actual_names+=("${path##*/}")
done < <(find "$candidate_directory" -mindepth 1 -maxdepth 1 -print0 | sort -z)
((${#actual_names[@]} == ${#expected_names[@]})) ||
    die "candidate-set-invalid: candidate file set is incomplete or unlisted"
for index in "${!expected_names[@]}"; do
    [[ "${actual_names[$index]}" == "${expected_names[$index]}" ]] ||
        die "candidate-set-invalid: candidate file set is incomplete or unlisted"
done

# The always-on verification is the owner's verification: the exact set, the
# signature under the key this repository pins, and every declared digest.
minisign -V -q -p "$public_key" \
    -m "$candidate_directory/$SHA256SUMS_NAME" \
    -x "$candidate_directory/$SIGNATURE_NAME" ||
    die "signature-invalid: SHA256SUMS signature did not verify"

mapfile -t listed_names < <(awk '{print $2}' "$candidate_directory/$SHA256SUMS_NAME" | sort)
((${#listed_names[@]} == ${#checksummed_names[@]})) ||
    die "candidate-set-invalid: SHA256SUMS file set is incomplete or unlisted"
for index in "${!checksummed_names[@]}"; do
    [[ "${listed_names[$index]}" == "${checksummed_names[$index]}" ]] ||
        die "candidate-set-invalid: SHA256SUMS file set is incomplete or unlisted"
done

(
    cd "$candidate_directory"
    sha256sum -c "$SHA256SUMS_NAME"
) >/dev/null || die "digest-mismatch: candidate checksums do not validate"

# The release lane additionally carries the full source binding and the
# repository's own release-model validator. The proof lanes deliberately do
# not, which is what lets a retained candidate be re-proved from main without
# re-cutting it.
if [[ "$lane" == "release" ]]; then
    [[ -z "$(git -C "$repo_root" status --porcelain=v1 --untracked-files=all)" ]] ||
        die "source-unbound: source tree must be clean to publish to the release lane"
    [[ "$(git -C "$repo_root" rev-parse HEAD)" == "$source_commit" ]] ||
        die "source-unbound: HEAD must equal the candidate source commit"
    package_version="$(
        awk '
            $0 == "[package]" { in_package = 1; next }
            in_package && /^\[/ { exit }
            in_package && /^version = "/ {
                value = $0
                sub(/^version = "/, "", value)
                sub(/"$/, "", value)
                print value
                exit
            }
        ' "$repo_root/native/$PRODUCT/Cargo.toml"
    )"
    [[ "$package_version" == "$version" ]] ||
        die "source-unbound: shipping package version does not agree with the candidate"
    (
        cd "$repo_root"
        SOLSTONE_TMUX_TEST_COMPLETE_CANDIDATE="$candidate_directory" \
            cargo test --locked -p "$PRODUCT" --test release_validator \
            validates_real_complete_set_when_requested -- --exact
    ) || die "candidate-set-invalid: release-model validation failed"
fi

# Dot-separated numeric segments; non-numeric segments compare as strings.
# Same ordering the journal's publisher uses, so latest advances identically.
version_is_not_older() {
    local left="$1" right="$2"
    local -a left_parts right_parts
    IFS='.' read -r -a left_parts <<<"$left"
    IFS='.' read -r -a right_parts <<<"$right"
    local count=${#left_parts[@]}
    ((${#right_parts[@]} > count)) && count=${#right_parts[@]}
    local index l r
    for ((index = 0; index < count; index++)); do
        l="${left_parts[index]:-}"
        r="${right_parts[index]:-}"
        [[ "$l" == "$r" ]] && continue
        [[ -z "$l" ]] && return 1
        [[ -z "$r" ]] && return 0
        if [[ "$l" =~ ^[0-9]+$ && "$r" =~ ^[0-9]+$ ]]; then
            ((10#$l > 10#$r)) && return 0
            return 1
        fi
        [[ "$l" > "$r" ]] && return 0
        return 1
    done
    return 0
}

content_type_for() {
    case "$1" in
        *.tar.gz) echo "application/gzip" ;;
        *.deb) echo "application/vnd.debian.binary-package" ;;
        *.rpm) echo "application/x-rpm" ;;
        *.json) echo "application/json" ;;
        *.minisig | "$SHA256SUMS_NAME") echo "text/plain; charset=utf-8" ;;
        *) echo "application/octet-stream" ;;
    esac
}

stage_root="$(mktemp -d "${TMPDIR:-/tmp}/$PRODUCT-publish-origin.XXXXXX")"
cleanup() {
    rm -rf -- "$stage_root"
}
trap cleanup EXIT

# Returns 0 when the object is present (bytes land in $2), 1 when it is
# genuinely absent, and fails closed on every other outcome. An unreachable
# origin must never read as an empty one.
remote_get() {
    local key="$1" destination="$2" log status
    log="$stage_root/wrangler.log"
    set +e
    wrangler r2 object get "$BUCKET/$key" --remote --file "$destination" >"$log" 2>&1
    status=$?
    set -e
    if ((status == 0)); then
        return 0
    fi
    if grep -qF 'The specified key does not exist' "$log"; then
        rm -f "$destination"
        return 1
    fi
    cat "$log" >&2
    die "origin-unreachable: could not read $key"
}

remote_put() {
    local key="$1" file="$2" content_type="$3" cache_control="$4"
    wrangler r2 object put "$BUCKET/$key" \
        --file "$file" \
        --content-type "$content_type" \
        --cache-control "$cache_control" \
        --remote >/dev/null ||
        die "origin-unreachable: could not write $key"
}

checkpoint() {
    [[ "${SOLSTONE_ORIGIN_FAIL_AFTER:-}" == "$1" ]] || return 0
    die "injected-failure $1"
}

# The published key an owner fetches must be the key this repository pins, or
# the documented verify-first step authenticates against the wrong anchor.
# Nothing else places that object, so the publisher owns it: it restores the key
# when absent and refuses when a different one is already there.
if ! $dry_run; then
    published_key="$stage_root/published-minisign.pub"
    if remote_get "$PRODUCT/minisign.pub" "$published_key"; then
        cmp -s "$published_key" "$public_key" ||
            die "key-mismatch: the published minisign key differs from packaging/keys/$PRODUCT-release.pub"
    else
        remote_put "$PRODUCT/minisign.pub" "$public_key" \
            "text/plain; charset=utf-8" "no-cache"
        printf '  put      %s/minisign.pub (trust anchor was absent)\n' "$PRODUCT"
    fi
fi

# CHANGELOG.md mirror — lane-independent (there is one changelog, not one per
# lane) and gated to the release lane only, matching the stricter clean-tree/
# HEAD-bound checks above. This lets solstone.app's release-notes pages read
# prose release notes straight from the origin instead of depending on GitHub
# Releases for them; the origin never carried prose before this.
if [[ "$lane" == "release" ]]; then
    changelog_file="$repo_root/CHANGELOG.md"
    [[ -f "$changelog_file" && ! -L "$changelog_file" ]] ||
        die "changelog-missing: $changelog_file must be a regular file"
    changelog_key="$PRODUCT/CHANGELOG.md"
    if $dry_run; then
        printf '  would publish %s/%s\n' "$ORIGIN_URL" "$changelog_key"
    else
        changelog_remote="$stage_root/remote-changelog"
        if remote_get "$changelog_key" "$changelog_remote" && cmp -s "$changelog_remote" "$changelog_file"; then
            printf '  present  %s\n' "$changelog_key"
        else
            remote_put "$changelog_key" "$changelog_file" "text/plain; charset=utf-8" "no-cache"
            printf '  put      %s\n' "$changelog_key"
        fi
        rm -f "$changelog_remote"
    fi
    checkpoint "changelog"
fi

if [[ "$lane" == "dev" ]]; then
    object_cache_control="no-cache"
else
    object_cache_control="public, max-age=31536000, immutable"
fi

printf 'publishing %s %s to the %s lane of %s\n' \
    "$PRODUCT" "$version" "$lane" "$ORIGIN_URL"

published=()
for name in "${expected_names[@]}"; do
    key="$PRODUCT/$lane/$version/$name"
    local_file="$candidate_directory/$name"
    if $dry_run; then
        printf '  would publish %s/%s\n' "$ORIGIN_URL" "$key"
        continue
    fi
    remote_file="$stage_root/remote-object"
    if remote_get "$key" "$remote_file"; then
        if cmp -s "$remote_file" "$local_file"; then
            printf '  present  %s\n' "$key"
            rm -f "$remote_file"
            checkpoint "object:$name"
            continue
        fi
        # release and staging versioned objects are immutable. No R2 bucket
        # lock rule covers these prefixes, so the store will not refuse for us
        # and wrangler overwrites silently. The refusal is the publisher's.
        [[ "$lane" == "dev" ]] ||
            die "object-immutable: $key already exists with different bytes"
        rm -f "$remote_file"
    fi
    remote_put "$key" "$local_file" "$(content_type_for "$name")" "$object_cache_control"
    printf '  put      %s\n' "$key"
    published+=("$key")
    checkpoint "object:$name"
done
checkpoint "objects"

latest_key="$PRODUCT/$lane/latest"
if $dry_run; then
    printf '  would advance %s/%s to version=%s\n' "$ORIGIN_URL" "$latest_key" "$version"
    exit 0
fi

latest_file="$stage_root/remote-latest"
advance=true
if remote_get "$latest_key" "$latest_file"; then
    existing_body="$(cat "$latest_file")"
    [[ "$existing_body" =~ ^version=[^[:space:]/]+$ ]] ||
        die "latest-invalid: $latest_key is not a single version= line"
    existing_version="${existing_body#version=}"
    if ! version_is_not_older "$version" "$existing_version"; then
        advance=false
    fi
fi

if $advance; then
    printf 'version=%s\n' "$version" >"$stage_root/latest"
    remote_put "$latest_key" "$stage_root/latest" "text/plain; charset=utf-8" "no-cache"
    printf '  put      %s (version=%s)\n' "$latest_key" "$version"
else
    printf '  held     %s (already at version=%s)\n' "$latest_key" "$existing_version"
fi

printf 'published %s %s to %s/%s/%s/%s/ (%d new objects)\n' \
    "$PRODUCT" "$version" "$ORIGIN_URL" "$PRODUCT" "$lane" "$version" "${#published[@]}"
