# Lyteboard development handoff

Lyteboard is a LAN-only clipboard-sharing project with a shared Rust core, Tauri + React desktop GUI, and Linux CLI and daemon. Text and images, multiple peers, automatic reconnection, and local history remain the product scope. Windows and a Kotlin Android companion are future targets.

## LAN-1 checkpoint

- Phase 1 and 1.5 desktop and CLI behavior, daemon lifecycle, authenticated Unix IPC, and the 4 KiB control frame contract are intact. The Phase 2B in-process and cryptographic regression fixtures are preserved.
- `lyteboard_core::peer::lan` opens direct Tokio TCP connections with bounded length-prefixed frames. Listeners default to loopback and accept an explicit local address. A LAN QR contains a dial address and port, inviter Noise pin, expiring single-use invitation ID, and PSK. Both endpoints must compare the Noise transcript code and explicitly approve.
- Each data connection uses a fresh KK handshake and encrypted readiness. Both sides can send UTF-8 text up to 16 KiB and acknowledge it by transfer ID. Pins and revocations live in memory. Authentication, framing, and I/O errors drop the affected connection and Noise state. [LAN wire protocol](protocol.md), [Security](security.md)

## Demonstration

From the repository root, run the two-endpoint loopback harness or all focused LAN tests:

```sh
cargo test --locked -p lyteboard-core --test lan_tcp two_endpoints_pair_and_exchange_text -- --exact --nocapture
cargo test --locked -p lyteboard-core --test lan_tcp
```

The harness uses independently generated identities and two real TCP connections for pairing and text. The focused tests require loopback socket access. The workspace regressions also require Unix-domain socket access on Linux.

## Verification

All nine focused LAN tests passed. They cover pairing and bidirectional text, invalid and reused credentials, mismatched approval codes, substituted peers, tampering and plaintext injection, revocation, frame and text limits, timeouts, and disconnects. The final `cargo test --locked --workspace` passed all 55 tests on Linux. Workspace formatting, Clippy for the core and protocol with warnings denied, and `git diff --check` passed.

## Remaining work

The harness runs on one host. Explicit interface binding relies on host routing and firewall policy for LAN reachability. The caller drives accept, message handling, acknowledgment, and reconnect. Revocation blocks subsequent use of a live channel, and an idle receive call times out after five seconds. LAN-2 covers durable identity and trust, daemon integration, and automatic reconnect. Discovery, manual IP fallback, clipboard integration, image transfers, and history remain planned.
