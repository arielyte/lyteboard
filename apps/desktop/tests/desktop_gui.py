#!/usr/bin/env python3
"""Drive the real Tauri window with WebDriver and isolated Lyteboard daemons."""

import argparse
import base64
from contextlib import ExitStack
import errno
import json
import re
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "apps/daemon/tests"))

from clipboard_cli import KwinSelection, png, stop_kwin
from lan_cli import Device, isolated_keyring, pairing_code, wait_for

ELEMENT = "element-6066-11e4-a52e-4f735466cecf"


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


class Window:
    def __init__(self, args, env, cleanup):
        port = free_port()
        native = free_port()
        while native == port:
            native = free_port()
        driver = subprocess.Popen(
            [args.driver, "--native-driver", args.webkit_driver,
             "--port", str(port), "--native-port", str(native), "--native-host", "127.0.0.1"],
            env=dict(env, GDK_BACKEND="wayland"), start_new_session=True,
            stdout=subprocess.DEVNULL,
        )
        cleanup.callback(stop_kwin, driver)
        self.url = f"http://127.0.0.1:{port}"
        self.session = None

        def ready():
            try:
                self.request("/status")
                return True
            except (OSError, URLError):
                return False
        wait_for(ready)
        session = self.request("/session", {"capabilities": {"alwaysMatch": {
            "tauri:options": {"application": str(Path(args.binary).resolve())},
        }}})
        self.session = "/session/" + session["sessionId"]
        cleanup.callback(self.close)

    def request(self, path, body=None, method=None):
        request = Request(
            self.url + path,
            data=json.dumps(body).encode() if body is not None else None,
            headers={"Content-Type": "application/json"}, method=method,
        )
        try:
            with urlopen(request, timeout=45) as response:
                return json.load(response)["value"]
        except HTTPError as failure:
            detail = json.load(failure).get("value", {})
            # Do not include payloads or WebDriver request parameters in failure output.
            raise AssertionError(f"WebDriver {request.method} {path} returned {detail.get('error', failure.code)}") from None

    def close(self):
        if self.session is not None:
            self.request(self.session, method="DELETE")
            self.session = None

    def js(self, script, *args):
        return self.request(self.session + "/execute/sync", {"script": script, "args": args})

    def button(self, name):
        def find():
            return self.js("""
                const scope = document.querySelector('dialog[open]') ?? document;
                return [...scope.querySelectorAll('button')].find(button =>
                    !button.disabled && (button.getAttribute('aria-label') === arguments[0]
                    || button.textContent.trim() === arguments[0])) ?? null;
            """, name)
        try:
            return wait_for(find)
        except AssertionError:
            raise AssertionError(f"Button unavailable: {name}") from None

    def click(self, name):
        button = self.button(name)
        self.request(self.session + f"/element/{button[ELEMENT]}/click", {})

    def field(self, name, value):
        field = wait_for(lambda: self.js("""
            const scope = document.querySelector('dialog[open]') ?? document;
            return [...scope.querySelectorAll('label')].find(label =>
                label.childNodes[0].textContent.trim() === arguments[0]
            )?.querySelector('input, select') ?? null;
        """, name))
        path = self.session + f"/element/{field[ELEMENT]}"
        if self.js("return arguments[0].tagName === 'SELECT';", field):
            option = self.js("return [...arguments[0].options].find(option => option.value === arguments[1]);", field, value)
            self.request(self.session + f"/element/{option[ELEMENT]}/click", {})
        else:
            self.request(path + "/clear", {})
            self.request(path + "/value", {"text": value})

    def check_element(self, label):
        return wait_for(lambda: self.js("""
            const scope = document.querySelector('dialog[open]') ?? document;
            return [...scope.querySelectorAll('label')].find(label =>
                label.textContent.trim() === arguments[0]
            )?.querySelector('input[type=checkbox]') ?? null;
        """, label))

    def check(self, label, enabled):
        element = self.check_element(label)
        if self.js("return arguments[0].checked;", element) != enabled:
            self.request(self.session + f"/element/{element[ELEMENT]}/click", {})
        wait_for(lambda: self.js("return arguments[0].checked === arguments[1];", element, enabled))

    def checked(self, label):
        return self.js("return arguments[0].checked;", self.check_element(label))

    def visible(self, text):
        return self.js("return document.body.innerText.includes(arguments[0]);", text)

    def wait_text(self, text):
        try:
            wait_for(lambda: self.visible(text))
        except AssertionError:
            raise AssertionError(f"Unable to find expected status: {text}") from None

    def capture(self, element=None):
        suffix = f"/element/{element[ELEMENT]}/screenshot" if element else "/screenshot"
        return base64.b64decode(self.request(self.session + suffix))

    def refresh(self):
        self.click("Refresh daemon and devices")

    def page(self, title):
        self.click(title)
        wait_for(lambda: self.js("return document.querySelector('h1')?.textContent === arguments[0];", title))

    def peer_states(self):
        return self.js("""
            return Object.fromEntries([...document.querySelectorAll('.device-row')]
              .map(row => [row.querySelector('strong').textContent,
                           row.querySelector('.peer-state').textContent]));
        """)


