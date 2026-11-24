#!/usr/bin/env python3
"""Test automatic clipboard synchronization on three isolated KWin sessions."""

import argparse
from contextlib import ExitStack
from pathlib import Path
import tempfile
import time

from clipboard_cli import KwinSelection, SimulatedSelection, expect_failure, png, stop_kwin
from history_cli import entries
from lan_cli import Device, isolated_keyring, pair, wait_for


def status(device, mode="status"):
    return device.cli("clipboard", "sync", mode).splitlines()


def stable_history(devices):
    before = [entries(device)[0] for device in devices]
    time.sleep(0.85)
    assert [entries(device)[0] for device in devices] == before
    return before


def copied(selection, mime, data):
    try:
        return selection.read(mime) == data
    except AssertionError:
        # KWin's X11 bridge can be between selection owners.
        return False


def exercise(root, cleanup, x11):
    env = isolated_keyring(root, cleanup)
    env.update(LYTEBOARD_HISTORY="on", LYTEBOARD_DISCOVERY="off")
    alpha, beta, gamma = devices = [Device(root, name, env) for name in ("Alpha", "Beta", "Gamma")]
    a_clip, b_clip, c_clip = selections = [
        KwinSelection(root, device, cleanup, x11=x11 and index < 2)
        for index, device in enumerate(devices)
    ]
    for device, clipboard in zip(devices, selections):
        clipboard.seed(b"Already copied before daemon startup", sensitive=False)
        cleanup.callback(device.stop)
        device.start()
        device.env["LYTEBOARD_LISTEN_ADDR"] = device.listener("status")
        assert status(device) == ["Automatic clipboard sending off"]
        assert device.cli("clipboard", "auto") == "Automatic clipboard application off"

    pair(alpha, beta)
    pair(alpha, gamma)
    pair(beta, gamma)
    a_key = beta.devices()["Alpha"][0]
    b_key = alpha.devices()["Beta"][0]
    c_key = alpha.devices()["Gamma"][0]
    keys = (a_key, b_key, c_key)
    for device, own_key in zip(devices, keys):
        for peer in (key for key in keys if key != own_key):
            device.cli("clipboard", "sync", "add", peer)
            wait_for(lambda: device.online(peer))
    assert stable_history(devices) == [[], [], []]
    for device in devices:
        assert status(device, "on")[0] == "Automatic clipboard sending on"
    for device in (beta, gamma):
        device.cli("clipboard", "auto", "on")
    assert stable_history(devices) == [[], [], []]

    value = "Local change ✓ – שלום".encode()
    a_clip.seed(value, sensitive=False)
    wait_for(lambda: value.decode() in beta.cli("received") and value.decode() in gamma.cli("received"))
    wait_for(lambda: copied(b_clip, None, value) and copied(c_clip, None, value))
    records = stable_history(devices)
    assert [len(items) for items in records] == [2, 1, 1]
    assert {item[1] for item in records[0]} == {"sent"}
    assert all(items[0][1] == "received" for items in records[1:])
    a_clip.seed(value, sensitive=False)
    assert stable_history(devices) == records

    wayland_clip = c_clip if x11 else a_clip
    wayland_clip.seed(b"Synthetic sensitive clipboard entry", sensitive=True)
    assert stable_history(devices) == records

    image = png(185, 123)
    a_clip.seed(image, "image/png", sensitive=False)
    wait_for(lambda: copied(b_clip, "image/png", image) and copied(c_clip, "image/png", image))
    records = stable_history(devices)
    assert [len(items) for items in records] == [4, 2, 2]

    beta.cli("clipboard", "auto", "off")
    assert status(beta)[0] == "Automatic clipboard sending on"
    a_clip.seed(b"Manual application stays independent", sensitive=False)
    wait_for(lambda: "Manual application stays independent" in beta.cli("received"))
    assert copied(b_clip, "image/png", image)
    beta.cli("clipboard", "copy")
    wait_for(lambda: copied(b_clip, None, b"Manual application stays independent"))
    records = stable_history(devices)
    assert [len(items) for items in records] == [6, 3, 3]

    gamma.stop()
    wait_for(lambda: not alpha.online(c_key))
    a_clip.seed(b"Do not replay across reconnection", sensitive=False)
    wait_for(lambda: "Do not replay across reconnection" in beta.cli("received"))
    gamma.start()
    assert status(gamma) == ["Automatic clipboard sending on", *sorted((a_key, b_key))]
    assert gamma.cli("clipboard", "auto") == "Automatic clipboard application on"
    wait_for(lambda: alpha.online(c_key))
    assert gamma.cli("received") == "No received messages"
    assert len(stable_history(devices)[2]) == 3

    a_clip.seed(b"Only fresh content after reconnection", sensitive=False)
    wait_for(lambda: "Only fresh content after reconnection" in gamma.cli("received"))
    alpha.cli("devices", "remove", c_key)
    assert c_key not in status(alpha)
    expect_failure(alpha.run("clipboard", "sync", "add", c_key, check=False), "Device is not trusted")
    a_clip.seed(b"Only remaining trusted device", sensitive=False)
    wait_for(lambda: "Only remaining trusted device" in beta.cli("received"))
    assert len(stable_history(devices)[2]) == 4

    assert status(alpha, "off") == ["Automatic clipboard sending off", b_key]
    a_clip.seed(b"Manual while disabled", sensitive=False)
    records = stable_history(devices)
    assert "Manual while disabled" not in beta.cli("received")
    assert alpha.cli("clipboard", "send", b_key) == "Clipboard delivered to 1/1 devices"
    assert len(entries(beta)[0]) == len(records[1]) + 1
    assert status(alpha, "on")[0] == "Automatic clipboard sending on"
    records = stable_history(devices)
    assert [len(items) for items in records] == [11, 7, 4]

    # Losing the graphical session stops its monitor without interrupting IPC or LAN messages.
    stop_kwin(b_clip.process)
    def exposed_error():
        result = beta.run("clipboard", "sync", "status")
        return "sending off" in result.stdout and "monitoring is unavailable" in result.stderr
    wait_for(exposed_error)
    expect_failure(beta.run("clipboard", "sync", "on", check=False), "monitoring is unavailable")
    assert beta.cli("ping") == "Pong"
    assert beta.cli("send", a_key, "Transfer after display loss") == "Delivered"


