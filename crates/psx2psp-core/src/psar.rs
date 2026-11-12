//! DATA.PSAR: `PSISOIMG0000` (single disc) and `PSTITLEIMG000000` (multi-disc) writer/reader.
//!
//! Byte layout cross-checked against PSXPackager, popstationr and pop-fe (see `docs/FORMAT.md`).
//!
//! ```text
//! PSISOIMG (ISO+x)   0x000 magic · 0x00C p1 · 0x400 "_SLUS_00594" · 0x800 TOC (10-byte BCD entries)
//!                    0xBFC 0x00100000 · 0x1220 p2 · 0x1228 0x7FF · 0x122C title[128] · 0x12AC 3
//!                    0x4000 index[N] {u32 off (rel. ISO+0x100000), u32 len, 24×0}
//!                    0x100000 raw-deflate blocks of 0x9300 (16 raw sectors); len==0x9300 ⇒ stored
//! STARTDAT           "STARTDAT" 1 1 0x50 png_len … | boot PNG | PGD trailer (donor)
//! ```
//!
//! Blocks are compressed in parallel (rayon) and written sequentially — the image is streamed, never
//! fully buffered, and the source files are never modified (patches apply to in-memory blocks).

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use rayon::prelude::*;

use crate::disc::{DiscImage, LEAD_IN_FRAMES, RAW_SECTOR, TrackKind};
use crate::error::{Error, Result};
use crate::pbp::{PbpFile, Section};
use crate::{ImagePatch, Monitor};

pub const BLOCK: usize = 0x9300;
pub const ISO_DATA: u64 = 0x10_0000;
pub const MAX_DISCS: usize = 5;
const INDEX_AT: usize = 0x4000;
const INDEX_CAP: usize = (ISO_DATA as usize - INDEX_AT) / 32;
const TOC_AT: usize = 0x800;
const TOC_MAX: usize = 0x3FC;
const STARTDAT_HDR: usize = 0x50;
const TITLEIMG_HDR: u64 = 0x400;
const TITLEIMG_DISC_ALIGN: u64 = 0x8000;
/// Blocks compressed per parallel batch (64 × 37.6 KB ≈ 2.4 MB in flight).
const BATCH: usize = 64;

pub const MAGIC_ISO: &[u8; 12] = b"PSISOIMG0000";
pub const MAGIC_TITLE: &[u8; 16] = b"PSTITLEIMG000000";

// ───────────────────────────── donor (opaque Sony blobs) ─────────────────────────────

