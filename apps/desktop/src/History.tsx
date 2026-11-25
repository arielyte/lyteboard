import { useEffect, useRef, useState } from "react";
import {
  api,
  errorMessage,
  type Device,
  type HistoryEntry,
  type HistoryPage,
  type Preview,
} from "./api";
import { Dialog } from "./Dialog";

const date = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short",
});
const mediaLabels = { text: "Text", png: "PNG", jpeg: "JPEG", webp: "WebP" };
type State =
  | { kind: "loading" | "disabled" }
  | { kind: "error"; message: string }
  | { kind: "page"; page: HistoryPage };
type PendingAction =
  { type: "clear" } | { type: "resend"; entry: HistoryEntry };

let previewQueue: Promise<void> = Promise.resolve();

function EntryPreview({ entry }: { entry: HistoryEntry }) {
  const root = useRef<HTMLDivElement>(null);
  const [preview, setPreview] = useState<Preview>();
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let disposed = false;
    const observer = new IntersectionObserver((records) => {
      if (!records.some((record) => record.isIntersecting)) return;
      observer.disconnect();
      previewQueue = previewQueue.then(async () => {
        if (disposed) return;
        try {
          const next = await api.historyPreview(entry.id);
          if (!disposed) setPreview(next);
        } catch {
          if (!disposed) setFailed(true);
        }
      });
    });
    if (root.current) observer.observe(root.current);
    return () => {
      disposed = true;
      observer.disconnect();
    };
  }, [entry.id]);

  return (
    <div className="history-preview" ref={root}>
      {preview?.kind === "image" ? (
        <img
          src={`data:image/png;base64,${preview.data}`}
          alt={`Saved ${mediaLabels[entry.media]} clipboard image`}
        />
      ) : preview?.kind === "text" ? (
        <p>{preview.text}</p>
      ) : (
        <p className="muted">
          {failed ? "Preview unavailable" : "Loading preview…"}
        </p>
      )}
    </div>
  );
}

