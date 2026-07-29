#!/usr/bin/env bash
set -euo pipefail

umask 077

if (($# != 5)); then
    echo "usage: build-candidate.sh <rust-target> <source-commit> <executable> <output-directory> <tar,deb,rpm>" >&2
    exit 2
fi

rust_target="$1"
source_commit="$2"
source_executable="$3"
output_directory="$4"
format_list="$5"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

mapfile -t configured_targets < <("$repo_root/scripts/rust-targets.sh")
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
    x86_64-*-linux-gnu)
        archive_arch="x86_64"
        deb_arch="amd64"
        rpm_arch="x86_64"
        elf_machine_hex="3e00"
        ;;
    aarch64-*-linux-gnu)
        archive_arch="aarch64"
        deb_arch="arm64"
        rpm_arch="aarch64"
        elf_machine_hex="b700"
        ;;
    *)
        echo "Linux candidate packaging does not support target: $rust_target" >&2
        exit 1
        ;;
esac

declare -A requested=()
IFS=',' read -r -a formats <<< "$format_list"
if ((${#formats[@]} == 0)); then
    echo "at least one package format is required" >&2
    exit 1
fi
for format in "${formats[@]}"; do
    case "$format" in
        tar | deb | rpm) ;;
        *)
            echo "unsupported package format: $format" >&2
            exit 1
            ;;
    esac
    if [[ -n "${requested[$format]:-}" ]]; then
        echo "duplicate package format: $format" >&2
        exit 1
    fi
    requested["$format"]=1
done

require_tool() {
    local tool="$1"
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "required packaging tool is unavailable: $tool" >&2
        exit 1
    fi
}

require_version() {
    local tool="$1"
    local actual="$2"
    shift 2
    local accepted
    for accepted in "$@"; do
        if [[ "$actual" == "$accepted" ]]; then
            return
        fi
    done
    echo "required packaging tool has an unsupported version: $tool: $actual" >&2
    exit 1
}

for tool in \
    git rustc sha256sum jq install touch tar gzip sed grep od tr chmod mkdir \
    mktemp rm mv dirname
do
    require_tool "$tool"
done
if [[ -n "${requested[deb]:-}" ]]; then
    require_tool dpkg-deb
fi
if [[ -n "${requested[rpm]:-}" ]]; then
    require_tool rpmbuild
fi

require_version git "$(git --version)" \
    "git version 2.34.1" \
    "git version 2.43.0" \
    "git version 2.54.0"
require_version rustc "$(rustc --version)" \
    "rustc 1.97.1 (8bab26f4f 2026-07-14)"
require_version sha256sum "$(sha256sum --version | sed -n '1p')" \
    "sha256sum (GNU coreutils) 8.32" \
    "sha256sum (GNU coreutils) 9.4" \
    "sha256sum (GNU coreutils) 9.6"
require_version jq "$(jq --version)" \
    "jq-1.6" \
    "jq-1.7" \
    "jq-1.7.1"
require_version install "$(install --version | sed -n '1p')" \
    "install (GNU coreutils) 8.32" \
    "install (GNU coreutils) 9.4" \
    "install (GNU coreutils) 9.6"
require_version touch "$(touch --version | sed -n '1p')" \
    "touch (GNU coreutils) 8.32" \
    "touch (GNU coreutils) 9.4" \
    "touch (GNU coreutils) 9.6"
require_version tar "$(tar --version | sed -n '1p')" \
    "tar (GNU tar) 1.34" \
    "tar (GNU tar) 1.35"
require_version gzip "$(gzip --version | sed -n '1p')" \
    "gzip 1.10" \
    "gzip 1.12" \
    "gzip 1.13"
require_version sed "$(sed --version | sed -n '1p')" \
    "sed (GNU sed) 4.8" \
    "sed (GNU sed) 4.9"
require_version grep "$(grep --version | sed -n '1p')" \
    "grep (GNU grep) 3.7" \
    "grep (GNU grep) 3.11"
if [[ -n "${requested[deb]:-}" ]]; then
    dpkg_deb_version="$(dpkg-deb --version | sed -n '1p')"
    case "$dpkg_deb_version" in
        "Debian 'dpkg-deb' package archive backend version 1.21.1 "* | \
            "Debian 'dpkg-deb' package archive backend version 1.21.22 "* | \
            "Debian 'dpkg-deb' package archive backend version 1.22.6 "*) ;;
        *)
            echo "required packaging tool has an unsupported version: dpkg-deb: $dpkg_deb_version" >&2
            exit 1
            ;;
    esac
fi
if [[ -n "${requested[rpm]:-}" ]]; then
    require_version rpmbuild "$(rpmbuild --version)" \
        "RPM version 4.17.0" \
        "RPM version 4.18.2" \
        "RPM version 4.19.1.1" \
        "RPM version 4.20.1"
fi

if [[ ! "$source_commit" =~ ^[0-9a-f]{40}$ ]]; then
    echo "source commit must be lowercase 40-hex" >&2
    exit 1
