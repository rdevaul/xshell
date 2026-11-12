//! Image ingestion: the one path every image takes into a session.
//!
//! Whether a user attached it or the agent asked to view it, an image is
//! decoded here under hard limits, scaled down to a bounded size, and
//! re-encoded before anything is stored or sent to a provider. So:
//!
//! - only formats xshell decodes itself (PNG, JPEG, GIF, WebP) get in, and a
//!   crafted file meets a bounded decoder rather than the provider's;
//! - no original file bytes, and so no metadata such as EXIF or GPS tags,
//!   ever reach a provider;
//! - the width, height and media type recorded with an image are the ones
//!   this code decoded, never a client's claim.
//!
//! Output is always PNG. It is lossless, which matters for the line art and
//! text in CAD renders and plots, and one output format keeps provider
//! handling simple. A photo re-encoded as PNG is larger than as JPEG, but at
//! the bounded edge it stays far below the store's per-image limit.

use crate::images::{ImageStore, ImageStoreLimits};
use anyhow::Result;
use image::{DynamicImage, ImageFormat, ImageReader, Limits, imageops::FilterType};
use std::io::Cursor;
use thiserror::Error;
use xshell_core::{ImageMediaType, ImageRef};

/// Longest edge of an ingested image, in pixels. Larger images are scaled
/// down, keeping their aspect ratio.
pub const MAX_IMAGE_EDGE: u32 = 1568;
/// Most pixels a source image may have before decoding is refused.
pub const MAX_SOURCE_PIXELS: u64 = 50_000_000;
/// Largest source file accepted, in bytes: the store's per-image limit.
pub const MAX_SOURCE_BYTES: u64 = ImageStoreLimits::DEFAULT_MAX_IMAGE_BYTES;
/// Most memory the decoder may allocate.
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

/// An image after ingestion: re-encoded bytes and what was decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestedImage {
    pub bytes: Vec<u8>,
    pub media_type: ImageMediaType,
    pub width: u32,
    pub height: u32,
}

/// Why an image was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IngestError {
    #[error("the image file is {bytes} bytes; the limit is {limit}")]
    TooLarge { bytes: u64, limit: u64 },
    #[error("the image is {width}x{height} pixels; the limit is {MAX_SOURCE_PIXELS} pixels")]
    TooManyPixels { width: u32, height: u32 },
    #[error("not a supported image (PNG, JPEG, GIF or WebP)")]
    Unsupported,
    #[error("the image could not be decoded: {0}")]
    Invalid(String),
}

fn reader(source: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, IngestError> {
    let mut reader = ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .map_err(|error| IngestError::Invalid(error.to_string()))?;
    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP) => {}
        _ => return Err(IngestError::Unsupported),
    }
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    Ok(reader)
}

/// Decode, bound, scale and re-encode `source`.
pub fn ingest(source: &[u8]) -> Result<IngestedImage, IngestError> {
    let length = source.len() as u64;
    if length > MAX_SOURCE_BYTES {
        return Err(IngestError::TooLarge {
            bytes: length,
            limit: MAX_SOURCE_BYTES,
        });
    }
    // Read the header first, so an oversized image is refused before any
    // pixel memory is allocated.
    let (width, height) = reader(source)?
        .into_dimensions()
        .map_err(|error| IngestError::Invalid(error.to_string()))?;
    if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(IngestError::TooManyPixels { width, height });
    }
    let decoded = reader(source)?
        .decode()
        .map_err(|error| IngestError::Invalid(error.to_string()))?;
    let scaled = if decoded.width().max(decoded.height()) > MAX_IMAGE_EDGE {
        decoded.resize(MAX_IMAGE_EDGE, MAX_IMAGE_EDGE, FilterType::Triangle)
    } else {
        decoded
    };
    // Eight bits per channel, with alpha only when the source has it.
    let normalized = if scaled.color().has_alpha() {
        DynamicImage::ImageRgba8(scaled.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(scaled.to_rgb8())
    };
    let mut bytes = Vec::new();
    normalized
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|error| IngestError::Invalid(error.to_string()))?;
    Ok(IngestedImage {
        bytes,
        media_type: ImageMediaType::Png,
        width: normalized.width(),
        height: normalized.height(),
    })
}

/// Ingest `source` and store it in `store`, labelled `label`.
pub fn ingest_into(store: &ImageStore, source: &[u8], label: &str) -> Result<ImageRef> {
    let image = ingest(source)?;
    store.put(
        &image.bytes,
        image.media_type,
        image.width,
        image.height,
        label,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    /// A PNG of `width` x `height` pixels.
    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_fn(width, height, |x, y| Rgb([x as u8, y as u8, 7]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// The CRC-32 PNG chunks carry.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn decode(bytes: &[u8]) -> DynamicImage {
        image::load_from_memory_with_format(bytes, ImageFormat::Png).unwrap()
    }

    #[test]
    fn a_small_png_keeps_its_size_and_is_re_encoded() {
        let image = ingest(&png(40, 30)).unwrap();
        assert_eq!((image.width, image.height), (40, 30));
        assert_eq!(image.media_type, ImageMediaType::Png);
        assert_eq!(
            decode(&image.bytes).to_rgb8().get_pixel(5, 6),
            &Rgb([5, 6, 7])
        );
    }

    #[test]
    fn a_large_image_is_scaled_to_the_maximum_edge() {
        let image = ingest(&png(3136, 1000)).unwrap();
        assert_eq!((image.width, image.height), (MAX_IMAGE_EDGE, 500));
        let decoded = decode(&image.bytes);
        assert_eq!((decoded.width(), decoded.height()), (MAX_IMAGE_EDGE, 500));
    }

    #[test]
    fn alpha_is_kept_and_jpeg_becomes_png() {
        let rgba = RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 128]));
        let mut source = Vec::new();
        rgba.write_to(&mut Cursor::new(&mut source), ImageFormat::Png)
            .unwrap();
        assert!(decode(&ingest(&source).unwrap().bytes).color().has_alpha());

        let mut jpeg = Vec::new();
        RgbImage::from_pixel(8, 8, Rgb([200, 10, 10]))
            .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        let converted = ingest(&jpeg).unwrap();
        assert_eq!(converted.media_type, ImageMediaType::Png);
        assert!(converted.bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn garbage_truncated_and_oversized_images_are_refused() {
        assert_eq!(
            ingest(b"not an image at all"),
            Err(IngestError::Unsupported)
        );
        let mut truncated = png(64, 64);
        truncated.truncate(truncated.len() / 2);
        assert!(matches!(ingest(&truncated), Err(IngestError::Invalid(_))));
        // A header claiming 10000 x 10000 pixels is refused before any
        // pixels are decoded.
        let mut huge = png(1, 1);
        huge[16..20].copy_from_slice(&10_000_u32.to_be_bytes());
        huge[20..24].copy_from_slice(&10_000_u32.to_be_bytes());
        // Keep the header chunk's checksum valid: type and data, bytes 12..29.
        let crc = crc32(&huge[12..29]);
        huge[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(
            ingest(&huge),
            Err(IngestError::TooManyPixels {
                width: 10_000,
                height: 10_000
            })
        );
        let too_long = vec![0_u8; MAX_SOURCE_BYTES as usize + 1];
        assert!(matches!(
            ingest(&too_long),
            Err(IngestError::TooLarge { .. })
        ));
    }
}
