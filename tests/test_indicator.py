# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import pytest

from solstone_tmux import indicator


@pytest.fixture(autouse=True)
def reset_original_status_left():
    indicator._original_status_left = None
    yield
    indicator._original_status_left = None


def test_install_saves_and_prepends(monkeypatch):
    calls = []

    def fake_run_tmux_command(args):
        calls.append(args)
        if args == ["show", "-gv", "status-left"]:
            return "existing-status\n"
        return None

    monkeypatch.setattr(indicator, "run_tmux_command", fake_run_tmux_command)

    indicator.install()

    assert calls == [
        ["show", "-gv", "status-left"],
        ["set", "-g", "status-left", indicator._INDICATOR_FMT + "existing-status"],
        ["set", "-g", "@solstone", "observing"],
    ]
    assert indicator._original_status_left == "existing-status"


def test_install_idempotent(monkeypatch):
    calls = []
    indicator._original_status_left = "keep-me"

    def fake_run_tmux_command(args):
        calls.append(args)
        if args == ["show", "-gv", "status-left"]:
            return indicator._SENTINEL + "existing-status\n"
        return None

    monkeypatch.setattr(indicator, "run_tmux_command", fake_run_tmux_command)

    indicator.install()

    assert calls == [["show", "-gv", "status-left"]]
    assert indicator._original_status_left == "keep-me"


def test_install_tmux_unavailable(monkeypatch):
    calls = []

    def fake_run_tmux_command(args):
        calls.append(args)
        return None

    monkeypatch.setattr(indicator, "run_tmux_command", fake_run_tmux_command)

    indicator.install()

    assert calls == [["show", "-gv", "status-left"]]
    assert indicator._original_status_left is None


def test_update_syncing(monkeypatch):
    calls = []

    def fake_run_tmux_command(args):
        calls.append(args)
        return None

    monkeypatch.setattr(indicator, "run_tmux_command", fake_run_tmux_command)

    indicator.update(True)

    assert calls == [["set", "-g", "@solstone", "syncing"]]


def test_update_observing(monkeypatch):
    calls = []

    def fake_run_tmux_command(args):
        calls.append(args)
        return None

    monkeypatch.setattr(indicator, "run_tmux_command", fake_run_tmux_command)

    indicator.update(False)

    assert calls == [["set", "-g", "@solstone", "observing"]]


def test_remove_restores(monkeypatch):
    calls = []
    indicator._original_status_left = "my-original"

    def fake_run_tmux_command(args):
        calls.append(args)
        return None

    monkeypatch.setattr(indicator, "run_tmux_command", fake_run_tmux_command)

    indicator.remove()

    assert calls == [
        ["set", "-g", "status-left", "my-original"],
        ["set", "-g", "@solstone", ""],
    ]
    assert indicator._original_status_left is None


def test_remove_no_original(monkeypatch):
    calls = []

    def fake_run_tmux_command(args):
        calls.append(args)
        return None

    monkeypatch.setattr(indicator, "run_tmux_command", fake_run_tmux_command)

    indicator.remove()

    assert calls == [["set", "-g", "@solstone", ""]]
