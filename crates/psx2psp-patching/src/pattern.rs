//! Byte-pattern patches over the *user data* of data sectors, with automatic EDC/ECC
//! regeneration for every sector touched. Patterns support `??` wildcards on both sides
//! (a wildcard in the replacement keeps the original byte).
//!
//! Matching is per sector (2048 bytes for Form 1): patterns spanning a sector boundary are
//! not matched — by design, since PS1 code/data patches target specific in-sector words.

use std::sync::atomic::{AtomicUsize, Ordering};

use psx2psp_core::ImagePatch;

use crate::sector::{self, SECTOR};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternRule {
    pub search: Vec<Option<u8>>,
    pub replace: Vec<Option<u8>>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PatternError {
    #[error("bad hex token `{0}`")]
    Hex(String),
    #[error("search and replace must have equal length ({0} vs {1})")]
    Length(usize, usize),
    #[error("empty pattern")]
    Empty,
}

fn parse_hex(s: &str) -> Result<Vec<Option<u8>>, PatternError> {
    s.split_whitespace()
        .map(|t| match t {
            "??" | "**" => Ok(None),
            t => u8::from_str_radix(t, 16).map(Some).map_err(|_| PatternError::Hex(t.into())),
        })
        .collect()
}

impl PatternRule {
    /// `"3C 01 ?? 80"` → `"3C 01 ?? 00"`.
    pub fn from_hex(search: &str, replace: &str) -> Result<Self, PatternError> {
        let (s, r) = (parse_hex(search)?, parse_hex(replace)?);
        if s.is_empty() {
            return Err(PatternError::Empty);
        }
        if s.len() != r.len() {
            return Err(PatternError::Length(s.len(), r.len()));
        }
        Ok(Self { search: s, replace: r })
    }

    fn matches(&self, at: &[u8]) -> bool {
        self.search.iter().zip(at).all(|(p, b)| p.is_none_or(|p| p == *b))
    }

    /// Applies to every non-overlapping match starting at a multiple of `align`; returns the
    /// number of hits.
    fn apply(&self, data: &mut [u8], align: usize) -> usize {
        let (n, a) = (self.search.len(), align.max(1));
        let (mut i, mut hits) = (0, 0);
        while i + n <= data.len() {
            if self.matches(&data[i..i + n]) {
                for (d, r) in data[i..i + n].iter_mut().zip(&self.replace) {
                    if let Some(r) = r {
                        *d = *r;
                    }
                }
                hits += 1;
                i += n.next_multiple_of(a);
            } else {
                i += a;
            }
        }
        hits
    }
}

#[derive(Debug)]
pub struct PatternPatch {
    pub name: String,
    pub rules: Vec<PatternRule>,
    /// Matches only at user-data offsets that are a multiple of this (4 = MIPS instruction
    /// word). Files start on a sector boundary, so this is also the file-offset alignment.
    pub align: usize,
    /// Sectors modified so far (the patch runs in parallel over blocks).
    sectors: AtomicUsize,
}

impl PatternPatch {
    pub fn new(name: impl Into<String>, rules: Vec<PatternRule>) -> Self {
        Self { name: name.into(), rules, align: 1, sectors: AtomicUsize::new(0) }
    }

    #[must_use]
    pub fn aligned(mut self, align: usize) -> Self {
        self.align = align.max(1);
        self
    }

    /// Number of sectors this patch has modified since it was created.
    pub fn sectors_patched(&self) -> usize {
        self.sectors.load(Ordering::Relaxed)
    }

    /// Applies every rule, in order, to one sector's user data; returns the hits. Counts nothing:
    /// it is the bare transformation [`Self::apply_sector`] performs.
    pub fn apply_user(&self, user: &mut [u8]) -> usize {
        self.rules.iter().map(|r| r.apply(user, self.align)).sum()
    }

    /// Patches one raw sector; returns true if modified (EDC/ECC already regenerated).
    pub fn apply_sector(&self, s: &mut [u8; SECTOR]) -> bool {
        let Some(range) = sector::user_data_range(sector::mode(s)) else { return false };
        let hits = self.apply_user(&mut s[range]);
        if hits > 0 {
            sector::regenerate(s);
            self.sectors.fetch_add(1, Ordering::Relaxed);
        }
        hits > 0
    }

    /// Dry run over raw sectors: hits per rule, in rule order, without modifying anything.
    /// Rules run in sequence on a copy, exactly as [`Self::apply_sector`] would.
    pub fn hits(&self, block: &[u8]) -> Vec<usize> {
        let mut n = vec![0; self.rules.len()];
        for s in block.as_chunks::<SECTOR>().0 {
            let Some(range) = sector::user_data_range(sector::mode(s)) else { continue };
            let mut user = s[range].to_vec();
            for (k, r) in n.iter_mut().zip(&self.rules) {
                *k += r.apply(&mut user, self.align);
            }
        }
        n
    }
}

/// Names of the [`pal_to_ntsc`] rules, in rule order.
pub const PAL_TO_NTSC_RULES: [&str; 6] = ["Y Fix #1", "Y Fix #2", "Graphic Fix #1", "Graphic Fix #2", "Graphic Fix #3", "Graphic Fix #4"];

/// Decides from a dry run ([`PatternPatch::hits`] of [`pal_to_ntsc`]) whether the preset may be
/// applied: only if a video-mode rule (Graphic Fix) hits. The Y fixes re-centre a picture that
/// the video-mode fix has switched to NTSC timing; alone they leave a 50 Hz game with a display
/// range it never uses. Sheep, Dog 'n' Wolf (SLES-02895) is the measured case: Y Fix #2 hits
/// `PutDispEnv` in all 26 executables and no Graphic Fix hits. (Its hang on the loading screen
/// was the loader's executable CRC, now kept valid by [`crate::exe_crc`], not the Y fix itself.)
pub fn pal_to_ntsc_applies(hits: &[usize]) -> bool {
    hits.get(2..).is_some_and(|g| g.iter().any(|&h| h > 0))
}

/// PAL→NTSC video-mode patch: the six generic rules shipped in PSX2PSP v1.4.2's `patches.ini`.
///
/// * **Y Fix #1/#2** zero both immediates of the PsyQ libgpu `PutDispEnv` branch
///   `y1 = screen.y + (PAL ? 0x13 : 0x10)` (`addiu s1/s0,a0,…`), i.e. the GP1(07h) vertical
///   display start (PSX2PSP's "Y Fix": picture no longer shifted down).
/// * **Graphic Fix #1–#4** replace the `lui v0,0x8006/7; lw v0,…(v0)` load of the
///   video-standard flag with `addi v0,zero,0; nop`, i.e. the game always sees "NTSC" and
///   runs its 60 Hz timing.
///
/// Matching is word-aligned so the rules cannot fire inside compressed data at odd offsets.
/// These are generic sequences, not per-game knowledge: run [`PatternPatch::hits`] first and
/// apply only when [`pal_to_ntsc_applies`]. A game where no video-mode rule hits needs a
/// title-specific PPF.
pub fn pal_to_ntsc() -> PatternPatch {
    const Z: &str = "00 00 02 20 00 00 00 00";
    let rule = |s, r| PatternRule::from_hex(s, r).expect("static PAL→NTSC rule");
    PatternPatch::new(
        "PAL→NTSC",
        vec![
            rule("13 00 91 24 10 00 91 24", "00 00 91 24 00 00 91 24"),
            rule("13 00 90 24 10 00 90 24", "00 00 90 24 00 00 90 24"),
            rule("06 80 02 3C 30 DA 42 8C", Z),
            rule("06 80 02 3C 18 F5 42 8C", Z),
            rule("07 80 02 3C 90 EC 42 8C", Z),
            rule("07 80 02 3C 9C F1 42 8C", Z),
        ],
    )
    .aligned(4)
}

impl ImagePatch for PatternPatch {
    fn apply(&self, _offset: u64, block: &mut [u8]) {
        for chunk in block.as_chunks_mut::<SECTOR>().0 {
            self.apply_sector(chunk);
        }
    }

    fn describe(&self) -> String {
        format!("{} ({} rules)", self.name, self.rules.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_patch_and_ecc_regeneration() {
        let mut user = [0u8; 2048];
        user[100..104].copy_from_slice(&[0x3C, 0x01, 0x55, 0x80]);
        user[2046..2048].copy_from_slice(&[0x3C, 0x01]); // truncated at sector end: must not match
        let mut s = sector::build_mode2_form1(500, &user);
        let p = PatternPatch::new("test", vec![PatternRule::from_hex("3C 01 ?? 80", "3C 01 ?? 00").unwrap()]);
        assert!(p.apply_sector(&mut s));
        assert_eq!(&s[24 + 100..24 + 104], &[0x3C, 0x01, 0x55, 0x00]);
        assert!(sector::verify_edc(&s));
        assert!(!p.apply_sector(&mut s), "idempotent: second pass finds nothing");
        assert_eq!(p.sectors_patched(), 1);
    }

    #[test]
    fn audio_sectors_untouched() {
        let mut s = [0x3Cu8; SECTOR];
        let p = PatternPatch::new("x", vec![PatternRule::from_hex("3C", "00").unwrap()]);
        assert!(!p.apply_sector(&mut s));
        assert!(s.iter().all(|&b| b == 0x3C));
    }

    #[test]
    fn pal_to_ntsc_word_aligned() {
        let yfix = [0x13, 0x00, 0x91, 0x24, 0x10, 0x00, 0x91, 0x24];
        let gfix = [0x07, 0x80, 0x02, 0x3C, 0x9C, 0xF1, 0x42, 0x8C];
        let mut user = [0u8; 2048];
        user[64..72].copy_from_slice(&yfix);
        user[512..520].copy_from_slice(&gfix);
        user[1001..1009].copy_from_slice(&yfix); // misaligned: must stay untouched
        let mut s = sector::build_mode2_form1(1234, &user);
        let p = pal_to_ntsc();
        assert_eq!(p.rules.len(), 6);
        assert!(p.apply_sector(&mut s));
        assert_eq!(&s[24 + 64..24 + 72], &[0x00, 0x00, 0x91, 0x24, 0x00, 0x00, 0x91, 0x24]);
        assert_eq!(&s[24 + 512..24 + 520], &[0x00, 0x00, 0x02, 0x20, 0, 0, 0, 0]);
        assert_eq!(&s[24 + 1001..24 + 1009], &yfix);
        assert!(sector::verify_edc(&s));
        assert_eq!(p.sectors_patched(), 1);
    }

    #[test]
    fn dry_run_counts_without_writing_and_gates_y_fix() {
        let yfix = [0x13, 0x00, 0x90, 0x24, 0x10, 0x00, 0x90, 0x24];
        let gfix = [0x06, 0x80, 0x02, 0x3C, 0x18, 0xF5, 0x42, 0x8C];
        let mut user = [0u8; 2048];
        user[64..72].copy_from_slice(&yfix);
        let mut raw = sector::build_mode2_form1(7, &user).to_vec();
        let before = raw.clone();
        let p = pal_to_ntsc();
        let h = p.hits(&raw);
        assert_eq!(h, [0, 1, 0, 0, 0, 0]);
        assert_eq!(raw, before, "dry run leaves the data alone");
        assert_eq!(p.sectors_patched(), 0);
        assert!(!pal_to_ntsc_applies(&h), "Y fix alone (Sheep Dog case) must not be applied");
        user[256..264].copy_from_slice(&gfix);
        raw.extend_from_slice(&sector::build_mode2_form1(8, &user));
        let h = p.hits(&raw);
        assert_eq!(h, [0, 2, 0, 1, 0, 0]);
        assert!(pal_to_ntsc_applies(&h));
        assert_eq!(PAL_TO_NTSC_RULES.len(), p.rules.len());
    }

    #[test]
    fn parse_errors() {
        assert_eq!(PatternRule::from_hex("ZZ", "00"), Err(PatternError::Hex("ZZ".into())));
        assert_eq!(PatternRule::from_hex("00 01", "00"), Err(PatternError::Length(2, 1)));
        assert_eq!(PatternRule::from_hex("", ""), Err(PatternError::Empty));
    }
}
