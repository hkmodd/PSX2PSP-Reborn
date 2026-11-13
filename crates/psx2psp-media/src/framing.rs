//! Content-aware framing: how any picture becomes an exact XMB slot without cutting what matters.
//!
//! 1. **Trim.** Uniform borders (letterbox/pillarbox bars, scan margins, transparent margins) are
//!    not content: they go, at most 45 % per side, measured at full resolution.
//! 2. **Saliency.** On a ≤256 px proxy every pixel gets an importance: colour distinctness
//!    (frequency-tuned saliency, Achanta et al. 2009: CIELAB distance from the image mean) plus
//!    edge energy (Sobel on L*), smoothed. Logos, text, faces and the subject score high; sky,
//!    flat backdrops and gradients low.
//! 3. **Search.** Every window with the slot's aspect between *cover* (fills the slot, crops) and
//!    *contain* (shows everything, pads), at every position along the free axis. Score = saliency
//!    kept − saliency the XMB's own UI would hide (ICON0 over PIC1) − cost of the padding (cheap
//!    when the edge is a flat colour, dearer when it needs an ambient backdrop) − saliency on the
//!    cut lines (never slice through a logo). The best window wins; ties go to the centre.
//! 4. **Render.** Crop, resample in linear light with premultiplied alpha (Lanczos3 down,
//!    Catmull-Rom up) and fill any padding with the edge colour, transparency (sources with alpha,
//!    PIC0), or an ambient backdrop: the picture itself, blurred and dimmed, under a soft shadow.

use image::imageops::{self, FilterType};
use image::{Rgba, Rgba32FImage, RgbaImage};

use crate::images::Slot;

/// A rectangle in pixels (fractional).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    fn intersect(&self, o: &Rect) -> Option<Rect> {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = ((self.x + self.w).min(o.x + o.w), (self.y + self.h).min(o.y + o.h));
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

/// What fills the part of the slot the picture does not cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// The picture covers the whole slot.
    None,
    Transparent,
    /// The picture's own flat edge colour, continued.
    Solid([u8; 4]),
    /// The picture itself, blurred and dimmed, behind it.
    Ambient,
}

/// How a source picture sits in a slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Framing {
    /// The part of the source that is shown, in source pixels.
    pub crop: Rect,
    /// Where it lands in the slot, in slot pixels; the rest of the slot is `fill`.
    pub dest: Rect,
    pub fill: Fill,
}

impl Framing {
    /// Where a source point lands in the slot (outside it when cropped away).
    pub fn map(&self, x: f32, y: f32) -> (f32, f32) {
        (self.dest.x + (x - self.crop.x) * self.dest.w / self.crop.w, self.dest.y + (y - self.crop.y) * self.dest.h / self.crop.h)
    }
}

/// Longest side of the saliency proxy.
const PROXY: u32 = 256;
/// Window sizes tried between cover and contain.
const STEPS: u32 = 32;
/// Trim tolerance: largest channel difference from the border colour, 0–255.
const TRIM_TOL: i32 = 12;
/// At most this fraction of each side is trimmed.
const TRIM_MAX: f32 = 0.45;
/// Weight of above-average saliency on the cut lines.
const CUT_COST: f32 = 0.04;
/// Tie-break toward the centre.
const CENTRE_COST: f32 = 0.004;
/// Padding cost per unit of padded slot area, rising with the padded share (`PAD_GROWTH`): a slim
/// band reads as a frame, half a screen of backdrop as a picture that does not fit.
const SOLID_COST: f32 = 0.12;
const AMBIENT_COST: f32 = 0.70;
const ALPHA_COST: f32 = 0.25;
const PAD_GROWTH: f32 = 2.0;
/// Weight of the important part (saliency above the mean) next to all saliency: ordinary content
/// trades against padding, a whole logo or face never does.
const KEY_WEIGHT: f32 = 1.5;
/// Fixed cost of any padding at all, per fill: a band is a visible seam however thin, so a sliver
/// of padding loses to cropping an equally thin sliver of picture.
const SOLID_SEAM: f32 = 0.02;
const AMBIENT_SEAM: f32 = 0.03;
const ALPHA_SEAM: f32 = 0.02;

/// Slot areas the XMB covers with its own UI, with how much they hide (see `xmb.wgsl`: ICON0 at
/// (20, 96), PIC0 at (160, 46) over PIC1). PIC0 is mostly transparent text, so it weighs little.
fn occluders(slot: Slot) -> &'static [(Rect, f32)] {
    const PIC1: [(Rect, f32); 2] = [(Rect::new(20.0, 96.0, 144.0, 80.0), 0.8), (Rect::new(160.0, 46.0, 310.0, 180.0), 0.15)];
    match slot {
        Slot::Pic1 => &PIC1,
        _ => &[],
    }
}

