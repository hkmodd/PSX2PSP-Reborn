//! Translated code of a native EBOOT (`docs/recomp/PLAN.md`, slice S3): every PS-X EXE of the
//! disc, translated by psx2psp-recomp for a fixed place in PSP memory below the PS1 RAM, with
//! the slice of the dispatch table that routes its guest addresses there.
//!
//! Executables loaded at the same address (the levels of a game, say) are alternatives: they
//! share one host region, and the runtime keeps the one the guest has in RAM. Executables at
//! different addresses get disjoint regions, so a loader and the program it loads can be
//! resident together. Nothing here knows a game: what is translated is what the disc holds.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, OnceLock};

use psx2psp_recomp::emit::patchable;
use psx2psp_recomp::{Discovery, Hazards, Hit, PsxExe, Signatures, Translation, layout, overlay, translate};
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
    /// Bytes of the text up to its last translated word and that word's delay slot (the data
    /// after it is not checked).
    pub text_size: u32,
    /// [`fnv1a32`] of those bytes as loaded: the runtime takes this translation only when the
    /// guest RAM holds exactly that.
    pub hash: u32,
    /// [`fnv1a32`] of the first [`HEAD_LEN`] bytes of the text: the runtime's cheap check before
    /// it hashes the whole text (data loaded over the address never matches it).
    pub head_hash: u32,
    /// Host address of `code[0]`.
    pub host_base: u32,
    /// Allegrex code, little-endian words.
    pub code: Vec<u8>,
    /// Dispatch-table slice for `text_addr..text_addr + text_size`: one little-endian word per
    /// guest word, the host address of its translation or [`layout::MISS`].
    pub table: Vec<u8>,
    /// Guest instructions the runtime executes for the translation (COP0, GTE, syscalls...).
    pub runtime_calls: usize,
    /// Patch table ([`patch_groups`]): one group of little-endian words per
    /// [`PATCH_GROUP_BLOCKS`] blocks of RAM from the text's first block.
    pub patch: Vec<Vec<u8>>,
    /// A file without a PS-X EXE header, placed by [`overlay::locate_all`].
    pub overlay: bool,
}

/// Bytes of RAM per block of the runtime's rewrite check (`psp/psxrt/native.c`, BLK_SHIFT).
pub const PATCH_BLOCK: u32 = 1024;
/// Blocks per group of the patch table.
pub const PATCH_GROUP_BLOCKS: u32 = 4;
/// Largest raw group the runtime reads (its buffer).
pub const PATCH_GROUP_MAX: usize = 32 * 1024;

/// The patch table of a translation (GOAL.MD S20d): for every word of the text that is code
/// (discovered, or a delay slot) and whose rewrite [`patchable`] could take, where the runtime
/// writes its new immediate or shift amount when the game rewrites it.
///
/// Group `g` covers RAM blocks `first + 4g ..` (`first`: the block of the text's first word).
/// Its words: [`PATCH_GROUP_BLOCKS`] + 1 offsets (in words, from the group's start) where each
/// block's entries start and the last one ends, then the entries. An entry is a word
/// `index | n << 8` (`index`: the word's in its block), then either `n` host addresses (the
/// copies to patch, [`Translation::patch`]) or, for `n` 0, the word as translated: it is not
/// one plain copy (it uses `k0`/`k1`, say), and any rewrite sends its block to the interpreter.
pub fn patch_groups(exe: &PsxExe, d: &Discovery, t: &Translation) -> Vec<Vec<u8>> {
    let ram = exe.text_addr & 0x1F_FFFF;
    let len = exe.text.len() as u32;
    let first = ram / PATCH_BLOCK;
    let blocks = (ram + len).div_ceil(PATCH_BLOCK) - first;
    let mut code: BTreeSet<u32> = d.code.clone();
    code.extend(d.delay_slots.keys());
    let head = PATCH_GROUP_BLOCKS as usize + 1;
    let group = |g: u32, pin: bool| {
        let mut w = vec![0u32; head];
        for k in 0..PATCH_GROUP_BLOCKS {
            w[k as usize] = w.len() as u32;
            let lo = (first + g * PATCH_GROUP_BLOCKS + k) * PATCH_BLOCK;
            for o in (lo.max(ram)..(lo + PATCH_BLOCK).min(ram + len)).step_by(4) {
                let pc = exe.text_addr + (o - ram);
                let Some(word) = exe.word(pc).filter(|&x| code.contains(&pc) && patchable(x)) else { continue };
                let index = (o - lo) / 4;
                match t.patch.get(&pc).filter(|h| !pin && h.len() < 256) {
                    Some(h) => {
                        w.push(index | (h.len() as u32) << 8);
                        w.extend(h);
                    }
                    None => w.extend([index, word]),
                }
            }
        }
        w[head - 1] = w.len() as u32;
        w
    };
    (0..blocks.div_ceil(PATCH_GROUP_BLOCKS))
        .map(|g| {
            let mut w = group(g, false);
            if w.len() * 4 > PATCH_GROUP_MAX {
                w = group(g, true); // two words per code word: always fits
            }
            w.iter().flat_map(|x| x.to_le_bytes()).collect()
        })
        .collect()
}

