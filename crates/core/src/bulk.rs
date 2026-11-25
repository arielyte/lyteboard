//! Bounded clipboard payloads and a strictly ordered bulk receiver.

use std::io::Cursor;

use image::{
    ColorType, ImageDecoder, ImageEncoder, Limits,
    codecs::{
        jpeg::JpegDecoder,
        png::{PngDecoder, PngEncoder},
        webp::WebPDecoder,
    },
};
pub use lyteboard_protocol::peer::Media;
use lyteboard_protocol::peer::{BulkRecord, CHUNK_BYTES, Manifest, PeerFailure, TransferId};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

const MAX_AXIS: u32 = 8192;
const MAX_PIXELS: u64 = 16_000_000;
const MAX_DECODED: u64 = 64 * 1024 * 1024;
static VALIDATION: Semaphore = Semaphore::const_new(1);

#[derive(Debug, Error)]
pub enum Error {
    #[error("Payload is empty.")]
    Empty,
    #[error("Payload exceeds the {0}-byte transfer limit.")]
    TooLarge(usize),
    #[error("Text must be valid UTF-8 without NUL bytes.")]
    InvalidText,
    #[error("Image is malformed or does not match its declared format.")]
    Image,
    #[error("Image exceeds the dimension or decoded size limit.")]
    Dimensions,
    #[error("Animated images are unsupported.")]
    Animation,
    #[error("Transfer length, ID, or chunk sequence does not match.")]
    Sequence,
    #[error("Transfer digest does not match.")]
    Integrity,
    #[error("Payload validation failed.")]
    Validation,
}

impl Error {
    pub fn peer_failure(&self) -> PeerFailure {
        match self {
            Self::TooLarge(_) | Self::Dimensions => PeerFailure::TooLarge,
            Self::Animation => PeerFailure::UnsupportedMedia,
            Self::Integrity => PeerFailure::IntegrityFailed,
            _ => PeerFailure::InvalidPayload,
        }
    }
}

pub struct Payload {
    media: Media,
    bytes: Zeroizing<Vec<u8>>,
    digest: [u8; 32],
}

impl Payload {
    pub fn text(text: String) -> Result<Self, Error> {
        Self::new(Media::Text, Zeroizing::new(text.into_bytes()), None)
    }

    pub async fn validate(
        media: Media,
        bytes: Zeroizing<Vec<u8>>,
        expected: Option<[u8; 32]>,
    ) -> Result<Self, Error> {
        let permit = VALIDATION.acquire().await.map_err(|_| Error::Validation)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            Self::new(media, bytes, expected)
        })
        .await
        .map_err(|_| Error::Validation)?
    }

    fn new(
        media: Media,
        bytes: Zeroizing<Vec<u8>>,
        expected: Option<[u8; 32]>,
    ) -> Result<Self, Error> {
        validate_size(media, bytes.len())?;
        let digest: [u8; 32] = Sha256::digest(&*bytes).into();
        if expected.is_some_and(|expected| expected != digest) {
            return Err(Error::Integrity);
        }
        if media == Media::Text {
            if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
                return Err(Error::InvalidText);
            }
        } else {
            validate_image(media, &bytes)?;
        }
        Ok(Self {
            media,
            bytes,
            digest,
        })
    }

    pub fn manifest(&self, id: TransferId) -> Manifest {
        Manifest {
            id,
            media: self.media,
            size: self.bytes.len() as u32,
            digest: self.digest,
        }
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn media(&self) -> Media {
        self.media
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_text(&self) -> Option<&str> {
        (self.media == Media::Text)
            .then(|| std::str::from_utf8(&self.bytes).expect("validated text"))
    }
}

pub fn validate_size(media: Media, size: usize) -> Result<(), Error> {
    if size == 0 {
        Err(Error::Empty)
    } else if size > media.limit() {
        Err(Error::TooLarge(media.limit()))
    } else {
        Ok(())
    }
}

/// Ordered chunk cursor shared by LAN senders. Every chunk requires a peer credit.
#[derive(Default)]
pub struct Sender {
    offset: usize,
    next: u32,
}

impl Sender {
    pub fn credit(
        &mut self,
        expected: TransferId,
        payload: &Payload,
        id: TransferId,
        next: u32,
    ) -> Result<BulkRecord, Error> {
        if id != expected || next != self.next {
            return Err(Error::Sequence);
        }
        let bytes = payload.bytes();
        if self.offset == bytes.len() {
            return Ok(BulkRecord::End { id });
        }
        let end = (self.offset + CHUNK_BYTES).min(bytes.len());
        let chunk = Zeroizing::new(bytes[self.offset..end].to_vec());
        self.offset = end;
        self.next += 1;
        Ok(BulkRecord::Chunk {
            id,
            index: next,
            bytes: chunk,
        })
    }
}

pub struct Receiver {
    manifest: Manifest,
    bytes: Zeroizing<Vec<u8>>,
    next: u32,
}

impl Receiver {
    pub fn new(manifest: Manifest) -> Result<Self, Error> {
        validate_size(manifest.media, manifest.size as usize)?;
        Ok(Self {
            bytes: Zeroizing::new(Vec::with_capacity(manifest.size as usize)),
            manifest,
            next: 0,
        })
    }

    pub fn chunk(&mut self, id: TransferId, index: u32, bytes: &[u8]) -> Result<u32, Error> {
        let len = (self.manifest.size as usize - self.bytes.len()).min(CHUNK_BYTES);
        if id != self.manifest.id || index != self.next || len == 0 || bytes.len() != len {
            return Err(Error::Sequence);
        }
        self.bytes.extend_from_slice(bytes);
        self.next += 1;
        Ok(self.next)
    }

    pub async fn finish(self, id: TransferId) -> Result<Payload, Error> {
        if id != self.manifest.id || self.bytes.len() != self.manifest.size as usize {
            return Err(Error::Sequence);
        }
        Payload::validate(self.manifest.media, self.bytes, Some(self.manifest.digest)).await
    }
}

fn validate_image(media: Media, bytes: &[u8]) -> Result<(), Error> {
    decode_image(media, bytes, |_, _, _, _| Ok(()))
}

/// Decode a validated, bounded clipboard image and return a thumbnail at most 128 pixels per axis.
pub async fn thumbnail(
    media: Media,
    bytes: Zeroizing<Vec<u8>>,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    validate_size(media, bytes.len())?;
    let permit = VALIDATION.acquire().await.map_err(|_| Error::Validation)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_image(media, &bytes, sample_thumbnail)
    })
    .await
    .map_err(|_| Error::Validation)?
}