def compare_and_pair(window, peer):
    compare = wait_for(lambda: window.js("return document.querySelector('.verification-code')?.textContent;"))
    assert compare == wait_for(lambda: pairing_code(peer))
    peer.cli("pair", "confirm", input_text=compare + "\n")
    assert peer.cli("pair", "status") != "Paired"
    window.field("Enter the matching code", compare)
    window.click("Confirm match")
    window.wait_text("Device paired successfully.")
    window.click("Done")
    wait_for(lambda: peer.cli("pair", "status") == "Paired")


def discover(window, alpha, beta, host):
    window.page("Settings")
    window.click("Turn off discovery")
    window.wait_text("Discovery disabled.")
    assert alpha.cli("network", "status").endswith("discovery off")
    window.click("Turn on discovery")
    window.wait_text("Discovery enabled.")
    window.field("Listener IP and port", f"{host}:0")
    window.click("Apply listener")
    window.wait_text("LAN listener updated.")
    assert alpha.listener("status").startswith(host + ":")
    if host != "127.0.0.1":
        peer_address = beta.listener("listen", f"{host}:0")
    else:
        peer_address = None
    window.page("Devices")
    window.click("Discover")
    if peer_address:
        wait_for(lambda: peer_address in alpha.discovery())
        window.click("Refresh results")
        window.wait_text(peer_address)
        window.wait_text("Unverified address")
        window.click("Use address")
        window.wait_text("Peer address saved.")
    else:
        window.wait_text("No devices discovered.")
        window.wait_text("Set a LAN listener in Settings")
    window.click("Close Discover devices")


