#!/usr/bin/env python3
"""Verify the real Android bridge against two isolated Linux daemons on an emulator."""

import argparse
import base64
from contextlib import ExitStack
import hashlib
import json
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "apps/daemon/tests"))
from clipboard_cli import SimulatedSelection, png
from lan_cli import Device, isolated_keyring, pairing_code, wait_for

APP = "app.lyteboard.android"
DIR = "files/lan6b-test"


def wait(check, seconds=15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.15)
    raise AssertionError("timed out waiting for Android LAN state")


class Android:
    def __init__(self, serial, package, instrumentation):
        self.adb = ["adb", "-s", serial]
        self.process = None
        for apk in (package, instrumentation):
            self.run("install", "--abi", "arm64-v8a", "-r", str(apk))
        self.run("shell", "am", "force-stop", APP)

    def run(self, *arguments, input_text=None, check=True):
        return subprocess.run([*self.adb, *arguments], input=input_text, text=True,
                              check=check, capture_output=True, timeout=30)

    def shell(self, script, *, input_text=None, check=True):
        return self.run("shell", "run-as", APP, "sh", "-c", f"'{script}'",
                        input_text=input_text, check=check)

    def start(self):
        self.shell(f"mkdir -p {DIR}; rm -f {DIR}/request {DIR}/reply")
        self.process = subprocess.Popen([*self.adb, "shell", "am", "instrument", "-w",
                                         f"{APP}.test/{APP}.LanVerification"],
                                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        return self.command("state")

    def stop(self):
        if self.process is None:
            return
        try:
            self.command("stop")
            output, _ = self.process.communicate(timeout=10)
            assert "INSTRUMENTATION_CODE: -1" in output, output
        finally:
            self.run("shell", "am", "force-stop", APP)
            self.process.wait(timeout=10)
            self.process = None
            self.shell(f"rm -f {DIR}/request {DIR}/incoming {DIR}/reply {DIR}/reply.tmp")

    def command(self, op, *, expect_error=False, **values):
        self.shell(f"cat > {DIR}/incoming; mv {DIR}/incoming {DIR}/request",
                   input_text=json.dumps(dict(op=op, **values)))
        def response():
            read = self.shell(f"cat {DIR}/reply && rm {DIR}/reply", check=False)
            if read.returncode == 0:
                return json.loads(read.stdout)
            if self.process and self.process.poll() is not None:
                raise AssertionError(self.process.stdout.read())
            return None
        value = wait(response)
        if expect_error:
            assert "error" in value
            return value["error"]
        assert "error" not in value, value
        return value["result"]

    def pair(self, linux):
        # The emulator sees host loopback at 10.0.2.2. Only the IP hint changes.
        # The Linux-generated invitation ID, expiry, key pin, and PSK are preserved.
        prefix = "lyteboard-lan:v1:"
        original = linux.cli("pair", "invite")
        assert original.startswith(prefix)
        raw = base64.urlsafe_b64decode(original[len(prefix):] + "===")
        assert raw.startswith(bytes.fromhex("8701447f000001"))
        hint = base64.urlsafe_b64encode(raw[:3] + bytes([10, 0, 2, 2]) + raw[7:])
        self.command("invite", value=prefix + hint.decode().rstrip("="))
        linux_code = wait_for(lambda: pairing_code(linux))
        android_code = wait(lambda: self.command("state")["pairing"])
        assert android_code == linux_code
        self.command("approve", value=linux_code)
        assert self.command("state")["pairing"] == "waiting"
        linux.cli("pair", "confirm", input_text=android_code + "\n")
        wait(lambda: self.command("state")["pairing"] == "complete")
        return self.connected(linux.env["LYTEBOARD_DEVICE_NAME"])

    def connected(self, label):
        return wait(lambda: next((peer["id"] for peer in self.command("state")["devices"]
                                 if peer["label"] == label and peer["connection"] == "CONNECTED"), None), 40)

    def screen(self):
        self.run("shell", "uiautomator", "dump", "/sdcard/lyteboard-verification.xml")
        return self.run("shell", "cat", "/sdcard/lyteboard-verification.xml").stdout

    def tap(self, label):
        def target():
            for element in ET.fromstring(self.screen()).iter("node"):
                if element.get("text") == label:
                    return [int(value) for value in re.findall(r"\d+", element.get("bounds", ""))]
            return None
        left, top, right, bottom = wait(target, 10)
        self.run("shell", "input", "tap", str((left + right) // 2), str((top + bottom) // 2))

    def verify_activity(self, alpha, beta, android_id):
        self.stop()
        try:
            self.run("shell", "wm", "size", "480x900")
            self.run("shell", "input", "keyevent", "KEYCODE_WAKEUP")
            self.run("shell", "wm", "dismiss-keyguard")
            self.run("shell", "am", "start", "-W", "-n", f"{APP}/.MainActivity")
            wait(lambda: alpha.online(android_id) and beta.online(android_id))
            screen = self.screen()
            assert "Lyteboard" in screen and "Pair device" in screen and "Send clipboard" in screen
            self.run("shell", "am", "start", "-W", "-n", f"{APP}/.MainActivity",
                     "-a", "android.intent.action.SEND", "-t", "text/plain",
                     "--es", "android.intent.extra.TEXT", "Lyteboard-share-test")
            wait(lambda: "Lyteboard-share-test" in self.screen())
            self.tap("Cancel")
            sent = "Lyteboard-clipboard-test"
            assert beta.cli("send", android_id, sent) == "Delivered"
            self.tap("Preview")
            self.tap("Copy")
            self.tap("Send clipboard")
            wait(lambda: sent in self.screen())
            self.tap("Alpha")
            self.tap("Beta")
            self.tap("Send to 2")
            wait(lambda: sent in alpha.cli("received") and sent in beta.cli("received"))
            self.run("shell", "input", "keyevent", "KEYCODE_HOME")
            wait(lambda: not beta.online(android_id))
            self.run("shell", "am", "start", "-W", "-n", f"{APP}/.MainActivity")
            wait(lambda: beta.online(android_id))
        finally:
            self.run("shell", "am", "force-stop", APP)
            self.run("shell", "rm", "-f", "/sdcard/lyteboard-verification.xml")
            self.run("shell", "wm", "size", "reset")


def transfer(android, linux, selection, android_id, linux_id, kind, data, mime):
    android.command("send", id=linux_id, kind=kind, bytes=base64.b64encode(data).decode())
    wait(lambda: android.command("state")["send"] == "Delivered.")
    linux.cli("clipboard", "copy")
    written = selection.read()
    if kind in {"JPEG", "WEBP"}:
        assert written.startswith(b"\x89PNG\r\n\x1a\n")
    else:
        assert written == data
    previous = android.command("state").get("sequence")
    selection.seed(data, mime)
    linux.cli("clipboard", "send", android_id)
    digest = hashlib.sha256(data).hexdigest()
    def arrived():
        state = android.command("state")
        return state.get("sequence") != previous and state.get("digest") == digest
    wait(arrived)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("serial")
    args = parser.parse_args()
    build = Path("apps/android/app/build/outputs/apk")
    with tempfile.TemporaryDirectory(prefix="lyteboard-android-") as directory, ExitStack() as cleanup:
        root = Path(directory)
        android = Android(args.serial, build / "debug/app-debug.apk",
                          build / "androidTest/debug/app-debug-androidTest.apk")
        cleanup.callback(android.stop)
        initial = android.start()
        assert not initial["devices"], "Use a disposable emulator profile with empty Lyteboard data."
        android_id = initial["id"]
        keyring = isolated_keyring(root, cleanup)
        alpha, beta = (Device(root, name, keyring) for name in ("Alpha", "Beta"))
        for linux in (alpha, beta):
            linux.env["LYTEBOARD_DISCOVERY"] = "off"
            cleanup.callback(linux.stop)
            linux.start()
        assert android.command("platform")
        discovered = android.command("discovery")
        print("Android NSD local advertisement resolved." if discovered else
              "Android NSD local advertisement was not resolved on this emulator.", flush=True)
        alpha_id, beta_id = (android.pair(device) for device in (alpha, beta))
        assert alpha_id != beta_id
        alpha_clipboard, beta_clipboard = (SimulatedSelection(root, device) for device in (alpha, beta))

        text = ("Hello, 世界 ✓\n" * 2000).encode()
        transfer(android, alpha, alpha_clipboard, android_id, alpha_id, "TEXT",
                 text, "text/plain;charset=utf-8")
        assert not android.command("history_next")
        android.command("history", value=True)
        transfer(android, alpha, alpha_clipboard, android_id, alpha_id, "TEXT",
                 text, "text/plain;charset=utf-8")
        digest = hashlib.sha256(text).hexdigest()
        assert android.command("history_next") == {"sent": True, "device": alpha_id, "digest": digest}
        assert android.command("history_next") == {"sent": False, "device": alpha_id, "digest": digest}
        android.command("history", value=False)

        transfer(android, beta, beta_clipboard, android_id, beta_id, "PNG",
                 png(160, 160), "image/png")
        images = {
            "JPEG": ("/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAA0JCgsKCA0LCgsODg0PEyAVExISEyccHhcgLikxMC4pLSwzOko+MzZGNywtQFdBRkxOUlNSMj5aYVpQYEpRUk//2wBDAQ4ODhMREyYVFSZPNS01T09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT0//wAARCAACAAIDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAb/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFQEBAQAAAAAAAAAAAAAAAAAABAf/xAAUEQEAAAAAAAAAAAAAAAAAAAAA/9oADAMBAAIRAxEAPwCCAUkN/9k="),
            "WEBP": "UklGRjgAAABXRUJQVlA4ICwAAACQAQCdASoCAAIAAgA0JaACdLoAA5gA/vmTb/+QH/+QH/+QH/8gP+IXeyAwAA==",
        }
        for kind, sample in images.items():
            transfer(android, beta, beta_clipboard, android_id, beta_id, kind,
                     base64.b64decode(sample), "image/" + kind.lower())

        android.command("active", value=False)
        assert not alpha.online(android_id) or wait(lambda: not alpha.online(android_id))
        android.command("active", value=True)
        assert android.connected("Alpha") == alpha_id

        android.stop()
        restored = android.start()
        assert restored["id"] == android_id
        assert len(restored["devices"]) == 2
        assert android.connected("Alpha") == alpha_id
        assert android.connected("Beta") == beta_id

        alpha.stop()
        alpha.env["LYTEBOARD_LISTEN_ADDR"] = "127.0.0.1:0"
        alpha.start()
        moved = alpha.listener("status")
        android.command("network_changed")
        android.command("hint", id=alpha_id, address="10.0.2.2:" + moved.split(":")[1])
        assert android.connected("Alpha") == alpha_id
        android.command("connect", id=alpha_id, address="10.0.2.2:" + moved.split(":")[1])
        assert android.connected("Alpha") == alpha_id
        android.verify_activity(alpha, beta, android_id)
        android.start()
        assert android.connected("Alpha") == alpha_id
        android.command("revoke", id=alpha_id)
        wait(lambda: not alpha.online(android_id))
        android.stop()
        revoked = android.start()
        assert [peer["id"] for peer in revoked["devices"]] == [beta_id]
        android.command("connect", expect_error=True, id=alpha_id,
                        address="10.0.2.2:" + moved.split(":")[1])
        assert android.connected("Beta") == beta_id
        print("Android/Linux LAN verification passed: two peers, Unicode, PNG/JPEG/WebP, "
              "foreground clipboard, Android share intent, history retention, reconnection, persistence, revocation.")


if __name__ == "__main__":
    main()
