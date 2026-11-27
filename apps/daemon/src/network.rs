//! The LAN worker owns peer state and runs away from the authenticated IPC runtime.

mod bulk;
mod clipboard;
mod discovery;
mod sessions;
mod sync;

use std::{
    cell::RefCell, env, io, net::SocketAddr, path::PathBuf, rc::Rc, sync::Arc, time::Duration,
};

use lyteboard_core::peer::{
    CredentialError, Endpoint, PairingStatus,
    lan::{self, Incoming, Listener, PendingPair},
    storage::{StorageError, open_endpoint},
};
use lyteboard_core::{
    bulk::Payload,
    clipboard::{Clipboard, monitor::Monitor},
};
use lyteboard_protocol::{
    ipc::network::{
        ClipboardPreferences, ClipboardSyncCommand, Command, DEVICES_PER_PAGE, Device, Discovered,
        DiscoveryState, PairStatus, QueueOutcome, Reply,
    },
    peer::{MAX_LABEL_BYTES, NoiseKey, validate_label},
};
use tokio::{
    runtime,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::{AbortHandle, LocalSet, spawn_local},
    time::{Instant, MissedTickBehavior, interval, sleep, timeout},
};
use zeroize::Zeroizing;

use crate::{history::History, preferences::Preferences};

use bulk::{Batch, Inbox};
use clipboard::{AutoClipboard, ClipboardWrites};
use discovery::{Discovery, Hints, LocalNetworks};
use sessions::Sessions;

#[derive(Clone)]
pub struct Network {
    requests: mpsc::Sender<(Operation, oneshot::Sender<Output>)>,
    clipboard_writes: ClipboardWrites,
}

enum Operation {
    Control(Command),
    Upload(Vec<NoiseKey>, Payload, OwnedSemaphorePermit),
    Download(usize),
    DownloadReceived([u8; 16]),
}

enum Output {
    Control(Reply),
    Download(Option<Arc<Payload>>),
}

impl Network {
    pub fn start(
        profile: Option<PathBuf>,
        history: History,
        preferences: Preferences,
    ) -> io::Result<(Self, oneshot::Receiver<()>)> {
        let (tx, rx) = mpsc::channel(32);
        let (finished, done) = oneshot::channel();
        let clipboard_writes = ClipboardWrites::default();
        let writes = clipboard_writes.clone();
        let runtime = runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("lyteboard-lan".into())
            .spawn(move || {
                LocalSet::new().block_on(
                    &runtime,
                    Worker::new(profile, history, writes, preferences).run(rx),
                );
                let _ = finished.send(());
            })?;
        Ok((
            Self {
                requests: tx,
                clipboard_writes,
            },
            done,
        ))
    }

    pub fn mark_clipboard_write(&self, payload: &Payload) {
        self.clipboard_writes.remember(payload);
    }

    pub async fn request(&self, command: Command) -> Reply {
        self.control(Operation::Control(command)).await
    }

    pub async fn upload(
        &self,
        peers: Vec<NoiseKey>,
        payload: Payload,
        slot: OwnedSemaphorePermit,
    ) -> Reply {
        self.control(Operation::Upload(peers, payload, slot)).await
    }

    pub async fn download(&self, offset: usize) -> Result<Option<Arc<Payload>>, String> {
        match self.dispatch(Operation::Download(offset)).await? {
            Output::Download(item) => Ok(item),
            Output::Control(_) => unreachable!(),
        }
    }

    pub async fn download_received(&self, id: [u8; 16]) -> Result<Option<Arc<Payload>>, String> {
        match self.dispatch(Operation::DownloadReceived(id)).await? {
            Output::Download(item) => Ok(item),
            Output::Control(_) => unreachable!(),
        }
    }

    async fn control(&self, operation: Operation) -> Reply {
        match self.dispatch(operation).await {
            Ok(Output::Control(reply)) => reply,
            Ok(Output::Download(_)) => unreachable!(),
            Err(error) => Reply::Error(error),
        }
    }

