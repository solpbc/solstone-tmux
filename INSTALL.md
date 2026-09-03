# Installing solstone-tmux

solstone-tmux experiences tmux sessions along with you and syncs completed
observations to your journal. The current release is a native application and
requires tmux.

Supported systems are Linux on x86_64 or aarch64 and macOS on Apple silicon.
Intel macOS, 32-bit systems, and Windows are not supported.

Download packages from the
[latest solstone-tmux release](https://github.com/solpbc/solstone-tmux/releases/latest).

**Verify first.** Download the package you will install, `SHA256SUMS`, and
`SHA256SUMS.minisig` from that release. Then fetch the published key,
authenticate the checksum file, and check the package against it:

```sh
curl -fLo solstone-tmux-release.pub https://updates.solstone.app/solstone-tmux/minisign.pub
minisign -Vm SHA256SUMS -p solstone-tmux-release.pub
# Linux
awk -v package='<downloaded-package>' '$2 == package' SHA256SUMS | sha256sum -c -
# macOS
awk -v package='<downloaded-package>' '$2 == package' SHA256SUMS | shasum -a 256 -c -
```

The complete release has 13 files: 11 packages and target records,
`SHA256SUMS`, and its detached signature. Minisign authenticates `SHA256SUMS`;
the next command verifies the downloaded package against one of its 11 entries.
Replace `<downloaded-package>` with its exact filename. You run these checks;
`apt` and `dnf` do not. If either command refuses the files, stop. Then install
one of the packages below.

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
tar -xzf solstone-tmux-1.0.2-<architecture>-linux.tar.gz
sudo install -m 0755 solstone-tmux /usr/local/bin/solstone-tmux
/usr/local/bin/solstone-tmux --version
```

### deb

`apt` does not check our minisign signature. Complete the verify-first step
above before running:

```sh
sudo apt install ./solstone-tmux_1.0.2_<architecture>.deb
/usr/bin/solstone-tmux --version
```

The package declares its tmux dependency.

### RPM

`dnf` does not check our minisign signature. Complete the verify-first step
above before running:

```sh
sudo dnf install ./solstone-tmux-1.0.2-1.<architecture>.rpm
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

The macOS installer does not run minisign for you. Complete the verify-first
step above before running:

```sh
sudo installer -pkg solstone-tmux-1.0.2-aarch64-macos.pkg -target /
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
