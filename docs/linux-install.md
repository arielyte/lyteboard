# Local Linux installation

Lyteboard runs as a normal KDE application with a separate daemon. This installs only for your current user, without sudo or a system service. The release window embeds its frontend. Normal use needs no repository, Cargo, Node, npm, Vite, or terminal.

## Install or upgrade

Run from the repository in your normal graphical login session:

```sh
python3 scripts/install-linux.py install
```

Use the same command to upgrade. Building requires the Rust toolchain pinned by the repository, Python 3.9+, npm, and the [desktop build prerequisites](../README.md#build-and-run). Runtime dependencies include GTK 3, WebKitGTK 4.1, and an unlocked persistent Secret Service collection. Wayland clipboard operations need `wl-clipboard` 2.3+; X11 needs `xclip`. The script does not install system packages. It refreshes KDE and desktop caches when their utilities are available.

To install an already-built release:

```sh
cargo build --locked --release -p lyteboard-daemon -p lyteboard-cli
npm --prefix apps/desktop ci
npm --prefix apps/desktop run tauri build -- --no-bundle --ci -- --locked
python3 scripts/install-linux.py install --skip-build
```

For a custom Cargo output directory, pass `--release-dir /absolute/path/to/target/release`. Release artifacts must have been built from the same revision. A CLI-only installation uses `python3 scripts/install-linux.py install --cli-only`; it does not change desktop startup preferences or install a launcher.

The installer uses these paths:

| Item | Location |
| --- | --- |
| Release binaries | `~/.local/lib/lyteboard/bin/` |
| Shell command symlinks | `~/.local/bin/lyteboard`, `lyteboardd`, `lyteboard-desktop` |
| Application launcher | `$XDG_DATA_HOME/applications/io.github.arielyte.lyteboard.desktop` |
| Application icon | `$XDG_DATA_HOME/icons/hicolor/256x256/apps/io.github.arielyte.lyteboard.png` |
| Uninstaller | `~/.local/lib/lyteboard/install-linux.py` |

`XDG_DATA_HOME` defaults to `~/.local/share`; `XDG_CONFIG_HOME` defaults to `~/.config`. Use the same XDG environment for installation and removal. The launcher contains an absolute executable path, so it works independently of your shell's PATH and the repository. Search for **Lyteboard** in KDE's application launcher. Right-click its entry to add it to Favorites or pin it to the task manager.

If needed, add `export PATH="$HOME/.local/bin:$PATH"` to your shell startup file (for example `~/.bashrc` or `~/.zshrc`), then reopen the shell. Fish users can run `fish_add_path "$HOME/.local/bin"`. The GUI needs no PATH changes.

## Lifecycle and upgrades

The first GUI installation enables **Settings → Start daemon when Lyteboard opens** in the existing private desktop preferences. Subsequent upgrades preserve explicit choices, including disabling that setting. **Open Lyteboard at login** remains independently opt-in. Automatic clipboard sending, automatic application, and persistent history retain their existing privacy defaults.

Closing or quitting the GUI leaves the daemon running. **Stop daemon and quit** is the explicit tray action for stopping both. The CLI and GUI share the same single-instance launcher and authenticated IPC.

An upgrade stages complete release binaries before changing the installation. It closes windows belonging to this installed executable, asks the active daemon in the current runtime profile to stop through authenticated IPC, waits for its process to exit, replaces the binaries under the existing lifecycle locks, and restarts whichever installed components were running. A stopped daemon stays stopped until requested or the GUI opens. Quit any development window before installation; the installer refuses to replace a running, unrelated GUI. A failed build or missing artifact leaves running binaries alone. If installation is interrupted during replacement, rerun the same install command before launching the application.

The installer never changes device profiles, credentials, trust, history, or daemon settings. A daemon restart drops current sessions and in-memory received items; trusted peers reconnect normally. An upgrade can briefly interrupt transfers, so finish active transfers first. An already active optional `lyteboardd.service` is stopped and restarted through the user service manager after confirming its PID matches this runtime profile. No unit is installed or enabled automatically. A custom service executable path is refused until its `ExecStart` is updated to the installed binary. Other active runtime profiles using these same installed binaries must be stopped before an upgrade; the installer refuses to leave their old executables running.

## Remove

No repository or build tools are needed to uninstall:

```sh
python3 "$HOME/.local/lib/lyteboard/install-linux.py" uninstall
```

Or from the repository:

```sh
python3 scripts/install-linux.py uninstall
```

Removal stops the installed processes and removes binaries, symlinks, the application launcher, icon, and managed login autostart entry. Identity, trusted devices, saved settings, history, and Secret Service credentials are preserved. A few empty installation directories and lock files may remain. There is no automatic data-purge option. If you use the optional systemd unit, first disable and remove it using the [daemon guide](daemon.md#install-the-optional-systemd-user-service).

The installer refuses to replace unmanaged files at command symlink paths. If upgrading from the older manual installation, stop its daemon, quit its GUI, and move the three old executables from `~/.local/bin` to a backup directory before installing. Leave profile and configuration files in place.

## Verify

```sh
desktop-file-validate "${XDG_DATA_HOME:-$HOME/.local/share}/applications/io.github.arielyte.lyteboard.desktop"
gtk-launch io.github.arielyte.lyteboard
"$HOME/.local/bin/lyteboard" ping
"$HOME/.local/bin/lyteboard" status
"$HOME/.local/bin/lyteboard" network status
```

Close the window and verify `ping` still succeeds. Reopen from KDE and confirm only one window appears. Reinstall while running and confirm `status` reports a new daemon PID, with the same paired devices and settings. A locked credential store is reported in the LAN controls; unlock your login keyring before pairing or reconnecting.

On another device, use the Linux listener's current LAN IP and TCP port. Allow that actual port through your firewall for the intended LAN source range. mDNS uses UDP 5353 and does not replace the TCP firewall rule. See the [Wi-Fi/firewall troubleshooting guide](../apps/android/README.md). When testing image clipboard reception, prepare paste commands before receiving the image so copying a command does not replace the selection. LAN-6E passed the user's physical Samsung-to-KDE-to-ChatGPT paste test.
