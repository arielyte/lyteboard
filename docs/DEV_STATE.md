# Lyteboard development handoff

Lyteboard is a LAN-only clipboard-sharing project with a shared Rust core, Tauri + React desktop GUI, and native Linux/Windows CLI and daemon. The Kotlin Android companion pairs with Linux and shares foreground clipboard content, text, and images. Windows has the LAN-7A platform foundation; native Windows runtime acceptance remains pending.

## LAN-1 checkpoint

- Phase 1 and 1.5 desktop and CLI behavior, daemon lifecycle, authenticated Unix IPC, and the 4 KiB control frame contract are intact. The Phase 2B in-process and cryptographic regression fixtures are preserved.
- `lyteboard_core::peer::lan` opens direct Tokio TCP connections with bounded length-prefixed frames. Listeners default to loopback and accept an explicit local address. A LAN QR contains a dial address and port, inviter Noise pin, expiring single-use invitation ID, and PSK. Both endpoints must compare the Noise transcript code and explicitly approve.
- Each data connection uses a fresh KK handshake and encrypted readiness. Both sides can send UTF-8 text up to 16 KiB and acknowledge it by transfer ID. Authentication, framing, and I/O errors drop the affected connection and Noise state. [LAN wire protocol](protocol.md), [Security](security.md)

## LAN-2A checkpoint

On Linux, `lyteboard_core::peer::storage::open_endpoint` opens a durable LAN endpoint from an absolute profile path. Call it on a blocking thread. It loads or provisions the existing two-seed identity through the session's unlocked, persistent Secret Service collection. Locked, unavailable, and missing credentials return errors. The profile stores only public identity metadata and a versioned trust snapshot in private files. Profile locking and synced atomic replacement protect updates. Pairing commits and revocations are saved before success is reported.

Trusted-device records retain the Noise pin, display name, revocation state, and a last-known address when available. The scanner records the QR dial hint and refreshes it after a successful outbound KK session. The responder has no advertised scanner listener address in the current protocol. A caller can supply a hint for an already trusted peer with `Endpoint::remember_address`. Address changes leave the authenticated pin intact.

## LAN-2B checkpoint

The Linux daemon owns the persistent peer profile, listener, pending approval, and authenticated sessions. LAN work runs on a separate worker from same-UID Unix IPC. Existing daemon lifecycle commands and the 4 KiB IPC limit still apply. The CLI can configure the listener, pair, display or compare codes, list or rename devices, send or view short test messages, and revoke a device. Successful sends wait for a matching encrypted acknowledgment. Revocation persists a tombstone and closes the affected sessions.

The first network command opens a new profile. Existing profiles load on startup. Lyteboard binds to `127.0.0.1:0` by default. `network listen IP:PORT` changes the active listener for that process. Set `LYTEBOARD_LISTEN_ADDR` before starting the daemon to retain a fixed port across restarts. Use an absolute `LYTEBOARD_PROFILE` and a separate private `XDG_RUNTIME_DIR` for each instance. Without the profile override, the daemon uses `$XDG_DATA_HOME/lyteboard` or `$HOME/.local/share/lyteboard`.

## LAN-3 checkpoint

The Linux daemon uses mDNS to advertise `_lyteboard._tcp.local.` with its listener IP and port, wire version, and public Noise key. Ads carry no device labels, trusted-device lists, private keys, or pairing secrets. The daemon advertises on the listener's interface and browses interfaces on that subnet. It treats every resolved address as an unverified hint and only dials on-link addresses for an active local pin. A peer becomes online after Noise KK and encrypted readiness. [mdns-sd documentation](https://docs.rs/mdns-sd/0.22.0/mdns_sd/)

One daemon scheduler reconnects paired peers with saved or discovered addresses. It permits four outbound handshakes at once, retries with jittered exponential delays between 0.5 and 60 seconds, and rotates address candidates after failures. If both peers dial at once, confirmed competing sockets converge on the connection initiated by the lower public key. Failed or unacknowledged messages are not resent automatically.

## LAN-4A checkpoint

The Linux CLI can read the current text clipboard, send one snapshot to selected paired devices, and copy received text into the local clipboard. On Wayland it uses `wl-paste` and `wl-copy` from `wl-clipboard` 2.3 or newer. KDE Plasma works with KWin's data-control support. On X11 it uses `xclip` and the `CLIPBOARD` selection. Explicit operations use the CLI's graphical session. Auto-apply uses the daemon's graphical environment, starts off, and resets at daemon restart.

LAN-4A used inline authenticated delivery and the 4 KiB IPC framing, with a 512-byte text limit superseded by LAN-4B. Empty, non-text, oversized, and invalid text are rejected with errors. The clipboard is only read for an explicit send. Applying a received message does not trigger any outbound action. A slow automatic writer coalesces pending writes to the latest text. Clipboard commands and helper errors do not log payloads.

## LAN-4B checkpoint

The Linux CLI shares up to 1 MiB of UTF-8 text and up to 10 MiB of static PNG, JPEG, or WebP. Clipboard sends choose a supported image target when present. At LAN-4B the copy command published the received MIME type and auto-apply supported text. See LAN-6E for current clipboard image formats.

