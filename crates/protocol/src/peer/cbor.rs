use minicbor::{Decoder, data::Type};

use super::{VERSION, WireError};

pub(super) struct Reader<'a>(Decoder<'a>);

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8], limit: usize) -> Result<Self, WireError> {
        if bytes.len() > limit {
            return Err(WireError::TooLarge);
        }
        Ok(Self(Decoder::new(bytes)))
    }

    pub(super) fn array(&mut self) -> Result<u64, WireError> {
        let start = self.0.position();
        let len = self
            .0
            .array()
            .map_err(|_| WireError::Malformed)?
            .ok_or(WireError::Malformed)?;
        self.check_header(start, 0, len)?;
        Ok(len)
    }

    pub(super) fn version(&mut self) -> Result<(), WireError> {
        if self.uint()? != u64::from(VERSION) {
            return Err(WireError::UnsupportedVersion);
        }
        Ok(())
    }

    pub(super) fn uint(&mut self) -> Result<u64, WireError> {
        let start = self.0.position();
        let value = self.0.u64().map_err(|_| WireError::Malformed)?;
        self.check_header(start, 0, value)?;
        Ok(value)
    }

    pub(super) fn bytes(&mut self, limit: usize) -> Result<&'a [u8], WireError> {
        let start = self.0.position();
        let data = self.0.bytes().map_err(|_| WireError::Malformed)?;
        self.check_string_header(start, data.len(), limit)?;
        Ok(data)
    }

    pub(super) fn fixed<const N: usize>(&mut self) -> Result<&'a [u8; N], WireError> {
        self.bytes(N)?.try_into().map_err(|_| WireError::Malformed)
    }

    pub(super) fn text(&mut self, limit: usize) -> Result<&'a str, WireError> {
        let start = self.0.position();
        let text = self.0.str().map_err(|_| WireError::Malformed)?;
        self.check_string_header(start, text.len(), limit)?;
        Ok(text)
    }

    pub(super) fn optional_bytes<const N: usize>(&mut self) -> Result<Option<[u8; N]>, WireError> {
        if self.0.datatype().map_err(|_| WireError::Malformed)? == Type::Null {
            let start = self.0.position();
            self.0.null().map_err(|_| WireError::Malformed)?;
            self.check_header(start, 0, 0)?;
            return Ok(None);
        }
        self.fixed().copied().map(Some)
    }

    pub(super) fn finish(self) -> Result<(), WireError> {
        if self.0.position() != self.0.input().len() {
            return Err(WireError::Malformed);
        }
        Ok(())
    }

    fn check_string_header(&self, start: usize, len: usize, limit: usize) -> Result<(), WireError> {
        if len > limit {
            return Err(WireError::TooLarge);
        }
        self.check_header(start, len, len as u64)
    }

    // Minicbor accepts valid but non-preferred lengths. The v1 wire format permits only
    // the shortest RFC 8949 §4.2 header for each value.
    fn check_header(&self, start: usize, payload: usize, argument: u64) -> Result<(), WireError> {
        let width = match argument {
            0..=23 => 1,
            24..=255 => 2,
            256..=65_535 => 3,
            65_536..=4_294_967_295 => 5,
            _ => 9,
        };
        if self.0.position() - start != width + payload {
            return Err(WireError::Malformed);
        }
        Ok(())
    }
}
