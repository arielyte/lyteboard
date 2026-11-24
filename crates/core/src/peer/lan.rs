//! Direct TCP adapter for the in-memory peer core.

use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use lyteboard_protocol::peer::{
    FrameKind, LanInvite, MAX_ENVELOPE_BYTES, NoiseKey, Path, PeerFailure, QrCode, RoutedFrame,
    WireError, validate_lan_address,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{Instant, timeout, timeout_at},
};

use super::{
    Endpoint, HANDSHAKE_TIMEOUT, Pairing, PairingStatus, Session, SessionEvent, Time,
    VerificationCode, random,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Peer(#[from] super::Error),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("LAN connection failed")]
    Io(#[from] io::Error),
    #[error("LAN operation timed out")]
    Timeout(#[from] tokio::time::error::Elapsed),
    #[error("LAN channel is closed")]
    Closed,
    #[error("unexpected LAN frame")]
    Unexpected,
    #[error("system time unavailable")]
    Clock,
}

pub struct Listener(TcpListener);

impl Listener {
    /// Bind a specific local interface. With no address, use loopback and an ephemeral port.
    pub async fn bind(address: Option<SocketAddr>) -> Result<Self, Error> {
        let address = address.unwrap_or_else(|| SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
        if address.ip().is_unspecified() || address.ip().is_multicast() {
            return Err(WireError::Malformed.into());
        }
        Ok(Self(TcpListener::bind(address).await?))
    }

    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        Ok(self.0.local_addr()?)
    }

    pub fn invite(&self, endpoint: &Endpoint) -> Result<QrCode, Error> {
        Ok(endpoint.create_lan_invite(self.local_addr()?, now()?)?)
    }

    /// Handle one incoming handshake at a time per listener. The caller owns established channels.
    pub async fn accept<'a>(
        &mut self,
        endpoint: &'a Endpoint,
        label: &str,
    ) -> Result<Incoming<'a>, Error> {
        self.accept_socket()
            .await?
            .authenticate(endpoint, label)
            .await
    }

    /// Accept a TCP socket before authenticating it. Callers must bound concurrent handshakes.
    pub async fn accept_socket(&self) -> Result<IncomingSocket, Error> {
        let (stream, _) = self.0.accept().await?;
        Ok(IncomingSocket(stream))
    }
}

pub struct IncomingSocket(TcpStream);

impl IncomingSocket {
    pub async fn authenticate<'a>(
        self,
        endpoint: &'a Endpoint,
        label: &str,
    ) -> Result<Incoming<'a>, Error> {
        timeout(HANDSHAKE_TIMEOUT, async {
            let mut link = Link::new(self.0, endpoint.noise_key())?;
            let (peer, first) = link.read(FRAME_TIMEOUT).await?;
            match first.kind {
                FrameKind::PairingOpen => {
                    let (pairing, reply) = endpoint.accept_pairing_with(
                        Path::DirectTcp,
                        peer,
                        first,
                        label,
                        now()?,
                    )?;
                    link.write(&reply).await?;
                    Ok(Incoming::Pairing(
                        PendingPair::prepare(link, pairing, peer).await?,
                    ))
                }
                FrameKind::SessionOpen => {
                    let (mut session, reply) =
                        endpoint.accept_session(peer, Path::DirectTcp, first, now()?)?;
                    link.write(&reply).await?;
                    send_ready(&mut link, &mut session).await?;
                    Ok(Incoming::Session(TcpSession::new(link, session, peer)))
                }
                _ => Err(Error::Unexpected),
            }
        })
        .await?
    }
}

pub enum Incoming<'a> {
    Pairing(PendingPair<'a>),
    Session(TcpSession<'a>),
}

pub async fn connect_pairing<'a>(
    endpoint: &'a Endpoint,
    qr: &str,
    label: &str,
) -> Result<PendingPair<'a>, Error> {
    let invite = LanInvite::decode_qr(qr)?;
    timeout(HANDSHAKE_TIMEOUT, async {
        let (pairing, first) = endpoint.begin_lan_pairing(&invite, label, now()?)?;
        let mut link = Link::connect(endpoint, invite.address).await?;
        link.write(&first).await?;
        PendingPair::prepare(link, pairing, invite.noise_key).await
    })
    .await?
}

