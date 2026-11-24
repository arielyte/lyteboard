use std::{collections::BTreeMap, net::SocketAddr, rc::Rc, sync::Arc, time::Duration};

use lyteboard_core::{
    bulk::Payload,
    peer::{Endpoint, SessionEvent, TrustedDevice, lan},
};
use lyteboard_protocol::{
    ipc::network::{ConnectionState, Delivery, HistoryDirection, MAX_TEST_TEXT_BYTES},
    peer::{BulkRecord, NoiseKey, PeerFailure, TransferId},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::spawn_local,
    time::{Instant, sleep_until, timeout_at},
};

use super::{
    Activity, Shared,
    bulk::{Batch, Incoming, Outgoing, PROGRESS_TIMEOUT},
    lan_error, peer_error,
    sync::Automatic,
};

const DELIVERY_LIMIT: usize = 64;
const MAX_DIALING: usize = 4;
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_LIFETIME: Duration = Duration::from_secs(3600);

type ChannelKey = (NoiseKey, bool);

#[derive(Default)]
pub(super) struct Sessions {
    channels: BTreeMap<ChannelKey, Channel>,
    deliveries: BTreeMap<u64, Transfer>,
    retry: BTreeMap<NoiseKey, Retry>,
    next_id: u64,
}

struct Channel {
    id: u64,
    jobs: mpsc::Sender<Job>,
    _cancel_on_drop: oneshot::Sender<()>,
    ready: Option<Instant>,
}

struct Mailbox {
    id: u64,
    jobs: mpsc::Receiver<Job>,
    canceled: oneshot::Receiver<()>,
}

struct Job {
    id: u64,
    content: Content,
    automatic: Option<Automatic>,
}

enum Content {
    Text(Arc<Payload>),
    Bulk(Arc<Batch>),
}

impl Content {
    fn payload(&self) -> &Arc<Payload> {
        match self {
            Self::Text(payload) => payload,
            Self::Bulk(batch) => &batch.payload,
        }
    }
}

enum Pending {
    Ack {
        id: TransferId,
        job: u64,
        deadline: Instant,
        content: Content,
    },
    Bulk(Outgoing),
}

impl Pending {
    fn deadline(&self) -> Instant {
        match self {
            Self::Ack { deadline, .. } => *deadline,
            Self::Bulk(transfer) => transfer.deadline(),
        }
    }

    fn job_for(&self, id: TransferId) -> Option<u64> {
        match self {
            Self::Ack {
                id: expected, job, ..
            } if *expected == id => Some(*job),
            Self::Bulk(transfer) if transfer.id == id => Some(transfer.job),
            _ => None,
        }
    }
}

struct Transfer {
    channel: u64,
    status: Delivery,
}

#[derive(Default)]
struct Retry {
    due: Option<Instant>,
    failures: u32,
    candidate: usize,
}

impl Retry {
    fn defer(&mut self, now: Instant, jitter: u32) {
        let ceiling = (1000 * (1u64 << self.failures.min(6))).min(60_000);
        let floor = ceiling / 2;
        let delay = floor + u64::from(jitter) % (ceiling - floor + 1);
        self.due = Some(now + Duration::from_millis(delay));
        self.failures = self.failures.saturating_add(1);
    }

    fn address(&mut self, now: Instant, candidates: &[SocketAddr]) -> Option<SocketAddr> {
        if candidates.is_empty() || self.due.is_some_and(|due| due > now) {
            return None;
        }
        let address = candidates[self.candidate % candidates.len()];
        self.candidate = self.candidate.wrapping_add(1);
        Some(address)
    }
}

impl Sessions {
    pub(super) fn connected(&self) -> usize {
        self.channels
            .values()
            .filter(|channel| channel.ready.is_some())
            .count()
    }