/// Plans how `src` fills `slot`.
pub fn plan(slot: Slot, src: &RgbaImage) -> Framing {
    let (tw, th) = slot.size();
    let (sw, sh) = src.dimensions();
    let full = Rect::new(0.0, 0.0, sw as f32, sh as f32);
    if sw < 2 || sh < 2 {
        return contain(slot, full, Fill::Transparent);
    }
    let t = trim(src);
    let (x0, y0, x1, y1) = t.bounds;
    let (cw, ch) = (x1 - x0, y1 - y0);
    let content = if (x0, y0, cw, ch) == (0, 0, sw, sh) { None } else { Some(imageops::crop_imm(src, x0, y0, cw, ch).to_image()) };
    let content = content.as_ref().unwrap_or(src);
    let k = (PROXY as f32 / cw.max(ch) as f32).min(1.0);
    let (pw, ph) = (((cw as f32 * k).round() as u32).max(1), ((ch as f32 * k).round() as u32).max(1));
    let proxy = imageops::thumbnail(content, pw, ph);
    let s = saliency(&proxy);
    let mean = s.iter().sum::<f32>() / s.len() as f32;
    let (pwu, phu) = (pw as usize, ph as usize);
    let sal = Sat::new(&s, pwu, phu);
    // The important part: saliency above the mean (the logo, the title, the face), valued on its
    // own scale so dropping a whole object costs the same whatever its area.
    let key = Sat::new(&s.iter().map(|v| (v - mean).max(0.0)).collect::<Vec<_>>(), pwu, phu);

    let (cwf, chf) = (cw as f32, ch as f32);
    let aspect = tw as f32 / th as f32;
    // Content wider than the slot: cover crops the sides, contain pads top and bottom.
    let pad_vertical = cwf / chf > aspect;
    let fill = pad_fill(slot, src, content, &t, pad_vertical);
    let (fill_cost, seam_cost) = match fill {
        Fill::Transparent if slot == Slot::Pic0 => (0.0, 0.0),
        Fill::Transparent => (ALPHA_COST, ALPHA_SEAM),
        Fill::Solid(_) => (SOLID_COST, SOLID_SEAM),
        _ => (AMBIENT_COST, AMBIENT_SEAM),
    };
    let cover = if pad_vertical { (chf * aspect, chf) } else { (cwf, cwf / aspect) };
    let ratio = if pad_vertical { cwf / cover.0 } else { chf / cover.1 };
    let (sx, sy) = (pw as f32 / cwf, ph as f32 / chf);
    let cells = |r: &Rect| {
        let (ax, ay) = (((r.x * sx).round() as usize).min(pwu), ((r.y * sy).round() as usize).min(phu));
        let (bx, by) = ((((r.x + r.w) * sx).round() as usize).min(pwu), (((r.y + r.h) * sy).round() as usize).min(phu));
        (ax, ay, bx, by)
    };
    let total = sal.sum(0, 0, pwu, phu).max(1e-9);
    let key_total = key.sum(0, 0, pwu, phu);
    // Kept value: share of all saliency plus `KEY_WEIGHT` × share of the important part.
    let mass = |r: &Rect| {
        let (ax, ay, bx, by) = cells(r);
        let k = if key_total > 1e-6 { key.sum(ax, ay, bx, by) / key_total } else { 0.0 };
        sal.sum(ax, ay, bx, by) / total + KEY_WEIGHT as f64 * k
    };
    let density = total / (pw * ph) as f64;
    // Saliency on a 2-proxy-pixel band around a cut line above the image mean (cutting through
    // ordinary content is what any crop does; slicing a logo or a face is what costs).
    let band = |r: Rect| {
        let (ax, ay, bx, by) = cells(&r);
        (sal.sum(ax, ay, bx, by) / ((bx.saturating_sub(ax)).max(1) * (by.saturating_sub(ay)).max(1)) as f64 / density - 1.0).max(0.0)
    };

    let mut best: Option<(f64, Rect, (f32, f32))> = None;
    for i in 0..=STEPS {
        let f = ratio.powf(i as f32 / STEPS as f32);
        let win = (cover.0 * f, cover.1 * f);
        let vis = (win.0.min(cwf), win.1.min(chf));
        let pad = 1.0 - (vis.0 * vis.1) / (win.0 * win.1);
        let (free_x, free_y) = (cwf - vis.0, chf - vis.1);
        let (nx, ny) = ((free_x * sx).ceil().max(0.0) as u32, (free_y * sy).ceil().max(0.0) as u32);
        for jx in 0..=nx {
            for jy in 0..=ny {
                let x = if nx == 0 { free_x / 2.0 } else { free_x * jx as f32 / nx as f32 };
                let y = if ny == 0 { free_y / 2.0 } else { free_y * jy as f32 / ny as f32 };
                let v = Rect::new(x, y, vis.0, vis.1);
                let mut score = mass(&v);
                let (ox, oy, s) = (x - (win.0 - vis.0) / 2.0, y - (win.1 - vis.1) / 2.0, win.0 / tw as f32);
                for (o, w) in occluders(slot) {
                    if let Some(r) = Rect::new(ox + o.x * s, oy + o.y * s, o.w * s, o.h * s).intersect(&v) {
                        score -= mass(&r) * *w as f64;
                    }
                }
                let (bw, bh) = (2.0 / sx, 2.0 / sy);
                let mut cut = 0.0;
                if x > 0.5 {
                    cut += band(Rect::new(x - bw / 2.0, y, bw, vis.1));
                }
                if x + vis.0 < cwf - 0.5 {
                    cut += band(Rect::new(x + vis.0 - bw / 2.0, y, bw, vis.1));
                }
                if y > 0.5 {
                    cut += band(Rect::new(x, y - bh / 2.0, vis.0, bh));
                }
                if y + vis.1 < chf - 0.5 {
                    cut += band(Rect::new(x, y + vis.1 - bh / 2.0, vis.0, bh));
                }
                let off = ((x + vis.0 / 2.0 - cwf / 2.0).abs() / cwf + (y + vis.1 / 2.0 - chf / 2.0).abs() / chf) as f64;
                // Padded slot pixels across the padded axis.
                let band_px = pad * if pad_vertical { th as f32 } else { tw as f32 };
                let seam = if band_px >= 0.5 { seam_cost } else { 0.0 };
                score -= (pad * fill_cost * (1.0 + PAD_GROWTH * pad) + seam) as f64 + CUT_COST as f64 * cut + CENTRE_COST as f64 * off;
                if best.as_ref().is_none_or(|b| score > b.0 + 1e-9) {
                    best = Some((score, v, win));
                }
            }
        }
    }
    let (_, v, win) = best.expect("at least one window");
    let s = tw as f32 / win.0;
    let dest = Rect::new((win.0 - v.w) / 2.0 * s, (win.1 - v.h) / 2.0 * s, v.w * s, v.h * s);
    let padded = dest.w < tw as f32 - 0.5 || dest.h < th as f32 - 0.5;
    Framing { crop: Rect::new(x0 as f32 + v.x, y0 as f32 + v.y, v.w, v.h), dest, fill: if padded { fill } else { Fill::None } }
}

