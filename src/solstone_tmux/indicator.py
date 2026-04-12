# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Tmux status-left indicator for observer and sync state."""

import logging

from .capture import run_tmux_command

logger = logging.getLogger(__name__)

_original_status_left: str | None = None
_SENTINEL = "#{?@solstone,"
_INDICATOR_FMT = (
    "#{?@solstone,"
    "#{?#{==:#{@solstone},syncing},#[fg=yellow]☼#[default],#[fg=colour245]☼#[default]},"
    "}"
)


def install() -> None:
    """Install the status indicator into tmux's status-left."""
    global _original_status_left

    status_left = run_tmux_command(["show", "-gv", "status-left"])
    if status_left is None:
        logger.warning("Unable to install tmux status indicator: tmux unavailable")
        return

    status_left = status_left.rstrip("\n")
    if _SENTINEL in status_left:
        return

    _original_status_left = status_left
    new_value = f"{_INDICATOR_FMT}{status_left}"
    run_tmux_command(["set", "-g", "status-left", new_value])
    run_tmux_command(["set", "-g", "@solstone", "observing"])


def update(syncing: bool) -> None:
    """Update the indicator state user variable."""
    value = "syncing" if syncing else "observing"
    run_tmux_command(["set", "-g", "@solstone", value])


def remove() -> None:
    """Remove the status indicator and restore the original status-left."""
    global _original_status_left

    if _original_status_left is None:
        run_tmux_command(["set", "-g", "@solstone", ""])
        return

    run_tmux_command(["set", "-g", "status-left", _original_status_left])
    run_tmux_command(["set", "-g", "@solstone", ""])
    _original_status_left = None
