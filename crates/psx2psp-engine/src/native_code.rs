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

use psx2psp_recomp::emit::{Reloc, mem_op, patchable, repatch, takes_offset, translate_floating};
use psx2psp_recomp::{Discovery, Hazards, Hit, PsxExe, Signatures, Translation, decipher, layout, overlay, translate};
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
    /// One byte per word of the text up to `text_size`, nonzero where discovery found code
    /// (delay slots and fused words included, which have no table entry): the words the
    /// translation runs natively. Not written to the EBOOT.
    pub code_words: Vec<u8>,
}

/// Bytes of RAM per block of the runtime's rewrite check (`psp/psxrt/native.c`, BLK_SHIFT).
pub const PATCH_BLOCK: u32 = 1024;
/// Blocks per group of the patch table.
pub const PATCH_GROUP_BLOCKS: u32 = 4;
/// Largest raw group the runtime reads (its buffer).
pub const PATCH_GROUP_MAX: usize = 32 * 1024;

/// The patch table of a translation (GOAL.MD S20d): for every word of the text that is code
/// (discovered, or a delay slot), where the runtime writes its new immediate or shift amount
/// when the game rewrites it and [`patchable`] can take that, or else the word as translated,
/// which tells the runtime which words a rewrite changed (`psp/psxrt/coderun.h`: those words
/// and the runs of translated code into them go to the interpreter, the rest of the block stays).
///
/// Group `g` covers RAM blocks `first + 4g ..` (`first`: the block of the text's first word).
/// Its words: [`PATCH_GROUP_BLOCKS`] + 1 offsets (in words, from the group's start) where each
/// block's entries start and the last one ends, then the entries. An entry is a word
/// `index | n << 8` (`index`: the word's in its block), then either `n` host addresses (the
/// copies to patch, [`Translation::patch`]) or, for `n` 0, the word as translated: not
/// patchable, or not one plain copy (it uses `k0`/`k1`, say); any rewrite of it is a change.
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
                let Some(word) = exe.word(pc).filter(|_| code.contains(&pc)) else { continue };
                let index = (o - lo) / 4;
                match t.patch.get(&pc).filter(|h| !pin && h.len() < 256 && patchable(word)) {
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
    /// The distinct [`floating_units`] of the overlays [`overlay::locate_all`] does not place,
    /// translated: the runtime moves one where the guest runs its words.
    pub floating: Vec<FloatingCode>,
    /// Bytes where the runtime places [`CodePack::floating`] (0 without it): at least
    /// [`floating_arena`] of their slots; at `base`, or after the head when `blob_host` is 0.
    pub arena: u32,
    /// Host address where the runtime reads the head ([`floating_blob`]) of [`CodePack::floating`], below
    /// `base`; 0 without it, or when the runtime takes head and arena from the user partition
    /// (they do not fit below the images: at most [`FLOATING_PARTITION_MAX`] bytes).
    pub blob_host: u32,
}

/// Most bytes of slots [`floating_arena`] asks for floating code; the room left below the images
/// may give it more.
pub const FLOATING_ARENA_MAX: u32 = 4 << 20;

/// Most bytes of head and arena the runtime takes from the user partition for floating code
/// that does not fit below the images ([`CodePack::blob_host`] 0). A PSP-3000 has 26 MB of it
/// free after psxrt's own memory (PSP log, 2026-11-18): this leaves 10 MB; a PSP-1000 that has
/// not the room runs the floating code in the interpreter.
pub const FLOATING_PARTITION_MAX: u32 = 16 << 20;

/// The bodies of `codes` ([`FloatingCode::body`]) as DATA.PSAR holds them, one after another,
/// each packed by `pack` (deflate, or the bytes as they are): their bytes, and each one's byte
/// offset there and length.
pub fn floating_bodies(codes: &[FloatingCode], pack: impl Fn(&[u8]) -> Vec<u8>) -> (Vec<u8>, Vec<(u32, u32)>) {
    let mut out = Vec::new();
    let mut at = Vec::with_capacity(codes.len());
    for c in codes {
        let z = pack(&c.body().iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>());
        at.push((out.len() as u32, z.len() as u32));
        out.extend_from_slice(&z);
    }
    (out, at)
}

/// The head of [`CodePack::floating`] as the runtime keeps it in RAM (`psp/psxrt/floating.h`):
/// header, units, the index of their entries by [`floating_key`] of the two words there, and the
/// data (guest words, sites, map); `bodies` gives each unit's body in [`floating_bodies`] (the
/// simulator, which does not read them, takes zeros).
pub fn floating_blob(codes: &[FloatingCode], bodies: &[(u32, u32)]) -> Vec<u32> {
    assert_eq!(codes.len(), bodies.len());
    let mut index: Vec<[u32; 3]> = Vec::new();
    for (u, c) in codes.iter().enumerate() {
        for e in c.indexed() {
            index.push([floating_key(&c.unit.words[e..e + 2]), u as u32, e as u32]);
        }
    }
    index.sort_unstable();
    let head = 4 + FLOATING_UNIT_WORDS * codes.len() + 3 * index.len();
    let mut w = vec![codes.len() as u32, index.len() as u32, FLOATING_BASE, FLOATING_HOST];
    let mut data: Vec<u32> = Vec::new();
    for (c, &(b, bl)) in codes.iter().zip(bodies) {
        let mut put = |v: &[u32]| {
            let at = (head + data.len()) as u32;
            data.extend_from_slice(v);
            at
        };
        let (g, s, m) = (put(&c.unit.words), put(&c.sites), put(&c.map()));
        let (gl, sl, hl, rl) = (c.unit.words.len(), c.sites.len() / 2, c.code.len(), c.relocs.len());
        w.extend([g, gl as u32, s, sl as u32, m, hl as u32, rl as u32, b, bl]);
    }
    w.extend(index.iter().flatten());
    w.extend(data);
    w
}

/// Words of a unit's header in [`floating_blob`] (`psp/psxrt/floating.h` FlUnit).
const FLOATING_UNIT_WORDS: usize = 9;

/// How far back from a word the runtime looks for an index entry (`FL_WALK`): every word a unit
/// translates has an entry at most `FLOATING_WALK - 1` words before it.
const FLOATING_WALK: usize = 8;

/// `w` without what a site of floating code takes from RAM (`psp/psxrt/floating.c` fl_form): a
/// j/jal's target, an immediate (addi as addiu), a load's or store's offset or a shift amount.
pub fn floating_form(w: u32) -> u32 {
    match (w >> 26, w & 0x3F) {
        (2 | 3, _) => w & 0xFC00_0000,
        (8, _) => (w | 1 << 26) & 0xFFFF_0000,
        (9..=0xF, _) => w & 0xFFFF_0000,
        _ if mem_op(w) => w & 0xFFFF_0000,
        (0, 0 | 2 | 3) => w & !(0x1F << 6),
        _ => w,
    }
}