    pub(super) fn state(&self, peer: NoiseKey) -> ConnectionState {
        let inbound = self.channels.get(&(peer, false));
        let outbound = self.channels.get(&(peer, true));
        if inbound
            .iter()
            .chain(outbound.iter())
            .any(|channel| channel.ready.is_some())
        {
            ConnectionState::Online
        } else if inbound.is_some() || outbound.is_some() {
            ConnectionState::Connecting
        } else {
            ConnectionState::Offline
        }
    }

    pub(super) fn address_changed(&mut self, peer: NoiseKey) {
        if self
            .channels
            .get(&(peer, true))
            .is_some_and(|channel| channel.ready.is_none())
        {
            let channel = self.channels.remove(&(peer, true)).unwrap();
            self.finish_pending(channel.id, "Peer address changed during connection.");
        }
        self.retry.remove(&peer);
    }

    fn mark_ready(&mut self, key: ChannelKey, local: NoiseKey) -> bool {
        let Some(channel) = self.channels.get_mut(&key) else {
            return false;
        };
        channel.ready = Some(Instant::now());
        self.retry.remove(&key.0);
        if self
            .channels
            .get(&(key.0, !key.1))
            .is_some_and(|other| other.ready.is_some())
        {
            // Both peers choose the same socket independently. Keep the one initiated
            // by the lower public key once both confirmed paths exist.
            let keep_outbound = local < key.0;
            if let Some(displaced) = self.channels.remove(&(key.0, !keep_outbound)) {
                self.finish_pending(displaced.id, "Duplicate connection closed.");
            }
        }
        self.channels.contains_key(&key)
    }

    pub(super) fn delivery(&self, id: u64) -> Result<Delivery, String> {
        self.deliveries
            .get(&id)
            .map(|transfer| transfer.status.clone())
            .ok_or_else(|| "Delivery receipt is unavailable.".into())
    }

    pub(super) fn remove_peer(&mut self, peer: NoiseKey) {
        self.retry.remove(&peer);
        for key in [(peer, false), (peer, true)] {
            if let Some(channel) = self.channels.remove(&key) {
                self.finish_pending(channel.id, "Device removed.");
            }
        }
    }

    fn register(&mut self, key: ChannelKey) -> Mailbox {
        let id = self.id();
        let (send, jobs) = mpsc::channel(4);
        let (cancel, canceled) = oneshot::channel();
        self.channels.insert(
            key,
            Channel {
                id,
                jobs: send,
                _cancel_on_drop: cancel,
                ready: None,
            },
        );
        Mailbox { id, jobs, canceled }
    }

    pub(super) fn queue_automatic(
        &mut self,
        peer: NoiseKey,
        batch: Arc<Batch>,
        automatic: Automatic,
    ) {
        let _ = self.enqueue(peer, Content::Bulk(batch), Some(automatic));
    }

