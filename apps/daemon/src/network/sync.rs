use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};

use lyteboard_core::{
    bulk::Payload,
    clipboard::{Error, monitor::Monitor},
};
use lyteboard_protocol::{
    ipc::network::{MAX_RECIPIENTS, Reply},
    peer::NoiseKey,
};
use tokio::{
    task::{AbortHandle, spawn_local},
    time::{Instant, sleep},
};

use super::{
    Activity, Shared,
    clipboard::{ClipboardWrites, Fingerprint},
};

const QUIET_PERIOD: Duration = Duration::from_millis(200);
const MAX_AGE: Duration = Duration::from_secs(4);

#[derive(Default)]
pub(super) struct Sync {
    recipients: BTreeMap<NoiseKey, Recipient>,
    running: Option<Running>,
}

#[derive(Clone)]
struct Recipient {
    token: Rc<()>,
    since: Instant,
}

struct Running {
    task: AbortHandle,
    last_error: Rc<RefCell<Option<String>>>,
    _token: Rc<Cell<bool>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Permission to start this event on a session that was already authenticated.
pub(super) struct Automatic {
    pub observed: Instant,
    run: Weak<Cell<bool>>,
    recipient: Weak<()>,
}

impl Automatic {
    pub fn is_current(&self) -> bool {
        Instant::now() < self.observed + MAX_AGE
            && self.run.upgrade().is_some_and(|run| run.get())
            && self.recipient.upgrade().is_some()
    }
}

impl Sync {
    pub fn set_recipient(&mut self, key: NoiseKey) -> Result<(), String> {
        if self.recipients.len() >= MAX_RECIPIENTS && !self.recipients.contains_key(&key) {
            return Err("Select at most 32 automatic clipboard recipients.".into());
        }
        self.recipients.entry(key).or_insert_with(|| Recipient {
            token: Rc::new(()),
            since: Instant::now(),
        });
        Ok(())
    }

    pub fn remove(&mut self, key: NoiseKey) {
        self.recipients.remove(&key);
        if self.recipients.is_empty() {
            self.disable();
        }
    }

    pub fn has_recipients(&self) -> bool {
        !self.recipients.is_empty()
    }

    pub fn is_enabled(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|run| !run.task.is_finished())
    }

    pub fn status(&self) -> Reply {
        Reply::ClipboardSync {
            enabled: self.is_enabled(),
            peers: self.recipients.keys().map(hex::encode).collect(),
            last_error: self
                .running
                .as_ref()
                .and_then(|run| run.last_error.borrow().clone()),
        }
    }

    pub fn disable(&mut self) {
        self.running = None;
    }

    pub fn enable(&mut self, activity: &Shared, monitor: Monitor) {
        let active = Rc::new(Cell::new(true));
        let last_error = Rc::new(RefCell::new(None));
        let task = spawn_local(run(
            monitor,
            Rc::downgrade(activity),
            Rc::downgrade(&active),
            Rc::clone(&last_error),
        ));
        self.running = Some(Running {
            task: task.abort_handle(),
            last_error,
            _token: active,
        });
    }
}

struct Seen(Option<Fingerprint>);

impl Seen {
    fn should_share(&mut self, new: Fingerprint, writes: &ClipboardWrites) -> bool {
        let previous = self.0.replace(new);
        previous != Some(new) && !writes.contains(new)
    }
}

async fn run(
    mut monitor: Monitor,
    activity: Weak<RefCell<Activity>>,
    active: Weak<Cell<bool>>,
    last_error: Rc<RefCell<Option<String>>>,
) {
    let mut seen = Seen(None);
    loop {
        let change = match monitor.changed().await {
            Ok(change) => change,
            Err(error) => {
                if let Some(run) = active.upgrade() {
                    run.set(false);
                }
                *last_error.borrow_mut() = Some(error.to_string());
                return;
            }
        };
        sleep(QUIET_PERIOD).await;
        if monitor.newer() {
            continue;
        }
        let observed = Instant::from_std(change.at);
        let payload = match monitor.read(&change).await {
            Ok(Some((media, bytes))) => Payload::validate(media, bytes, None)
                .await
                .map(Some)
                .map_err(|error| error.to_string()),
            Ok(None) | Err(Error::Empty | Error::NoSupportedContent) => Ok(None),
            Err(error) => Err(error.to_string()),
        };
        if monitor.newer() {
            continue;
        }
        let payload = match payload {
            Ok(payload) => {
                *last_error.borrow_mut() = None;
                let Some(payload) = payload else { continue };
                payload
            }
            Err(error) => {
                *last_error.borrow_mut() = Some(error);
                continue;
            }
        };
        let Some(activity) = activity.upgrade() else {
            return;
        };
        let mut state = activity.borrow_mut();
        if !seen.should_share(Fingerprint::from(&payload), &state.clipboard_writes)
            || Instant::now() >= observed + MAX_AGE
        {
            continue;
        }
        let recipients = state
            .sync
            .recipients
            .iter()
            .filter(|(_, recipient)| recipient.since <= observed)
            .map(|(peer, recipient)| (*peer, Rc::downgrade(&recipient.token)))
            .collect::<Vec<_>>();
        let Ok(batch) = state.bulk_batch(payload) else {
            continue;
        };
        for (peer, recipient) in recipients {
            state.sessions.queue_automatic(
                peer,
                Arc::clone(&batch),
                Automatic {
                    observed,
                    run: active.clone(),
                    recipient,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_from_lyteboard_and_repeated_contents_are_suppressed() {
        let writes = ClipboardWrites::default();
        let mut seen = Seen(None);
        let write = Payload::text("Restored clipboard".into()).unwrap();
        writes.remember(&write);
        assert!(!seen.should_share(Fingerprint::from(&write), &writes));
        let new = Payload::text("Local edit".into()).unwrap();
        assert!(seen.should_share(Fingerprint::from(&new), &writes));
        assert!(!seen.should_share(Fingerprint::from(&new), &writes));
        // A clipboard manager can reassert a recent remote selection after a local change.
        assert!(!seen.should_share(Fingerprint::from(&write), &writes));
    }

    #[test]
    fn pending_automatic_jobs_require_a_current_event_and_both_permissions() {
        let active = Rc::new(Cell::new(true));
        let preference = Rc::new(());
        let mut job = Automatic {
            observed: Instant::now(),
            run: Rc::downgrade(&active),
            recipient: Rc::downgrade(&preference),
        };
        assert!(job.is_current());
        job.observed -= MAX_AGE;
        assert!(!job.is_current());
        job.observed = Instant::now();
        drop(preference);
        assert!(!job.is_current());
    }
}
