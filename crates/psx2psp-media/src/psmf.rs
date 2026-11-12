//! Native PSMF muxer for `ICON1.PMF`, plus a strict parser used for validation.
//!
//! FFmpeg can encode the H.264 but has no PSMF muxer, and none of the competing tools (PSXPackager,
//! PSX2PSP-Enhanced, pop-fe) can create an ICON1. The layout below was taken byte for byte from
//! Sony-authored PMFs (an XMB `ICON1.PMF` and a 480×272 gameboot):
//!
//! ```text
//! 0x000  "PSMF0014" · BE32 0x800 (stream offset) · BE32 stream size
//! 0x050  BE32 0x3E · TS6 first · TS6 last · BE32 mux rate · BE32 0x15F90 · 01 01 · BE32 0x24
//! 0x06E  TS6 first · TS6 last · BE16 1 · BE32 0x12 · BE16 streams(1)
//! 0x082  stream entry: E0 00 20 14 · 8×00 · width/16 · height/16 · 00 00   (no EP map)
//! 0x800  MPEG-2 program stream, fixed 2048-byte packs (14-byte pack header, no stuffing)
//! ```
//!
//! Every group of pictures starts on a fresh pack holding: the system header, a private-stream-2
//! (`0xBF`) table with the byte size of each access unit in the group, and the first video PES
//! (PTS + DTS + P-STD extension `1E 60 81`). Access units start with an AUD and use 4-byte start
//! codes; timestamps are 29.97 fps with `DTS = PTS − 3003` and the first PTS at 90 000.

use crate::MediaError;

pub const PACK: usize = 2048;
pub const HEADER: usize = 0x800;
/// One frame at 30000/1001 fps in 90 kHz ticks.
pub const FRAME_TICKS: u64 = 3003;
pub const FIRST_PTS: u64 = 90_000;
pub const ICON1_W: u32 = 144;
pub const ICON1_H: u32 = 80;

/// Program mux rate in 50 B/s units (1.25 MB/s), as in every Sony sample.
const MUX_RATE: u32 = 25_000;
/// 2048 bytes at the mux rate, in 90 kHz ticks (147.456, rounded up).
const PACK_TICKS: u64 = 148;
/// SCR lead over the DTS of the access unit being delivered (Sony sample: 86 997 − 10 147).
const SCR_LEAD: u64 = 76_850;
const PACK_HDR: usize = 14;
const SYSTEM_HEADER: [u8; 18] = [0, 0, 1, 0xBB, 0, 0x0C, 0x80, 0xC3, 0x51, 0x80, 0xF0, 0x7F, 0xB9, 0xE0, 0x81, 0xBD, 0xE0, 0x08];
const PSTD_EXT: [u8; 3] = [0x1E, 0x60, 0x81];
/// `00 00 00 01 09 F0`: access unit delimiter, primary_pic_type = any.
const AUD: [u8; 6] = [0, 0, 0, 1, 0x09, 0xF0];

fn fmt(msg: impl Into<String>) -> MediaError {
    MediaError::Format(msg.into())
}

// ───────────────────────────────── H.264 Annex-B ─────────────────────────────────

/// NAL payloads (header byte first) of an Annex-B stream, start codes and trailing zeros removed.
pub fn split_nals(es: &[u8]) -> Vec<&[u8]> {
    nal_units(es).into_iter().map(|(_, n)| n).collect()
}

/// Like [`split_nals`], also telling whether each NAL had a 4-byte start code, so the muxer can
/// reproduce the input byte for byte (Sony streams mix 4-byte codes on AUD/SPS/PPS with 3-byte
/// ones on SEI and slices).
fn nal_units(es: &[u8]) -> Vec<(bool, &[u8])> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(es.len(), |&n| n - 3);
        while e > s && es[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push((s >= 4 && es[s - 4] == 0, &es[s..e]));
        }
    }
    out
}

#[derive(Debug, Default)]
struct AccessUnit {
    bytes: Vec<u8>,
    idr: bool,
    vcl: bool,
}

