#!/usr/bin/env python3
"""Native Windows lifecycle, Noise pairing, bulk transfer, history and restart smoke test."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, closing
import os
from pathlib import Path
import subprocess
import sqlite3
import tempfile
import sys

from lan_cli import Device as LanDevice, pair, send, wait_for

BIN = Path("target/debug").resolve()


class Device(LanDevice):
    def __init__(self, root, name):
        self.env = dict(os.environ, LYTEBOARD_RUNTIME_DIR=str(root / name),
                        LYTEBOARD_PROFILE=str(root / (name + "-profile")),
                        LYTEBOARD_DEVICE_NAME=name, LYTEBOARD_DISCOVERY="off",
                        LYTEBOARD_LISTEN_ADDR="127.0.0.1:0", LYTEBOARD_HISTORY="on")
        self.started = False

    def run(self, *arguments, input_text=None, check=True):
        result = subprocess.run([str(BIN / "lyteboard.exe"), *arguments], env=self.env,
                                input=input_text, text=True, encoding="utf-8",
                                capture_output=True, timeout=90)
        if check and result.returncode:
            raise AssertionError(f"{arguments}: {result.stdout} {result.stderr}")
        return result

    def start(self):
        self.cli("daemon", "start")
        self.started = True
        wait_for(lambda: self.run("ping", check=False).returncode == 0)

    def stop(self):
        if self.started:
            self.cli("daemon", "stop")
            wait_for(lambda: self.run("ping", check=False).returncode != 0)
            self.started = False


def main():
    if sys.platform != "win32":
        raise SystemExit("Run this harness on native Windows.")
    with tempfile.TemporaryDirectory(prefix="lyteboard-windows-") as directory, ExitStack() as cleanup:
        root = Path(directory)
        alpha, beta = (Device(root, name) for name in ("Alpha", "Beta"))
        for device in (alpha, beta):
            cleanup.callback(device.stop)
        # Concurrent launchers must converge on one authenticated daemon.
        with ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(lambda _: alpha.start(), range(4)))
        first_status = alpha.cli("status")
        assert "Daemon running" in first_status
        assert alpha.cli("version")
        beta.start()
        pair(alpha, beta)
        alpha_key = beta.devices()["Alpha"][0]
        beta_key = alpha.devices()["Beta"][0]
        send(alpha, beta, beta_key, "Windows Noise transfer")
        send(beta, alpha, alpha_key, "Windows reply")
        # Exceeds control frame size, exercises binary named-pipe upload plus encrypted TCP.
        text = "Windows bulk text " * 4000
        alpha.cli("send", beta_key, "--stdin", input_text=text)
        wait_for(lambda: any(len(row.split("\t")) == 6 and row.split("\t")[4] == str(len(text.encode("utf-8")))
                             for row in beta.cli("history", "list").splitlines()))
        assert "History on" in beta.cli("history", "status")
        # Windows clipboard operations fail explicitly; network delivery remains available.
        assert beta.run("clipboard", "copy", check=False).returncode != 0
        old_identity = (root / "Alpha-profile" / "identity.dpapi").read_bytes()
        alpha_address = alpha.listener("status")
        beta.cli("devices", "address", alpha_key, alpha_address)
        alpha.stop()
        alpha.env["LYTEBOARD_LISTEN_ADDR"] = alpha_address
        alpha.start()
        assert (root / "Alpha-profile" / "identity.dpapi").read_bytes() == old_identity
        assert alpha.devices()["Beta"][0] == beta_key
        send(beta, alpha, alpha_key, "After Windows restart")
        beta.stop()
        # Inspect the disposable stopped profile to verify exact bulk/history bytes.
        with closing(sqlite3.connect(root / "Beta-profile" / "history" / "history.sqlite3")) as db:
            assert db.execute("SELECT payload FROM entries WHERE sent=0 ORDER BY id DESC LIMIT 1").fetchone()[0] == text.encode("utf-8")
        beta.start()
        assert "text/plain" in beta.cli("history", "list")
        alpha.cli("devices", "remove", beta_key)
        assert alpha.run("send", beta_key, "revoked", check=False).returncode != 0
        beta.cli("history", "clear", "--yes")
        print("Native Windows lifecycle, pairing, bulk transfer, history, restart and revocation passed")


if __name__ == "__main__":
    main()
