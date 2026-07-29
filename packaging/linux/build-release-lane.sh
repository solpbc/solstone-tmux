#!/usr/bin/env bash
set -euo pipefail

umask 077

if (($# != 2)); then
    echo "usage: build-release-lane.sh <source-commit> <output-directory>" >&2
    exit 2
fi

source_commit="$1"
output_directory="$2"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "Linux release lanes must run on a native Linux machine" >&2
    exit 1
fi
if [[ ! "$source_commit" =~ ^[0-9a-f]{40}$ ]]; then
    echo "source commit must be lowercase 40-hex" >&2
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
if [[ "$(git -C "$repo_root" rev-parse HEAD)" != "$source_commit" ]]; then
    echo "source commit must equal the checked-out HEAD" >&2
    exit 1
fi
if [[ -n "$(git -C "$repo_root" status --porcelain=v1 --untracked-files=all)" ]]; then
    echo "Linux release source tree must be clean" >&2
    exit 1
fi

host_target="$(rustc -vV | sed -n 's/^host: //p')"
case "$host_target" in
    x86_64-*-linux-gnu | aarch64-*-linux-gnu) ;;
    *)
        echo "unsupported native Linux release host: $host_target" >&2
        exit 1
        ;;
esac
if ! "$repo_root/scripts/rust-targets.sh" | grep -Fx "$host_target" >/dev/null; then
    echo "native Rust host is absent from rust-toolchain.toml: $host_target" >&2
    exit 1
fi

for tool in cargo cargo-deny find git grep make rustc script sed seq sleep tee tmux; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "required Linux release tool is unavailable: $tool" >&2
        exit 1
    fi
done

output_parent="$(dirname "$output_directory")"
if [[ ! -d "$output_parent" ]]; then
    echo "candidate output parent does not exist: $output_parent" >&2
    exit 1
fi
work_root="$(mktemp -d "$output_parent/.solstone-tmux-release.XXXXXX")"
observer_pid=""
client_pid=""
cleanup() {
    set +e
    if [[ -n "$observer_pid" ]]; then
        kill -TERM "$observer_pid" >/dev/null 2>&1
        wait "$observer_pid" >/dev/null 2>&1
    fi
    if [[ -n "$client_pid" ]]; then
        kill -TERM "$client_pid" >/dev/null 2>&1
        wait "$client_pid" >/dev/null 2>&1
    fi
    if [[ -d "$work_root/tmux" ]]; then
        HOME="$work_root/home" TMUX_TMPDIR="$work_root/tmux" \
            tmux kill-server >/dev/null 2>&1
    fi
    rm -rf -- "$work_root"
}
trap cleanup EXIT

cd "$repo_root"
cargo fetch --locked
make ci

packager_log="$work_root/packager.log"
make package-linux \
    RUST_TARGET="$host_target" \
    SOURCE_COMMIT="$source_commit" \
    OUTPUT_DIRECTORY="$output_directory" \
    2>&1 | tee "$packager_log"

source_executable="$repo_root/target/$host_target/release/solstone-tmux"
SOLSTONE_TMUX_TEST_CANDIDATE="$output_directory" \
SOLSTONE_TMUX_TEST_TARGET="$host_target" \
SOLSTONE_TMUX_TEST_EXECUTABLE="$source_executable" \
SOLSTONE_TMUX_TEST_PACKAGER_LOG="$(<"$packager_log")" \
    cargo test --locked -p solstone-tmux --test release_validator \
        validates_real_linux_lane_when_requested -- --exact

export HOME="$work_root/home"
export XDG_CONFIG_HOME="$work_root/config"
export XDG_DATA_HOME="$work_root/data"
export TMUX_TMPDIR="$work_root/tmux"
export TERM=xterm-256color
mkdir -m 0700 \
    "$HOME" \
    "$XDG_CONFIG_HOME" \
    "$XDG_DATA_HOME" \
    "$TMUX_TMPDIR"
mkdir -m 0700 "$XDG_CONFIG_HOME/solstone-tmux"
tmux_path="$(command -v tmux)"
tmux -f /dev/null new-session -d -s candidate \
    "while :; do printf 'durable candidate observation\\n'; sleep 1; done"
script -q -c "tmux attach-session -t candidate" /dev/null \
    >"$work_root/tmux-client.log" 2>&1 &
client_pid="$!"
printf '{"tmux_path":"%s"}\n' "$tmux_path" \
    >"$XDG_CONFIG_HOME/solstone-tmux/local-observer.json"
printf '{"capture_interval":5,"segment_interval":2,"status_indicator":false}\n' \
    >"$XDG_CONFIG_HOME/solstone-tmux/config.json"
chmod 0600 "$XDG_CONFIG_HOME/solstone-tmux/"*.json
"$source_executable" run >"$work_root/observer.stdout" 2>"$work_root/observer.stderr" &
observer_pid="$!"
observed=false
for _ in $(seq 1 45); do
    if find "$XDG_DATA_HOME/solstone-tmux/captures" \
        -type f -name '*.jsonl' -size +0c -print -quit 2>/dev/null |
        grep -q .
    then
        observed=true
        break
    fi
    sleep 1
done
if ! $observed; then
    echo "foreground observer did not produce durable local output" >&2
    exit 1
fi
kill -TERM "$observer_pid"
wait "$observer_pid"
observer_pid=""
if find "$XDG_DATA_HOME/solstone-tmux/captures" \
    \( -name '*.incomplete' -o -name '*.incomplete.meta' \) -print -quit |
    grep -q .
then
    echo "foreground observer did not shut down cleanly" >&2
    exit 1
fi

printf '%s\n' "$output_directory"
