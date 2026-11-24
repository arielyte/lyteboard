import { useCallback, useEffect, useState } from "react";
import {
  api,
  discoveryLabels,
  errorMessage,
  type Device,
  type Listener,
  type NearbyDevice,
} from "./api";
import { Dialog } from "./Dialog";

export function ManageDevice({
  device,
  changed,
  close,
}: {
  device: Device;
  changed: (message: string) => void;
  close: () => void;
}) {
  const [name, setName] = useState(device.name);
  const [address, setAddress] = useState(device.address ?? "");
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState("");

  async function save(
    action: () => Promise<void>,
    message: string,
    remove = false,
  ) {
    setBusy(true);
    setError("");
    setSaved("");
    try {
      await action();
      if (remove) close();
      else setSaved(message);
      changed(message);
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      title="Manage device"
      close={() => {
        if (!busy) close();
      }}
    >
      {error && (
        <p className="message danger" role="alert">
          {error}
        </p>
      )}
      {saved && (
        <p className="message success" role="status">
          {saved}
        </p>
      )}
      <form
        className="field-row"
        onSubmit={(event) => {
          event.preventDefault();
          void save(() => api.rename(device.id, name), "Device renamed.");
        }}
      >
        <label>
          Device name
          <input
            maxLength={64}
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        <button disabled={busy || !name.trim()}>Save name</button>
      </form>
      <form
        className="field-row"
        onSubmit={(event) => {
          event.preventDefault();
          void save(
            () => api.setAddress(device.id, address),
            "Peer address saved.",
          );
        }}
      >
        <label>
          Peer IP and port
          <input
            placeholder="192.168.1.10:47851"
            spellCheck={false}
            value={address}
            onChange={(event) => setAddress(event.target.value)}
          />
        </label>
        <button disabled={busy || !address.trim()}>Save address</button>
      </form>
      <p className="muted">
        Lyteboard verifies the device key when it connects.
      </p>
      <div className="key-block">
        <span>Device ID</span>
        <code>{device.id}</code>
      </div>
      <div className="remove-section">
        {confirm ? (
          <>
            <p>
              Remove <strong>{device.name}</strong>? It will need to pair again
              before sharing.
            </p>
            <div className="dialog-actions">
              <button
                disabled={busy}
                onClick={() => setConfirm(false)}
                type="button"
              >
                Keep device
              </button>
              <button
                className="destructive"
                disabled={busy}
                onClick={() =>
                  void save(
                    () => api.remove(device.id),
                    "Device removed.",
                    true,
                  )
                }
                type="button"
              >
                Confirm removal
              </button>
            </div>
          </>
        ) : (
          <button
            className="quiet danger-text"
            disabled={busy}
            onClick={() => setConfirm(true)}
            type="button"
          >
            Remove device
          </button>
        )}
      </div>
    </Dialog>
  );
}

export function DiscoverDevices({
  listener,
  devices,
  close,
  openImport,
  changed,
}: {
  listener: Listener;
  devices: Device[];
  close: () => void;
  openImport: () => void;
  changed: (message: string) => void;
}) {
  const [entries, setEntries] = useState<NearbyDevice[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState("");

  const refresh = useCallback(async () => {
    setBusy(true);
    setError("");
    try {
      setEntries(await api.discover());
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    const initial = window.setTimeout(() => void refresh(), 0);
    return () => window.clearTimeout(initial);
  }, [refresh]);

  async function useAddress(entry: NearbyDevice) {
    setBusy(true);
    setError("");
    setSaved("");
    try {
      await api.setAddress(entry.id, entry.address);
      setSaved("Peer address saved.");
      changed("Peer address saved.");
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog title="Discover devices" close={close}>
      <div className="split-line">
        <p className="muted">
          Discovery {discoveryLabels[listener.discovery].toLowerCase()}
        </p>
        <button disabled={busy} onClick={() => void refresh()} type="button">
          Refresh results
        </button>
      </div>
      {error && (
        <p className="message danger" role="alert">
          {error}
        </p>
      )}
      {saved && (
        <p className="message success" role="status">
          {saved}
        </p>
      )}
      {listener.loopback && (
        <p className="message">
          Set a LAN listener in Settings to discover devices on your network.
        </p>
      )}
      {listener.discovery === "off" && (
        <p className="message">
          Turn on discovery in Settings or enter a paired device’s address
          manually.
        </p>
      )}
      {listener.discovery === "unavailable" && !listener.loopback && (
        <p className="message">
          Check your LAN connection or enter a paired device’s address manually.
        </p>
      )}
      <div aria-live="polite">
        {entries === null && !error && (
          <p className="muted">Reading discovery results…</p>
        )}
        {entries?.length === 0 && !error && (
          <p className="muted">No devices discovered.</p>
        )}
        {!!entries?.length && (
          <ul className="nearby">
            {entries.map((entry) => {
              const known = devices.find((device) => device.id === entry.id);
              return (
                <li key={`${entry.id}/${entry.address}`}>
                  <div>
                    <strong>{known?.name ?? entry.id.slice(0, 12)}</strong>
                    <p className="muted">
                      {entry.address} · Unverified address
                    </p>
                  </div>
                  {entry.paired && (
                    <button
                      disabled={busy}
                      onClick={() => void useAddress(entry)}
                      type="button"
                    >
                      Use address
                    </button>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>
      <p className="muted">Pair new devices using their invitation.</p>
      <div className="dialog-actions">
        <button className="primary" type="button" onClick={openImport}>
          Import invitation
        </button>
      </div>
    </Dialog>
  );
}