fi
resolved_commit="$(git -C "$repo_root" rev-parse --verify "$source_commit^{commit}")"
if [[ "$resolved_commit" != "$source_commit" ]]; then
    echo "source commit does not resolve exactly" >&2
    exit 1
fi
if [[ "$(git -C "$repo_root" rev-parse HEAD)" != "$source_commit" ]]; then
    echo "source commit must equal the checked-out HEAD" >&2
    exit 1
fi
if [[ ! -f "$source_executable" || -L "$source_executable" || ! -x "$source_executable" ]]; then
    echo "source executable must be an executable regular file" >&2
    exit 1
fi
if [[ -e "$output_directory" || -L "$output_directory" ]]; then
    echo "candidate output already exists: $output_directory" >&2
    exit 1
fi
output_parent="$(dirname "$output_directory")"
if [[ ! -d "$output_parent" ]]; then
    echo "candidate output parent does not exist: $output_parent" >&2
    exit 1
fi

version_stdout_file="$(mktemp)"
version_stderr_file="$(mktemp)"
cleanup_early() {
    rm -f -- "$version_stdout_file" "$version_stderr_file"
}
trap cleanup_early EXIT
if ! "$source_executable" --version >"$version_stdout_file" 2>"$version_stderr_file"; then
    echo "source executable --version failed" >&2
    exit 1
fi
if [[ -s "$version_stderr_file" ]]; then
    echo "source executable --version wrote to stderr" >&2
    exit 1
fi
version_line="$(sed -n '1p' "$version_stdout_file")"
if [[ "$(sed -n '$=' "$version_stdout_file")" != "1" ]]; then
    echo "source executable --version is not source-bound" >&2
    exit 1
fi
if [[ "$version_line" =~ ^solstone-tmux\ ([0-9]+\.[0-9]+\.[0-9]+)\ \(source\ ([0-9a-f]{40})\)$ ]]; then
    product_version="${BASH_REMATCH[1]}"
    embedded_commit="${BASH_REMATCH[2]}"
else
    echo "source executable --version is not source-bound" >&2
    exit 1
fi
if [[ "$embedded_commit" != "$source_commit" ]]; then
    echo "source executable commit does not match requested commit" >&2
    exit 1
fi
if ! LC_ALL=C grep -aFq "$source_commit" "$source_executable"; then
    echo "source commit is not embedded in executable bytes" >&2
    exit 1
fi
source_hash="$(sha256sum "$source_executable" | sed 's/[[:space:]].*$//')"
elf_machine="$(od -An -tx1 -j18 -N2 "$source_executable" | tr -d ' \n')"
if [[ "$elf_machine" != "$elf_machine_hex" ]]; then
    echo "source executable architecture does not match Rust target" >&2
    exit 1
fi

