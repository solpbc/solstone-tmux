# installing solstone-tmux

these instructions are for a coding agent and human working together. solstone-tmux experiences your tmux sessions along with you — every 5 seconds it takes in what's on each pane. lightweight, pure python, no system dependencies. works alongside a desktop observer or standalone.

solstone must already be installed and running. if it isn't, start there: https://solstone.app/install

> **most users should run `sol observer install --platform tmux` from the solstone host instead of following this file by hand.** that one command handles the clone, the build, the registration, and the systemd unit — including a `--dry-run` mode to preview every step. the instructions below are for developers building from source or troubleshooting the install.

## before you begin

if `sol` is not in PATH, check `~/.local/bin/sol`.

check if solstone-tmux is already installed and running:

```
systemctl --user status solstone-tmux
sol observer list
```

if it's already active and connected, you're done.

## install

1. **clone and install the service.**
   ```
   git clone https://github.com/solpbc/solstone-tmux.git solstone-tmux
   cd solstone-tmux
   make install-service
   ```
   this installs the `solstone-tmux` command via pipx and sets up the systemd user unit.

2. **run setup.**
   ```
   solstone-tmux setup
   ```
   this prompts for your solstone journal URL, registers the observer, and writes the config file.

3. **verify.**
   ```
   solstone-tmux status
   systemctl --user status solstone-tmux
   ```

## updating after a code change

```
git pull && make install-service
```

`make install-service` skips CI for a fresh install, but runs `make ci` before upgrading an existing owned install. if tests fail, the upgrade aborts before touching the installed service.

## optional cache retention

by default, synced segments are deleted after 7 days. to change this, add `cache_retention_days` to config.json:

- positive number: keep synced segments for that many days (default: `7`)
- `0`: delete immediately after confirmed sync
- `-1`: keep forever (never auto-delete)

## status bar indicator

solstone-tmux shows a ☼ symbol at the left edge of your tmux status bar while running:

- **yellow ☼** — observer active, sync connected
- **grey ☼** — observer active, sync offline (journal unreachable or not configured)
- **absent** — observer not running

the indicator is removed automatically on clean shutdown (SIGTERM, SIGINT). if the observer is killed with SIGKILL or the system crashes, the indicator may persist. to clear it manually:

```
tmux set -g @solstone ""
```

to disable the indicator entirely, add to config.json:

```json
{
  "status_indicator": false
}
```

## notes

- if pipx is not installed: `pip install --user pipx` or install via your package manager.
- the observer works offline — segments sync when your journal becomes available.