/// All the translated code of a disc.
#[derive(Debug, Clone, Default)]
pub struct CodePack {
    /// Host address of the lowest region (0 when there is no code).
    pub base: u32,
    /// Bytes from `base` to the end of the highest region.
    pub size: u32,
    pub images: Vec<CodeImage>,
    /// Executables and overlays left to the runtime's interpreter, and why.
    pub skipped: Vec<(String, String)>,
}

/// Bytes of text covered by [`CodeImage::head_hash`] (fewer when the text is shorter).
pub const HEAD_LEN: usize = 2048;

/// FNV-1a, 32 bits: the image check the runtime repeats on the guest RAM.
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811C_9DC5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

struct Parsed {
    name: String,
    exe: PsxExe,
    overlay: bool,
    disc: Discovery,
    hz: Hazards,
}

/// Most images the runtime takes (`psp/psxrt/native.c`, MAX_IMAGES).
pub const MAX_IMAGES: usize = 256;
/// Code taken from the guest's RAM ([`crate::captured_code`]).
pub struct Captured {
    pub name: String,
    pub at: u32,
    pub bytes: Vec<u8>,
    /// The RAM it was taken from, at the address of its first byte (`at`'s segment): switch
    /// tables outside the code are read there.
    pub ram: Arc<Vec<u8>>,
    /// Addresses where the guest started running code in it (empty for code found by its shape):
    /// the code is taken with them whether it has a prologue or not, and they are entries of
    /// every image holding these bytes there.
    pub entries: Vec<u32>,
}

/// Smallest piece of captured code translated: one block of the runtime's code checks.
const MIN_CAPTURED: u32 = 0x400;
/// Largest disc file looked at as an overlay: the PS1 RAM above the kernel area.
pub const OVERLAY_MAX: u32 = 0x1F_0000;