/// The whole of `r`, letterboxed into the slot.
fn contain(slot: Slot, r: Rect, fill: Fill) -> Framing {
    let (tw, th) = slot.size();
    let s = (tw as f32 / r.w).min(th as f32 / r.h);
    let (dw, dh) = (r.w * s, r.h * s);
    Framing { crop: r, dest: Rect::new((tw as f32 - dw) / 2.0, (th as f32 - dh) / 2.0, dw, dh), fill }
}

/// How padding is filled for this slot and picture: the page colour the trim removed from the
/// padded sides, else the content's own edge colour when it is flat (judged at full resolution, so
/// film grain or a fine texture is not mistaken for a flat colour), else the ambient backdrop.
fn pad_fill(slot: Slot, src: &RgbaImage, content: &RgbaImage, t: &Trimmed, pad_vertical: bool) -> Fill {
    if slot == Slot::Pic0 || (slot == Slot::Icon0 && has_alpha(src)) {
        return Fill::Transparent;
    }
    let solid = |a: [f32; 4], b: [f32; 4]| {
        ((0..4).all(|c| (a[c] - b[c]).abs() <= 0.08) && a[3] > 0.95 && b[3] > 0.95).then(|| Fill::Solid(std::array::from_fn(|c| ((a[c] + b[c]) / 2.0 * 255.0).round() as u8)))
    };
    let unit = |p: [u8; 4]| p.map(|c| c as f32 / 255.0);
    let (w, h) = src.dimensions();
    let (x0, y0, x1, y1) = t.bounds;
    let trimmed = if pad_vertical { y0 > 0 && y1 < h } else { x0 > 0 && x1 < w };
    if trimmed {
        let (a, b) = if pad_vertical { (t.refs[0], t.refs[1]) } else { (t.refs[2], t.refs[3]) };
        if let Some(f) = solid(unit(a), unit(b)) {
            return f;
        }
    }
    let (w, h) = content.dimensions();
    let strip = |x0: u32, y0: u32, sw: u32, sh: u32| {
        let px: Vec<[f32; 4]> = (y0..y0 + sh).flat_map(|y| (x0..x0 + sw).map(move |x| (x, y))).map(|(x, y)| unit(content.get_pixel(x, y).0)).collect();
        let n = px.len() as f32;
        let mean: [f32; 4] = std::array::from_fn(|c| px.iter().map(|p| p[c]).sum::<f32>() / n);
        let sd = (0..4).map(|c| (px.iter().map(|p| (p[c] - mean[c]).powi(2)).sum::<f32>() / n).sqrt()).fold(0.0, f32::max);
        (mean, sd)
    };
    let (a, b) = if pad_vertical {
        let k = (h / 100).clamp(1, h);
        (strip(0, 0, w, k), strip(0, h - k, w, k))
    } else {
        let k = (w / 100).clamp(1, w);
        (strip(0, 0, k, h), strip(w - k, 0, k, h))
    };
    if a.1 <= 0.035
        && b.1 <= 0.035
        && let Some(f) = solid(a.0, b.0)
    {
        return f;
    }
    Fill::Ambient
}

