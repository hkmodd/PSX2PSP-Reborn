//! Test-only reference decoder, written from the format description, used to check the encoder's
//! bitstream field by field and its analysis/synthesis round trip without an external tool.
//! Scope is what the encoder emits: no gain-control points, no tonal components, joint stereo
//! with the M/S matrix (code 3) and unit weights.

use super::tables::{BAND, CLC_BITS, CLC_PAIR, FRAME, MAX_QUANT, SUBBAND, VLC_PAIR, tables};

pub struct BitReader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    pub fn get(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            let bit = self.b.get(self.pos / 8).map_or(0, |&x| (x >> (7 - self.pos % 8)) & 1);
            v = v << 1 | u32::from(bit);
            self.pos += 1;
        }
        v
    }

    fn get_signed(&mut self, n: u32) -> i32 {
        let v = self.get(n) as i32;
        if v >= 1 << (n - 1) { v - (1 << n) } else { v }
    }

    fn vlc(&mut self, sel: usize) -> i32 {
        let t = &tables().vlc[sel - 1];
        let (mut code, mut len) = (0u32, 0u32);
        loop {
            code = code << 1 | self.get(1);
            len += 1;
            if let Some(sym) = t.iter().position(|c| c.len == len && c.bits == code) {
                return sym as i32 - 31;
            }
            assert!(len < 9, "invalid VLC");
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }
}

impl Drop for BitReader<'_> {
    fn drop(&mut self) {
        // A unit that reads past its bytes was mis-sized by the encoder (zeros are read silently).
        assert!(std::thread::panicking() || self.pos <= self.b.len() * 8, "read {} bits from {} bytes", self.pos, self.b.len());
    }
}

/// Decoder-side synthesis: windowed IMDCT + overlap-add per band, then the IQMF tree.
pub struct Synthesis {
    prev: [[f32; BAND]; 4],
    iqmf: [Vec<f32>; 3],
}

impl Default for Synthesis {
    fn default() -> Self {
        Self { prev: [[0.0; BAND]; 4], iqmf: std::array::from_fn(|_| vec![0.0; 46]) }
    }
}

pub fn iqmf(hist: &mut Vec<f32>, lo: &[f32], hi: &[f32], out: &mut [f32]) {
    let w = &tables().qmf;
    let n = lo.len();
    let mut temp = hist.clone();
    for i in 0..n {
        temp.push(lo[i] + hi[i]);
        temp.push(lo[i] - hi[i]);
    }
    for j in 0..n {
        let (mut s1, mut s2) = (0.0, 0.0);
        for k in 0..24 {
            s1 += temp[2 * j + 2 * k] * w[2 * k];
            s2 += temp[2 * j + 2 * k + 1] * w[2 * k + 1];
        }
        out[2 * j] = s2;
        out[2 * j + 1] = s1;
    }
    *hist = temp[2 * n..].to_vec();
}

impl Synthesis {
    /// One frame of spectral lines (s16 units) → 1024 PCM samples.
    pub fn frame(&mut self, spectrum: &[f32], out: &mut [f32]) {
        let t = tables();
        let mut bands = [[0.0f32; BAND]; 4];
        for (b, band) in bands.iter_mut().enumerate() {
            let mut x = spectrum[b * BAND..(b + 1) * BAND].to_vec();
            if b & 1 == 1 {
                x.reverse();
            }
            let mut y = [0.0f32; 2 * BAND];
            for (n, yn) in y.iter_mut().enumerate() {
                let s: f64 = x
                    .iter()
                    .enumerate()
                    .map(|(k, &xk)| f64::from(xk) * (std::f64::consts::PI / 256.0 * (n as f64 + 0.5 + 128.0) * (k as f64 + 0.5)).cos())
                    .sum();
                let (wi, wj) = (t.window[n], t.window[(BAND - 1) - n.min(511 - n)]);
                *yn = s as f32 * wi / (0.5 * (wi * wi + wj * wj));
            }
            for i in 0..BAND {
                band[i] = y[i] + self.prev[b][i];
            }
            self.prev[b].copy_from_slice(&y[BAND..]);
        }
        let (mut low, mut high) = ([0.0f32; 512], [0.0f32; 512]);
        let [h0, h1, h2] = &mut self.iqmf;
        iqmf(h0, &bands[0], &bands[1], &mut low);
        iqmf(h1, &bands[3], &bands[2], &mut high);
        iqmf(h2, &low, &high, out);
    }
}

