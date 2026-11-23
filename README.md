# Lyteboard

Lyteboard is an open-source project for sharing text and images between Linux, Windows, and Android devices. The design defaults to manual sharing with remembered QR pairing and end-to-end encryption. It calls for direct LAN transfers when available, an encrypted relay fallback, and no accounts, ads, tracking, or telemetry.

Phase 1.5 provides a persistent Linux daemon and authenticated local IPC shared by the CLI and the desktop window. Clipboard capture, pairing, synchronization, history, and WebSocket forwarding are upcoming milestones.

## Repository layout

| Path | Purpose |
| --- | --- |
| `apps/desktop/` | React and Vite frontend with a Tauri Rust shell in `src-tauri/` |
| `apps/cli/` | `lyteboard` command-line entry point |
| `apps/daemon/` | `lyteboardd` Linux daemon |
| `crates/core/` | Shared daemon client and Linux socket transport |
| `crates/protocol/` | Versioned IPC and peer protocol boundary |
| `services/relay/` | Independent Axum relay process |
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

Run `./target/debug/lyteboardd` for foreground operation. See the [Linux daemon guide](docs/daemon.md) for CLI and GUI verification, the IPC trust boundary, and optional systemd installation. To run the independent relay health scaffold, use `cargo run --locked -p lyteboard-relay` and check `http://127.0.0.1:8787/healthz`.

With the desktop dependencies installed:

```sh
cargo build --locked --workspace
cd apps/desktop
npm ci
npm run tauri dev
```

Run the desktop window through Tauri to access the daemon status command. To build the native binary with the frontend embedded, run `npm run tauri build -- --no-bundle -- --locked` from `apps/desktop`. Cargo emits the release binary in the workspace's `target/release/` directory.

The Android scaffold is deferred until an Android SDK, a supported JDK/Gradle pairing, and device or emulator tooling are available. See [Platform constraints](docs/platforms.md) for the documented configuration, [Contributing](CONTRIBUTING.md) for checks, and [Architecture](docs/architecture.md) for process boundaries.

[MIT License](LICENSE)
