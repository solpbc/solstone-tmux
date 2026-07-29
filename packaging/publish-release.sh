#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

set -euo pipefail

umask 077
export LC_ALL=C

die() {
    printf 'native release publisher: %s\n' "$1" >&2
    exit 1
}

if (($# != 3)); then
    echo "usage: publish-release.sh <source-commit> <unsigned-candidate-directory> <minisign-secret-key>" >&2
    exit 2
fi

source_commit="$1"
candidate_directory="$2"
secret_key="$3"

required_tools=(
    cargo find gh git grep install jq minisign mktemp realpath rm sed sha256sum
    sort tar
)
for tool in "${required_tools[@]}"; do
    command -v "$tool" >/dev/null 2>&1 ||
        die "required release tool is unavailable: $tool"
done
[[ "$(minisign -v 2>&1)" == "minisign 0.11" ]] ||
    die "minisign 0.11 is required"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" ||
    die "current directory is not a Git worktree"
repo_root="$(realpath "$repo_root")"
[[ -z "$(git -C "$repo_root" status --porcelain=v1 --untracked-files=all)" ]] ||
    die "source tree must be clean"
[[ "$source_commit" =~ ^[0-9a-f]{40}$ ]] ||
    die "source commit must be lowercase 40-hex"
[[ "$(git -C "$repo_root" rev-parse HEAD)" == "$source_commit" ]] ||
    die "source commit must equal HEAD"

candidate_directory="$(realpath "$candidate_directory")"
secret_key="$(realpath "$secret_key")"
[[ -d "$candidate_directory" && ! -L "$candidate_directory" ]] ||
    die "unsigned candidate must be a real directory"
[[ -f "$secret_key" && ! -L "$secret_key" ]] ||
    die "minisign secret key must be a regular file"
case "$secret_key" in
    "$repo_root" | "$repo_root"/*)
        die "minisign secret key must remain outside the repository"
        ;;
esac
public_key="$repo_root/packaging/keys/solstone-tmux-release.pub"
[[ -f "$public_key" && ! -L "$public_key" ]] ||
    die "release public key must be a regular file"

read_package_version() {
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
    ' "$1"
}

manifest_version="$(read_package_version "$repo_root/native/solstone-tmux/Cargo.toml")"
[[ "$manifest_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "native Cargo.toml has no canonical package version"
lock_version="$(
    awk '
        $0 == "[[package]]" {
            in_package = 1
            name = ""
            version = ""
            next
        }
        in_package && /^name = "/ {
            name = $0
            sub(/^name = "/, "", name)
            sub(/"$/, "", name)
            next
        }
        in_package && /^version = "/ {
            version = $0
            sub(/^version = "/, "", version)
            sub(/"$/, "", version)
            if (name == "solstone-tmux") {
                print version
                exit
            }
        }
    ' "$repo_root/Cargo.lock"
)"
[[ "$lock_version" == "$manifest_version" ]] ||
    die "Cargo.lock does not agree with the native package version"

version="$manifest_version"
tag="v$version"
title="solstone-tmux $version"
[[ "$tag" == "v1.0.0" && "$title" == "solstone-tmux 1.0.0" ]] ||
    die "publisher is restricted to the native 1.0.0 cutover"
notes="$("$repo_root/scripts/extract_changelog.sh" "$version" "$repo_root/CHANGELOG.md")" ||
    die "could not extract exact release notes"
[[ "$notes" == "## [$version]"* ]] ||
    die "release notes do not start with the expected version heading"

mapfile -t configured_targets < <("$repo_root/scripts/rust-targets.sh")
((${#configured_targets[@]} == 3)) ||
    die "expected exactly three configured Rust targets"

unsigned_names=(
    "solstone-tmux-$version-1.aarch64.rpm"
    "solstone-tmux-$version-1.x86_64.rpm"
    "solstone-tmux-$version-aarch64-linux.tar.gz"
    "solstone-tmux-$version-aarch64-macos.pkg"
    "solstone-tmux-$version-aarch64-macos.tar.gz"
    "solstone-tmux-$version-x86_64-linux.tar.gz"
    "solstone-tmux_${version}_amd64.deb"
    "solstone-tmux_${version}_arm64.deb"
)
for target in "${configured_targets[@]}"; do
    unsigned_names+=("solstone-tmux-$version-$target.target.json")
done
mapfile -t unsigned_names < <(printf '%s\n' "${unsigned_names[@]}" | sort)
publishable_names=("${unsigned_names[@]}" "SHA256SUMS" "SHA256SUMS.minisig")
mapfile -t publishable_names < <(printf '%s\n' "${publishable_names[@]}" | sort)

assert_exact_files() {
    local root="$1"
    shift
    local -a expected=("$@")
    local -a actual=()
    local path
    while IFS= read -r -d '' path; do
        [[ -f "$path" && ! -L "$path" ]] ||
            die "candidate entries must be regular files"
        actual+=("${path##*/}")
    done < <(find "$root" -mindepth 1 -maxdepth 1 -print0 | sort -z)
    ((${#actual[@]} == ${#expected[@]})) ||
        die "candidate file set is incomplete or unlisted"
    local index
    for index in "${!expected[@]}"; do
        [[ "${actual[$index]}" == "${expected[$index]}" ]] ||
            die "candidate file set is incomplete or unlisted"
    done
}

assert_exact_files "$candidate_directory" "${unsigned_names[@]}"

stage_root="$(mktemp -d "${TMPDIR:-/tmp}/solstone-tmux-publish.XXXXXX")"
cleanup() {
    rm -rf -- "$stage_root"
}
trap cleanup EXIT
publishable="$stage_root/publishable"
downloads="$stage_root/downloads"
extracted="$stage_root/extracted"
install -d -m 0700 "$publishable" "$downloads" "$extracted"
for name in "${unsigned_names[@]}"; do
    install -m 0644 "$candidate_directory/$name" "$publishable/$name"
done

(
    cd "$repo_root"
    SOLSTONE_TMUX_TEST_UNSIGNED_CANDIDATE="$publishable" \
        cargo test --locked -p solstone-tmux --test release_validator \
        validates_real_unsigned_set_when_requested -- --exact
) || die "unsigned aggregate candidate validation failed"

executable_tar="solstone-tmux-$version-x86_64-linux.tar.gz"
tar -xzf "$publishable/$executable_tar" -C "$extracted" solstone-tmux ||
    die "could not extract the source-bound executable"
executable="$extracted/solstone-tmux"
[[ -f "$executable" && ! -L "$executable" && -x "$executable" ]] ||
    die "candidate executable is not a regular executable"
version_stdout="$stage_root/version.stdout"
version_stderr="$stage_root/version.stderr"
"$executable" --version >"$version_stdout" 2>"$version_stderr" ||
    die "candidate executable --version failed"
[[ ! -s "$version_stderr" ]] ||
    die "candidate executable --version wrote to stderr"
[[ "$(sed -n '$=' "$version_stdout")" == "1" ]] ||
    die "candidate executable --version output is not one line"
[[ "$(sed -n '1p' "$version_stdout")" == "$title (source $source_commit)" ]] ||
    die "candidate executable version, version number, or source commit disagrees"
grep -aFq "$source_commit" "$executable" ||
    die "source commit is absent from candidate executable bytes"

for target in "${configured_targets[@]}"; do
    record="$publishable/solstone-tmux-$version-$target.target.json"
    jq -e \
        --arg version "$version" \
        --arg commit "$source_commit" \
        --arg target "$target" \
        '.product_version == $version and
         .source_commit == $commit and
         .rust_target == $target' \
        "$record" >/dev/null ||
        die "target record version, source commit, or target disagrees"
done

local_tag_present=false
if git -C "$repo_root" show-ref --verify --quiet "refs/tags/$tag"; then
    local_tag_present=true
    [[ "$(git -C "$repo_root" cat-file -t "refs/tags/$tag")" == "tag" ]] ||
        die "existing local release tag is not annotated"
    [[ "$(git -C "$repo_root" rev-parse "$tag^{commit}")" == "$source_commit" ]] ||
        die "existing local release tag does not peel to HEAD"
else
    tag_status="$?"
    [[ "$tag_status" == "1" ]] ||
        die "could not inspect the local release tag"
fi

checksum_file="$publishable/SHA256SUMS"
: >"$checksum_file"
for name in "${unsigned_names[@]}"; do
    digest="$(sha256sum "$publishable/$name" | sed 's/[[:space:]].*$//')"
    [[ "$digest" =~ ^[0-9a-f]{64}$ ]] ||
        die "could not compute a canonical candidate digest"
    printf '%s  %s\n' "$digest" "$name" >>"$checksum_file"
done
minisign -S \
    -s "$secret_key" \
    -m "$checksum_file" \
    -x "$publishable/SHA256SUMS.minisig" \
    -c "solstone-tmux release signature" \
    -t "$title SHA256SUMS" ||
    die "could not sign SHA256SUMS"
minisign -V \
    -q \
    -p "$public_key" \
    -m "$checksum_file" \
    -x "$publishable/SHA256SUMS.minisig" ||
    die "generated SHA256SUMS signature did not verify"
assert_exact_files "$publishable" "${publishable_names[@]}"
(
    cd "$repo_root"
    SOLSTONE_TMUX_TEST_COMPLETE_CANDIDATE="$publishable" \
        cargo test --locked -p solstone-tmux --test release_validator \
        validates_real_complete_set_when_requested -- --exact
) || die "complete aggregate candidate validation failed"

repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner)" ||
    die "could not resolve the GitHub repository"
[[ "$repo" =~ ^[^/]+/[^/]+$ ]] ||
    die "GitHub repository identity is ambiguous"

remote_refs="$(
    gh api "repos/$repo/git/matching-refs/tags/$tag"
)" || die "could not inspect the remote release tag"
remote_ref_count="$(
    jq --arg ref "refs/tags/$tag" '[.[] | select(.ref == $ref)] | length' <<<"$remote_refs"
)"
[[ "$remote_ref_count" == "0" || "$remote_ref_count" == "1" ]] ||
    die "remote release tag state is ambiguous"
