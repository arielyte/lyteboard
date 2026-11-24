//! Deterministic v1 encodings used by pairing and authenticated peer sessions.

mod cbor;
mod frame;
mod invite;

use std::fmt;

use minicbor::Encoder;
use thiserror::Error;

use cbor::Reader;
pub use frame::{FrameKind, RoutedFrame};
pub use invite::{Invite, QrCode};

pub type RouteId = [u8; 32];
pub type NoiseKey = [u8; 32];
pub type ChannelId = [u8; 16];
pub type InviteId = [u8; 16];

const VERSION: u8 = 1;
pub const MAX_ENVELOPE_BYTES: usize = 36 * 1024;
pub const MAX_CIPHERTEXT_BYTES: usize = 34 * 1024;
pub const MAX_HANDSHAKE_BYTES: usize = 1024;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_LABEL_BYTES: usize = 64;
const MAX_ORIGIN_BYTES: usize = 128;
const NOISE_TAG_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum WireError {
    #[error("malformed peer wire data")]
    Malformed,
    #[error("unsupported peer wire version")]
    UnsupportedVersion,
    #[error("peer wire size limit exceeded")]
    TooLarge,
    #[error("invalid relay origin")]
    InvalidOrigin,
}

/// A canonical WSS origin whose exact bytes can be bound into a transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin(String);

impl Origin {
    pub fn parse(input: &str) -> Result<Self, WireError> {
        if input.len() > MAX_ORIGIN_BYTES {
            return Err(WireError::TooLarge);
        }
        let authority = input
            .strip_prefix("wss://")
            .ok_or(WireError::InvalidOrigin)?;
        let host = match authority.split_once(':') {
            Some((host, port)) => {
                if port.is_empty()
                    || port.starts_with('0')
                    || !port.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(WireError::InvalidOrigin);
                }
                if port.parse::<u16>().ok().is_none_or(|port| port == 443) {
                    return Err(WireError::InvalidOrigin);
                }
                host
            }
            None => authority,
        };

