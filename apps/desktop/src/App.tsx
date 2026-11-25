import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  errorMessage,
  type DaemonStatus,
  type Device,
  type DevicesView,
  type DesktopConfiguration,
  type DesktopNotice,
} from "./api";
import { DiscoverDevices, ManageDevice } from "./DeviceDialogs";
import { History } from "./History";
import { Settings } from "./Settings";
import { PairDialog, type PairMode } from "./PairDialog";

type Page = "Devices" | "History" | "Settings";
export type Theme = "system" | "light" | "dark";
type Modal =
  | { kind: "pair"; mode: PairMode }
  | { kind: "discover" }
  | { kind: "manage"; device: Device };
export type Connection =
  | { state: "checking" | "offline" }
  | { state: "error"; error: string }
  | {
      state: "connected";
      daemon: DaemonStatus;
      network?: DevicesView;
      error?: string;
    };
type Notice = { message: string; error?: boolean; details?: string[] };
const pages: Page[] = ["Devices", "History", "Settings"];

function storedTheme(): Theme {
  try {
    const saved = localStorage.getItem("lyteboard-theme");
    return saved === "light" || saved === "dark" ? saved : "system";
  } catch {
    return "system";
  }
}

export function App() {
  const [page, setPage] = useState<Page>("Devices");
  const [theme, setTheme] = useState<Theme>(storedTheme);
  const [connection, setConnection] = useState<Connection>({
    state: "checking",
  });
  const [refreshing, setRefreshing] = useState(false);
  const [modal, setModal] = useState<Modal | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [sending, setSending] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [desktop, setDesktop] = useState<DesktopConfiguration>();
  const [daemonBusy, setDaemonBusy] = useState(false);
  const [viewEpoch, setViewEpoch] = useState(0);
  const dirty = useRef(false);
  const revision = useRef(0);
  const pending = useRef(false);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    try {
      localStorage.setItem("lyteboard-theme", theme);
    } catch {
      // Appearance still applies for this window.
    }
  }, [theme]);

  const refresh = useCallback(async () => {
    const current = ++revision.current;
    pending.current = true;
    setRefreshing(true);
    let next: Connection;
    try {
      const daemon = await api.daemon();
      if (daemon?.running) {
        try {
          next = { state: "connected", daemon, network: await api.devices() };
        } catch (error) {
          next = { state: "connected", daemon, error: errorMessage(error) };
        }
      } else {
        next = { state: "offline" };
      }
    } catch (error) {
      next = { state: "error", error: errorMessage(error) };
    }
    if (current === revision.current) {
      const available =
        next.state === "connected"
          ? new Set(
              next.network?.devices
                .filter((device) => device.state === "online")
                .map((device) => device.id),
            )
          : new Set<string>();
      setSelected((previous) => {
        const next = previous.filter((id) => available.has(id));
        return next.length === previous.length ? previous : next;
      });
      setConnection(next);
      setRefreshing(false);
      pending.current = false;
    }
  }, []);

  useEffect(() => {
    const initial = window.setTimeout(() => void refresh(), 0);
    const visibleRefresh = () => {
      if (
        document.visibilityState === "visible" &&
        document.hasFocus() &&
        !pending.current
      ) {
        if (dirty.current) {
          dirty.current = false;
          setViewEpoch((value) => value + 1);
        }
        void refresh();
      }
    };
    let active = true;
    const loadDesktop = async (initial = false) => {
      try {
        const config = await api.desktopConfiguration();
        if (!active) return;
        setDesktop(config);
        if (initial && config.lastNotice) setNotice(config.lastNotice);
      } catch (error) {
        if (active) setNotice({ message: errorMessage(error), error: true });
      }
    };
    const listeners = [
      listen("desktop-changed", () => {
        if (!active) return;
        void loadDesktop();
        visibleRefresh();
      }),
      listen<DesktopNotice>("desktop-notice", ({ payload }) => {
        if (!active) return;
        setNotice(payload);
        dirty.current = true;
      }),
    ];
    void loadDesktop(true);
    const timer = window.setInterval(visibleRefresh, 5000);
    window.addEventListener("focus", visibleRefresh);
    document.addEventListener("visibilitychange", visibleRefresh);
    const closeShortcut = (event: KeyboardEvent) => {
      if (event.ctrlKey && !event.altKey && event.key.toLowerCase() === "w") {
        event.preventDefault();
        void getCurrentWindow()
          .close()
          .catch((error) => {
            if (active)
              setNotice({ message: errorMessage(error), error: true });
          });
      }
    };
    window.addEventListener("keydown", closeShortcut);
    return () => {
      active = false;
      for (const listener of listeners)
        void listener.then((unlisten) => unlisten()).catch(() => {});
      revision.current += 1;
      pending.current = false;
      window.clearTimeout(initial);
      window.clearInterval(timer);
      window.removeEventListener("focus", visibleRefresh);
      window.removeEventListener("keydown", closeShortcut);
      document.removeEventListener("visibilitychange", visibleRefresh);
    };
  }, [refresh]);

  useEffect(() => {
    void api.selectTrayRecipients(selected).catch((error) => {
      setNotice({ message: errorMessage(error), error: true });
    });
  }, [selected]);

  async function manageDaemon(start: boolean) {
    setDaemonBusy(true);
    setNotice(null);
    try {
      await (start ? api.startDaemon() : api.stopDaemon());
    } catch (error) {
      setNotice({ message: errorMessage(error), error: true });
    } finally {
      setDaemonBusy(false);
      void refresh();
    }
  }

  async function minimize() {
    try {
      await api.minimizeToTray();
    } catch (error) {
      setNotice({ message: errorMessage(error), error: true });
    }
  }

  const changed = useCallback(
    (message: string) => {
      setNotice({ message });
      void refresh();
    },
    [refresh],
  );
  const paired = useCallback(
    () => changed("Device paired successfully."),
    [changed],
  );
  const network =
    connection.state === "connected" ? connection.network : undefined;
  const deviceList = network?.devices ?? [];
  const online = deviceList.filter((device) => device.state === "online");
  const connected = connection.state === "connected";

  async function send(recipients: string[]) {
    const names = new Map(deviceList.map(({ id, name }) => [id, name]));
    setSending(true);
    setNotice(null);
    try {
      const outcomes = await api.send(recipients);
      const failures = outcomes.filter((outcome) => outcome.error !== null);
      const delivered = outcomes.length - failures.length;
      setNotice({
        message: `Clipboard delivered to ${delivered} of ${outcomes.length} ${outcomes.length === 1 ? "device" : "devices"}.`,
        error: failures.length > 0,
        details: failures.map(
          ({ id, error }) => `${names.get(id) ?? id}: ${error}`,
        ),
      });
      if (failures.length === 0) setSelected([]);
    } catch (error) {
      setNotice({ message: errorMessage(error), error: true });
    } finally {
      setSending(false);
      setViewEpoch((value) => value + 1);
      void refresh();
    }
  }

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="mark" aria-hidden="true" />
          Lyteboard
        </div>
        <nav aria-label="Main navigation">
          {pages.map((item) => (
            <button
              key={item}
              type="button"
              aria-current={page === item ? "page" : undefined}
              onClick={() => {
                setPage(item);
                setNotice(null);
              }}
            >
              {item}
            </button>
          ))}
        </nav>
        <div className="daemon-state" aria-live="polite">
          <span
            className={`dot ${connected ? "online" : connection.state === "checking" ? "connecting" : "offline"}`}
          />
          {connected
            ? "Daemon running"
            : connection.state === "checking"
              ? "Checking daemon"
              : connection.state === "offline"
                ? "Daemon offline"
                : "Daemon unavailable"}
        </div>
      </aside>

      <main>
        <header className="page-header">
          <h1>{page}</h1>
          <div className="toolbar">
            {desktop?.trayAvailable && desktop.preferences.minimizeToTray && (
              <button
                className="icon-button"
                aria-label="Minimize to tray"
                title="Minimize to tray"
                type="button"
                onClick={() => void minimize()}
              >
                −
              </button>
            )}
            <button
              className={page === "Devices" && network ? "icon-button" : ""}
              aria-label="Refresh daemon and devices"
              title="Refresh daemon and devices"
              disabled={refreshing}
              onClick={() => void refresh()}
              type="button"
            >
              {page === "Devices" && network ? (
                <span aria-hidden="true">↻</span>
              ) : (
                "Refresh"
              )}
            </button>
            {page === "Devices" && network && (
              <>
                <button
                  onClick={() => setModal({ kind: "discover" })}
                  type="button"
                >
                  Discover
                </button>
                <button
                  className="primary"
                  onClick={() => setModal({ kind: "pair", mode: "invite" })}
                  type="button"
                >
                  Pair device
                </button>
              </>
            )}
          </div>
        </header>

        {connection.state === "offline" && page !== "Settings" && (
          <div className="message offline-message" role="status">
            <div className="setting-line">
              <div>
                <strong>Lyteboard daemon is offline</strong>
                <p>Start it to connect to your devices.</p>
              </div>
              <button
                className="primary"
                disabled={daemonBusy}
                onClick={() => void manageDaemon(true)}
                type="button"
              >
                {daemonBusy ? "Starting…" : "Start daemon"}
              </button>
            </div>
          </div>
        )}
        {connection.state === "error" && (
          <p className="message danger" role="alert">
            {connection.error}
          </p>
        )}
        {connection.state === "connected" && connection.error && (
          <div className="message danger" role="alert">
            <strong>LAN controls unavailable</strong>
            <p>{connection.error}</p>
          </div>
        )}

        {notice && (
          <div
            className={`message ${notice.error ? "danger" : "success"}`}
            role={notice.error ? "alert" : "status"}
          >
            <strong>{notice.message}</strong>
            {!!notice.details?.length && (
              <ul>
                {notice.details.map((line, index) => (
                  <li key={index}>{line}</li>
                ))}
              </ul>
            )}
          </div>
        )}

        {page === "Devices" && (
          <>
            {connection.state === "checking" && (
              <p className="muted">Checking daemon status…</p>
            )}
            {network &&
              (deviceList.length === 0 ? (
                <section className="empty">
                  <span className="device-icon" aria-hidden="true" />
                  <h2>No paired devices</h2>
                  <p>
                    Pair another device to send your clipboard over the local
                    network.
                  </p>
                </section>
              ) : (
                <section aria-label="Paired devices">
                  <div className="selection-bar">
                    <button
                      className="quiet"
                      disabled={online.length === 0 || sending}
                      onClick={() =>
                        setSelected(
                          selected.length === online.length
                            ? []
                            : online.map(({ id }) => id),
                        )
                      }
                      type="button"
                    >
                      {online.length > 0 && selected.length === online.length
                        ? "Clear selection"
                        : "Select online"}
                    </button>
                    <button
                      className="primary"
                      disabled={!selected.length || sending}
                      onClick={() => void send(selected)}
                      type="button"
                    >
                      {sending
                        ? "Sending…"
                        : selected.length
                          ? `Send to selected (${selected.length})`
                          : "Send to selected"}
                    </button>
                  </div>
                  <ul className="device-list">
                    {deviceList.map((device) => (
                      <li className="device-row" key={device.id}>
                        <input
                          type="checkbox"
                          aria-label={`Select ${device.name}`}
                          checked={selected.includes(device.id)}
                          disabled={device.state !== "online" || sending}
                          onChange={(event) => {
                            const checked = event.target.checked;
                            setSelected((previous) =>
                              checked
                                ? [...previous, device.id]
                                : previous.filter((id) => id !== device.id),
                            );
                          }}
                        />
                        <span className="device-icon" aria-hidden="true" />
                        <div className="device-label">
                          <strong>{device.name}</strong>
                          <span
                            className="muted"
                            title={device.address ?? undefined}
                          >
                            {device.address ?? "No saved address"}
                          </span>
                        </div>
                        <span className={`peer-state ${device.state}`}>
                          <span className={`dot ${device.state}`} />
                          {device.state}
                        </span>
                        <div className="row-actions">
                          <button
                            disabled={device.state !== "online" || sending}
                            aria-label={`Send clipboard to ${device.name}`}
                            onClick={() => void send([device.id])}
                            type="button"
                          >
                            Send
                          </button>
                          <button
                            aria-label={`Manage ${device.name}`}
                            onClick={() => setModal({ kind: "manage", device })}
                            type="button"
                          >
                            Manage
                          </button>
                        </div>
                      </li>
                    ))}
                  </ul>
                </section>
              ))}
          </>
        )}

        {page === "History" && connection.state === "connected" && (
          <History
            key={connection.daemon.pid}
            devices={deviceList}
            revision={viewEpoch}
            openSettings={() => setPage("Settings")}
          />
        )}

        {page === "Settings" && (
          <Settings
            key={
              connection.state === "connected"
                ? connection.daemon.pid
                : connection.state
            }
            theme={theme}
            setTheme={setTheme}
            connection={connection}
            changed={changed}
            daemonBusy={daemonBusy}
            manageDaemon={manageDaemon}
            desktop={desktop}
            setDesktop={setDesktop}
            revision={viewEpoch}
          />
        )}
      </main>
      {modal?.kind === "manage" && (
        <ManageDevice
          device={modal.device}
          changed={changed}
          close={() => setModal(null)}
        />
      )}
      {modal?.kind === "discover" && network && (
        <DiscoverDevices
          listener={network.listener}
          devices={deviceList}
          changed={changed}
          close={() => setModal(null)}
          openImport={() => setModal({ kind: "pair", mode: "import" })}
        />
      )}
      {modal?.kind === "pair" && (
        <PairDialog
          initialMode={modal.mode}
          loopback={network?.listener.loopback ?? false}
          paired={paired}
          close={() => setModal(null)}
        />
      )}
    </div>
  );
}
