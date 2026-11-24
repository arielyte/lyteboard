use std::{fmt, net::SocketAddr, time::Duration};

use lyteboard_protocol::peer::{
    FrameKind, Invite, InviteId, LanInvite, NoiseKey, Path, QrCode, Record, RouteId, RoutedFrame,
    route_proof_message, validate_label,
};
use zeroize::Zeroizing;

use super::{
    Endpoint, Error, HANDSHAKE_TIMEOUT, Lease, PeerContext, Slot, State, Time, TrustedDevice,
    crypto::{Handshake, Transport},
    identity::{valid_route, verify_route},
    random, remote_route, within,
};

const INVITE_TTL: Duration = Duration::from_secs(300);
const CLOCK_SKEW: u64 = 60;

pub(super) struct LocalInvite {
    id: InviteId,
    created: Time,
    expires_at: u64,
    path: Path,
    available: Option<Zeroizing<[u8; 32]>>,
    pub(super) reserved_by: Option<u64>,
}

impl LocalInvite {
    fn valid(&self, now: Time) -> bool {
        now.unix_seconds < self.expires_at && within(self.created, now, INVITE_TTL)
    }

    pub(super) fn expired(&self, now: Time) -> bool {
        now.monotonic >= self.created.monotonic && !self.valid(now)
    }
}

struct PairTarget<'a> {
    path: Path,
    route: RouteId,
    key: NoiseKey,
    id: InviteId,
    psk: &'a [u8; 32],
    expires_at: u64,
}

impl Endpoint {
    /// Issue one new invitation. Keep the returned QR text in a trusted display or import channel.
    pub fn create_invite(&self, now: Time) -> Result<QrCode, Error> {
        self.issue_invite(None, now)
    }

    pub(super) fn create_lan_invite(
        &self,
        address: SocketAddr,
        now: Time,
    ) -> Result<QrCode, Error> {
        self.issue_invite(Some(address), now)
    }

    fn issue_invite(&self, address: Option<SocketAddr>, now: Time) -> Result<QrCode, Error> {
        let mut state = self.state_at(now)?;
        if state.invite.is_some() {
            return Err(Error::Invitation);
        }
        let mut id = [0; 16];
        let mut psk = Zeroizing::new([0; 32]);
        random(&mut id)?;
        random(&mut psk)?;
        let expires_at = now
            .unix_seconds
            .checked_add(INVITE_TTL.as_secs())
            .ok_or(Error::Expired)?;
        let (path, qr) = if let Some(address) = address {
            (
                Path::DirectTcp,
                LanInvite::new(
                    address,
                    id,
                    Zeroizing::new(*psk),
                    self.noise_key(),
                    expires_at,
                )?
                .encode_qr(),
            )
        } else {
            let origin = self.origin.clone().ok_or(Error::Unexpected)?;
            (
                Path::Relay,
                Invite::new(
                    origin,
                    id,
                    Zeroizing::new(*psk),
                    self.noise_key(),
                    self.route_id(),
                    expires_at,
                )
                .encode_qr(),
            )
        };
        state.invite = Some(LocalInvite {
            id,
            created: now,
            expires_at,
            path,
            available: Some(psk),
            reserved_by: None,
        });
        Ok(qr)
    }

    pub fn cancel_invite(&self) -> Result<(), Error> {
        self.state()?.clear_invite();
        Ok(())
    }

    pub fn begin_pairing(
        &self,
        qr: &str,
        label: &str,
        now: Time,
    ) -> Result<(Pairing<'_>, RoutedFrame), Error> {
        let invite = Invite::decode_qr(qr)?;
        if self.origin.as_ref() != Some(&invite.origin) || !valid_route(&invite.route) {
            return Err(Error::Invitation);
        }
        self.begin_pairing_with(
            PairTarget {
                path: Path::Relay,
                route: invite.route,
                key: invite.noise_key,
                id: invite.id,
                psk: invite.psk(),
                expires_at: invite.expires_at,
            },
            label,
            now,
        )
    }