    async fn dispatch(&self, operation: Operation) -> Result<Output, String> {
        let (tx, rx) = oneshot::channel();
        if self.requests.try_send((operation, tx)).is_err() {
            return Err("LAN worker is unavailable or busy".into());
        }
        match timeout(Duration::from_secs(2), rx).await {
            Ok(Ok(reply)) => Ok(reply),
            _ => Err("LAN worker is busy. Check credential protection and retry.".into()),
        }
    }
}

struct Worker {
    profile: Option<PathBuf>,
    label: Result<Rc<str>, String>,
    endpoint: Option<Rc<Endpoint>>,
    listener: Option<(Rc<Listener>, AbortHandle)>,
    activity: Rc<RefCell<Activity>>,
    discovery: Option<Discovery>,
    discovery_enabled: bool,
    preferences: Preferences,
    send_error: Option<String>,
    apply_error: Option<String>,
}

struct Activity {
    pair: Option<PairJob>,
    pair_view: PairStatus,
    next_pair: u64,
    sessions: Sessions,
    hints: Hints,
    networks: LocalNetworks,
    inbox: Inbox,
    incoming_bulk: Arc<Semaphore>,
    outgoing_bulk: Arc<Semaphore>,
    clipboard: AutoClipboard,
    clipboard_writes: ClipboardWrites,
    sync: sync::Sync,
    history: History,
}

struct PairJob {
    id: u64,
    approval: Option<oneshot::Sender<Zeroizing<String>>>,
    _cancel_on_drop: oneshot::Sender<()>,
}

type Shared = Rc<RefCell<Activity>>;

pub(super) fn profile_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    return env::var_os("LYTEBOARD_PROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            lyteboard_core::windows::app_data()
                .ok()
                .map(|p| p.join("profile"))
        });
    #[cfg(target_os = "linux")]
    env::var_os("LYTEBOARD_PROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            let data_dir = env::var_os("XDG_DATA_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
                });
            data_dir.map(|path| path.join("lyteboard"))
        })
}

