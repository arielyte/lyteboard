//! Event-driven Linux clipboard monitoring. Only the newest selection is retained.

use std::{
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};

use lyteboard_protocol::peer::Media;
use rustix::event::{PollFd, PollFlags, poll};
use tokio::{
    io::AsyncReadExt,
    net::unix::pipe,
    sync::{Semaphore, oneshot, watch},
    time::timeout,
};
use wl_clipboard_rs::{
    paste::Seat,
    watch::{CancelHandle, ClipboardEvent, ClipboardType, Watcher},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::ConnectionExt as _,
    },
};
use zeroize::Zeroizing;

use super::{Backend, Clipboard, Error, TOOL_TIMEOUT};

static WATCHER: Semaphore = Semaphore::const_new(1);

#[derive(Default)]
struct WaylandStop {
    stopped: bool,
    handle: Option<CancelHandle>,
}

pub struct Monitor {
    clipboard: Clipboard,
    updates: watch::Receiver<Option<Arc<Change>>>,
    wayland_stop: Arc<Mutex<WaylandStop>>,
    _x11_stop: UnixStream,
}

pub struct Change {
    pub at: Instant,
    source: Mutex<Option<Source>>,
}

enum Source {
    Ignore,
    Wayland(Media, OwnedFd),
    X11,
}

type Snapshot = Option<(Media, Zeroizing<Vec<u8>>)>;
type Updates = watch::Sender<Option<Arc<Change>>>;

impl Monitor {
    pub async fn start(clipboard: Clipboard) -> Result<Self, Error> {
        timeout(TOOL_TIMEOUT, Self::open(clipboard))
            .await
            .map_err(|_| unavailable(clipboard.0))?
    }

    async fn open(clipboard: Clipboard) -> Result<Self, Error> {
        let backend = clipboard.0;
        if backend == Backend::X11 {
            clipboard.check_writer().await?;
        }
        let (x11_stop, stop_rx) = UnixStream::pair().map_err(|_| unavailable(backend))?;
        let (tx, updates) = watch::channel(None);
        let (init, ready) = oneshot::channel();
        let wayland_stop = Arc::new(Mutex::new(WaylandStop::default()));
        let stop = Arc::clone(&wayland_stop);
        let claim = WATCHER.acquire().await.map_err(|_| unavailable(backend))?;
        thread::Builder::new()
            .name("lyteboard-clipboard".into())
            .spawn(move || {
                let _claim = claim;
                match backend {
                    Backend::Wayland => watch_wayland(tx, init, stop),
                    Backend::X11 => {
                        let _ = watch_x11(tx, init, stop_rx);
                    }
                }
            })
            .map_err(|_| unavailable(backend))?;
        let monitor = Self {
            clipboard,
            updates,
            wayland_stop,
            _x11_stop: x11_stop,
        };
        ready.await.map_err(|_| unavailable(backend))?;
        Ok(monitor)
    }

    pub async fn changed(&mut self) -> Result<Arc<Change>, Error> {
        self.updates
            .changed()
            .await
            .map_err(|_| unavailable(self.clipboard.0))?;
        Ok(Arc::clone(
            self.updates
                .borrow_and_update()
                .as_ref()
                .expect("change published"),
        ))
    }

    pub fn newer(&self) -> bool {
        // A disconnected watcher cannot vouch for the current selection.
        self.updates.has_changed().unwrap_or(true)
    }