/// Open a new KK session. The address is a dial hint, and `peer` must be an active Noise pin.
pub async fn connect_session(
    endpoint: &Endpoint,
    address: SocketAddr,
    peer: NoiseKey,
) -> Result<TcpSession<'_>, Error> {
    validate_lan_address(address)?;
    timeout(HANDSHAKE_TIMEOUT, async {
        let (mut session, first) = endpoint.begin_session(peer, Path::DirectTcp, now()?)?;
        let mut link = Link::connect(endpoint, address).await?;
        link.write(&first).await?;
        send_ready(&mut link, &mut session).await?;
        let (from, frame) = link.read(FRAME_TIMEOUT).await?;
        if !matches!(session.receive(from, frame, now()?)?, SessionEvent::Ready) {
            return Err(Error::Unexpected);
        }
        endpoint.remember_address(peer, address)?;
        Ok(TcpSession::new(link, session, peer))
    })
    .await?
}

async fn send_ready(link: &mut Link, session: &mut Session<'_>) -> Result<(), Error> {
    let (from, frame) = link.read(FRAME_TIMEOUT).await?;
    let SessionEvent::Send(reply) = session.receive(from, frame, now()?)? else {
        return Err(Error::Unexpected);
    };
    link.write(&reply).await
}

pub struct PendingPair<'a> {
    link: Link,
    pairing: Pairing<'a>,
    peer: NoiseKey,
}

impl<'a> PendingPair<'a> {
    async fn prepare(link: Link, pairing: Pairing<'a>, peer: NoiseKey) -> Result<Self, Error> {
        let mut pending = Self {
            link,
            pairing,
            peer,
        };
        while pending.pairing.verification_code().is_none() {
            pending.advance(FRAME_TIMEOUT).await?;
        }
        Ok(pending)
    }

    pub fn verification_code(&self) -> &VerificationCode {
        self.pairing
            .verification_code()
            .expect("pairing has reached confirmation")
    }

    pub fn peer_key(&self) -> NoiseKey {
        self.peer
    }

    pub fn approval_window(&self) -> Result<Duration, Error> {
        Ok(self.pairing.approval_window(now()?)?)
    }

    /// Wait for peer input without consuming it while the local user compares codes.
    pub async fn wait_for_input(&self) -> Result<(), Error> {
        self.link.wait_for_input().await
    }

    /// Handle an early peer approval or rejection without approving locally.
    pub async fn receive_before_approval(&mut self) -> Result<(), Error> {
        self.advance(FRAME_TIMEOUT).await
    }

    /// Call after explicit local approval and comparison with the other device's displayed code.
    pub async fn approve(mut self, compared_code: &str) -> Result<PairingStatus, Error> {
        let wait = self.pairing.approval_window(now()?)?;
        let deadline = Instant::now() + wait;
        timeout_at(deadline, async {
            for frame in self.pairing.approve(compared_code, now()?)? {
                self.link.write(&frame).await?;
            }
            while self.pairing.status() == PairingStatus::InProgress {
                self.advance(wait).await?;
            }
            Ok(self.pairing.status())
        })
        .await?
    }

    pub async fn reject(mut self) -> Result<(), Error> {
        if let Some(frame) = self.pairing.reject(now()?)? {
            self.link.write(&frame).await?;
        }
        Ok(())
    }

    async fn advance(&mut self, wait: Duration) -> Result<(), Error> {
        let (from, frame) = self.link.read(wait).await?;
        for reply in self.pairing.receive(from, frame, now()?)? {
            self.link.write(&reply).await?;
        }
        Ok(())
    }
}

/// A mutually confirmed ordered channel. Failed or canceled I/O drops its socket and Noise state.
pub struct TcpSession<'a> {
    channel: Option<(Link, Session<'a>)>,
    peer: NoiseKey,
}

impl<'a> TcpSession<'a> {
    fn new(link: Link, session: Session<'a>, peer: NoiseKey) -> Self {
        Self {
            channel: Some((link, session)),
            peer,
        }
    }

    pub fn peer_key(&self) -> NoiseKey {
        self.peer
    }

    /// Await input without consuming it, so this wait can be canceled to send or close.
    pub async fn wait_for_input(&self) -> Result<(), Error> {
        let (link, _) = self.channel.as_ref().ok_or(Error::Closed)?;
        link.wait_for_input().await
    }

    pub async fn reject_text(&mut self, id: [u8; 16], reason: PeerFailure) -> Result<(), Error> {
        self.send(|session, now| session.peer_error(Some(id), reason, now))
            .await
    }

