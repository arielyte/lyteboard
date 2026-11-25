use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use lyteboard_core::{
    bulk::{self, Media, Payload},
    peer::{Endpoint, PairingStatus, lan},
};
use lyteboard_protocol::peer::{LanInvite, NoiseKey, validate_lan_address};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use zeroize::Zeroizing;

mod transfer;

const RETRY_MAX: Duration = Duration::from_secs(30);
const HINT_LIFETIME: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum MobileError {
    #[error("{message}")]
    Operation { message: String },
}

impl MobileError {
    fn message(message: impl Into<String>) -> Self {
        Self::Operation {
            message: message.into(),
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait LanAccess: Send + Sync {
    fn is_on_link(&self, numeric_ip: String) -> bool;
}

#[derive(Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Connection {
    Offline,
    Connecting,
    Connected,
}

#[derive(uniffi::Record)]
pub struct Device {
    pub id: String,
    pub label: String,
    pub address: Option<String>,
    pub connection: Connection,
}

#[derive(Clone, Default, uniffi::Enum)]
pub enum PairState {
    #[default]
    Idle,
    Connecting,
    Compare {
        code: String,
    },
    WaitingForPeer,
    Complete,
    Failed {
        message: String,
    },
}

#[derive(Clone, Copy, uniffi::Enum)]
pub enum Kind {
    Text,
    Png,
    Jpeg,
    Webp,
}

impl From<Kind> for Media {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Text => Self::Text,
            Kind::Png => Self::Png,
            Kind::Jpeg => Self::Jpeg,
            Kind::Webp => Self::Webp,
        }
    }
}

impl From<Media> for Kind {
    fn from(kind: Media) -> Self {
        match kind {
            Media::Text => Self::Text,
            Media::Png => Self::Png,
            Media::Jpeg => Self::Jpeg,
            Media::Webp => Self::Webp,
        }
    }
}

#[derive(uniffi::Record)]
pub struct Received {
    pub sequence: u64,
    pub device_id: String,
    pub kind: Kind,
    pub size: u32,
    pub sha256: String,
}

#[derive(uniffi::Record)]
pub struct Snapshot {
    pub id: String,
    pub devices: Vec<Device>,
    pub pairing: PairState,
    pub send_status: String,
    pub latest: Option<Received>,
}

#[derive(uniffi::Object)]
pub struct Mobile {
    driver: Option<(tokio::runtime::Handle, oneshot::Sender<()>)>,
    engine: Arc<Engine>,
}

struct Engine {
    endpoint: Endpoint,
    label: String,
    access: Box<dyn LanAccess>,
    model: Mutex<Model>,
    outgoing: Arc<Semaphore>,
    incoming: Arc<Semaphore>,
    dialing: Semaphore,
}

#[derive(Default)]
struct Model {
    active: bool,
    sequence: u64,
    pairing: PairState,
    pair: Option<PairJob>,
    peers: BTreeMap<NoiseKey, Peer>,
    hints: BTreeMap<NoiseKey, (SocketAddr, Instant)>,
    latest: Option<(u64, NoiseKey, Payload)>,
    send_status: String,
    sending: Option<NoiseKey>,
}

struct PairJob {
    id: u64,
    task: JoinHandle<()>,
    approval: Option<oneshot::Sender<String>>,
}

struct Peer {
    id: u64,
    task: JoinHandle<()>,
    state: Connection,
    sender: Option<mpsc::Sender<Job>>,
}

struct Job {
    kind: Media,
    bytes: Zeroizing<Vec<u8>>,
    permit: OwnedSemaphorePermit,
}

#[uniffi::export]
pub fn open_mobile(label: String, access: Box<dyn LanAccess>) -> Result<Arc<Mobile>, MobileError> {
    #[cfg(target_os = "android")]
    {
        let endpoint = super::keystore::PREPARED
            .lock()
            .map_err(|_| MobileError::message("Profile initialization failed."))?
            .take()
            .ok_or_else(|| MobileError::message("Android credential store was not initialized."))?;
        Mobile::with_endpoint(endpoint, label, access).map(Arc::new)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (label, access);
        Err(MobileError::message("Android Keystore is required."))
    }
}

