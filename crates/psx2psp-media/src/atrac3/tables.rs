//! ATRAC3 format constants.
//!
//! Everything here is a property of the bitstream: any conforming decoder (Sony's, the PSP's,
//! FFmpeg's) uses these exact values, so an encoder must too. They are transcribed from the
//! format as documented publicly — the multimedia.cx wiki "RealAudio atrc" page and the tables
//! in FFmpeg's `libavcodec/atrac.c` / `atrac3data.h` — as data, not code. Derived tables (scale
//! factors, windows, canonical Huffman codes) are rebuilt here from their defining formulas.

use std::f64::consts::PI;
use std::sync::OnceLock;

/// Samples per channel per frame.
pub const FRAME: usize = 1024;
/// Samples per QMF band per frame (4 bands).
pub const BAND: usize = 256;

/// First spectral line of each of the 32 quantisation subbands, plus the end (1024).
pub const SUBBAND: [usize; 33] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 288, 320, 352, 384, 416, 448, 480, 512, 576, 640, 704, 768, 896,
    1024,
];

/// Largest quantised level per selector; a mantissa `m` decodes to `m · sf / MAX_QUANT[sel]`.
pub const MAX_QUANT: [f32; 8] = [0.0, 1.5, 2.5, 3.5, 4.5, 7.5, 15.5, 31.5];
/// Mantissa range ±n the encoder emits per selector (the VLC alphabets cover exactly this).
pub const MANT_MAX: [i32; 8] = [0, 1, 2, 3, 4, 7, 15, 31];
/// Constant-length-coding width per selector (selector 1 codes a pair in 4 bits).
pub const CLC_BITS: [u32; 8] = [0, 4, 3, 3, 4, 4, 5, 6];
/// Selector-1 CLC: 2-bit field → mantissa.
pub const CLC_PAIR: [i32; 4] = [0, 1, -2, -1];
/// Selector-1 VLC: symbol index → mantissa pair.
pub const VLC_PAIR: [(i32, i32); 9] = [(0, 0), (0, 1), (0, -1), (1, 0), (-1, 0), (1, 1), (1, -1), (-1, 1), (-1, -1)];

/// One half of the symmetric 48-tap QMF prototype (the window is twice this, mirrored).
pub const QMF_HALF: [f64; 24] = [
    -0.00001461907,
    -0.00009205479,
    -0.000056157569,
    0.00030117269,
    0.0002422519,
    -0.00085293897,
    -0.0005205574,
    0.0020340169,
    0.00078333891,
    -0.0042153862,
    -0.00075614988,
    0.0078402944,
    -0.000061169922,
    -0.01344162,
    0.0024626821,
    0.021736089,
    -0.007801671,
    -0.034090221,
    0.01880949,
    0.054326009,
    -0.043596379,
    -0.099384367,
    0.13207909,
    0.46424159,
];

/// Huffman alphabets for selectors 1..=7 as (symbol, code length) in canonical order: codes are
/// assigned in this order, counting up. Symbol − 31 is the mantissa (selectors 2..=7) or the
/// [`VLC_PAIR`] index (selector 1).
const HUFF: [&[(u8, u8)]; 7] = [
    &[(31, 1), (32, 3), (33, 3), (34, 4), (35, 4), (36, 5), (37, 5), (38, 5), (39, 5)],
    &[(31, 1), (32, 3), (30, 3), (33, 3), (29, 3)],
    &[(31, 1), (32, 3), (30, 3), (33, 4), (29, 4), (34, 4), (28, 4)],
    &[(31, 1), (32, 3), (30, 3), (33, 4), (29, 4), (34, 5), (28, 5), (35, 5), (27, 5)],
    &[(31, 2), (32, 3), (30, 3), (33, 4), (29, 4), (34, 4), (28, 4), (38, 4), (24, 4), (35, 5), (27, 5), (36, 6), (26, 6), (37, 6), (25, 6)],
    &[
        (31, 3), (32, 4), (30, 4), (33, 4), (29, 4), (34, 4), (28, 4), (46, 4), (16, 4), (35, 5), (27, 5), (36, 5), (26, 5), (37, 5), (25, 5), (38, 6),
        (24, 6), (39, 6), (23, 6), (40, 6), (22, 6), (41, 6), (21, 6), (42, 7), (20, 7), (43, 7), (19, 7), (44, 7), (18, 7), (45, 7), (17, 7),
    ],
    &[
        (31, 3), (62, 4), (0, 4), (32, 5), (30, 5), (33, 5), (29, 5), (34, 5), (28, 5), (35, 5), (27, 5), (36, 5), (26, 5), (37, 6), (25, 6), (38, 6),
        (24, 6), (39, 6), (23, 6), (40, 6), (22, 6), (41, 6), (21, 6), (42, 6), (20, 6), (43, 6), (19, 6), (44, 6), (18, 6), (45, 7), (17, 7), (46, 7),
        (16, 7), (47, 7), (15, 7), (48, 7), (14, 7), (49, 7), (13, 7), (50, 7), (12, 7), (51, 7), (11, 7), (52, 8), (10, 8), (53, 8), (9, 8), (54, 8),
        (8, 8), (55, 8), (7, 8), (56, 8), (6, 8), (57, 8), (5, 8), (58, 8), (4, 8), (59, 8), (3, 8), (60, 8), (2, 8), (61, 8), (1, 8),
    ],
];