def history_and_preferences(window, alpha, beta, clip, args):
    window.page("History")
    window.wait_text("Clipboard history is off")
    window.click("Open Settings")
    window.wait_text("Local clipboard history is stored without encryption at rest.")
    assert not window.checked("Automatic sending")
    assert not window.checked("Automatically apply received content")
    sender = beta.devices()["Alpha"][0]
    wait_for(lambda: beta.online(sender))
    window.page("Devices")
    clip.seed(b"previous clipboard")
    assert beta.cli("send", sender, "Incoming with history disabled") == "Delivered"
    window.wait_text("Received text from Beta laptop")
    assert clip.read() == b"previous clipboard"
    window.click("Copy received content")
    window.wait_text("Received content copied to the clipboard.")
    assert clip.read() == b"Incoming with history disabled"
    profile = Path(alpha.env["LYTEBOARD_PROFILE"])
    assert not (profile / "history" / "history.sqlite3").exists()

    window.page("Settings")
    window.check("Automatically apply received content", True)
    assert alpha.cli("clipboard", "auto") == "Automatic clipboard application on"
    assert beta.cli("send", sender, "Automatically applied with history disabled") == "Delivered"
    wait_for(lambda: clip.read() == b"Automatically applied with history disabled")
    window.check("Automatically apply received content", False)
    assert alpha.cli("clipboard", "auto") == "Automatic clipboard application off"
    assert not (profile / "history" / "history.sqlite3").exists()
    window.check("Persistent clipboard history", True)
    window.wait_text("History enabled.")
    for label, value in (("Maximum entries", "25"), ("Maximum MiB", "12"), ("Maximum days", "3")):
        window.field(label, value)
    window.click("Save retention")
    window.wait_text("Retention saved.")
    assert alpha.cli("history", "status") == "History on (25 entries, 12 MiB, 3 days)"
    window.page("History")
    window.wait_text("No history entries")

    sender = beta.devices()["Alpha"][0]
    wait_for(lambda: beta.online(sender))
    for index in range(17):
        assert beta.cli("send", sender, f"Archive entry {index:02d}") == "Delivered"
    window.page("Devices")
    image = png(320, 160)
    clip.seed(image, "image/png")
    window.click("Send clipboard to Beta laptop")
    window.wait_text("Clipboard delivered to 1 of 1 device.")

    window.page("History")
    wait_for(lambda: window.js("""
        const img = document.querySelector('.history-entry img');
        return img && img.naturalWidth === 128 && img.naturalHeight === 64;
    """))
    window.wait_text("Sent to Beta laptop")
    clip.seed(b"Replace the live clipboard before restoring the image")
    window.click("Restore")
    window.wait_text("Restored to clipboard.")
    assert clip.read("image/png") == image
    window.click("Next")
    window.wait_text("Page 2")
    assert wait_for(lambda: window.js("return document.querySelectorAll('.history-entry').length === 2;"))
    window.click("Previous")
    window.field("Search history text", "archive entry 05")
    window.click("Search")
    window.wait_text("Archive entry 05")
    assert window.js("return document.querySelectorAll('.history-entry').length;") == 1
    window.wait_text("Received from Beta laptop")
    window.click("Restore")
    window.wait_text("Restored to clipboard.")
    assert clip.read() == b"Archive entry 05"
    window.click("Resend")
    window.check("Beta laptop", True)
    window.click("Resend to selected")
    window.wait_text("Clipboard delivered to 1 of 1 device.")
    assert "Archive entry 05" in beta.cli("received")
    assert len(alpha.cli("history", "list", "--search", "archive entry 05").splitlines()) == 3

    window.field("Search history text", "archive entry 04")
    window.click("Search")
    window.wait_text("Archive entry 04")
    window.click("Delete")
    window.wait_text("History entry deleted.")
    window.wait_text("No matching entries")
    window.click("Clear history")
    window.click("Keep history")
    assert "Archive" not in alpha.cli("history", "list")

    window.page("Settings")
    window.check("Persistent clipboard history", False)
    window.wait_text("History disabled.")
    window.page("History")
    window.wait_text("Clipboard history is off")
    window.page("Settings")
    window.check("Persistent clipboard history", True)
    window.wait_text("History enabled.")
    window.page("History")
    window.wait_text("Sent to Beta laptop")

    window.page("Settings")
    window.check("Beta laptop", True)
    window.check("Automatic sending", True)
    window.check("Automatically apply received content", True)
    window.click("Turn off discovery")
    window.wait_text("Discovery disabled.")
    listener = f"{args.lan_ip}:{free_port()}"
    window.field("Listener IP and port", listener)
    window.click("Apply listener")
    window.wait_text("LAN listener updated.")
    alpha.env.pop("LYTEBOARD_LISTEN_ADDR", None)

    alpha.stop()
    window.refresh()
    window.wait_text("Daemon offline")
    alpha.start()
    window.refresh()
    window.wait_text("Daemon running")
    assert alpha.listener("status") == listener
    assert alpha.cli("network", "status").endswith("discovery off")
    assert alpha.cli("history", "status") == "History on (25 entries, 12 MiB, 3 days)"
    assert alpha.cli("clipboard", "sync", "status").startswith("Automatic clipboard sending on")
    assert alpha.cli("clipboard", "auto") == "Automatic clipboard application on"
    for label in ("Beta laptop", "Automatic sending", "Automatically apply received content", "Persistent clipboard history"):
        assert window.checked(label)
    window.page("History")
    window.wait_text("Sent to Beta laptop")
    window.click("Clear history")
    window.click("Confirm clear")
    window.wait_text("No history entries")

    alpha.stop()
    file = Path(alpha.env["LYTEBOARD_PROFILE"]) / "history" / "history.sqlite3"
    file.write_bytes(b"corrupt disposable history")
    alpha.start()
    window.refresh()
    window.wait_text("History unavailable")
    window.wait_text("History storage is unavailable")
    assert alpha.cli("ping") == "Pong"