remote_tag_present=false
if [[ "$remote_ref_count" == "1" ]]; then
    remote_tag_present=true
    remote_tag_type="$(
        jq -r --arg ref "refs/tags/$tag" '.[] | select(.ref == $ref) | .object.type' \
            <<<"$remote_refs"
    )"
    remote_tag_object="$(
        jq -r --arg ref "refs/tags/$tag" '.[] | select(.ref == $ref) | .object.sha' \
            <<<"$remote_refs"
    )"
    [[ "$remote_tag_type" == "tag" && "$remote_tag_object" =~ ^[0-9a-f]{40}$ ]] ||
        die "remote release tag is not annotated"
    remote_tag="$(
        gh api "repos/$repo/git/tags/$remote_tag_object"
    )" || die "could not peel the remote release tag"
    [[ "$(jq -r '.object.type' <<<"$remote_tag")" == "commit" &&
        "$(jq -r '.object.sha' <<<"$remote_tag")" == "$source_commit" ]] ||
        die "remote release tag does not peel to the exact source commit"
fi

release_pages="$(
    gh api --paginate --slurp "repos/$repo/releases?per_page=100"
)" || die "could not inspect releases"
relevant_releases="$(
    jq \
        --arg tag "$tag" \
        --arg title "$title" \
        '[.[][] | select(.tag_name == $tag or .name == $title)]' \
        <<<"$release_pages"
)"
release_count="$(jq 'length' <<<"$relevant_releases")"
[[ "$release_count" == "0" || "$release_count" == "1" ]] ||
    die "release state is ambiguous"

