# Lyteboard development handoff

Lyteboard is a LAN-only clipboard-sharing project with a shared Rust core, Tauri + React desktop GUI, and Linux CLI and daemon. Text and images, multiple peers, automatic reconnection, and local history remain the product scope. Windows and a Kotlin Android companion are future targets.

## LAN-1 checkpoint

- Phase 1 and 1.5 desktop and CLI behavior, daemon lifecycle, authenticated Unix IPC, and the 4 KiB control frame contract are intact. The Phase 2B in-process and cryptographic regression fixtures are preserved.
- `lyteboard_core::peer::lan` opens direct Tokio TCP connections with bounded length-prefixed frames. Listeners default to loopback and accept an explicit local address. A LAN QR contains a dial address and port, inviter Noise pin, expiring single-use invitation ID, and PSK. Both endpoints must compare the Noise transcript code and explicitly approve.
- Each data connection uses a fresh KK handshake and encrypted readiness. Both sides can send UTF-8 text up to 16 KiB and acknowledge it by transfer ID. Authentication, framing, and I/O errors drop the affected connection and Noise state. [LAN wire protocol](protocol.md), [Security](security.md)

## LAN-2A checkpoint

On Linux, `lyteboard_core::peer::storage::open_endpoint` opens a durable LAN endpoint from an absolute profile path. Call it on a blocking thread. It loads or provisions the existing two-seed identity through the session's unlocked, persistent Secret Service collection. Locked, unavailable, and missing credentials return errors. The profile stores only public identity metadata and a versioned trust snapshot in private files. Profile locking and synced atomic replacement protect updates. Pairing commits and revocations are saved before success is reported.

Trusted-device records retain the Noise pin, display name, revocation state, and a last-known address when available. The scanner records the QR dial hint and refreshes it after a successful outbound KK session. The responder has no advertised scanner listener address in the current protocol. A caller can supply a hint for an already trusted peer with `Endpoint::remember_address`. Address changes leave the authenticated pin intact.

## Demonstration

From the repository root, run the two-endpoint loopback harness or all focused LAN tests:

```sh
cargo test --locked -p lyteboard-core --test lan_tcp two_endpoints_pair_and_exchange_text -- --exact --nocapture
cargo test --locked -p lyteboard-core --test lan_tcp
```

The harness uses independently generated identities and two real TCP connections for pairing and text. The focused tests require loopback socket access. The workspace regressions also require Unix-domain socket access on Linux.

## Verification

Run the storage regressions and the workspace checkpoint from the repository root:

```sh
cargo test --locked -p lyteboard-core --lib peer::storage
cargo test --locked --workspace
```

The storage tests use private temporary profiles and ephemeral credentials. They cover identity reload, multiple peers, saved dial hints, durable revocation, invalid and missing files, interrupted writes, and a new TCP session after both endpoints reopen their profiles. An isolated GNOME Keyring smoke test also verified identity reload by two processes and explicit rejection when the collection was locked.

On Linux, all 7 focused storage tests and all 62 workspace tests passed. Workspace formatting, core and protocol Clippy with warnings denied, and `git diff --check` passed.

## Remaining work

Persistence is Linux-only. Trust writes are synchronous under the peer-state lock. The profile path must remain stable for credential lookup. The profile detects missing trust data when its identity anchor exists. Restoring an older valid snapshot can also restore its older trust state.

The harness runs on one host. The caller still drives networking and reconnect. Daemon integration, automatic reconnection, discovery, clipboard integration, image transfers, and history remain pending.
