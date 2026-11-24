#!/usr/bin/env python3
"""Run after building the Linux CLI and daemon. --kwin uses real isolated Wayland selections."""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import signal
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
    assert args == (["--no-newline", "--type", "text"] if tool == "wl-paste"
                    else ["-selection", "clipboard", "-out", "-target", "UTF8_STRING"])
    if not path.exists():
        print("Nothing is copied" if tool == "wl-paste"
              else "Error: target UTF8_STRING not available", file=sys.stderr)
        sys.exit(1)
    sys.stdout.buffer.write(path.read_bytes())
else:
    assert args == (["--type", "text/plain;charset=utf-8", "--sensitive"] if tool == "wl-copy"
                    else ["-selection", "clipboard", "-in", "-target", "UTF8_STRING", "-silent"])
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

    def read(self):
        return self.file.read_bytes() if self.file.exists() else None

    def seed(self, data):
        self.file.write_bytes(data)


def stop_kwin(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


class KwinSelection:
    def __init__(self, root, device, cleanup, x11=False):
        self.x11 = x11
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
        command = ["dbus-run-session", "--", "kwin_wayland", "--virtual", "--no-kactivities",
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
        cleanup.callback(stop_kwin, process)
        wait_for(socket.is_socket)
        device.env.update(XDG_SESSION_TYPE="wayland", WAYLAND_DISPLAY=str(socket))
        if x11:
            xenv = home / "x11.json"
            wait_for(xenv.is_file)
            device.env.update(json.loads(xenv.read_text()))
            device.env["XDG_SESSION_TYPE"] = "x11"
        self.env = device.env

    def read(self):
        command = (["xclip", "-selection", "clipboard", "-out", "-target", "UTF8_STRING"]
                   if self.x11 else ["wl-paste", "--no-newline", "--type", "text"])
        result = subprocess.run(command, env=self.env, capture_output=True, timeout=3)
        if result.returncode:
            if b"Nothing is copied" in result.stderr or b"STRING not available" in result.stderr:
                return None
            raise AssertionError("reading the isolated graphical selection failed")
        return result.stdout

    def seed(self, data, mime="text/plain;charset=utf-8"):
        command = (["xclip", "-selection", "clipboard", "-in", "-target", "UTF8_STRING", "-silent"]
                   if self.x11 else ["wl-copy", "--type", mime, "--sensitive"])
        subprocess.run(command, env=self.env, input=data, check=True, timeout=3,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def expect_failure(result, message):
    assert result.returncode != 0
    assert message in result.stderr, result.stderr


def exercise(alpha, beta, gamma, source, manual, automatic, kwin, x11):
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
        expect_failure(alpha.run("clipboard", "send", beta_id, check=False), "Clipboard has no text")

    oversized = ("é" * 257).encode()
    source.seed(oversized)
    expect_failure(alpha.run("clipboard", "send", beta_id, gamma_id, check=False), "512-byte transfer limit")
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

    alpha.cli("clipboard", "auto", "on")
    beta.cli("clipboard", "auto", "on")
    later = "Copied again — café\n".encode()
    source.seed(later)
    assert alpha.cli("clipboard", "send", beta_id, gamma_id) == "Clipboard delivered to 2/2 devices"
    wait_for(lambda: manual.read() == later and automatic.read() == later)
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
    exact_limit = ("é" * 256).encode()
    source.seed(exact_limit)
    assert alpha.cli("clipboard", "send", gamma_id) == "Clipboard delivered to 1/1 devices"
    assert automatic.read() == later
    gamma.cli("clipboard", "copy")
    assert automatic.read() == exact_limit
    oldest = 3 if kwin else 4
    gamma.cli("clipboard", "copy", "--index", str(oldest))
    assert automatic.read() == text
    expect_failure(gamma.run("clipboard", "copy", "--index", str(oldest + 1), check=False), "No received text")

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
    assert gamma.cli("clipboard", "auto") == "Automatic clipboard application off"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kwin", action="store_true")
    parser.add_argument("--x11", action="store_true", help="exercise Xwayland for two of the real selections")
    args = parser.parse_args()
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
        exercise(*devices, *selections, args.kwin, args.x11)
    if args.kwin:
        mode = "real isolated Xwayland and KWin Wayland selections" if args.x11 else "real isolated KWin Wayland selections"
    else:
        mode = "simulated Wayland and X11 adapters"
    print(f"Clipboard CLI checks passed with {mode}.")


if __name__ == "__main__":
    main()
