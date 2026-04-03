# solstone-tmux

Standalone tmux terminal observer for [solstone](https://solpbc.org). Captures tmux terminal sessions to a local cache and syncs them to a solstone server.

## Install

```bash
pipx install solstone-tmux
```

## Setup

```bash
solstone-tmux setup
```

Prompts for your solstone server URL and auto-registers an observer key.

## Run

```bash
# Run directly
solstone-tmux run

# Or install as a systemd user service
solstone-tmux install-service
systemctl --user status solstone-tmux
```

## Status

```bash
solstone-tmux status
```

Shows capture state, sync state, cache size, and last sync time.

## License

AGPL-3.0-only. Copyright (c) 2026 sol pbc.
