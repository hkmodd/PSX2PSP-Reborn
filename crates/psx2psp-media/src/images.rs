//! XMB artwork: decode any common format, fit to the exact PSP slot size, re-encode as RGBA PNG.

use std::io::Cursor;

use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageReader, RgbaImage};

use crate::{MediaError, framing};

/// An XMB image slot with its native size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Game icon, 144×80.
    Icon0,
    /// Information overlay, 310×180.
    Pic0,
    /// Background, 480×272.
    Pic1,
    /// POPS boot splash (STARTDAT), 480×272.
    Boot,
}

impl Slot {
    pub const ALL: [Slot; 4] = [Self::Icon0, Self::Pic0, Self::Pic1, Self::Boot];

    pub const fn size(self) -> (u32, u32) {
        match self {
            Self::Icon0 => (144, 80),
            Self::Pic0 => (310, 180),
            Self::Pic1 | Self::Boot => (480, 272),
        }
    }

    /// Backgrounds fill (crop), icons/overlays fit (transparent letterbox).
    pub const fn fill(self) -> bool {
        matches!(self, Self::Pic1 | Self::Boot)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Icon0 => "ICON0",
            Self::Pic0 => "PIC0",
            Self::Pic1 => "PIC1",
            Self::Boot => "BOOT",
        }
    }
}

/// A slot-ready image: RGBA pixels for preview + the PNG that goes into the EBOOT.
#[derive(Debug, Clone)]
pub struct SlotImage {
    pub slot: Slot,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub png: Vec<u8>,
}

/// Fits `src` to `slot` with content-aware framing (see [`crate::framing`]).
pub fn fit(slot: Slot, src: &RgbaImage) -> RgbaImage {
    if src.dimensions() == slot.size() {
        return src.clone();
    }
    framing::render(slot, src, &framing::plan(slot, src))
}

/// Decodes any supported format, upright (EXIF orientation applied), as RGBA.
pub fn decode(bytes: &[u8]) -> Result<RgbaImage, MediaError> {
    let err = |e: image::ImageError| MediaError::Image(e.to_string());
    let mut decoder = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| MediaError::Image(e.to_string()))?.into_decoder().map_err(err)?;
    let orientation = decoder.orientation().map_err(err)?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(err)?;
    img.apply_orientation(orientation);
    Ok(img.to_rgba8())
}

pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Best, PngFilter::Adaptive)
        .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8)
        .map_err(|e| MediaError::Image(e.to_string()))?;
    Ok(out)
}

/// Decodes `bytes` (PNG/JPEG/BMP/GIF/WebP), fits it to `slot`, encodes the PNG.
pub fn prepare(slot: Slot, bytes: &[u8]) -> Result<SlotImage, MediaError> {
    let src = decode(bytes)?;
    let img = fit(slot, &src);
    let png = if src.dimensions() == slot.size() && bytes.starts_with(b"\x89PNG") {
        bytes.to_vec() // already slot-sized PNG: keep the author's exact bytes
    } else {
        encode_png(&img)?
    };
    Ok(SlotImage { slot, width: img.width(), height: img.height(), rgba: img.into_raw(), png })
}

pub fn prepare_file(slot: Slot, path: &std::path::Path) -> Result<SlotImage, MediaError> {
    let bytes = std::fs::read(path).map_err(|e| MediaError::Io(path.display().to_string(), e))?;
    prepare(slot, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_square_fills_every_slot_without_gaps() {
        let src = RgbaImage::from_pixel(1000, 1000, image::Rgba([255, 0, 0, 255]));
        let icon = fit(Slot::Icon0, &src);
        assert_eq!(icon.dimensions(), (144, 80));
        assert!(icon.pixels().all(|p| *p == image::Rgba([255, 0, 0, 255])), "a flat picture continues its own colour");

        let bg = fit(Slot::Pic1, &src);
        assert_eq!(bg.dimensions(), (480, 272));
        assert!(bg.pixels().all(|p| p[3] == 255), "fill leaves no gaps");
    }

    #[test]
    fn prepare_roundtrips_png() {
        let src = RgbaImage::from_pixel(64, 64, image::Rgba([1, 2, 3, 128]));
        let png = encode_png(&src).unwrap();
        let s = prepare(Slot::Pic0, &png).unwrap();
        assert_eq!((s.width, s.height), (310, 180));
        assert_eq!(s.rgba.len(), 310 * 180 * 4);
        let back = image::load_from_memory(&s.png).unwrap();
        assert_eq!((back.width(), back.height()), (310, 180));
    }
}