/// Translates `exes` (file name, file bytes) and the files of `overlays` that
/// [`overlay::locate_all`] places (S24): executables the guest loads by name, overlays it reads
/// to an address of its own. Files of `exes` that are not a PS-X EXE are reported as skipped,
/// files of `overlays` that are not code are ignored; duplicates (same address and text) are
/// translated once. Overlays never make the build fail: past [`MAX_IMAGES`], or when the code
/// would not fit, they are left to the interpreter (the largest group first). Fails only when
/// the executables alone do not fit between [`CODE_FLOOR`] and the PS1 RAM. `captured` is code
/// taken from the guest's RAM, translated as overlays at its address unless an image already
/// taken holds those bytes there.
pub fn build(exes: Vec<(String, Vec<u8>)>, overlays: Vec<(String, Vec<u8>)>, captured: Vec<Captured>) -> Result<CodePack, String> {
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
            parsed.push((name, exe, false, None));
        }
    }
    let at = overlay::locate_all(&overlays.iter().map(|(_, b)| b.as_slice()).collect::<Vec<_>>());
    for ((name, bytes), at) in overlays.into_iter().zip(at) {
        let Some(exe) = at.and_then(|(base, _)| overlay::as_exe(&bytes, base)) else { continue };
        if parsed.len() >= MAX_IMAGES {
            pack.skipped.push((name, format!("overlay at {:#010x}: more than {MAX_IMAGES} images", exe.text_addr)));
            continue;
        }
        let hash = fnv1a32(&exe.text);
        if seen.insert((exe.text_addr, exe.text.len(), hash)) {
            parsed.push((name, exe, true, None));
        }
    }
    // A captured run keeps only what lies outside the text of every disc image: one image across
    // an executable and the slot its overlays are loaded into would call the overlay resident at
    // capture directly, and run it after another overlay is loaded there.
    let disc: Vec<(u32, u32)> = parsed.iter().map(|(_, x, ..)| (x.text_addr & 0x1FFF_FFFF, (x.text_addr & 0x1FFF_FFFF) + x.text.len() as u32)).collect();
    // Where code ran, with the bytes it ran: a seed of each image holding them there.
    let ran: Vec<(u32, Vec<u8>)> = captured
        .iter()
        .flat_map(|c| c.entries.iter().filter_map(|&e| e.checked_sub(c.at).map(|o| (e, c.bytes[o as usize..(o as usize + 16).min(c.bytes.len())].to_vec()))))
        .collect();
    let captured = captured.into_iter().flat_map(|Captured { at, bytes, ram, entries, .. }| {
        let mut pieces = vec![(at & 0x1FFF_FFFF, (at & 0x1FFF_FFFF) + bytes.len() as u32)];
        for &(s, e) in &disc {
            pieces = pieces.into_iter().flat_map(|(a, b)| [(a, b.min(s)), (a.max(e), b)]).filter(|(a, b)| b > a).collect();
        }
        let seg = at & !0x1FFF_FFFF;
        let base = at & 0x1FFF_FFFF;
        pieces
            .into_iter()
            .filter(|(a, b)| b - a >= MIN_CAPTURED)
            .map(|(a, b)| (seg | a, bytes[(a - base) as usize..(b - base) as usize].to_vec(), (seg, ram.clone()), !entries.is_empty()))
            .collect::<Vec<_>>()
    });
    for (at, bytes, far, ran_code) in captured {
        // Code taken where it ran also leaves out what an image taken before holds as it is:
        // those entries seed that image.
        let mut pieces = vec![(at, at + bytes.len() as u32)];
        for (_, x, ..) in parsed.iter().filter(|_| ran_code) {
            let (xs, xe) = (x.text_addr, x.text_addr + x.text.len() as u32);
            pieces = pieces
                .into_iter()
                .flat_map(|(a, b)| {
                    let (s, e) = (a.max(xs), b.min(xe));
                    let same = s < e && bytes[(s - at) as usize..(e - at) as usize] == x.text[(s - xs) as usize..(e - xs) as usize];
                    if same { [(a, s), (e, b)] } else { [(a, b), (b, b)] }
                })
                .filter(|(a, b)| b > a)
                .collect();
        }
        for (a, b) in pieces.into_iter().filter(|(a, b)| b - a >= MIN_CAPTURED) {
            let (name, bytes) = (format!("RAM {a:08X}"), &bytes[(a - at) as usize..(b - at) as usize]);
            let held = |x: &PsxExe| a.checked_sub(x.text_addr).is_some_and(|o| x.text.get(o as usize..o as usize + bytes.len()) == Some(bytes));
            if parsed.iter().any(|(_, x, ..)| held(x)) {
                continue;
            }
            let first = ran.iter().filter(|(e, m)| (a..b).contains(e) && bytes[(e - a) as usize..].starts_with(m)).map(|&(e, _)| e).min();
            let exe = match (overlay::as_exe(bytes, a), first) {
                (Some(x), None) => x,
                (Some(x), Some(pc0)) => PsxExe { pc0, ..x },
                (None, Some(pc0)) => PsxExe { pc0, gp0: 0, text_addr: a, text: bytes[..bytes.len() & !3].to_vec(), bss: (0, 0), stack: (0, 0) },
                (None, None) => continue,
            };
            if parsed.len() >= MAX_IMAGES {
                pack.skipped.push((name, format!("captured code at {a:#010x}: more than {MAX_IMAGES} images")));
                continue;
            }
            if seen.insert((exe.text_addr, exe.text.len(), fnv1a32(&exe.text))) {
                parsed.push((name, exe, true, Some(far.clone())));
            }
        }
    }
    let discover = |exe: &PsxExe, far: &Option<(u32, Arc<Vec<u8>>)>, seeds: &BTreeSet<u32>| {
        let seeds: Vec<u32> = seeds.iter().copied().collect();
        match far {
            Some((seg, ram)) => Discovery::run_seeded(exe, &[(*seg, &ram[..])], &seeds),
            None => Discovery::run_seeded(exe, &[], &seeds),
        }
    };
    let mut seeds: Vec<BTreeSet<u32>> = parsed
        .iter()
        .map(|(_, x, ..)| ran.iter().filter(|(e, b)| x.contains(*e) && x.text.get((e - x.text_addr) as usize..).is_some_and(|t| t.starts_with(b))).map(|&(e, _)| e).collect())
        .collect();
    let mut discs: Vec<Discovery> = parsed.par_iter().zip(&seeds).map(|((_, exe, _, far), s)| discover(exe, far, s)).collect();
    // A `jal` into the text of another image goes through that image's table: its target must be
    // an entry there even when nothing in that image calls it (Silent Hill's SetSp).
    for _ in 0..3 {
        let grown: Vec<BTreeSet<u32>> = (0..parsed.len())
            .into_par_iter()
            .map(|i| {
                let x = &parsed[i].1;
                let mut s = seeds[i].clone();
                for (j, d) in discs.iter().enumerate().filter(|&(j, _)| j != i) {
                    let y = &parsed[j].1;
                    s.extend(d.calls.keys().copied().filter(|&t| x.contains(t) && !y.contains(t) && !discs[i].code.contains(&t)));
                }
                s
            })
            .collect();
        let redo: Vec<usize> = (0..parsed.len()).filter(|&i| grown[i].len() > seeds[i].len()).collect();
        if redo.is_empty() {
            break;
        }
        seeds = grown;
        let again: Vec<(usize, Discovery)> = redo.into_par_iter().map(|i| (i, discover(&parsed[i].1, &parsed[i].3, &seeds[i]))).collect();
        for (i, d) in again {
            discs[i] = d;
        }
    }
    let parsed: Vec<Parsed> = parsed
        .into_par_iter()
        .zip(discs)
        .map(|((name, exe, overlay, _), disc)| {
            let hz = Hazards::check(&exe, &disc);
            Parsed { name, exe, overlay, disc, hz }
        })
        .collect();

    // Sizes first: a group of executables at one address takes the largest of them.
    let sizes: Vec<Result<u32, String>> = parsed
        .par_iter()
        .map(|p| translate(&p.exe, &p.disc, &p.hz, PROVISIONAL).map(|t| t.words.len() as u32 * 4).map_err(|e| e.to_string()))
        .collect();
    // Group: region size, and whether an executable is in it.
    let mut groups: BTreeMap<u32, (u32, bool)> = BTreeMap::new();
    let mut ok = Vec::new();
    for (p, size) in parsed.into_iter().zip(sizes) {
        match size {
            Ok(n) => {
                let g = groups.entry(p.exe.text_addr).or_default();
                g.0 = g.0.max(n.next_multiple_of(ALIGN));
                g.1 |= !p.overlay;
                ok.push((p, n));
            }
            Err(e) => pack.skipped.push((p.name, e)),
        }
    }
    let base_for = |groups: &BTreeMap<u32, (u32, bool)>| {
        let total: u32 = groups.values().map(|g| g.0).sum();
        (total, layout::RAM.checked_sub(total).map(|b| b & !0xFFFF).filter(|&b| b >= CODE_FLOOR))
    };
    while base_for(&groups).1.is_none() {
        let Some((&at, &(size, _))) = groups.iter().filter(|(_, g)| !g.1).max_by_key(|(_, g)| g.0) else { break };
        groups.remove(&at);
        for (p, _) in ok.iter().filter(|(p, _)| p.exe.text_addr == at) {
            pack.skipped.push((p.name.clone(), format!("overlay at {at:#010x}: its {size} B of code do not fit")));
        }
        ok.retain(|(p, _)| p.exe.text_addr != at);
    }
    if ok.is_empty() {
        return Ok(pack);
    }
    let Some(base) = base_for(&groups).1 else {
        let total = base_for(&groups).0;
        return Err(format!("translated code needs {total} bytes, more than the {} below the PS1 RAM", layout::RAM - CODE_FLOOR));
    };
    let mut at = base;
    for g in groups.values_mut() {
        let len = g.0;
        g.0 = at;
        at += len;
    }
    pack.base = base;
    pack.size = at - base;

    let images: Vec<Result<CodeImage, String>> = ok
        .into_par_iter()
        .map(|(p, len)| {
            let host_base = groups[&p.exe.text_addr].0;
            let t = translate(&p.exe, &p.disc, &p.hz, host_base).map_err(|e| e.to_string())?;
            if t.words.len() as u32 * 4 != len {
                return Err(format!("translation at {host_base:#010x} is {} words, {} at first", t.words.len(), len / 4));
            }
            // The text up to its last translated word and that word's delay slot: the data after
            // it (an overlay's variables, which the game sets before it calls in) stays out of
            // the hashes, or the runtime would never find the text it translated.
            let len = p.exe.text.len() as u32;
            let last = t.map.keys().map(|&g| g.wrapping_sub(p.exe.text_addr)).filter(|&o| o < len).max();
            let text_size = last.map_or(len, |o| (o + 8).min(len));
            let text = &p.exe.text[..text_size as usize];
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
                hash: fnv1a32(text),
                head_hash: fnv1a32(&text[..text.len().min(HEAD_LEN)]),
                host_base,
                code: t.words.iter().flat_map(|w| w.to_le_bytes()).collect(),
                table: table.iter().flat_map(|w| w.to_le_bytes()).collect(),
                runtime_calls: t.runtime.len(),
                patch: patch_groups(&p.exe, &p.disc, &t),
                overlay: p.overlay,
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

/// The PsyQ library signatures ([`psx2psp_recomp::psyq`]), decompressed on first use (~9 MB of
/// text, about 0.1 s). From RecompOne's `psyq.json` (MIT, `resources/psyq_sigs.LICENSE`).
pub fn psyq() -> &'static Signatures {
    static RAW: &[u8] = include_bytes!("../../../resources/psyq_sigs.tsv.z");
    static DB: OnceLock<Signatures> = OnceLock::new();
    DB.get_or_init(|| {
        let text = String::from_utf8(crate::inflate(RAW)).expect("embedded PsyQ signatures are UTF-8");
        Signatures::parse(&text).expect("embedded PsyQ signatures parse")
    })
}

/// The library functions of each executable in `exes` (file name, bytes) that is a PS-X EXE.
pub fn library_functions(exes: &[(String, Vec<u8>)]) -> Vec<(String, PsxExe, Vec<Hit<'static>>)> {
    exes.par_iter()
        .filter_map(|(name, bytes)| {
            let exe = PsxExe::parse(bytes).ok()?;
            let hits = psyq().scan(&exe);
            Some((name.clone(), exe, hits))
        })
        .collect()
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
        let pack = build(
            vec![
            ("A.EXE".into(), exe(0x8001_0000, &[ADDIU_V0_1, JR_RA, 0])),
            ("B.EXE".into(), exe(0x8001_0000, &[JR_RA, ADDIU_V0_1, 0, 0])),
            ("C.EXE".into(), exe(0x8010_0000, &[JR_RA, 0])),
            ("A2.EXE".into(), exe(0x8001_0000, &[ADDIU_V0_1, JR_RA, 0])),
            ("NOTES.TXT".into(), b"hello".to_vec()),
            ],
            vec![],
            vec![],
        )
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
        assert_eq!(c.head_hash, c.hash); // text shorter than HEAD_LEN
        assert!((0..3).all(|i| word(&a.table, i) == layout::MISS || (a.host_base..a.host_base + a.code.len() as u32).contains(&word(&a.table, i))));
    }

    #[test]
    fn the_patch_table_lists_each_patchable_code_word_by_block() {
        const ADDIU_K0_5: u32 = 0x241A_0005; // uses the guest's k0: not one plain copy
        let text = [ADDIU_V0_1, ADDIU_K0_5, JR_RA, ADDIU_V0_1, 0x1234_5678];
        let pack = build(vec![("P.EXE".into(), exe(0x8001_0FF8, &text))], vec![], vec![]).unwrap();
        let p = &pack.images[0];
        let x = PsxExe::parse(&exe(0x8001_0FF8, &text)).unwrap();
        let d = Discovery::run(&x);
        let t = translate(&x, &d, &Hazards::check(&x, &d), p.host_base).unwrap();
        assert_eq!(p.patch.len(), 1, "RAM blocks 0x43-0x44: one group");
        let g: Vec<u32> = p.patch[0].chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let h = |pc: u32| t.patch[&pc].clone();
        let mut want = vec![5, 0, 0, 0, 0];
        want.extend([0xFE | (h(0x8001_0FF8).len() as u32) << 8]);
        want.extend(h(0x8001_0FF8));
        want.extend([0xFF, ADDIU_K0_5]);
        want[1] = want.len() as u32;
        want.extend([1 | (h(0x8001_1004).len() as u32) << 8]); // the jr slot, in the next block
        want.extend(h(0x8001_1004));
        want[2] = want.len() as u32;
        want[3] = want.len() as u32;
        want[4] = want.len() as u32;
        assert_eq!(g, want, "the data word after the slot is left out");
        assert!(!t.patch.contains_key(&0x8001_0FFC));
    }

    /// An overlay for `base`: functions (`addiu sp,sp,-8; jr ra; addiu sp,sp,8`) at word offsets
    /// that form a Golomb ruler (every shift but 0 lines up at most one call with one function),
    /// and a caller that `jal`s each of them.
    fn overlay(base: u32) -> Vec<u8> {
        const AT: [u32; 8] = [0, 4, 16, 36, 60, 88, 128, 136];
        let mut w = vec![0u32; 0x200];
        for (k, &at) in AT.iter().enumerate() {
            w[at as usize..at as usize + 3].copy_from_slice(&[0x27BD_FFF8, JR_RA, 0x27BD_0008]);
            w[200 + 2 * k] = 0x0C00_0000 | (base + 4 * at) >> 2 & 0x03FF_FFFF;
        }
        w[216] = JR_RA;
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn overlays_are_translated_at_the_address_their_calls_name() {
        let pack = build(
            vec![("MAIN.EXE".into(), exe(0x8001_0000, &[ADDIU_V0_1, JR_RA, 0]))],
            vec![("LEVEL.OVL".into(), overlay(0x800A_0000)), ("LEVEL.DAT".into(), vec![0x5A; 0x800])],
            vec![],
        )
        .unwrap();
        assert!(pack.skipped.is_empty(), "{:?}", pack.skipped);
        let [m, o] = &pack.images[..] else { panic!("{:?}", pack.images.iter().map(|i| &i.name).collect::<Vec<_>>()) };
        assert_eq!((m.name.as_str(), m.overlay), ("MAIN.EXE", false));
        // The text checked ends with the last function translated (words 136-138): the caller
        // at 200 is reached by nothing, and the rest is data.
        assert_eq!((o.name.as_str(), o.overlay, o.text_addr, o.text_size), ("LEVEL.OVL", true, 0x800A_0000, 4 * 139));
        assert_eq!(o.hash, fnv1a32(&overlay(0x800A_0000)[..4 * 139]));
        assert!(o.host_base >= m.host_base + m.code.len() as u32, "disjoint regions");
        let word = |i: usize| u32::from_le_bytes(o.table[4 * i..4 * i + 4].try_into().unwrap());
        assert_ne!(word(36), layout::MISS, "each function is translated");
    }

    /// Code captured at `at`, with a RAM of zeros around it.
    fn cap(at: u32, bytes: Vec<u8>) -> Captured {
        let mut ram = vec![0u8; 0x20_0000];
        let o = (at & 0x1F_FFFF) as usize;
        ram[o..o + bytes.len()].copy_from_slice(&bytes);
        Captured { name: format!("RAM {at:08X}"), at, bytes, ram: Arc::new(ram), entries: Vec::new() }
    }

    #[test]
    fn captured_code_is_translated_where_no_image_holds_it() {
        let main = [ADDIU_V0_1, JR_RA, 0, 0x27BD_FFF8, JR_RA, 0x27BD_0008];
        let body = overlay(0x8003_0000);
        let pack = build(
            vec![("MAIN.EXE".into(), exe(0x8001_0000, &main))],
            vec![],
            vec![
                // The executable's own functions, as RAM holds them: nothing new.
                cap(0x8001_000C, main[3..].iter().flat_map(|w| w.to_le_bytes()).collect()),
                // Code no disc file holds as it is (a decompressed body): taken at its address.
                cap(0x8003_0000, body.clone()),
            ],
        )
        .unwrap();
        let names: Vec<&str> = pack.images.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["MAIN.EXE", "RAM 80030000"]);
        let o = &pack.images[1];
        assert_eq!((o.overlay, o.text_addr, o.hash), (true, 0x8003_0000, fnv1a32(&body[..4 * 139])));
    }

    #[test]
    fn calls_from_another_image_are_entries_of_the_image_called() {
        // Silent Hill's SetSp: a leaf after a BIOS stub (`jr t2`), called only from elsewhere.
        let mut body = vec![0x27BD_FFF8, JR_RA, 0x27BD_0008, 0x240A_00B0, 0x0140_0008, 0x2409_0009, 0, 0x03A0_1021, JR_RA, 0x0080_E821];
        body.resize(0x100, 0);
        let body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();
        let set_sp = 0x800A_001Cu32;
        let table = |main: &[u32]| {
            let pack = build(vec![("MAIN.EXE".into(), exe(0x8001_0000, main))], vec![], vec![cap(0x800A_0000, body.clone())]).unwrap();
            let o = pack.images.iter().find(|i| i.text_addr == 0x800A_0000).unwrap();
            o.table.get(28..32).map_or(layout::MISS, |w| u32::from_le_bytes(w.try_into().unwrap()))
        };
        assert_eq!(table(&[JR_RA, 0]), layout::MISS);
        assert_ne!(table(&[0x0C00_0000 | (set_sp >> 2 & 0x03FF_FFFF), 0, JR_RA, 0]), layout::MISS);
    }

    #[test]
    fn code_that_ran_is_translated_without_a_prologue() {
        // Crash 2's code in its NSF data: leaves with no prologue and nothing calling them in
        // the image; a leaf at +0 and one at +0x10, after a `jr ra` and data.
        let mut body = vec![ADDIU_V0_1, JR_RA, 0, 0xFFFF_FFFF, ADDIU_V0_1, JR_RA, 0];
        body.resize(0x100, 0xFFFF_FFFF);
        let body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();
        let at = 0x800A_0000;
        let images = |entries: Vec<u32>| build(vec![], vec![], vec![Captured { entries, ..cap(at, body.clone()) }]).unwrap().images;
        assert!(images(vec![]).is_empty());
        let im = images(vec![at, at + 0x10]);
        let slot = |i: usize| u32::from_le_bytes(im[0].table[4 * i..4 * i + 4].try_into().unwrap());
        assert_eq!((im.len(), im[0].text_addr), (1, at));
        assert!(slot(0) != layout::MISS && slot(4) != layout::MISS);
        // The code an image already holds is left to it, its entries seeding it.
        let main = [JR_RA, 0, 0, 0, ADDIU_V0_1, JR_RA, 0];
        let words: Vec<u8> = main.iter().flat_map(|w| w.to_le_bytes()).collect();
        let pack = build(vec![("MAIN.EXE".into(), exe(0x8001_0000, &main))], vec![], vec![Captured { entries: vec![0x8001_0010], ..cap(0x8001_0000, words) }]).unwrap();
        assert_eq!(pack.images.len(), 1);
        assert_ne!(u32::from_le_bytes(pack.images[0].table[16..20].try_into().unwrap()), layout::MISS);
    }

    #[test]
    fn captured_code_leaves_out_the_text_of_disc_images() {
        // RAM as captured: the executable with a word rewritten, then a body past its text. One
        // image across both would call into the executable's text directly.
        let main = [ADDIU_V0_1, JR_RA, 0, 0x27BD_FFF8, JR_RA, 0x27BD_0008];
        let body = overlay(0x8001_0018);
        let mut ram: Vec<u8> = main.iter().flat_map(|w| w.to_le_bytes()).collect();
        ram[..4].copy_from_slice(&0x2402_0002u32.to_le_bytes());
        ram.extend(&body);
        let pack = build(vec![("MAIN.EXE".into(), exe(0x8001_0000, &main))], vec![], vec![cap(0x8001_0000, ram)]).unwrap();
        let names: Vec<&str> = pack.images.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["MAIN.EXE", "RAM 80010018"]);
        assert_eq!(pack.images[1].hash, fnv1a32(&body[..4 * 139]));
    }
}