impl Worker {
    fn new(
        profile: Option<PathBuf>,
        history: History,
        clipboard_writes: ClipboardWrites,
        preferences: Preferences,
    ) -> Self {
        let name = env::var("LYTEBOARD_DEVICE_NAME").unwrap_or_else(|_| "Lyteboard".into());
        let label = if name.trim().is_empty() || validate_label(&name).is_err() {
            Err(format!(
                "Set LYTEBOARD_DEVICE_NAME to a display name of at most {MAX_LABEL_BYTES} bytes without controls."
            ))
        } else {
            Ok(Rc::from(name))
        };
        Self {
            profile,
            label,
            endpoint: None,
            listener: None,
            discovery: None,
            discovery_enabled: preferences
                .values()
                .ok()
                .and_then(|values| values.discovery)
                .unwrap_or_else(|| env::var("LYTEBOARD_DISCOVERY").as_deref() != Ok("off")),
            preferences,
            send_error: None,
            apply_error: None,
            activity: Rc::new(RefCell::new(Activity {
                pair: None,
                pair_view: PairStatus::Idle,
                next_pair: 0,
                sessions: Sessions::default(),
                hints: Hints::default(),
                networks: LocalNetworks::load(),
                inbox: Inbox::default(),
                incoming_bulk: Arc::new(Semaphore::new(2)),
                outgoing_bulk: Arc::new(Semaphore::new(2)),
                clipboard: AutoClipboard::default(),
                clipboard_writes,
                sync: sync::Sync::default(),
                history,
            })),
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<(Operation, oneshot::Sender<Output>)>) {
        // Existing profiles can accept sessions as soon as the daemon starts. On first run,
        // leave local-only daemon commands usable without touching the keyring.
        if self
            .profile
            .as_ref()
            .is_some_and(|path| path.join("identity.json").is_file())
        {
            let _ = self.ensure_ready().await;
        }
        if let Ok(values) = self.preferences.values() {
            if values.apply && !values.paused {
                self.apply_error = self.start_clipboard_apply(false).await.err();
            }
            if !values.recipients.is_empty() {
                if let Err(error) = self.load_recipients().await {
                    self.send_error = Some(error);
                } else if values.send && !values.paused {
                    self.send_error = self.start_clipboard_send(false).await.err();
                }
            }
        }
        let mut scheduler = interval(Duration::from_millis(250));
        scheduler.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut refresh_networks = Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                request = rx.recv() => {
                    let Some((operation, response)) = request else { break };
                    if !response.is_closed() {
                        let reply = match operation {
                            Operation::Control(command) => Output::Control(self.command(command).await.unwrap_or_else(Reply::Error)),
                            Operation::Upload(peers, payload, _slot) => Output::Control(self.upload(peers, payload).await.unwrap_or_else(Reply::Error)),
                            Operation::Download(offset) => Output::Download(self.activity.borrow().inbox.payload(offset)),
                            Operation::DownloadReceived(id) => Output::Download(self.activity.borrow().inbox.by_id(id)),
                        };
                        let _ = response.send(reply);
                    }
                }
                event = Discovery::next(self.discovery.as_ref()) => {
                    if let (Some(event), Some(endpoint)) = (event, &self.endpoint) {
                        let mut activity = self.activity.borrow_mut();
                        let Activity { hints, networks, .. } = &mut *activity;
                        hints.update(event, endpoint.noise_key(), networks);
                    } else {
                        self.stop_discovery().await;
                    }
                }
                _ = scheduler.tick(), if self.listener.is_some() => {
                    if Instant::now() >= refresh_networks {
                        self.activity.borrow_mut().networks = LocalNetworks::load();
                        refresh_networks = Instant::now() + Duration::from_secs(5);
                    }
                    if let Some(endpoint) = &self.endpoint {
                        sessions::reconnect(&self.activity, endpoint);
                    }
                }
            }
        }
        self.stop_discovery().await;
    }

    async fn stop_discovery(&mut self) {
        self.activity.borrow_mut().hints = Hints::default();
        if let Some(discovery) = self.discovery.take() {
            discovery.stop().await;
        }
    }

    async fn update_discovery(&mut self, endpoint: &Endpoint) {
        self.stop_discovery().await;
        if self.discovery_enabled
            && let Ok(address) = self.listener().local_addr()
            && !address.ip().is_loopback()
        {
            let mut activity = self.activity.borrow_mut();
            activity.networks = LocalNetworks::load();
            self.discovery =
                Discovery::start(endpoint.noise_key(), address, &activity.networks).ok();
        }
    }

    fn ensure_endpoint(&mut self) -> Result<Rc<Endpoint>, String> {
        if let Some(endpoint) = &self.endpoint {
            return Ok(Rc::clone(endpoint));
        }
        let path = self
            .profile
            .as_ref()
            .ok_or("Set LYTEBOARD_PROFILE to an absolute private data directory.")?;
        let endpoint = Rc::new(open_endpoint(path).map_err(storage_error)?);
        self.endpoint = Some(Rc::clone(&endpoint));
        Ok(endpoint)
    }

    async fn ensure_ready(&mut self) -> Result<Rc<Endpoint>, String> {
        let endpoint = self.ensure_endpoint()?;
        if self.listener.is_none() {
            let address = env::var_os("LYTEBOARD_LISTEN_ADDR")
                .map(|value| {
                    value
                        .to_str()
                        .and_then(|value| value.parse().ok())
                        .ok_or("Set LYTEBOARD_LISTEN_ADDR to an IP and port.")
                })
                .transpose()?
                .or_else(|| {
                    self.preferences
                        .values()
                        .ok()
                        .and_then(|values| values.listen)
                });
            self.bind(address, &endpoint).await?;
        }
        Ok(endpoint)
    }

    async fn bind(
        &mut self,
        address: Option<SocketAddr>,
        endpoint: &Rc<Endpoint>,
    ) -> Result<(), String> {
        let label = self.label.as_ref().map_err(Clone::clone)?;
        if let Some((current, _)) = &self.listener {
            if current.local_addr().ok() == address {
                return Ok(());
            }
            if self.activity.borrow().pair.is_some() {
                return Err("Finish or cancel pairing before changing the listener.".into());
            }
        }
        let listener = Rc::new(Listener::bind(address).await.map_err(|error| match error {
            lan::Error::Io(error) => format!("Could not bind the LAN listener: {error}"),
            error => error.to_string(),
        })?);
        endpoint.cancel_invite().map_err(peer_error)?;
        let task = spawn_local(accept_connections(
            Rc::clone(&listener),
            Rc::clone(endpoint),
            Rc::clone(label),
            Rc::clone(&self.activity),
        ));
        if let Some((_, old)) = self.listener.replace((listener, task.abort_handle())) {
            old.abort();
        }
        self.update_discovery(endpoint).await;
        Ok(())
    }

    async fn upload(&mut self, peers: Vec<NoiseKey>, payload: Payload) -> Result<Reply, String> {
        let endpoint = self.ensure_ready().await?;
        let batch = self.activity.borrow().bulk_batch(payload)?;
        let outcomes = peers
            .into_iter()
            .map(|peer| {
                match sessions::queue_bulk(
                    &self.activity,
                    Rc::clone(&endpoint),
                    peer,
                    Arc::clone(&batch),
                ) {
                    Ok(id) => QueueOutcome::Queued(id),
                    Err(error) => QueueOutcome::Error(
                        error
                            .chars()
                            .take(48)
                            .map(|ch| {
                                if ch.is_ascii_graphic() || ch == ' ' {
                                    ch
                                } else {
                                    '?'
                                }
                            })
                            .collect(),
                    ),
                }
            })
            .collect();
        Ok(Reply::BulkQueued(outcomes))
    }

    async fn load_recipients(&mut self) -> Result<(), String> {
        let pins = self.preferences.values()?.recipients;
        let endpoint = self.ensure_ready().await?;
        let mut state = self.activity.borrow_mut();
        for device in endpoint.trusted_devices().map_err(peer_error)? {
            if pins.contains(&device.noise_key) {
                state.sync.set_recipient(device.noise_key)?;
            }
        }
        Ok(())
    }

    async fn start_clipboard_send(&mut self, persist: bool) -> Result<(), String> {
        if !self.activity.borrow().sync.has_recipients() {
            self.load_recipients().await?;
        }
        if !self.activity.borrow().sync.has_recipients() {
            return Err("Select a paired device before enabling automatic sending.".into());
        }
        if persist && self.preferences.values()?.paused {
            return self.preferences.update(|values| values.send = true);
        }
        if !self.activity.borrow().sync.is_enabled() {
            let clipboard = Clipboard::for_session().map_err(|error| error.to_string())?;
            let monitor = Monitor::start(clipboard)
                .await
                .map_err(|error| error.to_string())?;
            if persist {
                self.preferences.update(|values| values.send = true)?;
            }
            self.activity
                .borrow_mut()
                .sync
                .enable(&self.activity, monitor);
        }
        self.send_error = None;
        Ok(())
    }

    async fn start_clipboard_apply(&mut self, persist: bool) -> Result<(), String> {
        if persist && self.preferences.values()?.paused {
            return self.preferences.update(|values| values.apply = true);
        }
        if !self.activity.borrow().clipboard.is_enabled() {
            let clipboard = Clipboard::for_session().map_err(|error| {
                format!("{error} Restart lyteboardd from that graphical session to enable automatic writes.")
            })?;
            clipboard
                .check_writer()
                .await
                .map_err(|error| error.to_string())?;
            if persist {
                self.preferences.update(|values| values.apply = true)?;
            }
            let mut activity = self.activity.borrow_mut();
            let writes = activity.clipboard_writes.clone();
            activity.clipboard.enable(clipboard, writes);
        }
        self.apply_error = None;
        Ok(())
    }

    fn clipboard_preferences(&self) -> Result<Reply, String> {
        let saved = self.preferences.values()?;
        let activity = self.activity.borrow();
        let Reply::ClipboardSync {
            enabled: send_active,
            last_error: send_error,
            ..
        } = activity.sync.status()
        else {
            unreachable!()
        };
        let Reply::ClipboardAuto {
            enabled: apply_active,
            last_error: apply_error,
        } = activity.clipboard.status()
        else {
            unreachable!()
        };
        Ok(Reply::ClipboardPreferences(ClipboardPreferences {
            paused: saved.paused,
            send: saved.send,
            apply: saved.apply,
            peers: saved.recipients.iter().map(hex::encode).collect(),
            send_active,
            apply_active,
            send_error: send_error.or_else(|| self.send_error.clone()),
            apply_error: apply_error.or_else(|| self.apply_error.clone()),
        }))
    }

    async fn set_clipboard_paused(&mut self, paused: bool) -> Result<Reply, String> {
        if paused {
            let mut activity = self.activity.borrow_mut();
            activity.sync.disable();
            activity.clipboard.disable();
        }
        self.preferences.update(|values| values.paused = paused)?;
        let saved = self.preferences.values()?;
        self.apply_error = None;
        self.send_error = None;
        if !paused {
            if saved.apply {
                self.apply_error = self.start_clipboard_apply(false).await.err();
            }
            if saved.send {
                self.send_error = self.start_clipboard_send(false).await.err();
            }
        }
        self.clipboard_preferences()
    }

    async fn clipboard_sync(&mut self, command: ClipboardSyncCommand) -> Result<Reply, String> {
        match command {
            ClipboardSyncCommand::Status => (),
            ClipboardSyncCommand::Set { enabled: false } => {
                self.activity.borrow_mut().sync.disable();
                self.send_error = None;
                self.preferences.update(|values| values.send = false)?;
            }
            ClipboardSyncCommand::Set { enabled: true } => self.start_clipboard_send(true).await?,
            ClipboardSyncCommand::Recipient {
                peer,
                enabled: false,
            } => {
                self.preferences.update(|values| {
                    values.recipients.remove(&peer);
                    if values.recipients.is_empty() {
                        values.send = false;
                    }
                })?;
                self.activity.borrow_mut().sync.remove(peer);
            }
            ClipboardSyncCommand::Recipient {
                peer,
                enabled: true,
            } => {
                let endpoint = self.ensure_ready().await?;
                if !endpoint
                    .trusted_devices()
                    .map_err(peer_error)?
                    .iter()
                    .any(|device| device.noise_key == peer)
                {
                    return Err("Device is not trusted.".into());
                }
                self.preferences.update(|values| {
                    values.recipients.insert(peer);
                })?;
                self.activity.borrow_mut().sync.set_recipient(peer)?;
            }
        }
        let paused = self.preferences.values().is_ok_and(|values| values.paused);
        let mut status = self.activity.borrow().sync.status();
        if let Reply::ClipboardSync { last_error, .. } = &mut status
            && last_error.is_none()
        {
            *last_error = if paused {
                Some("Automatic synchronization is paused.".into())
            } else {
                self.send_error.clone()
            };
        }
        Ok(status)
    }

    async fn command(&mut self, command: Command) -> Result<Reply, String> {
        if let Command::Listen { address } = command {
            let endpoint = self.ensure_endpoint()?;
            self.bind(Some(address), &endpoint).await?;
            self.preferences
                .update(|values| values.listen = Some(address))?;
            return self.status();
        }
        match command {
            Command::ClipboardSync(command) => return self.clipboard_sync(command).await,
            Command::LatestReceived => {
                return Ok(Reply::LatestReceived(self.activity.borrow().inbox.latest()));
            }
            Command::ClipboardPreferences => return self.clipboard_preferences(),
            Command::SetClipboardPaused { paused } => {
                return self.set_clipboard_paused(paused).await;
            }
            Command::ClipboardAutoStatus => return Ok(self.clipboard_apply_status()),
            Command::SetClipboardAuto { enabled } => {
                if enabled {
                    self.start_clipboard_apply(true).await?;
                } else {
                    self.activity.borrow_mut().clipboard.disable();
                    self.apply_error = None;
                    self.preferences.update(|values| values.apply = false)?;
                }
                return Ok(self.clipboard_apply_status());
            }
            _ => {}
        }
        let endpoint = self.ensure_ready().await?;
        match command {
            Command::Listen { .. }
            | Command::LatestReceived
            | Command::ClipboardPreferences
            | Command::SetClipboardPaused { .. }
            | Command::ClipboardAutoStatus
            | Command::ClipboardSync(_)
            | Command::SetClipboardAuto { .. } => unreachable!(),
            Command::Status => self.status(),
            Command::SetDiscovery { enabled } => {
                self.preferences
                    .update(|values| values.discovery = Some(enabled))?;
                self.discovery_enabled = enabled;
                self.update_discovery(&endpoint).await;
                self.status()
            }
            Command::Discover { offset } => {
                let devices = endpoint.trusted_devices().map_err(peer_error)?;
                let state = self.activity.borrow();
                let mut entries = state
                    .hints
                    .entries()
                    .skip(offset)
                    .take(DEVICES_PER_PAGE + 1)
                    .map(|(key, address)| Discovered {
                        key,
                        address,
                        paired: devices.iter().any(|device| device.noise_key == key),
                    })
                    .collect::<Vec<_>>();
                let more = entries.len() > DEVICES_PER_PAGE;
                entries.truncate(DEVICES_PER_PAGE);
                Ok(Reply::Discovered { entries, more })
            }
            Command::SetAddress { peer, address } => {
                endpoint
                    .remember_address(peer, address)
                    .map_err(peer_error)?;
                self.activity.borrow_mut().sessions.address_changed(peer);
                Ok(Reply::Ok)
            }
            Command::Invite => {
                if self.activity.borrow().pair.is_some() {
                    return Err("A pairing is already in progress.".into());
                }
                let qr = self.listener().invite(&endpoint).map_err(lan_error)?;
                self.activity.borrow_mut().pair_view = PairStatus::Idle;
                Ok(Reply::Invite(qr.as_str().to_owned()))
            }
            Command::Accept { qr } => {
                let label = Rc::clone(self.label.as_ref().map_err(Clone::clone)?);
                let activity = Rc::clone(&self.activity);
                let (id, cancel) = activity.borrow_mut().begin_pair()?;
                let qr = Zeroizing::new(qr);
                spawn_local(async move {
                    let result = tokio::select! {
                        biased;
                        _ = cancel => return,
                        result = async {
                            let pending = lan::connect_pairing(&endpoint, &qr, &label).await.map_err(lan_error)?;
                            drop(qr);
                            approve_pair(&activity, id, pending).await
                        } => result,
                    };
                    activity.borrow_mut().finish_pair(id, result);
                });
                Ok(Reply::Ok)
            }
            Command::PairStatus => Ok(Reply::Pair(self.activity.borrow().pair_view.clone())),
            Command::Confirm { code } => {
                let code = Zeroizing::new(code);
                let mut activity = self.activity.borrow_mut();
                if !matches!(&activity.pair_view, PairStatus::Compare { code: expected } if *expected == *code)
                {
                    return Err(
                        "No matching verification code. Check both devices with `pair status`."
                            .into(),
                    );
                }
                let approval = activity
                    .pair
                    .as_mut()
                    .and_then(|job| job.approval.take())
                    .ok_or("There is no pairing awaiting approval.")?;
                approval
                    .send(code)
                    .map_err(|_| "Pairing ended before approval.")?;
                activity.pair_view = PairStatus::Approving;
                Ok(Reply::Ok)
            }
            Command::CancelPair => {
                endpoint.cancel_invite().map_err(peer_error)?;
                let mut activity = self.activity.borrow_mut();
                activity.pair = None;
                activity.pair_view = PairStatus::Idle;
                Ok(Reply::Ok)
            }
            Command::Devices { offset } => {
                let devices = endpoint.trusted_devices().map_err(peer_error)?;
                let more = devices.len() > offset.saturating_add(DEVICES_PER_PAGE);
                let state = self.activity.borrow();
                let entries = devices
                    .into_iter()
                    .skip(offset)
                    .take(DEVICES_PER_PAGE)
                    .map(|device| Device {
                        key: device.noise_key,
                        name: device.label,
                        address: device.last_known_address,
                        state: state.sessions.state(device.noise_key),
                    })
                    .collect();
                Ok(Reply::Devices { entries, more })
            }
            Command::Rename { peer, name } => {
                if name.trim().is_empty() {
                    return Err("Device name cannot be empty.".into());
                }
                endpoint.rename_device(peer, name).map_err(peer_error)?;
                Ok(Reply::Ok)
            }
            Command::Remove { peer } => {
                endpoint.revoke(peer).map_err(peer_error)?;
                let mut state = self.activity.borrow_mut();
                state.sessions.remove_peer(peer);
                state.sync.remove(peer);
                if self
                    .preferences
                    .values()
                    .is_ok_and(|values| values.recipients.contains(&peer))
                {
                    let _ = self.preferences.update(|values| {
                        values.recipients.remove(&peer);
                        if values.recipients.is_empty() {
                            values.send = false;
                        }
                    });
                }
                Ok(Reply::Ok)
            }
            Command::Send { peer, text } => {
                sessions::queue_text(&self.activity, endpoint, peer, text).map(Reply::Queued)
            }
            Command::Delivery { id } => self
                .activity
                .borrow()
                .sessions
                .delivery(id)
                .map(Reply::Delivery),
            Command::BulkUpload { .. }
            | Command::BulkDownload { .. }
            | Command::BulkDownloadReceived { .. }
            | Command::History(_) => Err("Use the bulk stream.".into()),
            Command::Received { offset } => Ok(Reply::Received(
                self.activity.borrow().inbox.message(offset),
            )),
        }
    }

    fn clipboard_apply_status(&self) -> Reply {
        let mut status = self.activity.borrow().clipboard.status();
        if let Reply::ClipboardAuto { last_error, .. } = &mut status
            && last_error.is_none()
        {
            *last_error = self.apply_error.clone();
        }
        status
    }

    fn listener(&self) -> &Listener {
        &self.listener.as_ref().expect("network was initialized").0
    }

    fn status(&self) -> Result<Reply, String> {
        let listener = self.listener().local_addr().map_err(lan_error)?;
        let discovery = if !self.discovery_enabled {
            DiscoveryState::Off
        } else if listener.ip().is_loopback() {
            DiscoveryState::Loopback
        } else if self.discovery.is_some() {
            DiscoveryState::Running
        } else {
            DiscoveryState::Unavailable
        };
        Ok(Reply::Status {
            listener,
            sessions: self.activity.borrow().sessions.connected(),
            discovery,
        })
    }
}

