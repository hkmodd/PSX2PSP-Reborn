//! Byte-pattern patches over the *user data* of data sectors, with automatic EDC/ECC
//! regeneration for every sector touched. Patterns support `??` wildcards on both sides
//! (a wildcard in the replacement keeps the original byte).
//!
//! Matching is per sector (2048 bytes for Form 1): patterns spanning a sector boundary are
//! not matched — by design, since PS1 code/data patches target specific in-sector words.

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

    /// Applies to every non-overlapping match; returns the number of hits.
    fn apply(&self, data: &mut [u8]) -> usize {
        let n = self.search.len();
        let (mut i, mut hits) = (0, 0);
        while i + n <= data.len() {
            if self.matches(&data[i..i + n]) {
                for (d, r) in data[i..i + n].iter_mut().zip(&self.replace) {
                    if let Some(r) = r {
                        *d = *r;
                    }
                }
                hits += 1;
                i += n;
            } else {
                i += 1;
            }
        }
        hits
    }
}

#[derive(Debug, Clone)]
pub struct PatternPatch {
    pub name: String,
    pub rules: Vec<PatternRule>,
}

impl PatternPatch {
    /// Patches one raw sector; returns true if modified (EDC/ECC already regenerated).
    pub fn apply_sector(&self, s: &mut [u8; SECTOR]) -> bool {
        let Some(range) = sector::user_data_range(sector::mode(s)) else { return false };
        let hits: usize = self.rules.iter().map(|r| r.apply(&mut s[range.clone()])).sum();
        if hits > 0 {
            sector::regenerate(s);
        }
        hits > 0
    }
}

impl ImagePatch for PatternPatch {
    fn apply(&self, _offset: u64, block: &mut [u8]) {
        for chunk in block.chunks_exact_mut(SECTOR) {
            self.apply_sector(chunk.try_into().unwrap());
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
        let p = PatternPatch {
            name: "test".into(),
            rules: vec![PatternRule::from_hex("3C 01 ?? 80", "3C 01 ?? 00").unwrap()],
        };
        assert!(p.apply_sector(&mut s));
        assert_eq!(&s[24 + 100..24 + 104], &[0x3C, 0x01, 0x55, 0x00]);
        assert!(sector::verify_edc(&s));
        assert!(!p.apply_sector(&mut s), "idempotent: second pass finds nothing");
    }

    #[test]
    fn audio_sectors_untouched() {
        let mut s = [0x3Cu8; SECTOR];
        let p = PatternPatch { name: "x".into(), rules: vec![PatternRule::from_hex("3C", "00").unwrap()] };
        assert!(!p.apply_sector(&mut s));
        assert!(s.iter().all(|&b| b == 0x3C));
    }

    #[test]
    fn parse_errors() {
        assert_eq!(PatternRule::from_hex("ZZ", "00"), Err(PatternError::Hex("ZZ".into())));
        assert_eq!(PatternRule::from_hex("00 01", "00"), Err(PatternError::Length(2, 1)));
        assert_eq!(PatternRule::from_hex("", ""), Err(PatternError::Empty));
    }
}
