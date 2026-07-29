# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Point PyPI installations to the native solstone-tmux release."""

import sys


MESSAGE = (
    "solstone-tmux 1.0.0 is native-only; install it from "
    "https://github.com/solpbc/solstone-tmux/releases\n"
)


def main() -> int:
    """Direct the owner to the native release."""
    sys.stderr.write(MESSAGE)
    return 1
