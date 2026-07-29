# Installing solstone-tmux

solstone-tmux experiences tmux sessions along with you and syncs completed
observations to your journal. Version 1.0.0 is a native application and requires
tmux.

Supported systems are Linux on x86_64 or aarch64 and macOS on Apple silicon.
Intel macOS, 32-bit systems, and Windows are not supported.

Download packages from the
[solstone-tmux releases](https://github.com/solpbc/solstone-tmux/releases) page
and verify them with the published `SHA256SUMS` signature before installation.

## Linux

Choose one format. The deb and RPM packages install
`/usr/bin/solstone-tmux`; the tarball installs
`/usr/local/bin/solstone-tmux`.

| System | Tar name | deb name | RPM name |
| --- | --- | --- | --- |
| Linux x86_64 | `x86_64` | `amd64` | `x86_64` |
| Linux aarch64 | `aarch64` | `arm64` | `aarch64` |

### Tarball

Install tmux with your system package manager, then:

```sh
tar -xzf solstone-tmux-1.0.0-<architecture>-linux.tar.gz
sudo install -m 0755 solstone-tmux /usr/local/bin/solstone-tmux
/usr/local/bin/solstone-tmux --version
```

### deb

```sh
sudo apt install ./solstone-tmux_1.0.0_<architecture>.deb
/usr/bin/solstone-tmux --version
```

The package declares its tmux dependency.

### RPM

```sh
sudo dnf install ./solstone-tmux-1.0.0-1.<architecture>.rpm
/usr/bin/solstone-tmux --version
```

The package declares its tmux dependency.

If this is a new installation, continue with [Pair and activate](#pair-and-activate).
If a Python installation already owns `solstone-tmux.service`, complete the
cutover below first.

### Retire a previous Python installation

The native service refuses to alter a unit it did not write. A surviving
`~/.local/bin/solstone-tmux` shim can also shadow the native executable.
Preserve `~/.local/share/solstone-tmux`; it contains the settings and cache used
during adoption.

Set `native_bin` to `/usr/bin/solstone-tmux` for deb or RPM, or to
`/usr/local/bin/solstone-tmux` for the tarball.

<!-- legacy-python-retirement:start -->

1. Stop and disable the previous user service:

   ```sh
   systemctl --user disable --now solstone-tmux.service
   ```

2. Confirm that
   `~/.config/systemd/user/solstone-tmux.service` is the previous Python unit,
   then remove it and reload systemd:

   ```sh
   grep -Fx 'Description=Solstone Tmux Terminal Observer' \
     ~/.config/systemd/user/solstone-tmux.service
   rm ~/.config/systemd/user/solstone-tmux.service
   systemctl --user daemon-reload
   systemctl --user status solstone-tmux.service
   ```

   The final status should report that the unit is absent.

3. Remove the previous package with the tool that installed it:

   ```sh
   uv tool uninstall solstone-tmux
   ```

   or:

   ```sh
   pipx uninstall solstone-tmux
   ```

4. Refresh command lookup and verify the native executable wins:

   ```sh
   hash -r
   command -v solstone-tmux
   ```

   The result must equal the format-specific `native_bin` path above. If it
   still resolves to `~/.local/bin/solstone-tmux`, remove that shim only after
   confirming it belongs to the retired installation, then repeat these two
   commands.

<!-- legacy-python-retirement:end -->

When no native config exists, the first native run on Linux imports the
previous `stream`, `capture_interval`, `segment_interval`,
`cache_retention_days`, and `status_indicator` settings and continues using the
existing cache in place. This is limited settings adoption, not a general
migration. Previous credentials are not carried over; pair again through
`setup`.

## macOS

The notarized pkg supports Apple silicon and installs
`/usr/local/bin/solstone-tmux`:

```sh
sudo installer -pkg solstone-tmux-1.0.0-aarch64-macos.pkg -target /
/usr/local/bin/solstone-tmux --version
```

Install tmux before activation. The tarball contains a Developer-ID-signed
binary, but only the pkg is notarized and stapled.

## Pair and activate

`setup` and `install-service` are separate steps:

1. `setup` reads one private-link pairing link from standard input and stores
   the new pairing:

   ```sh
   solstone-tmux setup < pairing-link.txt
   ```

2. `install-service` activates the observer as the current user's service:

   ```sh
   solstone-tmux install-service
   solstone-tmux status
   ```

Run these through the format-specific absolute path during a Linux cutover if
command lookup has not yet been refreshed. Service installation records the
exact executable invoked; it does not assume an install prefix.

To run without activating a service:

```sh
solstone-tmux run
```

## Status indicator

By default, solstone-tmux owns a small tmux status indicator while it runs:

- yellow means observation is active and sync is connected;
- grey means observation is active and sync is unavailable;
- absent means the observer is not running.

Set `"status_indicator": false` in the native `config.json` to leave tmux
options untouched.

## Uninstall

On Linux, remove the owned user service first:

```sh
solstone-tmux uninstall-service
```

Then remove the installed format:

```sh
sudo apt remove solstone-tmux
```

or:

```sh
sudo dnf remove solstone-tmux
```

For a tar installation, remove `/usr/local/bin/solstone-tmux` after
`uninstall-service` succeeds.

On macOS:

```sh
/usr/local/bin/solstone-tmux uninstall-service
sudo rm /usr/local/bin/solstone-tmux
```

These commands leave settings and cached observations in place.

## Rollback window

The previous Python path remains available only during the operator's short
pre-publication cutover window. It may be restored from the operator's own
rollback materials during that window after the native service is removed.
The 1.0.0 release provides no long-lived Python fallback.