    fn enqueue(
        &mut self,
        peer: NoiseKey,
        content: Content,
        automatic: Option<Automatic>,
    ) -> Result<u64, String> {
        let channel = self
            .channels
            .get(&(peer, false))
            .filter(|channel| channel.ready.is_some())
            .or_else(|| self.channels.get(&(peer, true)))
            .ok_or("Peer connection is unavailable.")?;
        let channel_id = channel.id;
        if let Some(automatic) = &automatic
            && (!automatic.is_current()
                || channel.ready.is_none_or(|ready| ready > automatic.observed)
                || self.deliveries.values().any(|transfer| {
                    transfer.channel == channel_id && matches!(transfer.status, Delivery::Pending)
                }))
        {
            return Err("Peer was offline or busy when the clipboard changed.".into());
        }
        let jobs = channel.jobs.clone();
        let id = self.delivery_slot()?;
        jobs.try_send(Job {
            id,
            content,
            automatic,
        })
        .map_err(|_| "Peer connection is busy or closed.")?;
        self.deliveries.insert(
            id,
            Transfer {
                channel: channel_id,
                status: Delivery::Pending,
            },
        );
        Ok(id)
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn delivery_slot(&mut self) -> Result<u64, String> {
        if self.deliveries.len() >= DELIVERY_LIMIT {
            let completed = self
                .deliveries
                .iter()
                .find_map(|(&id, transfer)| {
                    (!matches!(transfer.status, Delivery::Pending)).then_some(id)
                })
                .ok_or("Too many pending deliveries.")?;
            self.deliveries.remove(&completed);
        }
        Ok(self.id())
    }

    fn finish(&mut self, key: ChannelKey, id: u64, reason: &str) {
        if self
            .channels
            .get(&key)
            .is_some_and(|channel| channel.id == id)
        {
            self.channels.remove(&key);
            if self.state(key.0) == ConnectionState::Offline {
                self.retry
                    .entry(key.0)
                    .or_default()
                    .defer(Instant::now(), getrandom::u32().unwrap_or(0));
            }
        }
        self.finish_pending(id, reason);
    }

    fn finish_pending(&mut self, id: u64, reason: &str) {
        for transfer in self.deliveries.values_mut() {
            if transfer.channel == id && matches!(transfer.status, Delivery::Pending) {
                transfer.status = Delivery::Failed(reason.into());
            }
        }
    }

    fn acknowledge(&mut self, id: u64, result: Delivery) {
        if let Some(transfer) = self.deliveries.get_mut(&id)
            && matches!(transfer.status, Delivery::Pending)
        {
            transfer.status = result;
        }
    }
}

pub(super) fn queue_text(
    activity: &Shared,
    endpoint: Rc<Endpoint>,
    peer: NoiseKey,
    text: String,
) -> Result<u64, String> {
    if text.is_empty() || text.len() > MAX_TEST_TEXT_BYTES || text.contains('\0') {
        return Err(format!(
            "Text must be 1–{MAX_TEST_TEXT_BYTES} UTF-8 bytes without NUL."
        ));
    }
    let payload = Payload::text(text).map_err(|error| error.to_string())?;
    queue(activity, endpoint, peer, Content::Text(Arc::new(payload)))
}

pub(super) fn queue_bulk(
    activity: &Shared,
    endpoint: Rc<Endpoint>,
    peer: NoiseKey,
    batch: Arc<Batch>,
) -> Result<u64, String> {
    queue(activity, endpoint, peer, Content::Bulk(batch))
}

fn queue(
    activity: &Shared,
    endpoint: Rc<Endpoint>,
    peer: NoiseKey,
    content: Content,
) -> Result<u64, String> {
    let device = endpoint
        .trusted_devices()
        .map_err(peer_error)?
        .into_iter()
        .find(|device| device.noise_key == peer)
        .ok_or("Device is not trusted.")?;
    if activity.borrow().sessions.state(peer) == ConnectionState::Offline {
        start_dial(activity, endpoint, &device)?;
    }
    activity.borrow_mut().sessions.enqueue(peer, content, None)
}

pub(super) fn reconnect(activity: &Shared, endpoint: &Rc<Endpoint>) {
    let Ok(devices) = endpoint.trusted_devices() else {
        return;
    };
    activity
        .borrow_mut()
        .sessions
        .retry
        .retain(|peer, _| devices.iter().any(|device| device.noise_key == *peer));
    for device in devices {
        let _ = start_dial(activity, Rc::clone(endpoint), &device);
    }
}

fn start_dial(
    activity: &Shared,
    endpoint: Rc<Endpoint>,
    device: &TrustedDevice,
) -> Result<(), &'static str> {
    let peer = device.noise_key;
    let mut state = activity.borrow_mut();
    let Activity {
        sessions,
        hints,
        networks,
        ..
    } = &mut *state;
    if sessions.state(peer) != ConnectionState::Offline {
        return Ok(());
    }
    if sessions
        .channels
        .values()
        .filter(|channel| channel.ready.is_none())
        .count()
        >= MAX_DIALING
    {
        return Err("Too many connections in progress.");
    }
    // Always leave a place for the last authenticated or manually configured hint.
    // Rotate candidates across attempts so a forged record cannot monopolize the dialer.
    let mut candidates = Vec::new();
    for address in device
        .last_known_address
        .into_iter()
        .chain(hints.addresses(&peer).iter().copied())
    {
        if networks.contains(address) && !candidates.contains(&address) {
            candidates.push(address);
        }
    }
    if candidates.is_empty() {
        return Err("No local address. Use `devices address` or enable discovery.");
    }
    let address = sessions
        .retry
        .entry(peer)
        .or_default()
        .address(Instant::now(), &candidates)
        .ok_or("Peer is offline. Reconnect is scheduled.")?;
    let key = (peer, true);
    let Mailbox {
        id,
        jobs,
        mut canceled,
    } = sessions.register(key);
    let activity = Rc::clone(activity);
    spawn_local(async move {
        let result = tokio::select! {
            biased;
            _ = &mut canceled => Err("Connection closed.".into()),
            result = async {
                let session = lan::connect_session(&endpoint, address, peer).await.map_err(lan_error)?;
                if !activity.borrow_mut().sessions.mark_ready(key, endpoint.noise_key()) {
                    return Err("Duplicate connection closed.".into());
                }
                run_channel(&activity, session, jobs).await
            } => result,
        };
        finish(&activity, key, id, result);
    });
    Ok(())
}