/// More than 1 % of the pixels see through.
fn has_alpha(src: &RgbaImage) -> bool {
    let n = src.pixels().filter(|p| p[3] < 250).count();
    n * 100 > (src.width() * src.height()) as usize
}

/// The content box without uniform borders.
struct Trimmed {
    /// `(x0, y0, x1, y1)`.
    bounds: (u32, u32, u32, u32),
    /// Border colours: top, bottom, left, right.
    refs: [[u8; 4]; 4],
}

fn trim(src: &RgbaImage) -> Trimmed {
    let (w, h) = src.dimensions();
    let same = |a: &Rgba<u8>, b: &[u8; 4]| (a[3] < 8 && b[3] < 8) || (0..4).all(|c| (a[c] as i32 - b[c] as i32).abs() <= TRIM_TOL);
    let median = |px: Vec<Rgba<u8>>| -> [u8; 4] {
        std::array::from_fn(|c| {
            let mut v: Vec<u8> = px.iter().map(|p| p[c]).collect();
            let m = v.len() / 2;
            *v.select_nth_unstable(m).1
        })
    };
    // A line is uniform when at most 0.5 % of its pixels differ from the border colour.
    let uniform = |px: &mut dyn Iterator<Item = &Rgba<u8>>, n: u32, r: &[u8; 4]| px.filter(|p| !same(p, r)).take((n / 200 + 1) as usize).count() as u32 <= n / 200;
    let (my, mx) = ((h as f32 * TRIM_MAX) as u32, (w as f32 * TRIM_MAX) as u32);

    let top_ref = median((0..w).map(|x| *src.get_pixel(x, 0)).collect());
    let mut y0 = 0;
    while y0 < my && uniform(&mut (0..w).map(|x| src.get_pixel(x, y0)), w, &top_ref) {
        y0 += 1;
    }
    let bot_ref = median((0..w).map(|x| *src.get_pixel(x, h - 1)).collect());
    let mut y1 = h;
    while h - y1 < my && y1 > y0 + 1 && uniform(&mut (0..w).map(|x| src.get_pixel(x, y1 - 1)), w, &bot_ref) {
        y1 -= 1;
    }
    let rows = y1 - y0;
    let left_ref = median((y0..y1).map(|y| *src.get_pixel(0, y)).collect());
    let mut x0 = 0;
    while x0 < mx && uniform(&mut (y0..y1).map(|y| src.get_pixel(x0, y)), rows, &left_ref) {
        x0 += 1;
    }
    let right_ref = median((y0..y1).map(|y| *src.get_pixel(w - 1, y)).collect());
    let mut x1 = w;
    while w - x1 < mx && x1 > x0 + 1 && uniform(&mut (y0..y1).map(|y| src.get_pixel(x1 - 1, y)), rows, &right_ref) {
        x1 -= 1;
    }
    // Everything uniform: nothing is a border.
    let refs = [top_ref, bot_ref, left_ref, right_ref];
    if y0 >= my && h - y1 >= my && x0 >= mx && w - x1 >= mx && uniform(&mut src.pixels(), w * h, &top_ref) {
        return Trimmed { bounds: (0, 0, w, h), refs };
    }
    Trimmed { bounds: (x0, y0, x1, y1), refs }
}

