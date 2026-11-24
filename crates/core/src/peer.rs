//! In-process v1 peer identity, pairing, and authenticated session state.

mod crypto;
mod identity;
mod pairing;
mod session;
mod trust;

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use lyteboard_protocol::peer::{Context, FrameKind, Origin, RouteId, RoutedFrame, WireError};
use thiserror::Error;

pub use crypto::CryptoError;
pub use identity::{CredentialError, CredentialStore, Credentials, DeviceIdentity};
pub use pairing::{Pairing, PairingStatus, VerificationCode};
pub use session::{Session, SessionEvent};
pub use trust::TrustedDevice;

#[derive(Clone, Copy)]
pub struct Time {
    pub unix_seconds: u64,
    /// A consistent, suspend-aware monotonic clock sample for this endpoint.
    pub monotonic: Duration,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("operating system random source unavailable")]
    Random,
    #[error("peer state unavailable")]
    Unavailable,
    #[error("peer limit reached")]
    Limit,
    #[error("invitation unavailable")]
    Invitation,
    #[error("operation expired")]
    Expired,
    #[error("peer is not trusted")]
    Untrusted,
    #[error("peer key binding overlaps an existing record")]
    ConflictingIdentity,
    #[error("unexpected peer message or protocol state")]
    Unexpected,
    #[error("peer route proof or identity did not match")]
    Authentication,
    #[error("pairing verification code did not match")]
    Verification,
    #[error("pairing was rejected")]
    Rejected,
    #[error("peer channel is closed")]
    Closed,
}

pub struct Endpoint {
    identity: DeviceIdentity,
    origin: Origin,
    state: Mutex<State>,
}

impl Endpoint {
    pub fn new(identity: DeviceIdentity, origin: Origin) -> Self {
        Self {
            identity,
            origin,
            state: Mutex::default(),
        }
    }

    pub fn route_id(&self) -> RouteId {
        self.identity.route_id()
    }

    pub fn noise_key(&self) -> [u8; 32] {
        self.identity.noise_key()
    }

    pub fn is_trusted(&self, route: RouteId) -> Result<bool, Error> {
        Ok(self.state()?.trust.active(&route).is_some())
    }

    pub fn trusted_devices(&self) -> Result<Vec<TrustedDevice>, Error> {
        Ok(self.state()?.trust.active_devices())
    }

    /// Release expired pairing and handshake reservations. Admission also runs this sweep.
    pub fn expire_pending(&self, now: Time) -> Result<(), Error> {
        self.state_at(now).map(|_| ())
    }

    /// Revoke an existing binding and invalidate any channels using its route.
    pub fn revoke(&self, route: RouteId) -> Result<(), Error> {
        let mut state = self.state()?;
        state.trust.revoke(&route)?;
        state.disconnect(route);
        Ok(())
    }

    /// Explicitly discard a binding, for example before re-pairing after an interrupted commit.
    pub fn forget(&self, route: RouteId) -> Result<(), Error> {
        let mut state = self.state()?;
        state.trust.forget(&route)?;
        state.disconnect(route);
        Ok(())
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, Error> {
        self.state.lock().map_err(|_| Error::Unavailable)
    }

    fn state_at(&self, now: Time) -> Result<MutexGuard<'_, State>, Error> {
        let mut state = self.state()?;
        if state
            .invite
            .as_ref()
            .is_some_and(|invite| invite.expired(now))
        {
            state.clear_invite();
        }
        let expired: Vec<_> = state
            .pending
            .iter()
            .filter_map(|(id, pending)| {
                let elapsed = now.monotonic.checked_sub(pending.started.monotonic)?;
                (elapsed >= pending.limit
                    || pending
                        .wall_deadline
                        .is_some_and(|deadline| now.unix_seconds > deadline))
                .then_some(*id)
            })
            .collect();
        for id in expired {
            state.release(id);
        }
        Ok(state)
    }

    fn frame(
        &self,
        context: &Context,
        initiator: bool,
        kind: FrameKind,
        body: Vec<u8>,
    ) -> RoutedFrame {
        RoutedFrame {
            to: remote_route(context, initiator),
            channel: context.channel,
            kind,
            selector: if kind == FrameKind::PairingOpen {
                context.invite
            } else {
                None
            },
            body,
        }
    }

    fn receive_frame(
        &self,
        context: &Context,
        initiator: bool,
        from: RouteId,
        frame: &RoutedFrame,
        kind: FrameKind,
    ) -> Result<(), Error> {
        frame.validate()?;
        if frame.to != self.route_id()
            || from != remote_route(context, initiator)
            || frame.channel != context.channel
            || frame.kind != kind
        {
            return Err(Error::Unexpected);
        }
        Ok(())
    }
}

fn remote_route(context: &Context, initiator: bool) -> RouteId {
    if initiator {
        context.responder
    } else {
        context.initiator
    }
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

fn within(start: Time, now: Time, limit: Duration) -> bool {
    now.monotonic
        .checked_sub(start.monotonic)
        .is_some_and(|elapsed| elapsed < limit)
}

fn random<const N: usize>(bytes: &mut [u8; N]) -> Result<(), Error> {
    getrandom::fill(bytes).map_err(|_| Error::Random)
}

#[derive(Default)]
struct State {
    invite: Option<pairing::LocalInvite>,
    trust: trust::Registry,
    pending: BTreeMap<u64, Pending>,
    live: BTreeMap<Slot, u64>,
    next_id: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Slot {
    route: RouteId,
    initiator: bool,
}

struct Pending {
    slot: Slot,
    started: Time,
    limit: Duration,
    wall_deadline: Option<u64>,
}

impl State {
    fn check_capacity(&self, slot: Slot) -> Result<(), Error> {
        if self.pending.len() >= 8 || self.pending.values().any(|pending| pending.slot == slot) {
            return Err(Error::Limit);
        }
        Ok(())
    }

    fn reserve(
        &mut self,
        slot: Slot,
        started: Time,
        wall_deadline: Option<u64>,
    ) -> Result<u64, Error> {
        self.check_capacity(slot)?;
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(Error::Limit)?;
        self.pending.insert(
            id,
            Pending {
                slot,
                started,
                limit: HANDSHAKE_TIMEOUT,
                wall_deadline,
            },
        );
        Ok(id)
    }

    fn check(&self, slot: Slot, id: u64) -> Result<(), Error> {
        if self
            .pending
            .get(&id)
            .is_some_and(|pending| pending.slot == slot)
            || self.live.get(&slot) == Some(&id)
        {
            Ok(())
        } else {
            Err(Error::Closed)
        }
    }

    fn clear_invite(&mut self) {
        if let Some(invite) = self.invite.take()
            && let Some(id) = invite.reserved_by
        {
            self.release(id);
        }
    }

    fn release(&mut self, id: u64) {
        self.pending.remove(&id);
        self.live.retain(|_, owner| *owner != id);
        self.trust.discard(id);
        if self
            .invite
            .as_ref()
            .is_some_and(|invite| invite.reserved_by == Some(id))
        {
            self.invite = None;
        }
    }

    fn disconnect(&mut self, route: RouteId) {
        let pending: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(id, pending)| (pending.slot.route == route).then_some(*id))
            .collect();
        for id in pending {
            self.release(id);
        }
        self.live.retain(|slot, _| slot.route != route);
    }
}

struct Lease<'a> {
    endpoint: &'a Endpoint,
    id: u64,
    slot: Slot,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.endpoint.state() {
            state.release(self.id);
        }
    }
}