class DesktopDevice(Device):
    """Also stop a daemon launched by the GUI rather than the test process."""

    def stop(self):
        if self.process is None and self.run("ping", check=False).returncode == 0:
            self.cli("daemon", "stop")
        else:
            super().stop()


def daemon_lifecycle(cleanup, args, window, alpha):
    window.wait_text("Daemon offline")
    window.click("Start daemon")
    window.wait_text("Daemon running")
    assert alpha.cli("ping") == "Pong"

    window.page("Settings")
    window.wait_text("No system tray is available")
    window.click("Stop daemon")
    window.wait_text("Daemon offline")
    assert alpha.run("ping", check=False).returncode != 0
    window.click("Start daemon")
    window.wait_text("Daemon running")
    status = alpha.cli("status")

    assert alpha.cli("daemon", "start") == "Daemon already running"
    assert alpha.cli("status").split("PID ")[1].split(",")[0] == status.split("PID ")[1].split(",")[0]
    window.click("Stop daemon")
    window.wait_text("Daemon offline")
    children = [subprocess.Popen(
        [str(ROOT / "target/debug/lyteboard"), "daemon", "start"], env=alpha.env,
        stdout=subprocess.PIPE, text=True,
    ) for _ in range(4)]
    outcomes = [child.communicate(timeout=6)[0].strip() for child in children]
    assert all(child.returncode == 0 for child in children)
    assert outcomes.count("Daemon started") == 1
    assert outcomes.count("Daemon already running") == 3
    window.wait_text("Daemon running")

    assert not window.checked("Start daemon when Lyteboard opens")
    assert not window.checked("Open Lyteboard at login")
    login_entry = Path(alpha.env["XDG_CONFIG_HOME"]) / "autostart/io.github.arielyte.lyteboard.desktop"
    window.check("Open Lyteboard at login", True)
    assert login_entry.exists() and "Terminal=false" in login_entry.read_text()
    window.check("Open Lyteboard at login", False)
    assert not login_entry.exists()
    window.check("Start daemon when Lyteboard opens", True)

    window.close()
    assert alpha.cli("ping") == "Pong"
    alpha.stop()
    window = Window(args, alpha.env, cleanup)
    window.wait_text("Daemon running")
    assert alpha.cli("ping") == "Pong"
    window.page("Settings")
    assert window.checked("Start daemon when Lyteboard opens")
    window.check("Start daemon when Lyteboard opens", False)
    window.page("Devices")
    return window


