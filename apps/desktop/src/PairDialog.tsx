import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorMessage, type Invitation, type PairState } from "./api";
import { Dialog } from "./Dialog";

export type PairMode = "invite" | "import";

export function PairDialog({
  initialMode,
  loopback,
  close,
  paired,
}: {
  initialMode: PairMode;
  loopback: boolean;
  close: () => void;
  paired: () => void;
}) {
  const [mode, setMode] = useState(initialMode);
  const [invitation, setInvitation] = useState<Invitation | null>(null);
  const [reveal, setReveal] = useState(false);
  const [paste, setPaste] = useState("");
  const [code, setCode] = useState("");
  const [state, setState] = useState<PairState>({ state: "idle" });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [finished, setFinished] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const task = useRef(false);
  const revision = useRef(0);
  const involved = useRef(false);
  const registered = useRef(false);
  const completed = useRef(false);

  const update = useCallback(
    (next: PairState) => {
      setState(next);
      if (next.state !== "idle") {
        setInvitation(null);
        setReveal(false);
      }
      if (next.state !== "compare") setCode("");
      if (next.state === "paired" && involved.current && !completed.current) {
        completed.current = true;
        registered.current = false;
        setFinished(true);
        setPaste("");
        setError("");
        paired();
      }
    },
    [paired],
  );

  useEffect(() => {
    let stopped = false;
    let timer: number;
    const poll = async () => {
      if (
        !task.current &&
        document.visibilityState === "visible" &&
        document.hasFocus()
      ) {
        const current = revision.current;
        try {
          const status = await api.pairStatus();
          if (!stopped && current === revision.current) update(status);
        } catch (failure) {
          if (!stopped && current === revision.current)
            setError(errorMessage(failure));
        }
      }
      if (!stopped) {
        setNow(Date.now());
        timer = window.setTimeout(poll, 700);
      }
    };
    void poll();
    return () => {
      stopped = true;
      window.clearTimeout(timer);
    };
  }, [update]);

  const seconds = invitation
    ? Math.max(0, Math.ceil((invitation.expiresAt * 1000 - now) / 1000))
    : 0;
  useEffect(() => {
    if (!invitation) return;
    const expiry = window.setTimeout(
      () => {
        setInvitation(null);
        setReveal(false);
        setError("This invitation expired. Generate a new one to pair.");
      },
      Math.max(0, invitation.expiresAt * 1000 - Date.now()),
    );
    return () => window.clearTimeout(expiry);
  }, [invitation]);

  async function act(action: () => Promise<void>) {
    if (task.current) return;
    task.current = true;
    revision.current += 1;
    setBusy(true);
    setError("");
    try {
      await action();
    } catch (failure) {
      setError(errorMessage(failure));
    } finally {
      task.current = false;
      setBusy(false);
    }
  }

  function dismiss() {
    if (task.current) return;
    if (registered.current && !completed.current) {
      void act(async () => {
        try {
          await api.cancelPair();
        } finally {
          close();
        }
      });
    } else {
      close();
    }
  }

  const comparing = state.state === "compare";
  const waiting = state.state === "connecting" || state.state === "approving";
  const pending = comparing || waiting;

  return (
    <Dialog title="Pair a device" close={dismiss}>
      {error && (
        <p className="message danger" role="alert">
          {error}
        </p>
      )}
      {finished ? (
        <>
          <p className="message success" role="status">
            Device paired successfully.
          </p>
          <div className="dialog-actions">
            <button className="primary" onClick={close} type="button">
              Done
            </button>
          </div>
        </>
      ) : pending ? (
        <>
          {comparing ? (
            <form
              className="stack"
              onSubmit={(event) => {
                event.preventDefault();
                const entered = code.trim().toUpperCase();
                setCode("");
                void act(async () => {
                  await api.confirmPair(entered);
                  involved.current = true;
                  registered.current = true;
                  update(await api.pairStatus());
                });
              }}
            >
              <p>
                Compare this code on both devices. Approve only if it matches.
              </p>
              <output
                className="verification-code"
                aria-label="Verification code"
              >
                {state.code}
              </output>
              <label>
                Enter the matching code
                <input
                  autoComplete="off"
                  autoCapitalize="characters"
                  maxLength={19}
                  spellCheck={false}
                  value={code}
                  onChange={(event) => setCode(event.target.value)}
                />
              </label>
              <div className="dialog-actions">
                <button
                  className="primary"
                  disabled={busy || code.trim().length !== 19}
                >
                  Confirm match
                </button>
              </div>
            </form>
          ) : (
            <p className="message" role="status">
              {state.state === "connecting"
                ? "Connecting to the other device…"
                : "Local approval recorded. Waiting for the other device…"}
            </p>
          )}
          <button
            className="quiet"
            disabled={busy}
            onClick={() =>
              void act(async () => {
                await api.cancelPair();
                registered.current = false;
                involved.current = false;
                update({ state: "idle" });
              })
            }
            type="button"
          >
            Cancel pairing
          </button>
        </>
      ) : invitation && seconds > 0 ? (
        <>
          <p className="muted">Scan this invitation on the other device.</p>
          <div className="qr-wrap">
            <svg
              role="img"
              aria-label="Pairing invitation QR code"
              viewBox={`0 0 ${invitation.size} ${invitation.size}`}
              shapeRendering="crispEdges"
            >
              <rect
                width={invitation.size}
                height={invitation.size}
                fill="white"
              />
              <path d={invitation.modules} fill="black" />
            </svg>
          </div>
          <p className="muted expiry">
            Expires in {Math.floor(seconds / 60)}:
            {String(seconds % 60).padStart(2, "0")}
          </p>
          <button
            className="quiet"
            type="button"
            onClick={() => setReveal(!reveal)}
          >
            {reveal ? "Hide invitation text" : "Show invitation text"}
          </button>
          {reveal && (
            <label>
              Invitation text
              <textarea
                readOnly
                rows={3}
                value={invitation.text}
                spellCheck={false}
              />
              <small>
                Pause automatic clipboard sending before copying this
                invitation.
              </small>
            </label>
          )}
          <div className="dialog-actions">
            <button disabled={busy} onClick={dismiss} type="button">
              Cancel invitation
            </button>
          </div>
        </>
      ) : (
        <>
          {state.state === "failed" && !error && (
            <p className="message danger" role="alert">
              {state.error}
            </p>
          )}
          <div className="segmented" aria-label="Pairing method">
            <button
              type="button"
              aria-pressed={mode === "invite"}
              onClick={() => {
                setPaste("");
                setError("");
                setMode("invite");
              }}
              disabled={busy}
            >
              Show QR
            </button>
            <button
              type="button"
              aria-pressed={mode === "import"}
              onClick={() => {
                setError("");
                setMode("import");
              }}
              disabled={busy}
            >
              Import invitation
            </button>
          </div>
          {mode === "invite" ? (
            <>
              <p className="muted">
                Create a single-use invitation for the other device.
              </p>
              {loopback && (
                <p className="message">
                  The listener is loopback-only. For another computer, set a LAN
                  address in Settings first.
                </p>
              )}
              <div className="dialog-actions">
                <button
                  className="primary"
                  disabled={busy}
                  onClick={() =>
                    void act(async () => {
                      const next = await api.invite();
                      involved.current = true;
                      registered.current = true;
                      setNow(Date.now());
                      setState({ state: "idle" });
                      setInvitation(next);
                    })
                  }
                  type="button"
                >
                  Generate QR
                </button>
              </div>
            </>
          ) : (
            <form
              className="stack"
              onSubmit={(event) => {
                event.preventDefault();
                const entered = paste.trim();
                setPaste("");
                void act(async () => {
                  await api.importInvitation(entered);
                  involved.current = true;
                  registered.current = true;
                  update(await api.pairStatus());
                });
              }}
            >
              <label>
                Paste invitation
                <input
                  type="password"
                  autoComplete="off"
                  spellCheck={false}
                  value={paste}
                  maxLength={512}
                  onChange={(event) => setPaste(event.target.value)}
                />
              </label>
              <div className="dialog-actions">
                <button className="primary" disabled={busy || !paste.trim()}>
                  Connect
                </button>
              </div>
            </form>
          )}
        </>
      )}
    </Dialog>
  );
}