    pub(super) fn begin_lan_pairing(
        &self,
        invite: &LanInvite,
        label: &str,
        now: Time,
    ) -> Result<(Pairing<'_>, RoutedFrame), Error> {
        self.begin_pairing_with(
            PairTarget {
                path: Path::DirectTcp,
                route: invite.noise_key,
                key: invite.noise_key,
                id: invite.id,
                psk: invite.psk(),
                expires_at: invite.expires_at,
            },
            label,
            now,
        )
    }

    fn begin_pairing_with(
        &self,
        invite: PairTarget<'_>,
        label: &str,
        now: Time,
    ) -> Result<(Pairing<'_>, RoutedFrame), Error> {
        validate_label(label)?;
        if invite.route == self.peer_id(invite.path) || invite.key == self.noise_key() {
            return Err(Error::Invitation);
        }
        let skew_deadline = invite.expires_at.saturating_add(CLOCK_SKEW);
        if now.unix_seconds > skew_deadline
            || invite.expires_at
                > now
                    .unix_seconds
                    .saturating_add(INVITE_TTL.as_secs() + CLOCK_SKEW)
        {
            return Err(Error::Expired);
        }
        let mut channel = [0; 16];
        random(&mut channel)?;
        let context = PeerContext {
            path: invite.path,
            channel,
            initiator: self.peer_id(invite.path),
            responder: invite.route,
            invite: Some(invite.id),
        };
        let slot = Slot {
            route: invite.route,
            initiator: true,
        };
        let mut state = self.state_at(now)?;
        state.check_capacity(slot)?;
        let mut handshake = Handshake::new(
            &self.identity,
            true,
            &self.encode_context(&context)?,
            Some(&invite.key),
            Some(invite.psk),
        )?;
        let first = self.frame(&context, true, FrameKind::PairingOpen, handshake.write()?);
        let id = state.reserve(slot, now, Some(skew_deadline))?;
        Ok((
            Pairing {
                lease: Some(Lease {
                    endpoint: self,
                    id,
                    slot,
                }),
                context,
                started: now,
                scanner_limit: Some((skew_deadline, invite.key)),
                label: label.into(),
                phase: Some(Phase::Handshake(handshake)),
                status: PairingStatus::InProgress,
            },
            first,
        ))
    }

    pub fn accept_pairing(
        &self,
        from: RouteId,
        first: RoutedFrame,
        label: &str,
        now: Time,
    ) -> Result<(Pairing<'_>, RoutedFrame), Error> {
        self.accept_pairing_with(Path::Relay, from, first, label, now)
    }

