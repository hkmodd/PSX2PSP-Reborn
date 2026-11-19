//! Spectral quantisation, bit allocation and sound-unit packing.
//!
//! Every subband is quantised up front at each of the 7 selectors (word lengths), recording the
//! squared error and the exact cost in bits. Allocation is then a greedy rate–distortion descent:
//! repeatedly take the upgrade with the best squared-error drop per bit that still fits the
//! frame, counting the side information it drags in (selector fields up to that subband, gain
//! fields of a newly coded QMF band, the scale factor). It is run once with Huffman costs and
//! once with constant-length costs; the sound unit keeps whichever ends with less distortion.

use super::bits::BitWriter;
use super::tables::{CLC_BITS, CLC_PAIR, MANT_MAX, MAX_QUANT, SUBBAND, VLC_PAIR, band_of, tables};

/// Fixed bits of a sound unit: id 6, bands 2, tonal count 5, subband count 5, coding mode 1.
pub const SU_FIXED: u32 = 19;
/// Second joint-stereo unit: 2-bit id instead of 6, plus weighting (1 + 3) and matrix (4 × 2).
pub const SU_JOINT_FIXED: u32 = SU_FIXED - 4 + 12;

#[derive(Clone, Copy, Default)]
struct Choice {
    /// Squared error with this selector (the subband's energy for selector 0).
    dist: f32,
    /// Bits with Huffman / constant-length mantissas, scale factor included.
    vlc: u32,
    clc: u32,
    sf: u8,
}

/// One subband's quantisation options.
struct Subband {
    choice: [Choice; 8],
}

/// Quantises `x` at `(sel, sf)` into `m`, returning the squared error.
fn quantize(x: &[f32], sel: usize, sf: usize, m: &mut [i32]) -> f32 {
    let s = tables().sf[sf];
    let (q, step, max) = (MAX_QUANT[sel] / s, s / MAX_QUANT[sel], MANT_MAX[sel]);
    let mut d = 0.0;
    for (o, &v) in m.iter_mut().zip(x) {
        *o = ((v * q).round() as i32).clamp(-max, max);
        let e = v - *o as f32 * step;
        d += e * e;
    }
    d
}

fn pair_index(a: i32, b: i32) -> usize {
    VLC_PAIR.iter().position(|&p| p == (a, b)).expect("selector-1 mantissas are within ±1")
}

fn clc_pair(v: i32) -> u32 {
    CLC_PAIR.iter().position(|&p| p == v).expect("selector-1 mantissas are within ±1") as u32
}

/// Huffman cost of mantissas `m` at selector `sel`.
fn vlc_bits(m: &[i32], sel: usize) -> u32 {
    let t = &tables().vlc[sel - 1];
    if sel == 1 { m.as_chunks::<2>().0.iter().map(|p| t[31 + pair_index(p[0], p[1])].len).sum() } else { m.iter().map(|&v| t[(v + 31) as usize].len).sum() }
}

fn clc_bits(len: usize, sel: usize) -> u32 {
    if sel == 1 { len as u32 / 2 * CLC_BITS[1] } else { len as u32 * CLC_BITS[sel] }
}

/// Smallest scale-factor index whose value covers `peak`.
fn sf_for(peak: f32) -> usize {
    tables().sf.partition_point(|&s| s < peak).min(63)
}

fn analyse(spectrum: &[f32]) -> Vec<Subband> {
    let mut m = [0i32; 128];
    (0..32)
        .map(|s| {
            let x = &spectrum[SUBBAND[s]..SUBBAND[s + 1]];
            let m = &mut m[..x.len()];
            let energy: f32 = x.iter().map(|v| v * v).sum();
            let mut choice = [Choice { dist: energy, ..Choice::default() }; 8];
            let peak = x.iter().fold(0.0f32, |a, v| a.max(v.abs()));
            // Below one LSB of output every option costs bits for nothing.
            if peak >= 0.5 {
                let top = sf_for(peak);
                for (sel, c) in choice.iter_mut().enumerate().skip(1) {
                    let mut best: Option<(f32, usize)> = None;
                    for sf in top.saturating_sub(2)..=top {
                        let d = quantize(x, sel, sf, m);
                        if best.is_none_or(|(bd, _)| d < bd) {
                            best = Some((d, sf));
                        }
                    }
                    let (dist, sf) = best.expect("at least one candidate");
                    quantize(x, sel, sf, m);
                    *c = Choice { dist, vlc: 6 + vlc_bits(m, sel), clc: 6 + clc_bits(x.len(), sel), sf: sf as u8 };
                }
            }
            Subband { choice }
        })
        .collect()
}

