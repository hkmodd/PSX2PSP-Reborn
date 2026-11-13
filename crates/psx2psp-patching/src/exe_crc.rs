//! Executable checksums that code patches must keep valid.
//!
//! Infogrames' PS1 loader (Sheep, Dog 'n' Wolf: `LOADER.EXE` 0x8011EBC8) loads every PS-X EXE
//! through a check: header word 0x100 = CRC of header bytes 0..0x100, word 0x104 = CRC of the
//! `t_size` body bytes. On a mismatch it reads the file again, three times, then resets the drive
//! and starts over, forever: the loading bar stops. The CRC is the reflected 0xEDB88320 one with
//! init 0xFFFFFFFF and **no** final XOR (table at 0x80132750, routine 0x8011F0AC).
//!
//! The scheme is recognised, never assumed: an executable takes part only when both words match
//! its unpatched contents, a 2^-64 coincidence otherwise. Its words are then recomputed over the
//! patched contents and written back with [`SectorWrites`], the last patch of the chain.

use std::collections::BTreeMap;

use psx2psp_core::{DiscImage, ImagePatch, Result};

use crate::sector::{self, SECTOR};

const CRC_LUT: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = (c >> 1) ^ if c & 1 != 0 { 0xEDB8_8320 } else { 0 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32 (0xEDB88320, init 0xFFFFFFFF) without the final XOR: `!crc32(data)`.
pub fn loader_crc(data: &[u8]) -> u32 {
    data.iter().fold(!0u32, |c, &b| (c >> 8) ^ CRC_LUT[((c ^ b as u32) & 0xFF) as usize])
}

/// 4-byte writes into the user data of given sectors (by LBA), EDC/ECC regenerated.
#[derive(Debug, Default)]
pub struct SectorWrites {
    writes: BTreeMap<u32, Vec<(usize, [u8; 4])>>,
}

impl SectorWrites {
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    pub fn push(&mut self, lba: u32, user_offset: usize, value: [u8; 4]) {
        self.writes.entry(lba).or_default().push((user_offset, value));
    }
}

impl ImagePatch for SectorWrites {
    fn apply(&self, offset: u64, block: &mut [u8]) {
        let first = offset / SECTOR as u64;
        let last = first + (block.len() / SECTOR) as u64;
        let (Ok(first), Ok(last)) = (u32::try_from(first), u32::try_from(last)) else { return };
        for (&lba, ws) in self.writes.range(first..last) {
            let at = (lba - first) as usize * SECTOR;
            let s: &mut [u8; SECTOR] = (&mut block[at..at + SECTOR]).try_into().unwrap();
            let Some(r) = sector::user_data_range(sector::mode(s)) else { continue };
            for (o, v) in ws {
                s[r.start + o..r.start + o + 4].copy_from_slice(v);
            }
            sector::regenerate(s);
        }
    }

    fn describe(&self) -> String {
        format!("executable CRC fix ({} sector(s))", self.writes.len())
    }
}

/// Executables whose loader CRC the patches change, and the writes that make it valid again.
#[derive(Debug, Default)]
pub struct ExeCrcFix {
    /// ISO paths of the executables fixed.
    pub files: Vec<String>,
    pub writes: SectorWrites,
}

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Plans the CRC fix for every PS-X EXE of `img` that uses the loader CRC. `patch` must transform
/// one raw sector (by LBA) exactly as the patch chain will in the stream. A disc without an
/// ISO9660 filesystem has nothing to fix.
pub fn plan(img: &DiscImage, patch: impl Fn(u32, &mut [u8; SECTOR])) -> Result<ExeCrcFix> {
    let mut fix = ExeCrcFix::default();
    let Ok(files) = img.iso_files() else { return Ok(fix) };
    let (mut rd, mut raw) = (img.reader(), [0u8; SECTOR]);
    let user = |raw: &[u8; SECTOR]| sector::user_data_range(sector::mode(raw)).filter(|r| r.len() == 2048);
    'files: for f in files {
        if f.size < 0x800 || f.lba >= img.sectors() {
            continue;
        }
        img.read_sector(&mut rd, f.lba, &mut raw)?;
        let Some(r) = user(&raw) else { continue };
        if &raw[r.start..r.start + 8] != b"PS-X EXE" {
            continue;
        }
        let t_size = word(&raw[r], 0x1C) as usize;
        let n = t_size.div_ceil(2048) as u32;
        if t_size == 0 || 0x800 + t_size > f.size as usize || f.lba.saturating_add(n) >= img.sectors() {
            continue;
        }
        let (mut old, mut new) = (Vec::with_capacity((n as usize + 1) * 2048), Vec::with_capacity((n as usize + 1) * 2048));
        for lba in f.lba..=f.lba + n {
            img.read_sector(&mut rd, lba, &mut raw)?;
            let Some(r) = user(&raw) else { continue 'files };
            old.extend_from_slice(&raw[r.clone()]);
            patch(lba, &mut raw);
            new.extend_from_slice(&raw[r]);
        }
        let body = 0x800..0x800 + t_size;
        if old == new || word(&old, 0x100) != loader_crc(&old[..0x100]) || word(&old, 0x104) != loader_crc(&old[body.clone()]) {
            continue;
        }
        let (h, b) = (loader_crc(&new[..0x100]), loader_crc(&new[body]));
        if word(&new, 0x100) != h {
            fix.writes.push(f.lba, 0x100, h.to_le_bytes());
        }
        if word(&new, 0x104) != b {
            fix.writes.push(f.lba, 0x104, b.to_le_bytes());
        }
        if word(&new, 0x100) != h || word(&new, 0x104) != b {
            fix.files.push(f.path);
        }
    }
    Ok(fix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_crc_is_crc32_without_final_xor() {
        assert_eq!(loader_crc(b"123456789"), !0xCBF4_3926);
        assert_eq!(loader_crc(&[]), 0xFFFF_FFFF);
    }

    #[test]
    fn sector_writes_land_in_user_data_and_keep_edc_valid() {
        let user = [0x11u8; 2048];
        let mut block = [sector::build_mode2_form1(40, &user), sector::build_mode2_form1(41, &user)].concat();
        let mut w = SectorWrites::default();
        w.push(41, 0x104, [1, 2, 3, 4]);
        w.push(99, 0, [9; 4]); // outside the block: ignored
        w.apply(40 * SECTOR as u64, &mut block);
        let (s40, s41): (&[u8; SECTOR], &[u8; SECTOR]) = (block[..SECTOR].try_into().unwrap(), block[SECTOR..].try_into().unwrap());
        assert_eq!(s40, &sector::build_mode2_form1(40, &user), "other sectors untouched");
        assert_eq!(&s41[24 + 0x104..24 + 0x108], &[1, 2, 3, 4]);
        assert!(sector::verify_edc(s41));
    }
}