/// Summed-area table.
struct Sat {
    t: Vec<f64>,
    w: usize,
}

impl Sat {
    fn new(v: &[f32], w: usize, h: usize) -> Self {
        let mut t = vec![0.0; (w + 1) * (h + 1)];
        for y in 0..h {
            let mut row = 0.0;
            for x in 0..w {
                row += v[y * w + x] as f64;
                t[(y + 1) * (w + 1) + x + 1] = t[y * (w + 1) + x + 1] + row;
            }
        }
        Self { t, w: w + 1 }
    }

    /// Sum over `[x0, x1) × [y0, y1)`.
    fn sum(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> f64 {
        if x1 <= x0 || y1 <= y0 {
            return 0.0;
        }
        self.t[y1 * self.w + x1] - self.t[y0 * self.w + x1] - self.t[y1 * self.w + x0] + self.t[y0 * self.w + x0]
    }
}

fn srgb_to_linear(c: u8) -> f32 {
    static LUT: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        std::array::from_fn(|i| {
            let c = i as f32 / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        })
    })[c as usize]
}

fn linear_to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round() as u8
}

/// Per-pixel importance of the proxy, ≥ 0.
fn saliency(p: &RgbaImage) -> Vec<f32> {
    let (w, h) = (p.width() as usize, p.height() as usize);
    let n = w * h;
    let lab_of = |px: &Rgba<u8>| {
        let [r, g, b] = [px[0], px[1], px[2]].map(srgb_to_linear);
        let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.950_47;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.088_83;
        let f = |t: f32| if t > 0.008_856 { t.cbrt() } else { 7.787 * t + 16.0 / 116.0 };
        let (fx, fy, fz) = (f(x), f(y), f(z));
        [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
    };
    let alpha: Vec<f32> = p.pixels().map(|px| px[3] as f32 / 255.0).collect();
    let mut lab: [Vec<f32>; 3] = std::array::from_fn(|_| Vec::with_capacity(n));
    for px in p.pixels() {
        let l = lab_of(px);
        for c in 0..3 {
            lab[c].push(l[c]);
        }
    }
    for ch in &mut lab {
        blur121(ch, w, h);
        blur121(ch, w, h);
    }
    // Distinctness from the dominant colour: the median, not the mean (Achanta's choice), which
    // the subject itself would drag toward it and leave the backdrop looking salient.
    let base: [f32; 3] = std::array::from_fn(|c| {
        let mut v: Vec<f32> = lab[c].iter().zip(&alpha).filter(|(_, a)| **a > 0.5).map(|(v, _)| *v).collect();
        if v.is_empty() {
            return 0.0;
        }
        let m = v.len() / 2;
        *v.select_nth_unstable_by(m, f32::total_cmp).1
    });
    let mut ft: Vec<f32> = (0..n).map(|i| (0..3).map(|c| (lab[c][i] - base[c]).powi(2)).sum::<f32>().sqrt()).collect();
    let l = &lab[0];
    let at = |x: usize, y: usize| l[y.min(h - 1) * w + x.min(w - 1)];
    let mut grad: Vec<f32> = (0..n)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            let (xm, ym) = (x.saturating_sub(1), y.saturating_sub(1));
            let gx = at(x + 1, ym) + 2.0 * at(x + 1, y) + at(x + 1, y + 1) - at(xm, ym) - 2.0 * at(xm, y) - at(xm, y + 1);
            let gy = at(xm, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1) - at(xm, ym) - 2.0 * at(x, ym) - at(x + 1, ym);
            (gx * gx + gy * gy).sqrt()
        })
        .collect();
    normalise(&mut ft);
    normalise(&mut grad);
    let mut s: Vec<f32> = ft.iter().zip(&grad).map(|(f, g)| 0.55 * f + 0.45 * g).collect();
    for _ in 0..2 {
        box_blur(&mut s, w, h, 2);
    }
    // A small floor keeps a flat picture's mass even; it is small so a flat backdrop cropped
    // away costs next to nothing next to cutting into the subject.
    s.iter_mut().zip(&alpha).for_each(|(v, a)| *v = (*v + 0.004) * a);
    s
}