    pub async fn read(&self, change: &Change) -> Result<Snapshot, Error> {
        let input = change.source.lock().expect("clipboard event lock").take();
        match input {
            Some(Source::X11) => self.clipboard.read_selection(true).await,
            Some(Source::Wayland(media, fd)) => {
                let receiver =
                    pipe::Receiver::from_owned_fd(fd).map_err(|_| self.clipboard.0.access())?;
                let mut bytes = Zeroizing::new(Vec::new());
                timeout(
                    TOOL_TIMEOUT,
                    receiver
                        .take((media.limit() + 1) as u64)
                        .read_to_end(&mut bytes),
                )
                .await
                .map_err(|_| Error::Timeout(self.clipboard.0.hint()))?
                .map_err(|_| self.clipboard.0.access())?;
                if bytes.len() > media.limit() {
                    return Err(Error::TooLarge(media.limit()));
                }
                Ok((!bytes.is_empty()).then_some((media, bytes)))
            }
            Some(Source::Ignore) | None => Ok(None),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let mut stop = self.wayland_stop.lock().expect("clipboard stop lock");
        stop.stopped = true;
        if let Some(handle) = stop.handle.take() {
            handle.cancel();
        }
        // Dropping the X11 socket pair's local end wakes its blocking poll.
    }
}

fn unavailable(backend: Backend) -> Error {
    Error::Monitor(match backend {
        Backend::Wayland => {
            "Use a Wayland session with ext-data-control or wlr-data-control, such as current KDE Plasma. Retry after checking the session."
        }
        Backend::X11 => "Check DISPLAY and XAUTHORITY and ensure the X server supports XFixes.",
    })
}

fn publish(events: &Updates, at: Instant, source: Source) {
    events.send_replace(Some(Arc::new(Change {
        at,
        source: Mutex::new(Some(source)),
    })));
}

pub(super) fn is_sensitive(mime: &str) -> bool {
    mime.eq_ignore_ascii_case("x-kde-passwordManagerHint")
        || mime.eq_ignore_ascii_case("application/x-kde-passwordManagerHint")
}

fn choose_offer(types: &[String]) -> Option<(Media, &str)> {
    if types.iter().any(|mime| is_sensitive(mime)) {
        return None;
    }
    for media in [Media::Png, Media::Jpeg, Media::Webp] {
        if let Some(mime) = types.iter().find(|mime| mime.as_str() == media.mime()) {
            return Some((media, mime));
        }
    }
    for preferred in [
        Media::Text.mime(),
        "UTF8_STRING",
        "text/plain",
        "TEXT",
        "STRING",
    ] {
        if let Some(mime) = types
            .iter()
            .find(|mime| mime.eq_ignore_ascii_case(preferred))
        {
            return Some((Media::Text, mime));
        }
    }
    None
}

fn watch_wayland(events: Updates, ready: oneshot::Sender<()>, stop: Arc<Mutex<WaylandStop>>) {
    let Ok(mut watcher) = Watcher::new(ClipboardType::Regular, Seat::Unspecified) else {
        return;
    };
    {
        let mut stop = stop.lock().expect("clipboard stop lock");
        if stop.stopped {
            return;
        }
        stop.handle = Some(watcher.cancel_handle());
    }
    // Its first event describes the selection that existed before subscription.
    if !matches!(watcher.next_event(), Ok(Some(_))) || ready.send(()).is_err() {
        return;
    }
    while !events.is_closed()
        && let Ok(Some(event)) = watcher.next_event()
    {
        let at = Instant::now();
        let source = match event {
            ClipboardEvent::Changed {
                mime_types,
                mut offer,
                ..
            } => match choose_offer(&mime_types) {
                Some((media, mime)) => {
                    let Ok(fd) = offer.receive(mime) else { return };
                    Source::Wayland(media, fd.into())
                }
                None => Source::Ignore,
            },
            ClipboardEvent::Cleared { .. } => Source::Ignore,
        };
        publish(&events, at, source);
    }
}

fn watch_x11(
    events: Updates,
    ready: oneshot::Sender<()>,
    stop: UnixStream,
) -> Result<(), Box<dyn std::error::Error>> {
    let (connection, screen) = x11rb::connect(None)?;
    let version = connection.xfixes_query_version(2, 0)?.reply()?;
    if version.major_version < 2 {
        return Ok(());
    }
    let clipboard = connection.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    let root = connection.setup().roots[screen].root;
    connection
        .xfixes_select_selection_input(
            root,
            clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?
        .check()?;
    if ready.send(()).is_err() {
        return Ok(());
    }

    let mut fds = [
        PollFd::new(connection.stream(), PollFlags::IN),
        PollFd::new(&stop, PollFlags::IN),
    ];
    loop {
        // Requests during setup can already have read events into X11's receive buffer.
        while !events.is_closed()
            && let Some(event) = connection.poll_for_event()?
        {
            if let Event::XfixesSelectionNotify(event) = event
                && event.selection == clipboard
            {
                let source = if event.owner == 0 {
                    Source::Ignore
                } else {
                    Source::X11
                };
                publish(&events, Instant::now(), source);
            }
        }
        if events.is_closed() {
            return Ok(());
        }
        match poll(&mut fds, None) {
            Err(rustix::io::Errno::INTR) => continue,
            result => {
                result?;
            }
        }
        if !fds[1].revents().is_empty()
            || fds[0]
                .revents()
                .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_offers_are_ignored_and_supported_images_take_priority() {
        let offer = |types: &[&str]| types.iter().map(|mime| (*mime).into()).collect::<Vec<_>>();
        let sensitive = offer(&["text/plain", "image/png", "x-kde-passwordManagerHint"]);
        assert_eq!(choose_offer(&sensitive), None);
        let regular = offer(&["text/plain", "image/webp", "image/png"]);
        assert_eq!(choose_offer(&regular), Some((Media::Png, "image/png")));
        let unknown = offer(&["text/uri-list", "application/pdf"]);
        assert_eq!(choose_offer(&unknown), None);
    }
}