# Exact derivation: git show -s --format=%ct "$source_commit"
SOURCE_DATE_EPOCH="$(git -C "$repo_root" show -s --format=%ct "$source_commit")"
export SOURCE_DATE_EPOCH
if [[ ! "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]]; then
    echo "source commit timestamp is invalid" >&2
    exit 1
fi

work_root="$(mktemp -d "$output_parent/.solstone-tmux-packaging.XXXXXX")"
cleanup_work() {
    rm -rf -- "$work_root"
    cleanup_early
}
trap cleanup_work EXIT
candidate_root="$work_root/candidate"
stage_root="$work_root/stage"
mkdir -m 0700 -- "$candidate_root" "$stage_root"

artifact_names=()
add_artifact() {
    artifact_names+=("$1")
}

if [[ -n "${requested[tar]:-}" ]]; then
    tar_name="solstone-tmux-${product_version}-${archive_arch}-linux.tar.gz"
    tar_stage="$stage_root/tar"
    mkdir -m 0700 -- "$tar_stage"
    install -m 0755 -- "$source_executable" "$tar_stage/solstone-tmux"
    {
        echo "solstone-tmux requires tmux."
        echo "Install with: install -m 0755 solstone-tmux /usr/local/bin/solstone-tmux"
    } >"$tar_stage/INSTALL.md"
    chmod 0644 "$tar_stage/INSTALL.md"
    touch -d "@$SOURCE_DATE_EPOCH" -- \
        "$tar_stage/solstone-tmux" \
        "$tar_stage/INSTALL.md"
    tar --format=ustar \
        --owner=0 \
        --group=0 \
        --numeric-owner \
        --mtime="@$SOURCE_DATE_EPOCH" \
        --mode='u+rwX,go+rX,go-w' \
        -C "$tar_stage" \
        -cf "$work_root/linux.tar" \
        INSTALL.md solstone-tmux
    gzip -n -9 <"$work_root/linux.tar" >"$candidate_root/$tar_name"
    chmod 0644 "$candidate_root/$tar_name"
    add_artifact "$tar_name"
fi

if [[ -n "${requested[deb]:-}" ]]; then
    deb_name="solstone-tmux_${product_version}_${deb_arch}.deb"
    deb_stage="$stage_root/deb"
    mkdir -m 0755 -- \
        "$deb_stage" \
        "$deb_stage/DEBIAN" \
        "$deb_stage/usr" \
        "$deb_stage/usr/bin"
    install -m 0755 -- "$source_executable" "$deb_stage/usr/bin/solstone-tmux"
    {
        echo "Package: solstone-tmux"
        echo "Version: $product_version"
        echo "Architecture: $deb_arch"
        echo "Maintainer: sol pbc"
        echo "Depends: tmux"
        echo "Description: solstone tmux observer"
    } >"$deb_stage/DEBIAN/control"
    chmod 0644 "$deb_stage/DEBIAN/control"
    touch -d "@$SOURCE_DATE_EPOCH" -- \
        "$deb_stage" \
        "$deb_stage/DEBIAN" \
        "$deb_stage/DEBIAN/control" \
        "$deb_stage/usr" \
        "$deb_stage/usr/bin" \
        "$deb_stage/usr/bin/solstone-tmux"
    dpkg-deb --build --root-owner-group -Zgzip -z9 \
        "$deb_stage" \
        "$candidate_root/$deb_name"
    chmod 0644 "$candidate_root/$deb_name"
    add_artifact "$deb_name"
fi

if [[ -n "${requested[rpm]:-}" ]]; then
    rpm_name="solstone-tmux-${product_version}-1.${rpm_arch}.rpm"
    rpm_root="$stage_root/rpmbuild"
    mkdir -m 0700 -- \
        "$rpm_root" \
        "$rpm_root/BUILD" \
        "$rpm_root/BUILDROOT" \
        "$rpm_root/RPMS" \
        "$rpm_root/SOURCES" \
        "$rpm_root/SPECS" \
        "$rpm_root/SRPMS"
    install -m 0755 -- "$source_executable" "$rpm_root/SOURCES/solstone-tmux"
    touch -d "@$SOURCE_DATE_EPOCH" -- "$rpm_root/SOURCES/solstone-tmux"
    spec_file="$rpm_root/SPECS/solstone-tmux.spec"
    {
        echo "Name: solstone-tmux"
        echo "Version: $product_version"
        echo "Release: 1"
        echo "Summary: solstone tmux observer"
        echo "License: AGPL-3.0-only"
        echo "BuildArch: $rpm_arch"
        echo "Requires: tmux"
        echo "AutoReqProv: no"
        echo "Source0: solstone-tmux"
        echo
        echo "%description"
        echo "solstone-tmux experiences tmux sessions along with the owner."
        echo
        echo "%prep"
        echo
        echo "%build"
        echo
        echo "%install"
        echo 'install -D -m 0755 %{SOURCE0} %{buildroot}/usr/bin/solstone-tmux'
        echo
        echo "%files"
        echo "/usr/bin/solstone-tmux"
    } >"$spec_file"
    chmod 0644 "$spec_file"
    touch -d "@$SOURCE_DATE_EPOCH" -- "$spec_file"
    rpmbuild -bb \
        --define "_topdir $rpm_root" \
        --define "_build_id_links none" \
        --define "use_source_date_epoch_as_buildtime 1" \
        --define "clamp_mtime_to_source_date_epoch 1" \
        "$spec_file"
    built_rpm="$rpm_root/RPMS/$rpm_arch/$rpm_name"
    if [[ ! -f "$built_rpm" || -L "$built_rpm" ]]; then
        echo "rpmbuild did not produce the expected package" >&2
        exit 1
    fi
    install -m 0644 -- "$built_rpm" "$candidate_root/$rpm_name"
    add_artifact "$rpm_name"
fi

if [[ "$(sha256sum "$source_executable" | sed 's/[[:space:]].*$//')" != "$source_hash" ]]; then
    echo "source executable changed during packaging" >&2
    exit 1
fi

rustc_vv_file="$work_root/rustc-vv.txt"
rustc -vV >"$rustc_vv_file"
record_name="solstone-tmux-${product_version}-${rust_target}.target.json"
artifacts_json="$work_root/artifacts.json"
: >"$artifacts_json"
for name in "${artifact_names[@]}"; do
    digest="$(sha256sum "$candidate_root/$name" | sed 's/[[:space:]].*$//')"
    jq -cn --arg name "$name" --arg sha256 "$digest" \
        '{name: $name, sha256: $sha256}' >>"$artifacts_json"
done
jq -s 'sort_by(.name)' "$artifacts_json" >"$work_root/artifacts-array.json"
jq -n \
    --arg product_version "$product_version" \
    --arg source_commit "$source_commit" \
    --arg rust_target "$rust_target" \
    --rawfile rustc_vv "$rustc_vv_file" \
    --arg executable_name "solstone-tmux" \
    --arg executable_sha256 "$source_hash" \
    --slurpfile artifacts "$work_root/artifacts-array.json" \
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
chmod 0644 "$candidate_root/$record_name"

mv -- "$candidate_root" "$output_directory"
trap cleanup_early EXIT
rm -rf -- "$work_root"
for name in "${artifact_names[@]}" "$record_name"; do
    digest="$(sha256sum "$output_directory/$name" | sed 's/[[:space:]].*$//')"
    echo "$digest  $name"
done