/// Groups NALs into access units, each led by an AUD with 4-byte start codes. Streams without
/// AUDs get one synthesised per picture. Encoder user-data SEI (x264's settings string) and
/// end-of-sequence/stream NALs are dropped.
fn access_units(es: &[u8]) -> Result<Vec<AccessUnit>, MediaError> {
    let nals = nal_units(es);
    let has_aud = nals.iter().any(|(_, n)| n[0] & 0x1F == 9);
    let mut aus: Vec<AccessUnit> = Vec::new();
    for (long, n) in nals {
        let ty = n[0] & 0x1F;
        if (ty == 6 && n.get(1) == Some(&5)) || ty == 10 || ty == 11 {
            continue;
        }
        let vcl = matches!(ty, 1 | 5);
        let cur_vcl = aus.last().is_some_and(|a| a.vcl);
        let start = aus.is_empty()
            || if has_aud {
                ty == 9
            } else {
                // first_mb_in_slice == 0 ⇔ ue(v) "1" ⇔ top bit of the slice header set.
                cur_vcl && ((vcl && n.get(1).is_some_and(|b| b & 0x80 != 0)) || matches!(ty, 6..=8))
            };
        if start {
            aus.push(AccessUnit::default());
            if !has_aud {
                aus.last_mut().unwrap().bytes.extend(AUD);
            }
        }
        let au = aus.last_mut().unwrap();
        // The AU table counts from a 4-byte start code on the delimiter.
        if long || ty == 9 {
            au.bytes.push(0);
        }
        au.bytes.extend([0, 0, 1]);
        au.bytes.extend(n);
        au.vcl |= vcl;
        au.idr |= ty == 5;
    }
    aus.retain(|a| a.vcl);
    match aus.first() {
        None => Err(fmt("H.264 stream has no pictures")),
        Some(a) if !a.idr => Err(fmt("H.264 stream must start with an IDR picture")),
        _ => Ok(aus),
    }
}

// ───────────────────────────────── MPEG-2 PS ─────────────────────────────────

fn pack_header(out: &mut Vec<u8>, scr: u64) {
    let m = MUX_RATE;
    out.extend([
        0,
        0,
        1,
        0xBA,
        0x44 | ((scr >> 27) & 0x38) as u8 | ((scr >> 28) & 0x03) as u8,
        (scr >> 20) as u8,
        ((scr >> 12) & 0xF8) as u8 | 0x04 | ((scr >> 13) & 0x03) as u8,
        (scr >> 5) as u8,
        ((scr << 3) & 0xF8) as u8 | 0x04,
        0x01,
        (m >> 14) as u8,
        (m >> 6) as u8,
        ((m << 2) & 0xFC) as u8 | 0x03,
        0xF8,
    ]);
}

fn timestamp(prefix: u8, ts: u64) -> [u8; 5] {
    [
        (prefix << 4) | ((ts >> 29) & 0x0E) as u8 | 1,
        (ts >> 22) as u8,
        ((ts >> 14) & 0xFE) as u8 | 1,
        (ts >> 7) as u8,
        ((ts << 1) & 0xFE) as u8 | 1,
    ]
}

fn read_ts(b: &[u8]) -> u64 {
    ((b[0] as u64 >> 1) & 7) << 30 | (b[1] as u64) << 22 | (b[2] as u64 >> 1) << 15 | (b[3] as u64) << 7 | b[4] as u64 >> 1
}

/// Private stream 2 table: `01 E0`, 12 zero bytes, BE16 table length, BE16 count, then per AU
/// `04 80 | size` (`04 00 | size` for the last one).
fn au_table(sizes: &[usize]) -> Result<Vec<u8>, MediaError> {
    let n = sizes.len();
    let body = 18 + 4 * n;
    let mut p = Vec::with_capacity(6 + body);
    p.extend([0, 0, 1, 0xBF]);
    p.extend((body as u16).to_be_bytes());
    p.extend([0x01, 0xE0]);
    p.extend([0u8; 12]);
    p.extend(((2 + 4 * n) as u16).to_be_bytes());
    p.extend((n as u16).to_be_bytes());
    for (i, &s) in sizes.iter().enumerate() {
        let s = u16::try_from(s).map_err(|_| fmt(format!("access unit of {s} bytes exceeds the 64 KiB PSMF limit")))?;
        p.extend(if i + 1 < n { [0x04, 0x80] } else { [0x04, 0x00] });
        p.extend(s.to_be_bytes());
    }
    Ok(p)
}