/// The index key of two words of floating code (`fl_key`): [`fnv1a32`] of their [`floating_form`].
pub fn floating_key(w: &[u32]) -> u32 {
    let b: Vec<u8> = w.iter().flat_map(|&x| floating_form(x).to_le_bytes()).collect();
    fnv1a32(&b)
}

/// Host address a [`FloatingCode`] is translated at ([`translate_floating`]).
pub const FLOATING_HOST: u32 = PROVISIONAL;

/// A [`FloatingUnit`] translated at [`FLOATING_BASE`] and [`FLOATING_HOST`]: the runtime copies
/// `code` to its place and applies `relocs` as [`Translation::moved`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatingCode {
    pub unit: FloatingUnit,
    /// Allegrex code.
    pub code: Vec<u32>,
    /// Per word of the unit: offset of its translation in `code` (words), or `u32::MAX`.
    pub table: Vec<u32>,
    /// Per word of `code` that depends on where it is: its offset (words) << 2 | the kind
    /// ([`reloc_kind`]).
    pub relocs: Vec<u32>,
    /// Pairs by guest word (`psp/psxrt/floating.h`): `j << 1 | kind`, host word offset. Kind 0:
    /// the host word is a copy of guest word `j` ([`Translation::patch`], [`patchable`]); kind 1:
    /// the lui/ori pair from it holds the target of the j/jal at `j` ([`Translation::targets`]).
    /// Where RAM may differ from the unit: a game that links the code where it loads it.
    pub sites: Vec<u32>,
}

/// The two bits a [`FloatingCode`] relocation keeps of its kind.
pub fn reloc_kind(r: Reloc) -> u32 {
    match r {
        Reloc::Guest => 0,
        Reloc::GuestPair => 1,
        Reloc::Jump => 2,
        Reloc::HostPair => 3,
    }
}

impl FloatingCode {
    /// The translation of `unit` at [`FLOATING_BASE`] and [`FLOATING_HOST`].
    pub fn translate(unit: FloatingUnit) -> Result<Self, String> {
        let (exe, d) = unit.discover();
        let t = translate_floating(&exe, &d, &Hazards::check(&exe, &d), FLOATING_HOST).map_err(|e| e.to_string())?;
        let at = |h: u32| (h - FLOATING_HOST) / 4;
        let mut table = vec![u32::MAX; unit.words.len()];
        for (&g, &h) in &t.map {
            if let Some(slot) = g.checked_sub(FLOATING_BASE).and_then(|o| table.get_mut(o as usize / 4)) {
                *slot = at(h);
            }
        }
        let relocs = t.relocs.iter().map(|&(h, r)| at(h) << 2 | reloc_kind(r)).collect();
        let word = |g: u32| g.checked_sub(FLOATING_BASE).map(|o| o / 4).filter(|&j| (j as usize) < unit.words.len());
        let mut sites: Vec<(u32, u32)> = Vec::new();
        for (&g, hs) in &t.patch {
            let Some(j) = word(g) else { continue };
            let w = unit.words[j as usize];
            // every copy takes the word: one that does not (k0/k1, a fused pair, a group's
            // offset) pins it
            let host = |h: u32| t.words[at(h) as usize];
            if (patchable(w) && hs.iter().all(|&h| repatch(host(h), w).is_some())) || hs.iter().all(|&h| takes_offset(host(h), w)) {
                sites.extend(hs.iter().map(|&h| (j << 1, at(h))));
            }
        }
        sites.extend(t.targets.iter().filter_map(|&(h, g)| Some((word(g)? << 1 | 1, at(h)))));
        sites.sort_unstable();
        let sites = sites.into_iter().flat_map(|(a, b)| [a, b]).collect();
        Ok(Self { unit, code: t.words, table, relocs, sites })
    }

    /// The words [`floating_blob`] indexes, each with a word after it: for each translated word
    /// with none of them [`FLOATING_WALK`] - 1 words back or less, it (the last but one when it is
    /// the last).
    fn indexed(&self) -> Vec<usize> {
        let n = self.unit.words.len();
        let mut out: Vec<usize> = Vec::new();
        for j in (0..n).filter(|&j| self.table[j] != u32::MAX && n >= 2) {
            if out.last().is_none_or(|&e| j - e >= FLOATING_WALK) {
                out.push(j.min(n - 2));
            }
        }
        out
    }

    /// A bit per guest word, from bit 0 of the first: the word has a translation.
    fn map(&self) -> Vec<u32> {
        let mut m = vec![0u32; self.unit.words.len().div_ceil(32)];
        for (j, &t) in self.table.iter().enumerate() {
            if t != u32::MAX {
                m[j / 32] |= 1 << (j % 32);
            }
        }
        m
    }

    /// What DATA.PSAR holds of it, read into its slot when the runtime places it: code, table,
    /// relocations.
    pub fn body(&self) -> Vec<u32> {
        [&self.code[..], &self.table, &self.relocs].concat()
    }

    /// Its words in [`floating_blob`]: unit header, index entries, guest words, sites and map.
    pub fn blob_words(&self) -> usize {
        FLOATING_UNIT_WORDS + 3 * self.indexed().len() + self.unit.words.len() + self.sites.len() + self.unit.words.len().div_ceil(32)
    }

    /// Bytes of its slot in the arena: its body's, rounded up to 64 (`fl_alloc`).
    pub fn slot_bytes(&self) -> u64 {
        (4 * (self.code.len() + self.table.len() + self.relocs.len()) as u64).next_multiple_of(64)
    }
}