def exercise(root, cleanup, args):
    env = isolated_keyring(root, cleanup)
    env.update(LYTEBOARD_HISTORY="off", LYTEBOARD_DISCOVERY="on")
    alpha = DesktopDevice(root, "Alpha", env)
    beta, gamma = [Device(root, name, env) for name in ("Beta", "Gamma")]
    clip = KwinSelection(root, alpha, cleanup)
    for device in (alpha, beta, gamma):
        cleanup.callback(device.stop)
    window = Window(args, alpha.env, cleanup)
    window = daemon_lifecycle(cleanup, args, window, alpha)

    for device in (beta, gamma):
        device.start()
    for device in (alpha, beta, gamma):
        device.env["LYTEBOARD_LISTEN_ADDR"] = device.listener("status")
    gamma_address = gamma.env["LYTEBOARD_LISTEN_ADDR"]
    window.refresh()
    window.wait_text("No paired devices")

    window.click("Pair device")
    window.click("Generate QR")
    window.wait_text("Scan this invitation")
    window.click("Cancel invitation")
    wait_for(lambda: window.js("return document.querySelector('dialog[open]') === null;"))
    assert alpha.cli("pair", "status") == "No pairing in progress"
    window.click("Pair device")
    window.click("Generate QR")
    qr = wait_for(lambda: window.js("return document.querySelector('.qr-wrap svg') ?? null;"))
    decoded = None
    if shutil.which("zbarimg"):
        def scan_qr():
            # A DOM element can be present just before WebKit paints its first frame.
            result = subprocess.run(["zbarimg", "--quiet", "--raw", "-"], input=window.capture(qr),
                                    capture_output=True)
            return result.stdout.decode().strip() if result.returncode == 0 else None
        decoded = wait_for(scan_qr)
    window.click("Show invitation text")
    invitation = window.js("return document.querySelector('textarea').value;")
    assert invitation.startswith("lyteboard-lan:v1:")
    if decoded:
        assert decoded == invitation
    beta.cli("pair", "accept", input_text=invitation + "\n")
    compare_and_pair(window, beta)

    window.click("Pair device")
    window.click("Import invitation")
    window.field("Paste invitation", "invalid invitation")
    window.click("Connect")
    window.wait_text("Enter a valid Lyteboard LAN invitation.")
    window.field("Paste invitation", gamma.cli("pair", "invite"))
    window.click("Connect")
    compare_and_pair(window, gamma)

    wait_for(lambda: window.peer_states() == {"Beta": "online", "Gamma": "online"})
    window.click("Send clipboard to Beta")
    window.wait_text("Clipboard has no text")
    clip.seed("One peer from the GUI ✓".encode())
    window.click("Send clipboard to Beta")
    window.wait_text("Clipboard delivered to 1 of 1 device.")
    assert "One peer from the GUI" in beta.cli("received")
    assert "One peer from the GUI" not in gamma.cli("received")

    clip.seed("Two peers from the GUI ✓".encode())
    window.click("Select online")
    window.click("Send to selected (2)")
    window.wait_text("Clipboard delivered to 2 of 2 devices.")
    assert all("Two peers from the GUI" in device.cli("received") for device in (beta, gamma))

    image = png(80, 60)
    clip.seed(image, "image/png")
    window.click("Send clipboard to Beta")
    window.wait_text("Clipboard delivered to 1 of 1 device.")
    assert f"image/png, {len(image)} bytes" in beta.cli("received")

    window.click("Manage Beta")
    window.field("Device name", "Beta laptop")
    window.click("Save name")
    window.wait_text("Device renamed.")
    window.click("Close Manage device")
    window.wait_text("Beta laptop")
    window.click("Manage Gamma")
    window.field("Peer IP and port", gamma_address)
    window.click("Save address")
    window.wait_text("Peer address saved.")
    window.click("Close Manage device")
    assert alpha.devices()["Gamma"][1] == gamma_address

    gamma.stop()
    wait_for(lambda: alpha.devices()["Gamma"][2] != "online")
    wait_for(lambda: window.peer_states().get("Gamma") in ("offline", "connecting"))
    gamma.start()
    wait_for(lambda: alpha.devices()["Gamma"][2] == "online")
    window.refresh()
    wait_for(lambda: window.peer_states().get("Gamma") == "online")

    window.page("Settings")
    window.field("Theme", "dark")
    wait_for(lambda: window.js("return getComputedStyle(document.documentElement).colorScheme === 'dark';"))
    window.page("Devices")
    if args.screenshots:
        args.screenshots.mkdir(parents=True, exist_ok=True)
        (args.screenshots / "devices-dark.png").write_bytes(window.capture())
    window.page("Settings")
    window.field("Theme", "light")
    wait_for(lambda: window.js("return getComputedStyle(document.documentElement).colorScheme === 'light';"))
    window.page("Devices")
    if args.screenshots:
        (args.screenshots / "devices-light.png").write_bytes(window.capture())

    discover(window, alpha, beta, args.lan_ip)
    window.click("Manage Gamma")
    window.click("Remove device")
    assert "Gamma" in alpha.devices()
    window.click("Keep device")
    window.click("Remove device")
    window.click("Confirm removal")
    window.wait_text("Device removed.")
    assert "Gamma" not in alpha.devices()
    history_and_preferences(window, alpha, beta, clip, args)
    window.close()
    assert alpha.cli("ping") == "Pong"