pub(super) async fn incoming(activity: &Shared, session: lan::TcpSession<'_>, local: NoiseKey) {
    let key = (session.peer_key(), false);
    let Mailbox { id, jobs, canceled } = {
        let mut state = activity.borrow_mut();
        let mailbox = state.sessions.register(key);
        state.sessions.mark_ready(key, local);
        mailbox
    };
    let result = tokio::select! {
        biased;
        _ = canceled => Err("Connection closed.".into()),
        result = run_channel(activity, session, jobs) => result,
    };
    finish(activity, key, id, result);
}

fn finish(activity: &Shared, key: ChannelKey, id: u64, result: Result<(), String>) {
    let reason = result
        .err()
        .unwrap_or_else(|| "Connection closed before delivery was acknowledged.".into());
    activity.borrow_mut().sessions.finish(key, id, &reason);
}

async fn run_channel(
    activity: &Shared,
    mut session: lan::TcpSession<'_>,
    mut jobs: mpsc::Receiver<Job>,
) -> Result<(), String> {
    let peer = session.peer_key();
    let expires = Instant::now() + SESSION_LIFETIME;
    let mut pending: Option<Pending> = None;
    let mut incoming: Option<Incoming> = None;
    loop {
        let deadline = expires
            .min(pending.as_ref().map_or(expires, Pending::deadline))
            .min(incoming.as_ref().map_or(expires, Incoming::deadline));
        tokio::select! {
            biased;
            _ = sleep_until(deadline) => return Err("Transfer or session timed out.".into()),
            job = jobs.recv(), if pending.is_none() => {
                let Some(job) = job else { return Ok(()) };
                if job.automatic.as_ref().is_some_and(|automatic| !automatic.is_current()) {
                    activity.borrow_mut().sessions.acknowledge(job.id, Delivery::Failed("Automatic clipboard update expired.".into()));
                } else {
                    pending = Some(start_transfer(&mut session, job).await?);
                }
            }
            readable = session.wait_for_input() => {
                readable.map_err(lan_error)?;
                let event = timeout_at(deadline, session.receive()).await
                    .map_err(|_| "Transfer timed out.")?.map_err(lan_error)?;
                match event {
                    SessionEvent::Text { id, text } => {
                        if text.len() > MAX_TEST_TEXT_BYTES {
                            session.reject_payload(id, PeerFailure::TooLarge).await.map_err(lan_error)?;
                        } else {
                            let payload = Payload::text(text).map_err(|error| error.to_string())?;
                            publish(activity, peer, payload).await;
                            session.acknowledge(id).await.map_err(lan_error)?;
                        }
                    }
                    SessionEvent::Bulk(BulkRecord::Credit { id, next }) => {
                        let Some(Pending::Bulk(mut transfer)) = pending.take() else {
                            return Err("Unexpected bulk credit.".into());
                        };
                        let record = transfer.credit(id, next).map_err(|error| error.to_string())?;
                        pending = Some(if matches!(record, BulkRecord::End { .. }) {
                            Pending::Ack {
                                id, job: transfer.job, deadline: transfer.expires.min(Instant::now() + ACK_TIMEOUT),
                                content: Content::Bulk(transfer.into_batch()),
                            }
                        } else {
                            Pending::Bulk(transfer)
                        });
                        session.send_bulk(record).await.map_err(lan_error)?;
                    }
                    SessionEvent::Bulk(record) => handle_bulk(activity, &mut session, &mut incoming, record).await?,
                    SessionEvent::DeliveryAck { id, .. } => {
                        let Some(Pending::Ack { id: expected, job, content, .. }) = pending.take() else {
                            return Err("Unexpected delivery acknowledgment.".into());
                        };
                        if expected != id {
                            return Err("Unexpected delivery acknowledgment.".into());
                        }
                        let history = activity.borrow().history.clone();
                        history.record(HistoryDirection::Sent, peer, content.payload()).await;
                        activity.borrow_mut().sessions.acknowledge(job, Delivery::Delivered);
                    }
                    SessionEvent::PeerError { id, code } => {
                        let Some(job) = pending.as_ref().and_then(|transfer| transfer.job_for(id?)) else {
                            return Err("Unexpected peer error.".into());
                        };
                        pending = None;
                        activity.borrow_mut().sessions.acknowledge(job, Delivery::Failed(failure_message(code).into()));
                    }
                    SessionEvent::Closed => return Ok(()),
                    _ => return Err("Unexpected peer record.".into()),
                }
            }
        }
    }
}

