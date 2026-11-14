//! Translated code of a native EBOOT (`docs/recomp/PLAN.md`, slice S3): every PS-X EXE of the
//! disc, translated by psx2psp-recomp for a fixed place in PSP memory below the PS1 RAM, with
//! the slice of the dispatch table that routes its guest addresses there.
//!
//! Executables loaded at the same address (the levels of a game, say) are alternatives: they
//! share one host region, and the runtime keeps the one the guest has in RAM. Executables at
//! different addresses get disjoint regions, so a loader and the program it loads can be
//! resident together. Nothing here knows a game: what is translated is what the disc holds.

use std::collections::{BTreeMap, HashSet};

use psx2psp_recomp::{Discovery, Hazards, PsxExe, layout, translate};
use rayon::prelude::*;

/// Lowest host address translated code may take: below it are the runtime module, its heap and
/// the PS1 VRAM, SPU RAM and MDEC buffers (`psp/psxrt/main.c`).
pub const CODE_FLOOR: u32 = 0x08C0_0000;
/// Where code is translated first, to learn its size (any base in the runtime's 256 MB region).
const PROVISIONAL: u32 = 0x0880_0000;
/// Alignment of each host region.
const ALIGN: u32 = 0x40;

/// One translated executable.
#[derive(Debug, Clone)]
pub struct CodeImage {
    /// File name on the disc, for the runtime's log.
    pub name: String,
    pub text_addr: u32,
    pub text_size: u32,
    /// [`fnv1a32`] of the text as loaded: the runtime takes this translation only when the
    /// guest RAM holds exactly that.
    pub hash: u32,
    /// Host address of `code[0]`.
    pub host_base: u32,
    /// Allegrex code, little-endian words.
    pub code: Vec<u8>,
    /// Dispatch-table slice for `text_addr..text_addr + text_size`: one little-endian word per
    /// guest word, the host address of its translation or [`layout::MISS`].
    pub table: Vec<u8>,
    /// Guest instructions the runtime executes for the translation (COP0, GTE, syscalls...).
    pub runtime_calls: usize,
}

/// All the translated code of a disc.
#[derive(Debug, Clone, Default)]
pub struct CodePack {
    /// Host address of the lowest region (0 when there is no code).
    pub base: u32,
    /// Bytes from `base` to the end of the highest region.
    pub size: u32,
    pub images: Vec<CodeImage>,
    /// Executables left to the runtime's interpreter, and why.
    pub skipped: Vec<(String, String)>,
}

/// FNV-1a, 32 bits: the image check the runtime repeats on the guest RAM.
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811C_9DC5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

struct Parsed {
    name: String,
    exe: PsxExe,
    hash: u32,
    disc: Discovery,
    hz: Hazards,
}