release_present=false
release_id=""
release_draft=""
if [[ "$release_count" == "1" ]]; then
    release_present=true
    release_id="$(jq -r '.[0].id' <<<"$relevant_releases")"
    [[ "$release_id" =~ ^[1-9][0-9]*$ ]] ||
        die "release identifier is invalid"
    [[ "$(jq -r '.[0].tag_name' <<<"$relevant_releases")" == "$tag" &&
        "$(jq -r '.[0].target_commitish' <<<"$relevant_releases")" == "$source_commit" &&
        "$(jq -r '.[0].name' <<<"$relevant_releases")" == "$title" &&
        "$(jq -r '.[0].body' <<<"$relevant_releases")" == "$notes" ]] ||
        die "release metadata differs from the exact candidate"
    release_draft="$(jq -r '.[0].draft' <<<"$relevant_releases")"
    [[ "$release_draft" == "true" || "$release_draft" == "false" ]] ||
        die "release draft state is invalid"
fi
if $release_present && ! $remote_tag_present; then
    die "release exists without the exact annotated remote tag"
fi

verify_remote_assets() {
    local release_json="$1"
    local require_complete="$2"
    local -a remote_names=()
    local -A seen_names=()
    local name asset_count asset_id download
    mapfile -t remote_names < <(jq -r '.[0].assets[].name' <<<"$release_json" | sort)
    for name in "${remote_names[@]}"; do
        [[ -z "${seen_names[$name]:-}" ]] ||
            die "release contains duplicate asset names"
        seen_names["$name"]=1
        if ! printf '%s\n' "${publishable_names[@]}" | grep -Fxq "$name"; then
            die "release contains an unlisted asset"
        fi
        asset_count="$(
            jq --arg name "$name" '[.[0].assets[] | select(.name == $name)] | length' \
                <<<"$release_json"
        )"
        [[ "$asset_count" == "1" ]] ||
            die "release asset state is ambiguous"
        asset_id="$(
            jq -r --arg name "$name" '.[0].assets[] | select(.name == $name) | .id' \
                <<<"$release_json"
        )"
        [[ "$asset_id" =~ ^[1-9][0-9]*$ ]] ||
            die "release asset identifier is invalid"
        download="$downloads/$name"
        gh api "repos/$repo/releases/assets/$asset_id" \
            -H "Accept: application/octet-stream" >"$download" ||
            die "could not download an existing release asset"
        local_digest="$(sha256sum "$publishable/$name" | sed 's/[[:space:]].*$//')"
        remote_digest="$(sha256sum "$download" | sed 's/[[:space:]].*$//')"
        [[ "$local_digest" == "$remote_digest" ]] ||
            die "release asset bytes differ from the exact candidate"
    done
    if [[ "$require_complete" == "true" ]]; then
        ((${#remote_names[@]} == ${#publishable_names[@]})) ||
            die "published release asset set is incomplete"
        local index
        for index in "${!publishable_names[@]}"; do
            [[ "${remote_names[$index]}" == "${publishable_names[$index]}" ]] ||
                die "published release asset set is incomplete"
        done
    fi
}

if $release_present; then
    verify_remote_assets "$relevant_releases" "$([[ "$release_draft" == "false" ]] && echo true || echo false)"
    if [[ "$release_draft" == "false" ]]; then
        printf '%s\n' "release $tag is already published and exact"
        exit 0
    fi
fi

if ! $remote_tag_present; then
    if ! $local_tag_present; then
        git -C "$repo_root" tag -a "$tag" "$source_commit" -m "$title" ||
            die "could not create the exact annotated local tag"
    fi
    tag_object="$(git -C "$repo_root" rev-parse "refs/tags/$tag")"
    [[ "$(git -C "$repo_root" cat-file -t "$tag_object")" == "tag" ]] ||
        die "local release tag object is not annotated"
    gh api -X POST "repos/$repo/git/refs" \
        -f "ref=refs/tags/$tag" \
        -f "sha=$tag_object" >/dev/null ||
        die "could not push the exact annotated release tag"
fi

if ! $release_present; then
    created_release="$(
        gh api -X POST "repos/$repo/releases" \
            -f "tag_name=$tag" \
            -f "target_commitish=$source_commit" \
            -f "name=$title" \
            -f "body=$notes" \
            -F draft=true \
            -F prerelease=false
    )" || die "could not create the exact draft release"
    release_id="$(jq -r '.id' <<<"$created_release")"
    [[ "$release_id" =~ ^[1-9][0-9]*$ ]] ||
        die "created draft release identifier is invalid"
    relevant_releases="$(jq -n --argjson release "$created_release" '[$release]')"
fi

for name in "${publishable_names[@]}"; do
    existing_count="$(
        jq --arg name "$name" '[.[0].assets[] | select(.name == $name)] | length' \
            <<<"$relevant_releases"
    )"
    if [[ "$existing_count" == "0" ]]; then
        gh release upload "$tag" "$publishable/$name" ||
            die "could not upload release asset"
    elif [[ "$existing_count" != "1" ]]; then
        die "draft release asset state is ambiguous"
    fi
