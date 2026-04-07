# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import pytest

from solstone_tmux.streams import _strip_hostname, stream_name


class TestStripHostname:
    def test_simple(self):
        assert _strip_hostname("archon") == "archon"

    def test_domain(self):
        assert _strip_hostname("ja1r.local") == "ja1r"

    def test_fqdn(self):
        assert _strip_hostname("my.host.example.com") == "my"

    def test_ip(self):
        assert _strip_hostname("192.168.1.1") == "192-168-1-1"

    def test_empty(self):
        assert _strip_hostname("") == ""

    def test_whitespace(self):
        assert _strip_hostname("  archon  ") == "archon"


class TestStreamName:
    def test_host(self):
        assert stream_name(host="archon") == "archon"

    def test_host_with_qualifier(self):
        assert stream_name(host="archon", qualifier="tmux") == "archon.tmux"

    def test_observer(self):
        assert stream_name(observer="laptop") == "laptop"

    def test_host_domain_stripped(self):
        assert stream_name(host="ja1r.local", qualifier="tmux") == "ja1r.tmux"

    def test_no_source_raises(self):
        with pytest.raises(ValueError):
            stream_name()

    def test_invalid_name_raises(self):
        with pytest.raises(ValueError):
            stream_name(host="", qualifier="tmux")
