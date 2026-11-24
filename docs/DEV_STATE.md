# Lyteboard development handoff

## Scope

Lyteboard will share text and images over direct Noise-encrypted TCP between paired devices on the same LAN, with offline operation, multiple peers, automatic reconnection, and local history. Keep the Rust core, Tauri + React desktop GUI, and Linux CLI and persistent daemon. Windows and a Kotlin Android companion remain future targets. QR invitations will advertise a reachable listener. mDNS and manual IP fallback come later.

## LAN-0 baseline

- Phase 1 and 1.5 desktop and CLI entry points, daemon lifecycle, same-UID Unix IPC, and 4 KiB control frames are intact.
- Phase 2B preserves checked X25519, XKpsk0 QR pairing, both devices' confirmation, in-memory trust and revocation, fresh pinned KK sessions, and encrypted record handling. Its identity format, Ed25519 peer binding, and in-memory wire fixtures remain in place. The existing WSS origin and routed envelope belong to those fixtures and the in-process state machine. Replace them when defining the LAN wire format without rotating Noise keys.
- The relay service and health endpoint, WebSocket/TLS transport, registration messages, and relay dependencies are gone. Unfinished Phase 2C CLI and IPC commands were removed, restoring the Phase 1.5 control interface.
- The unwired Linux draft in `crates/core/src/peer/storage.rs` retains private profile locking, atomic file writes, and Secret Service identity handling. The relay configuration and per-origin trust draft were removed. Durable trust and revocation are pending.
- Direct TCP, discovery, clipboard integration, and history are planned. See [architecture](architecture.md), [LAN wire proposal](protocol.md), and [security](security.md).

The Linux checkpoint passed formatting, 46 locked workspace tests, locked workspace Clippy for all targets with warnings denied, and `git diff --check`. Workspace tests ran with Unix-domain socket access for the existing IPC tests.

## Next implementation task

LAN-1 is a two-endpoint Rust TCP slice with in-memory trust, bounded frames, and an explicit LAN listener. Pair using a reachable QR address, establish a fresh KK session, and exchange acknowledged short text in both directions. Exercise wrong pins, expired and reused invitations, and malformed frames. Follow with durable trust, daemon integration, and reconnect to the saved address.
