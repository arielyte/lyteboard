use std::{fmt, ops::Deref};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use minicbor::Encoder;
use zeroize::Zeroizing;

use super::{InviteId, MAX_ORIGIN_BYTES, NoiseKey, Origin, Reader, RouteId, VERSION, WireError};

const QR_PREFIX: &str = "lyteboard-pair:v1:";
const MAX_QR_BYTES: usize = 512;
const MAX_DECODED_BYTES: usize = (MAX_QR_BYTES - QR_PREFIX.len()).div_ceil(4) * 3;

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

        let mut qr = Zeroizing::new(String::with_capacity(MAX_QR_BYTES));
        qr.push_str(QR_PREFIX);
        URL_SAFE_NO_PAD.encode_string(&*bytes, &mut qr);
        QrCode(qr)
    }

    pub fn decode_qr(qr: &str) -> Result<Self, WireError> {
        if qr.len() > MAX_QR_BYTES {
            return Err(WireError::TooLarge);
        }
        let payload = qr.strip_prefix(QR_PREFIX).ok_or(WireError::Malformed)?;
        let mut storage = Zeroizing::new([0; MAX_DECODED_BYTES]);
        let used = URL_SAFE_NO_PAD
            .decode_slice(payload, &mut *storage)
            .map_err(|_| WireError::Malformed)?;
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
