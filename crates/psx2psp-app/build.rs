//! Embeds `assets/icon.ico` in the Windows executable (Explorer, taskbar, shortcuts).
//!
//! MSVC's linker takes a compiled resource (`.res`) directly, so the resource is written here
//! from the ICO — no resource compiler or extra crate. Other targets need nothing.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    if var("CARGO_CFG_TARGET_OS") != "windows" {
        return;
    }
    if var("CARGO_CFG_TARGET_ENV") != "msvc" {
        println!("cargo:warning=executable icon is embedded on MSVC targets only");
        return;
    }
    let Ok(ico) = std::fs::read("assets/icon.ico") else {
        // First checkout before `cargo run --example icon`: build without an icon.
        println!(
            "cargo:warning=assets/icon.ico missing: run `cargo run -p psx2psp-app --example icon`"
        );
        return;
    };
    let res = PathBuf::from(var("OUT_DIR")).join("icon.res");
    std::fs::write(&res, icon_res(&ico)).expect("write icon.res");
    println!("cargo:rustc-link-arg-bins={}", Path::new(&res).display());
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;

/// A `.res` file with every image of `ico` as an `RT_ICON` (IDs 1…n) and one `RT_GROUP_ICON`
/// (ID 1, the one Windows shows for the executable).
fn icon_res(ico: &[u8]) -> Vec<u8> {
    let u16_at = |o: usize| u16::from_le_bytes([ico[o], ico[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(ico[o..o + 4].try_into().unwrap());
    assert_eq!(&ico[..4], &[0, 0, 1, 0], "not an ICO file");
    let count = u16_at(4) as usize;

    let mut out = Vec::new();
    entry(&mut out, 0, 0, 0, &[]); // the conventional empty first entry
    let mut group = Vec::new();
    group.extend_from_slice(&[0, 0, 1, 0]);
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let d = 6 + 16 * i;
        let (size, offset) = (u32_at(d + 8) as usize, u32_at(d + 12) as usize);
        let id = (i + 1) as u16;
        entry(&mut out, RT_ICON, id, 0x1010, &ico[offset..offset + size]);
        group.extend_from_slice(&ico[d..d + 12]); // width, height, colours, reserved, planes, bpp, size
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut out, RT_GROUP_ICON, 1, 0x1030, &group);
    out
}

/// One resource: 32-byte header (numeric type and name, neutral language), data, DWORD padding.
fn entry(out: &mut Vec<u8>, kind: u16, name: u16, flags: u16, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    for v in [kind, name] {
        out.extend_from_slice(&[0xff, 0xff]);
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // data version
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // language: neutral
    out.extend_from_slice(&[0; 8]); // version, characteristics
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(4), 0);
}