/// Opaque, copyrighted Sony data we cannot generate and do not ship: lifted at runtime from a
/// user-supplied PS1 Classic EBOOT (e.g. `BASE.PBP`).
#[derive(Debug, Clone)]
pub struct Donor {
    /// Loader module (`~PSP` signed module or DarkAlex ELF).
    pub data_psp: Vec<u8>,
    /// Default boot splash (STARTDAT PNG), 480×272.
    pub boot_png: Vec<u8>,
    /// Encrypted `\0PGD` trailer that follows the STARTDAT PNG.
    pub pgd: Vec<u8>,
    /// PSTITLEIMG constants (only present if the donor itself is multi-disc).
    pub title_consts: Option<TitleConsts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleConsts {
    pub unk18: [u8; 16],
    pub data3: [u8; 128],
}

impl TitleConsts {
    /// The constants popstation writes into every PSTITLEIMG header (`popstation.cpp` words at
    /// +0x18 and `data3[]` at +0x28C). Used when the donor is single-disc and so carries none.
    pub const POPSTATION: Self = Self {
        unk18: *b"\xBC\xC5\xC9\x2C\x0F\xA9\xB5\x33\xB3\xB4\xF6\x06\xBA\x45\x59\xB2",
        data3: [
            0x8C, 0x0E, 0xAE, 0x9D, 0x39, 0x8C, 0xFE, 0x24, 0x65, 0x21, 0xAD, 0x2D, 0x65, 0xA9, 0x61, 0xDF, //
            0xD4, 0x4A, 0x13, 0x03, 0xB4, 0xCD, 0x32, 0x5F, 0xBD, 0xB0, 0xF4, 0xF9, 0x8A, 0x70, 0xBE, 0x1E, //
            0x39, 0x2C, 0x7D, 0xC0, 0xC1, 0xC6, 0x6B, 0x81, 0xAA, 0x3C, 0x06, 0x53, 0x94, 0x1B, 0xCE, 0xE5, //
            0x44, 0x16, 0xCF, 0xDB, 0xB1, 0xE3, 0x89, 0x7B, 0xA2, 0xD2, 0xE7, 0xD6, 0xC1, 0x26, 0x6B, 0x58, //
            0x8D, 0x2C, 0xE6, 0xC3, 0x15, 0x97, 0xD0, 0x29, 0xC9, 0x16, 0x81, 0xB6, 0xCC, 0x42, 0xEE, 0x0C, //
            0x28, 0x10, 0xEA, 0xF2, 0x6B, 0x6F, 0x90, 0x30, 0x05, 0xBE, 0x4A, 0x2F, 0x4A, 0xBC, 0xDC, 0xE5, //
            0x87, 0xCE, 0x19, 0xB9, 0x80, 0xDE, 0xB8, 0x32, 0xDD, 0xAD, 0x89, 0x67, 0xE0, 0x92, 0x78, 0x89, //
            0xE8, 0xDC, 0x45, 0x1D, 0x0C, 0xBE, 0x8B, 0x99, 0x4D, 0x50, 0xB3, 0xD6, 0x58, 0x96, 0x61, 0x75,
        ],
    };
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Real size of a DATA.PSP module: `~PSP` header field +0x2C, or the extent of an ELF32's
/// section/program header tables and segments. `None` if unknown (keep everything).
fn module_size(b: &[u8]) -> Option<usize> {
    let n = if b.starts_with(b"~PSP") && b.len() >= 0x30 {
        rd32(b, 0x2C) as usize
    } else if b.starts_with(b"\x7FELF") && b.len() >= 0x34 && b[4] == 1 {
        let rd16 = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
        let (phoff, shoff) = (rd32(b, 0x1C) as usize, rd32(b, 0x20) as usize);
        let (phentsize, phnum, shentsize, shnum) = (rd16(0x2A), rd16(0x2C), rd16(0x2E), rd16(0x30));
        let mut end = (shoff + shentsize * shnum).max(phoff + phentsize * phnum).max(0x34);
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            if ph + 0x14 <= b.len() {
                end = end.max(rd32(b, ph + 4) as usize + rd32(b, ph + 0x10) as usize);
            }
        }
        end
    } else {
        return None;
    };
    (n > 0 && n <= b.len()).then_some(n)
}

fn wr32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

impl Donor {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let f = std::fs::File::open(path).map_err(Error::io_path(path))?;
        Self::from_pbp(&mut PbpFile::open(io::BufReader::new(f))?)
    }

    pub fn from_pbp<R: Read + Seek>(pbp: &mut PbpFile<R>) -> Result<Self> {
        let mut data_psp = pbp.read(Section::DataPsp)?;
        // The section range also covers the zero padding up to the aligned PSAR: trim to the real size.
        if let Some(n) = module_size(&data_psp) {
            data_psp.truncate(n);
        }
        if data_psp.is_empty() {
            return Err(Error::format("donor PBP", "DATA.PSP is empty"));
        }

        let psar = pbp.range(Section::DataPsar);
        let r = pbp.inner_mut();
        let mut head = [0u8; TITLEIMG_HDR as usize];
        r.seek(SeekFrom::Start(psar.start))?;
        r.read_exact(&mut head).map_err(|_| Error::format("donor PBP", "DATA.PSAR too short"))?;
        let (p1, title_consts) = if head.starts_with(MAGIC_TITLE) {
            let consts = TitleConsts {
                unk18: head[0x18..0x28].try_into().unwrap(),
                data3: head[0x28C..0x30C].try_into().unwrap(),
            };
            (rd32(&head, 0x10) as u64, Some(consts))
        } else if head.starts_with(MAGIC_ISO) {
            (rd32(&head, 0x0C) as u64, None)
        } else {
            return Err(Error::format("donor PBP", "DATA.PSAR is not PSISOIMG/PSTITLEIMG (not a PS1 Classic)"));
        };

        let sd_start = psar.start + p1;
        if sd_start + STARTDAT_HDR as u64 > psar.end {
            return Err(Error::format("donor PBP", "STARTDAT offset out of range"));
        }
        let mut sd = vec![0u8; (psar.end - sd_start) as usize];
        r.seek(SeekFrom::Start(sd_start))?;
        r.read_exact(&mut sd)?;
        if !sd.starts_with(b"STARTDAT") {
            return Err(Error::format("donor PBP", "no STARTDAT section"));
        }
        let hs = rd32(&sd, 0x10) as usize;
        let png_len = rd32(&sd, 0x14) as usize;
        let png = sd.get(hs..hs + png_len).ok_or_else(|| Error::format("donor PBP", "STARTDAT PNG truncated"))?;
        let pgd = sd[hs + png_len..].to_vec();
        if !pgd.starts_with(b"\0PGD") {
            return Err(Error::format("donor PBP", "STARTDAT trailer is not a PGD block"));
        }
        Ok(Self { data_psp, boot_png: png.to_vec(), pgd, title_consts })
    }
}

