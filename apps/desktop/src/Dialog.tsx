import { useEffect, useId, useRef, type ReactNode } from "react";

export function Dialog({
  title,
  close,
  children,
}: {
  title: string;
  close: () => void;
  children: ReactNode;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const heading = useId();
  useEffect(() => {
    const element = dialog.current;
    element?.showModal();
    return () => element?.close();
  }, []);
  return (
    <dialog
      ref={dialog}
      aria-labelledby={heading}
      onCancel={(event) => {
        event.preventDefault();
        close();
      }}
    >
      <header className="dialog-header">
        <h2 id={heading}>{title}</h2>
        <button
          className="icon-button"
          type="button"
          aria-label={`Close ${title}`}
          onClick={close}
        >
          ×
        </button>
      </header>
      <div className="dialog-content">{children}</div>
    </dialog>
  );
}
