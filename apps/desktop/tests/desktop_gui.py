#!/usr/bin/env python3
"""Drive the real Tauri window with WebDriver and isolated Lyteboard daemons."""

import argparse
import base64
from contextlib import ExitStack
import json
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


def exercise(root, cleanup, args):
    env = isolated_keyring(root, cleanup)
    env.update(LYTEBOARD_HISTORY="off", LYTEBOARD_DISCOVERY="on")
    alpha, beta, gamma = [Device(root, name, env) for name in ("Alpha", "Beta", "Gamma")]
    clip = KwinSelection(root, alpha, cleanup)
    for device in (alpha, beta, gamma):
        cleanup.callback(device.stop)
    window = Window(args, alpha.env, cleanup)
    window.wait_text("Daemon offline")
    window.wait_text("lyteboard daemon start")

    for device in (alpha, beta, gamma):
        device.start()
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
    qr_image = window.capture(qr) if shutil.which("zbarimg") else None
    window.click("Show invitation text")
    invitation = window.js("return document.querySelector('textarea').value;")
    assert invitation.startswith("lyteboard-lan:v1:")
    if qr_image:
        decoded = subprocess.run(["zbarimg", "--quiet", "--raw", "-"], input=qr_image,
                                 capture_output=True, check=True).stdout.decode().strip()
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
    window.refresh()
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
    window.click("History")
    window.wait_text("lyteboard history list")
    window.close()
    assert alpha.cli("ping") == "Pong"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", default="tauri-driver")
    parser.add_argument("--webkit-driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/release/lyteboard-desktop")
    parser.add_argument("--lan-ip", default="127.0.0.1", help="use an on-link IPv4 address to also test real mDNS")
    parser.add_argument("--screenshots", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="lyteboard-desktop-") as directory, ExitStack() as cleanup:
        exercise(Path(directory), cleanup, args)
    print("Native Lyteboard GUI checks passed.")


if __name__ == "__main__":
    main()
