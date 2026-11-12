//! XMB artwork: decode any common format, fit to the exact PSP slot size, re-encode as RGBA PNG.

use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::imageops::{self, FilterType};
use image::{ImageEncoder, RgbaImage};

use crate::MediaError;

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

pub fn fit(slot: Slot, src: &RgbaImage) -> RgbaImage {
    let (tw, th) = slot.size();
    let (sw, sh) = src.dimensions();
    if (sw, sh) == (tw, th) {
        return src.clone();
    }
    let (sx, sy) = (tw as f64 / sw as f64, th as f64 / sh as f64);
    let scale = if slot.fill() { sx.max(sy) } else { sx.min(sy) };
    let (nw, nh) = (((sw as f64 * scale).round() as u32).max(1), ((sh as f64 * scale).round() as u32).max(1));
    let scaled = imageops::resize(src, nw, nh, FilterType::Lanczos3);
    let mut out = RgbaImage::new(tw, th);
    let dx = (tw as i64 - nw as i64) / 2;
    let dy = (th as i64 - nh as i64) / 2;
    imageops::overlay(&mut out, &scaled, dx, dy);
    out
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
    let src = image::load_from_memory(bytes).map_err(|e| MediaError::Image(e.to_string()))?.to_rgba8();
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
    fn icon_is_letterboxed_background_is_cropped() {
        let src = RgbaImage::from_pixel(1000, 1000, image::Rgba([255, 0, 0, 255]));
        let icon = fit(Slot::Icon0, &src);
        assert_eq!(icon.dimensions(), (144, 80));
        assert_eq!(icon.get_pixel(0, 40)[3], 0, "letterbox is transparent");
        assert_eq!(icon.get_pixel(72, 40)[0], 255);

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
