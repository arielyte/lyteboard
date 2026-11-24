use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const MAX_BULK_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 32 * 1024;

pub type TransferId = [u8; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Media {
    Text = 0,
    Png = 1,
    Jpeg = 2,
    Webp = 3,
}

impl Media {
    pub fn limit(self) -> usize {
        if self == Self::Text {
            MAX_BULK_TEXT_BYTES
        } else {
            MAX_IMAGE_BYTES
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Text => "text/plain;charset=utf-8",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    pub(super) fn from_tag(tag: u64) -> Result<Self, super::WireError> {
        match tag {
            0 => Ok(Self::Text),
            1 => Ok(Self::Png),
            2 => Ok(Self::Jpeg),
            3 => Ok(Self::Webp),
            _ => Err(super::WireError::Malformed),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    pub id: TransferId,
    pub media: Media,
    pub size: u32,
    pub digest: [u8; 32],
}

#[derive(PartialEq, Eq)]
pub enum BulkRecord {
    Begin(Manifest),
    Credit {
        id: TransferId,
        next: u32,
    },
    Chunk {
        id: TransferId,
        index: u32,
        bytes: Zeroizing<Vec<u8>>,
    },
    End {
        id: TransferId,
    },
}