fn ts6(h: &mut [u8], o: usize, ts: u64) {
    h[o..o + 2].fill(0);
    h[o + 2..o + 6].copy_from_slice(&(ts as u32).to_be_bytes());
}

fn psmf_header(stream_size: u32, frames: u64, width: u32, height: u32) -> [u8; HEADER] {
    let last = FIRST_PTS + frames * FRAME_TICKS;
    let mut h = [0u8; HEADER];
    h[..8].copy_from_slice(b"PSMF0014");
    h[0x08..0x0C].copy_from_slice(&(HEADER as u32).to_be_bytes());
    h[0x0C..0x10].copy_from_slice(&stream_size.to_be_bytes());
    h[0x50..0x54].copy_from_slice(&0x3Eu32.to_be_bytes());
    ts6(&mut h, 0x54, FIRST_PTS);
    ts6(&mut h, 0x5A, last);
    h[0x60..0x64].copy_from_slice(&MUX_RATE.to_be_bytes());
    h[0x64..0x68].copy_from_slice(&0x0001_5F90u32.to_be_bytes());
    h[0x68] = 1;
    h[0x69] = 1;
    h[0x6A..0x6E].copy_from_slice(&0x24u32.to_be_bytes());
    ts6(&mut h, 0x6E, FIRST_PTS);
    ts6(&mut h, 0x74, last);
    h[0x7A..0x7C].copy_from_slice(&1u16.to_be_bytes());
    h[0x7C..0x80].copy_from_slice(&0x12u32.to_be_bytes());
    h[0x80..0x82].copy_from_slice(&1u16.to_be_bytes());
    h[0x82..0x86].copy_from_slice(&[0xE0, 0x00, 0x20, 0x14]);
    h[0x8E] = (width / 16) as u8;
    h[0x8F] = (height / 16) as u8;
    h
}

/// Muxes a 144×80, 29.97 fps H.264 Annex-B stream (no B-frames) into an `ICON1.PMF`.
pub fn mux_icon1(es: &[u8]) -> Result<Vec<u8>, MediaError> {
    mux(es, ICON1_W, ICON1_H)
}