def unavailable_monitor(root, cleanup):
    env = isolated_keyring(root, cleanup)
    alpha, beta = devices = [Device(root, name, env) for name in ("Alpha", "Beta")]
    a_clip, b_clip = [SimulatedSelection(root, device) for device in devices]
    for device in devices:
        cleanup.callback(device.stop)
        device.start()
    pair(alpha, beta)
    peer = alpha.devices()["Beta"][0]
    wait_for(lambda: alpha.online(peer))
    alpha.cli("clipboard", "sync", "add", peer)
    expect_failure(alpha.run("clipboard", "sync", "on", check=False), "monitoring is unavailable")
    assert status(alpha) == ["Automatic clipboard sending off", peer]
    a_clip.seed(b"Manual clipboard remains available")
    assert beta.cli("received") == "No received messages"
    assert alpha.cli("clipboard", "send", peer) == "Clipboard delivered to 1/1 devices"
    assert beta.cli("clipboard", "copy") == "Copied received text"
    assert b_clip.read() == b"Manual clipboard remains available"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--x11", action="store_true", help="use nested Xwayland for Alpha and Beta")
    mode.add_argument("--simulated", action="store_true", help="test manual fallback without a usable graphical monitor")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="lyteboard-clipboard-sync-") as directory, ExitStack() as cleanup:
        if args.simulated:
            unavailable_monitor(Path(directory), cleanup)
        else:
            exercise(Path(directory), cleanup, args.x11)
    if args.simulated:
        print("Manual fallback passed with simulated clipboard adapters and no graphical monitor.")
    else:
        print("Automatic clipboard checks passed with " + ("KWin Wayland and nested Xwayland." if args.x11 else "KWin Wayland."))


if __name__ == "__main__":
    main()
