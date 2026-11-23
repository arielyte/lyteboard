# Contributing to Lyteboard

Use the toolchains and host packages in the [README](README.md). Linux is the first development target. Keep OS-specific dependencies at the platform boundary and keep the headless Rust build independent of the desktop libraries.

## Checks

From the repository root on a headless machine:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

With the native desktop dependencies installed, check the whole workspace:

```sh
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --workspace
```

From `apps/desktop`:

```sh
npm ci
npm run format:check
npm run lint
npm run build
npm run tauri build -- --debug --no-bundle -- --locked
```

To fix formatting, use `cargo fmt --all` and `npm run format` from their respective directories. Keep `Cargo.lock` and `apps/desktop/package-lock.json` in sync with intentional dependency changes. CI checks Linux and Windows builds and compiles the desktop app with the embedded frontend. Android checks will follow the Android scaffold.

The Linux IPC tests use isolated temporary runtime directories and real local sockets. Run them with `cargo test --locked -p lyteboard-protocol -p lyteboard-core -p lyteboard-daemon`. See the [daemon guide](docs/daemon.md#verify-the-cli-and-gui-together) for the desktop lifecycle smoke test.

Use synthetic clipboard data in tests and bug reports. Keep clipboard contents, pairing tokens, private keys, and local databases out of logs and Git. See the [architecture notes](docs/architecture.md) before proposing changes to process ownership, IPC, or transport.