/// Lays out `pack.floating` for images from host address `base` down, the runtime's own memory
/// under `floor`, and sets `pack.base` to the lowest region. Below the images: the arena where the
/// runtime reads units' bodies and places them, and below it the head it finds them with. Units go
/// in by head size, smallest first, while both fit over `floor`; the arena then takes the room
/// left, up to twice the slots. When not all of them fit there, the runtime takes head and arena
/// from the user partition instead (`blob_host` 0, the arena [`floating_arena`] of the slots), up
/// to [`FLOATING_PARTITION_MAX`] bytes of it. Units left out go to `pack.skipped`.
fn place_floating(pack: &mut CodePack, base: u32, floor: u32) {
    let blob_room = |words: u64| (4 * words).next_multiple_of(0x1_0000);
    let all_words = 4 + pack.floating.iter().map(|c| c.blob_words() as u64).sum::<u64>();
    let all_host: u64 = pack.floating.iter().map(FloatingCode::slot_bytes).sum();
    let largest = pack.floating.iter().map(FloatingCode::slot_bytes).max().unwrap_or(0);
    let below = largest <= floating_arena(all_host) && blob_room(all_words) + floating_arena(all_host) <= u64::from(base - floor);
    let room = if below { u64::from(base - floor) } else { u64::from(FLOATING_PARTITION_MAX) };
    let mut order: Vec<(usize, usize)> = pack.floating.iter().map(FloatingCode::blob_words).enumerate().map(|(k, w)| (w, k)).collect();
    order.sort_unstable();
    let (mut keep, mut words, mut host) = (vec![false; pack.floating.len()], 4u64, 0u64);
    for (w, k) in order {
        let code = pack.floating[k].slot_bytes();
        let (w2, h2) = (words + w as u64, host + code);
        if code <= floating_arena(h2) && blob_room(w2) + floating_arena(h2) <= room {
            (keep[k], words, host) = (true, w2, h2);
        }
    }
    let left = keep.iter().filter(|&&k| !k).count();
    if left > 0 {
        let place = if below { "below the images" } else { "of the user partition" };
        pack.skipped.push(("floating code".into(), format!("{left} of {} units do not fit in the {room} B {place}", keep.len())));
    }
    let mut keep = keep.into_iter();
    pack.floating.retain(|_| keep.next() == Some(true));
    (pack.arena, pack.blob_host) = (0, 0);
    if !pack.floating.is_empty() && below {
        pack.arena = (2 * host + 0x1_0000).min(room - blob_room(words)) as u32 & !0xFFFF;
        pack.blob_host = base - pack.arena - blob_room(words) as u32;
    } else if !pack.floating.is_empty() {
        pack.arena = floating_arena(host) as u32;
    }
    pack.base = if below { base - pack.arena } else { base };
}

/// The arena for floating code of `host` bytes: room for each unit twice, at most
/// [`FLOATING_ARENA_MAX`].
fn floating_arena(host: u64) -> u64 {
    (2 * host + 0x1_0000).next_multiple_of(0x1_0000).min(u64::from(FLOATING_ARENA_MAX))
}

/// Where floating code is discovered and translated before it is moved ([`Translation::moved`]):
/// the start of RAM, where no game's code is at a fixed address above the kernel area.
pub const FLOATING_BASE: u32 = 0x8000_0000;

/// A piece of code the game runs wherever it loads it: words, and where in them the guest may
/// enter (word offsets).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FloatingUnit {
    pub words: Vec<u32>,
    pub entries: Vec<u32>,
}

impl FloatingUnit {
    /// The unit's code discovered on its own at [`FLOATING_BASE`].
    pub fn discover(&self) -> (PsxExe, Discovery) {
        let exe = PsxExe { pc0: FLOATING_BASE, gp0: 0, text_addr: FLOATING_BASE, text: self.words.iter().flat_map(|w| w.to_le_bytes()).collect(), bss: (0, 0), stack: (0, 0) };
        let seeds: Vec<u32> = self.entries.iter().map(|&o| FLOATING_BASE + 4 * o).collect();
        let mut d = Discovery::run_file(&exe, &[], &[], &seeds);
        d.explore_known(&exe, &seeds);
        (exe, d)
    }
}

/// The floating units of `bytes`, a file or a piece of one whose code the game places where it
/// wants (Crash 2's native GOOL pieces sit in the bytecode of entries it loads in any page): each
/// run of consecutive words that the file's discovery at [`FLOATING_BASE`] finds as code, with its
/// byte offset in `bytes`. A run is loaded whole wherever the file is (the words around it are
/// what the file holds there); runs are apart, as the pages of a file may be.
pub fn floating_units(bytes: &[u8]) -> Vec<(usize, FloatingUnit)> {
    let exe = overlay::as_exe(bytes, FLOATING_BASE).unwrap_or(PsxExe { pc0: FLOATING_BASE, gp0: 0, text_addr: FLOATING_BASE, text: bytes[..bytes.len() & !3].to_vec(), bss: (0, 0), stack: (0, 0) });
    let d = Discovery::run_file(&exe, &[], &[], &[]);
    let word = |a: u32| exe.word(a).unwrap_or(0);
    let mut out: Vec<(usize, FloatingUnit)> = Vec::new();
    // Zero words no instruction falls into (at a run's start, or after the delay slot of a jump
    // that does not return), two or more, are padding (one is a `nop` code may be entered at:
    // Crash 2's GOOL native code): RAM holds other data there (Spider-Man's CD.WAD modules
    // follow each other with zeros the game loads its data over), so a unit around it never
    // matches. Units are cut there, and the padding is units of its own, `nop`s only, at most
    // FLOATING_WALK words each (the same few in every pack): code may be entered at them (Crash 2
    // enters GOOL native code at four `nop`s after an op word, 8014F7E4 in its first level, where
    // the island it finds starts), and a `nop` placed where RAM holds a zero is exact anywhere.
    let ends = |w: u32| w >> 26 == 2 || (w >> 26 == 0 && w & 0x3F == 8) || w & 0xFFFF_0000 == 0x1000_0000;
    let mut pad = None;
    let mut last = None;
    for &a in &d.code {
        let run = last == Some(a.wrapping_sub(4));
        let zero = |a: u32| word(a) == 0 && d.code.contains(&a) && !d.delay_slots.contains_key(&a);
        if zero(a) && (pad == Some(a - 4) || zero(a + 4) && (!run || d.delay_slots.get(&(a - 4)).is_some_and(|&j| ends(word(j))))) {
            if pad != Some(a - 4) || out.last().is_none_or(|u| u.1.words.len() >= FLOATING_WALK) {
                out.push(((a - FLOATING_BASE) as usize, FloatingUnit { words: Vec::new(), entries: Vec::new() }));
            }
            let u = &mut out.last_mut().unwrap().1;
            u.entries.push(u.words.len() as u32);
            u.words.push(0);
            pad = Some(a);
            last = None;
            continue;
        }
        if !run {
            out.push(((a - FLOATING_BASE) as usize, FloatingUnit { words: Vec::new(), entries: Vec::new() }));
        }
        let u = &mut out.last_mut().unwrap().1;
        // Every word the file's code reaches other than a delay slot seeds the unit's own
        // discovery: a jump table or a pointer in the file's data outside the run is not in it.
        if !d.delay_slots.contains_key(&a) {
            u.entries.push(u.words.len() as u32);
        }
        u.words.push(word(a));
        last = Some(a);
    }
    out
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
    /// Only the entries count: the words that ran are as taken, others (variables) changed.
    pub seeds_only: bool,
}