async fn start_transfer(session: &mut lan::TcpSession<'_>, job: Job) -> Result<Pending, String> {
    match job.content {
        Content::Text(payload) => {
            let text = payload.as_text().expect("inline text payload");
            let id = session.send_text(text).await.map_err(lan_error)?;
            Ok(Pending::Ack {
                id,
                job: job.id,
                deadline: Instant::now() + ACK_TIMEOUT,
                content: Content::Text(payload),
            })
        }
        Content::Bulk(batch) => {
            let id = lan::TcpSession::transfer_id().map_err(lan_error)?;
            let transfer = Outgoing::new(id, job.id, batch);
            session
                .send_bulk(BulkRecord::Begin(transfer.manifest()))
                .await
                .map_err(lan_error)?;
            Ok(Pending::Bulk(transfer))
        }
    }
}

async fn handle_bulk(
    activity: &Shared,
    session: &mut lan::TcpSession<'_>,
    incoming: &mut Option<Incoming>,
    record: BulkRecord,
) -> Result<(), String> {
    match record {
        BulkRecord::Begin(manifest) => {
            let slot = activity.borrow().incoming_bulk.clone().try_acquire_owned();
            match slot {
                Ok(slot) if incoming.is_none() => match Incoming::new(manifest, slot) {
                    Ok(transfer) => {
                        *incoming = Some(transfer);
                        session
                            .send_bulk(BulkRecord::Credit {
                                id: manifest.id,
                                next: 0,
                            })
                            .await
                            .map_err(lan_error)
                    }
                    Err(error) => session
                        .reject_payload(manifest.id, error.peer_failure())
                        .await
                        .map_err(lan_error),
                },
                _ => session
                    .reject_payload(manifest.id, PeerFailure::Busy)
                    .await
                    .map_err(lan_error),
            }
        }
        BulkRecord::Chunk { id, index, bytes } => {
            let transfer = incoming.as_mut().ok_or("Unexpected bulk chunk.")?;
            let next = transfer
                .chunk(id, index, &bytes)
                .map_err(|error| error.to_string())?;
            session
                .send_bulk(BulkRecord::Credit { id, next })
                .await
                .map_err(lan_error)
        }
        BulkRecord::End { id } => {
            let transfer = incoming.take().ok_or("Unexpected bulk end.")?;
            let deadline = transfer.deadline().min(Instant::now() + PROGRESS_TIMEOUT);
            match timeout_at(deadline, transfer.finish(id)).await {
                Ok(Ok((payload, _slot))) => {
                    // Image validation yields to the worker. Recheck the lease before publishing.
                    session.ensure_active().map_err(lan_error)?;
                    publish(activity, session.peer_key(), payload).await;
                    session.acknowledge(id).await.map_err(lan_error)?;
                    Ok(())
                }
                Ok(Err(error)) => session
                    .reject_payload(id, error.peer_failure())
                    .await
                    .map_err(lan_error),
                Err(_) => Err("Bulk validation timed out.".into()),
            }
        }
        BulkRecord::Credit { .. } => Err("Unexpected bulk credit.".into()),
    }
}