/// Parsed sound unit: the dequantised spectrum and the fields the tests inspect.
pub struct SoundUnit {
    pub spectrum: Vec<f32>,
    pub bands_coded: u32,
    pub selectors: Vec<u32>,
    pub clc: bool,
}

pub fn sound_unit(r: &mut BitReader<'_>, joint_second: bool) -> SoundUnit {
    if joint_second {
        assert_eq!(r.get(2), 3, "JS sound unit id");
    } else {
        assert_eq!(r.get(6), 0x28, "sound unit id");
    }
    let bands_coded = r.get(2);
    for _ in 0..=bands_coded {
        assert_eq!(r.get(3), 0, "gain points (encoder writes none)");
    }
    assert_eq!(r.get(5), 0, "tonal components (encoder writes none)");
    let n = r.get(5) as usize + 1;
    let clc = r.get(1) == 1;
    let selectors: Vec<u32> = (0..n).map(|_| r.get(3)).collect();
    let sfs: Vec<usize> = selectors.iter().map(|&s| if s > 0 { r.get(6) as usize } else { 0 }).collect();
    let t = tables();
    let mut spectrum = vec![0.0f32; FRAME];
    for s in 0..n {
        let sel = selectors[s] as usize;
        if sel == 0 {
            continue;
        }
        let (first, last) = (SUBBAND[s], SUBBAND[s + 1]);
        let mut m = Vec::with_capacity(last - first);
        while m.len() < last - first {
            match (clc, sel) {
                (true, 1) => {
                    let c = r.get(4) as usize;
                    m.extend([CLC_PAIR[c >> 2], CLC_PAIR[c & 3]]);
                }
                (true, _) => m.push(r.get_signed(CLC_BITS[sel])),
                (false, 1) => {
                    let (a, b) = VLC_PAIR[r.vlc(1) as usize];
                    m.extend([a, b]);
                }
                (false, _) => m.push(r.vlc(sel)),
            }
        }
        let scale = t.sf[sfs[s]] / MAX_QUANT[sel];
        for (o, v) in spectrum[first..last].iter_mut().zip(m) {
            *o = v as f32 * scale;
        }
    }
    SoundUnit { spectrum, bands_coded, selectors, clc }
}

/// Decodes one frame into per-channel spectra (L, R), undoing joint stereo.
pub fn frame_spectra(frame: &[u8], joint: bool) -> [Vec<f32>; 2] {
    if !joint {
        let half = frame.len() / 2;
        let l = sound_unit(&mut BitReader::new(&frame[..half]), false).spectrum;
        let r = sound_unit(&mut BitReader::new(&frame[half..]), false).spectrum;
        return [l, r];
    }
    let c1 = sound_unit(&mut BitReader::new(frame), false).spectrum;
    let rev: Vec<u8> = frame.iter().rev().copied().collect();
    let skip = rev.iter().take_while(|&&b| b == 0xF8).count();
    let mut r = BitReader::new(&rev[skip..]);
    assert_eq!((r.get(1), r.get(3)), (0, 7), "unit channel weights");
    for _ in 0..4 {
        assert_eq!(r.get(2), 3, "M/S matrix");
    }
    let c2 = sound_unit(&mut r, true).spectrum;
    let l = c1.iter().zip(&c2).map(|(a, b)| a + b).collect();
    let rr = c1.iter().zip(&c2).map(|(a, b)| a - b).collect();
    [l, rr]
}

/// Decodes a whole RIFF ATRAC3 file to interleaved stereo PCM (f32, s16 units).
pub fn decode_riff(file: &[u8]) -> Vec<f32> {
    let block = usize::from(u16::from_le_bytes([file[32], file[33]]));
    let joint = u16::from_le_bytes([file[44], file[45]]) == 1;
    let data = &file[60..];
    let mut syn = [Synthesis::default(), Synthesis::default()];
    let mut pcm = Vec::new();
    for frame in data.chunks_exact(block) {
        let spectra = frame_spectra(frame, joint);
        let mut ch = [[0.0f32; FRAME]; 2];
        for c in 0..2 {
            syn[c].frame(&spectra[c], &mut ch[c]);
        }
        let [l, r] = &ch;
        pcm.extend(l.iter().zip(r).flat_map(|(&a, &b)| [a, b]));
    }
    pcm
}