// ───────────────────────────────────── TOC ─────────────────────────────────────

fn bcd(v: u32) -> u8 {
    debug_assert!(v < 100);
    ((v / 10) << 4 | (v % 10)) as u8
}

fn from_bcd(b: u8) -> u32 {
    (b >> 4) as u32 * 10 + (b & 0xF) as u32
}

fn msf(frames: u32) -> [u8; 3] {
    [bcd(frames / 75 / 60), bcd(frames / 75 % 60), bcd(frames % 75)]
}

fn ctrl(kind: TrackKind) -> u8 {
    if kind.is_data() { 0x41 } else { 0x01 }
}

/// Builds the POPS TOC: A0 (first track, disc type 0x20 = CD-XA, deliberately non-BCD),
/// A1 (last track), A2 (lead-out, +2 s), then one entry per track.
pub fn build_toc(disc: &DiscImage) -> Vec<u8> {
    let first = &disc.tracks[0];
    let last = disc.tracks.last().unwrap();
    let mut t = Vec::with_capacity(10 * (3 + disc.tracks.len()));
    t.extend([ctrl(first.kind), 0, 0xA0, 0, 0, 0, 0, bcd(first.number as u32), 0x20, 0]);
    t.extend([ctrl(last.kind), 0, 0xA1, 0, 0, 0, 0, bcd(last.number as u32), 0, 0]);
    let lo = msf(disc.sectors() + LEAD_IN_FRAMES);
    t.extend([ctrl(last.kind), 0, 0xA2, 0, 0, 0, 0, lo[0], lo[1], lo[2]]);
    for tr in &disc.tracks {
        let a = msf(tr.start);
        let p = msf(tr.start + LEAD_IN_FRAMES);
        t.extend([ctrl(tr.kind), 0, bcd(tr.number as u32), a[0], a[1], a[2], 0, p[0], p[1], p[2]]);
    }
    t.truncate(TOC_MAX);
    t
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocTrack {
    pub number: u8,
    pub kind: TrackKind,
    pub start: u32,
}

/// Parses a PSISOIMG TOC → (tracks, lead-out LBA).
pub fn parse_toc(toc: &[u8]) -> (Vec<TocTrack>, Option<u32>) {
    let mut tracks = Vec::new();
    let mut leadout = None;
    for e in toc.chunks_exact(10) {
        if e.iter().all(|&b| b == 0) {
            break;
        }
        let p = from_bcd(e[7]) * 60 * 75 + from_bcd(e[8]) * 75 + from_bcd(e[9]);
        match e[2] {
            0xA0 | 0xA1 => {}
            0xA2 => leadout = p.checked_sub(LEAD_IN_FRAMES),
            n if (1..=0x99).contains(&n) => {
                let kind = if e[0] & 0x40 != 0 { TrackKind::Mode2 } else { TrackKind::Audio };
                if let Some(start) = p.checked_sub(LEAD_IN_FRAMES) {
                    tracks.push(TocTrack { number: from_bcd(n) as u8, kind, start });
                }
            }
            _ => {}
        }
    }
    (tracks, leadout)
}

/// CUE sheet for an extracted single-BIN image.
pub fn toc_to_cue(bin_name: &str, tracks: &[TocTrack]) -> String {
    let mut s = format!("FILE \"{bin_name}\" BINARY\n");
    for t in tracks {
        let mode = if t.kind.is_data() { "MODE2/2352" } else { "AUDIO" };
        let (m, sec, f) = (t.start / 75 / 60, t.start / 75 % 60, t.start % 75);
        s += &format!("  TRACK {:02} {mode}\n    INDEX 01 {m:02}:{sec:02}:{f:02}\n", t.number);
    }
    s
}

// ───────────────────────────────────── writer ─────────────────────────────────────

pub struct DiscSpec<'a> {
    pub image: &'a DiscImage,
    /// 9-character product code, e.g. `SLUS00594`.
    pub disc_id: String,
    pub title: String,
    /// Applied in order to each block before compression.
    pub patches: Vec<&'a dyn ImagePatch>,
}

