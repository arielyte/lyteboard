import { useEffect, useState } from "react";
import {
  api,
  discoveryLabels,
  errorMessage,
  type Device,
  type ClipboardPreferences,
  type DesktopConfiguration,
  type HistoryConfiguration,
  type HistoryLimits,
} from "./api";
import type { Connection, Theme } from "./App";

export function Settings({
  theme,
  setTheme,
  connection,
  changed,
  daemonBusy,
  manageDaemon,
  desktop,
  setDesktop,
  revision,
}: {
  theme: Theme;
  setTheme: (theme: Theme) => void;
  connection: Connection;
  changed: (message: string) => void;
  daemonBusy: boolean;
  manageDaemon: (start: boolean) => Promise<void>;
  desktop: DesktopConfiguration | undefined;
  setDesktop: (configuration: DesktopConfiguration) => void;
  revision: number;
}) {
  const listener =
    connection.state === "connected" ? connection.network?.listener : undefined;
  const [editing, setEditing] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function save(action: () => Promise<unknown>, message: string) {
    setBusy(true);
    setError("");
    try {
      await action();
      setEditing(null);
      changed(message);
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="settings">
      <section className="setting-group">
        <h2>Appearance</h2>
        <label className="setting-line">
          Theme
          <select
            value={theme}
            onChange={(event) => setTheme(event.target.value as Theme)}
          >
            <option value="system">System</option>
            <option value="light">Light</option>
            <option value="dark">Dark</option>
          </select>
        </label>
      </section>
      <section className="setting-group">
        <h2>Daemon</h2>
        <div className="setting-line">
          <span>
            {connection.state === "connected"
              ? "Running"
              : connection.state === "offline"
                ? "Offline"
                : connection.state === "checking"
                  ? "Checking…"
                  : "Unavailable"}
          </span>
          <button
            type="button"
            disabled={
              daemonBusy ||
              connection.state === "checking" ||
              connection.state === "error"
            }
            onClick={() => void manageDaemon(connection.state !== "connected")}
          >
            {daemonBusy
              ? "Working…"
              : connection.state === "connected"
                ? "Stop daemon"
                : "Start daemon"}
          </button>
        </div>
        {connection.state === "connected" && (
          <>
            <div className="setting-line">
              <span className="muted">Version</span>
              <span>{connection.daemon.version}</span>
            </div>
            <div className="setting-line">
              <span className="muted">Process ID</span>
              <span>{connection.daemon.pid}</span>
            </div>
          </>
        )}
      </section>
      <DesktopControls configuration={desktop} update={setDesktop} />
      {connection.state === "connected" && (
        <SavedPreferences
          devices={connection.network?.devices ?? []}
          revision={revision}
        />
      )}
      {connection.state === "connected" && (
        <section className="setting-group">
          <h2>Local network</h2>
          {error && (
            <p className="message danger" role="alert">
              {error}
            </p>
          )}
          <form
            className="field-row"
            onSubmit={(event) => {
              event.preventDefault();
              void save(
                () => api.setListener(editing ?? listener?.address ?? ""),
                "LAN listener updated.",
              );
            }}
          >
            <label>
              Listener IP and port
              <input
                value={editing ?? listener?.address ?? ""}
                placeholder="192.168.1.10:47851"
                spellCheck={false}
                onChange={(event) => setEditing(event.target.value)}
              />
            </label>
            <button disabled={busy || !editing?.trim()}>Apply listener</button>
          </form>
          <p className="muted">
            For another computer, enter this device’s LAN IP and an available
            port. This address is saved for the next daemon start.
          </p>
          {listener && (
            <div className="setting-line">
              <div>
                Discovery{" "}
                <p className="muted">{discoveryLabels[listener.discovery]}</p>
              </div>
              <button
                disabled={busy}
                type="button"
                onClick={() => {
                  const enabled =
                    listener.discovery !== "running" &&
                    listener.discovery !== "loopback";
                  void save(
                    () => api.setDiscovery(enabled),
                    enabled ? "Discovery enabled." : "Discovery disabled.",
                  );
                }}
              >
                {listener.discovery === "running" ||
                listener.discovery === "loopback"
                  ? "Turn off discovery"
                  : "Turn on discovery"}
              </button>
            </div>
          )}
        </section>
      )}
      <section className="setting-group">
        <h2>Privacy</h2>
        <ul className="privacy-list">
          <li>
            Clipboard transfers are end-to-end encrypted between paired devices.
          </li>
          <li>Local clipboard history is stored without encryption at rest.</li>
          <li>
            Automatic sharing can include sensitive clipboard content, including
            passwords that apps leave unmarked.
          </li>
          <li>Lyteboard communicates only on your local network.</li>
        </ul>
      </section>
    </div>
  );
}

function DesktopControls({
  configuration,
  update,
}: {
  configuration?: DesktopConfiguration;
  update: (configuration: DesktopConfiguration) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  async function save(action: () => Promise<DesktopConfiguration>) {
    setBusy(true);
    setError("");
    try {
      update(await action());
    } catch (error) {
      setError(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="setting-group">
      <h2>Desktop</h2>
      {(error || configuration?.error) && (
        <p className="message danger" role="alert">
          {error || configuration?.error}
        </p>
      )}
      {configuration && (
        <>
          {(
            [
              ["startDaemon", "Start daemon when Lyteboard opens"],
              ["minimizeToTray", "Minimize to tray"],
              ["closeToTray", "Close to tray"],
            ] as const
          ).map(([name, label]) => {
            const enabled = configuration.preferences[name];
            return (
              <label className="setting-line preference-toggle" key={name}>
                <span>{label}</span>
                <input
                  type="checkbox"
                  checked={enabled}
                  disabled={
                    busy ||
                    (!configuration.trayAvailable &&
                      name !== "startDaemon" &&
                      !enabled)
                  }
                  onChange={(event) => {
                    const checked = event.target.checked;
                    void save(() =>
                      api.setDesktopPreferences({
                        ...configuration.preferences,
                        [name]: checked,
                      }),
                    );
                  }}
                />
              </label>
            );
          })}
          <label className="setting-line preference-toggle">
            <span>Open Lyteboard at login</span>
            <input
              type="checkbox"
              checked={configuration.startsAtLogin}
              disabled={busy}
              onChange={(event) => {
                const checked = event.target.checked;
                void save(() => api.setLoginStartup(checked));
              }}
            />
          </label>
          <p className="muted">
            {configuration.trayAvailable
              ? "The tray sends to the devices selected on the Devices screen."
              : "No system tray is available in this session. The daemon runs independently of this window."}
          </p>
        </>
      )}
    </section>
  );
}

function SavedPreferences({
  devices,
  revision,
}: {
  devices: Device[];
  revision: number;
}) {
  const [clipboard, setClipboard] = useState<ClipboardPreferences>();
  const [history, setHistory] = useState<HistoryConfiguration>();
  const [clipboardError, setClipboardError] = useState("");
  const [historyError, setHistoryError] = useState("");
  const [editing, setEditing] = useState<
    Partial<Record<keyof HistoryLimits, string>>
  >({});
  const [clipboardBusy, setClipboardBusy] = useState(false);
  const [historyBusy, setHistoryBusy] = useState(false);
  const [historyNotice, setHistoryNotice] = useState("");
  const [refresh, setRefresh] = useState(0);

  useEffect(() => {
    let active = true;
    void api.clipboardPreferences().then(
      (settings) => {
        if (active) setClipboard(settings);
      },
      (error) => {
        if (active) setClipboardError(errorMessage(error));
      },
    );
    void api.historyConfiguration().then(
      (settings) => {
        if (active) setHistory(settings);
      },
      (error) => {
        if (active) setHistoryError(errorMessage(error));
      },
    );
    return () => {
      active = false;
    };
  }, [refresh, revision]);

  async function changeClipboard(
    operation: () => Promise<ClipboardPreferences>,
  ) {
    setClipboardBusy(true);
    setClipboardError("");
    try {
      setClipboard(await operation());
    } catch (error) {
      setClipboardError(errorMessage(error));
      try {
        setClipboard(await api.clipboardPreferences());
      } catch {
        // Keep the previous status until the daemon responds again.
      }
    } finally {
      setClipboardBusy(false);
    }
  }

  async function changeHistory(
    enabled: boolean,
    limits: HistoryLimits,
    message: string,
  ) {
    setHistoryBusy(true);
    setHistoryError("");
    setHistoryNotice("");
    try {
      const saved = await api.configureHistory(enabled, limits);
      setHistory(saved);
      setEditing({});
      if (!saved.error) setHistoryNotice(message);
    } catch (error) {
      setHistoryError(errorMessage(error));
      try {
        setHistory(await api.historyConfiguration());
      } catch {
        // Keep the previous status until the daemon responds again.
      }
    } finally {
      setHistoryBusy(false);
    }
  }

  function retry() {
    setClipboardError("");
    setHistoryError("");
    setRefresh((value) => value + 1);
  }

  return (
    <>
      <section className="setting-group">
        <h2>Clipboard synchronization</h2>
        {clipboardError && (
          <p className="message danger" role="alert">
            {clipboardError}
          </p>
        )}
        {clipboard ? (
          <>
            <label className="setting-line preference-toggle">
              <span>Automatic sending</span>
              <input
                type="checkbox"
                checked={clipboard.send}
                disabled={
                  clipboardBusy || (!clipboard.send && !clipboard.peers.length)
                }
                onChange={(event) => {
                  const enabled = event.target.checked;
                  void changeClipboard(() => api.setClipboardSending(enabled));
                }}
              />
            </label>
            {clipboard.paused && (
              <div className="setting-line">
                <p className="muted">Automatic synchronization is paused.</p>
                <button
                  type="button"
                  disabled={clipboardBusy}
                  onClick={() =>
                    void changeClipboard(() => api.setClipboardPause(false))
                  }
                >
                  Resume synchronization
                </button>
              </div>
            )}
            {!clipboard.paused && clipboard.send && !clipboard.send_active && (
              <p className="muted">
                Saved on. Clipboard monitoring is inactive.
              </p>
            )}
            {clipboard.send_error && (
              <p className="message danger" role="alert">
                {clipboard.send_error}
              </p>
            )}
            <label className="setting-line preference-toggle">
              <span>Automatically apply received content</span>
              <input
                type="checkbox"
                checked={clipboard.apply}
                disabled={clipboardBusy}
                onChange={(event) => {
                  const enabled = event.target.checked;
                  void changeClipboard(() =>
                    api.setClipboardApplication(enabled),
                  );
                }}
              />
            </label>
            <p className="muted">
              Incoming text and images replace your local clipboard when
              enabled.
            </p>
            {!clipboard.paused &&
              clipboard.apply &&
              !clipboard.apply_active && (
                <p className="muted">
                  Saved on. Automatic application is inactive.
                </p>
              )}
            {clipboard.apply_error && (
              <p className="message danger" role="alert">
                {clipboard.apply_error}
              </p>
            )}
            <div
              className="preference-checks"
              role="group"
              aria-label="Automatic sharing recipients"
            >
              <strong>Automatic sharing recipients</strong>
              {devices.length ? (
                devices.map((device) => (
                  <label key={device.id} className="check-line">
                    <input
                      type="checkbox"
                      checked={clipboard.peers.includes(device.id)}
                      disabled={clipboardBusy}
                      onChange={(event) => {
                        const enabled = event.target.checked;
                        void changeClipboard(() =>
                          api.setClipboardRecipient(device.id, enabled),
                        );
                      }}
                    />
                    {device.name}
                  </label>
                ))
              ) : (
                <p className="muted">
                  Pair a device to select it for automatic sharing.
                </p>
              )}
            </div>
          </>
        ) : clipboardError ? (
          <button className="preference-retry" type="button" onClick={retry}>
            Retry settings
          </button>
        ) : (
          <p className="muted">Loading clipboard settings…</p>
        )}
      </section>
      <section className="setting-group">
        <h2>Clipboard history</h2>
        {historyError && (
          <p className="message danger" role="alert">
            {historyError}
          </p>
        )}
        {history?.error && (
          <p className="message danger" role="alert">
            {history.error}
          </p>
        )}
        {historyNotice && (
          <p className="message success" role="status">
            {historyNotice}
          </p>
        )}
        {history ? (
          <>
            <label className="setting-line preference-toggle">
              <span>Persistent clipboard history</span>
              <input
                type="checkbox"
                checked={history.enabled}
                disabled={historyBusy}
                onChange={(event) => {
                  const enabled = event.target.checked;
                  void changeHistory(
                    enabled,
                    history.limits,
                    enabled ? "History enabled." : "History disabled.",
                  );
                }}
              />
            </label>
            <form
              className="stack"
              onSubmit={(event) => {
                event.preventDefault();
                void changeHistory(
                  history.enabled,
                  {
                    entries: Number(editing.entries ?? history.limits.entries),
                    mib: Number(editing.mib ?? history.limits.mib),
                    days: Number(editing.days ?? history.limits.days),
                  },
                  "Retention saved.",
                );
              }}
            >
              <div className="retention-fields">
                {(
                  [
                    ["entries", "Maximum entries", 10_000],
                    ["mib", "Maximum MiB", 1024],
                    ["days", "Maximum days", 3650],
                  ] as const
                ).map(([name, label, maximum]) => (
                  <label key={name}>
                    {label}
                    <input
                      type="number"
                      min={1}
                      max={maximum}
                      required
                      value={editing[name] ?? history.limits[name]}
                      onChange={(event) => {
                        const value = event.target.value;
                        setEditing((current) => ({
                          ...current,
                          [name]: value,
                        }));
                      }}
                    />
                  </label>
                ))}
              </div>
              <p className="muted">
                Lower limits can remove older entries immediately while history
                is enabled.
              </p>
              <button
                className="preference-retry"
                disabled={
                  historyBusy ||
                  (!Object.keys(editing).length && !history.error)
                }
              >
                Save retention
              </button>
            </form>
          </>
        ) : historyError ? (
          <button className="preference-retry" type="button" onClick={retry}>
            Retry history settings
          </button>
        ) : (
          <p className="muted">Loading history settings…</p>
        )}
      </section>
    </>
  );
}