impl Activity {
    fn bulk_batch(&self, payload: Payload) -> Result<Arc<Batch>, String> {
        let slot = self
            .outgoing_bulk
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Bulk sending is busy. Retry after the current transfers.")?;
        Ok(Arc::new(Batch {
            payload: Arc::new(payload),
            _slot: slot,
        }))
    }

    fn begin_pair(&mut self) -> Result<(u64, oneshot::Receiver<()>), String> {
        if self.pair.is_some() {
            return Err("A pairing is already in progress.".into());
        }
        self.next_pair += 1;
        let id = self.next_pair;
        let (cancel, canceled) = oneshot::channel();
        self.pair = Some(PairJob {
            id,
            approval: None,
            _cancel_on_drop: cancel,
        });
        self.pair_view = PairStatus::Connecting;
        Ok((id, canceled))
    }

    fn finish_pair(&mut self, id: u64, result: Result<PairingStatus, String>) {
        if self.pair.as_ref().is_some_and(|job| job.id == id) {
            self.pair = None;
            self.pair_view = match result {
                Ok(PairingStatus::Complete | PairingStatus::LocalTrustSaved) => PairStatus::Paired,
                Ok(_) => PairStatus::Failed("Pairing ended without saving trust.".into()),
                Err(error) => PairStatus::Failed(error),
            };
        }
    }
}