pub struct PsarJob<'a> {
    pub discs: Vec<DiscSpec<'a>>,
    /// Game title for the PSTITLEIMG header (multi-disc).
    pub title: String,
    /// Deflate level, clamped to 1..=9 (fully-uncompressed EBOOTs are reported not to boot).
    pub level: u32,
    pub boot_png: &'a [u8],
    pub donor: &'a Donor,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PsarStats {
    pub raw_bytes: u64,
    pub stored_bytes: u64,
    pub blocks: u64,
}

fn validate_id(id: &str) -> Result<()> {
    let ok = id.len() == 9
        && id.as_bytes()[..4].iter().all(u8::is_ascii_alphabetic)
        && id.as_bytes()[4..].iter().all(u8::is_ascii_digit);
    if ok { Ok(()) } else { Err(Error::format("disc ID", format!("`{id}` must be 4 letters + 5 digits, e.g. SLUS00594"))) }
}

fn put_title(buf: &mut [u8], title: &str) {
    buf.fill(0);
    let mut end = title.len().min(buf.len() - 1);
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    buf[..end].copy_from_slice(&title.as_bytes()[..end]);
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn compress_block(block: &[u8], level: Compression) -> Option<Vec<u8>> {
    let mut e = DeflateEncoder::new(Vec::with_capacity(BLOCK / 2), level);
    e.write_all(block).ok()?;
    let v = e.finish().ok()?;
    (v.len() < BLOCK).then_some(v)
}

struct DiscHeader<'a> {
    spec: &'a DiscSpec<'a>,
    toc: Vec<u8>,
    index: BlockIndex,
    p1: u32,
    p2: u32,
}

impl DiscHeader<'_> {
    fn bytes(&self) -> Vec<u8> {
        let mut h = vec![0u8; ISO_DATA as usize];
        h[..12].copy_from_slice(MAGIC_ISO);
        wr32(&mut h, 0x0C, self.p1);
        let id = self.spec.disc_id.as_bytes();
        h[0x400] = b'_';
        h[0x401..0x405].copy_from_slice(&id[..4]);
        h[0x405] = b'_';
        h[0x406..0x40B].copy_from_slice(&id[4..9]);
        h[TOC_AT..TOC_AT + self.toc.len()].copy_from_slice(&self.toc);
        wr32(&mut h, 0xBFC, ISO_DATA as u32);
        wr32(&mut h, 0x1220, self.p2);
        wr32(&mut h, 0x1228, 0x7FF);
        put_title(&mut h[0x122C..0x12AC], &self.spec.title);
        wr32(&mut h, 0x12AC, 3);
        for (i, &(off, len)) in self.index.iter().enumerate() {
            let e = INDEX_AT + i * 32;
            wr32(&mut h, e, off);
            wr32(&mut h, e + 4, len);
        }
        h
    }
}

/// Per-block (offset relative to ISO_DATA, stored length) table of a PSISOIMG.
pub type BlockIndex = Vec<(u32, u32)>;

