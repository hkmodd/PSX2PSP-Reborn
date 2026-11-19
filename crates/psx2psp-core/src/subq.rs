//! Subchannel Q replacement files (`.sbi`, `.lsd`) → the PSISOIMG subchannel table.
//!
//! A BIN keeps no subchannel data, so the sectors a LibCrypt disc deliberately damages are lost.
//! Redump publishes them as `.sbi` (type byte + Q without CRC, CRC implied bad) or `.lsd` (full Q
//! with CRC); emulators load the file beside the image. POPS reads the same facts from the table
//! at PSISOIMG 0x12D4/0x12D8: pops_0Xg.prx (fw 6.61) walks it in sector order between two
//! `0xFFFFFFFF` sentinels, matches the sector as LBA − 150 and hands the game the entry's 8 bytes
//! (track, index, relative MSF, absolute MSF) for GetlocP.
//!
//! What the game must see is what the drive reports. A Q with a bad CRC never becomes the drive's
//! position, so GetlocP returns the last sector whose Q was valid, usually the previous one
//! (DuckStation does exactly this). A replaced Q with a valid CRC (`.lsd`) is reported verbatim.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::disc::{LEAD_IN_FRAMES, Track};
use crate::error::{Error, Result};
use crate::psar::SUBQ_ENTRY;

const SENTINEL_START: [u8; SUBQ_ENTRY] = [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
const SENTINEL_END: [u8; SUBQ_ENTRY] = [0xFF; SUBQ_ENTRY];

/// A loaded replacement file, ready for [`crate::DiscSpec::subchannels`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub path: PathBuf,
    /// Replaced sectors in the file.
    pub sectors: usize,
    /// Of those, the ones whose Q CRC is bad (all of them for `.sbi`).
    pub damaged: usize,
    pub table: Vec<u8>,
}

/// The replacement file beside a disc image, as emulators look for it: `<image>.sbi`, then
/// `<image>.lsd` (the CUE's name for a CUE, the BIN's for a bare BIN).
pub fn find_beside(image: &Path) -> Option<PathBuf> {
    ["sbi", "lsd", "SBI", "LSD"].into_iter().map(|e| image.with_extension(e)).find(|p| p.is_file())
}

/// [`find_beside`] + [`table`]. `Ok(None)` when there is no file.
pub fn load_beside(image: &Path, tracks: &[Track]) -> Result<Option<Replacement>> {
    let Some(path) = find_beside(image) else { return Ok(None) };
    let bytes = std::fs::read(&path).map_err(Error::io_path(&path))?;
    let entries = parse(&bytes)?;
    let damaged = entries.values().filter(|q| q.is_none()).count();
    Ok(Some(Replacement { sectors: entries.len(), damaged, table: table(&entries, tracks), path }))
}

/// Replaced sectors (LBA − 150) → their Q bytes 1..=9 without the reserved byte (track, index,
/// rel M S F, abs M S F) when the CRC is valid, `None` when it is bad.
pub type Entries = BTreeMap<u32, Option<[u8; 8]>>;

/// Parses an SBI (`"SBI\0"` header) or an LSD (headerless 15-byte records).
pub fn parse(d: &[u8]) -> Result<Entries> {
    let mut out = Entries::new();
    if let Some(mut d) = d.strip_prefix(b"SBI\0") {
        while !d.is_empty() {
            let [m, s, f, kind, ..] = *d else { return Err(Error::format("SBI", "truncated entry")) };
            let len = match kind {
                1 => 10,
                2 | 3 => 3,
                k => return Err(Error::format("SBI", format!("unknown entry type {k}"))),
            };
            if d.len() < 4 + len {
                return Err(Error::format("SBI", "truncated entry"));
            }
            out.insert(sector("SBI", m, s, f)?, None);
            d = &d[4 + len..];
        }
    } else {
        if d.is_empty() || !d.len().is_multiple_of(15) {
            return Err(Error::format("LSD", "not an SBI file and not a whole number of 15-byte LSD records"));
        }
        for r in d.as_chunks::<15>().0 {
            let q = &r[3..15];
            let valid = q[10..12] == (!crc16(&q[..10])).to_be_bytes();
            out.insert(sector("LSD", r[0], r[1], r[2])?, valid.then(|| [q[1], q[2], q[3], q[4], q[5], q[7], q[8], q[9]]));
        }
    }
    if out.is_empty() {
        return Err(Error::format("SBI", "no entries"));
    }
    Ok(out)
}

