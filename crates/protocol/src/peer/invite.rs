use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6},
    ops::Deref,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use minicbor::Encoder;
use zeroize::Zeroizing;

use super::{InviteId, MAX_ORIGIN_BYTES, NoiseKey, Origin, Reader, RouteId, VERSION, WireError};

const QR_PREFIX: &str = "lyteboard-pair:v1:";
const LAN_QR_PREFIX: &str = "lyteboard-lan:v1:";
const MAX_QR_BYTES: usize = 512;
const MAX_DECODED_BYTES: usize = MAX_QR_BYTES.div_ceil(4) * 3;

pub struct Invite {
    pub origin: Origin,
    pub id: InviteId,
    psk: Zeroizing<[u8; 32]>,
    pub noise_key: NoiseKey,
    pub route: RouteId,
    pub expires_at: u64,
}

impl Invite {
    pub fn new(
        origin: Origin,
        id: InviteId,
        psk: Zeroizing<[u8; 32]>,
        noise_key: NoiseKey,
        route: RouteId,
        expires_at: u64,
    ) -> Self {
        Self {
            origin,
            id,
            psk,
            noise_key,
            route,
            expires_at,
        }
    }

    pub fn psk(&self) -> &[u8; 32] {
        &self.psk
    }

    pub fn encode_qr(&self) -> QrCode {
        // Preallocate the entire secret-bearing buffers so they never have to reallocate.
        let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_DECODED_BYTES));
        Encoder::new(&mut *bytes)
            .array(7)
            .unwrap()
            .u8(VERSION)
            .unwrap()
            .str(self.origin.as_str())
            .unwrap()
            .bytes(&self.id)
            .unwrap()
            .bytes(self.psk())
            .unwrap()
            .bytes(&self.noise_key)
            .unwrap()
            .bytes(&self.route)
            .unwrap()
            .u64(self.expires_at)
            .unwrap();

        encode_qr(QR_PREFIX, &bytes)
    }

    pub fn decode_qr(qr: &str) -> Result<Self, WireError> {
        let mut storage = Zeroizing::new([0; MAX_DECODED_BYTES]);
        let used = decode_qr(QR_PREFIX, qr, &mut storage[..])?;
        let mut reader = Reader::new(&storage[..used], MAX_DECODED_BYTES)?;
        if reader.array()? != 7 {
            return Err(WireError::Malformed);
        }
        reader.version()?;
        let origin = reader.text(MAX_ORIGIN_BYTES)?;
        let id = *reader.fixed()?;
        let psk = reader.fixed()?;
        let noise_key = *reader.fixed()?;
        let route = *reader.fixed()?;
        let expires_at = reader.uint()?;
        reader.finish()?;

        Ok(Self::new(
            Origin::parse(origin)?,
            id,
            Zeroizing::new(*psk),
            noise_key,
            route,
            expires_at,
        ))
    }
}

impl fmt::Debug for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Invite([redacted])")
    }
}

/// Direct TCP invitation. The address is a dial hint separate from the Noise pin.
pub struct LanInvite {
    pub address: SocketAddr,
    pub id: InviteId,
    psk: Zeroizing<[u8; 32]>,
    pub noise_key: NoiseKey,
    pub expires_at: u64,
}

impl LanInvite {
    pub fn new(
        address: SocketAddr,
        id: InviteId,
        psk: Zeroizing<[u8; 32]>,
        noise_key: NoiseKey,
        expires_at: u64,
    ) -> Result<Self, WireError> {
        let ip = address.ip();
        if address.port() == 0
            || ip.is_unspecified()
            || ip.is_multicast()
            || ip == IpAddr::V4(Ipv4Addr::BROADCAST)
            || matches!(address, SocketAddr::V6(v6) if v6.scope_id() != 0
                || v6.flowinfo() != 0 || v6.ip().is_unicast_link_local()
                || v6.ip().to_ipv4_mapped().is_some())
        {
            return Err(WireError::Malformed);
        }
        Ok(Self {
            address,
            id,
            psk,
            noise_key,
            expires_at,
        })
    }

    pub fn psk(&self) -> &[u8; 32] {
        &self.psk
    }

    pub fn encode_qr(&self) -> QrCode {
        let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_DECODED_BYTES));
        let mut encoder = Encoder::new(&mut *bytes);
        encoder.array(7).unwrap().u8(VERSION).unwrap();
        match self.address.ip() {
            IpAddr::V4(ip) => encoder.bytes(&ip.octets()).unwrap(),
            IpAddr::V6(ip) => encoder.bytes(&ip.octets()).unwrap(),
        };
        encoder
            .u16(self.address.port())
            .unwrap()
            .bytes(&self.id)
            .unwrap()
            .bytes(self.psk())
            .unwrap()
            .bytes(&self.noise_key)
            .unwrap()
            .u64(self.expires_at)
            .unwrap();
        encode_qr(LAN_QR_PREFIX, &bytes)
    }

    pub fn decode_qr(qr: &str) -> Result<Self, WireError> {
        let mut storage = Zeroizing::new([0; MAX_DECODED_BYTES]);
        let used = decode_qr(LAN_QR_PREFIX, qr, &mut storage[..])?;
        let mut reader = Reader::new(&storage[..used], MAX_DECODED_BYTES)?;
        if reader.array()? != 7 {
            return Err(WireError::Malformed);
        }
        reader.version()?;
        let address = reader.bytes(16)?;
        let port = u16::try_from(reader.uint()?).map_err(|_| WireError::Malformed)?;
        let address = match address.len() {
            4 => SocketAddr::from((Ipv4Addr::from(<[u8; 4]>::try_from(address).unwrap()), port)),
            16 => SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(<[u8; 16]>::try_from(address).unwrap()),
                port,
                0,
                0,
            )),
            _ => return Err(WireError::Malformed),
        };
        let id = *reader.fixed()?;
        let psk = Zeroizing::new(*reader.fixed()?);
        let noise_key = *reader.fixed()?;
        let expires_at = reader.uint()?;
        reader.finish()?;
        Self::new(address, id, psk, noise_key, expires_at)
    }
}

fn encode_qr(prefix: &str, bytes: &[u8]) -> QrCode {
    let mut qr = Zeroizing::new(String::with_capacity(MAX_QR_BYTES));
    qr.push_str(prefix);
    URL_SAFE_NO_PAD.encode_string(bytes, &mut qr);
    QrCode(qr)
}

fn decode_qr(prefix: &str, qr: &str, storage: &mut [u8]) -> Result<usize, WireError> {
    if qr.len() > MAX_QR_BYTES {
        return Err(WireError::TooLarge);
    }
    let payload = qr.strip_prefix(prefix).ok_or(WireError::Malformed)?;
    URL_SAFE_NO_PAD
        .decode_slice(payload, storage)
        .map_err(|_| WireError::Malformed)
}

/// A secret invitation string. Only expose its contents for user-authorized sharing.
pub struct QrCode(Zeroizing<String>);

impl QrCode {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Deref for QrCode {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Debug for QrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QrCode([redacted])")
    }
}