/// Streams one disc (header placeholder + compressed blocks). Returns (index, bytes written
/// after the 0x100000 header, stats). The header itself is written by the caller once p1/p2 are known.
fn stream_disc<W: Write + Seek>(
    w: &mut W,
    spec: &DiscSpec<'_>,
    level: Compression,
    monitor: &dyn Monitor,
    done_before: u64,
    total: u64,
) -> Result<(BlockIndex, u64, PsarStats)> {
    let size = spec.image.size_bytes();
    let nblocks = size.div_ceil(BLOCK as u64) as usize;
    if nblocks > INDEX_CAP {
        return Err(Error::Unsupported(format!("disc of {size} bytes exceeds the PSISOIMG index capacity")));
    }
    w.write_all(&vec![0u8; ISO_DATA as usize])?;

    let mut rd = spec.image.reader();
    let mut index = Vec::with_capacity(nblocks);
    let mut pos: u64 = 0;
    let mut stats = PsarStats::default();
    let mut batch = vec![0u8; BATCH * BLOCK];

    for first in (0..nblocks).step_by(BATCH) {
        if monitor.cancelled() {
            return Err(Error::Cancelled);
        }
        let count = BATCH.min(nblocks - first);
        let buf = &mut batch[..count * BLOCK];
        let got = read_full(&mut rd, buf)?;
        buf[got..].fill(0); // last block zero-padded to 0x9300
        let base = first as u64 * BLOCK as u64;

        let out: Vec<Option<Vec<u8>>> = buf
            .par_chunks_mut(BLOCK)
            .enumerate()
            .map(|(i, blk)| {
                let off = base + (i * BLOCK) as u64;
                for p in &spec.patches {
                    p.apply(off, blk);
                }
                compress_block(blk, level)
            })
            .collect();

        for (i, c) in out.iter().enumerate() {
            let data: &[u8] = c.as_deref().unwrap_or(&buf[i * BLOCK..(i + 1) * BLOCK]);
            w.write_all(data)?;
            let off = u32::try_from(pos).map_err(|_| Error::Unsupported("disc data exceeds 4 GiB".into()))?;
            index.push((off, data.len() as u32));
            pos += data.len() as u64;
        }
        stats.blocks += count as u64;
        stats.raw_bytes = (stats.raw_bytes + (count * BLOCK) as u64).min(size);
        monitor.progress(done_before + stats.raw_bytes, total);
    }
    stats.stored_bytes = pos;
    Ok((index, pos, stats))
}

fn write_startdat<W: Write>(w: &mut W, png: &[u8], pgd: &[u8]) -> Result<()> {
    let mut h = [0u8; STARTDAT_HDR];
    h[..8].copy_from_slice(b"STARTDAT");
    wr32(&mut h, 0x08, 1);
    wr32(&mut h, 0x0C, 1);
    wr32(&mut h, 0x10, STARTDAT_HDR as u32);
    wr32(&mut h, 0x14, png.len() as u32);
    w.write_all(&h)?;
    w.write_all(png)?;
    w.write_all(pgd)?;
    Ok(())
}

/// Pads with ASCII '0' to a 16-byte boundary (as popstation/PSXPackager do).
fn pad16<W: Write + Seek>(w: &mut W) -> Result<()> {
    let p = w.stream_position()?;
    w.write_all(&b"0000000000000000"[..(p.next_multiple_of(16) - p) as usize])?;
    Ok(())
}