/// The PSISOIMG subchannel table: start sentinel, one entry per replaced sector in ascending order
/// (POPS walks it both ways from its last hit), end sentinel.
pub fn table(entries: &Entries, tracks: &[Track]) -> Vec<u8> {
    let mut t = Vec::with_capacity((entries.len() + 2) * SUBQ_ENTRY);
    t.extend(SENTINEL_START);
    for (&sector, q) in entries {
        let q = match q {
            Some(q) => *q,
            None => {
                // Like the drive: the newest earlier sector whose Q was valid.
                let mut s = sector;
                while s > 0 && matches!(entries.get(&s), Some(None)) {
                    s -= 1;
                }
                entries.get(&s).copied().flatten().unwrap_or_else(|| position(tracks, s))
            }
        };
        t.extend(sector.to_le_bytes());
        t.extend(q);
    }
    t.extend(SENTINEL_END);
    t
}

/// The Q a clean sector carries: track, index 1, MSF from the track start, absolute MSF.
fn position(tracks: &[Track], sector: u32) -> [u8; 8] {
    let (number, start) = tracks.iter().rev().find(|t| t.start <= sector).map_or((1, 0), |t| (t.number, t.start));
    let [rm, rs, rf] = msf(sector - start);
    let [am, as_, af] = msf(sector + LEAD_IN_FRAMES);
    [bcd(u32::from(number)), 1, rm, rs, rf, am, as_, af]
}

fn sector(what: &'static str, m: u8, s: u8, f: u8) -> Result<u32> {
    let bad = || Error::format(what, format!("bad BCD position {m:02x}:{s:02x}:{f:02x}"));
    let (m, s, f) = (from_bcd(m).ok_or_else(bad)?, from_bcd(s).ok_or_else(bad)?, from_bcd(f).ok_or_else(bad)?);
    if s >= 60 || f >= 75 {
        return Err(bad());
    }
    ((m * 60 + s) * 75 + f).checked_sub(LEAD_IN_FRAMES).ok_or_else(bad)
}

fn from_bcd(b: u8) -> Option<u32> {
    (b >> 4 < 10 && b & 15 < 10).then(|| u32::from(b >> 4) * 10 + u32::from(b & 15))
}

fn bcd(v: u32) -> u8 {
    (((v / 10 % 10) << 4) | (v % 10)) as u8
}

fn msf(s: u32) -> [u8; 3] {
    [bcd(s / 75 / 60), bcd(s / 75 % 60), bcd(s % 75)]
}