/// Scales so the 98th percentile is 1 (all zero when flat).
fn normalise(v: &mut [f32]) {
    let mut sorted = v.to_vec();
    let k = (sorted.len() * 98 / 100).min(sorted.len() - 1);
    let p = *sorted.select_nth_unstable_by(k, f32::total_cmp).1;
    let inv = if p > 1e-6 { 1.0 / p } else { 0.0 };
    v.iter_mut().for_each(|x| *x = (*x * inv).min(1.5));
}

/// Separable [1 2 1]/4, edges clamped.
fn blur121(v: &mut [f32], w: usize, h: usize) {
    let mut t = v.to_vec();
    for y in 0..h {
        for x in 0..w {
            let r = y * w;
            t[r + x] = (v[r + x.saturating_sub(1)] + 2.0 * v[r + x] + v[r + (x + 1).min(w - 1)]) / 4.0;
        }
    }
    for y in 0..h {
        for x in 0..w {
            v[y * w + x] = (t[y.saturating_sub(1) * w + x] + 2.0 * t[y * w + x] + t[(y + 1).min(h - 1) * w + x]) / 4.0;
        }
    }
}

/// Box blur of radius `r` (separable, edges clamped).
fn box_blur(v: &mut [f32], w: usize, h: usize, r: usize) {
    let pass = |src: &[f32], dst: &mut [f32], len: usize, count: usize, idx: &dyn Fn(usize, usize) -> usize| {
        for j in 0..count {
            for i in 0..len {
                let (a, b) = (i.saturating_sub(r), (i + r).min(len - 1));
                dst[idx(j, i)] = (a..=b).map(|k| src[idx(j, k)]).sum::<f32>() / (b - a + 1) as f32;
            }
        }
    };
    let mut t = vec![0.0; v.len()];
    pass(v, &mut t, w, h, &|y, x| y * w + x);
    pass(&t, v, h, w, &|x, y| y * w + x);
}

/// Linear-light, premultiplied copy of a region.
fn to_linear(src: &RgbaImage, x: u32, y: u32, w: u32, h: u32) -> Rgba32FImage {
    Rgba32FImage::from_fn(w, h, |i, j| {
        let p = src.get_pixel(x + i, y + j);
        let a = p[3] as f32 / 255.0;
        Rgba([srgb_to_linear(p[0]) * a, srgb_to_linear(p[1]) * a, srgb_to_linear(p[2]) * a, a])
    })
}

/// Renders `src` into `slot` as `f` says.
pub fn render(slot: Slot, src: &RgbaImage, f: &Framing) -> RgbaImage {
    let (tw, th) = slot.size();
    let (sw, sh) = src.dimensions();
    let cx = (f.crop.x.round().max(0.0) as u32).min(sw - 1);
    let cy = (f.crop.y.round().max(0.0) as u32).min(sh - 1);
    let cw = (f.crop.w.round().max(1.0) as u32).min(sw - cx);
    let ch = (f.crop.h.round().max(1.0) as u32).min(sh - cy);
    let dx = (f.dest.x.round().max(0.0) as u32).min(tw - 1);
    let dy = (f.dest.y.round().max(0.0) as u32).min(th - 1);
    let dw = (f.dest.w.round().max(1.0) as u32).min(tw - dx);
    let dh = (f.dest.h.round().max(1.0) as u32).min(th - dy);

    let lin = to_linear(src, cx, cy, cw, ch);
    let filter = if dw < cw { FilterType::Lanczos3 } else { FilterType::CatmullRom };
    let pic = if (dw, dh) == (cw, ch) { lin } else { imageops::resize(&lin, dw, dh, filter) };

    let mut canvas = match f.fill {
        Fill::None | Fill::Transparent => Rgba32FImage::new(tw, th),
        Fill::Solid(c) => {
            let a = c[3] as f32 / 255.0;
            Rgba32FImage::from_pixel(tw, th, Rgba([srgb_to_linear(c[0]) * a, srgb_to_linear(c[1]) * a, srgb_to_linear(c[2]) * a, a]))
        }
        Fill::Ambient => ambient(src, f, tw, th),
    };
    for (x, y, p) in pic.enumerate_pixels() {
        let a = p[3].clamp(0.0, 1.0);
        let d = canvas.get_pixel_mut(dx + x, dy + y);
        for c in 0..3 {
            d[c] = p[c].clamp(0.0, a) + d[c] * (1.0 - a);
        }
        d[3] = a + d[3] * (1.0 - a);
    }
    RgbaImage::from_fn(tw, th, |x, y| {
        let p = canvas.get_pixel(x, y);
        let a = p[3].clamp(0.0, 1.0);
        if a <= 0.0 {
            return Rgba([0, 0, 0, 0]);
        }
        Rgba([linear_to_srgb(p[0] / a), linear_to_srgb(p[1] / a), linear_to_srgb(p[2] / a), (a * 255.0).round() as u8])
    })
}