impl Mobile {
    #[cfg(target_os = "android")]
    fn with_endpoint(
        endpoint: Endpoint,
        label: String,
        access: Box<dyn LanAccess>,
    ) -> Result<Self, MobileError> {
        lyteboard_protocol::peer::validate_label(&label)
            .map_err(|_| MobileError::message("Invalid device label."))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| MobileError::message("Could not start the LAN worker."))?;
        // Keep a dedicated driver running even while Kotlin is idle.
        let handle = runtime.handle().clone();
        let (done, stopped) = oneshot::channel::<()>();
        std::thread::Builder::new()
            .name("lyteboard-lan".into())
            .spawn(move || {
                let _ = runtime.block_on(stopped);
                runtime.shutdown_background();
            })
            .map_err(|_| MobileError::message("Could not start the LAN worker."))?;
        Ok(Self {
            driver: Some((handle, done)),
            engine: Arc::new(Engine {
                endpoint,
                label,
                access,
                model: Mutex::default(),
                outgoing: Arc::new(Semaphore::new(1)),
                incoming: Arc::new(Semaphore::new(2)),
                dialing: Semaphore::new(4),
            }),
        })
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        &self
            .driver
            .as_ref()
            .expect("driver exists while Mobile is live")
            .0
    }
}

#[uniffi::export]
impl Mobile {
    pub fn set_active(&self, active: bool) {
        let mut model = self.engine.model();
        if model.active == active {
            return;
        }
        model.active = active;
        if active {
            self.engine.schedule(self.handle(), &mut model);
        } else {
            model.pause();
        }
    }

    pub fn snapshot(&self) -> Result<Snapshot, MobileError> {
        let model = self.engine.model();
        let devices = self
            .engine
            .endpoint
            .trusted_devices()
            .map_err(|error| MobileError::message(error.to_string()))?;
        Ok(Snapshot {
            id: hex::encode(self.engine.endpoint.noise_key()),
            devices: devices
                .into_iter()
                .map(|device| Device {
                    id: hex::encode(device.noise_key),
                    label: device.label,
                    address: device.last_known_address.map(|address| address.to_string()),
                    connection: model
                        .peers
                        .get(&device.noise_key)
                        .map_or(Connection::Offline, |peer| peer.state),
                })
                .collect(),
            pairing: model.pairing.clone(),
            send_status: model.send_status.clone(),
            latest: model
                .latest
                .as_ref()
                .map(|(sequence, peer, payload)| Received {
                    sequence: *sequence,
                    device_id: hex::encode(peer),
                    kind: payload.media().into(),
                    size: payload.bytes().len() as u32,
                    sha256: hex::encode(payload.digest()),
                }),
        })
    }

    pub fn received_bytes(&self, sequence: u64) -> Result<Vec<u8>, MobileError> {
        self.engine
            .model()
            .latest
            .as_ref()
            .filter(|entry| entry.0 == sequence)
            .map(|entry| entry.2.bytes().to_vec())
            .ok_or_else(|| MobileError::message("That received item is no longer available."))
    }

    pub fn import_invitation(&self, invitation: String) -> Result<(), MobileError> {
        let invitation = Zeroizing::new(invitation);
        let parsed = LanInvite::decode_qr(&invitation)
            .map_err(|_| MobileError::message("Invalid LAN invitation."))?;
        let mut model = self.engine.model();
        if !model.active || model.pair.is_some() {
            return Err(MobileError::message(
                "Open Lyteboard and finish the current pairing first.",
            ));
        }
        let id = model.next();
        let engine = Arc::clone(&self.engine);
        let task = self.handle().spawn(async move {
            let result = engine.pair(id, &invitation, parsed.address).await;
            let mut model = engine.model();
            if model.pair.as_ref().is_some_and(|job| job.id == id) {
                model.pair = None;
                model.pairing = match result {
                    Ok(()) => PairState::Complete,
                    Err(message) => PairState::Failed { message },
                };
                engine.schedule(&tokio::runtime::Handle::current(), &mut model);
            }
        });
        model.pairing = PairState::Connecting;
        model.pair = Some(PairJob {
            id,
            task,
            approval: None,
        });
        Ok(())
    }

    pub fn confirm_pairing(&self, code: String) -> Result<(), MobileError> {
        let mut model = self.engine.model();
        let sender = model
            .pair
            .as_mut()
            .and_then(|job| job.approval.take())
            .ok_or_else(|| MobileError::message("There is no code to approve."))?;
        sender
            .send(code)
            .map_err(|_| MobileError::message("Pairing has expired."))?;
        model.pairing = PairState::WaitingForPeer;
        Ok(())
    }

    pub fn cancel_pairing(&self) {
        self.engine.model().cancel_pairing();
    }

