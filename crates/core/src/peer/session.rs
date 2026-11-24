use std::time::Duration;

use lyteboard_protocol::peer::{
    Context, FrameKind, MAX_TEXT_BYTES, Path, PeerFailure, Record, RouteId, RoutedFrame,
};
use zeroize::Zeroizing;

use super::{
    Endpoint, Error, HANDSHAKE_TIMEOUT, Lease, Slot, State, Time,
    crypto::{Handshake, Transport},
    random, within,
};

const SESSION_LIFETIME: Duration = Duration::from_secs(3600);

impl Endpoint {
    pub fn begin_session(
        &self,
        to: RouteId,
        path: Path,
        now: Time,
    ) -> Result<(Session<'_>, RoutedFrame), Error> {
        let mut state = self.state_at(now)?;
        let pin = state.trust.active(&to).ok_or(Error::Untrusted)?.noise_key;
        let slot = Slot {
            route: to,
            initiator: true,
        };
        state.check_capacity(slot)?;
        let mut channel = [0; 16];
        random(&mut channel)?;
        let context = Context {
            origin: self.origin.clone(),
            path,
            channel,
            initiator: self.route_id(),
            responder: to,
            invite: None,
        };
        let mut handshake =
            Handshake::new(&self.identity, true, &context.encode()?, Some(&pin), None)?;
        let first = self.frame(&context, true, FrameKind::SessionOpen, handshake.write()?);
        let id = state.reserve(slot, now, None)?;
        Ok((
            Session {
                lease: Some(Lease {
                    endpoint: self,
                    id,
                    slot,
                }),
                context,
                started: now,
                phase: Some(Phase::Handshake(handshake)),
            },
            first,
        ))
    }

    pub fn accept_session(
        &self,
        from: RouteId,
        path: Path,
        first: RoutedFrame,
        now: Time,
    ) -> Result<(Session<'_>, RoutedFrame), Error> {
        first.validate()?;
        let mut state = self.state_at(now)?;
        let pin = state.trust.active(&from).ok_or(Error::Untrusted)?.noise_key;
        let context = Context {
            origin: self.origin.clone(),
            path,
            channel: first.channel,
            initiator: from,
            responder: self.route_id(),
            invite: None,
        };
        self.receive_frame(&context, false, from, &first, FrameKind::SessionOpen)?;
        let slot = Slot {
            route: from,
            initiator: false,
        };
        state.check_capacity(slot)?;
        let mut handshake =
            Handshake::new(&self.identity, false, &context.encode()?, Some(&pin), None)?;
        handshake.read(&first.body)?;
        let reply = self.frame(&context, false, FrameKind::Handshake, handshake.write()?);
        let (_, remote, transport) = handshake.finish()?;
        if remote != pin {
            return Err(Error::Authentication);
        }
        let id = state.reserve(slot, now, None)?;
        Ok((
            Session {
                lease: Some(Lease {
                    endpoint: self,
                    id,
                    slot,
                }),
                context,
                started: now,
                phase: Some(Phase::Waiting(transport)),
            },
            reply,
        ))
    }
}

pub struct Session<'a> {
    lease: Option<Lease<'a>>,
    context: Context,
    started: Time,
    phase: Option<Phase>,
}

enum Phase {
    Handshake(Handshake),
    Waiting(Transport),
    Ready {
        transport: Transport,
        established: Time,
    },
}

pub enum SessionEvent {
    Send(RoutedFrame),
    Ready,
    Text {
        id: [u8; 16],
        text: String,
    },
    DeliveryAck {
        id: [u8; 16],
        duplicate: bool,
    },
    PeerError {
        id: Option<[u8; 16]>,
        code: PeerFailure,
    },
    Closed,
}

