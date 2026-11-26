#!/usr/bin/env python3
"""Run after building the Linux CLI and daemon. --kwin uses real isolated Wayland selections."""

import argparse
import base64
from contextlib import ExitStack
import json
import os
from pathlib import Path
import signal
import select
import random
import socket
import struct
import sqlite3
import sys
import zlib
import subprocess
import tempfile
import time

from lan_cli import Device, isolated_keyring, pair, wait_for


FAKE_UTILITY = """#!/usr/bin/env python3
import os
from pathlib import Path
import sys

tool = Path(sys.argv[0]).name
args = sys.argv[1:]
path = Path(os.environ["LYTEBOARD_TEST_CLIPBOARD"])
if args in (["--version"], ["-version"]):
    sys.exit(0)
if tool == "wl-paste" or "-out" in args:
    if not path.exists():
        print("Nothing is copied" if tool == "wl-paste"
              else "Error: target UTF8_STRING not available", file=sys.stderr)
        sys.exit(1)
    mime = path.with_suffix(".type").read_text()
    if "--list-types" in args or "TARGETS" in args:
        print(mime)
        sys.exit(0)
    assert args == (["--no-newline", "--type", "text" if mime.startswith("text/") else mime] if tool == "wl-paste"
                    else ["-selection", "clipboard", "-out", "-target", "UTF8_STRING" if mime.startswith("text/") else mime])
    sys.stdout.buffer.write(path.read_bytes())
else:
    mime = args[1] if tool == "wl-copy" else args[4]
    (path.with_suffix(".type")).write_text("text/plain;charset=utf-8" if mime == "UTF8_STRING" else mime)
    data = sys.stdin.buffer.read()
    if path.with_suffix(".blocked").exists():
        sys.stderr.buffer.write(data)
        sys.exit(1)
    pending = path.with_suffix(".tmp")
    pending.write_bytes(data)
    pending.replace(path)
"""


class SimulatedSelection:
    def __init__(self, root, device, x11=False):
        self.file = root / f"clipboard-{device.env['LYTEBOARD_DEVICE_NAME']}"
        binary = root / "tools"
        binary.mkdir(exist_ok=True)
        for name in ("wl-copy", "wl-paste", "xclip"):
            tool = binary / name
            tool.write_text(FAKE_UTILITY)
            tool.chmod(0o700)
        device.env.update(PATH=f"{binary}:{os.environ['PATH']}",
                          LYTEBOARD_TEST_CLIPBOARD=str(self.file),
                          XDG_SESSION_TYPE="x11" if x11 else "wayland",
                          WAYLAND_DISPLAY="fake-wayland", DISPLAY=":fake")

    def read(self, _mime=None):
        return self.file.read_bytes() if self.file.exists() else None

    def seed(self, data, mime="text/plain;charset=utf-8"):
        self.file.with_suffix(".type").write_text(mime)
        self.file.write_bytes(data)


def stop_kwin(process, pidfd=None):
    try:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        if pidfd is not None and not select.select([pidfd], [], [], 5)[0]:
            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
            assert select.select([pidfd], [], [], 5)[0], "isolated compositor did not stop"
    finally:
        if pidfd is not None:
            os.close(pidfd)