/// Muxes an H.264 Annex-B stream of `width`×`height` (multiples of 16) into a PSMF.
pub fn mux(es: &[u8], width: u32, height: u32) -> Result<Vec<u8>, MediaError> {
    if !width.is_multiple_of(16) || !height.is_multiple_of(16) || width == 0 || height == 0 {
        return Err(fmt(format!("{width}×{height}: PSMF needs dimensions that are multiples of 16")));
    }
    let aus = access_units(es)?;
    let dts = |i: usize| FIRST_PTS - FRAME_TICKS + i as u64 * FRAME_TICKS;
    let mut out = vec![0u8; HEADER];
    let mut scr_prev: Option<u64> = None;

    let mut g0 = 0;
    while g0 < aus.len() {
        let g1 = aus[g0 + 1..].iter().position(|a| a.idr).map_or(aus.len(), |p| g0 + 1 + p);
        let group = &aus[g0..g1];
        let sizes: Vec<usize> = group.iter().map(|a| a.bytes.len()).collect();
        let mut starts = Vec::with_capacity(group.len());
        let mut data = Vec::with_capacity(sizes.iter().sum());
        for a in group {
            starts.push(data.len());
            data.extend(&a.bytes);
        }

        let mut pos = 0;
        let mut first = true;
        while pos < data.len() {
            let pack_at = out.len();
            let au_at = starts.partition_point(|&s| s <= pos) - 1;
            let scr = dts(g0 + au_at).saturating_sub(SCR_LEAD).max(scr_prev.map_or(0, |p| p + PACK_TICKS));
            scr_prev = Some(scr);
            pack_header(&mut out, scr);
            let mut room = PACK - PACK_HDR;
            if first {
                let table = au_table(&sizes)?;
                if SYSTEM_HEADER.len() + table.len() + 9 + 13 + 1 > room {
                    return Err(fmt(format!("GOP of {} pictures is too long for one PSMF table (use a keyframe interval ≤ 300)", group.len())));
                }
                out.extend(SYSTEM_HEADER);
                out.extend(&table);
                room -= SYSTEM_HEADER.len() + table.len();
            }
            // Header data: optional PTS+DTS (10) and, on a group's first PES, the P-STD extension.
            let ext = if first { PSTD_EXT.len() } else { 0 };
            let cap_ts = room - 9 - ext - 10;
            let next = starts.partition_point(|&s| s < pos);
            let stamp = (next < starts.len() && starts[next] < pos + cap_ts).then_some(g0 + next);
            let hdl = ext + if stamp.is_some() { 10 } else { 0 };
            let cap = room - 9 - hdl;
            let take = cap.min(data.len() - pos);
            let left = cap - take;
            // Gaps under 6 bytes can't hold a padding packet: absorb them as PES header stuffing.
            let (stuff, pad) = if left < 6 { (left, 0) } else { (0, left) };

            out.extend([0, 0, 1, 0xE0]);
            out.extend(((3 + hdl + stuff + take) as u16).to_be_bytes());
            out.push(0x81);
            out.push(if stamp.is_some() { 0xC0 } else { 0 } | if first { 0x01 } else { 0 });
            out.push((hdl + stuff) as u8);
            if let Some(i) = stamp {
                out.extend(timestamp(3, dts(i) + FRAME_TICKS));
                out.extend(timestamp(1, dts(i)));
            }
            if first {
                out.extend(PSTD_EXT);
            }
            out.extend(std::iter::repeat_n(0xFF, stuff));
            out.extend(&data[pos..pos + take]);
            if pad > 0 {
                out.extend([0, 0, 1, 0xBE]);
                out.extend(((pad - 6) as u16).to_be_bytes());
                out.extend(std::iter::repeat_n(0xFF, pad - 6));
            }
            debug_assert_eq!(out.len() - pack_at, PACK);
            pos += take;
            first = false;
        }
        g0 = g1;
    }

    let stream = u32::try_from(out.len() - HEADER).map_err(|_| fmt("PSMF over 4 GiB"))?;
    out[..HEADER].copy_from_slice(&psmf_header(stream, aus.len() as u64, width, height));
    Ok(out)
}

// ───────────────────────────────── parser ─────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsmfInfo {
    pub version: [u8; 4],
    pub width: u32,
    pub height: u32,
    pub first_ts: u64,
    pub last_ts: u64,
    pub packs: usize,
    /// Number of private-stream-2 AU tables (= groups of pictures).
    pub groups: usize,
    /// AU sizes listed by the tables, in order.
    pub table_sizes: Vec<usize>,
    /// Access units in the video elementary stream (counted by AUD).
    pub frames: usize,
    /// First video PTS / DTS found in a PES header.
    pub first_pts: Option<(u64, Option<u64>)>,
    /// Concatenated `0xE0` PES payloads (Annex-B H.264).
    pub video: Vec<u8>,
}