        for label in host.split('.') {
            if label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-')
            {
                return Err(WireError::InvalidOrigin);
            }
        }
        // Exclude IPv4 literals and alternate numeric forms interpreted as IPs by URL parsers.
        let last = host.rsplit('.').next().ok_or(WireError::InvalidOrigin)?;
        if last.bytes().all(|b| b.is_ascii_digit())
            || last
                .strip_prefix("0x")
                .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(WireError::InvalidOrigin);
        }

        Ok(Self(input.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Path {
    Relay,
    Lan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Context {
    pub origin: Origin,
    pub path: Path,
    pub channel: ChannelId,
    pub initiator: RouteId,
    pub responder: RouteId,
    pub invite: Option<InviteId>,
}

impl Context {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        if self.initiator == self.responder || (self.invite.is_some() && self.path != Path::Relay) {
            return Err(WireError::Malformed);
        }
        let mut encoder = Encoder::new(Vec::new());
        encoder
            .array(9)
            .unwrap()
            .str("lyteboard.peer")
            .unwrap()
            .u8(VERSION)
            .unwrap()
            .u8(u8::from(self.invite.is_none()))
            .unwrap()
            .u8(u8::from(self.path == Path::Lan))
            .unwrap()
            .str(self.origin.as_str())
            .unwrap()
            .bytes(&self.channel)
            .unwrap()
            .bytes(&self.initiator)
            .unwrap()
            .bytes(&self.responder)
            .unwrap();
        encode_optional(&mut encoder, self.invite.as_ref());

        Ok(encoder.into_writer())
    }
}

pub fn route_proof_message(hash: &[u8; 32], route: &RouteId) -> Vec<u8> {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .array(4)
        .unwrap()
        .str("lyteboard.pair-route")
        .unwrap()
        .u8(VERSION)
        .unwrap()
        .bytes(hash)
        .unwrap()
        .bytes(route)
        .unwrap();
    encoder.into_writer()
}

fn encode_optional(encoder: &mut Encoder<Vec<u8>>, value: Option<&[u8; 16]>) {
    match value {
        Some(bytes) => {
            encoder.bytes(bytes).unwrap();
        }
        None => {
            encoder.null().unwrap();
        }
    }
}

#[derive(Eq, PartialEq)]
pub enum Record {
    Hello {
        label: String,
        proof: [u8; 64],
    },
    PairApprove,
    PairCommit,
    PairStored,
    PairDone,
    PairReject,
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
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerFailure {
    InvalidPayload = 1,
    TooLarge = 2,
    UnsupportedMedia = 3,
    IntegrityFailed = 4,
    Busy = 5,
    TimedOut = 6,
    Denied = 7,
}

impl PeerFailure {
    fn read(reader: &mut Reader<'_>) -> Result<Self, WireError> {
        match reader.uint()? {
            1 => Ok(Self::InvalidPayload),
            2 => Ok(Self::TooLarge),
            3 => Ok(Self::UnsupportedMedia),
            4 => Ok(Self::IntegrityFailed),
            5 => Ok(Self::Busy),
            6 => Ok(Self::TimedOut),
            7 => Ok(Self::Denied),
            _ => Err(WireError::Malformed),
        }
    }
}

impl Record {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut encoder = Encoder::new(Vec::new());
        let len = match self {
            Self::Hello { label, .. } => {
                validate_label(label)?;
                4
            }
            Self::Text { text, .. } => {
                validate_text(text)?;
                4
            }
            Self::DeliveryAck { .. } | Self::PeerError { .. } => 4,
            _ => 2,
        };
        encoder
            .array(len)
            .unwrap()
            .u8(VERSION)
            .unwrap()
            .u8(self.tag())
            .unwrap();
        match self {
            Self::Hello { label, proof } => {
                encoder.str(label).unwrap().bytes(proof).unwrap();
            }
            Self::Text { id, text } => {
                encoder.bytes(id).unwrap().str(text).unwrap();
            }
            Self::DeliveryAck { id, duplicate } => {
                encoder.bytes(id).unwrap().u8(u8::from(*duplicate)).unwrap();
            }
            Self::PeerError { id, code } => {
                encode_optional(&mut encoder, id.as_ref());
                encoder.u8(*code as u8).unwrap();
            }
            _ => {}
        }
        Ok(encoder.into_writer())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut reader = Reader::new(bytes, MAX_CIPHERTEXT_BYTES - NOISE_TAG_BYTES)?;
        let len = reader.array()?;
        if !matches!(len, 2 | 4) {
            return Err(WireError::Malformed);
        }
        reader.version()?;
        let record = match (len, reader.uint()?) {
            (4, 0) => {
                let label = reader.text(MAX_LABEL_BYTES)?;
                validate_label(label)?;
                let proof = *reader.fixed()?;
                reader.finish()?;
                return Ok(Self::Hello {
                    label: label.to_owned(),
                    proof,
                });
            }
            (2, 1) => Self::PairApprove,
            (2, 2) => Self::PairCommit,
            (2, 3) => Self::PairStored,
            (2, 4) => Self::PairDone,
            (2, 5) => Self::PairReject,
            (2, 6) => Self::Ready,
            (4, 7) => {
                let id = *reader.fixed()?;
                let text = reader.text(MAX_TEXT_BYTES)?;
                validate_text(text)?;
                reader.finish()?;
                return Ok(Self::Text {
                    id,
                    text: text.to_owned(),
                });
            }
            (4, 12) => Self::DeliveryAck {
                id: *reader.fixed()?,
                duplicate: match reader.uint()? {
                    0 => false,
                    1 => true,
                    _ => return Err(WireError::Malformed),
                },
            },
            (4, 13) => Self::PeerError {
                id: reader.optional_bytes()?,
                code: PeerFailure::read(&mut reader)?,
            },
            (2, 14) => Self::Close,
            _ => return Err(WireError::Malformed),
        };
        reader.finish()?;
        Ok(record)
    }

    fn tag(&self) -> u8 {
        match self {
            Self::Hello { .. } => 0,
            Self::PairApprove => 1,
            Self::PairCommit => 2,
            Self::PairStored => 3,
            Self::PairDone => 4,
            Self::PairReject => 5,
            Self::Ready => 6,
            Self::Text { .. } => 7,
            Self::DeliveryAck { .. } => 12,
            Self::PeerError { .. } => 13,
            Self::Close => 14,
        }
    }
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("tag", &self.tag())
            .finish_non_exhaustive()
    }
}

pub fn validate_label(label: &str) -> Result<(), WireError> {
    if label.len() > MAX_LABEL_BYTES {
        return Err(WireError::TooLarge);
    }
    if label.chars().any(char::is_control) {
        return Err(WireError::Malformed);
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<(), WireError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(WireError::TooLarge);
    }
    if text.is_empty() || text.contains('\0') {
        return Err(WireError::Malformed);
    }
    Ok(())
}