/// A disc file, or a run of code cut from one, that [`overlay::locate_all`] may place.
pub struct Overlay {
    pub name: String,
    pub bytes: Vec<u8>,
    /// The file `bytes` were cut from and their offset in it (deciphered when they are): switch
    /// and pointer tables outside the code are read there.
    pub file: Option<(u32, Arc<Vec<u8>>)>,
}

impl Overlay {
    /// A whole file, or an archive entry: nothing around it.
    pub fn whole(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self { name: name.into(), bytes, file: None }
    }
}

/// The data discovery reads around an image's text.
enum Around {
    /// The RAM a captured run was taken from, at the address of its first byte.
    Ram(u32, Arc<Vec<u8>>),
    /// The disc file an overlay was cut from, at the address of its first byte.
    File(u32, Arc<Vec<u8>>),
}

/// Smallest piece of code found by its shape translated: one block of the runtime's code checks.
/// Code taken where it ran has no false finds: a piece of it is translated at any size if it
/// holds an entry (Crash 2 runs 0x130 bytes of GOOL native code alone at 8016F4B4).
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
pub fn build(exes: Vec<(String, Vec<u8>)>, overlays: Vec<Overlay>, captured: Vec<Captured>) -> Result<CodePack, String> {
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
    // An overlay runs in the segment of the executables that call it: kuseg when they are linked
    // there (Lilo & Stitch at 00018000 calls its overlay's 000DA070), its `jal` and links with it.
    let segs: BTreeSet<u32> = parsed.iter().map(|(_, x, ..): &(_, PsxExe, _, _)| x.text_addr & !0x1FFF_FFFF).collect();
    let seg = if segs.len() == 1 { segs.into_iter().next().unwrap() } else { 0x8000_0000 };
    let at = overlay::locate_all(&overlays.iter().map(|o| o.bytes.as_slice()).collect::<Vec<_>>());
    let mut unplaced = Vec::new();
    for (Overlay { name, bytes, file }, at) in overlays.into_iter().zip(at) {
        let Some((base, exe)) = at.map(|(b, _)| seg | b & 0x1FFF_FFFF).and_then(|base| Some((base, overlay::as_exe(&bytes, base)?))) else {
            unplaced.push((name, bytes));
            continue;
        };
        if parsed.len() >= MAX_IMAGES {
            pack.skipped.push((name, format!("overlay at {:#010x}: more than {MAX_IMAGES} images", exe.text_addr)));
            continue;
        }
        let hash = fnv1a32(&exe.text);
        if seen.insert((exe.text_addr, exe.text.len(), hash)) {
            parsed.push((name, exe, true, file.map(|(o, f)| Around::File(base.wrapping_sub(o), f))));
        }
    }
    // Code an image deciphers in its own text before it runs it ([`decipher`]): what the disc
    // holds there is noise; the deciphered code is floating, placed where RAM holds it.
    let ciphered: Vec<(String, FloatingUnit)> = parsed.par_iter().flat_map_iter(|(name, exe, _, far)| deciphered_units(exe, far).into_iter().map(move |u| (format!("{name} deciphered"), u))).collect();
    floating(&mut pack, &unplaced, &ciphered);
    // A captured run keeps only what lies outside the text of every disc image: one image across
    // an executable and the slot its overlays are loaded into would call the overlay resident at
    // capture directly, and run it after another overlay is loaded there.
    let disc: Vec<(u32, u32)> = parsed.iter().map(|(_, x, ..)| (x.text_addr & 0x1FFF_FFFF, (x.text_addr & 0x1FFF_FFFF) + x.text.len() as u32)).collect();
    // Where code ran, with the bytes it ran: a seed of each image holding them there.
    let ran: Vec<(u32, Vec<u8>)> = captured
        .iter()
        .flat_map(|c| c.entries.iter().filter_map(|&e| e.checked_sub(c.at).map(|o| (e, c.bytes[o as usize..(o as usize + 16).min(c.bytes.len())].to_vec()))))
        .collect();
    let captured = captured.into_iter().filter(|c| !c.seeds_only).flat_map(|Captured { at, bytes, ram, entries, .. }| {
        let mut pieces = vec![(at & 0x1FFF_FFFF, (at & 0x1FFF_FFFF) + bytes.len() as u32)];
        for &(s, e) in &disc {
            pieces = pieces.into_iter().flat_map(|(a, b)| [(a, b.min(s)), (a.max(e), b)]).filter(|(a, b)| b > a).collect();
        }
        let seg = at & !0x1FFF_FFFF;
        let base = at & 0x1FFF_FFFF;
        pieces
            .into_iter()
            .filter(|(a, b)| b - a >= MIN_CAPTURED || !entries.is_empty())
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
        let entry_in = |a: u32, b: u32| ran.iter().any(|(e, _)| (a..b).contains(e));
        for (a, b) in pieces.into_iter().filter(|&(a, b)| b - a >= MIN_CAPTURED || (ran_code && entry_in(a, b))) {
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
                parsed.push((name, exe, true, Some(Around::Ram(far.0, far.1.clone()))));
            }
        }
    }
    let discover = |exe: &PsxExe, around: &Option<Around>, seeds: &BTreeSet<u32>| {
        let seeds: Vec<u32> = seeds.iter().copied().collect();
        match around {
            Some(Around::Ram(seg, ram)) => Discovery::run_file(exe, &[(*seg, &ram[..])], &[], &seeds),
            Some(Around::File(at, file)) => Discovery::run_file(exe, &[], &[(*at, &file[..])], &seeds),
            None => Discovery::run_file(exe, &[], &[], &seeds),
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
    let mut ok = Vec::new();
    for (p, size) in parsed.into_iter().zip(sizes) {
        match size {
            Ok(n) => ok.push((p, n, 0)),
            Err(e) => pack.skipped.push((p.name, e)),
        }
    }
    // Texts that share an address are never in RAM together: the levels of a game at one
    // address, the rooms of an overlay slot that each start at their own first function (Silent
    // Hill: 246 runs of SILENT from 800CB370 on). They share a host region, as large as the
    // largest. A group is every text left that holds one address, the last byte of the text
    // that ends first among them (the fewest groups), and is keyed by it. Texts that share no
    // address get disjoint regions, so they can be resident together.
    let span = |x: &PsxExe| (x.text_addr & 0x1FFF_FFFF, (x.text_addr & 0x1FFF_FFFF) + x.text.len().max(1) as u32);
    let mut order: Vec<usize> = (0..ok.len()).collect();
    order.sort_by_key(|&i| span(&ok[i].0.exe).1);
    for &i in &order {
        if ok[i].2 != 0 {
            continue;
        }
        let at = span(&ok[i].0.exe).1 - 1;
        for o in ok.iter_mut().filter(|o| o.2 == 0) {
            let (a, b) = span(&o.0.exe);
            if a <= at && at < b {
                o.2 = at | 0x8000_0000;
            }
        }
    }
    // Group: region size, and whether an executable is in it.
    let mut groups: BTreeMap<u32, (u32, bool)> = BTreeMap::new();
    for (p, n, at) in &ok {
        let g = groups.entry(*at).or_default();
        g.0 = g.0.max(n.next_multiple_of(ALIGN));
        g.1 |= !p.overlay;
    }
    let base_for = |groups: &BTreeMap<u32, (u32, bool)>| {
        let total: u32 = groups.values().map(|g| g.0).sum();
        (total, layout::RAM.checked_sub(total).map(|b| b & !0xFFFF).filter(|&b| b >= CODE_FLOOR))
    };
    while base_for(&groups).1.is_none() {
        let Some((&at, &(size, _))) = groups.iter().filter(|(_, g)| !g.1).max_by_key(|(_, g)| g.0) else { break };
        groups.remove(&at);
        for (p, ..) in ok.iter().filter(|o| o.2 == at) {
            pack.skipped.push((p.name.clone(), format!("overlay at {:#010x}: the {size} B of code of the texts over {at:#010x} do not fit", p.exe.text_addr)));
        }
        ok.retain(|o| o.2 != at);
    }
    if ok.is_empty() {
        pack.floating.clear();
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
    place_floating(&mut pack, base, CODE_FLOOR);
    pack.size = at - pack.base;

    let images: Vec<Result<CodeImage, String>> = ok
        .into_par_iter()
        .map(|(p, len, at)| {
            let host_base = groups[&at].0;
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
                code_words: (0..text_size.div_ceil(4)).map(|w| u8::from(p.disc.code.contains(&(p.exe.text_addr + 4 * w)))).collect(),
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

/// The floating units of the code `exe` deciphers in its text ([`decipher::deciphered`]): the
/// deciphered text discovered from its entry and the entries of the jumps into what was
/// deciphered, and each run of the code it finds in a deciphered span, entered at every word
/// but a delay slot. `far`: the data around the text, as the image's own discovery reads it.
fn deciphered_units(exe: &PsxExe, far: &Option<Around>) -> Vec<FloatingUnit> {
    let Some(d) = decipher::deciphered(exe) else { return Vec::new() };
    let x = PsxExe { text: d.text, ..exe.clone() };
    let disc = match far {
        Some(Around::Ram(seg, ram)) => Discovery::run_file(&x, &[(*seg, &ram[..])], &[], &d.entries),
        Some(Around::File(at, file)) => Discovery::run_file(&x, &[], &[(*at, &file[..])], &d.entries),
        None => Discovery::run_file(&x, &[], &[], &d.entries),
    };
    let mut out: Vec<FloatingUnit> = Vec::new();
    for &(s, e) in &d.spans {
        let mut last = None;
        for &a in disc.code.range(s..e) {
            if last != Some(a.wrapping_sub(4)) {
                out.push(FloatingUnit { words: Vec::new(), entries: Vec::new() });
            }
            let u = out.last_mut().unwrap();
            if !disc.delay_slots.contains_key(&a) {
                u.entries.push(u.words.len() as u32);
            }
            u.words.push(x.word(a).unwrap_or(0));
            last = Some(a);
        }
    }
    out
}

/// [`CodePack::floating`] of the overlays `unplaced` and the units `more`: their units, the same
/// words once (with the entries every file gives them), translated.
fn floating(pack: &mut CodePack, unplaced: &[(String, Vec<u8>)], more: &[(String, FloatingUnit)]) {
    let mut units: Vec<(FloatingUnit, &str)> = unplaced.par_iter().flat_map_iter(|(n, b)| floating_units(b).into_iter().map(move |(_, u)| (u, n.as_str()))).collect();
    units.extend(more.iter().map(|(n, u)| (u.clone(), n.as_str())));
    units.par_sort_unstable_by(|a, b| a.0.words.cmp(&b.0.words).then(a.1.cmp(b.1)));
    let mut uniq: Vec<(FloatingUnit, &str)> = Vec::new();
    for (u, n) in units {
        match uniq.last_mut() {
            Some((v, _)) if v.words == u.words => {
                v.entries.extend(u.entries);
                v.entries.sort_unstable();
                v.entries.dedup();
            }
            _ => uniq.push((u, n)),
        }
    }
    let done: Vec<(Result<FloatingCode, String>, &str)> = uniq.into_par_iter().map(|(u, n)| (FloatingCode::translate(u), n)).collect();
    for (c, n) in done {
        match c {
            Ok(c) => pack.floating.push(c),
            Err(e) => pack.skipped.push((format!("floating code of {n}"), e)),
        }
    }
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

    /// Code an executable deciphers in its text before it jumps in (Spyro 2's LibCrypt check:
    /// XOR with a key that steps `key = c - key`) is a floating unit of the deciphered words,
    /// entered where the jump goes.
    #[test]
    fn code_deciphered_in_the_text_is_floating() {
        const T: u32 = 0x8001_0000;
        let i = |op: u32, rs: u32, rt: u32, imm: u32| op << 26 | rs << 21 | rt << 16 | (imm & 0xFFFF);
        let r = |rs: u32, rt: u32, rd: u32, f: u32| rs << 21 | rt << 16 | rd << 11 | f;
        let (v0, v1, t0, t2, t6, t7) = (2, 3, 8, 10, 14, 15);
        let (key, c, at) = (0x6D24_339Bu32, 0x16D1_6348u32, T + 4 * 18);
        let plain = [ADDIU_V0_1, 0x2442_0002, JR_RA, 0];
        let mut w = vec![
            i(0x0F, 0, t0, at >> 16),
            i(0x09, t0, t0, at),
            i(0x0F, 0, v0, key >> 16),
            i(0x0D, v0, v0, key),
            i(0x0F, 0, v1, c >> 16),
            i(0x0D, v1, v1, c),
            i(0x0F, 0, t6, at >> 16),
            i(0x09, t6, t6, at),
            i(0x08, 0, t7, plain.len() as u32),
            i(0x23, t6, t2, 0),
            i(0x08, t7, t7, 0xFFFF),
            r(t2, v0, t2, 0x26),
            i(0x2B, t6, t2, 0),
            i(0x08, t6, t6, 4),
            i(0x05, t7, 0, 0xFFFA),
            r(v1, v0, v0, 0x22),
            r(t0, 0, 0, 8),
            0,
        ];
        let mut k = key;
        for p in plain {
            w.push(p ^ k);
            k = c.wrapping_sub(k);
        }
        let pack = build(vec![("SCES_000.00".into(), exe(T, &w))], vec![], vec![]).unwrap();
        let units: Vec<&FloatingUnit> = pack.floating.iter().map(|f| &f.unit).collect();
        assert_eq!(units.len(), 1, "{units:?}");
        assert_eq!(units[0].words, plain);
        assert_eq!(units[0].entries[0], 0);
    }

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
    fn texts_that_share_an_address_share_a_host_region() {
        // Silent Hill's rooms: one slot, each text from its own first function. R1 and R2 hold
        // 800CB378, R3 overlaps R2 only: a group of its own. M shares nothing.
        let pack = build(
            vec![
                ("R1.EXE".into(), exe(0x800C_B370, &[JR_RA, 0, JR_RA, 0])),
                ("R2.EXE".into(), exe(0x800C_B378, &[ADDIU_V0_1, JR_RA, 0])),
                ("R3.EXE".into(), exe(0x800C_B380, &[JR_RA, 0])),
                ("M.EXE".into(), exe(0x8001_0000, &[JR_RA, 0])),
            ],
            vec![],
            vec![],
        )
        .unwrap();
        let [m, r1, r2, r3] = &pack.images[..] else { panic!("{} images", pack.images.len()) };
        assert_eq!((m.name.as_str(), r1.name.as_str(), r3.name.as_str()), ("M.EXE", "R1.EXE", "R3.EXE"));
        assert_eq!(r1.host_base, r2.host_base);
        let region = |i: &CodeImage| i.host_base..i.host_base + i.code.len() as u32;
        let apart = |a: &CodeImage, b: &CodeImage| region(a).end <= region(b).start || region(b).end <= region(a).start;
        assert!(apart(m, r1) && apart(m, r2) && apart(m, r3) && apart(r1, r3) && apart(r2, r3));
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
        want.extend([0, JR_RA]); // in the next block: not patchable, the word as translated
        want.extend([1 | (h(0x8001_1004).len() as u32) << 8]); // the jr slot
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
            vec![Overlay::whole("LEVEL.OVL", overlay(0x800A_0000)), Overlay::whole("LEVEL.DAT", vec![0x5A; 0x800])],
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
        Captured { name: format!("RAM {at:08X}"), at, bytes, ram: Arc::new(ram), entries: Vec::new(), seeds_only: false }
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
        // A piece under MIN_CAPTURED is taken when it ran (Crash 2's 8016F4B4, 0x130 bytes).
        let small = |entries: Vec<u32>| build(vec![], vec![], vec![Captured { entries, ..cap(at, body[..0x40].to_vec()) }]).unwrap().images;
        assert!(small(vec![]).is_empty());
        assert_eq!(small(vec![at + 0x10]).len(), 1);
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

    /// Crash 2's GOOL native pieces between bytecode words (the island test of discovery): each
    /// run of code is a unit at its offset, and discovered on its own it is all code again.
    #[test]
    fn floating_units_are_the_runs_of_code_of_a_file() {
        const NOP: u32 = 0;
        let (s0, s5, s6, v0, a0, sp, ra) = (16, 21, 22, 2, 4, 29, 31);
        let i = |op: u32, rs: u32, rt: u32, imm: u32| op << 26 | rs << 21 | rt << 16 | imm;
        let jr_ra = JR_RA;
        let jalr_s5_ra = ra << 21 | s5 << 11 | 9;
        let w = [
            jr_ra,
            NOP,
            0x1C30_184E, // bytecode
            0x49BE_0BE0,
            NOP,
            i(0x23, s6, v0, 0xFFFC),
            i(9, s6, s6, 0xFFFC),
            i(0x2B, s0, v0, 372),
            i(0x2B, s0, s6, 188),
            jalr_s5_ra,
            NOP,
            0x1C30_1851, // bytecode
            0x49BE_0BE0,
            i(0x23, sp, a0, 0),
            jr_ra,
            NOP,
            0x49BE_0BE0, // bytecode
            i(0x23, s0, v0, 352),
            NOP,
            jalr_s5_ra,
            NOP,
        ];
        let bytes: Vec<u8> = w.iter().flat_map(|w| w.to_le_bytes()).collect();
        let units = floating_units(&bytes);
        let at: Vec<(usize, usize)> = units.iter().map(|(o, u)| (*o, u.words.len())).collect();
        assert_eq!(at, [(0, 2), (16, 7), (68, 4)]);
        for (o, u) in &units {
            assert_eq!(u.words, w[o / 4..o / 4 + u.words.len()]);
            assert_eq!(u.entries[0], 0);
            let (_, d) = u.discover();
            assert_eq!(d.code.len(), u.words.len(), "unit at {o}");
        }
    }

    /// Zeros no instruction falls into, two or more, are padding: the file's code reaches them
    /// (Spider-Man's CD.WAD modules follow each other with zeros), RAM holds other data there, so
    /// units are cut there and the padding is units of its own. A single `nop` and a delay slot
    /// stay.
    #[test]
    fn floating_units_cut_at_padding() {
        const NOP: u32 = 0;
        let addiu_v0 = |n: u32| 9 << 26 | 2 << 16 | n;
        let w = [NOP, NOP, NOP, addiu_v0(1), JR_RA, NOP, NOP, NOP, addiu_v0(2), JR_RA, NOP, NOP, addiu_v0(3), JR_RA, NOP];
        let bytes: Vec<u8> = w.iter().flat_map(|w| w.to_le_bytes()).collect();
        let exe = PsxExe { pc0: FLOATING_BASE, gp0: 0, text_addr: FLOATING_BASE, text: bytes.clone(), bss: (0, 0), stack: (0, 0) };
        let d = Discovery::run_file(&exe, &[], &[], &[]);
        assert!((0..3).all(|k| d.code.contains(&(FLOATING_BASE + 4 * k))), "the file's code reaches the zeros at its start");
        let at: Vec<(usize, usize)> = floating_units(&bytes).iter().map(|(o, u)| (*o, u.words.len())).collect();
        assert_eq!(at, [(0, 3), (12, 3), (32, 3), (48, 3)]);
    }

    /// Padding an island starts at is units of `nop`s, FLOATING_WALK words at most each: Crash 2
    /// enters GOOL native code at four `nop`s after an op word (8014F7E4 in its first level).
    #[test]
    fn floating_units_keep_the_nops_an_island_starts_at() {
        const NOP: u32 = 0;
        let addiu_v0 = |n: u32| 9 << 26 | 2 << 16 | n;
        let op = 0x49BE_0BE0; // A GOOL op word: COP2 with no R3000 meaning.
        let mut w = vec![op];
        w.extend([NOP; 10]);
        w.extend([addiu_v0(1), addiu_v0(2), addiu_v0(3), addiu_v0(4), JR_RA, NOP]);
        let bytes: Vec<u8> = w.iter().flat_map(|w| w.to_le_bytes()).collect();
        let units = floating_units(&bytes);
        let at: Vec<(usize, usize)> = units.iter().map(|(o, u)| (*o, u.words.len())).collect();
        assert_eq!(at, [(4, 8), (36, 2), (44, 6)]);
        assert!(units[..2].iter().all(|(_, u)| u.words.iter().all(|&w| w == NOP) && u.entries.len() == u.words.len()));
    }

    /// Position-independent code with every kind of relocation floating code has: relative
    /// branches and a `bal` inside it, `jal`/`j` to the game, a `jalr` and a `bal` past its end.
    fn floating_program() -> Vec<u32> {
        const NOP: u32 = 0;
        let (t0, t2, t3, s5, a0, sp, ra) = (8, 10, 11, 21, 4, 29, 31);
        let i = |op: u32, rs: u32, rt: u32, imm: i32| op << 26 | rs << 21 | rt << 16 | (imm as u32 & 0xFFFF);
        let b = |op: u32, rs: u32, rt: u32, from: i32, to: i32| i(op, rs, rt, to - from - 1);
        let bal = |from: i32, to: i32| i(1, 0, 0x11, to - from - 1);
        let j = |op: u32, a: u32| op << 26 | (a >> 2) & 0x03FF_FFFF;
        vec![
            i(9, sp, sp, -8),
            i(0x2B, sp, ra, 4),
            i(0x23, a0, t2, 0),
            bal(3, 12),
            NOP,
            i(9, 0, t0, 3),
            i(9, t0, t0, -1),
            b(5, t0, 0, 7, 6),
            NOP,
            j(3, 0x8003_0000),
            NOP,
            b(4, t2, 0, 11, 40),
            NOP,
            t3 << 21 | s5 << 11 | 9,
            NOP,
            bal(15, 60),
            NOP,
            i(0x23, sp, ra, 4),
            i(9, sp, sp, 8),
            JR_RA,
            NOP,
            j(2, 0x8004_0000),
            NOP,
        ]
    }

    /// The runtime's move of a unit (floating.c `fl_place`, through the sim) is the
    /// translation's own ([`Translation::moved`]), word for word, at any guest and host address.
    #[test]
    fn the_runtime_moves_floating_code_as_the_translation_does() {
        let unit = FloatingUnit { words: floating_program(), entries: vec![0] };
        let code = FloatingCode::translate(unit.clone()).unwrap();
        let kinds: std::collections::BTreeSet<u32> = code.relocs.iter().map(|r| r & 3).collect();
        assert!(kinds.contains(&0) && kinds.contains(&1) && kinds.contains(&2), "{kinds:?}");
        let (exe, d) = unit.discover();
        let t = translate_floating(&exe, &d, &Hazards::check(&exe, &d), FLOATING_HOST).unwrap();
        assert_eq!(t.words, code.code);
        let blob = head(std::slice::from_ref(&code));
        let body = code.body();
        for (guest, host) in [(FLOATING_BASE, FLOATING_HOST), (0x8012_3450, 0x08B4_5670), (0x801F_F000, 0x0960_0000), (0x8000_1000, 0x0880_0400)] {
            let c = psx2psp_sim::floating_place(&blob, &body, 0, guest, host, None).unwrap();
            assert_eq!(c, t.moved(guest.wrapping_sub(FLOATING_BASE), host).words, "{guest:08X} {host:08X}");
            let same = psx2psp_sim::floating_place(&blob, &body, 0, guest, host, Some(&unit.words)).unwrap();
            assert_eq!(same, c, "RAM as the unit's");
        }
        assert_eq!(psx2psp_sim::floating_place(&blob, &body, 1, FLOATING_BASE, FLOATING_HOST, None), None, "no unit 1");
        assert_eq!(psx2psp_sim::floating_place(&blob[..blob.len() - 1], &body, 0, FLOATING_BASE, FLOATING_HOST, None), None, "a short head");
        assert_eq!(psx2psp_sim::floating_place(&blob, &body[1..], 0, FLOATING_BASE, FLOATING_HOST, None), None, "a short body");
        assert_eq!(psx2psp_sim::floating_place(&blob, &body, 0, FLOATING_BASE, FLOATING_HOST, Some(&unit.words[1..])), None, "short RAM");
        let n = code.code.len();
        let gone = (0..unit.words.len()).find(|&j| code.table[j] != u32::MAX).unwrap();
        let mut bad = body.clone();
        bad[n + gone] = u32::MAX;
        assert_eq!(psx2psp_sim::floating_place(&blob, &bad, 0, FLOATING_BASE, FLOATING_HOST, None), None, "a table that is not the map");
        bad[n + gone] = n as u32;
        assert_eq!(psx2psp_sim::floating_place(&blob, &bad, 0, FLOATING_BASE, FLOATING_HOST, None), None, "a table out of the code");
    }

    /// A game that links floating code where it loads it (Spider-Man's CD.WAD modules) rewrites
    /// its jal/j targets, immediates and load or store offsets: the runtime finds the unit in RAM
    /// all the same and places it with RAM's words, as the translation of the linked words would
    /// be; RAM that differs anywhere else (a register, an opcode) is not the unit.
    #[test]
    fn the_runtime_takes_linked_floating_code_from_ram() {
        let unit = FloatingUnit { words: floating_program(), entries: vec![0] };
        let code = FloatingCode::translate(unit.clone()).unwrap();
        let kinds: BTreeSet<u32> = code.sites.chunks(2).map(|s| s[0] & 1).collect();
        assert_eq!(kinds, BTreeSet::from([0, 1]), "{:?}", code.sites);
        let blob = head(std::slice::from_ref(&code));
        let body = code.body();
        let mut linked = unit.words.clone();
        linked[5] = linked[5] & 0xFFFF_0000 | 7; // addiu t0, zero, 7
        linked[2] = linked[2] & 0xFFFF_0000 | 0x7FF0; // lw t2, %lo(0x7FF0)(a0)
        let jump = |op: u32, t: u32| op << 26 | (t >> 2 & 0x03FF_FFFF);
        linked[9] = jump(3, 0x8005_1230);
        let (exe, d) = FloatingUnit { words: linked.clone(), entries: vec![0] }.discover();
        let t = translate_floating(&exe, &d, &Hazards::check(&exe, &d), FLOATING_HOST).unwrap();
        for (guest, host) in [(FLOATING_BASE, FLOATING_HOST), (0x8012_3450, 0x08B4_5670)] {
            let c = psx2psp_sim::floating_place(&blob, &body, 0, guest, host, Some(&linked)).unwrap();
            assert_eq!(c, t.moved(guest.wrapping_sub(FLOATING_BASE), host).words, "{guest:08X} {host:08X}");
        }
        let mut ram = vec![0u8; psx2psp_sim::RAM];
        let at = 0x0004_5670;
        let mut put = |w: &[u32]| {
            for (k, x) in w.iter().enumerate() {
                ram[at + 4 * k..at + 4 * k + 4].copy_from_slice(&x.to_le_bytes());
            }
            psx2psp_sim::floating_find(&blob, &ram, at as u32).unwrap()
        };
        assert_eq!(put(&linked), Some((0, at as u32)), "linked");
        linked[2] ^= 1 << 16; // lw t3
        assert_eq!(put(&linked), None, "a load's register differs");
        linked[2] ^= 1 << 16;
        linked[9] = jump(2, 0x8005_1230); // j for jal
        assert_eq!(put(&linked), None, "another opcode");
    }

    /// The head ([`floating_blob`]) of floating code, with no bodies (the simulator's).
    fn head(codes: &[FloatingCode]) -> Vec<u32> {
        floating_blob(codes, &vec![(0, 0); codes.len()])
    }

    /// The head holds what [`FloatingCode::blob_words`] counts, and a pack with floating code
    /// lays out blob, arena and images upwards, over [`CODE_FLOOR`].
    #[test]
    fn floating_code_is_laid_out_below_the_images() {
        let code = FloatingCode::translate(FloatingUnit { words: floating_program(), entries: vec![0] }).unwrap();
        let mut two = code.clone();
        two.unit.words[0] ^= 1;
        assert_eq!(head(&[code.clone(), two.clone()]).len(), 4 + code.blob_words() + two.blob_words());
        let bytes: Vec<u8> = floating_program().iter().flat_map(|w| w.to_le_bytes()).collect();
        let pack = build(vec![("MAIN.EXE".into(), exe(0x8001_0000, &[ADDIU_V0_1, JR_RA, 0]))], vec![Overlay { name: "GOOL.BIN".into(), bytes, file: None }], vec![]).unwrap();
        assert_eq!(pack.floating.len(), 1, "{:?}", pack.skipped);
        let blob = 4 * head(&pack.floating).len() as u32;
        assert!(pack.blob_host >= CODE_FLOOR && pack.blob_host + blob <= pack.base, "{:08X} {blob} {:08X}", pack.blob_host, pack.base);
        assert!(u64::from(pack.arena) >= pack.floating[0].slot_bytes());
        assert_eq!(pack.images[0].host_base, pack.base + pack.arena);
    }

    /// Floating code that does not fit below the images goes to the user partition: head and
    /// arena out of the code's range (`blob_host` 0), every unit kept, the images where they
    /// were; with the room, below them again.
    #[test]
    fn floating_code_that_does_not_fit_below_goes_to_the_partition() {
        let code = FloatingCode::translate(FloatingUnit { words: floating_program(), entries: vec![0] }).unwrap();
        let mut two = code.clone();
        two.unit.words[0] ^= 1;
        let base = 0x0900_0000;
        let mut pack = CodePack { floating: vec![code.clone(), two], ..CodePack::default() };
        place_floating(&mut pack, base, base - 0x1_0000);
        assert_eq!((pack.floating.len(), pack.blob_host, pack.base), (2, 0, base), "{:?}", pack.skipped);
        assert!(pack.skipped.is_empty(), "{:?}", pack.skipped);
        assert!(u64::from(pack.arena) >= 2 * code.slot_bytes() && pack.arena % 0x1_0000 == 0, "{}", pack.arena);
        place_floating(&mut pack, base, base - 0x4_0000);
        assert_eq!(pack.floating.len(), 2);
        assert!(pack.blob_host >= base - 0x4_0000 && pack.base == base - pack.arena && pack.blob_host < pack.base, "{:08X} {:08X}", pack.blob_host, pack.base);
    }

    /// DATA.PSAR holds each unit's body where its head says, deflated or as it is (when deflate
    /// is not smaller): read back, it is the body the runtime places.
    #[test]
    fn the_head_says_where_each_body_is() {
        let code = FloatingCode::translate(FloatingUnit { words: floating_program(), entries: vec![0] }).unwrap();
        let mut two = code.clone();
        two.unit.words[0] ^= 1;
        let codes = [code, two];
        let deflate = |r: &[u8]| {
            let z = miniz_oxide::deflate::compress_to_vec(r, 6);
            if z.len() < r.len() { z } else { r.to_vec() }
        };
        for (bytes, at) in [floating_bodies(&codes, |r| r.to_vec()), floating_bodies(&codes, deflate)] {
            let blob = floating_blob(&codes, &at);
            for (u, c) in codes.iter().enumerate() {
                let x = &blob[4 + FLOATING_UNIT_WORDS * u..][..FLOATING_UNIT_WORDS];
                let (off, len, raw) = (x[7] as usize, x[8] as usize, 4 * (x[5] + x[1] + x[6]) as usize);
                let z = &bytes[off..off + len];
                let body = if len == raw { z.to_vec() } else { miniz_oxide::inflate::decompress_to_vec(z).unwrap() };
                assert_eq!(body, c.body().iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>(), "unit {u}");
            }
        }
    }

    /// The runtime finds a unit in RAM from any word it translates, at the place RAM holds it,
    /// and not where a word of it differs.
    #[test]
    fn the_runtime_finds_floating_code_by_its_words() {
        let unit = FloatingUnit { words: floating_program(), entries: vec![0] };
        let code = FloatingCode::translate(unit.clone()).unwrap();
        let blob = head(std::slice::from_ref(&code));
        let mut ram = vec![0u8; psx2psp_sim::RAM];
        let at = 0x0001_2340;
        for (k, w) in unit.words.iter().enumerate() {
            ram[at + 4 * k..at + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
        }
        for e in 0..unit.words.len() {
            let found = psx2psp_sim::floating_find(&blob, &ram, (at + 4 * e) as u32).unwrap();
            assert_eq!(found, (code.table[e] != u32::MAX).then_some((0, at as u32)), "word {e}");
        }
        assert_eq!(psx2psp_sim::floating_find(&blob, &ram, at as u32 + 2), Ok(None), "unaligned");
        ram[at + 4 * 2 + 2] ^= 1;
        assert_eq!(psx2psp_sim::floating_find(&blob, &ram, at as u32), Ok(None), "a word differs");
        assert_eq!(psx2psp_sim::floating_find(&blob, &ram[1..], at as u32), Err(()));
    }
}