/// CRC-16/CCITT (poly 0x1021, init 0) of Q bytes 0..10; the disc stores its complement big-endian.
fn crc16(d: &[u8]) -> u16 {
    d.iter().fold(0u16, |mut v, &b| {
        v ^= u16::from(b) << 8;
        for _ in 0..8 {
            v = if v & 0x8000 != 0 { v << 1 ^ 0x1021 } else { v << 1 };
        }
        v
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TrackKind;
    use crate::psar::libcrypt_subchannels;

    fn data_track() -> Vec<Track> {
        vec![Track { number: 1, kind: TrackKind::Mode2, start: 0, index0: None }]
    }

    fn sbi(lbas: &[u32]) -> Vec<u8> {
        let mut v = b"SBI\0".to_vec();
        for &l in lbas {
            v.extend(msf(l));
            v.push(1);
            v.extend([0x41, 1, 1, 0, 0, 0, 0, 0, 0, 0]); // contents never reach the game
        }
        v
    }

    fn entries(t: &[u8]) -> Vec<(u32, [u8; 8])> {
        t.as_chunks::<SUBQ_ENTRY>().0.iter().map(|e| (u32::from_le_bytes(e[..4].try_into().unwrap()), e[4..].try_into().unwrap())).collect()
    }

    /// The magic-word table models a disc whose both pair sectors are damaged; the same disc as
    /// an SBI must give the identical table, byte for byte.
    #[test]
    fn sbi_of_a_magic_word_disc_equals_the_magic_word_table() {
        const PAIRS: [(u32, u32); 16] = [
            (14105, 14110), (14231, 14236), (14485, 14490), (14579, 14584),
            (14649, 14654), (14899, 14904), (15056, 15061), (15130, 15135),
            (15242, 15247), (15312, 15317), (15378, 15383), (15628, 15633),
            (15919, 15924), (16031, 16036), (16101, 16106), (16167, 16172),
        ];
        let magic = 0xFFFF; // every pair damaged → every table entry is one the SBI also has
        let lbas: Vec<u32> = PAIRS.iter().flat_map(|&(a, b)| [a, b]).collect();
        let t = table(&parse(&sbi(&lbas)).unwrap(), &data_track());
        assert_eq!(t, libcrypt_subchannels(magic));
    }

    #[test]
    fn damaged_sector_reports_the_last_valid_position_and_runs_reach_back() {
        // LBA 14105 alone; 42045..=42047 a run of three damaged sectors.
        let t = table(&parse(&sbi(&[42046, 14105, 42045, 42047])).unwrap(), &data_track());
        let e = entries(&t);
        assert_eq!(e.len(), 6);
        assert_eq!(e[0].0, u32::MAX);
        assert_eq!(e[5], (u32::MAX, [0xFF; 8]));
        // 14105 → 14104: rel 13954 = 03:06:04, abs 03:08:04.
        assert_eq!(e[1], (13955, [1, 1, 0x03, 0x06, 0x04, 0x03, 0x08, 0x04]));
        // Every sector of the run answers the one before it: LBA 42044 = abs 09:20:44.
        for (k, s) in [41895, 41896, 41897].into_iter().enumerate() {
            assert_eq!(e[2 + k], (s, [1, 1, 0x09, 0x18, 0x44, 0x09, 0x20, 0x44]));
        }
    }

    #[test]
    fn lsd_valid_crc_is_verbatim_and_bad_crc_falls_back() {
        let q = |m, s, f, valid: bool| {
            let mut q = vec![0x41, 1, 1, 0x03, 0x06, 0x05, 0, m, s, f];
            let crc = !crc16(&q);
            q.extend(if valid { crc } else { !crc }.to_be_bytes());
            q
        };
        let mut d = Vec::new();
        d.extend([0x03, 0x08, 0x05]);
        d.extend(q(0x23, 0x08, 0x05, true));
        d.extend([0x03, 0x08, 0x06]);
        d.extend(q(0x03, 0x08, 0x06, false));
        let e = entries(&table(&parse(&d).unwrap(), &data_track()));
        assert_eq!(e[1], (13955, [1, 1, 0x03, 0x06, 0x05, 0x23, 0x08, 0x05]));
        // 14106 is damaged; its predecessor 14105 is a valid replacement, so that is reported.
        assert_eq!(e[2], (13956, [1, 1, 0x03, 0x06, 0x05, 0x23, 0x08, 0x05]));
    }

    #[test]
    fn crc_is_ccitt_xmodem() {
        // CRC-16/XMODEM check value.
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn malformed_files_are_errors() {
        for bad in [&b"SBI\0"[..], b"SBI\0\x03\x08\x05\x01\x41", b"SBI\0\x03\x08\x05\x07\0\0\0", b"SBI\0\x0a\x08\x05\x02\0\0\0", b"", &[0u8; 14]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