/// Side information that depends on how many subbands are coded: 3 bits per selector field and
/// 3 bits of gain-point count per QMF band up to the last coded one.
fn side_bits(coded: usize) -> u32 {
    3 * coded as u32 + 3 * (band_of(SUBBAND[coded] - 1) as u32 + 1)
}

/// The allocation for one sound unit.
#[derive(Clone)]
pub struct Plan {
    sel: [u8; 32],
    sf: [u8; 32],
    vlc: bool,
}

impl Plan {
    fn coded(&self) -> usize {
        self.sel.iter().rposition(|&s| s > 0).map_or(1, |i| i + 1)
    }
}

/// Greedy allocation of `budget` bits across the subbands of one or more sound units (several
/// when they share a frame, as the two halves of joint stereo do). Returns the plans and the
/// remaining distortion. Plain squared error is the right target for joint stereo too: with
/// L = M + S and R = M − S, the L/R error energy is twice the M plus S error energy.
fn greedy(units: &[Vec<Subband>], fixed: &[u32], budget: u32, vlc: bool) -> (Vec<Plan>, f32) {
    let bits = |c: &Choice| i64::from(if vlc { c.vlc } else { c.clc });
    let budget = i64::from(budget);
    let mut sel = vec![[0u8; 32]; units.len()];
    let mut coded = vec![1usize; units.len()];
    let mut used: i64 = fixed.iter().map(|f| i64::from(f + side_bits(1))).sum();
    if used > budget {
        return (Vec::new(), f32::INFINITY);
    }
    loop {
        let mut best: Option<(f32, usize, usize, u8, i64)> = None;
        for (u, subbands) in units.iter().enumerate() {
            for (s, sb) in subbands.iter().enumerate() {
                let cur = &sb.choice[usize::from(sel[u][s])];
                let side = if s + 1 > coded[u] { i64::from(side_bits(s + 1) - side_bits(coded[u])) } else { 0 };
                for ns in sel[u][s] + 1..8 {
                    let next = &sb.choice[usize::from(ns)];
                    let gain = cur.dist - next.dist;
                    // A finer selector can cost fewer Huffman bits: then it is a free win.
                    let cost = bits(next) + side - bits(cur);
                    if gain <= 0.0 || used + cost > budget {
                        continue;
                    }
                    let ratio = if cost <= 0 { f32::INFINITY } else { gain / cost as f32 };
                    if best.is_none_or(|b| ratio > b.0) {
                        best = Some((ratio, u, s, ns, cost));
                    }
                }
            }
        }
        let Some((_, u, s, ns, cost)) = best else { break };
        sel[u][s] = ns;
        coded[u] = coded[u].max(s + 1);
        used += cost;
    }
    let mut dist = 0.0;
    let plans = units
        .iter()
        .zip(&sel)
        .map(|(subbands, sel)| {
            let mut sf = [0u8; 32];
            for (s, sb) in subbands.iter().enumerate() {
                let c = &sb.choice[usize::from(sel[s])];
                dist += c.dist;
                sf[s] = c.sf;
            }
            Plan { sel: *sel, sf, vlc }
        })
        .collect();
    (plans, dist)
}

/// Allocates the units sharing `budget` bits, trying Huffman and constant-length coding.
pub fn allocate(spectra: &[&[f32]], fixed: &[u32], budget: u32) -> Vec<Plan> {
    let units: Vec<Vec<Subband>> = spectra.iter().map(|s| analyse(s)).collect();
    let (a, da) = greedy(&units, fixed, budget, true);
    let (b, db) = greedy(&units, fixed, budget, false);
    assert!(da.is_finite() || db.is_finite(), "frame budget below the fixed side information");
    if da <= db { a } else { b }
}

