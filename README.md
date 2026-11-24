# Lyteboard

Lyteboard is an open-source project for sharing text and images directly between devices on the same local network. The design uses authenticated QR pairing and Noise encryption and works without internet access. Linux is the first target, with Windows and a Kotlin Android companion planned.

The Linux desktop window and CLI use the same daemon to pair devices and share encrypted clipboard text and images. Paired devices reconnect using mDNS or a saved LAN address. The CLI also manages opt-in clipboard history and automatic synchronization. [Development state](docs/DEV_STATE.md)

## Repository layout

| Path | Purpose |
| --- | --- |
| `apps/desktop/` | React and Vite frontend with a Tauri Rust shell in `src-tauri/` |
| `apps/cli/` | `lyteboard` command-line entry point |
| `apps/daemon/` | `lyteboardd` Linux daemon |
| `crates/core/` | Shared Linux daemon client, device pairing, and encrypted peer sessions |
| `crates/protocol/` | Versioned IPC and peer protocol boundary |
| `contrib/systemd/` | Optional systemd user unit |
| `docs/` | Architecture, platform requirements, and tooling decisions |

## Build and run

Install [rustup](https://rustup.rs/) and [Node.js 24 LTS](https://nodejs.org/en/about/previous-releases). Rust is pinned in `rust-toolchain.toml`. The headless packages need only the Rust toolchain and a platform linker. Desktop development also needs Tauri's system dependencies, including GTK 3 and WebKitGTK 4.1 on Linux or the MSVC build tools and WebView2 on Windows. Use the package list for your distribution. [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)

From the repository root:

```sh
cargo build --locked
./target/debug/lyteboard daemon start
./target/debug/lyteboard status
./target/debug/lyteboard daemon stop
```

Run `./target/debug/lyteboardd` for foreground operation. See the [Linux daemon guide](docs/daemon.md) for CLI and GUI verification, the IPC trust boundary, and optional systemd installation.

With the desktop dependencies installed:

```sh
cargo build --locked --workspace
cd apps/desktop
npm ci
npm run tauri dev
```

Start the daemon from the same graphical session, then run the desktop window through Tauri. To build the native binary with the frontend embedded, run `npm run tauri build -- --no-bundle -- --locked` from `apps/desktop`. Cargo emits the release binary in the workspace's `target/release/` directory.

The Android scaffold is deferred until an Android SDK, a supported JDK/Gradle pairing, and device or emulator tooling are available. See [Platform constraints](docs/platforms.md) for the documented configuration, [Contributing](CONTRIBUTING.md) for checks, and [Architecture](docs/architecture.md) for process boundaries.

[MIT License](LICENSE)
