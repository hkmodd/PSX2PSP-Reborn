//! Native ATRAC3 encoder: 44.1 kHz stereo PCM → RIFF/WAVE ATRAC3 (`SND0.AT3`).
//!
//! Pipeline per channel and 1024-sample frame: three-stage QMF into 4 bands → windowed MDCT
//! (256 lines per band) → per-subband scale factor + word length chosen by a greedy
//! rate–distortion allocator → Huffman or constant-length mantissas. The three PSP rates are
//! the Sony frame sizes: 66 kbps (192 B, joint stereo M/S), 105 kbps (2 × 152 B) and 132 kbps
//! (2 × 192 B). Gain control and tonal components are valid-but-unused bitstream features (both
//! written as "none"), which every decoder accepts.
//!
//! Written from the format, not ported: constants in [`tables`] are bitstream data; the analysis
//! is derived as the adjoint of the decoder's synthesis.

mod alloc;
mod analysis;
mod bits;
#[cfg(test)]
mod decode;
mod tables;

use psx2psp_core::Monitor;

use crate::MediaError;
use crate::tools::at3_block_align;
use alloc::{SU_FIXED, SU_JOINT_FIXED, allocate, write_unit};
use analysis::Analysis;
use bits::BitWriter;
use tables::FRAME;

/// End-to-end codec delay in samples (QMF tree both ways + one 256-sample MDCT block per band),
/// checked by `analysis_synthesis_is_transparent`. The encoder appends silent frames to flush it.
pub const DELAY: usize = 1162;

/// Frame-level encoder state for one stream.
pub struct Encoder {
    block: usize,
    joint: bool,
    ch: [Analysis; 2],
}

impl Encoder {
    /// `kbps` ∈ {66, 105, 132}.
    pub fn new(kbps: u32) -> Option<Self> {
        let block = usize::from(at3_block_align(kbps)?);
        Some(Self { block, joint: kbps == 66, ch: Default::default() })
    }

    pub fn block_align(&self) -> usize {
        self.block
    }

    pub fn joint_stereo(&self) -> bool {
        self.joint
    }

    /// Encodes one frame (1024 samples per channel, s16 scale) and appends `block_align` bytes.
    pub fn frame(&mut self, left: &[f32], right: &[f32], out: &mut Vec<u8>) {
        let mut spec = [[0.0f32; FRAME]; 2];
        if self.joint {
            // Matrix code 3 decodes L = c1 + c2, R = c1 − c2.
            let mid: Vec<f32> = left.iter().zip(right).map(|(l, r)| (l + r) * 0.5).collect();
            let side: Vec<f32> = left.iter().zip(right).map(|(l, r)| (l - r) * 0.5).collect();
            self.ch[0].frame(&mid, &mut spec[0]);
            self.ch[1].frame(&side, &mut spec[1]);
        } else {
            self.ch[0].frame(left, &mut spec[0]);
            self.ch[1].frame(right, &mut spec[1]);
        }
        let start = out.len();
        out.resize(start + self.block, 0);
        let frame = &mut out[start..];
        if self.joint {
            // Both units share the frame: the first from the front, the second byte-reversed
            // from the back. Two partial bytes are the only slack needed.
            let budget = (self.block * 8 - 14) as u32;
            let plans = allocate(&[&spec[0], &spec[1]], &[SU_FIXED, SU_JOINT_FIXED], budget);
            let mut w1 = BitWriter::with_capacity(self.block);
            write_unit(&mut w1, &spec[0], &plans[0], false);
            let mut w2 = BitWriter::with_capacity(self.block);
            w2.put(0, 1); // weighting: off
            w2.put(7, 3); // unit weights
            for _ in 0..4 {
                w2.put(3, 2); // M/S matrix on every QMF band
            }
            write_unit(&mut w2, &spec[1], &plans[1], true);
            let (a, b) = (w1.into_bytes(), w2.into_bytes());
            assert!(a.len() + b.len() <= self.block, "joint-stereo units overlap");
            frame[..a.len()].copy_from_slice(&a);
            for (i, byte) in b.into_iter().enumerate() {
                frame[self.block - 1 - i] = byte;
            }
        } else {
            let half = self.block / 2;
            for (c, chunk) in frame.chunks_exact_mut(half).enumerate() {
                let plan = allocate(&[&spec[c]], &[SU_FIXED], (half * 8) as u32);
                let mut w = BitWriter::with_capacity(half);
                write_unit(&mut w, &spec[c], &plan[0], false);
                let b = w.into_bytes();
                assert!(b.len() <= half, "sound unit over budget");
                chunk[..b.len()].copy_from_slice(&b);
            }
        }
    }
}