    /// A numeric socket address is always a hint. KK checks the already trusted pin.
    pub fn connect(&self, device_id: String, address: String) -> Result<(), MobileError> {
        let peer = parse_pin(&device_id)?;
        let address = address
            .parse()
            .map_err(|_| MobileError::message("Enter a numeric IP and port."))?;
        validate_lan_address(address).map_err(|_| MobileError::message("Invalid LAN address."))?;
        if !self.engine.access.is_on_link(address.ip().to_string()) {
            return Err(MobileError::message(
                "Address is outside the local network.",
            ));
        }
        let mut model = self.engine.model();
        if !model.active || !self.engine.is_trusted(peer) {
            return Err(MobileError::message(
                "Open Lyteboard and select a trusted device.",
            ));
        }
        model.remove_peer(peer);
        model.hints.insert(peer, (address, Instant::now()));
        self.engine.schedule(self.handle(), &mut model);
        Ok(())
    }

    pub fn discovery_hint(&self, device_id: String, address: String) {
        let (Ok(peer), Ok(address)) = (parse_pin(&device_id), address.parse::<SocketAddr>()) else {
            return;
        };
        let mut model = self.engine.model();
        if model.active && validate_lan_address(address).is_ok() && self.engine.is_trusted(peer) {
            model.hints.insert(peer, (address, Instant::now()));
        }
    }

    pub fn remove_device(&self, device_id: String) -> Result<(), MobileError> {
        let peer = parse_pin(&device_id)?;
        let mut model = self.engine.model();
        self.engine
            .endpoint
            .revoke(peer)
            .map_err(|error| MobileError::message(error.to_string()))?;
        model.remove_peer(peer);
        model.hints.remove(&peer);
        if model.latest.as_ref().is_some_and(|entry| entry.1 == peer) {
            model.latest = None;
        }
        Ok(())
    }

    pub fn send(&self, device_id: String, kind: Kind, bytes: Vec<u8>) -> Result<(), MobileError> {
        let peer = parse_pin(&device_id)?;
        let bytes = Zeroizing::new(bytes);
        let kind = kind.into();
        bulk::validate_size(kind, bytes.len())
            .map_err(|error| MobileError::message(error.to_string()))?;
        let mut model = self.engine.model();
        let permit = Arc::clone(&self.engine.outgoing)
            .try_acquire_owned()
            .map_err(|_| MobileError::message("Wait for the current transfer."))?;
        let sender = model
            .peers
            .get(&peer)
            .and_then(|peer| peer.sender.as_ref())
            .ok_or_else(|| MobileError::message("The device is offline."))?;
        sender
            .try_send(Job {
                kind,
                bytes,
                permit,
            })
            .map_err(|_| MobileError::message("The connection closed."))?;
        model.sending = Some(peer);
        model.send_status = "Sending…".into();
        Ok(())
    }
}

impl Drop for Mobile {
    fn drop(&mut self) {
        self.engine.model().pause();
        if let Some((_, done)) = self.driver.take() {
            let _ = done.send(());
        }
    }
}

impl Model {
    fn next(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    fn cancel_pairing(&mut self) {
        if let Some(job) = self.pair.take() {
            job.task.abort();
        }
        self.pairing = PairState::Idle;
    }

    fn remove_peer(&mut self, peer: NoiseKey) {
        if let Some(peer) = self.peers.remove(&peer) {
            peer.task.abort();
        }
        self.finish_send(peer, "Connection changed before delivery was confirmed.");
    }

    fn finish_send(&mut self, peer: NoiseKey, message: &str) {
        if self.sending == Some(peer) {
            self.sending = None;
            self.send_status = message.into();
        }
    }

    fn pause(&mut self) {
        self.active = false;
        self.cancel_pairing();
        for peer in std::mem::take(&mut self.peers).into_values() {
            peer.task.abort();
        }
        self.hints.clear();
        self.sending = None;
        self.send_status.clear();
    }
}

impl Engine {
    fn model(&self) -> MutexGuard<'_, Model> {
        self.model.lock().expect("mobile state lock")
    }

    fn is_trusted(&self, peer: NoiseKey) -> bool {
        self.endpoint.is_trusted(peer).unwrap_or(false)
    }

