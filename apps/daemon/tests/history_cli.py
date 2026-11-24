#!/usr/bin/env python3
"""Verify daemon history over real IPC/TCP with isolated simulated clipboard selections."""

from contextlib import ExitStack
from pathlib import Path
import tempfile
import time

from clipboard_cli import SimulatedSelection, expect_failure, png
from lan_cli import Device, isolated_keyring, pair, wait_for


def entries(device, *args):
    output = device.cli("history", "list", *args)
    if output == "No history entries":
        return [], False
    header, *rows = output.splitlines()
    assert header == "ID\tUNIX TIME\tDIRECTION\tCONTENT TYPE\tBYTES\tDEVICE"
    more = rows[-1].startswith("Next page:")
    if more:
        rows.pop()
    result = []
    for row in rows:
        identifier, timestamp, direction, mime, size, peer = row.split("\t")
        assert abs(int(timestamp) - time.time()) < 60
        result.append((int(identifier), direction, mime, int(size), peer))
    return result, more


def store_path(device):
    return Path(device.env["LYTEBOARD_PROFILE"]) / "history" / "history.sqlite3"


def exercise(root, cleanup):
    env = isolated_keyring(root, cleanup)
    for key in ("LYTEBOARD_HISTORY", "LYTEBOARD_HISTORY_ENTRIES",
                "LYTEBOARD_HISTORY_MIB", "LYTEBOARD_HISTORY_DAYS"):
        env.pop(key, None)
    alpha, beta, gamma = (Device(root, name, env) for name in ("Alpha", "Beta", "Gamma"))
    a_clip = SimulatedSelection(root, alpha, x11=True)
    b_clip = SimulatedSelection(root, beta)
    alpha.env["LYTEBOARD_HISTORY"] = "on"
    for device in (alpha, beta, gamma):
        cleanup.callback(device.stop)
        device.start()
        device.env["LYTEBOARD_LISTEN_ADDR"] = device.listener("status")
    assert alpha.cli("history", "status") == "History on (100 entries, 100 MiB, 7 days)"
    assert beta.cli("history", "status") == "History off"
    expect_failure(beta.run("history", "list", check=False), "History is disabled")
    pair(alpha, beta)
    beta_key = alpha.devices()["Beta"][0]
    alpha_key = beta.devices()["Alpha"][0]
    wait_for(lambda: alpha.online(beta_key) and beta.online(alpha_key))

    text = "Lyteboard 履歴 ✓\n" * 20_000
    assert alpha.cli("send", beta_key, "--stdin", input_text=text) == "Delivered"
    image = png(320, 250)
    b_clip.seed(image, "image/png")
    assert beta.cli("clipboard", "send", alpha_key) == "Clipboard delivered to 1/1 devices"
    assert not store_path(beta).parent.exists()
    expected = [(2, "received", "image/png", len(image), beta_key),
                (1, "sent", "text/plain;charset=utf-8", len(text.encode()), beta_key)]
    assert entries(alpha) == (expected, False)

    bad_key = "ee" * 32
    expect_failure(alpha.run("send", bad_key, "Untrusted", check=False), "Device is not trusted")
    assert entries(alpha) == (expected, False)

    alpha.stop()
    alpha.env.pop("LYTEBOARD_HISTORY")
    alpha.start()
    assert alpha.cli("history", "status") == "History off"
    expect_failure(alpha.run("history", "restore", "1", check=False), "History is disabled")
    wait_for(lambda: beta.online(alpha_key))
    assert beta.cli("send", alpha_key, "Skipped while history is off") == "Delivered"
    alpha.stop()
    alpha.env["LYTEBOARD_HISTORY"] = "on"
    alpha.start()
    assert entries(alpha) == (expected, False)

    assert alpha.cli("history", "restore", "1") == "Restored history entry 1"
    assert a_clip.read() == text.encode()
    assert alpha.cli("history", "restore", "2") == "Restored history entry 2"
    assert a_clip.read("image/png") == image
    assert entries(alpha) == (expected, False)
    wait_for(lambda: alpha.online(beta_key))
    assert alpha.cli("history", "resend", "2", beta_key) == "Clipboard delivered to 1/1 devices"
    assert beta.cli("clipboard", "copy") == "Copied received image"
    assert b_clip.read("image/png") == image
    assert entries(alpha)[0][0] == (3, "sent", "image/png", len(image), beta_key)

    assert alpha.cli("history", "delete", "1") == "Deleted history entry 1"
    expect_failure(alpha.run("history", "restore", "1", check=False), "was not found")
    for _ in range(17):
        assert alpha.cli("send", beta_key, "Pagination test") == "Delivered"
    page, more = entries(alpha)
    assert len(page) == 16 and more
    older, more = entries(alpha, "--before", str(page[-1][0]))
    assert len(older) == 3 and not more and older[-1][0] == 2
    assert alpha.cli("history", "clear", "--yes") == "Cleared history"
    assert entries(alpha) == ([], False)

    pair(alpha, gamma)
    gamma_key = alpha.devices()["Gamma"][0]
    wait_for(lambda: alpha.online(gamma_key))
    a_clip.seed(image, "image/png")
    assert alpha.cli("clipboard", "send", beta_key, gamma_key) == "Clipboard delivered to 2/2 devices"
    sent, more = entries(alpha)
    assert not more and len(sent) == 2
    assert {entry[1:] for entry in sent} == {("sent", "image/png", len(image), key)
                                          for key in (beta_key, gamma_key)}
    assert not store_path(gamma).exists()
    alpha.cli("devices", "remove", gamma_key)
    expect_failure(alpha.run("history", "resend", str(sent[0][0]), gamma_key, check=False),
                   "Device is not trusted")
    assert entries(alpha) == (sent, False)

    # Bad history must be visible to the CLI and independent of IPC and transfer success.
    alpha.stop()
    path = store_path(alpha)
    path.chmod(0o644)
    alpha.start()
    expect_failure(alpha.run("history", "status", check=False), "History storage is unavailable")
    assert alpha.cli("ping") == "Pong"
    alpha.stop()
    path.chmod(0o600)
    invalid = b"test fixture is not SQLite"
    path.write_bytes(invalid)
    alpha.start()
    expect_failure(alpha.run("history", "list", check=False), "History storage is unavailable")
    wait_for(lambda: beta.online(alpha_key))
    assert beta.cli("send", alpha_key, "Clipboard stays usable") == "Delivered"
    assert "Clipboard stays usable" in alpha.cli("received")
    assert path.read_bytes() == invalid


def main():
    with tempfile.TemporaryDirectory(prefix="lyteboard-history-") as directory, ExitStack() as cleanup:
        exercise(Path(directory), cleanup)
    print("Persistent clipboard history CLI checks passed.")


if __name__ == "__main__":
    main()