    pub async fn send_text(&mut self, text: &str) -> Result<[u8; 16], Error> {
        let mut id = [0; 16];
        random(&mut id)?;
        self.send(|session, now| session.send_text(id, text, now))
            .await?;
        Ok(id)
    }

    pub async fn acknowledge(&mut self, id: [u8; 16]) -> Result<(), Error> {
        self.send(|session, now| session.acknowledge(id, false, now))
            .await
    }

    async fn send(
        &mut self,
        encrypt: impl FnOnce(&mut Session<'a>, Time) -> Result<RoutedFrame, super::Error>,
    ) -> Result<(), Error> {
        // Keep both halves outside self while awaiting I/O so cancellation cannot reuse a
        // partially written frame or a Noise nonce.
        let (mut link, mut session) = self.channel.take().ok_or(Error::Closed)?;
        link.write(&encrypt(&mut session, now()?)?).await?;
        self.channel = Some((link, session));
        Ok(())
    }

    pub async fn receive(&mut self) -> Result<SessionEvent, Error> {
        let (mut link, mut session) = self.channel.take().ok_or(Error::Closed)?;
        let (from, frame) = link.read(FRAME_TIMEOUT).await?;
        let event = session.receive(from, frame, now()?)?;
        if !matches!(event, SessionEvent::Closed) {
            self.channel = Some((link, session));
        }
        Ok(event)
    }

    pub async fn close(mut self) -> Result<(), Error> {
        let (mut link, mut session) = self.channel.take().ok_or(Error::Closed)?;
        link.write(&session.close(now()?)?).await
    }
}

struct Link {
    stream: TcpStream,
    local: NoiseKey,
}

impl Link {
    fn new(stream: TcpStream, local: NoiseKey) -> Result<Self, Error> {
        #[cfg(target_os = "linux")]
        {
            use rustix::net::sockopt;
            let configure = || -> io::Result<()> {
                sockopt::set_tcp_keepidle(&stream, Duration::from_secs(10))?;
                sockopt::set_tcp_keepintvl(&stream, Duration::from_secs(3))?;
                sockopt::set_tcp_keepcnt(&stream, 3)?;
                sockopt::set_socket_keepalive(&stream, true)?;
                Ok(())
            };
            configure()?;
        }
        Ok(Self { stream, local })
    }

    async fn wait_for_input(&self) -> Result<(), Error> {
        if self.stream.peek(&mut [0]).await? == 0 {
            return Err(Error::Closed);
        }
        Ok(())
    }

    async fn connect(endpoint: &Endpoint, address: SocketAddr) -> Result<Self, Error> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await??;
        Self::new(stream, endpoint.noise_key())
    }

    async fn read(&mut self, wait: Duration) -> Result<(NoiseKey, RoutedFrame), Error> {
        let mut prefix = [0; 4];
        timeout(wait, self.stream.read_exact(&mut prefix[..1])).await??;
        timeout(FRAME_TIMEOUT, async {
            self.stream.read_exact(&mut prefix[1..]).await?;
            let length = u32::from_be_bytes(prefix) as usize;
            if length == 0 {
                return Err(WireError::Malformed.into());
            }
            if length > MAX_ENVELOPE_BYTES {
                return Err(WireError::TooLarge.into());
            }
            let mut frame = vec![0; length];
            self.stream.read_exact(&mut frame).await?;
            Ok(RoutedFrame::decode_forwarded(&frame)?)
        })
        .await?
    }

    async fn write(&mut self, frame: &RoutedFrame) -> Result<(), Error> {
        let data = frame.encode_forwarded(&self.local)?;
        let length = u32::try_from(data.len()).map_err(|_| WireError::TooLarge)?;
        timeout(FRAME_TIMEOUT, async {
            self.stream.write_all(&length.to_be_bytes()).await?;
            self.stream.write_all(&data).await
        })
        .await??;
        Ok(())
    }
}

fn now() -> Result<Time, Error> {
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Clock)?;
    #[cfg(target_os = "linux")]
    let monotonic = {
        use rustix::time::{ClockId, clock_gettime};

        let boot = clock_gettime(ClockId::Boottime);
        Duration::new(
            boot.tv_sec.try_into().map_err(|_| Error::Clock)?,
            boot.tv_nsec.try_into().map_err(|_| Error::Clock)?,
        )
    };
    #[cfg(not(target_os = "linux"))]
    let monotonic = {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    };
    Ok(Time {
        unix_seconds: wall.as_secs(),
        monotonic,
    })
}
