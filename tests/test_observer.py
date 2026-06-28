# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import time
from unittest.mock import Mock, patch

import solstone_tmux
from solstone_tmux.config import Config
from solstone_tmux.observer import TmuxObserver


BEACON_FIELDS = {
    "name",
    "stream_type",
    "version",
    "uptime",
    "last_successful_sync",
    "pending_queue_depth",
    "recent_error_count",
    "last_error_reason",
}


def _observer(tmp_path):
    observer = TmuxObserver(Config(base_dir=tmp_path, key="KEY", stream="archon.tmux"))
    observer._client = Mock(is_registered=True)
    observer._sync = Mock(
        is_connected=True,
        health_snapshot=Mock(
            return_value={
                "last_successful_sync": 1782600000000,
                "pending_queue_depth": 0,
                "recent_error_count": 0,
                "last_error_reason": None,
            }
        ),
    )
    return observer


def test_emit_status_registered_sends_health_beacon(tmp_path):
    observer = _observer(tmp_path)

    with patch("solstone_tmux.observer.indicator.update"):
        observer.emit_status()

    observer._client.relay_event.assert_called_once()
    call = observer._client.relay_event.call_args
    assert call.args == ("observe", "status")

    payload = call.kwargs
    assert BEACON_FIELDS <= payload.keys()
    assert payload["name"] == "archon.tmux"
    assert payload["stream_type"] == "tmux"
    assert payload["version"] == solstone_tmux.__version__
    assert isinstance(payload["uptime"], int)
    assert payload["uptime"] >= 0
    assert isinstance(payload["last_successful_sync"], int)
    assert isinstance(payload["pending_queue_depth"], int)
    assert isinstance(payload["recent_error_count"], int)
    assert payload["recent_error_count"] == 0
    assert payload["last_error_reason"] is None


def test_emit_status_unregistered_skips_relay(tmp_path):
    observer = _observer(tmp_path)
    observer._client.is_registered = False

    with patch("solstone_tmux.observer.indicator.update"):
        observer.emit_status()

    observer._client.relay_event.assert_not_called()


def test_emit_status_payload_excludes_tmux_session_names(tmp_path):
    observer = _observer(tmp_path)
    observer.sessions_seen = {"secret-project"}

    with patch("solstone_tmux.observer.indicator.update"):
        observer.emit_status()

    payload = observer._client.relay_event.call_args.kwargs
    assert "sessions" not in payload
    assert "tmux" not in payload
    assert "mode" not in payload
    assert "secret-project" not in " ".join(str(value) for value in payload.values())


def test_emit_status_relay_failure_isolated(tmp_path):
    observer = _observer(tmp_path)
    observer._client.relay_event.side_effect = RuntimeError("relay failed")

    with patch("solstone_tmux.observer.indicator.update"):
        observer.emit_status()


def test_status_uptime_uses_process_start_not_segment_start(tmp_path):
    observer = _observer(tmp_path)
    observer.process_start_mono = time.monotonic() - 100

    observer._start_segment()

    with patch("solstone_tmux.observer.indicator.update"):
        observer.emit_status()

    payload = observer._client.relay_event.call_args.kwargs
    assert payload["uptime"] >= 100