async fn accept_connections(
    listener: Rc<Listener>,
    endpoint: Rc<Endpoint>,
    label: Rc<str>,
    activity: Shared,
) {
    let slots = Arc::new(Semaphore::new(8));
    loop {
        let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
            return;
        };
        let Ok(socket) = listener.accept_socket().await else {
            sleep(Duration::from_millis(100)).await;
            continue;
        };
        let (endpoint, label, activity) = (
            Rc::clone(&endpoint),
            Rc::clone(&label),
            Rc::clone(&activity),
        );
        spawn_local(async move {
            let incoming = socket.authenticate(&endpoint, &label).await;
            drop(slot);
            match incoming {
                Ok(Incoming::Pairing(pending)) => {
                    let job = activity.borrow_mut().begin_pair();
                    let Ok((id, cancel)) = job else {
                        let _ = pending.reject().await;
                        return;
                    };
                    let result = tokio::select! {
                        biased;
                        _ = cancel => return,
                        result = approve_pair(&activity, id, pending) => result,
                    };
                    activity.borrow_mut().finish_pair(id, result);
                }
                Ok(Incoming::Session(session)) => {
                    sessions::incoming(&activity, session, endpoint.noise_key()).await
                }
                Err(_) => (),
            }
        });
    }
}

async fn approve_pair(
    activity: &Shared,
    id: u64,
    mut pending: PendingPair<'_>,
) -> Result<PairingStatus, String> {
    let remaining = pending.approval_window().map_err(lan_error)?;
    let (approval, mut approved) = oneshot::channel();
    {
        let mut activity = activity.borrow_mut();
        let job = activity
            .pair
            .as_mut()
            .filter(|job| job.id == id)
            .ok_or("Pairing canceled.")?;
        job.approval = Some(approval);
        activity.pair_view = PairStatus::Compare {
            code: pending.verification_code().as_str().to_owned(),
        };
    }
    let code = timeout(remaining, async {
        loop {
            tokio::select! {
                result = &mut approved => return result.map_err(|_| "Pairing canceled.".to_owned()),
                input = pending.wait_for_input() => {
                    input.map_err(lan_error)?;
                    pending.receive_before_approval().await.map_err(lan_error)?;
                }
            }
        }
    })
    .await
    .map_err(|_| "Pairing invitation expired.")??;
    pending.approve(&code).await.map_err(lan_error)
}

fn storage_error(error: StorageError) -> String {
    #[cfg(target_os = "windows")]
    if matches!(error, StorageError::Credential(_)) {
        return "Windows user-scoped identity protection failed. Restore this profile and its Windows user credentials, or choose a new profile and pair again. No replacement key was generated.".into();
    }
    match error {
        StorageError::Credential(CredentialError::Locked) =>
            "Secret Service is locked. Unlock your desktop keyring and retry.".into(),
        StorageError::Credential(CredentialError::Unavailable) =>
            "Secret Service is unavailable. Start a session keyring with an unlocked persistent collection and retry.".into(),
        StorageError::Credential(CredentialError::Missing) =>
            "This profile's keys are missing. Restore its Secret Service entry or use a new LYTEBOARD_PROFILE and pair again.".into(),
        error => error.to_string(),
    }
}

fn peer_error(error: lyteboard_core::peer::Error) -> String {
    match error {
        lyteboard_core::peer::Error::Storage(error) => storage_error(error),
        error => error.to_string(),
    }
}

fn lan_error(error: lan::Error) -> String {
    match error {
        lan::Error::Peer(error) => peer_error(error),
        error => error.to_string(),
    }
}
