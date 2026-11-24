use std::fmt;

use minicbor::Encoder;

use super::{
    ChannelId, InviteId, MAX_CIPHERTEXT_BYTES, MAX_ENVELOPE_BYTES, MAX_HANDSHAKE_BYTES,
    NOISE_TAG_BYTES, Reader, RouteId, VERSION, WireError, encode_optional,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameKind {
    PairingOpen,
    SessionOpen,
    Handshake,
    Transport,
}

impl FrameKind {
    fn tag(self) -> u8 {
        match self {
            Self::PairingOpen => 0,
            Self::SessionOpen => 1,
            Self::Handshake => 2,
            Self::Transport => 3,
        }
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, WireError> {
        match reader.uint()? {
            0 => Ok(Self::PairingOpen),
            1 => Ok(Self::SessionOpen),
            2 => Ok(Self::Handshake),
            3 => Ok(Self::Transport),
            _ => Err(WireError::Malformed),
        }
    }

    fn body_limit(self) -> usize {
        match self {
            Self::Transport => MAX_CIPHERTEXT_BYTES,
            _ => MAX_HANDSHAKE_BYTES,
        }
    }

    fn validate(self, selector: Option<InviteId>, body_len: usize) -> Result<(), WireError> {
        if body_len > self.body_limit() {
            return Err(WireError::TooLarge);
        }
        if (self == Self::PairingOpen) != selector.is_some()
            || body_len == 0
            || (self == Self::Transport && body_len < NOISE_TAG_BYTES)
        {
            return Err(WireError::Malformed);
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RoutedFrame {
    pub to: RouteId,
    pub channel: ChannelId,
    pub kind: FrameKind,
    pub selector: Option<InviteId>,
    pub body: Vec<u8>,
}

impl RoutedFrame {
    pub fn validate(&self) -> Result<(), WireError> {
        self.kind.validate(self.selector, self.body.len())
    }

    pub fn encode_outgoing(&self) -> Result<Vec<u8>, WireError> {
        self.encode(None)
    }

    pub fn encode_forwarded(&self, from: &RouteId) -> Result<Vec<u8>, WireError> {
        self.encode(Some(from))
    }

    pub fn decode_outgoing(bytes: &[u8]) -> Result<Self, WireError> {
        let mut reader = Reader::new(bytes, MAX_ENVELOPE_BYTES)?;
        if reader.array()? != 7 {
            return Err(WireError::Malformed);
        }
        reader.version()?;
        if reader.uint()? != 3 {
            return Err(WireError::Malformed);
        }
        Self::read_frame(reader)
    }

    pub fn decode_forwarded(bytes: &[u8]) -> Result<(RouteId, Self), WireError> {
        let mut reader = Reader::new(bytes, MAX_ENVELOPE_BYTES)?;
        if reader.array()? != 8 {
            return Err(WireError::Malformed);
        }
        reader.version()?;
        if reader.uint()? != 4 {
            return Err(WireError::Malformed);
        }
        let from = *reader.fixed()?;
        Ok((from, Self::read_frame(reader)?))
    }

    fn encode(&self, from: Option<&RouteId>) -> Result<Vec<u8>, WireError> {
        self.validate()?;
        let mut encoder = Encoder::new(Vec::new());
        match from {
            Some(route) => {
                encoder
                    .array(8)
                    .unwrap()
                    .u8(VERSION)
                    .unwrap()
                    .u8(4)
                    .unwrap()
                    .bytes(route)
                    .unwrap();
            }
            None => {
                encoder
                    .array(7)
                    .unwrap()
                    .u8(VERSION)
                    .unwrap()
                    .u8(3)
                    .unwrap();
            }
        }
        encoder
            .bytes(&self.to)
            .unwrap()
            .bytes(&self.channel)
            .unwrap()
            .u8(self.kind.tag())
            .unwrap();
        encode_optional(&mut encoder, self.selector.as_ref());
        encoder.bytes(&self.body).unwrap();
        let bytes = encoder.into_writer();
        if bytes.len() > MAX_ENVELOPE_BYTES {
            return Err(WireError::TooLarge);
        }
        Ok(bytes)
    }

    fn read_frame(mut reader: Reader<'_>) -> Result<Self, WireError> {
        let to = *reader.fixed()?;
        let channel = *reader.fixed()?;
        let kind = FrameKind::read(&mut reader)?;
        let selector = reader.optional_bytes()?;
        let body = reader.bytes(kind.body_limit())?;
        kind.validate(selector, body.len())?;
        reader.finish()?;
        Ok(Self {
            to,
            channel,
            kind,
            selector,
            body: body.to_vec(),
        })
    }
}

impl fmt::Debug for RoutedFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoutedFrame")
            .field("kind", &self.kind)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}
