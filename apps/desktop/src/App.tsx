import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useRef, useState } from "react";

type DaemonStatus = {
  running: boolean;
  version: string;
  pid: number;
  uptimeSeconds: number;
};

type View =
  | { kind: "checking" }
  | { kind: "offline" }
  | { kind: "connected"; status: DaemonStatus }
  | { kind: "error"; message: string };

async function readStatus(): Promise<View> {
  try {
    const status = await invoke<DaemonStatus | null>("daemon_status");
    return status?.running
      ? { kind: "connected", status }
      : { kind: "offline" };
  } catch (error) {
    return { kind: "error", message: String(error) };
  }
}

export function App() {
  const [view, setView] = useState<View>({ kind: "checking" });
  const request = useRef(0);

  const refresh = useCallback(async () => {
    const current = ++request.current;
    const result = await readStatus();
    if (current === request.current) setView(result);
  }, []);

  useEffect(() => {
    void refresh();
    return () => {
      request.current += 1;
    };
  }, [refresh]);

  return (
    <main>
      <h1>Lyteboard</h1>
      <div aria-live="polite">
        {view.kind === "checking" && <p>Checking daemon status…</p>}
        {view.kind === "offline" && <p>Daemon offline</p>}
        {view.kind === "error" && (
          <>
            <p>Daemon unavailable</p>
            <p>{view.message}</p>
          </>
        )}
        {view.kind === "connected" && (
          <>
            <p>Connected</p>
            <dl>
              <dt>Daemon version</dt>
              <dd>{view.status.version}</dd>
              <dt>Uptime</dt>
              <dd>{view.status.uptimeSeconds}s</dd>
              <dt>Process ID</dt>
              <dd>{view.status.pid}</dd>
            </dl>
          </>
        )}
      </div>
      <button
        type="button"
        onClick={() => {
          setView({ kind: "checking" });
          void refresh();
        }}
        disabled={view.kind === "checking"}
      >
        Refresh status
      </button>
    </main>
  );
}