impl PsmfInfo {
    /// Play length from the pictures actually present (29.97 fps). Sony samples do not keep the
    /// header's last timestamp in step with the stream, so it is not used for this.
    pub fn duration_secs(&self) -> f64 {
        self.frames as f64 * FRAME_TICKS as f64 / 90_000.0
    }
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

/// Parses and validates a PSMF: header fields, pack framing (every pack exactly 2048 bytes and
/// fully covered by PES packets), and extracts the video stream.
pub fn parse(pmf: &[u8]) -> Result<PsmfInfo, MediaError> {
    if pmf.len() < HEADER + PACK || &pmf[..4] != b"PSMF" {
        return Err(fmt("not a PSMF file"));
    }
    let off = be32(pmf, 8) as usize;
    let size = be32(pmf, 12) as usize;
    if off != HEADER || off + size != pmf.len() || !size.is_multiple_of(PACK) {
        return Err(fmt(format!("PSMF framing: offset {off:#x}, stream {size:#x}, file {:#x}", pmf.len())));
    }
    let ts = |o: usize| be32(pmf, o + 2) as u64;
    let mut info = PsmfInfo {
        version: pmf[4..8].try_into().unwrap(),
        width: pmf[0x8E] as u32 * 16,
        height: pmf[0x8F] as u32 * 16,
        first_ts: ts(0x54),
        last_ts: ts(0x5A),
        packs: size / PACK,
        groups: 0,
        table_sizes: Vec::new(),
        frames: 0,
        first_pts: None,
        video: Vec::new(),
    };
    for k in 0..info.packs {
        let base = HEADER + k * PACK;
        let p = &pmf[base..base + PACK];
        if p[..4] != [0, 0, 1, 0xBA] || p[4] & 0xC0 != 0x40 {
            return Err(fmt(format!("pack {k} at {base:#x}: no MPEG-2 pack header")));
        }
        let mut o = PACK_HDR + (p[13] & 7) as usize;
        while o < PACK {
            if o + 6 > PACK || p[o..o + 3] != [0, 0, 1] {
                return Err(fmt(format!("pack {k}: broken PES chain at +{o:#x}")));
            }
            let sid = p[o + 3];
            let len = u16::from_be_bytes([p[o + 4], p[o + 5]]) as usize;
            let body = p.get(o + 6..o + 6 + len).ok_or_else(|| fmt(format!("pack {k}: PES {sid:#04x} overruns the pack")))?;
            match sid {
                0xE0 => {
                    if body.len() < 3 || body[0] & 0xC0 != 0x80 {
                        return Err(fmt(format!("pack {k}: video PES is not MPEG-2")));
                    }
                    let hdl = body[2] as usize;
                    if body[1] & 0x80 != 0 && info.first_pts.is_none() {
                        let dts = (body[1] & 0x40 != 0).then(|| read_ts(&body[8..13]));
                        info.first_pts = Some((read_ts(&body[3..8]), dts));
                    }
                    info.video.extend(body.get(3 + hdl..).ok_or_else(|| fmt(format!("pack {k}: PES header overruns")))?);
                }
                0xBF if body.len() >= 18 && body[..2] == [0x01, 0xE0] => {
                    let n = u16::from_be_bytes([body[16], body[17]]) as usize;
                    let ents = body.get(18..18 + 4 * n).ok_or_else(|| fmt(format!("pack {k}: AU table truncated")))?;
                    info.table_sizes.extend(ents.chunks_exact(4).map(|e| u16::from_be_bytes([e[2], e[3]]) as usize));
                    info.groups += 1;
                }
                _ => {}
            }
            o += 6 + len;
        }
    }
    info.frames = split_nals(&info.video).iter().filter(|n| n[0] & 0x1F == 9).count();
    Ok(info)
}

/// Byte size of every access unit of an Annex-B stream, split at AUDs (4-byte start codes).
pub fn au_sizes(es: &[u8]) -> Vec<usize> {
    let mut at = Vec::new();
    let mut i = 0;
    while let Some(p) = es[i..].windows(5).position(|w| w == [0, 0, 0, 1, 9]) {
        at.push(i + p);
        i += p + 5;
    }
    at.iter().enumerate().map(|(k, &s)| at.get(k + 1).copied().unwrap_or(es.len()) - s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny synthetic stream: SPS, PPS, IDR, then P pictures, IDR every `gop`, no AUDs.
    fn synthetic(frames: usize, gop: usize, payload: usize) -> Vec<u8> {
        let mut es = Vec::new();
        for f in 0..frames {
            if f % gop == 0 {
                es.extend([0, 0, 0, 1, 0x67, 0x4D, 0x40, 0x15, 0xAA]);
                es.extend([0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80]);
                es.extend([0, 0, 1, 0x65, 0x88]);
            } else {
                es.extend([0, 0, 1, 0x41, 0x9A]);
            }
            es.extend((0..payload).map(|i| (i % 251 + 1) as u8));
        }
        es
    }

    #[test]
    fn mux_roundtrips_and_frames_every_pack() {
        let es = synthetic(95, 30, 700);
        let pmf = mux_icon1(&es).unwrap();
        assert_eq!(pmf.len() % PACK, 0);
        let info = parse(&pmf).unwrap();
        assert_eq!((info.width, info.height), (144, 80));
        assert_eq!(&info.version, b"0014");
        assert_eq!(info.frames, 95);
        assert_eq!(info.groups, 4, "IDR at 0, 30, 60, 90");
        assert_eq!(info.first_pts, Some((FIRST_PTS, Some(FIRST_PTS - FRAME_TICKS))));
        assert_eq!(info.last_ts, FIRST_PTS + 95 * FRAME_TICKS);
        assert_eq!(info.table_sizes, au_sizes(&info.video), "AU tables describe the stream exactly");
        // Video bytes survive untouched apart from the synthesised AUDs.
        let stripped: Vec<u8> = split_nals(&info.video).into_iter().filter(|n| n[0] & 0x1F != 9).flatten().copied().collect();
        let original: Vec<u8> = split_nals(&es).into_iter().flatten().copied().collect();
        assert_eq!(stripped, original);
    }

    #[test]
    fn header_matches_sony_icon1_layout() {
        let pmf = mux_icon1(&synthetic(10, 30, 100)).unwrap();
        let sony_0x50: [u8; 0x42] = [
            0x00, 0x00, 0x00, 0x3E, 0x00, 0x00, 0x00, 0x01, 0x5F, 0x90, 0x00, 0x00, 0x00, 0x01, 0xD4, 0xDE, //
            0x00, 0x00, 0x61, 0xA8, 0x00, 0x01, 0x5F, 0x90, 0x01, 0x01, 0x00, 0x00, 0x00, 0x24, 0x00, 0x00, //
            0x00, 0x01, 0x5F, 0x90, 0x00, 0x00, 0x00, 0x01, 0xD4, 0xDE, 0x00, 0x01, 0x00, 0x00, 0x00, 0x12, //
            0x00, 0x01, 0xE0, 0x00, 0x20, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x05, //
            0x00, 0x00,
        ];
        assert_eq!(pmf[0x50..0x92], sony_0x50, "last TS 0x1D4DE = 90000 + 10·3003");
        // First pack: pack header (SCR 10 147 as in the Sony sample), system header, AU table.
        assert_eq!(pmf[0x800..0x80E], [0, 0, 1, 0xBA, 0x44, 0x00, 0x05, 0x3D, 0x1C, 0x01, 0x01, 0x86, 0xA3, 0xF8]);
        assert_eq!(pmf[0x80E..0x820], SYSTEM_HEADER);
        assert_eq!(pmf[0x820..0x824], [0, 0, 1, 0xBF]);
    }

    #[test]
    fn rejects_streams_without_leading_idr() {
        assert!(mux_icon1(&[0, 0, 1, 0x41, 0x9A, 1, 2, 3]).is_err());
        assert!(mux_icon1(&[]).is_err());
    }

    /// Demux the Sony-authored ICON1 shipped with PSX2PSP (when present), remux its video and
    /// compare the AU tables: our grouping must reproduce Sony's.
    #[test]
    fn remux_sony_sample_if_present() {
        let path = r"E:\Programmi VARI\Consoles TOOLS\PSP\PSX2PSPv1.4.2_BASE\Pics\icon1.pmf";
        let Ok(sony) = std::fs::read(path) else { return };
        let a = parse(&sony).unwrap();
        assert_eq!((a.width, a.height, a.frames), (144, 80, 605));
        assert_eq!(a.table_sizes, au_sizes(&a.video));
        let b = parse(&mux_icon1(&a.video).unwrap()).unwrap();
        assert_eq!(b.video, a.video, "video stream preserved bit for bit");
        assert_eq!(b.table_sizes, a.table_sizes);
        assert_eq!(b.first_pts, a.first_pts);
    }
}