/// Writes DATA.PSAR starting at absolute position `psar` (the writer must be positioned there).
pub fn write_psar<W: Write + Seek>(w: &mut W, psar: u64, job: &PsarJob<'_>, monitor: &dyn Monitor) -> Result<PsarStats> {
    if job.discs.is_empty() || job.discs.len() > MAX_DISCS {
        return Err(Error::Unsupported(format!("{} discs (1..={MAX_DISCS} supported)", job.discs.len())));
    }
    for d in &job.discs {
        validate_id(&d.disc_id)?;
    }
    let level = Compression::new(job.level.clamp(1, 9));
    let total: u64 = job.discs.iter().map(|d| d.image.size_bytes()).sum();
    let png_trailer = (STARTDAT_HDR + job.boot_png.len()) as u32;
    let mut all = PsarStats::default();

    if let [spec] = job.discs.as_slice() {
        let (index, _, stats) = stream_disc(w, spec, level, monitor, 0, total)?;
        pad16(w)?;
        let p1 = u32::try_from(w.stream_position()? - psar).map_err(|_| Error::Unsupported("PSAR > 4 GiB".into()))?;
        write_startdat(w, job.boot_png, &job.donor.pgd)?;
        let end = w.stream_position()?;
        let hdr = DiscHeader { spec, toc: build_toc(spec.image), index, p1, p2: p1 + png_trailer };
        w.seek(SeekFrom::Start(psar))?;
        w.write_all(&hdr.bytes())?;
        w.seek(SeekFrom::Start(end))?;
        return Ok(stats);
    }

    // Multi-disc: 0x400 PSTITLEIMG header, each PSISOIMG at a 0x8000 boundary, one STARTDAT.
    w.write_all(&[0u8; TITLEIMG_HDR as usize])?;
    let mut disc_offsets = [0u32; MAX_DISCS];
    let mut done = 0u64;
    for (i, spec) in job.discs.iter().enumerate() {
        let here = w.stream_position()?;
        let aligned = (here - psar).next_multiple_of(TITLEIMG_DISC_ALIGN);
        io::copy(&mut io::repeat(0).take(psar + aligned - here), w)?;
        disc_offsets[i] = u32::try_from(aligned).map_err(|_| Error::Unsupported("PSAR > 4 GiB".into()))?;
        let start = psar + aligned;
        let (index, data_len, stats) = stream_disc(w, spec, level, monitor, done, total)?;
        done += spec.image.size_bytes();
        let end = start + ISO_DATA + data_len;
        // popstation semantics (PSX2PSP, 15+ years of working multi-disc EBOOTs): the per-disc
        // header leaves p1/p2 zero; the only STARTDAT pointer lives in the PSTITLEIMG header.
        let hdr = DiscHeader { spec, toc: build_toc(spec.image), index, p1: 0, p2: 0 };
        w.seek(SeekFrom::Start(start))?;
        w.write_all(&hdr.bytes())?;
        w.seek(SeekFrom::Start(end))?;
        all.raw_bytes += stats.raw_bytes;
        all.stored_bytes += stats.stored_bytes;
        all.blocks += stats.blocks;
    }
    pad16(w)?;
    let p1 = u32::try_from(w.stream_position()? - psar).map_err(|_| Error::Unsupported("PSAR > 4 GiB".into()))?;
    write_startdat(w, job.boot_png, &job.donor.pgd)?;
    let end = w.stream_position()?;

    let mut h = [0u8; TITLEIMG_HDR as usize];
    h[..16].copy_from_slice(MAGIC_TITLE);
    wr32(&mut h, 0x10, p1);
    let consts = job.donor.title_consts.clone().unwrap_or(TitleConsts::POPSTATION);
    h[0x18..0x28].copy_from_slice(&consts.unk18);
    for (i, off) in disc_offsets.iter().enumerate() {
        wr32(&mut h, 0x200 + i * 4, *off);
    }
    // 12 words PSXPackager fills with random values < 0xFFFF: derived deterministically from the
    // title so builds are reproducible.
    let mut x = job.title.bytes().fold(0x9E37_79B9u32, |a, b| a.rotate_left(5) ^ b as u32) | 1;
    for i in 0..12 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        wr32(&mut h, 0x214 + i * 4, x % 0xFFFF);
    }
    let id = job.discs[0].disc_id.as_bytes();
    h[0x264] = b'_';
    h[0x265..0x269].copy_from_slice(&id[..4]);
    h[0x269] = b'_';
    h[0x26A..0x26F].copy_from_slice(&id[4..9]);
    wr32(&mut h, 0x284, p1 + png_trailer);
    h[0x28C..0x30C].copy_from_slice(&consts.data3);
    put_title(&mut h[0x30C..0x38C], &job.title);
    wr32(&mut h, 0x38C, 7);
    w.seek(SeekFrom::Start(psar))?;
    w.write_all(&h)?;
    w.seek(SeekFrom::Start(end))?;
    Ok(all)
}

// ───────────────────────────────────── reader ─────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsarDisc {
    /// Offset of the PSISOIMG relative to the PSAR start.
    pub offset: u64,
    pub disc_id: String,
    pub title: String,
    pub toc: Vec<u8>,
    pub index: BlockIndex,
}

impl PsarDisc {
    pub fn tracks(&self) -> Vec<TocTrack> {
        parse_toc(&self.toc).0
    }

    /// Uncompressed image size: lead-out from the TOC when plausible, else all blocks.
    pub fn image_size(&self) -> u64 {
        let blocks = self.index.len() as u64 * BLOCK as u64;
        match parse_toc(&self.toc).1 {
            Some(lo) if (lo as u64 * RAW_SECTOR) <= blocks && lo > 0 => lo as u64 * RAW_SECTOR,
            _ => blocks,
        }
    }
}