/// A Huffman codeword, right-aligned. `len == 0` marks a symbol outside the alphabet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Code {
    pub bits: u32,
    pub len: u32,
}

pub struct Tables {
    /// `2^((i − 15) / 3)`, i in 0..64.
    pub sf: [f32; 64],
    /// 48-tap QMF window.
    pub qmf: [f32; 48],
    /// Analysis window `1 + sin(((n + ½)/256 − ½)·π)`, mirrored; its product with the decoder's
    /// synthesis window overlap-adds to exactly 2.
    pub window: [f32; 2 * BAND],
    /// `cos(π/256 · (n + ½)(k + ½))`, row-major by k: the DCT-IV kernel.
    pub dct4: Vec<f32>,
    /// `vlc[sel − 1][symbol]`, symbol = value + 31 (pair index + 31 for selector 1).
    pub vlc: [[Code; 63]; 7],
}

pub fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let sf = std::array::from_fn(|i| 2f64.powf((i as f64 - 15.0) / 3.0) as f32);
        let qmf = std::array::from_fn(|i| (2.0 * QMF_HALF[i.min(47 - i)]) as f32);
        let window = std::array::from_fn(|i| {
            let n = i.min(2 * BAND - 1 - i) as f64;
            (1.0 + (((n + 0.5) / BAND as f64 - 0.5) * PI).sin()) as f32
        });
        let dct4 = (0..BAND * BAND)
            .map(|i| {
                let (k, n) = ((i / BAND) as f64, (i % BAND) as f64);
                (PI / BAND as f64 * (n + 0.5) * (k + 0.5)).cos() as f32
            })
            .collect();
        let mut vlc = [[Code::default(); 63]; 7];
        for (table, alphabet) in vlc.iter_mut().zip(HUFF) {
            // Canonical assignment, left-aligned in 32 bits: each code takes the next 2^(32−len)
            // slice of the code space.
            let mut next = 0u64;
            for &(sym, len) in alphabet {
                let len = u32::from(len);
                table[usize::from(sym)] = Code { bits: (next >> (32 - len)) as u32, len };
                next += 1 << (32 - len);
            }
            debug_assert_eq!(next, 1 << 32, "complete prefix code");
        }
        Tables { sf, qmf, window, dct4, vlc }
    })
}

/// QMF band (0..4) holding spectral line `line`.
pub const fn band_of(line: usize) -> usize {
    line / BAND
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huffman_alphabets_are_complete_prefix_codes() {
        let t = tables();
        for (s, alphabet) in HUFF.iter().enumerate() {
            let kraft: f64 = alphabet.iter().map(|&(_, l)| 0.5f64.powi(i32::from(l))).sum();
            assert!((kraft - 1.0).abs() < 1e-12, "selector {}", s + 1);
            let codes: Vec<Code> = alphabet.iter().map(|&(sym, _)| t.vlc[s][usize::from(sym)]).collect();
            for (i, a) in codes.iter().enumerate() {
                for b in &codes[i + 1..] {
                    let (short, long) = if a.len <= b.len { (a, b) } else { (b, a) };
                    assert_ne!(long.bits >> (long.len - short.len), short.bits, "selector {}: prefix clash", s + 1);
                }
            }
        }
        // Spot checks of the canonical assignment.
        assert_eq!(t.vlc[0][31], Code { bits: 0b0, len: 1 });
        assert_eq!(t.vlc[0][32], Code { bits: 0b100, len: 3 });
        assert_eq!(t.vlc[0][39], Code { bits: 0b11111, len: 5 });
        assert_eq!(t.vlc[6][1], Code { bits: 0xFF, len: 8 });
    }

    #[test]
    fn alphabets_cover_the_mantissa_ranges() {
        let t = tables();
        for (sel, &m) in MANT_MAX.iter().enumerate().skip(2) {
            for v in -m..=m {
                assert!(t.vlc[sel - 1][(v + 31) as usize].len > 0, "selector {sel} value {v}");
            }
        }
        for i in 0..9 {
            assert!(t.vlc[0][31 + i].len > 0);
        }
    }

    #[test]
    fn windows_overlap_add_to_two() {
        let t = tables();
        for n in 0..BAND {
            let j = BAND - 1 - n;
            let (wi, wj) = (f64::from(t.window[n]), f64::from(t.window[j]));
            let syn = |a: f64| a / (0.5 * (wi * wi + wj * wj));
            let sum = wi * syn(wi) + wj * syn(wj);
            assert!((sum - 2.0).abs() < 1e-6);
        }
        assert_eq!(t.sf[15], 1.0);
        assert!((t.sf[63] - 65536.0).abs() < 0.5);
        assert_eq!(band_of(255), 0);
        assert_eq!(SUBBAND[32], FRAME);
    }
}
