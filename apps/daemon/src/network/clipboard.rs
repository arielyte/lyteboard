use std::{cell::RefCell, rc::Rc};

use lyteboard_core::clipboard::Clipboard;
use lyteboard_protocol::ipc::network::{MAX_TEST_TEXT_BYTES, Reply};
use tokio::{
    sync::watch,
    task::{AbortHandle, spawn_local},
};
use zeroize::Zeroizing;

/// An inbound-only sink. It has no access to the sending or connection paths.
#[derive(Default)]
pub(super) struct AutoClipboard(Option<Enabled>);

struct Enabled {
    pending: watch::Sender<Option<Zeroizing<String>>>,
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

    pub(super) fn enable(&mut self, clipboard: Clipboard) {
        let (pending, mut incoming) = watch::channel::<Option<Zeroizing<String>>>(None);
        let last_error = Rc::new(RefCell::new(None));
        let status = Rc::clone(&last_error);
        let task = spawn_local(async move {
            while incoming.changed().await.is_ok() {
                let text = incoming.borrow_and_update().clone();
                if let Some(text) = text {
                    let result = clipboard.write_text(&text, MAX_TEST_TEXT_BYTES).await;
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

    pub(super) fn received(&self, text: &str) {
        if let Some(enabled) = &self.0 {
            // Coalesce received text while a slow selection owner is still applying a write.
            enabled
                .pending
                .send_replace(Some(Zeroizing::new(text.to_owned())));
        }
    }
}
