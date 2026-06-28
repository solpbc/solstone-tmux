# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

from unittest.mock import Mock

import requests
import solstone_tmux
from solstone_tmux.config import Config, load_config
from solstone_tmux.upload import UploadClient


def test_register_posts_descriptor_and_persists(tmp_path):
    config = Config(base_dir=tmp_path, server_url="http://localhost:5015")
    config.ensure_dirs()
    client = UploadClient(config)
    client._session = Mock()
    client._session.post = Mock(
        return_value=Mock(
            status_code=200,
            json=lambda: {
                "key": "abcd1234efgh",
                "prefix": "abcd1234",
                "name": "archon.tmux",
                "ingest_url": "/app/observer/ingest",
                "protocol_version": 2,
            },
        )
    )

    assert client.ensure_registered(config) is True

    call = client._session.post.call_args
    assert call.args[0].endswith("/app/observer/register")
    body = call.kwargs["json"]
    assert body["stream_type"] == "tmux"
    assert isinstance(body["platform"], str) and body["platform"]
    assert body["hostname"]
    assert body["version"] == solstone_tmux.__version__
    assert "stream" not in body and "name" not in body
    assert config.key == "abcd1234efgh" and config.stream == "archon.tmux"
    reloaded = load_config(tmp_path)
    assert reloaded.key == "abcd1234efgh" and reloaded.stream == "archon.tmux"


def test_register_403_marks_revoked(tmp_path):
    config = Config(base_dir=tmp_path, server_url="http://localhost:5015")
    config.ensure_dirs()
    client = UploadClient(config)
    client._session = Mock()
    client._session.post = Mock(
        return_value=Mock(status_code=403, json=lambda: {}, text="")
    )

    assert client.ensure_registered(config) is False
    assert client.is_revoked is True
    assert client._session.post.call_count == 1


def test_preset_key_skips_register(tmp_path):
    config = Config(
        base_dir=tmp_path, server_url="http://localhost:5015", key="EXISTINGKEY"
    )
    config.ensure_dirs()
    client = UploadClient(config)
    client._session = Mock()

    assert client.ensure_registered(config) is True
    client._session.post.assert_not_called()


def test_upload_segment_keyless_bearer_no_stream(tmp_path):
    config = Config(
        base_dir=tmp_path, server_url="http://localhost:5015", key="MYKEY123"
    )
    config.ensure_dirs()
    seg = tmp_path / "seg"
    seg.mkdir()
    f = seg / "tmux_main_screen.jsonl"
    f.write_text("{}\n")
    client = UploadClient(config)
    client._session = Mock()
    client._session.post = Mock(
        return_value=Mock(status_code=200, json=lambda: {"status": "ok"}, text="")
    )

    res = client.upload_segment("20260610", "120000_300", [f])

    assert res.success is True
    call = client._session.post.call_args
    assert call.args[0] == "http://localhost:5015/app/observer/ingest"
    assert "/ingest/MYKEY123" not in call.args[0]
    assert call.kwargs["headers"]["Authorization"] == "Bearer MYKEY123"
    assert call.kwargs["headers"]["X-Solstone-Observer"] == "MYKEY123"
    data = call.kwargs["data"]
    assert "meta" not in data and "stream" not in data


def test_upload_segment_rejected_has_safe_http_reason(tmp_path):
    config = Config(
        base_dir=tmp_path, server_url="http://localhost:5015", key="MYKEY123"
    )
    config.sync_max_retries = 1
    config.sync_retry_delays = [0]
    config.ensure_dirs()
    seg = tmp_path / "seg"
    seg.mkdir()
    f = seg / "tmux_main_screen.jsonl"
    f.write_text("{}\n")
    client = UploadClient(config)
    response_text = "secret-project /tmp/private response body"
    client._session = Mock()
    client._session.post = Mock(
        return_value=Mock(status_code=400, json=lambda: {}, text=response_text)
    )

    result = client.upload_segment("20260610", "120000_300", [f])

    assert result.success is False
    assert result.reason == "http_400"
    assert response_text not in result.reason


def test_upload_segment_request_exception_reason_is_class_name(tmp_path):
    config = Config(
        base_dir=tmp_path, server_url="http://localhost:5015", key="MYKEY123"
    )
    config.sync_max_retries = 1
    config.sync_retry_delays = [0]
    config.ensure_dirs()
    seg = tmp_path / "seg"
    seg.mkdir()
    f = seg / "tmux_main_screen.jsonl"
    f.write_text("{}\n")
    client = UploadClient(config)
    client._session = Mock()
    client._session.post = Mock(side_effect=requests.Timeout("secret-project"))

    result = client.upload_segment("20260610", "120000_300", [f])

    assert result.success is False
    assert result.reason == "Timeout"


def test_relay_event_keyless_bearer_no_stream(tmp_path):
    config = Config(
        base_dir=tmp_path, server_url="http://localhost:5015", key="MYKEY123"
    )
    config.ensure_dirs()
    client = UploadClient(config)
    client._session = Mock()
    client._session.post = Mock(return_value=Mock(status_code=200))

    assert (
        client.relay_event("observe", "status", host="archon", platform="linux") is True
    )

    call = client._session.post.call_args
    assert call.args[0].endswith("/app/observer/ingest/event")
    assert "/ingest/MYKEY123" not in call.args[0]
    assert call.kwargs["headers"]["Authorization"] == "Bearer MYKEY123"
    assert call.kwargs["headers"]["X-Solstone-Observer"] == "MYKEY123"
    payload = call.kwargs["json"]
    assert payload["tract"] == "observe" and payload["event"] == "status"
    assert "stream" not in payload


def test_get_server_segments_keyless_bearer_no_stream_param(tmp_path):
    config = Config(
        base_dir=tmp_path, server_url="http://localhost:5015", key="MYKEY123"
    )
    config.ensure_dirs()
    client = UploadClient(config)
    client._session = Mock()
    client._session.get = Mock(
        return_value=Mock(status_code=200, json=lambda: [{"key": "120000_300"}])
    )

    res = client.get_server_segments("20260610")

    assert res == [{"key": "120000_300"}]
    call = client._session.get.call_args
    assert call.args[0].endswith("/app/observer/ingest/segments/20260610")
    assert "/ingest/MYKEY123" not in call.args[0]
    assert call.kwargs["headers"]["Authorization"] == "Bearer MYKEY123"
    assert call.kwargs["headers"]["X-Solstone-Observer"] == "MYKEY123"
    assert "params" not in call.kwargs