impl Session<'_> {
    pub fn receive(
        &mut self,
        from: RouteId,
        frame: RoutedFrame,
        now: Time,
    ) -> Result<SessionEvent, Error> {
        let result = self.receive_step(from, frame, now);
        if result.is_err() || self.phase.is_none() {
            self.abort();
        }
        result
    }

    fn receive_step(
        &mut self,
        from: RouteId,
        frame: RoutedFrame,
        now: Time,
    ) -> Result<SessionEvent, Error> {
        let lease = self.lease.as_ref().ok_or(Error::Closed)?;
        let endpoint = lease.endpoint;
        let initiator = lease.slot.initiator;
        let mut state = endpoint.state()?;
        let pin = self.check(&state, now)?;
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
                let (_, peer, mut transport) = handshake.finish()?;
                if peer != pin {
                    return Err(Error::Authentication);
                }
                let ready = encrypt(lease, &self.context, &mut transport, Record::Ready)?;
                self.phase = Some(Phase::Waiting(transport));
                Ok(SessionEvent::Send(ready))
            }
            Phase::Waiting(mut transport) => {
                endpoint.receive_frame(
                    &self.context,
                    initiator,
                    from,
                    &frame,
                    FrameKind::Transport,
                )?;
                if !matches!(
                    Record::decode(&transport.open(&frame.body)?)?,
                    Record::Ready
                ) {
                    return Err(Error::Unexpected);
                }
                let result = if initiator {
                    SessionEvent::Ready
                } else {
                    SessionEvent::Send(encrypt(
                        lease,
                        &self.context,
                        &mut transport,
                        Record::Ready,
                    )?)
                };
                state.pending.remove(&lease.id);
                state.live.insert(lease.slot, lease.id);
                self.phase = Some(Phase::Ready {
                    transport,
                    established: now,
                });
                Ok(result)
            }
            Phase::Ready {
                mut transport,
                established,
            } => {
                endpoint.receive_frame(
                    &self.context,
                    initiator,
                    from,
                    &frame,
                    FrameKind::Transport,
                )?;
                let record = Record::decode(&transport.open(&frame.body)?)?;
                let event = match record {
                    Record::Text { id, text } => SessionEvent::Text { id, text },
                    Record::DeliveryAck { id, duplicate } => {
                        SessionEvent::DeliveryAck { id, duplicate }
                    }
                    Record::PeerError { id, code } => SessionEvent::PeerError { id, code },
                    Record::Close => return Ok(SessionEvent::Closed),
                    _ => return Err(Error::Unexpected),
                };
                self.phase = Some(Phase::Ready {
                    transport,
                    established,
                });
                Ok(event)
            }
        }
    }

    /// Encrypt a v1 text record. The caller owns delivery and clipboard policy.
    pub fn send_text(&mut self, id: [u8; 16], text: &str, now: Time) -> Result<RoutedFrame, Error> {
        if text.is_empty() || text.len() > MAX_TEXT_BYTES || text.as_bytes().contains(&0) {
            return Err(Error::Limit);
        }
        self.send(
            Record::Text {
                id,
                text: text.to_owned(),
            },
            now,
        )
    }

    pub fn acknowledge(
        &mut self,
        id: [u8; 16],
        duplicate: bool,
        now: Time,
    ) -> Result<RoutedFrame, Error> {
        self.send(Record::DeliveryAck { id, duplicate }, now)
    }

    pub fn peer_error(
        &mut self,
        id: Option<[u8; 16]>,
        code: PeerFailure,
        now: Time,
    ) -> Result<RoutedFrame, Error> {
        self.send(Record::PeerError { id, code }, now)
    }

    pub fn close(&mut self, now: Time) -> Result<RoutedFrame, Error> {
        let result = self.send(Record::Close, now);
        self.abort();
        result
    }

    /// Tear down this channel after transport loss or uncertain delivery.
    pub fn abort(&mut self) {
        self.phase = None;
        self.lease = None;
    }

    fn send(&mut self, record: Record, now: Time) -> Result<RoutedFrame, Error> {
        if !matches!(self.phase, Some(Phase::Ready { .. })) {
            return Err(Error::Closed);
        }
        let result = (|| {
            let lease = self.lease.as_ref().ok_or(Error::Closed)?;
            let state = lease.endpoint.state()?;
            self.check(&state, now)?;
            let Some(Phase::Ready { transport, .. }) = self.phase.as_mut() else {
                return Err(Error::Closed);
            };
            encrypt(lease, &self.context, transport, record)
        })();
        if result.is_err() {
            self.abort();
        }
        result
    }

    fn check(&self, state: &State, now: Time) -> Result<[u8; 32], Error> {
        let lease = self.lease.as_ref().ok_or(Error::Closed)?;
        let pin = state
            .trust
            .active(&lease.slot.route)
            .ok_or(Error::Untrusted)?
            .noise_key;
        state.check(lease.slot, lease.id)?;
        let valid = match self.phase.as_ref().ok_or(Error::Closed)? {
            Phase::Ready { established, .. } => within(*established, now, SESSION_LIFETIME),
            _ => within(self.started, now, HANDSHAKE_TIMEOUT),
        };
        if !valid {
            return Err(Error::Expired);
        }
        Ok(pin)
    }
}

fn encrypt(
    lease: &Lease<'_>,
    context: &Context,
    transport: &mut Transport,
    record: Record,
) -> Result<RoutedFrame, Error> {
    let plaintext = Zeroizing::new(record.encode()?);
    Ok(lease.endpoint.frame(
        context,
        lease.slot.initiator,
        FrameKind::Transport,
        transport.seal(&plaintext)?,
    ))
}