async fn publish(activity: &Shared, peer: NoiseKey, payload: Payload) {
    let payload = Arc::new(payload);
    let history = {
        let mut state = activity.borrow_mut();
        state.clipboard.received(&payload);
        state.inbox.push(peer, Arc::clone(&payload));
        state.history.clone()
    };
    history
        .record(HistoryDirection::Received, peer, &payload)
        .await;
}

fn failure_message(code: PeerFailure) -> &'static str {
    match code {
        PeerFailure::TooLarge => "Peer rejected the payload size or image dimensions.",
        PeerFailure::InvalidPayload => "Peer rejected the payload format.",
        PeerFailure::IntegrityFailed => "Peer rejected the payload digest.",
        PeerFailure::UnsupportedMedia => "Peer does not support this media.",
        PeerFailure::Busy => "Peer bulk receiving is busy.",
        PeerFailure::TimedOut => "Peer transfer timed out.",
        PeerFailure::Denied => "Peer denied the transfer.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_is_bounded_jittered_and_rotates_hints() {
        let now = Instant::now();
        let addresses = [
            "127.0.0.1:8080".parse().unwrap(),
            "127.0.0.2:8080".parse().unwrap(),
        ];
        let mut retry = Retry::default();
        assert_eq!(retry.address(now, &addresses), Some(addresses[0]));
        retry.defer(now, 123);
        let due = retry.due.unwrap();
        assert_eq!(due - now, Duration::from_millis(623));
        assert_eq!(
            retry.address(due - Duration::from_millis(1), &addresses),
            None
        );
        assert_eq!(retry.address(due, &addresses), Some(addresses[1]));
        retry.defer(now, 999);
        assert_eq!(retry.due.unwrap() - now, Duration::from_millis(1999));
        for _ in 0..20 {
            retry.defer(now, u32::MAX);
            assert!(retry.due.unwrap() - now <= Duration::from_secs(60));
        }
        assert!(retry.due.unwrap() - now >= Duration::from_secs(30));
    }

    #[test]
    fn crossed_connections_keep_the_same_socket_on_both_peers() {
        for local in [[1; 32], [2; 32]] {
            let peer = if local == [1; 32] { [2; 32] } else { [1; 32] };
            for first in [false, true] {
                let mut sessions = Sessions::default();
                let mut one = sessions.register((peer, first));
                assert_eq!(sessions.state(peer), ConnectionState::Connecting);
                assert!(sessions.mark_ready((peer, first), local));
                let mut two = sessions.register((peer, !first));
                sessions.mark_ready((peer, !first), local);
                assert_eq!(sessions.connected(), 1);
                let outbound = local < peer;
                assert!(sessions.channels.contains_key(&(peer, outbound)));
                if first == outbound {
                    assert!(two.canceled.try_recv().is_err());
                    assert!(!one.canceled.is_terminated());
                } else {
                    assert!(one.canceled.try_recv().is_err());
                    assert!(!two.canceled.is_terminated());
                }
                sessions.remove_peer(peer);
                assert_eq!(sessions.state(peer), ConnectionState::Offline);
                assert!(!sessions.retry.contains_key(&peer));
            }
        }
    }
}
