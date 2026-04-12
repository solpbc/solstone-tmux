# installing solstone-tmux

these instructions are for a coding agent and human working together. solstone-tmux captures all active tmux sessions and panes every 5 seconds. lightweight, pure python, no system dependencies. works alongside a desktop observer or standalone.

solstone must already be installed and running. if it isn't, start there: https://solstone.app/install

## before you begin

if `sol` is not in PATH, check `~/.local/bin/sol` or use `.venv/bin/sol` inside the solstone repo.

check if solstone-tmux is already installed and running:

```
systemctl --user status solstone-tmux
sol observer list
```

if it's already active and connected, you're done.

## what to sort out together

- **stream name.** this identifies the capture source. the convention is `hostname.tmux` (e.g., `fedora.tmux`).

## install sequence

1. if not already cloned, clone into solstone's observers directory and install with pipx:
   ```
   cd "$(sol root)/observers"
   git clone https://github.com/solpbc/solstone-tmux.git
   cd solstone-tmux
   pipx install .
   ```

2. register the observer with solstone and save the API key:
   ```
   sol observer create solstone-tmux
   ```

3. write the config to `~/.local/share/solstone-tmux/config/config.json`:
   ```json
   {
     "server_url": "http://localhost:5015",
     "key": "THE_API_KEY_FROM_STEP_2",
     "stream": "HOSTNAME.tmux"
   }
   ```

   **optional: cache retention.** by default, synced segments are deleted after 7 days. to change this, add `cache_retention_days` to config.json:
   - positive number: keep synced segments for that many days (default: `7`)
   - `0`: delete immediately after confirmed sync
   - `-1`: keep forever (never auto-delete)

   ```json
   {
     "server_url": "http://localhost:5015",
     "key": "THE_API_KEY_FROM_STEP_2",
     "stream": "HOSTNAME.tmux",
     "cache_retention_days": 7
   }
   ```

4. install and start the systemd user service:
   ```
   solstone-tmux install-service
   ```

5. verify it's running and connected:
   ```
   systemctl --user status solstone-tmux
   sol observer list
   ```

## status bar indicator

solstone-tmux shows a ☼ symbol at the left edge of your tmux status bar while running:

- **yellow ☼** — observer active, sync connected
- **grey ☼** — observer active, sync offline (server unreachable or not configured)
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
- captures work offline — segments sync when the server becomes available.
