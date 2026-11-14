//! Recognition of PsyQ library functions in an executable, from instruction signatures.
//!
//! The signatures are RecompOne's `psyq.json` (github.com/flafmg/RecompOne, MIT, notice in
//! `resources/psyq_sigs.LICENSE`): for each function of the PsyQ libraries (libgpu, libcd,
//! libpress...) and SDK version, its object file, its offset in that object and its words, with
//! the bits relocated at link time as wildcards. The engine embeds them as text, one variant per
//! line: `name TAB library TAB hle TAB object TAB offset TAB words`, the words as 8 hex digits
//! each, `?` for a wildcard nibble.
//!
//! A signature matches where every fixed bit of its words does. At one address the longest
//! match wins, then the one with the most fixed bits; matches never overlap. Identical code under
//! different names (wrappers, aliases) stays ambiguous unless the hit's object is anchored: the
//! object's base (address minus the variant's offset) agrees with other, unambiguous hits.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};

use crate::PsxExe;

/// Shortest signature trusted, in words.
pub const MIN_WORDS: usize = 4;
/// Fewest fixed bits trusted.
const MIN_FIXED_BITS: u32 = 96;

/// One variant of one library function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub library: String,
    /// Marked in the database as a function to replace with native code.
    pub hle: bool,
    /// Object file of the library holding the function.
    pub object: String,
    /// Byte offset of the function in its object's text.
    pub offset: u32,
    value: Vec<u32>,
    mask: Vec<u32>,
}

impl Signature {
    /// Length in words.
    pub fn len(&self) -> usize {
        self.value.len()
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    fn fixed_bits(&self) -> u32 {
        self.mask.iter().map(|m| m.count_ones()).sum()
    }

    fn matches(&self, words: &[u32]) -> bool {
        words.len() >= self.value.len() && self.value.iter().zip(&self.mask).zip(words).all(|((v, m), w)| w & m == *v)
    }
}

/// A recognised function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit<'a> {
    pub addr: u32,
    pub sig: &'a Signature,
    /// Other functions with the same code here (empty when the name is certain).
    pub rivals: Vec<&'a Signature>,
}

impl Hit<'_> {
    /// First address past the function.
    pub fn end(&self) -> u32 {
        self.addr + 4 * self.sig.len() as u32
    }
}

/// The signature database.
#[derive(Clone, Debug, Default)]
pub struct Signatures {
    sigs: Vec<Signature>,
    /// First fully fixed word -> (signature, its index in the signature).
    index: HashMap<u32, Vec<(u32, u32)>>,
}

