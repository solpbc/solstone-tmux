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
    x86_64-*-linux-gnu | aarch64-*-linux-gnu | x86_64-*-linux-musl | aarch64-*-linux-musl) ;;
    *)
        echo "unsupported native Linux release host: $host_target" >&2
        exit 1
        ;;
esac
# The shipped Linux lanes are musl while the build hosts are gnu, so match on
# architecture, vendor and OS and ignore the libc component.
host_base="$(echo "$host_target" | cut -d- -f1-3)"
# The lane builds the configured target for this architecture, which is musl while the host is
# gnu. Resolve it here so the build, packaging and proof all refer to the same triple.
lane_target="$("$repo_root/scripts/rust-targets.sh" | while IFS= read -r configured; do
    if [[ "$(echo "$configured" | cut -d- -f1-3)" == "$host_base" ]]; then echo "$configured"; fi
done | head -n 1)"
if [[ -z "$lane_target" ]]; then
    echo "native Rust host cannot build any configured target: $host_target" >&2
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

###############################################################################
# !!!  DANGER  —  READ THIS BEFORE TOUCHING ANY tmux CALL IN THIS SCRIPT  !!!
#
# Every tmux invocation in this lane MUST carry an explicit
# -S "$lane_tmux_socket". Not TMUX_TMPDIR. Not a bare `tmux`. -S.
#
# TMUX_TMPDIR IS NOT ISOLATION. When $TMUX is set in the environment — which it
# is any time this lane runs inside a tmux pane, i.e. every automated build,
# every `tmux-run`, every operator shell — tmux reads its socket path out of $TMUX and
# ignores TMUX_TMPDIR completely. In that context
#
#     TMUX_TMPDIR=/some/scratch tmux kill-server
#
# is a plain `tmux kill-server` against the operator's REAL server.
#
# This is not hypothetical. That exact line, in this exact cleanup function,
# killed the extro box's live tmux server twice on 2026-08-08 (13:49 and 14:51
# MDT), each time ~75s after `make release-linux` started, taking down every
# pane, the hub daemon, and every running lane with it. The same script run over
# `ssh spark` on the same afternoon was harmless — because ssh does not carry
# $TMUX. That asymmetry is exactly how the bug stayed hidden.
#
# -S (and -L) are the ONLY forms that override $TMUX. Both halves below are
# load-bearing; removing either one re-arms the failure:
#
#   1. -S pins every tmux CLI call in this script to the scratch socket.
#   2. TMUX is unset so the observer binary lands on the scratch server too.
#      The observer deliberately passes no socket flag — asserted by
#      native/solstone-tmux/tests/tmux_adapter.rs — so it resolves
#      $TMUX -> TMUX_TMPDIR -> /tmp. With $TMUX still set it would observe the
#      operator's live session instead of the candidate session below, and the
#      lane's "durable local output" proof would pass for the wrong reason.
###############################################################################
unset TMUX TMUX_PANE
# The path tmux itself derives from TMUX_TMPDIR, pinned explicitly so the CLI
# calls and the observer's own resolution agree on one server.
lane_tmux_socket="$work_root/tmux/tmux-$UID/default"

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
    # -S, always. See the DANGER block above. The guard is a socket test on our
    # own scratch path, so this cannot fire against a server we did not create.
    if [[ -S "$lane_tmux_socket" ]]; then
        HOME="$work_root/home" tmux -S "$lane_tmux_socket" kill-server >/dev/null 2>&1
    fi
    rm -rf -- "$work_root"
}
trap cleanup EXIT

cd "$repo_root"
cargo fetch --locked
make ci

packager_log="$work_root/packager.log"
make package-linux \
    RUST_TARGET="$lane_target" \
    SOURCE_COMMIT="$source_commit" \
    OUTPUT_DIRECTORY="$output_directory" \
    2>&1 | tee "$packager_log"

source_executable="$repo_root/target/$lane_target/release/solstone-tmux"
SOLSTONE_TMUX_TEST_CANDIDATE="$output_directory" \
SOLSTONE_TMUX_TEST_TARGET="$lane_target" \
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
# tmux creates this directory itself under default resolution, but -S binds the
# socket at a literal path and creates no parents. The observer reaches the same
# socket the other way, through TMUX_TMPDIR, so the layout has to match.
mkdir -m 0700 "$(dirname "$lane_tmux_socket")"
mkdir -m 0700 "$XDG_CONFIG_HOME/solstone-tmux"
tmux_path="$(command -v tmux)"
tmux -S "$lane_tmux_socket" -f /dev/null new-session -d -s candidate \
    "while :; do printf 'durable candidate observation\\n'; sleep 1; done"
SOLSTONE_TMUX_LANE_SOCKET="$lane_tmux_socket" \
    script -q -c 'tmux -S "$SOLSTONE_TMUX_LANE_SOCKET" attach-session -t candidate' /dev/null \
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