/// Translates `exes` (file name, file bytes); files that are not a PS-X EXE are ignored,
/// duplicates (same address and text) are translated once. Fails only when the code does not
/// fit between [`CODE_FLOOR`] and the PS1 RAM.
pub fn build(exes: Vec<(String, Vec<u8>)>) -> Result<CodePack, String> {
    let mut pack = CodePack::default();
    let mut seen = HashSet::new();
    let mut parsed = Vec::new();
    for (name, bytes) in exes {
        let exe = match PsxExe::parse(&bytes) {
            Ok(x) => x,
            Err(e) => {
                pack.skipped.push((name, e.to_string()));
                continue;
            }
        };
        if exe.text_addr & 3 != 0 || exe.text.is_empty() {
            pack.skipped.push((name, format!("text at {:#010x}, {} bytes", exe.text_addr, exe.text.len())));
            continue;
        }
        let hash = fnv1a32(&exe.text);
        if seen.insert((exe.text_addr, exe.text.len(), hash)) {
            parsed.push((name, exe, hash));
        }
    }
    let parsed: Vec<Parsed> = parsed
        .into_par_iter()
        .map(|(name, exe, hash)| {
            let disc = Discovery::run(&exe);
            let hz = Hazards::check(&exe, &disc);
            Parsed { name, exe, hash, disc, hz }
        })
        .collect();

    // Sizes first: a group of executables at one address takes the largest of them.
    let sizes: Vec<Result<u32, String>> = parsed
        .par_iter()
        .map(|p| translate(&p.exe, &p.disc, &p.hz, PROVISIONAL).map(|t| t.words.len() as u32 * 4).map_err(|e| e.to_string()))
        .collect();
    let mut groups: BTreeMap<u32, u32> = BTreeMap::new();
    let mut ok = Vec::new();
    for (p, size) in parsed.into_iter().zip(sizes) {
        match size {
            Ok(n) => {
                let g = groups.entry(p.exe.text_addr).or_default();
                *g = (*g).max(n.next_multiple_of(ALIGN));
                ok.push((p, n));
            }
            Err(e) => pack.skipped.push((p.name, e)),
        }
    }
    if ok.is_empty() {
        return Ok(pack);
    }
    let total: u32 = groups.values().sum();
    let base = layout::RAM.checked_sub(total).map(|b| b & !0xFFFF).filter(|&b| b >= CODE_FLOOR);
    let Some(base) = base else {
        return Err(format!("translated code needs {total} bytes, more than the {} below the PS1 RAM", layout::RAM - CODE_FLOOR));
    };
    let mut at = base;
    for size in groups.values_mut() {
        let len = *size;
        *size = at;
        at += len;
    }
    pack.base = base;
    pack.size = at - base;

    let images: Vec<Result<CodeImage, String>> = ok
        .into_par_iter()
        .map(|(p, len)| {
            let host_base = groups[&p.exe.text_addr];
            let t = translate(&p.exe, &p.disc, &p.hz, host_base).map_err(|e| e.to_string())?;
            if t.words.len() as u32 * 4 != len {
                return Err(format!("translation at {host_base:#010x} is {} words, {} at first", t.words.len(), len / 4));
            }
            let text_size = p.exe.text.len() as u32;
            let mut table = vec![layout::MISS; text_size.div_ceil(4) as usize];
            for (&g, &h) in &t.map {
                if let Some(slot) = g.checked_sub(p.exe.text_addr).and_then(|o| table.get_mut(o as usize / 4)) {
                    *slot = h;
                }
            }
            Ok(CodeImage {
                name: p.name,
                text_addr: p.exe.text_addr,
                text_size,
                hash: p.hash,
                host_base,
                code: t.words.iter().flat_map(|w| w.to_le_bytes()).collect(),
                table: table.iter().flat_map(|w| w.to_le_bytes()).collect(),
                runtime_calls: t.runtime.len(),
            })
        })
        .collect();
    for img in images {
        match img {
            Ok(i) => pack.images.push(i),
            Err(e) => return Err(e),
        }
    }
    pack.images.sort_by(|a, b| (a.text_addr, &a.name).cmp(&(b.text_addr, &b.name)));
    Ok(pack)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PS-X EXE at `addr` whose text is `words`.
    fn exe(addr: u32, words: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 0x800];
        b[..8].copy_from_slice(b"PS-X EXE");
        let put = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut b, 0x10, addr);
        put(&mut b, 0x18, addr);
        put(&mut b, 0x1C, words.len() as u32 * 4);
        b.extend(words.iter().flat_map(|w| w.to_le_bytes()));
        b
    }

    const JR_RA: u32 = 0x03E0_0008;
    const ADDIU_V0_1: u32 = 0x2402_0001;

    #[test]
    fn fnv1a32_matches_the_reference() {
        assert_eq!(fnv1a32(b""), 0x811C_9DC5);
        assert_eq!(fnv1a32(b"a"), 0xE40C_292C);
        assert_eq!(fnv1a32(b"foobar"), 0xBF9C_F968);
    }

    #[test]
    fn executables_at_one_address_share_a_region_below_the_ram() {
        let pack = build(vec![
            ("A.EXE".into(), exe(0x8001_0000, &[ADDIU_V0_1, JR_RA, 0])),
            ("B.EXE".into(), exe(0x8001_0000, &[JR_RA, ADDIU_V0_1, 0, 0])),
            ("C.EXE".into(), exe(0x8010_0000, &[JR_RA, 0])),
            ("A2.EXE".into(), exe(0x8001_0000, &[ADDIU_V0_1, JR_RA, 0])),
            ("NOTES.TXT".into(), b"hello".to_vec()),
        ])
        .unwrap();
        assert_eq!(pack.images.len(), 3, "the copy of A is translated once");
        assert_eq!(pack.skipped.len(), 1);
        let [a, b, c] = &pack.images[..] else { unreachable!() };
        assert_eq!((a.name.as_str(), b.name.as_str(), c.name.as_str()), ("A.EXE", "B.EXE", "C.EXE"));
        assert_eq!(a.host_base, b.host_base);
        assert_eq!(a.host_base, pack.base);
        assert!(c.host_base >= a.host_base + a.code.len().max(b.code.len()) as u32);
        assert!(pack.base >= CODE_FLOOR && pack.base + pack.size <= layout::RAM);
        assert_eq!(pack.base & 0xFFFF, 0);
        // Table: the entry of each instruction is its host address, the rest MISS.
        let word = |t: &[u8], i: usize| u32::from_le_bytes(t[4 * i..4 * i + 4].try_into().unwrap());
        assert_eq!(a.table.len(), 12);
        assert_eq!(word(&a.table, 0), a.host_base);
        assert_eq!(c.hash, fnv1a32(&[JR_RA.to_le_bytes(), [0; 4]].concat()));
        assert!((0..3).all(|i| word(&a.table, i) == layout::MISS || (a.host_base..a.host_base + a.code.len() as u32).contains(&word(&a.table, i))));
    }
}