impl Signatures {
    /// Parses the text form (module docs). Signatures too short or too loose to trust are dropped.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut db = Signatures::default();
        for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.is_empty()) {
            let bad = || format!("signature line {}: {line:.80}", n + 1);
            let f: Vec<&str> = line.split('\t').collect();
            let [name, library, hle, object, offset, words] = f[..] else { return Err(bad()) };
            if words.len() % 8 != 0 || !words.is_ascii() {
                return Err(bad());
            }
            let (mut value, mut mask) = (Vec::with_capacity(words.len() / 8), Vec::with_capacity(words.len() / 8));
            for w in words.as_bytes().chunks_exact(8) {
                let (mut v, mut m) = (0u32, 0u32);
                for &c in w {
                    let (d, k) = match c {
                        b'?' => (0, 0),
                        _ => ((c as char).to_digit(16).ok_or_else(bad)?, 0xF),
                    };
                    v = v << 4 | d;
                    m = m << 4 | k;
                }
                value.push(v);
                mask.push(m);
            }
            let sig = Signature {
                name: name.to_string(),
                library: library.to_string(),
                hle: hle == "1",
                object: object.to_string(),
                offset: offset.parse().map_err(|_| bad())?,
                value,
                mask,
            };
            if sig.len() < MIN_WORDS || sig.fixed_bits() < MIN_FIXED_BITS {
                continue;
            }
            let Some(k) = sig.mask.iter().position(|&m| m == u32::MAX) else { continue };
            db.index.entry(sig.value[k]).or_default().push((db.sigs.len() as u32, k as u32));
            db.sigs.push(sig);
        }
        Ok(db)
    }

    pub fn len(&self) -> usize {
        self.sigs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sigs.is_empty()
    }

    /// The library functions in `exe`'s text, by address.
    pub fn scan(&self, exe: &PsxExe) -> Vec<Hit<'_>> {
        let words: Vec<u32> = exe.text.chunks_exact(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        // Best signatures at each start (word index): longest, then most fixed bits.
        let mut best: BTreeMap<usize, Vec<&Signature>> = BTreeMap::new();
        for (j, w) in words.iter().enumerate() {
            for &(s, k) in self.index.get(w).into_iter().flatten() {
                let Some(i) = j.checked_sub(k as usize) else { continue };
                let sig = &self.sigs[s as usize];
                if !sig.matches(&words[i..]) {
                    continue;
                }
                let at = best.entry(i).or_default();
                let rank = |x: &Signature| (x.len(), x.fixed_bits());
                match at.first().map(|b| rank(b).cmp(&rank(sig))) {
                    Some(std::cmp::Ordering::Greater) => {}
                    Some(std::cmp::Ordering::Equal) => at.push(sig),
                    _ => *at = vec![sig],
                }
            }
        }
        // Non-overlapping, longest first.
        let mut order: Vec<(usize, Vec<&Signature>)> = best.into_iter().collect();
        order.sort_by_key(|(i, s)| (Reverse((s[0].len(), s[0].fixed_bits())), *i));
        let mut taken: BTreeMap<usize, (usize, Vec<&Signature>)> = BTreeMap::new(); // start -> end
        for (i, s) in order {
            let end = i + s[0].len();
            // Accepted ranges are disjoint: the last one starting before `end` ends last.
            if taken.range(..end).next_back().is_none_or(|(_, (e, _))| *e <= i) {
                taken.insert(i, (end, s));
            }
        }
        // Object anchors: where the objects of the certain hits begin.
        let addr = |i: usize| exe.text_addr.wrapping_add(4 * i as u32);
        let seat = |x: &Signature, i: usize| (x.library.clone(), x.object.clone(), addr(i).wrapping_sub(x.offset));
        let mut seats: HashMap<(String, String, u32), u32> = HashMap::new();
        for (&i, (_, s)) in &taken {
            if s.iter().all(|x| x.name == s[0].name) {
                for x in s {
                    *seats.entry(seat(x, i)).or_default() += 1;
                }
            }
        }
        taken
            .into_iter()
            .map(|(i, (_, mut s))| {
                let votes = |x: &Signature| seats.get(&seat(x, i)).copied().unwrap_or(0);
                s.sort_by_key(|x| Reverse(votes(x)));
                let sig = s[0];
                let mut rivals: Vec<&Signature> = Vec::new();
                let anchored = votes(sig) > s.iter().filter(|x| x.name != sig.name).map(|x| votes(x)).max().unwrap_or(0);
                if !anchored {
                    for x in &s[1..] {
                        if x.name != sig.name && rivals.iter().all(|r| r.name != x.name) {
                            rivals.push(x);
                        }
                    }
                }
                Hit { addr: addr(i), sig, rivals }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An executable whose text is `words` at 0x80010000.
    fn exe(words: &[u32]) -> PsxExe {
        let mut b = vec![0u8; 0x800];
        b[..8].copy_from_slice(b"PS-X EXE");
        b[0x10..0x14].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        b[0x1C..0x20].copy_from_slice(&(4 * words.len() as u32).to_le_bytes());
        b[0x30..0x34].copy_from_slice(&0x801F_FFF0u32.to_le_bytes());
        for w in words {
            b.extend_from_slice(&w.to_le_bytes());
        }
        PsxExe::parse(&b).unwrap()
    }

    const DB: &str = "\
Short\tlibx\t0\tA\t0\t27BDFFE8AFBF001003E0000827BD0018
Long\tlibx\t1\tA\t16\t27BDFFE8AFBF00100C??????0000000003E0000827BD0018
Alias1\tliby\t0\tB\t0\t3C02800124420010AC4400000000000003E0000800000000
Alias2\tliby\t0\tC\t32\t3C02800124420010AC4400000000000003E0000800000000
Anchor\tliby\t0\tB\t24\t240A00B001400008240900390000000000000000
";

    #[test]
    fn longest_match_wins_and_wildcards_are_ignored() {
        let db = Signatures::parse(DB).unwrap();
        assert_eq!(db.len(), 5);
        let hits = db.scan(&exe(&[0x27BD_FFE8, 0xAFBF_0010, 0x0C00_1234, 0, 0x03E0_0008, 0x27BD_0018]));
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].addr, hits[0].sig.name.as_str(), hits[0].end()), (0x8001_0000, "Long", 0x8001_0018));
        assert!(hits[0].sig.hle && hits[0].rivals.is_empty());
    }

    #[test]
    fn same_code_is_ambiguous_unless_its_object_is_anchored() {
        let db = Signatures::parse(DB).unwrap();
        let alias = [0x3C02_8001, 0x2442_0010, 0xAC44_0000, 0, 0x03E0_0008, 0];
        let hits = db.scan(&exe(&alias));
        assert_eq!(hits[0].rivals.len(), 1);
        // Anchor sits 24 bytes into object B: B begins at 0x80010000, where Alias1 is.
        let mut words = alias.to_vec();
        words.extend([0x240A_00B0, 0x0140_0008, 0x2409_0039, 0, 0]);
        let hits = db.scan(&exe(&words));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].sig.name, "Anchor");
        assert_eq!((hits[0].sig.name.as_str(), hits[0].rivals.len()), ("Alias1", 0));
    }

    #[test]
    fn bad_lines_are_errors() {
        assert!(Signatures::parse("a\tb\t0\tc\t0\t1234").is_err());
        assert!(Signatures::parse("a\tb\t0\tc\tx\t27BDFFE8AFBF001003E0000827BD0018").is_err());
        assert!(Signatures::parse("a\tb\t0\tc\t0\t27BDFFE8AFBF001003E0000827BD00G8").is_err());
    }
}
