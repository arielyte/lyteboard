# Linux daemon and local IPC

The CLI and desktop window communicate with the same daemon. The GUI checks status on launch, while focused, and on manual refresh. Closing the window leaves the daemon running.

For normal desktop use, follow the [per-user installation guide](linux-install.md). The GUI starts the daemon when opened; login startup remains optional.

## Run without systemd

Build both binaries from the repository root:

```sh
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
./target/debug/lyteboard daemon start
./target/debug/lyteboard ping
./target/debug/lyteboard status
./target/debug/lyteboard version
./target/debug/lyteboard daemon stop
```

`daemon start` launches `lyteboardd` from the same directory as `lyteboard`. For foreground operation and startup diagnostics, run `./target/debug/lyteboardd` in another terminal. Ctrl+C and SIGTERM also shut it down. `version` queries the active daemon. `--version` prints the version of the local executable. Commands return a nonzero status on failure.

All three processes need the same `XDG_RUNTIME_DIR`. A normal Linux desktop login supplies it. In a shell without one, create a private directory with `mktemp -d` and export `XDG_RUNTIME_DIR` to that path in every terminal used for the test. Use an absolute path without `..`. The directory must be owned by the current user and have mode `0700`.

## Install the optional systemd user service

For an existing desktop installation, keep its installed binaries and omit the CLI-only install command below. Stop any manually started daemon first. The supplied unit expects `lyteboardd` at `~/.local/bin/lyteboardd`. From the repository root:

```sh
python3 scripts/install-linux.py install --cli-only
install -Dm644 contrib/systemd/lyteboardd.service \
  "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/lyteboardd.service"
systemctl --user daemon-reload
```

Use the user service manager for its lifecycle:

```sh
systemctl --user start lyteboardd.service
systemctl --user stop lyteboardd.service
systemctl --user enable --now lyteboardd.service
journalctl --user -u lyteboardd.service -f
```

Enabling the unit starts it when the systemd user manager reaches its default target. `Restart=on-failure` allows recovery from a crash and leaves the daemon stopped after a clean exit.

To remove the unit:

```sh
systemctl --user disable --now lyteboardd.service
rm "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/lyteboardd.service"
systemctl --user daemon-reload
```

## Verify the CLI and GUI together

1. Start the GUI with `npm run tauri dev` in `apps/desktop`. It should show **Daemon offline** when the daemon is stopped.
2. Start the daemon. Run `./target/debug/lyteboard status` from the repository root and click **Refresh** on the GUI’s Settings page. The version and process ID should match.
3. Run `./target/debug/lyteboard daemon stop`. The next CLI status should exit nonzero and the next GUI refresh should show **Daemon offline**.
4. Start the daemon again and refresh. Both interfaces should report the new process. Close the GUI and run `./target/debug/lyteboard ping` to confirm the daemon is still running.

## Protocol and trust boundary

The daemon binds `$XDG_RUNTIME_DIR/lyteboard/daemon.sock` with mode `0600` in a `0700` directory. Both endpoints check the peer's effective UID through Linux `SO_PEERCRED` before reading or writing IPC frames. The trust boundary is the logged-in OS user. Software running as that user can query or shut down the daemon, and an administrator can bypass user-level filesystem controls. Use a runtime directory managed by the login session or a trusted, private test directory. [Linux Unix sockets](https://man7.org/linux/man-pages/man7/unix.7.html), [XDG runtime directory](https://specifications.freedesktop.org/basedir/latest/)

An exclusive lock on `daemon.lock` guards startup and cleanup. The lock file stays in place between runs so restarting processes always lock the same inode. With the guard held, the daemon checks an existing socket by connecting. Only an owned socket that refused the connection can be removed as stale, and removal checks that the pathname still refers to the inspected inode. SIGINT, SIGTERM, and an authorized shutdown close the listener and clean up the owned socket. A crash can leave the pathname behind for the next startup to recover.

Each connection carries one request and one response. Frames start with a four-byte big-endian payload length, followed by up to 4,096 bytes of JSON. The envelope contains `version` and `body`. Version 1 provides Ping, GetStatus, GetVersion, and Shutdown. The daemon serves up to 32 concurrent authenticated requests. It closes incomplete requests after two seconds. Status reports the process version, PID, monotonic uptime, protocol version, and cumulative authenticated connections accepted, including the current request.

`lyteboard-protocol::ipc` owns the wire types. Both clients call `lyteboard-core::ipc::Client`, and `lyteboard-core::ipc::linux` owns socket handling, framing, and peer verification. The Tauri window can request status through its local capability. Windows daemon IPC and Android integration remain future work.