Bulk records travel in independently encrypted Noise sessions with a random ID per recipient, 32 KiB chunks, authenticated credits, SHA-256 verification, and an authenticated acknowledgment after validation and acceptance. Image validation checks the declared format, container length or end marker, decoder output, and dimensions. Images are limited to 8,192 pixels on either axis, 16 million pixels total, and 64 MiB decoded output. [Image decoder documentation](https://docs.rs/image/0.25.10/image/codecs/)

The daemon allows two incomplete inbound transfers, two outgoing batches that share their payloads across selected recipients, one validation task, and two authenticated local bulk streams. Local binary streams use a 20-second deadline after the bounded control request. Peer transfers use a 10-second progress timeout and a 60-second total deadline. The existing receive buffer holds at most 16 messages or 20 MiB in memory. Partial data is discarded when a transfer fails. IPC control frames remain limited to 4 KiB. [Wire protocol](protocol.md)

## LAN-4C checkpoint

The Linux daemon can save validated incoming clipboard text and images and outgoing content after an authenticated delivery acknowledgment. Each entry records a Unix timestamp in seconds, MIME type, byte count, direction, and the remote device's public Noise ID. A send to multiple devices saves one entry for each acknowledgment. Restoring an item writes the local clipboard. Resending uses the regular encrypted delivery path and requires current trust. Neither feature watches the clipboard.

History is off by default. Set `LYTEBOARD_HISTORY=on` in the daemon's environment before starting it. Existing daemons need a restart to pick up changes. Optional configuration uses the following positive integers.

| Variable | Default | Supported range |
| --- | --- | --- |
| `LYTEBOARD_HISTORY_ENTRIES` | 100 entries | 1–10,000 |
| `LYTEBOARD_HISTORY_MIB` | 100 MiB | 1–1,024 MiB |
| `LYTEBOARD_HISTORY_DAYS` | 7 days | 1–3,650 days |

The database lives at `history/history.sqlite3` under the daemon profile. The history directory is mode `0700` and the database and lock are `0600`. The daemon validates ownership and file types, uses a dedicated SQLite worker, and commits each record and its retention changes together. Oldest entries are removed first. Expiration runs at startup, on history access and insertion, and about once per minute while enabled. The database size limit includes SQLite pages after commit. A temporary rollback journal can use additional space. An item too large for the configured quota is delivered but omitted from history. Disabling history closes access and pauses cleanup without erasing previous entries.

**SQLite history is unencrypted at rest**, including clipboard bytes and metadata. Private permissions restrict access by other users. Processes running as the same user, administrators, unlocked disks, and backups can expose the database. Deletion and clearing do not guarantee erasure from snapshots, backups, or underlying media. Disk encryption can protect files when its volume is locked. History storage errors leave pairing and clipboard transfer available. `history status` reports an unavailable store, and a stalled or failed write can leave an item absent from history. The daemon leaves damaged databases in place. [SQLite journal and deletion settings](https://www.sqlite.org/pragma.html)

The history list contains metadata only and returns at most 16 entries per page. Restoring uses the existing bounded Unix bulk stream. Resending reads one bounded payload in the daemon and uses the existing transfer queue. All IPC control frames remain limited to 4 KiB.

### History demonstration

Run these commands on an already paired Linux device. Replace `"$peer_id"` with a current device ID from `lyteboard devices list`. Use IDs from `history list` in place of `1` and `2` if the profile already contains history.

```bash
lyteboard daemon stop
LYTEBOARD_HISTORY=on lyteboard daemon start
lyteboard history status
lyteboard send "$peer_id" 'History demonstration'
wl-copy --type image/png --sensitive < sample.png
lyteboard clipboard send "$peer_id"
lyteboard history list
lyteboard history restore 2
wl-paste --no-newline --type image/png | cmp - sample.png
lyteboard history resend 1 "$peer_id"
lyteboard history delete 1
# Voluntarily remove all entries in this demonstration profile.
lyteboard history clear --yes
```

If `history list` shows a next-page ID, pass it to `lyteboard history list --before ID`. On X11 use `xclip -selection clipboard -in -target image/png -silent < sample.png` to seed the test image and `xclip -selection clipboard -out -target image/png | cmp - sample.png` to verify restoration.

```sh
cargo build --locked -p lyteboard-daemon -p lyteboard-cli
cargo test --locked -p lyteboard-daemon --bin lyteboardd history::
cargo test --locked -p lyteboard-protocol history_metadata_and_max_recipient_request_fit_control_frames
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/history_cli.py
cargo test --locked --workspace
```

The history harness passed using three daemon profiles, real restarts, Unix IPC, Noise, and TCP with simulated clipboard adapters. It verified opt-in behavior, text and image restoration, multi-recipient metadata, resend and revocation, deletion and clearing, pagination, and transfers with an unavailable history database. The Rust tests passed for count, byte and age retention, permissions, corrupt storage, and the 4 KiB control-frame contract. All 78 workspace tests passed, along with the existing simulated clipboard and LAN CLI harnesses, focused Clippy with warnings denied, formatting, and `git diff --check`.

## LAN-4D checkpoint

Linux clipboard sending is manual by default. `clipboard sync` enables automatic sending of new clipboard text and static PNG, JPEG, or WebP to selected trusted devices. `clipboard auto` independently controls automatic application of received text and images. Both settings and the recipient list reset when the daemon restarts. Stopping the sender preserves its selections until restart. Removing the last recipient stops monitoring.

Wayland monitoring uses `ext-data-control` or `wlr-data-control` through `wl-clipboard-rs`. Current KDE Plasma supports background monitoring through KWin. Lyteboard reads the specific offer that generated the event and skips the initial selection. X11 uses XFixes `CLIPBOARD` owner events through `x11rb` and uses `xclip` to read and write selections. Both backends block for events with no periodic clipboard reads. Manual clipboard commands still use `wl-clipboard` 2.3+ on Wayland and `xclip` on X11. If monitoring is unavailable, the daemon and the manual clipboard commands remain usable wherever the manual adapter has access. [Wayland watcher](https://docs.rs/wl-clipboard-rs/0.9.5/wl_clipboard_rs/watch/struct.Watcher.html), [XFixes selection notifications](https://docs.rs/x11rb/0.14.0/x11rb/protocol/xfixes/trait.ConnectionExt.html)

The sender coalesces bursts with a 200 ms quiet period, ignores repeated content, and skips recent Lyteboard clipboard writes using a bounded fingerprint cache. Receive-side application, `clipboard copy`, and `history restore` suppress echoes. A recipient must have had a confirmed Noise session when the change was observed. Busy and offline peers miss that update, and expired updates are dropped. Reconnection never triggers a clipboard read or a resend. Transfers already in progress can finish when automatic sending is turned off. Revocation closes the affected session. Existing bulk limits, encryption, acknowledgments, and optional history still apply. IPC control frames remain limited to 4 KiB.

Offers advertising the KDE `x-kde-passwordManagerHint` target are skipped for automatic sending, including Wayland selections created with `wl-copy --sensitive`. **Password detection is not guaranteed.** Some applications and bridges omit the hint. Automatic sending should be turned off if the session is used to copy unmarked secrets. X11 reports selection-owner events, so a clipboard owner that mutates content without publishing another selection can go undetected. The bounded echo cache can temporarily suppress a deliberate recopy of the same content. Automatic sending is best-effort and can omit rapidly replaced content. Native Xorg remains unverified.

### Automatic clipboard demonstration

Start both daemons inside their graphical sessions and pair them. Enable automatic application on the receiver and then copy a test value on the sender.

```bash
# Receiver
lyteboard clipboard auto on

# Sender
lyteboard devices list
read -r -p 'Paired recipient ID: ' peer_id
lyteboard clipboard sync add "$peer_id"
lyteboard clipboard sync on
lyteboard clipboard sync status
printf 'Automatic clipboard test ✓' | wl-copy --type 'text/plain;charset=utf-8'
```

On the receiver, compare the copied text. Then send the image from the sender and compare its hash on each device.

```bash
# Receiver
wl-paste --no-newline --type text | cmp - <(printf 'Automatic clipboard test ✓')

# Sender
wl-copy --type image/png < sample.png
sha256sum sample.png

# Receiver
wl-paste --no-newline --type image/png | sha256sum

# Sender controls
lyteboard clipboard sync off
lyteboard clipboard sync remove "$peer_id"

# Receiver control
lyteboard clipboard auto off
```

On X11, seed the test value with `printf 'Automatic clipboard test ✓' | xclip -selection clipboard -in -target UTF8_STRING -silent` and the image with `xclip -selection clipboard -in -target image/png -silent < sample.png`. Read it back with `xclip -selection clipboard -out -target image/png | sha256sum`. To demonstrate filtering on Wayland, seed a disposable value with `printf 'Sensitive test' | wl-copy --sensitive` while automatic sending is enabled.

```sh
cargo build --locked -p lyteboard-daemon -p lyteboard-cli
cargo test --locked -p lyteboard-core --lib clipboard::
cargo test --locked -p lyteboard-daemon --bin lyteboardd network::sync::
cargo test --locked -p lyteboard-protocol network_payloads_fit_existing_frames_and_debug_is_redacted
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_sync_cli.py
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_sync_cli.py --x11
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_sync_cli.py --simulated
cargo test --locked --workspace
```

The graphical harness passed with three real nested KWin Wayland sessions and with two devices switched to real nested Xwayland selections. These tests exercised live change notifications, text and PNG transfers, three-device echo prevention, repeat suppression, sensitive Wayland offers, offline and revoked peers, daemon restart, and display loss. They used synthetic clipboard data in isolated sessions. The simulated mode verified manual fallback with fake clipboard utilities when monitoring could not start. All 81 workspace tests passed, along with the existing simulated clipboard and history harnesses, focused Clippy with warnings denied, formatting, and `git diff --check`.

## LAN-5A checkpoint

The Linux Tauri window uses the same authenticated Unix IPC client as the CLI. Its Devices view shows the daemon and each peer’s connection state. Users can send the clipboard to one online peer or a selection, with per-peer errors and success based on authenticated delivery acknowledgments. Clipboard bytes remain in Rust. The daemon keeps running when the window closes.

Pair device supports a locally rendered QR, optional invitation text, paste import, and explicit comparison-code confirmation. Invitation data lives in the open pairing dialog and is cleared on expiry, pairing, or dismissal. Avoid sharing the invitation with unintended devices. The GUI does not write it to the clipboard. If copying the revealed text manually, first pause automatic clipboard sending. [Tauri command capabilities](https://v2.tauri.app/security/capabilities/)

Discover shows live mDNS address hints. The GUI can save a discovered address for a paired device, or accept a manual IP and port from Manage. Settings exposes the daemon listener and discovery toggle, alongside System, Light, and Dark appearance. Device removal requires confirmation and revokes trust. Listener and discovery changes follow the daemon’s existing per-process lifetime. The History page points to the CLI. Full history management, image previews, camera-based QR import, automatic synchronization settings, tray support, and startup integration remain pending.

From the repository root on Linux, build and open the window in the graphical session that runs the daemon.

```sh
cargo build --locked -p lyteboard-daemon -p lyteboard-cli
npm --prefix apps/desktop run lint
npm --prefix apps/desktop run format:check
npm --prefix apps/desktop run build
npm --prefix apps/desktop run tauri build -- --no-bundle --ci -- --locked
cargo test --locked -p lyteboard-desktop -p lyteboard-core -p lyteboard-daemon -p lyteboard-protocol
target/debug/lyteboard daemon start
target/release/lyteboard-desktop
```

For a peer on another computer, enter this device’s on-link IP and port under **Settings → Local network** before generating an invitation. Pair and compare the verification codes on both devices. In Devices, use **Send** beside a peer or select multiple online peers. If the daemon is offline, start it from the same graphical session and refresh.

The native test uses `tauri-driver`, `WebKitWebDriver`, nested KWin, GNOME Keyring, and the built CLI and daemon executables. The optional `zbarimg` tool decodes the displayed QR. With `--lan-ip`, it checks actual on-host multicast discovery on the selected on-link interface. Use `--driver` and `--webkit-driver` for binaries outside `PATH`.

```bash
PYTHONDONTWRITEBYTECODE=1 python3 apps/desktop/tests/desktop_gui.py
read -r -p 'Local interface IPv4 address: ' lan_ip
PYTHONDONTWRITEBYTECODE=1 python3 apps/desktop/tests/desktop_gui.py --lan-ip "$lan_ip"
```

The locked Tauri release build, TypeScript, lint, formatting, desktop Clippy, and all 81 affected Rust tests passed. The native WebKit window passed against three isolated daemons in KWin Wayland. Verification covered offline recovery, both themes, QR decoding and cancellation, invitation import and comparison, text and PNG sends, transfer errors, actual on-host mDNS, listener and peer address changes, renaming and revocation, and daemon survival after window close. Verification used one Linux host.

## LAN-5B checkpoint

History shows real sent and received entries, local timestamps, content types, and device names where the peer is still paired. It pages through 16 entries at a time, searches saved text, and supports restore, resend, delete, and confirmed clear. Search matches literal substrings, folds ASCII case, and matches other characters exactly. Disabled history and unavailable storage have separate states. History refreshes on navigation or with **Refresh history**.

Visible previews load on demand through the authenticated Unix bulk stream. Text previews contain at most 160 characters. PNG, JPEG, and WebP previews are decoded under the transfer validator’s existing limits, resized to at most 128 pixels per axis, and returned as PNGs. The desktop receives thumbnails in memory without a filesystem grant. Restore and resend use the full saved payload. IPC control frames remain capped at 4 KiB.

The daemon saves explicit clipboard, history retention, discovery, and listener preferences in the profile’s private `preferences.json` using atomic replacement. Automatic sending, automatic application, and persistent history default off. Turning off history preserves the database, and lowering active retention can remove entries immediately. Automatic sending only resumes for still-trusted selected recipients and skips the clipboard already present at startup. An unavailable graphical session leaves the saved preference visible with an inactive status. The appearance preference belongs to the desktop. An explicit `LYTEBOARD_LISTEN_ADDR` environment setting takes precedence at startup. History and discovery environment values are fallbacks until saved settings exist.

Settings explains end-to-end encryption, LAN-only communication, unencrypted local history, and the possibility of sharing unmarked sensitive clipboard content. The CLI updates the same daemon preferences.

```sh
lyteboard history enable
lyteboard history retention --entries 100 --mib 100 --days 7
lyteboard history list --search 'example'
lyteboard history disable
lyteboard clipboard sync status
lyteboard clipboard auto status
```

Verification from the repository root, with the graphical test prerequisites listed under LAN-5A:

```sh
cargo build --locked -p lyteboard-daemon -p lyteboard-cli
npm --prefix apps/desktop run format:check
npm --prefix apps/desktop run lint
npm --prefix apps/desktop run typecheck
npm --prefix apps/desktop run build
npm --prefix apps/desktop run tauri build -- --no-bundle --ci -- --locked
cargo test --locked -p lyteboard-core -p lyteboard-protocol -p lyteboard-daemon -p lyteboard-cli -p lyteboard-desktop
PYTHONDONTWRITEBYTECODE=1 python3 apps/desktop/tests/desktop_gui.py
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/history_cli.py
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_sync_cli.py
```

All 84 affected Rust tests passed, along with Clippy, frontend formatting, lint, typecheck, the frontend build, and the locked Tauri release build. The native WebKit harness passed in isolated KWin Wayland sessions against three real daemons, including text and image restoration, thumbnails, search, pagination, resend, delete, clear confirmation, disabled history, corrupt storage, and preference recovery after an offline restart. The history, clipboard, LAN, and real KWin synchronization CLI harnesses also passed.

Full history items remain available for copying or resending. The GUI shows previews only. History search covers text payloads and does not index images. Stored passwords and other sensitive clipboard content can be readable by same-user processes and backups. System tray and automatic daemon startup remain deferred.

## LAN-5C checkpoint

The desktop can start and stop the daemon from Settings. The offline view also offers **Start daemon**. Both desktop and CLI start the installed sibling `lyteboardd` through the same launcher. A private launcher lock serializes concurrent starts, and the daemon retains its own socket lock and same-user authentication. Startup errors identify missing binaries or readiness failures. A locked Secret Service leaves the daemon reachable and shows the keyring error in the LAN controls.

The native KDE StatusNotifier tray can open Lyteboard, send the clipboard to the selection on the Devices screen, pause or resume both automatic clipboard directions, show connection status, quit the GUI, or explicitly stop the daemon and quit. Pausing preserves recipient and direction preferences across daemon restarts. The Settings screen can resume synchronization if the tray becomes unavailable. A second GUI launch activates the existing window for that runtime profile. Closing or quitting the GUI leaves the daemon running. [StatusNotifier tray library](https://docs.rs/ksni/0.3.7/ksni/)

**Start daemon when Lyteboard opens**, **Open Lyteboard at login**, **Close to tray**, and **Minimize to tray** are opt-in. Desktop preferences live in the private Lyteboard configuration directory. Login startup uses a user XDG autostart desktop entry. The app installs no system services. In sessions without a usable tray, the window closes normally. Native minimize-to-tray follows GTK iconification notifications where supplied. KDE Wayland exposes an explicit **Minimize to tray** toolbar button when enabled, since compositor minimize notifications may be unavailable to the client.

The foreground device view refreshes on focus and at five-second intervals while focused and visible. Desktop actions emit local events for immediate refresh, and pairing status is polled only while the pairing window is active. The tray refreshes its status when the menu is used and otherwise checks every 15 seconds. Hidden GUI views suspend foreground polling.

### Install and remove on Linux

Use the [local Linux installation guide](linux-install.md) and the LAN-6F installer below. It supersedes the manual binary-copy instructions from this checkpoint.

### Verification

```sh
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
cargo build --locked --release -p lyteboard-cli -p lyteboard-daemon
npm --prefix apps/desktop run format:check
npm --prefix apps/desktop run lint
npm --prefix apps/desktop run typecheck
npm --prefix apps/desktop run tauri build -- --no-bundle --ci -- --locked
desktop-file-validate contrib/desktop/io.github.arielyte.lyteboard.desktop
cargo test --locked -p lyteboard-core -p lyteboard-protocol -p lyteboard-daemon -p lyteboard-cli -p lyteboard-desktop
PYTHONDONTWRITEBYTECODE=1 python3 apps/desktop/tests/desktop_gui.py
PYTHONDONTWRITEBYTECODE=1 python3 apps/desktop/tests/desktop_tray.py
```

Use the WebDriver prerequisites from LAN-5A. The tray harness also needs Python GObject bindings and an active Plasma tray host on the current session bus. It controls the real tray menu over D-Bus, while the test window and clipboard use isolated KWin sessions. The GUI harness also tests the session-without-tray fallback. The login toggle creates and removes the native entry. Verification does not require logging out of the current desktop session.

Both native harnesses passed. Verification covered starting, stopping and reopening the GUI and daemon, four concurrent start requests, missing binaries, a locked keyring, live peer status, existing pairing, history, settings and transfers, the desktop launcher, and all tray actions. The login entry was created and removed in a temporary configuration. A full login cycle remains unverified. Frontend formatting, lint, typecheck, build, the locked Tauri build, desktop-file validation, Clippy, all 85 affected Rust tests, and the LAN and history CLI harnesses passed.

## LAN-6A checkpoint

`apps/android` is a native Compose app for arm64-v8a on API 26 and later. Gradle cross-compiles the workspace bridge with NDK 28.2.13676358 and generates Kotlin bindings from the resulting library's UniFFI metadata. The single typed call, `coreInfo()`, returns the Rust core version and the existing LAN peer wire version. The Android target excludes Linux Unix IPC, Secret Service, and clipboard backends. Pairing, networking, clipboard access, and background services remain outside this Android checkpoint.

The CachyOS development host initially had JDK 27 and Rust 1.99.1 for Linux. The SDK, NDK, Gradle, ADB, and Rust Android target were absent. Verification used a temporary JDK 21 and Android SDK, the checked-in Gradle wrapper, and the per-user `aarch64-linux-android` Rust target. See the setup guide for exact installation, APK, and ADB commands. [Android development guide](../apps/android/README.md)

```sh
cargo check --locked -p lyteboard-android --target aarch64-linux-android
cargo tree --locked -p lyteboard-android --target aarch64-linux-android
apps/android/gradlew -p apps/android :app:assembleDebug :app:lintDebug
cargo fmt --all -- --check
cargo clippy --locked -p lyteboard-android -p lyteboard-protocol --all-targets --features lyteboard-android/bindgen -- -D warnings
cargo test --locked --workspace
git diff --check
```

The arm64 Rust check and focused Clippy passed. The Android dependency tree excluded Secret Service, Wayland clipboard, and X11. All 85 Linux workspace tests passed with the real filesystem and loopback access needed by the existing IPC tests.

The debug APK built and passed `zipalign -c -P 16 4`. The packaged Rust and JNA ELF segments also use 16 KiB alignment. Android lint reported zero errors and eight warnings covering dependency updates, the arm64-only target, launcher icon, and backup configuration. A standalone Kotlin/JVM test loaded the host build of the same UniFFI bridge. On an Android 16 Google APIs emulator, ADB forced the arm64-v8a APK and reported `primaryCpuAbi=arm64-v8a`. The launched Compose screen displayed `Rust core 0.1.0` and `LAN protocol 1` through the ARM translation layer. No physical Android device was connected.

## LAN-6B checkpoint

Android imports existing Linux invitations and requires both devices to approve the displayed verification code. The UniFFI bridge uses the shared Rust pairing, Noise KK, TCP, payload validation, and bulk sender and receiver. Android initiates sessions and both peers can send through them. The Compose test screen supports multiple paired devices, numeric IP and port entry, text sending, system file-picker image sending, received text preview, explicit saving, and revocation. Transfers retain the shared 1 MiB UTF-8 and 10 MiB PNG, JPEG, or WebP limits. The app keeps the latest received item in memory.

The private identity is sealed with a non-exportable Android Keystore AES-GCM key. The Rust credential adapter passes seed buffers only through private JNI calls for storage. UniFFI exposes public IDs and operations. The versioned trust snapshot and revocation tombstones use the existing atomic storage implementation. Both files are under Android's app-private no-backup directory. Missing or unusable keys fail profile open and require restoring the original profile or explicitly resetting the app data.

The Android activity closes sessions and cancels pairing and discovery when it stops. While visible, the app reconnects with a delay capped at 30 seconds. It accepts Android NSD results for previously trusted pins as hints and requires the destination to be on a local interface subnet. An authenticated connection updates the saved address. There is no Android listener or advertisement, so Linux can send after Android connects. Android clipboard access, background services, Android-to-Android pairing, and scoped IPv6 link-local addresses remain outside this checkpoint. [Android setup and manual Linux commands](../apps/android/README.md)

```sh
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
apps/android/gradlew -p apps/android :app:assembleDebug :app:assembleDebugAndroidTest :app:lintDebug
PYTHONDONTWRITEBYTECODE=1 python3 apps/android/tests/lan_emulator.py emulator-5562
cargo clippy --locked -p lyteboard-android --target aarch64-linux-android -- -D warnings
cargo test --locked --workspace
git diff --check
```

The test used an Android 16 Google APIs emulator running the arm64-v8a APK through Android's ARM native bridge and two separate Linux daemons. Linux listeners used host loopback and the harness mapped the invitation IP to the emulator's `10.0.2.2` host alias. The host-side clipboard adapter was file-backed for byte comparisons. The LAN-6C checkpoint records the later physical Samsung result.

The emulator test passed pairing with both Linux peers, bidirectional chunked Unicode and PNG/JPEG/WebP with integrity checks, a manual listener-port change, identity and trust across process restarts, foreground disconnect and reconnect, and persistent revocation. The app and instrumentation APKs assembled successfully. The app APK passed 16 KiB ZIP alignment and Android lint reported zero errors and seven advisory warnings. All 85 Rust workspace tests passed, along with focused Clippy, formatting, and diff checks.

## LAN-6C checkpoint

Android has Devices, History, and Settings screens using the desktop palette and Noto Sans. Pairing starts with a native CameraX QR scan or a pasted Linux invitation. Both devices still compare and approve the Noise transcript code. On Android, **Send clipboard**, **Write text**, **Choose image**, and the system **Share → Lyteboard** action open a recipient picker. Transfers to multiple recipients use individually authenticated sessions and report confirmed deliveries. Received text and images support preview, copy, save, and resend. Text remains limited to 1 MiB and PNG, JPEG, and WebP to 10 MiB.

Local history is opt-in and stored in private SQLite outside Android backup. It records successful sent and received content. Defaults are 100 entries, 100 MiB of payload data, and 7 days. Retention is configurable, and the app supports single-item deletion and confirmed clearing. Saved history is unencrypted at rest. Turning history off pauses recording. Android clipboard reads are foreground and user-initiated, and the app cancels sends and closes sessions when the activity stops.

Android 14 and later subscribe to changes in resolved NSD services during foreground discovery. Earlier releases use Android's one-shot resolver as services appear. Resolved addresses wake reconnect attempts for trusted devices. Local Wi-Fi or Ethernet address changes invalidate sessions and stale discovery hints. Manual addresses are available in Settings. Rust still checks the paired Noise pin on every connection and retains the same LAN wire format and Keystore-protected identity.

The user verified physical Samsung-to-Linux pairing and text sharing over Wi-Fi. The initial timeout cleared when UFW allowed the actual Linux TCP listener port. Use `lyteboard network status`, confirm the listener is on a LAN IP, and limit the UFW rule to the actual port and local subnet. Check Wi-Fi client isolation if both devices still cannot connect. [Android pairing, firewall commands, and test instructions](../apps/android/README.md)

```sh
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
apps/android/gradlew -p apps/android :app:assembleDebug :app:assembleDebugAndroidTest :app:lintDebug
PYTHONDONTWRITEBYTECODE=1 python3 apps/android/tests/lan_emulator.py emulator-5562
cargo clippy --locked -p lyteboard-android --target aarch64-linux-android -- -D warnings
cargo test --locked --workspace
cargo fmt --all -- --check
git diff --check
```

The Android 16 emulator resolved a locally registered Android NSD advertisement. The cross-platform harness verified reconnecting to a trusted Linux daemon on a changed port using a new hint, foreground shutdown and reconnect, and persistent revocation. It also verified bidirectional Unicode and PNG/JPEG/WebP with integrity checks, an Android text share intent, and copying received text to the Android clipboard and sharing it to two Linux devices. Focused Android tests covered QR decoding and history storage, including age, item count, byte retention, reopening, and deletion. Android build, lint, and 16 KiB APK alignment passed. Lint reported zero errors and 13 advisory warnings. All 85 Rust workspace tests passed, along with arm64 Clippy, formatting, and diff checks.

The emulator's injected camera feed produced a black or cropped preview across live scan attempts. Synthetic frame decoding and manual invitation pairing passed. End-to-end camera pairing and Android NSD on physical Wi-Fi remain unverified for this APK. The physical Samsung pairing and text result above predates these interface changes.

### Linux automatic JPEG application investigation

**Closed after physical verification.** With automatic application enabled, the user sent three different JPEGs from Samsung Android to CachyOS KDE Wayland. Preparing the paste command before each send yielded `1623693`, `79307`, and `1573118` bytes. The original failure did not recur under this controlled procedure. No implementation defect was demonstrated. The Linux clipboard implementation stays intact, and the JPEG/WebP regression tests and MIME-only observer are retained.

On the physical Samsung transfer, `wl-paste --list-types` showed `image/jpeg` and `x-kde-passwordManagerHint` after automatic application. The separate `wl-paste --type image/jpeg` call promptly reported `Clipboard content is not available as requested type "image/jpeg"` and left an empty file. The user ran `cargo run -q -p lyteboard-cli -- clipboard copy` and then retrieved a valid 573×761 JPEG with the same two targets advertised. This verifies the Android transfer, Linux reception and retention, and manual restore for that image.

Automatic application uses the daemon's graphical environment and a serial worker that can coalesce queued writes. Manual application runs in the CLI's graphical environment and reads the retained entry over local IPC. Both call the same Linux writer with `wl-copy --type image/jpeg --sensitive`. The sensitivity target is expected on both paths. At inspection the live daemon used the same Wayland socket and system `wl-copy` as the terminal, reported automatic application on with no recorded writer error, and had automatic sending off. Lyteboard's Wayland watcher skips offers with the sensitivity target before requesting their data.

Ordinary automatic and manual application of a synthetic 573×761 JPEG passed in isolated KWin and in isolated Plasma with Klipper, including runs with Lyteboard automatic sending enabled on the receiver. A deliberately undrained Wayland data request made the next reader time out for both the automatic and manual owners despite both MIME types still being listed. The reported physical failure returned promptly with a different error. In wl-clipboard 2.3.0, that error means the paste process saw an offer and found no matching JPEG MIME type, before issuing any data request. Replacing a JPEG selection with a text offer between the two commands reproduced the exact error and exit status 1 in isolated KWin. The reason the physical paste process saw a different offer remains unknown. [wl-paste 2.3.0 offer selection](https://github.com/bugaevc/wl-clipboard/blob/v2.3.0/src/wl-paste.c#L1654)

During the live retest, the physical Plasma session published two consecutive offers with JPEG and the sensitivity marker. A text offer with no JPEG followed about seven seconds after the second offer. This confirms a selection change in the live session. The way to control for copying commands is to enter the paste command in the terminal before the phone sends and execute it without copying anything in between.

Start a MIME-only capture in the Linux graphical session before sending the test image. It reports timestamped selection changes and a fixed set of format flags. It requests zero clipboard payloads and does not report the source process.

```bash
cargo build --locked -p lyteboard-core --example clipboard_offers
timeout 30s target/debug/examples/clipboard_offers
```

Run these in a second terminal while the failure is present. They report status, MIME types, process state, and a byte count without printing clipboard contents.

```bash
lyteboard clipboard auto
timeout 4s wl-paste --list-types
ps -C wl-copy,cat -o pid,ppid,stat,wchan:24,comm
timeout 4s wl-paste --type image/jpeg | wc -c
printf 'wl-paste status %s\n' "${PIPESTATUS[0]}"
```

A paste status of 124 indicates a timeout. A status of 1 with the quoted MIME error points to the offer visible to that paste process. Run manual restore after the capture. The clipboard harness checks automatic JPEG and WebP application immediately and after a delay, and compares the bytes with manual restore.

```sh
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py --kwin
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py --kwin --x11
```

## LAN-6D checkpoint

On Linux, **Settings → Automatically apply received content** controls the daemon preference for both text and images. It is off on a new profile. The GUI shows the latest received item with its sender, type, size, and **Copy received content** action. The displayed item ID selects the same payload even when a newer transfer arrives. If it has been evicted or the daemon has restarted, the action reports that it is unavailable. The desktop refreshes on focus and every five seconds while visible. Update and restart the desktop and daemon together to enable the new IPC operations.

Incoming items use the daemon's existing in-memory buffer of at most 16 items and 20 MiB combined. The GUI gets metadata through the 4 KiB IPC control frame and copies through the existing bounded bulk channel. The payload stays out of the webview. Lyteboard writes durable history only when it is enabled. Explicit clipboard copies and automatic application publish content to the system clipboard, which the compositor or another clipboard manager can handle independently.

Android's debug configuration uses a stable per-user keystore path. Developers can select an existing standard debug key with the private `lyteboardDebugKeystore` Gradle property to preserve an installed app's identity during upgrades. The setup guide records signing, the actual TCP listener port required by UFW, and why the paste test command must be prepared before receiving an image. [Android guide](../apps/android/README.md)

```sh
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
cargo test --locked -p lyteboard-daemon -p lyteboard-core -p lyteboard-protocol -p lyteboard-desktop
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py --kwin
npm --prefix apps/desktop run lint
npm --prefix apps/desktop run format:check
npm --prefix apps/desktop run tauri build -- --no-bundle --ci -- --locked
cargo build --locked --release -p lyteboard-daemon
PYTHONDONTWRITEBYTECODE=1 python3 apps/desktop/tests/desktop_gui.py
apps/android/gradlew -p apps/android :app:assembleDebug :app:assembleDebugAndroidTest :app:lintDebug
git diff --check
```

All 86 affected Rust tests passed, along with focused Clippy, formatting, and diff checks. The KWin CLI harness exercised the 4 KiB metadata response and copied the original item by ID after a newer arrival. The full native Tauri suite verified incoming feedback, manual copy, and the automatic application toggle with history disabled, including the absence of a history database. Frontend lint, formatting, typecheck, and the release Tauri build passed. Android debug and instrumentation APKs assembled. Android lint completed with zero errors and 13 advisory warnings. Two separately packaged debug APKs had matching signing certificate fingerprints across different Android user homes when pointed at the same existing local debug key.

## LAN-6E checkpoint

On the physical Samsung-to-CachyOS KDE Wayland test, automatic and manual application published a JPEG that `wl-paste` retrieved in full at 79,307 bytes. The user's applications rejected direct JPEG paste. Publishing the same image as PNG allowed the user to paste it into ChatGPT.

On Linux, automatic application, **Copy received content**, CLI clipboard copy, and history restore now publish `image/png` for incoming images. The existing Rust `image` decoders convert JPEG and WebP. Validated PNG bytes pass through. Noise transfers, the receive buffer, saved history entries, and history resend retain the original MIME and bytes. The daemon uses the existing 8,192-pixel axis limit, 16 million pixels, and 64 MiB decoded-image limit, with one validation or conversion in progress and a 10 MiB PNG output limit. Conversion failure leaves the clipboard alone and is reported by manual copy or automatic application status.

The existing Wayland sensitivity hint remains enabled. The daemon remembers the actual PNG fingerprint before publication for echo suppression, including on X11. Native Xorg is outside the graphical regression runs.

```sh
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
cargo test --locked -p lyteboard-core --lib bulk::
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py --kwin --qt-paste
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py --kwin --x11 --qt-paste
git diff --check
```

All 87 workspace tests and focused Clippy passed. The simulated clipboard harness and the KWin Wayland and XWayland runs passed. The optional PyQt6 check pasted PNG, JPEG-derived PNG, and WebP-derived PNG into an image-capable Qt rich-text editor and checked the inserted image dimensions. The harness also verified byte-for-byte original JPEG and WebP history and resending, history restore as PNG, and suppression of an XWayland echo. A separate synthetic oversized conversion produced an explicit error on both automatic and manual application and preserved the previous clipboard and retained JPEG. The user subsequently confirmed physical LAN-6E acceptance: Samsung images received on CachyOS KDE Wayland can be copied and pasted directly into ChatGPT.

## LAN-6F checkpoint

LAN-6E passed user-reported physical acceptance: images received from Samsung Android on CachyOS KDE Wayland can now be copied and pasted directly into ChatGPT. No clipboard, protocol, or Android changes were needed for LAN-6F.

The Linux release is installed for the current user under `~/.local/lib/lyteboard/bin`, with shell symlinks in `~/.local/bin`. Its frontend is embedded. The installed GUI and CLI start the installed sibling daemon through the existing authenticated IPC lifecycle and single-instance locks. Normal application use does not require the repository, a terminal, or development tools.

`scripts/install-linux.py` builds all three release binaries, stages them, installs the existing icon and an absolute-path desktop entry, and refreshes available desktop/KDE caches. The GTK program name matches the desktop entry ID, so KDE can associate windows with the launcher without introducing a session-wide GTK singleton that would break separate development profiles. The first GUI installation enables **Start daemon when Lyteboard opens**. Upgrades preserve later explicit settings. Login autostart remains independently opt-in. Closing the window leaves the daemon running. No system service is installed or enabled.

Upgrade closes this installed GUI, stops the current daemon through authenticated IPC, waits for process exit, replaces binaries while holding the existing runtime locks, and restarts components that were running. An active optional standard systemd user service is managed through `systemctl --user`, with PID and executable checks. Other active runtime profiles using the installed binaries must be stopped first. Uninstall removes installed artifacts and managed login autostart, while retaining credentials, identity, trusted devices, history, and preferences. [Installation guide](linux-install.md)

### Exact installation, upgrade, and removal

Run install/upgrade from the repository in the normal graphical login session, without sudo:

```sh
# First installation, or upgrade (builds native releases).
python3 scripts/install-linux.py install
# Reuse native release artifacts already built from this revision.
python3 scripts/install-linux.py install --skip-build
# A new CLI-only installation, without a GUI launcher.
python3 scripts/install-linux.py install --cli-only
# Removal from any directory; user data is preserved.
python3 "$HOME/.local/lib/lyteboard/install-linux.py" uninstall
```

Search **Lyteboard** in KDE's launcher, then add it to Favorites or pin it to the task manager. If the shell cannot find `lyteboard`, add `~/.local/bin` to its PATH as described in the guide. GUI launch uses an absolute path.

### Focused verification

```sh
cargo build --locked --release -p lyteboard-daemon -p lyteboard-cli
npm --prefix apps/desktop run tauri build -- --no-bundle --ci -- --locked
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_install_linux.py
cargo test --locked -p lyteboard-desktop desktop_preferences::
cargo clippy --locked -p lyteboard-desktop --bin lyteboard-desktop -- -D warnings
cargo fmt --all -- --check
desktop-file-validate contrib/desktop/io.github.arielyte.lyteboard.desktop
PYTHONDONTWRITEBYTECODE=1 python3 apps/desktop/tests/desktop_gui.py \
  --lifecycle-only --binary "$HOME/.local/lib/lyteboard/bin/lyteboard-desktop" \
  --driver /tmp/lyteboard-lan6d-drivers/bin/tauri-driver \
  --webkit-driver /tmp/lyteboard-lan6d-drivers/usr/bin/WebKitWebDriver
git diff --check
```

The three installer tests passed: real release installation, authenticated daemon upgrades, duplicate starts, CLI-only install, another active profile blocking replacement, missing artifacts, managed login-entry updates, preference preservation, and repeated data-preserving uninstall. Systemd profile/executable checks were simulated; no user service was installed or enabled. The focused desktop-preference Rust test, Clippy, formatting, desktop validation, frontend typecheck/Vite production build, and native release build passed. Sandbox restrictions initially prevented private-path/socket tests; they passed outside the sandbox.

**Actual KDE Wayland verification:** the installed desktop entry opened the release with `/usr/bin:/bin` as PATH and `/tmp` as the working directory. Starting from an offline daemon brought up the installed release daemon and authenticated IPC. Closing the native KDE window left `ping` successful. Reopening worked. Live upgrades changed both installed GUI and daemon PIDs, removed the old processes, and preserved five existing profile/settings files byte-for-byte. Repeated launches retained a single GUI and daemon. KDE reported `desktopFileName=io.github.arielyte.lyteboard`; the launcher and icon are registered. The installed CLI was already on the user's PATH. Login autostart remained off.

The focused native WebDriver run also passed start/stop/restart, concurrent daemon starts, saved GUI startup preference, login opt-in toggling, and close/reopen behavior using the installed GUI in an isolated nested KWin session. The normal installed GUI remained open in the primary session, verifying independent runtime profiles. Isolated portal/accessibility services emitted advisory warnings without test failures.

Limitations: installation is per-user and local, without distribution packages. Finish active transfers before upgrade; daemon restart discards ephemeral received items while persistent data survives. If interrupted during binary replacement, rerun installation before opening the GUI. Login/logout startup and a real active systemd-service upgrade were not exercised. Earlier manual binary copies must be moved aside before creating the managed shell symlinks. The installer leaves small lock files/empty directories after removal. Full unrelated regressions were not repeated.

## LAN-7A checkpoint — Windows platform foundation

LAN-6F remains complete. Linux is locally installed and operational, and the user has verified physical Linux/Android interoperability. LAN-7A adds native Windows daemon, CLI, and Tauri entry points using the same Rust LAN worker, TCP/Noise sessions, explicit pairing approvals, persistent trust/revocation, mDNS discovery/reconnection, bounded transfers, and SQLite history. The LAN wire protocol is unchanged.

### Windows implementation and security boundaries

- `crates/windows` contains the Windows FFI behind safe APIs. Other application crates retain their existing unsafe-code prohibition. `ipc::local` selects Unix sockets or Tokio byte-mode named pipes; bounded framing, decoding, request validation, client operations, daemon handlers, worker queues, and timeouts are shared. The 4 KiB control-frame limit, 32-client cap, two local bulk slots, and 20-second bulk deadline remain. Windows local binary streams end with one zero byte after the announced bounded payload; Unix keeps its half-close. Windows bulk downloads preserve the original validated media and bytes.
- The daemon runs as the launching user, without a Windows service or administrator privileges. CLI/GUI lifecycle launches the sibling `lyteboardd.exe` directly with `CREATE_NO_WINDOW`, serializes launches, and checks readiness through authenticated IPC. Authenticated shutdown releases the listener and daemon lock before acknowledging. Console Ctrl+C is supported when a console exists; IPC shutdown also works without a console.
- Runtime, profile, preferences, and history use the current user's LocalAppData Known Folder, normally `%LOCALAPPDATA%\Lyteboard`, with `runtime`, `profile`, and `settings` subdirectories. `LYTEBOARD_RUNTIME_DIR` and `LYTEBOARD_PROFILE` provide absolute local development/test overrides. Network/UNC paths, parent traversal, reparse points, non-private final objects, and hardlinked data/lock files are rejected. Directory handles deny delete-sharing and hold ancestors stable while persistent stores are open. Atomic replacement flushes a private temporary file and uses `MoveFileExW` with replacement and write-through flags. SQLite sidecars inherit the private history directory ACL; history remains opt-in and **unencrypted at rest**.
- Each created directory/file has an explicit user-SID owner and protected DACL granting full access only to that user; directories propagate this grant to children. Existing final objects are checked by handle for owner, every access grant, type, and links. Final directories also require a protected DACL and both child-inheritance flags, so a parent cannot broaden access later and SQLite sidecars inherit only the user grant. Secure exclusive file locks enforce daemon, launcher, profile, history, preferences, and GUI instances. Windows GUI duplicates exit; bringing an existing window forward is deferred. Lock failures fail closed on Windows.
- Pipe names include the user SID and a hash of the canonical runtime directory. Creation supplies an explicit user-only security descriptor, `FILE_FLAG_FIRST_PIPE_INSTANCE` for the initial instance, and `PIPE_REJECT_REMOTE_CLIENTS` on every instance. A replacement listening instance exists before dispatching a client, retaining ownership of the pipe name. Both ends validate the kernel-reported peer PID's process-token user SID against the current user before exchanging requests. Client opens use identification-only security QoS to prevent the server from impersonating them. Other users, unknown peers, and pipe-name squatting fail closed. Processes already running as the same user remain in the local trust boundary. [Microsoft named-pipe security](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights), [remote-client rejection](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-createnamedpipew)
- Identity seeds are encrypted by current-user DPAPI with UI forbidden and saved only as `identity.dpapi` in the private profile. Plaintext and decrypted buffers are zeroized. No machine-wide protection or plaintext fallback exists. Missing credentials on an existing profile, corrupt ciphertext, and DPAPI failures return errors without generating replacement keys. Public identity metadata and trust retain their existing formats and identity binding. [Microsoft DPAPI](https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata)
- Windows clipboard operations deliberately return an unavailable error. Tray behavior and login startup are unavailable. Clipboard conversion/monitoring, GTK, Secret Service, Unix filesystem calls, and Unix signal handlers stay behind platform boundaries. No firewall changes, cloud infrastructure, installer, or packaging were introduced.

### Native Windows commands (PowerShell, normal user)

Use Rust/MSVC with the Windows SDK and C++ build tools, Node/npm, and WebView2 for the desktop. These are development build dependencies, not an elevation requirement for running Lyteboard. From the repository root:

```powershell
cargo build --locked -p lyteboard-daemon -p lyteboard-cli
npm --prefix apps/desktop ci
npm --prefix apps/desktop run tauri build -- --debug --no-bundle --ci -- --locked
.\target\debug\lyteboard.exe daemon start
.\target\debug\lyteboard.exe ping
.\target\debug\lyteboard.exe status
.\target\debug\lyteboard.exe version
.\target\debug\lyteboard.exe network status
.\target\debug\lyteboard.exe daemon stop
.\target\debug\lyteboard-desktop.exe
```

CLI `--version` and daemon `--version` report their local build versions; CLI `version` queries the running daemon. The GUI can start/stop the daemon, configure the listener/discovery, pair/manage devices, and inspect/manage history. Clipboard sends/copies/restores are deferred. Closing the GUI leaves the daemon running. The listener still defaults to `127.0.0.1:0`; configure the device's actual on-link address for physical pairing. Windows firewall policy can block LAN TCP or mDNS: this milestone never adds rules automatically.

### Verification and current evidence

**Passed locally on Linux:**

```sh
cargo build --locked -p lyteboard-cli -p lyteboard-daemon
cargo test --locked -p lyteboard-core -p lyteboard-daemon -p lyteboard-protocol -p lyteboard-desktop
cargo clippy --locked -p lyteboard-core -p lyteboard-daemon -p lyteboard-cli -p lyteboard-desktop --all-targets -- -D warnings
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/lan_cli.py
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py
cargo check --locked --target aarch64-linux-android -p lyteboard-core -p lyteboard-android
cargo fmt --all -- --check
git diff --check
```

All 87 affected Rust tests passed. The LAN harness passed pairing, authenticated delivery, trust persistence, changed-address reconnection, wrong-pin rejection, revocation, and shutdown. The simulated Wayland/X11 clipboard harness passed bulk limits, images, interrupted/excess IPC streams, manual/automatic application, and related history behavior. Socket/private-path tests required running outside the filesystem/network sandbox. Android's Rust core and UniFFI bridge type-checked for the existing ARM64 Android target; Android APK/hardware and graphical Linux acceptance were not repeated.

**Windows compilation checks passed on Linux**, using the installed Rust `x86_64-pc-windows-gnu` target and temporary MinGW headers/runtime/binutils plus a Clang compiler wrapper under `/tmp/lyteboard-lan7-cross`:

```sh
PATH="/tmp/lyteboard-lan7-cross/bin:/tmp/lyteboard-lan7-cross/usr/bin:$PATH" \
  cargo check --locked --target x86_64-pc-windows-gnu --all-targets \
  -p lyteboard-core -p lyteboard-cli -p lyteboard-daemon -p lyteboard-desktop -p lyteboard-windows
PATH="/tmp/lyteboard-lan7-cross/bin:/tmp/lyteboard-lan7-cross/usr/bin:$PATH" \
  cargo clippy --locked --target x86_64-pc-windows-gnu --all-targets \
  -p lyteboard-core -p lyteboard-cli -p lyteboard-daemon -p lyteboard-desktop -p lyteboard-windows -- -D warnings
```

The Windows workspace also passed the same Clippy check with `--workspace --all-targets`, including the Android host crate. These compile/type-check application and test sources, including Windows FFI and bundled SQLite; they do **not** execute Windows code or establish a native MSVC build result. No Windows device is connected and no Windows runtime/Wine is available locally.

The existing `windows-2025` CI matrix now exercises the real Windows entry points through its workspace Clippy/tests/build and embedded-frontend desktop build, then runs `python apps/daemon/tests/windows_cli.py`. New Windows-only Rust tests cover DPAPI round-trip/tamper failure; user-only versus NULL DACLs; hardlinks/path traversal; named-pipe first-instance and peer-SID checks; durable identity and missing/corrupt credential rejection; live daemon lifecycle, duplicate starts, malformed/oversized frames, stalled clients, shutdown and restart. The Python harness covers concurrent sibling-daemon launchers, two-daemon Noise pairing and bidirectional text, binary-pipe bulk upload, exact persisted history bytes, identity/trust across restart, clipboard unavailability, and revocation. Native invocation:

```powershell
cargo test --locked --workspace
cargo build --locked --workspace
python apps/daemon/tests/windows_cli.py
```

**Native Windows runtime verification is pending.** The added CI checks have not been executed from this uncommitted workspace. Actual different-account access denial, remote SMB pipe rejection, Windows desktop rendering/lifecycle, physical Windows/Linux/Android transfers, multicast discovery, and behavior under real Windows firewall policy still require Windows acceptance. ACL/security checks type-check but have not run here. Device identity recovery requires the original Windows user's DPAPI credentials/profile; copying ciphertext alone is not a portable identity backup. Existing same-user processes, administrators, and compromised sessions are outside the local protection boundary. No commit or push was performed.

## Manual two-daemon demonstration

Run from the repository root on Linux. The demonstration uses GNOME Keyring in a disposable D-Bus session. Choose free loopback ports if either example port is occupied.

```bash
cargo build --locked -p lyteboard-daemon -p lyteboard-cli
dbus-run-session -- bash
lan_demo_dir=$(mktemp -d /tmp/lyteboard-manual.XXXXXXXX)
mkdir -m 700 "$lan_demo_dir"/{run,data,config,alpha,beta}
printf %s disposable-lyteboard-demo-password | env -u GNOME_KEYRING_CONTROL \
  XDG_RUNTIME_DIR="$lan_demo_dir/run" XDG_DATA_HOME="$lan_demo_dir/data" \
  XDG_CONFIG_HOME="$lan_demo_dir/config" gnome-keyring-daemon --unlock \
  --components=secrets --control-directory="$lan_demo_dir/run/keyring" >/dev/null

a() {
  XDG_RUNTIME_DIR="$lan_demo_dir/alpha" LYTEBOARD_PROFILE="$lan_demo_dir/alpha-profile" \
    LYTEBOARD_DEVICE_NAME=Alpha LYTEBOARD_LISTEN_ADDR=127.0.0.1:47851 target/debug/lyteboard "$@"
}
b() {
  XDG_RUNTIME_DIR="$lan_demo_dir/beta" LYTEBOARD_PROFILE="$lan_demo_dir/beta-profile" \
    LYTEBOARD_DEVICE_NAME=Beta LYTEBOARD_LISTEN_ADDR=127.0.0.1:47852 target/debug/lyteboard "$@"
}

a daemon start
b daemon start
a status
a network listen 127.0.0.1:47851
b network status
a pair invite | b pair accept
a pair status
b pair status
```

Repeat the status checks until both display a verification code. Compare the complete codes, then enter the matching code at each prompt. `pair invite --qr` renders a new invitation with `qrencode` installed, and `pair accept` accepts the pasted payload on stdin.

```sh
a pair confirm
b pair confirm
a pair status
b pair status
a devices list
b devices list
```

Once both say `Paired`, send messages, restart both daemons, then revoke Beta:

```sh
alpha_id=$(b devices list | cut -f1)
beta_id=$(a devices list | cut -f1)
b send "$alpha_id" 'Hello from Beta'
a received
a send "$beta_id" 'Reply from Alpha'
b received
a devices rename "$beta_id" 'Beta laptop'

a daemon stop
b daemon stop
a daemon start
b daemon start
a devices list
b send "$alpha_id" 'After restart'

a devices remove "$beta_id"
a network status
b send "$alpha_id" 'Revoked message'  # Expected to fail.
a daemon stop
b daemon stop
exit
```

## Manual discovery and reconnection

Use these checks before revoking either device in the demonstration above. From the same shell, choose a local non-loopback interface. Use each device's own address when running on separate hosts.

```bash
read -r -p 'Local LAN IPv4 address: ' lan_ip
a network listen "$lan_ip:47851"
b network listen "$lan_ip:47852"
a network discovery on
b network discovery on
a network discover
b network discover
a devices list
b devices list
```

To exercise changed-address reconnection and shutdown over actual multicast, run the process harness below. With a single LAN IP it changes the listener's port. With a second IP on the same subnet it also changes the listener's IP.

```bash
python3 apps/daemon/tests/lan_cli.py --multicast "$lan_ip"
read -r -p 'Second local LAN IPv4 address on the same subnet: ' moved_ip
python3 apps/daemon/tests/lan_cli.py --multicast "$lan_ip" --move-to "$moved_ip"
```

To use a manual fallback for an existing pin, disable discovery and save the peer's current IP and port. This example uses Alpha's loopback listener from the two-daemon setup.

```bash
b network discovery off
b network discover
a daemon stop
a daemon start
b devices address "$alpha_id" 127.0.0.1:47851
b devices list
b send "$alpha_id" 'Manual fallback'
```

For a restart-persistent discovery opt-out, set `LYTEBOARD_DISCOVERY=off` in the daemon's environment. The CLI toggle applies to the running daemon. Loopback listeners leave multicast paused.

## Manual clipboard demonstration

Start each daemon from its graphical session and pair the devices using the steps above. Use separate graphical sessions to compare selections independently. On the sending device, enter a peer ID shown by `devices list` and seed a short synthetic value:

```bash
lyteboard daemon start
lyteboard devices list
read -r -p 'Paired device ID: ' peer_id
printf 'Lyteboard test ✓\n' | wl-copy --type 'text/plain;charset=utf-8' --sensitive
lyteboard clipboard send "$peer_id"
```

On the receiver, check the default and copy the newest received text. On Wayland, compare its bytes without printing the clipboard:

```bash
lyteboard clipboard auto
lyteboard clipboard copy
wl-paste --no-newline --type text | cmp - <(printf 'Lyteboard test ✓\n')
lyteboard clipboard auto on
```

Send a different value from the first device and compare again on the receiver. Use `lyteboard clipboard auto off` to disable automatic writes, or `lyteboard clipboard copy --index 2` to copy the second-newest message when available. For multiple recipients, use `lyteboard clipboard send "$peer_id" "$other_peer_id"`, with each ID from `devices list`. The CLI reports how many devices acknowledged the text.

For an X11 session, replace the Wayland seed and check with:

```bash
printf 'Lyteboard test ✓\n' | xclip -selection clipboard -in -target UTF8_STRING -silent
xclip -selection clipboard -out -target UTF8_STRING | cmp - <(printf 'Lyteboard test ✓\n')
```

### Larger text and images

After pairing, send larger text from standard input without passing it as a shell argument:

```bash
python3 -c 'import sys; sys.stdout.write("Lyteboard ✓\n" * 30000)' | lyteboard send "$peer_id" --stdin
```

For an image, put a synthetic PNG on the clipboard, send it to a paired device, and restore it on the receiver:

```bash
# Sender, with sample.png a local test image
wl-copy --type image/png --sensitive < sample.png
lyteboard clipboard send "$peer_id"

# Receiver
lyteboard received
lyteboard clipboard copy
wl-paste --no-newline --type image/png | sha256sum
```

On X11 substitute `xclip -selection clipboard -in -target image/png -silent < sample.png` and `xclip -selection clipboard -out -target image/png | sha256sum`. For PNG input, compare the receiver's hash with `sha256sum sample.png` on the sender. For JPEG or WebP, use the matching MIME type on the sender and read `image/png` on the Linux receiver. The PNG hash differs because the local clipboard view has a different encoding.

## Verification

```sh
cargo build --locked -p lyteboard-daemon -p lyteboard-cli
python3 apps/daemon/tests/lan_cli.py
cargo test --locked -p lyteboard-daemon --bin lyteboardd network::
```

- **Simulated discovery:** Rust tests feed synthetic resolved/removed records to the cache and cover malformed, self, loopback, broadcast, and off-link advertisements. They also check retry bounds and deterministic duplicate-session selection.
- **Real TCP, multicast disabled:** the process harness verifies manual fallback after an IP change, rejects an address serving a different Noise identity, and tests persistent trust, revocation, and shutdown.
- **Actual multicast:** the same harness exercises on-host multicast across real LAN interfaces. It checks discovery before pairing, withdrawal on disable and shutdown, and automatic reconnection after a listener address change with the same saved identity. Separate physical hosts were not used.

Clipboard-focused verification from the repository root after building the CLI and daemon:

```sh
cargo test --locked -p lyteboard-core --lib bulk::
cargo test --locked -p lyteboard-core --test lan_tcp bulk
cargo test --locked -p lyteboard-core --lib clipboard::
cargo test --locked -p lyteboard-protocol network_payloads
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py --kwin
PYTHONDONTWRITEBYTECODE=1 python3 apps/daemon/tests/clipboard_cli.py --kwin --x11
cargo test --locked --workspace
```

- **Simulated clipboard adapters:** the process harness uses real daemon IPC, Noise, and TCP. It checks exact 1 MiB Unicode and 10 MiB PNG boundaries, oversized text and images, interrupted and excessive IPC streams, image integrity, multiple recipients, and loop prevention.
- **Real graphical selections:** independent virtual KWin Wayland sessions and nested Xwayland displays round-tripped PNG, JPEG, and WebP byte for byte using synthetic clipboard data. This exercised actual clipboard MIME discovery and reads and writes, isolated from the primary desktop selection. Native Xorg remains unverified.
- **LAN-4B results:** all 74 workspace tests passed. The focused TCP tests rejected a tampered chunk and discarded an interrupted transfer before reconnection. The simulated and real clipboard harnesses passed, as did the separate two-daemon CLI harness with mDNS disabled. Formatting, focused Clippy with warnings denied, and `git diff --check` passed.

## Remaining work

Daemon integration and service advertisement are shared by Linux and Windows; Windows runtime and physical interoperability acceptance remain pending (see LAN-7A). Binding to a new local IP after DHCP or interface changes still requires changing the listener or restarting the daemon with the updated `LYTEBOARD_LISTEN_ADDR`. Multicast can be blocked by the LAN. Use the paired device’s manual address for a current on-link fallback. Scoped IPv6 link-local addresses are outside the current wire format. Connection status follows confirmed TCP sessions and can lag a silent link failure until keepalive detects it. Image MIME availability depends on the source application and clipboard owner. Animated images and file lists are outside the supported formats. Bulk transfers need LAN-4B at both endpoints. The desktop clipboard manager can retain a copy independently. Restart the daemon from the graphical session if automatic clipboard access fails.