/// Wraps ATRAC3 frames in the RIFF/WAVE layout the PSP accepts for SND0.AT3 (the same fmt chunk
/// atracdenc and Sony's tools write: tag 0x0270, 14-byte extradata, no `fact` chunk).
pub fn riff(frames: &[u8], block_align: usize, joint: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(60 + frames.len());
    let byte_rate = (block_align * 44_100 / FRAME) as u32;
    let mode: u16 = joint.into();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(52 + frames.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&32u32.to_le_bytes());
    b.extend_from_slice(&0x0270u16.to_le_bytes()); // WAVE_FORMAT_SONY_SCX (ATRAC3)
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&44_100u32.to_le_bytes());
    b.extend_from_slice(&byte_rate.to_le_bytes());
    b.extend_from_slice(&(block_align as u16).to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // bits per sample
    b.extend_from_slice(&14u16.to_le_bytes()); // extradata size
    // version, u32 LE 0x10, mode ×2, frame factor, 0: the bytes of the SND0.AT3 files that play in
    // the XMB (FFmpeg skips bytes 2-5, so only a PSP tells a wrong value there).
    for v in [1u16, 0x0010, 0, mode, mode, 1, 0] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    b.extend_from_slice(frames);
    b
}

/// Interleaved samples of a RIFF/WAVE file in 16-bit PCM, 44.1 kHz stereo (what
/// [`crate::tools::Ffmpeg::to_wav`] writes); `None` for any other layout.
pub fn read_wav(b: &[u8]) -> Option<Vec<i16>> {
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let (mut p, mut fmt_ok) = (12usize, false);
    while p + 8 <= b.len() {
        let len = u32::from_le_bytes(b[p + 4..p + 8].try_into().ok()?) as usize;
        let body = &b[p + 8..(p + 8).saturating_add(len).min(b.len())];
        match &b[p..p + 4] {
            b"fmt " => {
                let u16_at = |i: usize| body.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
                let tag = u16_at(0)?;
                // WAVE_FORMAT_EXTENSIBLE carries the real tag in its sub-format GUID.
                let pcm = tag == 1 || (tag == 0xFFFE && u16_at(24)? == 1);
                fmt_ok = pcm && u16_at(2)? == 2 && body.get(4..8)? == 44_100u32.to_le_bytes() && u16_at(14)? == 16;
            }
            // FFmpeg streaming to a pipe writes 0xFFFFFFFF: the data runs to the end of the file.
            b"data" if fmt_ok => return Some(body.as_chunks::<2>().0.iter().map(|&s| i16::from_le_bytes(s)).collect()),
            _ => {}
        }
        p = p.saturating_add(8).saturating_add(len).saturating_add(len & 1);
    }
    None
}

/// Encodes interleaved 44.1 kHz stereo s16 PCM into a complete RIFF ATRAC3 file.
pub fn encode(pcm: &[i16], kbps: u32, mon: &dyn Monitor) -> Result<Vec<u8>, MediaError> {
    let mut enc = Encoder::new(kbps).ok_or_else(|| MediaError::Format(format!("{kbps} kbps is not an ATRAC3 rate (66, 105, 132)")))?;
    let samples = pcm.len() / 2;
    let frames = (samples + DELAY).div_ceil(FRAME).max(1);
    let mut out = Vec::with_capacity(frames * enc.block_align());
    let (mut l, mut r) = ([0.0f32; FRAME], [0.0f32; FRAME]);
    for f in 0..frames {
        if mon.cancelled() {
            return Err(MediaError::Cancelled);
        }
        for i in 0..FRAME {
            let at = (f * FRAME + i) * 2;
            l[i] = pcm.get(at).map_or(0.0, |&v| f32::from(v));
            r[i] = pcm.get(at + 1).map_or(0.0, |&v| f32::from(v));
        }
        enc.frame(&l, &r, &mut out);
    }
    Ok(riff(&out, enc.block_align(), enc.joint_stereo()))
}