/// Discovers every disc in a DATA.PSAR located at absolute `psar`.
pub fn read_discs<R: Read + Seek>(r: &mut R, psar: u64) -> Result<Vec<PsarDisc>> {
    let mut head = [0u8; TITLEIMG_HDR as usize];
    r.seek(SeekFrom::Start(psar))?;
    r.read_exact(&mut head).map_err(|_| Error::format("DATA.PSAR", "too short"))?;
    let offsets: Vec<u64> = if head.starts_with(MAGIC_TITLE) {
        (0..MAX_DISCS).map(|i| rd32(&head, 0x200 + i * 4) as u64).filter(|&o| o != 0).collect()
    } else if head.starts_with(MAGIC_ISO) {
        vec![0]
    } else {
        return Err(Error::format("DATA.PSAR", "neither PSISOIMG nor PSTITLEIMG (encrypted/official PSN EBOOT?)"));
    };
    let mut discs = Vec::with_capacity(offsets.len());
    let mut h = vec![0u8; ISO_DATA as usize];
    for offset in offsets {
        r.seek(SeekFrom::Start(psar + offset))?;
        r.read_exact(&mut h).map_err(|_| Error::format("PSISOIMG", "header truncated"))?;
        if !h.starts_with(MAGIC_ISO) {
            return Err(Error::format("PSISOIMG", format!("bad magic at PSAR+{offset:#x}")));
        }
        let disc_id: String = h[0x401..0x405].iter().chain(&h[0x406..0x40B]).map(|&c| c as char).collect();
        let title_raw = &h[0x122C..0x12AC];
        let title = String::from_utf8_lossy(&title_raw[..title_raw.iter().position(|&c| c == 0).unwrap_or(128)]).into_owned();
        let toc_len = h[TOC_AT..TOC_AT + TOC_MAX].chunks_exact(10).take_while(|e| e.iter().any(|&b| b != 0)).count() * 10;
        let toc = h[TOC_AT..TOC_AT + toc_len].to_vec();
        let mut index = Vec::new();
        for i in 0..INDEX_CAP {
            let e = INDEX_AT + i * 32;
            let (off, len) = (rd32(&h, e), rd32(&h, e + 4));
            if len == 0 {
                break;
            }
            if len as usize > BLOCK {
                return Err(Error::format("PSISOIMG", format!("index entry {i} length {len:#x} > block")));
            }
            index.push((off, len));
        }
        discs.push(PsarDisc { offset, disc_id, title, toc, index });
    }
    Ok(discs)
}

/// Decompresses one disc to `out` (raw 2352-byte sectors). Returns bytes written.
pub fn extract_disc<R: Read + Seek, W: Write>(
    r: &mut R,
    psar: u64,
    disc: &PsarDisc,
    out: &mut W,
    monitor: &dyn Monitor,
) -> Result<u64> {
    let size = disc.image_size();
    let base = psar + disc.offset + ISO_DATA;
    let mut written = 0u64;
    let mut comp = vec![0u8; BLOCK];
    for chunk in disc.index.chunks(BATCH) {
        if monitor.cancelled() {
            return Err(Error::Cancelled);
        }
        let mut packed: Vec<Vec<u8>> = Vec::with_capacity(chunk.len());
        for &(off, len) in chunk {
            r.seek(SeekFrom::Start(base + off as u64))?;
            r.read_exact(&mut comp[..len as usize])?;
            packed.push(comp[..len as usize].to_vec());
        }
        let raw = packed.par_iter().map(|p| inflate(p)).collect::<Result<Vec<_>>>()?;
        for b in raw {
            if written == size {
                break;
            }
            let n = (size - written).min(BLOCK as u64) as usize;
            out.write_all(&b[..n])?;
            written += n as u64;
        }
        monitor.progress(written, size);
    }
    Ok(written)
}

fn inflate(p: &[u8]) -> Result<Vec<u8>> {
    if p.len() == BLOCK {
        return Ok(p.to_vec());
    }
    let mut v = Vec::with_capacity(BLOCK);
    DeflateDecoder::new(p).read_to_end(&mut v)?;
    if v.len() != BLOCK {
        return Err(Error::format("PSISOIMG", format!("block inflated to {} bytes", v.len())));
    }
    Ok(v)
}