    pub(super) fn accept_pairing_with(
        &self,
        path: Path,
        from: RouteId,
        first: RoutedFrame,
        label: &str,
        now: Time,
    ) -> Result<(Pairing<'_>, RoutedFrame), Error> {
        first.validate()?;
        validate_label(label)?;
        if first.kind != FrameKind::PairingOpen
            || first.to != self.peer_id(path)
            || from == self.peer_id(path)
            || (path == Path::Relay && !valid_route(&from))
        {
            return Err(Error::Invitation);
        }
        let mut state = self.state_at(now)?;
        let invite = state
            .invite
            .as_ref()
            .filter(|invite| {
                invite.path == path && first.selector == Some(invite.id) && invite.valid(now)
            })
            .and_then(|invite| invite.available.as_ref())
            .ok_or(Error::Invitation)?;
        let context = PeerContext {
            path,
            channel: first.channel,
            initiator: from,
            responder: self.peer_id(path),
            invite: first.selector,
        };
        let slot = Slot {
            route: from,
            initiator: false,
        };
        state.check_capacity(slot)?;
        let mut handshake = Handshake::new(
            &self.identity,
            false,
            &self.encode_context(&context)?,
            None,
            Some(invite),
        )?;
        handshake.read(&first.body)?;
        // Only an authenticated first flight consumes the invitation. Keep the lock across both actions.
        let id = state.reserve(slot, now, None)?;
        let invite = state
            .invite
            .as_mut()
            .expect("invitation held under the same lock");
        invite.available = None;
        invite.reserved_by = Some(id);
        let body = match handshake.write() {
            Ok(body) => body,
            Err(error) => {
                state.release(id);
                return Err(error.into());
            }
        };
        let second = self.frame(&context, false, FrameKind::Handshake, body);
        Ok((
            Pairing {
                lease: Some(Lease {
                    endpoint: self,
                    id,
                    slot,
                }),
                context,
                started: now,
                scanner_limit: None,
                label: label.into(),
                phase: Some(Phase::Handshake(handshake)),
                status: PairingStatus::InProgress,
            },
            second,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStatus {
    InProgress,
    /// The inviter saved local trust and emitted the final message. Delivery is unconfirmed.
    LocalTrustSaved,
    /// The scanner received the final authenticated confirmation and saved trust.
    Complete,
    Closed,
}

pub struct VerificationCode(Zeroizing<String>);

impl VerificationCode {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for VerificationCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VerificationCode([redacted])")
    }
}

pub struct Pairing<'a> {
    lease: Option<Lease<'a>>,
    context: PeerContext,
    started: Time,
    scanner_limit: Option<(u64, [u8; 32])>,
    label: String,
    phase: Option<Phase>,
    status: PairingStatus,
}

enum Phase {
    Handshake(Handshake),
    Confirm(Box<Confirmation>),
}

struct Confirmation {
    transport: Transport,
    hash: Zeroizing<[u8; 32]>,
    noise_key: [u8; 32],
    label: Option<String>,
    code: VerificationCode,
    step: Step,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Hello,
    Compare,
    LocalApproved,
    RemoteApproved,
    WaitCommit,
    WaitStored,
    WaitDone,
}

impl Pairing<'_> {
    pub fn status(&self) -> PairingStatus {
        self.status
    }

    pub fn verification_code(&self) -> Option<&VerificationCode> {
        match self.phase.as_ref()? {
            Phase::Confirm(confirmation) if confirmation.step != Step::Hello => {
                Some(&confirmation.code)
            }
            _ => None,
        }
    }

    pub fn receive(
        &mut self,
        from: RouteId,
        frame: RoutedFrame,
        now: Time,
    ) -> Result<Vec<RoutedFrame>, Error> {
        let result = self.receive_step(from, frame, now);
        if result.is_err() || self.phase.is_none() {
            self.cancel();
        }
        result
    }

    fn receive_step(
        &mut self,
        from: RouteId,
        frame: RoutedFrame,
        now: Time,
    ) -> Result<Vec<RoutedFrame>, Error> {
        let lease = self.lease.as_ref().ok_or(Error::Closed)?;
        let endpoint = lease.endpoint;
        let initiator = lease.slot.initiator;
        let mut state = endpoint.state()?;
        self.check(&state, now)?;
        match self.phase.take().ok_or(Error::Closed)? {
            Phase::Handshake(mut handshake) => {
                endpoint.receive_frame(
                    &self.context,
                    initiator,
                    from,
                    &frame,
                    FrameKind::Handshake,
                )?;
                handshake.read(&frame.body)?;
                let mut output = Vec::new();
                if initiator {
                    output.push(endpoint.frame(
                        &self.context,
                        true,
                        FrameKind::Handshake,
                        handshake.write()?,
                    ));
                }
                let (hash, noise_key, transport) = handshake.finish()?;
                if noise_key == endpoint.noise_key()
                    || self.scanner_limit.is_some_and(|(_, pin)| noise_key != pin)
                    || (self.context.path == Path::DirectTcp && noise_key != from)
                {
                    return Err(Error::Authentication);
                }
                let hash = Zeroizing::new(hash);
                let mut confirmation = Confirmation {
                    code: comparison_code(&hash),
                    hash,
                    noise_key,
                    transport,
                    label: None,
                    step: Step::Hello,
                };
                let hello = if self.context.path == Path::DirectTcp {
                    Record::LanHello {
                        label: self.label.clone(),
                    }
                } else {
                    let proof_message = Zeroizing::new(route_proof_message(
                        &confirmation.hash,
                        &endpoint.route_id(),
                    ));
                    Record::Hello {
                        label: self.label.clone(),
                        proof: endpoint.identity.sign(&proof_message),
                    }
                };
                output.push(encrypt(
                    endpoint,
                    &self.context,
                    initiator,
                    &mut confirmation,
                    hello,
                )?);
                state.pending.get_mut(&lease.id).ok_or(Error::Closed)?.limit = INVITE_TTL;
                self.phase = Some(Phase::Confirm(Box::new(confirmation)));
                Ok(output)
            }
            Phase::Confirm(mut confirmation) => {
                endpoint.receive_frame(
                    &self.context,
                    initiator,
                    from,
                    &frame,
                    FrameKind::Transport,
                )?;
                let plaintext = confirmation.transport.open(&frame.body)?;
                let record = Record::decode(&plaintext)?;
                let output = match (initiator, confirmation.step, record) {
                    (_, _, Record::PairReject) => return Err(Error::Rejected),
                    (_, Step::Hello, Record::LanHello { label })
                        if self.context.path == Path::DirectTcp =>
                    {
                        confirmation.label = Some(label);
                        confirmation.step = Step::Compare;
                        None
                    }
                    (_, Step::Hello, Record::Hello { label, proof })
                        if self.context.path != Path::DirectTcp =>
                    {
                        let proof_message =
                            Zeroizing::new(route_proof_message(&confirmation.hash, &from));
                        if !verify_route(&from, &proof_message, &proof) {
                            return Err(Error::Authentication);
                        }
                        confirmation.label = Some(label);
                        confirmation.step = Step::Compare;
                        None
                    }
                    (false, Step::Compare, Record::PairApprove) => {
                        confirmation.step = Step::RemoteApproved;
                        None
                    }
                    (false, Step::LocalApproved, Record::PairApprove) => Some(stage(
                        endpoint,
                        &self.context,
                        &mut state,
                        lease,
                        &mut confirmation,
                        Step::WaitStored,
                        Record::PairCommit,
                    )?),
                    (true, Step::WaitCommit, Record::PairCommit) => Some(stage(
                        endpoint,
                        &self.context,
                        &mut state,
                        lease,
                        &mut confirmation,
                        Step::WaitDone,
                        Record::PairStored,
                    )?),
                    (false, Step::WaitStored, Record::PairStored) => {
                        let done = encrypt(
                            endpoint,
                            &self.context,
                            false,
                            &mut confirmation,
                            Record::PairDone,
                        )?;
                        state.trust.activate(lease.id)?;
                        self.status = PairingStatus::LocalTrustSaved;
                        return Ok(vec![done]);
                    }
                    (true, Step::WaitDone, Record::PairDone) => {
                        state.trust.activate(lease.id)?;
                        self.status = PairingStatus::Complete;
                        return Ok(Vec::new());
                    }
                    _ => return Err(Error::Unexpected),
                };
                self.phase = Some(Phase::Confirm(confirmation));
                Ok(output.into_iter().collect())
            }
        }
    }

    /// Approve only after comparing the entire code on both trusted displays.
    pub fn approve(&mut self, compared_code: &str, now: Time) -> Result<Vec<RoutedFrame>, Error> {
        let result = self.approve_step(compared_code, now);
        if result.is_err() {
            self.cancel();
        }
        result
    }

    fn approve_step(&mut self, compared_code: &str, now: Time) -> Result<Vec<RoutedFrame>, Error> {
        let lease = self.lease.as_ref().ok_or(Error::Closed)?;
        let endpoint = lease.endpoint;
        let mut state = endpoint.state()?;
        self.check(&state, now)?;
        if self
            .verification_code()
            .is_none_or(|code| code.as_str() != compared_code)
        {
            return Err(Error::Verification);
        }
        let Some(Phase::Confirm(confirmation)) = self.phase.as_mut() else {
            return Err(Error::Unexpected);
        };
        let output = match (lease.slot.initiator, confirmation.step) {
            (true, Step::Compare) => {
                confirmation.step = Step::WaitCommit;
                Some(encrypt(
                    endpoint,
                    &self.context,
                    true,
                    confirmation,
                    Record::PairApprove,
                )?)
            }
            (false, Step::Compare) => {
                confirmation.step = Step::LocalApproved;
                None
            }
            (false, Step::RemoteApproved) => Some(stage(
                endpoint,
                &self.context,
                &mut state,
                lease,
                confirmation,
                Step::WaitStored,
                Record::PairCommit,
            )?),
            _ => return Err(Error::Unexpected),
        };
        Ok(output.into_iter().collect())
    }

    /// Emit an encrypted rejection when a pairing transport exists and close the local attempt.
    pub fn reject(&mut self, now: Time) -> Result<Option<RoutedFrame>, Error> {
        let result = (|| {
            let lease = self.lease.as_ref().ok_or(Error::Closed)?;
            let state = lease.endpoint.state()?;
            self.check(&state, now)?;
            let Some(Phase::Confirm(confirmation)) = self.phase.as_mut() else {
                return Ok(None);
            };
            Ok(Some(encrypt(
                lease.endpoint,
                &self.context,
                lease.slot.initiator,
                confirmation,
                Record::PairReject,
            )?))
        })();
        self.cancel();
        result
    }

    pub fn cancel(&mut self) {
        self.phase = None;
        self.lease = None;
        if self.status == PairingStatus::InProgress {
            self.status = PairingStatus::Closed;
        }
    }

    pub(super) fn approval_window(&self, now: Time) -> Result<Duration, Error> {
        let lease = self.lease.as_ref().ok_or(Error::Closed)?;
        let state = lease.endpoint.state()?;
        self.check(&state, now)?;
        let (created, wall_deadline) = match self.scanner_limit {
            Some((wall, _)) => (self.started, wall.saturating_add(1)),
            None => {
                let invite = state.invite.as_ref().ok_or(Error::Closed)?;
                (invite.created, invite.expires_at)
            }
        };
        let elapsed = now
            .monotonic
            .checked_sub(created.monotonic)
            .ok_or(Error::Expired)?;
        Ok((INVITE_TTL - elapsed).min(Duration::from_secs(wall_deadline - now.unix_seconds)))
    }

    fn check(&self, state: &State, now: Time) -> Result<(), Error> {
        let lease = self.lease.as_ref().ok_or(Error::Closed)?;
        state.check(lease.slot, lease.id)?;
        if !within(self.started, now, INVITE_TTL)
            || (matches!(self.phase, Some(Phase::Handshake(_)))
                && !within(self.started, now, HANDSHAKE_TIMEOUT))
            || match self.scanner_limit {
                Some((wall, _)) => now.unix_seconds > wall,
                None => state.invite.as_ref().is_none_or(|invite| {
                    invite.reserved_by != Some(lease.id) || !invite.valid(now)
                }),
            }
        {
            return Err(Error::Expired);
        }
        Ok(())
    }
}

fn comparison_code(hash: &[u8; 32]) -> VerificationCode {
    use std::fmt::Write;
    let mut code = Zeroizing::new(String::with_capacity(19));
    for (i, bytes) in hash[..8].as_chunks::<2>().0.iter().enumerate() {
        if i != 0 {
            code.push('-');
        }
        write!(code, "{:02X}{:02X}", bytes[0], bytes[1]).expect("writing to a String cannot fail");
    }
    VerificationCode(code)
}

fn stage(
    endpoint: &Endpoint,
    context: &PeerContext,
    state: &mut State,
    lease: &Lease<'_>,
    confirmation: &mut Confirmation,
    next: Step,
    record: Record,
) -> Result<RoutedFrame, Error> {
    let label = confirmation.label.take().ok_or(Error::Unexpected)?;
    state.trust.stage(
        lease.id,
        TrustedDevice {
            route: remote_route(context, lease.slot.initiator),
            noise_key: confirmation.noise_key,
            label,
        },
    )?;
    confirmation.step = next;
    encrypt(
        endpoint,
        context,
        lease.slot.initiator,
        confirmation,
        record,
    )
}

fn encrypt(
    endpoint: &Endpoint,
    context: &PeerContext,
    initiator: bool,
    confirmation: &mut Confirmation,
    record: Record,
) -> Result<RoutedFrame, Error> {
    let plaintext = Zeroizing::new(record.encode()?);
    let ciphertext = confirmation.transport.seal(&plaintext)?;
    Ok(endpoint.frame(context, initiator, FrameKind::Transport, ciphertext))
}
