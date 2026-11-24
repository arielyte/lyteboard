//! The LAN worker owns peer state and runs away from the authenticated IPC runtime.

mod discovery;
mod sessions;

use std::{
    cell::RefCell, collections::VecDeque, env, io, net::SocketAddr, path::PathBuf, rc::Rc,
    sync::Arc, time::Duration,
};

use lyteboard_core::peer::{
    CredentialError, Endpoint, PairingStatus,
    lan::{self, Incoming, Listener, PendingPair},
    storage::{StorageError, open_endpoint},
};
use lyteboard_protocol::{
    ipc::network::{
        Command, DEVICES_PER_PAGE, Device, Discovered, DiscoveryState, Message, PairStatus, Reply,
    },
    peer::{MAX_LABEL_BYTES, validate_label},
};
use tokio::{
    runtime,
    sync::{Semaphore, mpsc, oneshot},
    task::{AbortHandle, LocalSet, spawn_local},
    time::{Instant, MissedTickBehavior, interval, sleep, timeout},
};
use zeroize::Zeroizing;

use discovery::{Discovery, Hints, LocalNetworks};
use sessions::Sessions;

#[derive(Clone)]
pub struct Network(mpsc::Sender<(Command, oneshot::Sender<Reply>)>);

impl Network {
    pub fn start() -> io::Result<(Self, oneshot::Receiver<()>)> {
        let (tx, rx) = mpsc::channel(32);
        let (finished, done) = oneshot::channel();
        let runtime = runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("lyteboard-lan".into())
            .spawn(move || {
                LocalSet::new().block_on(&runtime, Worker::new().run(rx));
                let _ = finished.send(());
            })?;
        Ok((Self(tx), done))
    }

    pub async fn request(&self, command: Command) -> Reply {
        let (tx, rx) = oneshot::channel();
        if self.0.try_send((command, tx)).is_err() {
            return Reply::Error("LAN worker is unavailable or busy".into());
        }
        match timeout(Duration::from_secs(2), rx).await {
            Ok(Ok(reply)) => reply,
            _ => Reply::Error(
                "LAN worker is busy. Check the Secret Service session and retry.".into(),
            ),
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
}

struct Activity {
    pair: Option<PairJob>,
    pair_view: PairStatus,
    next_pair: u64,
    sessions: Sessions,
    hints: Hints,
    networks: LocalNetworks,
    inbox: VecDeque<Message>,
}

struct PairJob {
    id: u64,
    approval: Option<oneshot::Sender<Zeroizing<String>>>,
    _cancel_on_drop: oneshot::Sender<()>,
}

type Shared = Rc<RefCell<Activity>>;

impl Worker {
    fn new() -> Self {
        let profile = env::var_os("LYTEBOARD_PROFILE")
            .map(PathBuf::from)
            .or_else(|| {
                let data_dir = env::var_os("XDG_DATA_HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| {
                        env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
                    });
                data_dir.map(|path| path.join("lyteboard"))
            });
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
            discovery_enabled: env::var("LYTEBOARD_DISCOVERY").as_deref() != Ok("off"),
            activity: Rc::new(RefCell::new(Activity {
                pair: None,
                pair_view: PairStatus::Idle,
                next_pair: 0,
                sessions: Sessions::default(),
                hints: Hints::default(),
                networks: LocalNetworks::load(),
                inbox: VecDeque::new(),
            })),
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<(Command, oneshot::Sender<Reply>)>) {
        // Existing profiles can accept sessions as soon as the daemon starts. On first run,
        // leave local-only daemon commands usable without touching the keyring.
        if self
            .profile
            .as_ref()
            .is_some_and(|path| path.join("identity.json").is_file())
        {
            let _ = self.ensure_ready().await;
        }
        let mut scheduler = interval(Duration::from_millis(250));
        scheduler.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut refresh_networks = Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                request = rx.recv() => {
                    let Some((command, response)) = request else { break };
                    if !response.is_closed() {
                        let reply = self.command(command).await.unwrap_or_else(Reply::Error);
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
                .transpose()?;
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

    async fn command(&mut self, command: Command) -> Result<Reply, String> {
        if let Command::Listen { address } = command {
            let endpoint = self.ensure_endpoint()?;
            self.bind(Some(address), &endpoint).await?;
            return self.status();
        }
        let endpoint = self.ensure_ready().await?;
        match command {
            Command::Listen { .. } => unreachable!(),
            Command::Status => self.status(),
            Command::SetDiscovery { enabled } => {
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
                self.activity.borrow_mut().sessions.remove_peer(peer);
                Ok(Reply::Ok)
            }
            Command::Send { peer, text } => {
                sessions::queue(&self.activity, endpoint, peer, text).map(Reply::Queued)
            }
            Command::Delivery { id } => self
                .activity
                .borrow()
                .sessions
                .delivery(id)
                .map(Reply::Delivery),
            Command::Received { offset } => Ok(Reply::Received(
                self.activity.borrow().inbox.get(offset).cloned(),
            )),
        }
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
