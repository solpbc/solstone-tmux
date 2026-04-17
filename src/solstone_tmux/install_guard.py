# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
from __future__ import annotations

import argparse
import enum
import os
import sys
from pathlib import Path

APP_NAME = "solstone-tmux"
MARKER_PATH = Path.home() / ".config" / "solstone-tmux" / ".install-source"
PIPX_BIN_PATH = Path.home() / ".local" / "bin" / APP_NAME


class State(enum.Enum):
    ABSENT = "ABSENT"
    OWNED = "OWNED"
    PARTIAL_OWNED = "PARTIAL_OWNED"
    CROSS_REPO = "CROSS_REPO"
    UNKNOWN = "UNKNOWN"
    MALFORMED = "MALFORMED"


def _read_marker() -> tuple[State | None, Path | None]:
    if not MARKER_PATH.exists():
        return State.ABSENT, None

    lines = MARKER_PATH.read_text(encoding="utf-8").splitlines()
    if len(lines) != 1:
        return State.MALFORMED, None

    marker_path = Path(lines[0])
    if not marker_path.is_absolute():
        return State.MALFORMED, None

    return None, marker_path


def detect_state(repo_root: Path) -> tuple[State, Path | None]:
    marker_state, marker_path = _read_marker()
    repo_root = repo_root.resolve()
    bin_exists = PIPX_BIN_PATH.exists()

    if marker_state == State.MALFORMED:
        return State.MALFORMED, None

    if marker_state == State.ABSENT:
        if bin_exists:
            return State.UNKNOWN, None
        return State.ABSENT, None

    assert marker_path is not None
    resolved_marker = marker_path.resolve()
    if resolved_marker != repo_root:
        return State.CROSS_REPO, resolved_marker

    if bin_exists:
        return State.OWNED, None
    return State.PARTIAL_OWNED, None


def write_marker(repo_root: Path) -> None:
    MARKER_PATH.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
    tmp_path = MARKER_PATH.with_name(f"{MARKER_PATH.name}.tmp")
    tmp_path.write_text(f"{repo_root.resolve()}\n", encoding="utf-8")
    os.rename(tmp_path, MARKER_PATH)


def remove_marker() -> None:
    MARKER_PATH.unlink(missing_ok=True)


def _print_refusal(state: State, repo_root: Path, stale_path: Path | None) -> None:
    installed = "unknown"
    if state == State.UNKNOWN:
        installed = (
            "unknown (no .install-source marker \u2014 likely pre-hygiene install)"
        )
    elif state == State.MALFORMED:
        installed = "unknown (malformed marker)"
    elif stale_path is not None:
        installed = str(stale_path)

    print("mode: aborted \u2014 cross-repo contamination", file=sys.stderr)
    print(
        f"ERROR: Another {APP_NAME} install owns ~/.local/bin/{APP_NAME}.",
        file=sys.stderr,
    )
    print(f"  this repo:  {repo_root.resolve()}", file=sys.stderr)
    print(f"  installed:  {installed}", file=sys.stderr)
    print(
        "Run `make uninstall-service` from the installed repo first,",
        file=sys.stderr,
    )
    print(
        "or manually remove the pipx package and ~/.config/solstone-tmux/ if that repo is gone. No --force available.",
        file=sys.stderr,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Guard solstone-tmux installs by repo."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser("check", help="Print install ownership state")
    subparsers.add_parser("install", help="Validate install ownership")
    subparsers.add_parser("uninstall", help="Validate uninstall ownership")

    write_parser = subparsers.add_parser("write-marker", help="Write ownership marker")
    write_parser.add_argument("--repo-root", type=Path, required=True)

    subparsers.add_parser("remove-marker", help="Remove ownership marker")

    args = parser.parse_args(argv)
    repo_root = Path.cwd()

    if args.command == "check":
        state, _stale_path = detect_state(repo_root)
        print(state.name)
        return 0

    if args.command == "install":
        state, stale_path = detect_state(repo_root)
        if state == State.ABSENT:
            print("mode: fresh install")
            return 0
        if state == State.OWNED:
            print("mode: upgrade")
            return 0
        if state == State.PARTIAL_OWNED:
            print("mode: upgrade (repair)")
            return 0
        _print_refusal(state, repo_root, stale_path)
        return 2

    if args.command == "uninstall":
        state, stale_path = detect_state(repo_root)
        if state == State.ABSENT:
            print("no artifacts to remove")
            return 0
        if state in {State.OWNED, State.PARTIAL_OWNED}:
            return 0
        _print_refusal(state, repo_root, stale_path)
        return 2

    if args.command == "write-marker":
        write_marker(args.repo_root)
        return 0

    remove_marker()
    return 0


if __name__ == "__main__":
    sys.exit(main())