fn sample_thumbnail(
    color: ColorType,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let scale = width.max(height).max(128);
    let columns = (width * 128 / scale).max(1);
    let rows = (height * 128 / scale).max(1);
    let channels = usize::from(color.channel_count());
    let stride = usize::from(color.bytes_per_pixel());
    let mut pixels = Zeroizing::new(Vec::with_capacity((columns * rows) as usize * channels));
    for y in 0..rows {
        for x in 0..columns {
            let start = ((y * height / rows) * width + x * width / columns) as usize * stride;
            let pixel = &bytes[start..start + stride];
            if stride == channels {
                pixels.extend_from_slice(pixel);
            } else if stride == channels * 2 {
                pixels.extend(
                    pixel
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|channel| (u16::from_ne_bytes(*channel) >> 8) as u8),
                );
            } else {
                return Err(Error::Image);
            }
        }
    }
    let mut output = Zeroizing::new(Vec::new());
    let target = match channels {
        1 => ColorType::L8,
        2 => ColorType::La8,
        3 => ColorType::Rgb8,
        4 => ColorType::Rgba8,
        _ => return Err(Error::Image),
    };
    PngEncoder::new(&mut *output)
        .write_image(&pixels, columns, rows, target.into())
        .map_err(image_error)?;
    Ok(output)
}

fn decode_image<T>(
    media: Media,
    bytes: &[u8],
    consume: impl FnOnce(ColorType, u32, u32, &[u8]) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_AXIS);
    limits.max_image_height = Some(MAX_AXIS);
    limits.max_alloc = Some(MAX_DECODED);
    let cursor = Cursor::new(bytes);
    match media {
        Media::Png if bytes.ends_with(b"\0\0\0\0IEND\xaeB`\x82") => {
            let decoder = PngDecoder::with_limits(cursor, limits.clone()).map_err(image_error)?;
            if decoder.is_apng().map_err(image_error)? {
                return Err(Error::Animation);
            }
            decode(decoder, limits, consume)
        }
        Media::Jpeg if bytes.starts_with(b"\xff\xd8") && bytes.ends_with(b"\xff\xd9") => decode(
            JpegDecoder::new(cursor).map_err(image_error)?,
            limits,
            consume,
        ),
        Media::Webp
            if bytes.len() >= 12
                && &bytes[..4] == b"RIFF"
                && &bytes[8..12] == b"WEBP"
                && u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize
                    == bytes.len() - 8 =>
        {
            let decoder = WebPDecoder::new(cursor).map_err(image_error)?;
            if decoder.has_animation() {
                return Err(Error::Animation);
            }
            decode(decoder, limits, consume)
        }
        _ => Err(Error::Image),
    }
}