export function History({
  devices,
  openSettings,
  revision,
}: {
  devices: Device[];
  openSettings: () => void;
  revision: number;
}) {
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  const [cursors, setCursors] = useState<(number | null)[]>([null]);
  const [refresh, setRefresh] = useState(0);
  const [state, setState] = useState<State>({ kind: "loading" });
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [action, setAction] = useState<PendingAction>();
  const cursor = cursors.at(-1) ?? null;

  useEffect(() => {
    let active = true;
    async function load() {
      try {
        const configuration = await api.historyConfiguration();
        if (!active) return;
        if (configuration.error) {
          setState({ kind: "error", message: configuration.error });
        } else if (!configuration.enabled) {
          setState({ kind: "disabled" });
        } else {
          const page = await api.historyPage(cursor, query);
          if (active) setState({ kind: "page", page });
        }
      } catch (failure) {
        if (active) setState({ kind: "error", message: errorMessage(failure) });
      }
    }
    void load();
    return () => {
      active = false;
    };
  }, [cursor, query, refresh, revision]);

  async function mutate(
    operation: () => Promise<void>,
    message: string,
    reload = false,
  ) {
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await operation();
      setNotice(message);
      if (reload) {
        setState({ kind: "loading" });
        setRefresh((value) => value + 1);
      }
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      setBusy(false);
    }
  }

  function showPage(next: (number | null)[]) {
    setState({ kind: "loading" });
    setCursors(next);
  }

  function refreshPage() {
    setState({ kind: "loading" });
    setRefresh((value) => value + 1);
  }

  const entries = state.kind === "page" ? state.page.entries : [];
  return (
    <section className="history-screen" aria-label="Clipboard history">
      {error && (
        <p className="message danger" role="alert">
          {error}
        </p>
      )}
      {notice && (
        <p className="message success" role="status">
          {notice}
        </p>
      )}
      {state.kind === "disabled" ? (
        <div className="empty">
          <h2>Clipboard history is off</h2>
          <p>Enable it in Settings to save future transfers on this device.</p>
          <button type="button" onClick={openSettings}>
            Open Settings
          </button>
        </div>
      ) : state.kind === "error" ? (
        <div className="message danger" role="alert">
          <strong>History unavailable</strong>
          <p>{state.message}</p>
          <button type="button" onClick={refreshPage}>
            Retry history
          </button>
        </div>
      ) : (
        <>
          <div className="history-controls">
            <form
              className="field-row"
              onSubmit={(event) => {
                event.preventDefault();
                setQuery(draft.trim());
                showPage([null]);
                setRefresh((value) => value + 1);
              }}
            >
              <label>
                Search history text
                <input
                  type="search"
                  value={draft}
                  onChange={(event) => setDraft(event.target.value)}
                  maxLength={256}
                />
              </label>
              <button disabled={busy}>Search</button>
            </form>
            <div className="toolbar">
              <button type="button" disabled={busy} onClick={refreshPage}>
                Refresh history
              </button>
              <button
                className="danger-text"
                type="button"
                disabled={busy || state.kind === "loading"}
                onClick={() => {
                  setError("");
                  setAction({ type: "clear" });
                }}
              >
                Clear history
              </button>
            </div>
          </div>
          {state.kind === "loading" ? (
            <p className="muted" role="status">
              Loading history…
            </p>
          ) : !entries.length ? (
            <div className="empty">
              <h2>{query ? "No matching entries" : "No history entries"}</h2>
            </div>
          ) : (
            <ul className="history-list">
              {entries.map((entry) => {
                const time = new Date(entry.timestamp * 1000);
                const peer =
                  devices.find((device) => device.id === entry.peer)?.name ??
                  `Device ${entry.peer.slice(0, 12)}`;
                return (
                  <li key={entry.id} className="history-entry">
                    <div className="history-meta">
                      <strong>
                        {entry.direction === "sent"
                          ? "Sent to"
                          : "Received from"}{" "}
                        {peer}
                      </strong>
                      <time dateTime={time.toISOString()}>
                        {date.format(time)}
                      </time>
                      <span>
                        {mediaLabels[entry.media]} ·{" "}
                        {entry.size.toLocaleString()} bytes
                      </span>
                    </div>
                    <EntryPreview entry={entry} />
                    <div className="row-actions">
                      <button
                        disabled={busy}
                        onClick={() =>
                          void mutate(
                            () => api.restoreHistory(entry.id),
                            "Restored to clipboard.",
                          )
                        }
                        type="button"
                      >
                        Restore
                      </button>
                      <button
                        disabled={busy}
                        onClick={() => setAction({ type: "resend", entry })}
                        type="button"
                      >
                        Resend
                      </button>
                      <button
                        disabled={busy}
                        onClick={() =>
                          void mutate(
                            () => api.deleteHistory(entry.id),
                            "History entry deleted.",
                            true,
                          )
                        }
                        type="button"
                      >
                        Delete
                      </button>
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
          {(cursors.length > 1 ||
            (state.kind === "page" && state.page.more)) && (
            <div
              className="selection-bar history-pagination"
              aria-label="History pages"
            >
              <button
                type="button"
                disabled={busy || cursors.length === 1}
                onClick={() => showPage(cursors.slice(0, -1))}
              >
                Previous
              </button>
              <span className="muted">Page {cursors.length}</span>
              <button
                type="button"
                disabled={busy || state.kind !== "page" || !state.page.more}
                onClick={() => {
                  const last = entries.at(-1);
                  if (last) showPage([...cursors, last.id]);
                }}
              >
                Next
              </button>
            </div>
          )}
        </>
      )}
      {action?.type === "clear" && (
        <Dialog
          title="Clear all history"
          close={() => {
            if (!busy) setAction(undefined);
          }}
        >
          <p>Permanently delete all saved clipboard entries on this device?</p>
          {error && (
            <p className="message danger" role="alert">
              {error}
            </p>
          )}
          <div className="dialog-actions">
            <button
              disabled={busy}
              onClick={() => setAction(undefined)}
              type="button"
            >
              Keep history
            </button>
            <button
              className="destructive"
              disabled={busy}
              onClick={() =>
                void mutate(
                  async () => {
                    await api.clearHistory();
                    setAction(undefined);
                    setDraft("");
                    setQuery("");
                    setCursors([null]);
                  },
                  "History cleared.",
                  true,
                )
              }
              type="button"
            >
              Confirm clear
            </button>
          </div>
        </Dialog>
      )}
      {action?.type === "resend" && (
        <ResendHistory
          entry={action.entry}
          devices={devices}
          close={() => setAction(undefined)}
          sent={(message) => {
            setNotice(message);
            refreshPage();
          }}
        />
      )}
    </section>
  );
}

function ResendHistory({
  entry,
  devices,
  close,
  sent,
}: {
  entry: HistoryEntry;
  devices: Device[];
  close: () => void;
  sent: (message: string) => void;
}) {
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [errors, setErrors] = useState<string[]>([]);
  const online = devices.filter((device) => device.state === "online");
  const allowed = selected.filter((id) =>
    online.some((device) => device.id === id),
  );

  async function resend() {
    setBusy(true);
    setErrors([]);
    try {
      const outcomes = await api.resendHistory(entry.id, allowed);
      const failed = outcomes.filter((outcome) => outcome.error !== null);
      sent(
        `Clipboard delivered to ${outcomes.length - failed.length} of ${outcomes.length} ${outcomes.length === 1 ? "device" : "devices"}.`,
      );
      if (!failed.length) close();
      else {
        setErrors(
          failed.map(
            ({ id, error }) =>
              `${devices.find((device) => device.id === id)?.name ?? id}: ${error}`,
          ),
        );
        setSelected(failed.map(({ id }) => id));
      }
    } catch (failure) {
      setErrors([errorMessage(failure)]);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      title="Resend history entry"
      close={() => {
        if (!busy) close();
      }}
    >
      {online.length ? (
        <div className="preference-checks">
          {online.map((device) => (
            <label className="check-line" key={device.id}>
              <input
                type="checkbox"
                disabled={busy}
                checked={selected.includes(device.id)}
                onChange={(event) => {
                  const checked = event.target.checked;
                  setSelected((previous) =>
                    checked
                      ? [...previous, device.id]
                      : previous.filter((id) => id !== device.id),
                  );
                }}
              />
              {device.name}
            </label>
          ))}
        </div>
      ) : (
        <p className="muted">No paired devices are online.</p>
      )}
      {!!errors.length && (
        <ul className="message danger" role="alert">
          {errors.map((error, i) => (
            <li key={i}>{error}</li>
          ))}
        </ul>
      )}
      <div className="dialog-actions">
        <button
          className="primary"
          type="button"
          disabled={busy || !allowed.length}
          onClick={() => void resend()}
        >
          {busy ? "Resending…" : "Resend to selected"}
        </button>
      </div>
    </Dialog>
  );
}