done

release_pages="$(
    gh api --paginate --slurp "repos/$repo/releases?per_page=100"
)" || die "could not re-read the completed draft"
relevant_releases="$(
    jq \
        --arg tag "$tag" \
        --arg title "$title" \
        '[.[][] | select(.tag_name == $tag or .name == $title)]' \
        <<<"$release_pages"
)"
[[ "$(jq 'length' <<<"$relevant_releases")" == "1" ]] ||
    die "completed draft release state is ambiguous"
[[ "$(jq -r '.[0].id' <<<"$relevant_releases")" == "$release_id" &&
    "$(jq -r '.[0].draft' <<<"$relevant_releases")" == "true" ]] ||
    die "completed draft identity or state changed"
verify_remote_assets "$relevant_releases" true

gh api -X PATCH "repos/$repo/releases/$release_id" -F draft=false >/dev/null ||
    die "could not publish the verified draft"
published="$(
    gh api "repos/$repo/releases/$release_id"
)" || die "could not verify the published release"
[[ "$(jq -r '.draft' <<<"$published")" == "false" &&
    "$(jq -r '.tag_name' <<<"$published")" == "$tag" &&
    "$(jq -r '.target_commitish' <<<"$published")" == "$source_commit" &&
    "$(jq -r '.name' <<<"$published")" == "$title" &&
    "$(jq -r '.body' <<<"$published")" == "$notes" ]] ||
    die "published release metadata changed"

printf '%s\n' "published exact native release $tag"