#[cfg(test)]
mod tests {
    use super::analysis::Qmf;
    use super::decode::{Synthesis, decode_riff, frame_spectra};
    use super::*;

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((s >> 8) as f32 / (1 << 24) as f32 - 0.5) * 20_000.0
            })
            .collect()
    }

    /// (gain, SNR dB) of `y` against `x` delayed by `lag`, edges excluded.
    fn snr_at(x: &[f32], y: &[f32], lag: usize) -> (f64, f64) {
        let n = x.len().min(y.len() - lag) - 2048;
        let (mut xy, mut xx) = (0.0f64, 0.0f64);
        for i in 1024..n {
            xy += f64::from(x[i]) * f64::from(y[i + lag]);
            xx += f64::from(x[i]) * f64::from(x[i]);
        }
        let g = xy / xx;
        let (mut e, mut p) = (0.0f64, 0.0f64);
        for i in 1024..n {
            e += (f64::from(y[i + lag]) - g * f64::from(x[i])).powi(2);
            p += (g * f64::from(x[i])).powi(2);
        }
        (g, 10.0 * (p / e.max(1e-30)).log10())
    }

    /// Best (lag, gain, SNR dB) aligning `y` to an aperiodic `x`.
    fn align(x: &[f32], y: &[f32], max_lag: usize) -> (usize, f64, f64) {
        (0..max_lag).map(|lag| (lag, snr_at(x, y, lag))).map(|(lag, (g, s))| (lag, g, s)).fold((0, 0.0, f64::NEG_INFINITY), |b, c| if c.2 > b.2 { c } else { b })
    }

    fn sine(n: usize, hz: f32, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * hz * i as f32 / 44_100.0).sin()).collect()
    }

    fn interleave(l: &[f32], r: &[f32]) -> Vec<i16> {
        l.iter().zip(r).flat_map(|(&a, &b)| [a.round() as i16, b.round() as i16]).collect()
    }

    fn channel(pcm: &[f32], c: usize) -> Vec<f32> {
        pcm.iter().skip(c).step_by(2).copied().collect()
    }

    /// Unquantised analysis → reference synthesis: unity gain, the documented delay, and an error
    /// floor set by the 48-tap QMF (near-, not perfect, reconstruction).
    #[test]
    fn analysis_synthesis_is_transparent() {
        let x = noise(16 * FRAME, 1);
        let (mut a, mut s) = (Analysis::default(), Synthesis::default());
        let (mut spec, mut out, mut y) = (vec![0.0; FRAME], vec![0.0; FRAME], Vec::new());
        for f in x.as_chunks::<FRAME>().0 {
            a.frame(f, &mut spec);
            s.frame(&spec, &mut out);
            y.extend_from_slice(&out);
        }
        let (lag, g, snr) = align(&x, &y, 3 * FRAME);
        assert_eq!(lag, DELAY);
        assert!((g - 1.0).abs() < 1e-3, "gain {g}");
        assert!(snr > 70.0, "{snr:.1} dB");
    }

    #[test]
    fn qmf_split_is_the_identity_through_the_decoder() {
        // One stage on its own: split then the decoder's merge, 46-sample delay.
        let x = noise(4096, 7);
        let mut q = Qmf::default();
        let (mut lo, mut hi) = (vec![0.0; 2048], vec![0.0; 2048]);
        q.split(&x, &mut lo, &mut hi);
        let mut hist = vec![0.0; 46];
        let mut y = vec![0.0; 4096];
        super::decode::iqmf(&mut hist, &lo, &hi, &mut y);
        let (lag, g, snr) = align(&x, &y, 128);
        assert_eq!(lag, 46);
        assert!((g - 1.0).abs() < 1e-3 && snr > 70.0, "gain {g} snr {snr}");
    }

    /// The whole file through the reference decoder, at every rate: header, size, delay, quality.
    #[test]
    fn bitstream_round_trip() {
        let n = 44_100;
        // A tone and lowpass noise: unlike white noise, both compressible at 66 kbps.
        let mut acc = 0.0;
        let brown: Vec<f32> = noise(n, 3).iter().map(|v| (acc += 0.08 * (v - acc), acc).1).collect();
        let (l, r) = (sine(n, 1000.0, 12_000.0), brown);
        // Floors ~1.5 dB under the measured noise SNR (66: 9.0, 105: 12.5, 132: 13.8); the tone
        // sits at the format's ~35 dB ceiling (mantissas ≤ ±31, no tonal components).
        for (kbps, min_snr) in [(66, 7.5), (105, 11.0), (132, 12.3)] {
            let file = encode(&interleave(&l, &r), kbps, &()).unwrap();
            let block = usize::from(at3_block_align(kbps).unwrap());
            assert!(crate::tools::is_psp_atrac3(&file), "{kbps}");
            assert_eq!(crate::tools::riff_atrac3_block_align(&file), Some(block as u16));
            assert_eq!(file.len(), 60 + (n + DELAY).div_ceil(FRAME) * block);
            assert_eq!(u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize, file.len() - 8);
            assert_eq!(u32::from_le_bytes(file[56..60].try_into().unwrap()) as usize, file.len() - 60);
            assert_eq!(&file[36..52], &[14, 0, 1, 0, 0x10, 0, 0, 0, u8::from(kbps == 66), 0, u8::from(kbps == 66), 0, 1, 0, 0, 0]);
            let y = decode_riff(&file);
            assert_eq!(align(&r, &channel(&y, 1), 2 * FRAME).0, DELAY, "{kbps} kbps");
            for (c, x, floor) in [(0, &l, 33.0), (1, &r, min_snr)] {
                let (g, snr) = snr_at(x, &channel(&y, c), DELAY);
                eprintln!("{kbps} kbps ch{c}: gain {g:.3} snr {snr:.1} dB");
                assert!((0.8..1.1).contains(&g), "{kbps} kbps ch{c}: gain {g}");
                assert!(snr > floor, "{kbps} kbps ch{c}: {snr:.1} dB");
            }
        }
    }

    /// Sound-unit fields: ids, unused features off, and every unit inside its share of the frame.
    #[test]
    fn sound_units_fit_and_parse() {
        let pcm = interleave(&noise(8 * FRAME, 5), &noise(8 * FRAME, 6));
        for kbps in [66, 105, 132] {
            let mut enc = Encoder::new(kbps).unwrap();
            let block = enc.block_align();
            let mut out = Vec::new();
            for f in 0..8 {
                let ch = |c: usize| (0..FRAME).map(|i| f32::from(pcm[(f * FRAME + i) * 2 + c])).collect::<Vec<_>>();
                enc.frame(&ch(0), &ch(1), &mut out);
            }
            assert_eq!(out.len(), 8 * block);
            for frame in out.chunks_exact(block) {
                assert_eq!(frame[0] >> 2, 0x28, "{kbps}: first unit id");
                if kbps == 66 {
                    assert_eq!(frame[block - 1], 0x7F, "weighting 0/7 + M/S matrix codes, byte-reversed");
                } else {
                    assert_eq!(frame[block / 2] >> 2, 0x28, "{kbps}: second unit id");
                }
                // Full-band noise at these rates must code something in every unit.
                let spectra = frame_spectra(frame, kbps == 66);
                assert!(spectra.iter().all(|s| s.iter().any(|&v| v != 0.0)));
            }
        }
    }

    #[test]
    fn silence_and_bad_rates() {
        let file = encode(&vec![0; 2 * 3000], 132, &()).unwrap();
        assert!(decode_riff(&file).iter().all(|&v| v == 0.0));
        assert!(encode(&[0; 4], 64, &()).is_err());
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert!(matches!(encode(&[0; 4096], 66, &cancel), Err(MediaError::Cancelled)));
    }

    /// The fmt chunk of a 66 kbps SND0.AT3 that plays in the XMB of a PSP-3000 (taken from a
    /// working EBOOT), byte for byte; 132 kbps differs only in byte rate, block align and mode.
    #[test]
    fn fmt_chunk_is_the_one_the_xmb_plays() {
        let xmb_66: [u8; 40] = [
            b'f', b'm', b't', b' ', 0x20, 0, 0, 0, 0x70, 0x02, 0x02, 0, 0x44, 0xac, 0, 0, 0x4c, 0x20, 0, 0, 0xc0, 0, 0, 0,
            0x0e, 0, 0x01, 0, 0x10, 0, 0, 0, 0x01, 0, 0x01, 0, 0x01, 0, 0, 0,
        ];
        assert_eq!(riff(&[], 192, true)[12..52], xmb_66);
        let mut xmb_132 = xmb_66;
        xmb_132[16..18].copy_from_slice(&0x4099u16.to_le_bytes());
        xmb_132[20..22].copy_from_slice(&384u16.to_le_bytes());
        xmb_132[32..36].fill(0);
        assert_eq!(riff(&[], 384, false)[12..52], xmb_132);
    }
}