def startup_errors(root, cleanup, args):
    env = isolated_keyring(root, cleanup)
    no_daemon = root / "missing-daemon"
    no_daemon.mkdir()
    binary = no_daemon / "lyteboard-desktop"
    shutil.copy2(Path(args.binary).resolve(), binary)
    missing = Device(root, "Missing", env)
    KwinSelection(root, missing, cleanup)
    window = Window(argparse.Namespace(**{**vars(args), "binary": str(binary)}), missing.env, cleanup)
    window.wait_text("Daemon offline")
    window.click("Start daemon")
    window.wait_text("Install lyteboardd alongside the Lyteboard executable.")
    assert missing.run("ping", check=False).returncode != 0
    window.close()

    locked = DesktopDevice(root, "Locked", env)
    KwinSelection(root, locked, cleanup)
    cleanup.callback(locked.stop)
    window = Window(args, locked.env, cleanup)
    window.wait_text("Daemon offline")
    window.click("Start daemon")
    window.wait_text("No paired devices")
    window.page("Settings")
    window.click("Stop daemon")
    window.wait_text("Daemon offline")

    def secret_service(method, value):
        return subprocess.check_output([
            "gdbus", "call", "--session", "--dest", "org.freedesktop.secrets",
            "--object-path", "/org/freedesktop/secrets", "--method",
            "org.freedesktop.Secret.Service." + method, value,
        ], env=env, text=True, timeout=5)
    collection, = re.findall(r"'([^']+)'", secret_service("ReadAlias", "default"))
    secret_service("Lock", f"[objectpath '{collection}']")
    window.click("Start daemon")
    window.wait_text("Secret Service is locked. Unlock your desktop keyring and retry.")
    assert locked.cli("ping") == "Pong"
    window.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", default="tauri-driver")
    parser.add_argument("--webkit-driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/release/lyteboard-desktop")
    parser.add_argument("--lan-ip", default="127.0.0.1", help="use an on-link IPv4 address to also test real mDNS")
    parser.add_argument("--screenshots", type=Path)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="lyteboard-desktop-"))
    try:
        with ExitStack() as cleanup:
            exercise(root, cleanup, args)
            errors = root / "errors"
            errors.mkdir()
            startup_errors(errors, cleanup, args)
    finally:
        # On shutdown KWin can finish writing its output configuration after the session wrapper exits.
        for attempt in range(5):
            try:
                shutil.rmtree(root)
                break
            except OSError as error:
                if error.errno != errno.ENOTEMPTY or attempt == 4:
                    raise
                time.sleep(0.2)
    print("Native Lyteboard GUI checks passed.")


if __name__ == "__main__":
    main()
