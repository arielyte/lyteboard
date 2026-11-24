use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use lyteboard_core::{bulk::Payload, clipboard::Clipboard};
use lyteboard_protocol::{ipc::network::Reply, peer::Media};
use tokio::{
    sync::watch,
    task::{AbortHandle, spawn_local},
    time::Instant,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Fingerprint(Media, [u8; 32]);

impl From<&Payload> for Fingerprint {
    fn from(payload: &Payload) -> Self {
        Self(payload.media(), payload.digest())
    }
}

/// Briefly remembers Lyteboard-owned clipboard writes, including writes made by the CLI.
#[derive(Clone, Default)]
pub struct ClipboardWrites(Arc<Mutex<Recent>>);

type Recent = VecDeque<(Fingerprint, Instant)>;

const REMEMBERED_WRITES: usize = 128;
const ECHO_WINDOW: Duration = Duration::from_secs(120);

impl ClipboardWrites {
    pub fn remember(&self, payload: &Payload) {
        let fingerprint = Fingerprint::from(payload);
        let mut writes = self.0.lock().expect("clipboard writes lock");
        writes.retain(|(previous, _)| *previous != fingerprint);
        writes.push_front((fingerprint, Instant::now() + ECHO_WINDOW));
        writes.truncate(REMEMBERED_WRITES);
    }

    pub(super) fn contains(&self, fingerprint: Fingerprint) -> bool {
        let mut writes = self.0.lock().expect("clipboard writes lock");
        let now = Instant::now();
        writes.retain(|(_, expires)| *expires > now);
        writes.iter().any(|(previous, _)| *previous == fingerprint)
    }
}

#[derive(Default)]
pub(super) struct AutoClipboard(Option<Enabled>);

struct Enabled {
    pending: watch::Sender<Option<Arc<Payload>>>,
    last_error: Rc<RefCell<Option<String>>>,
    task: AbortHandle,
}

impl Drop for Enabled {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AutoClipboard {
    pub(super) fn status(&self) -> Reply {
        Reply::ClipboardAuto {
            enabled: self.0.is_some(),
            last_error: self
                .0
                .as_ref()
                .and_then(|enabled| enabled.last_error.borrow().clone()),
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    pub(super) fn enable(&mut self, clipboard: Clipboard, writes: ClipboardWrites) {
        let (pending, mut incoming) = watch::channel::<Option<Arc<Payload>>>(None);
        let last_error = Rc::new(RefCell::new(None));
        let status = Rc::clone(&last_error);
        let task = spawn_local(async move {
            while incoming.changed().await.is_ok() {
                let payload = incoming.borrow_and_update().clone();
                if let Some(payload) = payload {
                    writes.remember(&payload);
                    let result = clipboard
                        .write_content(payload.media(), payload.bytes())
                        .await;
                    *status.borrow_mut() = result.err().map(|error| error.to_string());
                }
            }
        });
        self.0 = Some(Enabled {
            pending,
            last_error,
            task: task.abort_handle(),
        });
    }

    pub(super) fn disable(&mut self) {
        self.0 = None;
    }

    pub(super) fn received(&self, payload: &Arc<Payload>) {
        if let Some(enabled) = &self.0 {
            // Coalesce selections while a slow owner is still applying a write.
            enabled.pending.send_replace(Some(Arc::clone(payload)));
        }
    }
}