/// The picture blurred, dimmed and slightly desaturated to the slot's size, with a soft shadow
/// under where the picture will sit.
fn ambient(src: &RgbaImage, f: &Framing, tw: u32, th: u32) -> Rgba32FImage {
    let (sw, sh) = src.dimensions();
    let aspect = tw as f32 / th as f32;
    // A cover-crop of the source centred on what is shown, so the colours belong to it.
    let (cw, ch) = if sw as f32 / sh as f32 > aspect { (sh as f32 * aspect, sh as f32) } else { (sw as f32, sw as f32 / aspect) };
    let cx = (f.crop.x + f.crop.w / 2.0 - cw / 2.0).clamp(0.0, sw as f32 - cw).round() as u32;
    let cy = (f.crop.y + f.crop.h / 2.0 - ch / 2.0).clamp(0.0, sh as f32 - ch).round() as u32;
    let (cw, ch) = ((cw.round() as u32).clamp(1, sw - cx), (ch.round() as u32).clamp(1, sh - cy));
    let (lw, lh) = ((tw / 8).max(2), (th / 8).max(2));
    let small = imageops::thumbnail(&*imageops::crop_imm(src, cx, cy, cw, ch), lw, lh);
    let mut planes: [Vec<f32>; 3] = std::array::from_fn(|c| small.pixels().map(|p| srgb_to_linear(p[c])).collect());
    for pl in &mut planes {
        for _ in 0..3 {
            box_blur(pl, lw as usize, lh as usize, 3);
        }
    }
    let low = Rgba32FImage::from_fn(lw, lh, |x, y| {
        let i = (y * lw + x) as usize;
        let (r, g, b) = (planes[0][i], planes[1][i], planes[2][i]);
        let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let k = 0.42;
        Rgba([(l + 0.75 * (r - l)) * k, (l + 0.75 * (g - l)) * k, (l + 0.75 * (b - l)) * k, 1.0])
    });
    let mut bg = imageops::resize(&low, tw, th, FilterType::Triangle);
    let d = f.dest;
    let sigma = 4.0f32;
    for (x, y, p) in bg.enumerate_pixels_mut() {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let ox = (d.x - px).max(px - (d.x + d.w)).max(0.0);
        let oy = (d.y - py).max(py - (d.y + d.h)).max(0.0);
        let shade = 1.0 - 0.5 * (-(ox * ox + oy * oy) / (2.0 * sigma * sigma)).exp();
        for c in 0..3 {
            p[c] *= shade;
        }
    }
    bg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise in 0..=255.
    fn noise(i: u32) -> u8 {
        let mut x = i.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
        x ^= x >> 15;
        x = x.wrapping_mul(0x2C1B_3C6D);
        (x >> 24) as u8
    }

    fn patch(img: &mut RgbaImage, x0: u32, y0: u32, w: u32, h: u32) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let n = noise(y * 7919 + x);
                img.put_pixel(x, y, Rgba([n, 255 - n, n / 2, 255]));
            }
        }
    }

    /// Low-contrast 2×2 checker: textured enough not to be trimmed, too faint to be salient.
    fn backdrop(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| if (x / 2 + y / 2) % 2 == 0 { Rgba([96, 104, 112, 255]) } else { Rgba([128, 136, 144, 255]) })
    }

    fn inside(f: &Framing, slot: Slot, r: Rect) -> bool {
        let (tw, th) = slot.size();
        let (a, b) = (f.map(r.x, r.y), f.map(r.x + r.w, r.y + r.h));
        a.0 >= -0.5 && a.1 >= -0.5 && b.0 <= tw as f32 + 0.5 && b.1 <= th as f32 + 0.5
    }

    #[test]
    fn off_centre_subject_stays_in_frame() {
        let mut img = backdrop(1000, 400);
        patch(&mut img, 820, 140, 140, 120);
        let f = plan(Slot::Boot, &img);
        assert!(inside(&f, Slot::Boot, Rect::new(820.0, 140.0, 140.0, 120.0)), "{f:?}");
        assert_eq!(f.fill, Fill::None, "a cover crop keeps the subject, no padding needed: {f:?}");
    }

    #[test]
    fn letterbox_bars_are_trimmed() {
        let mut img = RgbaImage::from_pixel(640, 480, Rgba([0, 0, 0, 255]));
        patch(&mut img, 0, 60, 640, 360);
        assert_eq!(trim(&img).bounds, (0, 60, 640, 420));
        let f = plan(Slot::Pic1, &img);
        assert!(f.crop.y >= 59.5 && f.crop.y + f.crop.h <= 420.5, "{f:?}");
    }

    #[test]
    fn title_and_logo_at_both_ends_pad_instead_of_cutting_one() {
        // Box-art shape: a title at the top, a logo at the bottom; a 16:9 crop must lose one.
        let mut img = backdrop(600, 600);
        patch(&mut img, 150, 20, 300, 90);
        patch(&mut img, 200, 490, 200, 90);
        for slot in [Slot::Icon0, Slot::Boot] {
            let f = plan(slot, &img);
            assert!(inside(&f, slot, Rect::new(150.0, 20.0, 300.0, 90.0)) && inside(&f, slot, Rect::new(200.0, 490.0, 200.0, 90.0)), "{f:?}");
            assert_eq!(f.fill, Fill::Ambient, "textured edges: ambient backdrop");
            let out = render(slot, &img, &f);
            assert!(out.pixels().all(|p| p[3] == 255), "ambient fill leaves no holes");
        }
    }

    #[test]
    fn flat_backdrop_is_cropped_not_padded() {
        // A square scene whose subject sits in a 16:9 band: cropping the dull rest beats padding.
        let mut img = backdrop(800, 800);
        patch(&mut img, 100, 300, 600, 200);
        let f = plan(Slot::Boot, &img);
        assert_eq!(f.fill, Fill::None, "{f:?}");
        assert!(inside(&f, Slot::Boot, Rect::new(100.0, 300.0, 600.0, 200.0)), "{f:?}");
    }

    #[test]
    fn logo_on_flat_colour_is_whole_and_padding_matches() {
        let mut img = RgbaImage::from_pixel(400, 400, Rgba([250, 250, 250, 255]));
        patch(&mut img, 100, 150, 200, 100);
        let f = plan(Slot::Icon0, &img);
        assert!(inside(&f, Slot::Icon0, Rect::new(100.0, 150.0, 200.0, 100.0)), "{f:?}");
        let out = render(Slot::Icon0, &img, &f);
        let c = out.get_pixel(0, 0);
        assert!(c[0] > 240 && c[3] == 255, "corner is the page colour: {c:?}");
    }

    #[test]
    fn transparent_logo_keeps_its_alpha() {
        let mut img = RgbaImage::new(300, 300);
        patch(&mut img, 50, 100, 200, 100);
        let f = plan(Slot::Icon0, &img);
        let out = render(Slot::Icon0, &img, &f);
        assert_eq!(out.get_pixel(0, 0)[3], 0);
        assert!(inside(&f, Slot::Icon0, Rect::new(50.0, 100.0, 200.0, 100.0)), "{f:?}");
    }

    #[test]
    fn pic1_keeps_the_subject_out_from_under_icon0() {
        // The subject sits at mid height, where ICON0 covers the left of the screen.
        let mut img = backdrop(1200, 272);
        patch(&mut img, 560, 100, 100, 70);
        let f = plan(Slot::Pic1, &img);
        let (x, _) = f.map(560.0, 100.0);
        assert!(x >= 164.0, "subject lands at x={x}, under ICON0 (20..164): {f:?}");
        assert!(inside(&f, Slot::Pic1, Rect::new(560.0, 100.0, 100.0, 70.0)), "{f:?}");
    }

    #[test]
    fn exact_aspect_is_a_plain_scale() {
        let mut img = backdrop(960, 544);
        patch(&mut img, 300, 200, 100, 100);
        let f = plan(Slot::Boot, &img);
        assert_eq!((f.crop, f.fill), (Rect::new(0.0, 0.0, 960.0, 544.0), Fill::None), "{f:?}");
        assert_eq!(render(Slot::Boot, &img, &f).dimensions(), (480, 272));
    }

    #[test]
    fn linear_light_downscale_keeps_brightness() {
        // A 1-px black/white checker averages to sRGB ~188 in linear light, not 128.
        let img = RgbaImage::from_fn(960, 544, |x, y| if (x + y) % 2 == 0 { Rgba([0, 0, 0, 255]) } else { Rgba([255, 255, 255, 255]) });
        let f = plan(Slot::Boot, &img);
        let out = render(Slot::Boot, &img, &f);
        let v = out.get_pixel(240, 136)[0];
        assert!((180..=196).contains(&v), "mid-grey in linear light: {v}");
    }
}
