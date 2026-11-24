#!/usr/bin/env python3
"""Run from the repository root after building both binaries."""

from contextlib import ExitStack
import argparse
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


BIN = Path("target/debug").resolve()


def wait_for(check):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError("timed out waiting for daemon state")


class Device:
    def __init__(self, root, name, keyring_env):
        runtime = root / name
        runtime.mkdir(mode=0o700)
        self.env = dict(keyring_env, XDG_RUNTIME_DIR=str(runtime),
                        LYTEBOARD_PROFILE=str(root / f"{name}-profile"),
                        LYTEBOARD_DEVICE_NAME=name)
        self.env.pop("LYTEBOARD_LISTEN_ADDR", None)
        self.process = None

    def run(self, *arguments, input_text=None, check=True):
        return subprocess.run([BIN / "lyteboard", *arguments], env=self.env,
                              input=input_text, text=True, capture_output=True,
                              check=check, timeout=32)

    def cli(self, *arguments, input_text=None):
        return self.run(*arguments, input_text=input_text).stdout.strip()

    def start(self):
        self.process = subprocess.Popen([BIN / "lyteboardd"], env=self.env,
                                        stdout=subprocess.DEVNULL)
        wait_for(lambda: self.run("ping", check=False).returncode == 0)

    def stop(self):
        if self.process is None:
            return
        try:
            if self.process.poll() is None:
                self.cli("daemon", "stop")
            assert self.process.wait(timeout=5) == 0
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
            self.process = None

    def listener(self, *arguments):
        status = self.cli("network", *arguments)
        return status.split()[2]

    def devices(self):
        rows = self.cli("devices", "list")
        if rows == "No paired devices":
            return {}
        return {name: (key, address, state) for key, name, address, state in
                (line.split("\t") for line in rows.splitlines())}

    def discovery(self):
        return self.cli("network", "discover")

    def online(self, key):
        return any(device == key and state == "online" for device, _, state in self.devices().values())


def pairing_code(device):
    status = device.cli("pair", "status")
    return status.removeprefix("Verification code: ") if status.startswith("Verification code: ") else None


def start_pair(inviter, scanner):
    invitation = inviter.cli("pair", "invite")
    assert invitation.startswith("lyteboard-lan:v1:")
    scanner.cli("pair", "accept", input_text=invitation + "\n")
    return wait_for(lambda: pairing_code(inviter)), wait_for(lambda: pairing_code(scanner))


def pair(inviter, scanner):
    first, second = start_pair(inviter, scanner)
    assert first == second
    scanner.cli("pair", "confirm", input_text=second + "\n")
    assert scanner.cli("pair", "status") != "Paired"
    assert inviter.cli("pair", "status").startswith("Verification code: ")
    inviter.cli("pair", "confirm", input_text=first + "\n")
    for device in (inviter, scanner):
        wait_for(lambda: device.cli("pair", "status") == "Paired")


def send(sender, recipient, key, text):
    wait_for(lambda: sender.online(key))
    assert sender.cli("send", key, text) == "Delivered"
    assert text in recipient.cli("received")