fn decode<T>(
    mut decoder: impl ImageDecoder,
    limits: Limits,
    consume: impl FnOnce(ColorType, u32, u32, &[u8]) -> Result<T, Error>,
) -> Result<T, Error> {
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED
    {
        return Err(Error::Dimensions);
    }
    decoder.set_limits(limits).map_err(image_error)?;
    let mut output = Zeroizing::new(vec![0; decoder.total_bytes() as usize]);
    let color = decoder.color_type();
    decoder.read_image(&mut output).map_err(image_error)?;
    consume(color, width, height, &output)
}

fn image_error(error: image::ImageError) -> Error {
    if matches!(error, image::ImageError::Limits(_)) {
        Error::Dimensions
    } else {
        Error::Image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, RgbImage};
    use lyteboard_protocol::peer::{MAX_BULK_TEXT_BYTES, MAX_IMAGE_BYTES};

    #[tokio::test]
    async fn history_thumbnail_keeps_aspect_ratio_and_has_bounded_output() {
        let source = image(ImageFormat::Png, 400, 200);
        let preview = thumbnail(Media::Png, Zeroizing::new(source)).await.unwrap();
        let decoded = ::image::load_from_memory_with_format(&preview, ImageFormat::Png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (128, 64));
        assert!(preview.len() <= lyteboard_protocol::ipc::network::MAX_HISTORY_PREVIEW_BYTES);
        assert!(
            thumbnail(Media::Png, Zeroizing::new(vec![1]))
                .await
                .is_err()
        );
    }

    fn image(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::new(width, height))
            .write_to(&mut output, format)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn supported_images_are_checked_before_exposure() {
        for (media, format) in [
            (Media::Png, ImageFormat::Png),
            (Media::Jpeg, ImageFormat::Jpeg),
            (Media::Webp, ImageFormat::WebP),
        ] {
            let bytes = image(format, 16, 16);
            let payload = Payload::new(media, Zeroizing::new(bytes.clone()), None).unwrap();
            assert_eq!(payload.bytes(), bytes);
            let wrong = if media == Media::Png {
                Media::Jpeg
            } else {
                Media::Png
            };
            assert!(matches!(
                Payload::new(wrong, Zeroizing::new(bytes.clone()), None),
                Err(Error::Image)
            ));
            assert!(matches!(
                Payload::new(
                    media,
                    Zeroizing::new(bytes[..bytes.len() / 2].to_vec()),
                    None
                ),
                Err(Error::Image)
            ));
            assert!(matches!(
                Payload::new(media, Zeroizing::new(image(format, MAX_AXIS + 1, 1)), None),
                Err(Error::Dimensions)
            ));
        }
    }

    #[tokio::test]
    async fn large_unicode_and_integrity_across_chunks() {
        let text = format!("a{}z", "é".repeat((MAX_BULK_TEXT_BYTES - 2) / 2));
        let payload = Payload::text(text.clone()).unwrap();
        let id = [7; 16];
        for tamper in [false, true] {
            let mut receiver = Receiver::new(payload.manifest(id)).unwrap();
            for (index, chunk) in payload.bytes().chunks(CHUNK_BYTES).enumerate() {
                let mut chunk = chunk.to_vec();
                if tamper && index == 1 {
                    chunk[0] ^= 1;
                }
                assert_eq!(
                    receiver.chunk(id, index as u32, &chunk).unwrap(),
                    index as u32 + 1
                );
            }
            let result = receiver.finish(id).await;
            if tamper {
                assert!(matches!(result, Err(Error::Integrity)));
            } else {
                assert_eq!(result.unwrap().as_text(), Some(text.as_str()));
            }
        }
    }

    #[tokio::test]
    async fn rejects_invalid_sizes_missing_chunks_and_wrong_order() {
        for (media, size) in [
            (Media::Text, MAX_BULK_TEXT_BYTES + 1),
            (Media::Png, MAX_IMAGE_BYTES + 1),
            (Media::Webp, 0),
        ] {
            let manifest = Manifest {
                id: [1; 16],
                media,
                size: size as u32,
                digest: [0; 32],
            };
            assert!(Receiver::new(manifest).is_err());
        }
        let payload = Payload::text("x".repeat(CHUNK_BYTES + 2)).unwrap();
        let id = [3; 16];
        let mut receiver = Receiver::new(payload.manifest(id)).unwrap();
        assert!(matches!(
            receiver.chunk(id, 1, &[0; 2]),
            Err(Error::Sequence)
        ));
        assert!(matches!(
            receiver.chunk([5; 16], 0, &[0; CHUNK_BYTES]),
            Err(Error::Sequence)
        ));
        receiver
            .chunk(id, 0, &payload.bytes()[..CHUNK_BYTES])
            .unwrap();
        assert!(matches!(receiver.finish(id).await, Err(Error::Sequence)));
    }
}
