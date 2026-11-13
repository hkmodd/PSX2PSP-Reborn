//! Rasterizes `assets/icon.svg` into the files the build embeds:
//! `assets/icon.ico` (Windows executable, every shell size) and `assets/icon.png` (window icon).
//!
//! `cargo run -p psx2psp-app --example icon` after editing the SVG.

use std::path::Path;

use resvg::{tiny_skia, usvg};

/// Sizes Windows asks for at 100–250 % scaling (shell, title bar, taskbar, Explorer views).
const ICO_SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
const WINDOW_SIZE: u32 = 128;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let tree = usvg::Tree::from_data(
        &std::fs::read(assets.join("icon.svg"))?,
        &usvg::Options::default(),
    )?;
    let render = |side: u32| -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut pm = tiny_skia::Pixmap::new(side, side).ok_or("pixmap")?;
        let k = side as f32 / tree.size().width();
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(k, k),
            &mut pm.as_mut(),
        );
        Ok(pm.encode_png()?)
    };
    let images = ICO_SIZES
        .iter()
        .map(|&s| render(s).map(|png| (s, png)))
        .collect::<Result<Vec<_>, _>>()?;
    std::fs::write(assets.join("icon.ico"), ico(&images))?;
    std::fs::write(assets.join("icon.png"), render(WINDOW_SIZE)?)?;
    println!(
        "icon.ico ({} sizes) and icon.png ({WINDOW_SIZE} px) written to {}",
        images.len(),
        assets.display()
    );
    Ok(())
}

/// ICO container with PNG-compressed entries (valid since Windows Vista).
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len();
    for (side, png) in images {
        let b = (*side % 256) as u8; // 0 means 256
        out.extend_from_slice(&[b, b, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in images {
        out.extend_from_slice(png);
    }
    out
}