/// Writes one sound unit. `joint_second` selects the 2-bit id of the second joint-stereo unit.
pub fn write_unit(w: &mut BitWriter, spectrum: &[f32], plan: &Plan, joint_second: bool) {
    let coded = plan.coded();
    let bands_coded = band_of(SUBBAND[coded] - 1) as u32;
    if joint_second {
        w.put(3, 2);
    } else {
        w.put(0x28, 6);
    }
    w.put(bands_coded, 2);
    for _ in 0..=bands_coded {
        w.put(0, 3); // no gain-control points
    }
    w.put(0, 5); // no tonal components
    w.put(coded as u32 - 1, 5);
    w.put(u32::from(!plan.vlc), 1);
    for &s in &plan.sel[..coded] {
        w.put(u32::from(s), 3);
    }
    for s in 0..coded {
        if plan.sel[s] > 0 {
            w.put(u32::from(plan.sf[s]), 6);
        }
    }
    let t = tables();
    let mut m = [0i32; 128];
    for s in 0..coded {
        let sel = usize::from(plan.sel[s]);
        if sel == 0 {
            continue;
        }
        let x = &spectrum[SUBBAND[s]..SUBBAND[s + 1]];
        let m = &mut m[..x.len()];
        quantize(x, sel, usize::from(plan.sf[s]), m);
        match (plan.vlc, sel) {
            (true, 1) => {
                for p in m.as_chunks::<2>().0 {
                    let c = t.vlc[0][31 + pair_index(p[0], p[1])];
                    w.put(c.bits, c.len);
                }
            }
            (true, _) => {
                for &v in m.iter() {
                    let c = t.vlc[sel - 1][(v + 31) as usize];
                    w.put(c.bits, c.len);
                }
            }
            (false, 1) => {
                for p in m.as_chunks::<2>().0 {
                    w.put(clc_pair(p[0]) << 2 | clc_pair(p[1]), 4);
                }
            }
            (false, _) => {
                for &v in m.iter() {
                    w.put_signed(v, CLC_BITS[sel]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::decode::{BitReader, sound_unit};
    use super::*;

    /// Spectrum with every kind of subband: loud, quiet, sub-LSB and silent.
    fn spectrum(seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..1024)
            .map(|i| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let r = (s >> 8) as f32 / (1 << 24) as f32 - 0.5;
                r * match i {
                    0..64 => 30_000.0,
                    64..512 => 900.0 / (1.0 + i as f32 / 64.0),
                    512..768 => 0.4,
                    _ => 0.0,
                }
            })
            .collect()
    }

    /// Every unit, in both mantissa codings and both unit kinds, parses back to exactly the
    /// encoder's quantised values, with the exact bit count the allocator budgeted.
    #[test]
    fn units_decode_to_the_quantised_spectrum() {
        for (seed, budget) in [(1, 1216), (2, 1536), (3, 400), (4, 120)] {
            let x = spectrum(seed);
            let plan = allocate(&[&x], &[SU_FIXED], budget).remove(0);
            for vlc in [true, false] {
                for joint_second in [false, true] {
                    let p = Plan { vlc, ..plan.clone() };
                    let mut w = BitWriter::default();
                    write_unit(&mut w, &x, &p, joint_second);
                    let bits = w.bits();
                    let bytes = w.into_bytes();
                    let mut r = BitReader::new(&bytes);
                    let su = sound_unit(&mut r, joint_second);
                    assert_eq!(r.pos(), bits, "decoder consumed exactly the unit");
                    assert_eq!(su.clc, !vlc);
                    let coded = p.coded();
                    assert_eq!(su.selectors, p.sel[..coded].iter().map(|&s| u32::from(s)).collect::<Vec<_>>());
                    assert_eq!(su.bands_coded as usize, band_of(SUBBAND[coded] - 1));
                    let t = tables();
                    for s in 0..32 {
                        let sel = usize::from(p.sel[s]);
                        let range = SUBBAND[s]..SUBBAND[s + 1];
                        let mut m = vec![0; range.len()];
                        if sel > 0 {
                            quantize(&x[range.clone()], sel, usize::from(p.sf[s]), &mut m);
                        }
                        let step = if sel > 0 { t.sf[usize::from(p.sf[s])] / MAX_QUANT[sel] } else { 0.0 };
                        for (got, q) in su.spectrum[range].iter().zip(&m) {
                            assert_eq!(*got, *q as f32 * step, "seed {seed} subband {s}");
                        }
                    }
                    if vlc == plan.vlc && !joint_second {
                        assert!(bits <= budget as usize, "{bits} > {budget}");
                    }
                }
            }
        }
    }

    #[test]
    fn allocation_spends_the_budget_where_the_energy_is() {
        let x = spectrum(9);
        let plan = allocate(&[&x], &[SU_FIXED], 1216).remove(0);
        assert!(plan.sel[..8].iter().all(|&s| s >= 3), "loud low subbands get fine steps: {:?}", plan.sel);
        assert!(plan.sel[26..].iter().all(|&s| s == 0), "sub-LSB and silent subbands cost nothing: {:?}", plan.sel);
        // Two units sharing one budget: the louder one takes the larger share.
        let quiet: Vec<f32> = x.iter().map(|v| v * 0.01).collect();
        let plans = allocate(&[&x, &quiet], &[SU_FIXED, SU_JOINT_FIXED], 1522);
        let bits = |i: usize, p: &Plan, s: &[f32]| {
            let mut w = BitWriter::default();
            write_unit(&mut w, s, p, i == 1);
            w.bits()
        };
        let (a, b) = (bits(0, &plans[0], &x), bits(1, &plans[1], &quiet));
        assert!(a > b && a + b <= 1522, "{a} + {b}");
    }
}
