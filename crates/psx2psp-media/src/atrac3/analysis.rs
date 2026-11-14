//! Time → frequency: the three-stage QMF tree into 4 bands, then a 512-point MDCT per band.
//!
//! Both stages are derived as the exact adjoints of the decoder's synthesis (IQMF and windowed
//! IMDCT), so that analysis followed by the standard decoder reconstructs the input.

use super::tables::{BAND, FRAME, tables};

/// The adjoint split followed by the decoder's IQMF has gain 2 (the window is twice the
/// prototype); halving it makes the pair unity. Checked by `analysis_synthesis_is_transparent`.
const QMF_GAIN: f32 = 0.5;
/// MDCT scale: the decoder's IMDCT is unnormalised (Σ X·cos) and the two windows overlap-add to
/// 2, so a DCT-IV pair (gain N/2 = 128) needs 1/256. The sign follows atracdenc, whose streams
/// are proven on PSP hardware; FFmpeg's decoder renders both polarity-inverted (inaudible, and
/// accounted for by `tests/atrac3_eval.rs`).
const MDCT_SCALE: f32 = 1.0 / 256.0;

/// One two-band QMF split: 2n samples in, n low + n high out.
#[derive(Clone)]
pub struct Qmf {
    hist: [f32; 46],
    buf: Vec<f32>,
}

impl Default for Qmf {
    fn default() -> Self {
        Self { hist: [0.0; 46], buf: Vec::new() }
    }
}

impl Qmf {
    /// The decoder synthesises `y[2j] = Σk v[j+k−23]·W[2k+1]`, `y[2j+1] = Σk u[j+k−23]·W[2k]`
    /// with `u = lo + hi`, `v = lo − hi`; this is its transpose, delayed 23 samples to be causal.
    pub fn split(&mut self, x: &[f32], lo: &mut [f32], hi: &mut [f32]) {
        let n = lo.len();
        debug_assert!(x.len() == 2 * n && hi.len() == n);
        let w = &tables().qmf;
        self.buf.clear();
        self.buf.extend_from_slice(&self.hist);
        self.buf.extend_from_slice(x);
        for m in 0..n {
            let at = 46 + 2 * m;
            let (mut u, mut v) = (0.0f32, 0.0f32);
            for k in 0..24 {
                u += self.buf[at + 1 - 2 * k] * w[2 * k];
                v += self.buf[at - 2 * k] * w[2 * k + 1];
            }
            lo[m] = (u + v) * QMF_GAIN;
            hi[m] = (u - v) * QMF_GAIN;
        }
        self.hist.copy_from_slice(&self.buf[2 * n..]);
    }
}

/// Windowed MDCT of one band: the previous 256 samples and the current 256 → 256 lines.
#[derive(Clone)]
pub struct Mdct {
    prev: [f32; BAND],
}

impl Default for Mdct {
    fn default() -> Self {
        Self { prev: [0.0; BAND] }
    }
}

impl Mdct {
    /// `X[k] = Σn z[n]·a[n]·cos(π/256·(n + ½ + 128)(k + ½)) / 256`, computed as the folded
    /// DCT-IV. `reverse` mirrors the lines, as the decoder does for the odd (spectrally inverted)
    /// QMF bands.
    pub fn forward(&mut self, cur: &[f32], out: &mut [f32], reverse: bool) {
        debug_assert!(cur.len() == BAND && out.len() == BAND);
        let t = tables();
        let z = |i: usize| if i < BAND { self.prev[i] * t.window[i] } else { cur[i - BAND] * t.window[i] };
        // MDCT(a, b, c, d) = DCT-IV(−c_r − d, a − b_r) over the four 128-sample quarters.
        let mut u = [0.0f32; BAND];
        for n in 0..BAND / 2 {
            u[n] = -z(383 - n) - z(384 + n);
            u[BAND / 2 + n] = z(n) - z(255 - n);
        }
        for (k, o) in out.iter_mut().enumerate() {
            let row = &t.dct4[k * BAND..(k + 1) * BAND];
            *o = row.iter().zip(&u).map(|(c, x)| c * x).sum::<f32>() * MDCT_SCALE;
        }
        if reverse {
            out.reverse();
        }
        self.prev.copy_from_slice(cur);
    }
}

/// Per-channel analysis state: PCM frame → 1024 spectral lines (4 bands × 256).
#[derive(Clone, Default)]
pub struct Analysis {
    /// Full-rate split, then the low and high halves.
    qmf: [Qmf; 3],
    mdct: [Mdct; 4],
}

impl Analysis {
    pub fn frame(&mut self, pcm: &[f32], spectrum: &mut [f32]) {
        debug_assert!(pcm.len() == FRAME && spectrum.len() == FRAME);
        let (mut low, mut high) = ([0.0f32; FRAME / 2], [0.0f32; FRAME / 2]);
        self.qmf[0].split(pcm, &mut low, &mut high);
        let mut bands = [[0.0f32; BAND]; 4];
        let [b0, b1, b2, b3] = &mut bands;
        self.qmf[1].split(&low, b0, b1);
        // The decoder rebuilds the high half as iqmf(lo = band 3, hi = band 2).
        self.qmf[2].split(&high, b3, b2);
        for (b, (m, band)) in self.mdct.iter_mut().zip(&bands).enumerate() {
            m.forward(band, &mut spectrum[b * BAND..(b + 1) * BAND], b & 1 == 1);
        }
    }
}
