# Lyteboard development handoff

Lyteboard is a LAN-only clipboard-sharing project with a shared Rust core, Tauri + React desktop GUI, and Linux CLI and daemon. Text and images, multiple peers, automatic reconnection, and local history remain the product scope. Windows and a Kotlin Android companion are future targets.

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

The Linux CLI shares up to 1 MiB of UTF-8 text and up to 10 MiB of static PNG, JPEG, or WebP. Clipboard sends choose a supported image target when present. Clipboard copy restores the received MIME type. Auto-apply stays off by default and applies text only when enabled. Clipboard writes never send data back to a peer.

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

On X11 substitute `xclip -selection clipboard -in -target image/png -silent < sample.png` and `xclip -selection clipboard -out -target image/png | sha256sum`. Compare the receiver's hash with `sha256sum sample.png` on the sender. JPEG and WebP use `image/jpeg` and `image/webp` respectively.

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

Discovery and daemon integration are Linux-only. Binding to a new local IP after DHCP or interface changes still requires `network listen` or a daemon restart with the updated `LYTEBOARD_LISTEN_ADDR`. Multicast can be blocked by the LAN. Use `devices address` for a current on-link fallback. Scoped IPv6 link-local addresses are outside the current wire format. Connection status follows confirmed TCP sessions and can lag a silent link failure until keepalive detects it. Image MIME availability depends on the source application and clipboard owner. Animated images and file lists are outside the supported formats. Bulk transfers need LAN-4B at both endpoints. The desktop clipboard manager can retain a copy independently. Restart the daemon from the graphical session if automatic writes cannot reach the display. Continuous clipboard monitoring and GUI peer controls remain pending.