    fn schedule(self: &Arc<Self>, runtime: &tokio::runtime::Handle, model: &mut Model) {
        if !model.active {
            return;
        }
        let Ok(devices) = self.endpoint.trusted_devices() else {
            return;
        };
        for device in devices {
            let key = device.noise_key;
            if model.peers.contains_key(&key) {
                continue;
            }
            let id = model.next();
            let engine = Arc::clone(self);
            let task = runtime.spawn(async move { engine.reconnect(key, id).await });
            model.peers.insert(
                key,
                Peer {
                    id,
                    task,
                    state: Connection::Offline,
                    sender: None,
                },
            );
        }
    }

    async fn pair(&self, id: u64, invitation: &str, address: SocketAddr) -> Result<(), String> {
        if !self.access.is_on_link(address.ip().to_string()) {
            return Err("Invitation address is outside the local network.".into());
        }
        let mut pending = lan::connect_pairing(&self.endpoint, invitation, &self.label)
            .await
            .map_err(|error| error.to_string())?;
        let (approval, mut approved) = oneshot::channel();
        {
            let mut model = self.model();
            let job = model
                .pair
                .as_mut()
                .filter(|job| job.id == id)
                .ok_or("Pairing canceled.")?;
            job.approval = Some(approval);
            model.pairing = PairState::Compare {
                code: pending.verification_code().as_str().to_owned(),
            };
        }
        let window = pending
            .approval_window()
            .map_err(|error| error.to_string())?;
        let code = timeout(window, async {
            loop {
                tokio::select! {
                    result = &mut approved => break result.map_err(|_| "Pairing canceled.".to_owned()),
                    input = pending.wait_for_input() => {
                        input.map_err(|error| error.to_string())?;
                        pending.receive_before_approval().await.map_err(|error| error.to_string())?;
                    }
                }
            }
        }).await.map_err(|_| "Pairing expired.")??;
        match pending
            .approve(&code)
            .await
            .map_err(|error| error.to_string())?
        {
            PairingStatus::Complete => Ok(()),
            _ => Err("Pairing ended before both devices confirmed.".into()),
        }
    }

    async fn reconnect(&self, key: NoiseKey, id: u64) {
        let mut delay = Duration::from_secs(1);
        let mut use_saved = false;
        loop {
            let saved = self
                .endpoint
                .trusted_devices()
                .ok()
                .and_then(|devices| devices.into_iter().find(|device| device.noise_key == key))
                .and_then(|device| device.last_known_address);
            let hint = self
                .model()
                .hints
                .get(&key)
                .filter(|(_, at)| at.elapsed() < HINT_LIFETIME)
                .map(|(address, _)| *address);
            let address = if use_saved {
                saved.or(hint)
            } else {
                hint.or(saved)
            };
            use_saved = !use_saved;
            if let Some(address) =
                address.filter(|address| self.access.is_on_link(address.ip().to_string()))
            {
                if !self.state(key, id, Connection::Connecting, None) {
                    return;
                }
                let connection = {
                    let Ok(_permit) = self.dialing.acquire().await else {
                        return;
                    };
                    lan::connect_session(&self.endpoint, address, key).await
                };
                if let Ok(session) = connection {
                    let (sender, jobs) = mpsc::channel(1);
                    if !self.state(key, id, Connection::Connected, Some(sender)) {
                        return;
                    }
                    delay = Duration::from_secs(1);
                    transfer::run(self, session, jobs).await;
                }
            }
            if !self.state(key, id, Connection::Offline, None) {
                return;
            }
            sleep(delay).await;
            delay = (delay * 2).min(RETRY_MAX);
        }
    }

    fn state(
        &self,
        key: NoiseKey,
        id: u64,
        state: Connection,
        sender: Option<mpsc::Sender<Job>>,
    ) -> bool {
        let mut model = self.model();
        let Some(peer) = model.peers.get_mut(&key).filter(|peer| peer.id == id) else {
            return false;
        };
        peer.state = state;
        peer.sender = sender;
        if state == Connection::Offline {
            model.finish_send(key, "Connection closed before delivery was confirmed.");
        }
        true
    }

    fn receive(&self, key: NoiseKey, payload: Payload) -> bool {
        let mut model = self.model();
        if !model.active || !self.is_trusted(key) {
            return false;
        }
        let sequence = model.next();
        model.latest = Some((sequence, key, payload));
        true
    }
}

fn parse_pin(id: &str) -> Result<NoiseKey, MobileError> {
    let mut key = [0; 32];
    hex::decode_to_slice(id, &mut key).map_err(|_| MobileError::message("Invalid device ID."))?;
    Ok(key)
}