class KwinSelection:
    def __init__(self, root, device, cleanup, x11=False):
        self.x11 = x11
        self.cleanup = cleanup
        home = root / f"kwin-{device.env['LYTEBOARD_DEVICE_NAME']}"
        for name in ("run", "data", "config"):
            (home / name).mkdir(mode=0o700, parents=True)
        compositor_env = dict(os.environ, XDG_RUNTIME_DIR=str(home / "run"),
                              XDG_DATA_HOME=str(home / "data"), XDG_CONFIG_HOME=str(home / "config"),
                              WAYLAND_DISPLAY="lyteboard-clipboard", XDG_SESSION_TYPE="wayland")
        for name in ("DISPLAY", "WAYLAND_SOCKET", "XAUTHORITY"):
            compositor_env.pop(name, None)
            device.env.pop(name, None)
        socket = home / "run" / "lyteboard-clipboard"
        compositor_pid = home / "compositor.pid"
        # The D-Bus launcher can exit while KWin is still saving its output configuration.
        command = ["dbus-run-session", "--", "sh", "-c",
                   'printf "%s\n" "$$" > "$1"; shift; exec "$@"', "kwin-test", str(compositor_pid),
                   "kwin_wayland", "--virtual", "--no-kactivities",
                   "--no-lockscreen", "--no-global-shortcuts", "--socket", socket.name]
        if x11:
            session = home / "x11-session"
            session.write_text("""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import time
target = Path(__file__).with_name("x11.json")
pending = target.with_suffix(".tmp")
pending.write_text(json.dumps({key: os.environ[key] for key in ("DISPLAY", "XAUTHORITY")
                              if key in os.environ}))
pending.replace(target)
time.sleep(3600)
""")
            session.chmod(0o700)
            command += ["--xwayland", "--exit-with-session", session]
        with (home / "kwin.log").open("wb") as log:
            process = subprocess.Popen(command, env=compositor_env, stdout=log,
                                       stderr=log, start_new_session=True)
        self.process = process
        try:
            wait_for(socket.is_socket)
            pidfd = os.pidfd_open(int(compositor_pid.read_text()))
        except BaseException:
            stop_kwin(process)
            raise
        cleanup.callback(stop_kwin, process, pidfd)
        device.env.update(XDG_SESSION_TYPE="wayland", WAYLAND_DISPLAY=str(socket))
        if x11:
            xenv = home / "x11.json"
            wait_for(xenv.is_file)
            device.env.update(json.loads(xenv.read_text()))
            device.env["XDG_SESSION_TYPE"] = "x11"
        self.env = device.env

    def read(self, mime=None):
        command = (["xclip", "-selection", "clipboard", "-out", "-target", mime or "UTF8_STRING"]
                   if self.x11 else ["wl-paste", "--no-newline", "--type", mime or "text"])
        result = subprocess.run(command, env=self.env, capture_output=True, timeout=3)
        if result.returncode:
            if any(error in result.stderr for error in (
                b"Nothing is copied",
                b"STRING not available",
                b"No suitable type of content copied",
                b"Clipboard content is not available as requested type",
            )):
                return None
            raise AssertionError("reading the isolated graphical selection failed")
        return result.stdout

    def seed(self, data, mime="text/plain;charset=utf-8", *, sensitive=True):
        if not self.x11:
            command = ["wl-copy", "--type", mime]
            if sensitive:
                command.append("--sensitive")
            subprocess.run(command, env=self.env,
                           input=data, check=True, timeout=3,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return

        target = "UTF8_STRING" if mime.startswith("text/") else mime
        process = subprocess.Popen(
            ["xclip", "-selection", "clipboard", "-in", "-target", target, "-quiet"],
            env=dict(self.env, LC_ALL="C"), stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE, start_new_session=True,
        )
        self.cleanup.callback(stop_kwin, process)
        process.stdin.write(data)
        process.stdin.close()
        assert select.select([process.stderr], [], [], 3)[0]
        assert b"Waiting for selection requests" in process.stderr.readline()



def paste_in_qt(selection, width, height):
    program = """
from PyQt6.QtGui import QImage, QPixmap, QTextDocument
from PyQt6.QtWidgets import QApplication, QTextEdit
from PyQt6.QtCore import QUrl
import sys
import time

class ImageEditor(QTextEdit):
    def insertFromMimeData(self, source):
        assert source.hasImage(), source.formats()
        image = source.imageData()
        if isinstance(image, QPixmap):
            image = image.toImage()
        self.textCursor().insertImage(image, "clipboard-image")

app = QApplication([])
edit = ImageEditor()
edit.show()
edit.setFocus()
deadline = time.monotonic() + 3
while time.monotonic() < deadline:
    app.processEvents()
    offer = app.clipboard().mimeData()
    if offer and offer.hasImage():
        break
    time.sleep(0.02)
edit.paste()
fragment = edit.document().begin().begin().fragment()
assert fragment.isValid() and fragment.charFormat().isImageFormat()
name = fragment.charFormat().toImageFormat().name()
image = edit.document().resource(QTextDocument.ResourceType.ImageResource, QUrl(name))
assert isinstance(image, (QImage, QPixmap))
assert (image.width(), image.height()) == tuple(map(int, sys.argv[1:]))
"""
    subprocess.run([sys.executable, "-c", program, str(width), str(height)],
                   env=dict(selection.env, QT_QPA_PLATFORM="xcb" if selection.x11 else "wayland"),
                   check=True, timeout=6, stdout=subprocess.DEVNULL)


def history_image(device):
    path = Path(device.env["LYTEBOARD_PROFILE"]) / "history" / "history.sqlite3"
    with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
        return db.execute("SELECT id, media, payload FROM entries WHERE sent=0 ORDER BY id DESC LIMIT 1").fetchone()


def expect_failure(result, message):
    assert result.returncode != 0
    assert message in result.stderr, result.stderr


def png(width, height, exact_size=None):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    random_bytes = random.Random(13)
    if exact_size:
        rows = (b"\0" * (1 + 3 * width)) * height
    else:
        rows = b"".join(b"\0" + random_bytes.randbytes(3 * width) for _ in range(height))
    tail = chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
    metadata = b""
    if exact_size:
        metadata = chunk(b"tEXt", b"Note\0" + b" " * (exact_size - len(header) - len(tail) - 17))
    return header + metadata + tail


def network_request(device, command):
    control = json.dumps({"version": 1, "body": {"network": command}}).encode()
    stream = socket.socket(socket.AF_UNIX)
    stream.settimeout(3)
    stream.connect(str(Path(device.env["XDG_RUNTIME_DIR"]) / "lyteboard" / "daemon.sock"))
    stream.sendall(struct.pack(">I", len(control)) + control)
    return stream


def local_upload(device, peer, size):
    return network_request(device, {
        "bulk_upload": {"peers": [peer], "media": "text", "size": size},
    })


def response(stream):
    with stream.makefile("rb") as reader:
        size, = struct.unpack(">I", reader.read(4))
        assert size <= 4096
        return json.loads(reader.read(size))["body"]["data"]


def exercise(alpha, beta, gamma, source, manual, automatic, kwin, x11, qt_paste):
    pair(alpha, beta)
    pair(alpha, gamma)
    beta_id = alpha.devices()["Beta"][0]
    gamma_id = alpha.devices()["Gamma"][0]
    wait_for(lambda: alpha.online(beta_id) and alpha.online(gamma_id))
    for device in (alpha, beta, gamma):
        assert device.cli("clipboard", "auto") == "Automatic clipboard application off"
    assert source.read() is None
    expect_failure(alpha.run("clipboard", "send", beta_id, check=False), "Clipboard has no text")
    if kwin and not x11:
        source.seed(b"disposable non-text payload", mime="application/octet-stream")
        expect_failure(alpha.run("clipboard", "send", beta_id, check=False), "Clipboard has no supported text or image")

    oversized = ("é" * (1024 * 1024 // 2 + 16384)).encode()
    source.seed(oversized)
    expect_failure(alpha.run("clipboard", "send", beta_id, gamma_id, check=False), "1048576-byte transfer limit")
    if not x11:
        assert source.read() == oversized
    assert beta.cli("received") == gamma.cli("received") == "No received messages"

    old = b"Local clipboard remains available"
    manual.seed(old)
    text = "Lyteboard test — ß, שלום, 日本語 ✓\n\n".encode()
    source.seed(text)
    assert gamma.cli("clipboard", "auto", "on") == "Automatic clipboard application on"
    assert alpha.cli("clipboard", "send", beta_id, gamma_id, beta_id) == "Clipboard delivered to 2/2 devices"
    assert manual.read() == old
    assert beta.cli("clipboard", "copy") == "Copied received text"
    assert manual.read() == text
    wait_for(lambda: automatic.read() == text)
    with network_request(beta, "latest_received") as stream:
        item = response(stream)["latest_received"]
    assert item["media"] == "text" and item["size"] == len(text)
    assert set(item) == {"id", "peer", "media", "size"}

    alpha.cli("clipboard", "auto", "on")
    beta.cli("clipboard", "auto", "on")
    later = "Copied again — café\n".encode()
    source.seed(later)
    assert alpha.cli("clipboard", "send", beta_id, gamma_id) == "Clipboard delivered to 2/2 devices"
    wait_for(lambda: manual.read() == later and automatic.read() == later)
    with network_request(beta, {"bulk_download_received": {"id": item["id"]}}) as stream:
        with stream.makefile("rb") as reader:
            size, = struct.unpack(">I", reader.read(4))
            assert size <= 4096
            assert json.loads(reader.read(size))["body"]["data"] == {
                "bulk_item": {"media": "text", "size": len(text)},
            }
            assert reader.read() == text
    assert not (Path(beta.env["LYTEBOARD_PROFILE"]) / "history" / "history.sqlite3").exists()
    time.sleep(0.25)
    assert alpha.cli("received") == "No received messages"
    assert len(beta.cli("received").splitlines()) == len(gamma.cli("received").splitlines()) == 2

    if not kwin:
        blocked = automatic.file.with_suffix(".blocked")
        blocked.touch()
        private = b"Synthetic payload in a failing helper"
        source.seed(private)
        assert alpha.cli("clipboard", "send", gamma_id) == "Clipboard delivered to 1/1 devices"
        def auto_error():
            result = gamma.run("clipboard", "auto")
            assert private.decode() not in result.stderr
            return "Clipboard access failed" in result.stderr
        wait_for(auto_error)
        blocked.unlink()
        assert automatic.read() == later

    gamma.cli("clipboard", "auto", "off")
    exact_limit = ("a" + "é" * (1024 * 1024 // 2 - 1) + "z").encode()
    source.seed(exact_limit)
    assert alpha.cli("clipboard", "send", gamma_id) == "Clipboard delivered to 1/1 devices"
    assert automatic.read() == later
    gamma.cli("clipboard", "copy")
    assert automatic.read() == exact_limit
    oldest = 3 if kwin else 4
    gamma.cli("clipboard", "copy", "--index", str(oldest))
    assert automatic.read() == text
    expect_failure(gamma.run("clipboard", "copy", "--index", str(oldest + 1), check=False), "No received content")

    # Local bulk data has a separately bounded raw phase. A partial sender
    # must release its slot, and extra data must never reach a recipient.
    with local_upload(alpha, gamma_id, 4096) as interrupted, local_upload(alpha, gamma_id, 4096) as another:
        assert response(interrupted) == "bulk_ready"
        assert response(another) == "bulk_ready"
        interrupted.sendall(b"partial")
        with local_upload(alpha, gamma_id, 4096) as busy:
            assert "busy" in response(busy)["error"]
        assert alpha.cli("ping") == "Pong"
    assert alpha.cli("ping") == "Pong"
    with local_upload(alpha, gamma_id, 3) as excessive:
        assert response(excessive) == "bulk_ready"
        excessive.sendall(b"four")
        excessive.shutdown(socket.SHUT_WR)
        assert "declared length" in response(excessive)["error"]

    stale = automatic.read()
    image = png(160, 160)
    assert len(image) > 32 * 1024
    source.seed(image, "image/png")
    assert alpha.cli("clipboard", "send", beta_id, gamma_id) == "Clipboard delivered to 2/2 devices"
    assert automatic.read() == stale
    assert beta.cli("clipboard", "copy") == "Copied received image"
    assert gamma.cli("clipboard", "copy") == "Copied received image"
    assert manual.read("image/png") == automatic.read("image/png") == image

    if qt_paste:
        paste_in_qt(manual, 160, 160)

    tiny_images = {
        "image/jpeg": "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAA0JCgsKCA0LCgsODg0PEyAVExISEyccHhcgLikxMC4pLSwzOko+MzZGNywtQFdBRkxOUlNSMj5aYVpQYEpRUk//2wBDAQ4ODhMREyYVFSZPNS01T09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT09PT0//wAARCAACAAIDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAb/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFQEBAQAAAAAAAAAAAAAAAAAABAf/xAAUEQEAAAAAAAAAAAAAAAAAAAAA/9oADAMBAAIRAxEAPwCCAUkN/9k=",
        "image/webp": "UklGRjgAAABXRUJQVlA4ICwAAACQAQCdASoCAAIAAgA0JaACdLoAA5gA/vmTb/+QH/+QH/+QH/8gP+IXeyAwAA==",
    }
    beta.cli("clipboard", "auto", "off")
    gamma.cli("clipboard", "auto", "on")
    beta.cli("history", "enable")
    alpha.cli("clipboard", "auto", "off")
    alpha.cli("history", "enable")
    source_pin = beta.devices()["Alpha"][0]
    for mime, encoded in tiny_images.items():
        image_bytes = base64.b64decode(encoded)
        previous = automatic.read("image/png")
        count = len(alpha.cli("received").splitlines()) if mime == "image/webp" else 0
        source.seed(image_bytes, mime)
        assert alpha.cli("clipboard", "send", beta_id, gamma_id) == "Clipboard delivered to 2/2 devices"

        def converted():
            current = automatic.read("image/png")
            if current and current != previous and current.startswith(b"\x89PNG\r\n\x1a\n"):
                return current
        prepared = wait_for(converted)
        time.sleep(0.25)
        assert automatic.read("image/png") == prepared

        # The manual writer on XWayland has no KDE hint. Its prepared PNG fingerprint
        # must suppress automatic sending when the clipboard owner changes.
        if x11:
            beta.cli("clipboard", "sync", "add", source_pin)
            beta.cli("clipboard", "sync", "on")
        assert beta.cli("clipboard", "copy") == "Copied received image"
        assert manual.read("image/png") == prepared
        if x11:
            time.sleep(0.6)
            beta.cli("clipboard", "sync", "off")
            received = alpha.cli("received")
            assert (0 if received == "No received messages" else len(received.splitlines())) == count

        entry, media, original = history_image(beta)
        assert media == (2 if mime == "image/jpeg" else 3) and original == image_bytes
        beta.cli("history", "restore", str(entry))
        assert manual.read("image/png") == prepared
        if qt_paste:
            paste_in_qt(manual, 2, 2)
            paste_in_qt(automatic, 2, 2)
        beta.cli("history", "resend", str(entry), source_pin)
        assert history_image(alpha)[1:] == (media, image_bytes)
    gamma.cli("clipboard", "auto", "off")

    source.seed(image[:-12], "image/png")
    expect_failure(alpha.run("clipboard", "send", beta_id, check=False), "Image is malformed")
    source.seed(png(8193, 1), "image/png")
    expect_failure(alpha.run("clipboard", "send", beta_id, check=False), "dimension or decoded size limit")
    if not kwin:
        source.seed(b"0" * (10 * 1024 * 1024 + 1), "image/png")
        expect_failure(alpha.run("clipboard", "send", beta_id, check=False), "10485760-byte transfer limit")
        largest_image = png(16, 16, exact_size=10 * 1024 * 1024)
        source.seed(largest_image, "image/png")
        assert alpha.cli("clipboard", "send", gamma_id) == "Clipboard delivered to 1/1 devices"
        gamma.cli("clipboard", "copy")
        assert automatic.read("image/png") == largest_image
    stdin_text = "入力✓" * 30_000
    assert alpha.cli("send", gamma_id, "--stdin", input_text=stdin_text) == "Delivered"
    gamma.cli("clipboard", "copy")
    assert automatic.read() == stdin_text.encode()
    assert len(alpha.cli("received").splitlines()) == 2

    original = alpha.env.copy()
    try:
        alpha.env.update(PATH="/nonexistent")
        expect_failure(alpha.run("clipboard", "send", beta_id, check=False),
                       "Install the wl-clipboard package" if kwin and not x11 else "Install the xclip package")
        alpha.env["PATH"] = original["PATH"]
        if kwin:
            if x11:
                alpha.env["DISPLAY"] = "invalid"
            else:
                alpha.env["WAYLAND_DISPLAY"] = "/nonexistent/wayland"
            expect_failure(alpha.run("clipboard", "send", beta_id, check=False), "Clipboard access failed")
        for name in ("XDG_SESSION_TYPE", "WAYLAND_DISPLAY", "DISPLAY"):
            alpha.env.pop(name, None)
        expect_failure(alpha.run("clipboard", "send", beta_id, check=False), "No graphical clipboard session")
    finally:
        alpha.env = original
    gamma.cli("clipboard", "auto", "on")
    gamma.stop()
    gamma.start()
    assert gamma.cli("clipboard", "auto") == "Automatic clipboard application on"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kwin", action="store_true")
    parser.add_argument("--x11", action="store_true", help="exercise Xwayland for two of the real selections")
    parser.add_argument("--qt-paste", action="store_true", help="paste into a real PyQt6 rich-text widget")
    args = parser.parse_args()
    if args.qt_paste and not args.kwin:
        parser.error("--qt-paste requires --kwin")
    if args.x11 and not args.kwin:
        parser.error("--x11 requires --kwin")
    with tempfile.TemporaryDirectory(prefix="lyteboard-clip-") as directory, ExitStack() as cleanup:
        root = Path(directory)
        keyring = isolated_keyring(root, cleanup)
        devices = [Device(root, name, keyring) for name in ("Alpha", "Beta", "Gamma")]
        selections = []
        for index, device in enumerate(devices):
            if args.kwin:
                selections.append(KwinSelection(root, device, cleanup, x11=args.x11 and index < 2))
            else:
                selections.append(SimulatedSelection(root, device, x11=index == 0))
            cleanup.callback(device.stop)
            device.start()
        exercise(*devices, *selections, args.kwin, args.x11, args.qt_paste)
    if args.kwin:
        mode = "real isolated Xwayland and KWin Wayland selections" if args.x11 else "real isolated KWin Wayland selections"
    else:
        mode = "simulated Wayland and X11 adapters"
    print(f"Clipboard CLI checks passed with {mode}.")


if __name__ == "__main__":
    main()