def stop_bus(bus):
    bus.terminate()
    try:
        bus.wait(timeout=5)
    except subprocess.TimeoutExpired:
        bus.kill()
        bus.wait()
    bus.stdout.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--multicast", metavar="LAN_IPV4", help="exercise actual mDNS on this local interface")
    parser.add_argument("--move-to", metavar="LAN_IPV4", help="move one listener to a second local IP on the same LAN")
    args = parser.parse_args()
    if args.move_to and not args.multicast:
        parser.error("--move-to requires --multicast")

    with tempfile.TemporaryDirectory(prefix="lyteboard-lan-cli-") as directory, ExitStack() as cleanup:
        root = Path(directory)
        for name in ("run", "data", "config"):
            (root / name).mkdir(mode=0o700)
        bus = subprocess.Popen(["dbus-daemon", "--session", "--nofork",
                                f"--address=unix:path={root / 'bus'}", "--print-address=1"],
                               stdout=subprocess.PIPE, text=True)
        cleanup.callback(stop_bus, bus)
        address = bus.stdout.readline().strip()
        assert address.startswith(f"unix:path={root / 'bus'}")
        keyring_env = dict(os.environ, DBUS_SESSION_BUS_ADDRESS=address,
                           XDG_RUNTIME_DIR=str(root / "run"),
                           XDG_DATA_HOME=str(root / "data"), XDG_CONFIG_HOME=str(root / "config"))
        keyring_env.pop("GNOME_KEYRING_CONTROL", None)
        subprocess.run(["gnome-keyring-daemon", "--unlock", "--components=secrets",
                        f"--control-directory={root / 'run' / 'keyring'}"],
                       env=keyring_env, input="disposable-lyteboard-test-password",
                       text=True, stdout=subprocess.DEVNULL, check=True, timeout=10)
        alpha, beta, gamma = (Device(root, name, keyring_env) for name in ("Alpha", "Beta", "Gamma"))
        for device in (alpha, beta, gamma):
            cleanup.callback(device.stop)
            device.start()
            assert device.listener("status").startswith("127.0.0.1:")
        assert alpha.cli("daemon", "start") == "Daemon already running"
        host = args.multicast or "127.0.0.1"
        alpha_address, beta_address, gamma_address = (
            device.listener("listen", f"{host}:0") for device in (alpha, beta, gamma)
        )
        if args.multicast:
            for device in (alpha, beta, gamma):
                assert "discovery on" in device.cli("network", "status")
            wait_for(lambda: beta_address in alpha.discovery())
            assert alpha.devices() == {}
            beta.cli("network", "discovery", "off")
            wait_for(lambda: beta_address not in alpha.discovery())
            assert beta.discovery() == "No discovered devices"
            beta.cli("network", "discovery", "on")
            wait_for(lambda: beta_address in alpha.discovery())
        else:
            for device in (alpha, beta, gamma):
                device.cli("network", "discovery", "off")
            assert beta.discovery() == "No discovered devices"
        _, port = alpha_address.split(":")
        with socket.create_connection((host, int(port)), timeout=2) as stalled:
            stalled.sendall(b"\0")
            assert alpha.cli("ping") == "Pong"
            assert alpha.listener("status") == alpha_address
            start_pair(alpha, beta)
            beta.cli("pair", "cancel")
            wait_for(lambda: alpha.run("pair", "status", check=False).returncode != 0)
            assert alpha.devices() == {}
            pair(alpha, beta)
        pair(alpha, gamma)
        peers = alpha.devices()
        assert set(peers) == {"Beta", "Gamma"}
        beta_key, _, _ = peers["Beta"]
        gamma_key, _, _ = peers["Gamma"]
        alpha_key, address, _ = beta.devices()["Alpha"]
        assert address == alpha_address
        assert gamma.devices()["Alpha"][0] == alpha_key

        send(beta, alpha, alpha_key, "Hello from Beta")
        send(alpha, beta, beta_key, "Reply from Alpha")
        alpha.cli("devices", "rename", beta_key, "Renamed Beta")

        # Save another real hint before restarting, allowing both ends to attempt a dial.
        alpha.cli("devices", "address", beta_key, beta_address)
        alpha.stop()
        beta.stop()
        if args.multicast:
            wait_for(lambda: alpha_address not in gamma.discovery())
            beta.env["LYTEBOARD_LISTEN_ADDR"] = beta_address
            alpha.env["LYTEBOARD_LISTEN_ADDR"] = f"{args.move_to or host}:0"
        else:
            alpha.env["LYTEBOARD_LISTEN_ADDR"] = "127.0.0.2:0"
            beta.env["LYTEBOARD_DISCOVERY"] = "off"
            beta.env["LYTEBOARD_LISTEN_ADDR"] = "127.0.0.3:0"
        alpha.start()
        beta.start()
        moved_address = alpha.listener("status")
        assert moved_address != alpha_address
        assert alpha.devices()["Renamed Beta"][0] == beta_key
        assert beta.devices()["Alpha"][0] == alpha_key
        assert "No received messages" == alpha.cli("received")
        if args.multicast:
            wait_for(lambda: moved_address in beta.discovery())
            assert alpha_address not in beta.discovery()
        else:
            beta.cli("devices", "address", alpha_key, gamma_address)
            assert beta.run("send", alpha_key, "wrong key", check=False).returncode != 0
            assert not beta.online(alpha_key)
            beta.cli("devices", "address", alpha_key, moved_address)
            gamma.cli("devices", "address", alpha_key, moved_address)
        wait_for(lambda: beta.online(alpha_key) and alpha.online(beta_key))
        send(beta, alpha, alpha_key, "After restart")
        send(alpha, beta, beta_key, "Reply after restart")
        send(gamma, alpha, alpha_key, "Independent peer")
        assert "(2 active sessions)" in alpha.cli("network", "status")

        alpha.cli("devices", "remove", beta_key)
        assert set(alpha.devices()) == {"Gamma"}
        assert "(1 active sessions)" in alpha.cli("network", "status")
        assert alpha.run("send", beta_key, "locally revoked", check=False).returncode != 0
        wait_for(lambda: "(0 active sessions)" in beta.cli("network", "status"))
        rejected = beta.run("send", alpha_key, "revoked peer", check=False)
        assert rejected.returncode != 0
        assert "revoked peer" not in alpha.cli("received")
        send(alpha, gamma, gamma_key, "Other session still works")

        alpha.stop()
        alpha.env["LYTEBOARD_LISTEN_ADDR"] = moved_address
        alpha.start()
        assert set(alpha.devices()) == {"Gamma"}
        assert beta.run("send", alpha_key, "still revoked", check=False).returncode != 0
        assert alpha.cli("ping") == "Pong"

        alpha.stop()
        subprocess.run(["gdbus", "call", "--session", "--dest", "org.freedesktop.secrets",
                        "--object-path", "/org/freedesktop/secrets", "--method",
                        "org.freedesktop.Secret.Service.Lock",
                        "[objectpath '/org/freedesktop/secrets/collection/login']"],
                       env=keyring_env, check=True, stdout=subprocess.DEVNULL, timeout=10)
        alpha.start()
        locked = alpha.run("network", "status", check=False)
        assert locked.returncode != 0 and "Unlock your desktop keyring" in locked.stderr
        assert alpha.cli("ping") == "Pong"
    mode = "real mDNS multicast" if args.multicast else "mDNS disabled and manual fallback"
    print(f"CLI LAN checks passed with {mode}: pairing, delivery, reconnection, revocation, and shutdown.")


if __name__ == "__main__":
    main()
