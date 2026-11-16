//! `cargo run --release -p psx2psp-recomp --example icache -- EXE TRACE FRAMES [HOT...]`: the PSP
//! I-cache (16 KB, 2 ways, 64-byte lines, LRU) on the translated code of EXE, driven by a trace
//! of the guest's straight-line runs (`psx2psp-sim --trace`, FRAMES frames long). Misses of the
//! layout the translator writes, then of layouts that pack the main-pass code executed at least
//! HOT times (default 1) apart from the rest, in guest order: what hot/cold placement can save.
//! The out-of-line slow paths are not in the model.
//!
//! The runtime's code joins the model with `RT=SYMS` (`psp-nm -S -n psxrt.elf`, text symbols):
//! at each device access, psx_update and GTE command the trace marks, the native runtime's
//! functions on that path are fetched (the first `RT_K` bytes of each, default all of it), psxrt
//! loaded at `RT_BASE` (hex, default 08804000). `SKIP=LO:HI` (hex guest pcs): accesses made
//! there run inside a recipe on the PSP and reach no runtime path.

use psx2psp_recomp::{Discovery, Hazards, PsxExe, translate};
use std::collections::HashMap;

const LINE: u32 = 64;
const SETS: usize = 128;

struct Cache {
    tag: [[u32; 2]; SETS],
    /// Way used least recently, per set.
    lru: [u8; SETS],
    last: u32,
    misses: u64,
    fetches: u64,
}

impl Cache {
    fn new() -> Self {
        Cache { tag: [[u32::MAX; 2]; SETS], lru: [0; SETS], last: u32::MAX, misses: 0, fetches: 0 }
    }
    fn touch(&mut self, line: u32) {
        if line == self.last {
            return;
        }
        self.last = line;
        self.fetches += 1;
        let s = line as usize % SETS;
        let w = if self.tag[s][0] == line {
            0
        } else if self.tag[s][1] == line {
            1
        } else {
            self.misses += 1;
            let w = self.lru[s] as usize;
            self.tag[s][w] = line;
            w
        };
        self.lru[s] = 1 - w as u8;
    }
    fn range(&mut self, start: u32, end: u32) {
        if end > start {
            for l in start / LINE..=(end - 1) / LINE {
                self.touch(l);
            }
        }
    }
}

/// What the trace says happened: a run of translated code (out-of-line entry, first and last
/// span), or a trip through the runtime (an index into the paths).
enum Ev {
    Run(Option<u32>, usize, usize),
    Rt(usize),
}

/// The runtime's functions on each path, by name (`name@hex` for a static one whose name repeats).
const COMMON: &[&str] = &["psxrt_rt_call", "native_rt_call", "rt_call", "sync_time", "io_kind_at", "psx_irq_pending", "restore_and_go"];
fn path(kind: u32, p: u32) -> Vec<&'static str> {
    let mut v: Vec<&str> = Vec::new();
    match kind {
        0 | 1 => {
            v.extend(COMMON);
            v.push(if kind == 0 { "psx_read" } else { "psx_write" });
            let dev: &[&str] = match (p, kind) {
                (0x1F80_1100..0x1F80_1130, 0) => &["poll_skip", "timer_tick", "timer_read", "sync@32804", "cpt", "__umoddi3"],
                (0x1F80_1100..0x1F80_1130, _) => &["timer_write", "sync@32804", "cpt", "timers_next", "psx_schedule"],
                (0x1F80_1040..0x1F80_1060, 0) => &["sio_read"],
                (0x1F80_1040..0x1F80_1060, _) => &["sio_write"],
                (0x1F80_1800..0x1F80_1804, 0) => &["cd_read"],
                (0x1F80_1800..0x1F80_1804, _) => &["cd_write"],
                (0x1F80_1080..0x1F80_1100, 0) => &["dma_read"],
                (0x1F80_1080..0x1F80_1100, _) => &["dma_write"],
                (0x1F80_1810..0x1F80_1818, 0) => &["gpu_read"],
                _ => &[],
            };
            v.extend(dev);
        }
        2 => v.extend([
            "psxrt_rt_call", "native_rt_call", "rt_call", "update", "psx_update", "timers_update", "sync@32804", "cpt",
            "timers_next", "dma_update", "cd_update", "sio_update", "psx_schedule", "__umoddi3", "restore_and_go",
        ]),
        // GTE commands: the routines of trampolines.S the translator enters (native.c gte_ops),
        // the others through GTE_CMD into C.
        _ => match p {
            0x06 => v.push("psxrt_gte_nclip"),
            0x2D => v.push("psxrt_gte_avsz3"),
            0x2E => v.push("psxrt_gte_avsz4"),
            0x01 => v.push("psxrt_gte_rtps"),
            0x30 => v.push("psxrt_gte_rtpt"),
            0x12 => v.push("psxrt_gte_mvmva"),
            0x3D => v.push("psxrt_gte_gpf"),
            _ => v.extend(["psxrt_gte_cmd", "native_gte_fast"]),
        },
    }
    v
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex");
    let exe = PsxExe::parse(&std::fs::read(&a[1]).expect("read EXE")).expect("PS-X EXE");
    let raw = std::fs::read(&a[2]).expect("read trace");
    let frames: f64 = a[3].parse().expect("frames");
    let hots: Vec<u64> = if a.len() > 4 { a[4..].iter().map(|s| s.parse().expect("hot count")).collect() } else { vec![1] };
    // Runtime symbols: name -> (address, size); a repeated name also as name@address.
    let rt_base = std::env::var("RT_BASE").map(|s| hex(&s)).unwrap_or(0x0880_4000);
    let rt_k: u32 = std::env::var("RT_K").map(|s| s.parse().expect("RT_K")).unwrap_or(u32::MAX);
    let mut syms: HashMap<String, (u32, u32)> = HashMap::new();
    if let Ok(f) = std::env::var("RT") {
        for l in std::fs::read_to_string(f).expect("read RT").lines() {
            let w: Vec<&str> = l.split_whitespace().collect();
            if w.len() == 4 {
                let (addr, size) = (hex(w[0]), hex(w[1]));
                syms.insert(format!("{}@{:x}", w[3], addr), (addr, size));
                syms.entry(w[3].to_string()).or_insert((addr, size));
            }
        }
    }
    let skip = std::env::var("SKIP").ok().map(|s| {
        let (l, h) = s.split_once(':').expect("SKIP=LO:HI");
        hex(l)..hex(h)
    });
    // Paths by (kind, device), as host ranges.
    let mut paths: Vec<Vec<(u32, u32)>> = Vec::new();
    let mut path_of: HashMap<(u32, u32), usize> = HashMap::new();
    let mut missing: Vec<&str> = Vec::new();
    let dev_key = |p: u32| match p {
        0x1F80_1100..0x1F80_1130 => 1,
        0x1F80_1040..0x1F80_1060 => 2,
        0x1F80_1800..0x1F80_1804 => 3,
        0x1F80_1080..0x1F80_1100 => 4,
        0x1F80_1810..0x1F80_1818 => 5,
        _ => 0,
    };
    let d = Discovery::run(&exe);
    let base = 0x0900_0000;
    let t = translate(&exe, &d, &Hazards::check(&exe, &d), base).expect("translate");
    let sp = &t.spans;
    let (lo, hi) = (exe.text_addr, exe.text_addr + exe.text.len() as u32);
    // Span index of the guest pc: the last span at or before it.
    let at = |pc: u32| sp.partition_point(|s| s.0 <= pc).checked_sub(1);
    // Each run as span indices: the first span it starts in at its start (or None: it enters
    // out-of-line code at `pc`, then the main pass from the span after), the last.
    let mut model: Vec<Ev> = Vec::with_capacity(raw.len() / 8);
    let (mut outside, mut odd, mut skipped) = (0u64, 0u64, 0u64);
    let mut rt_n: HashMap<u32, u64> = HashMap::new();
    let mut last_pc = 0u32;
    for c in raw.chunks_exact(8) {
        let (f, l) = (u32::from_le_bytes(c[..4].try_into().unwrap()), u32::from_le_bytes(c[4..].try_into().unwrap()));
        if f >= 0xFFFF_FFF0 {
            let kind = f & 15;
            if syms.is_empty() {
                continue;
            }
            if kind < 2 && skip.as_ref().is_some_and(|r| r.contains(&last_pc)) {
                skipped += 1;
                continue;
            }
            *rt_n.entry(kind).or_default() += 1;
            let key = (kind, if kind < 2 { dev_key(l) } else if kind == 3 { l } else { 0 });
            let k = *path_of.entry(key).or_insert_with(|| {
                let mut r = Vec::new();
                for name in path(kind, l) {
                    match syms.get(name) {
                        Some(&(addr, size)) => r.push((rt_base + addr, rt_base + addr + size.min(rt_k))),
                        None => missing.push(name),
                    }
                }
                paths.push(r);
                paths.len() - 1
            });
            model.push(Ev::Rt(k));
            continue;
        }
        last_pc = l;
        if f < lo || l >= hi || l < f {
            outside += 1;
            continue;
        }
        let (Some(i), Some(j)) = (at(f), at(l)) else {
            outside += 1;
            continue;
        };
        if sp[i].0 == f {
            model.push(Ev::Run(None, i, j));
        } else {
            odd += 1;
            model.push(Ev::Run(t.map.get(&f).copied(), i + 1, j));
        }
    }
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        println!("runtime symbols not found: {missing:?}");
    }
    // Heat: how many runs go through each span.
    let mut heat = vec![0u64; sp.len()];
    for e in &model {
        if let Ev::Run(_, i, j) = *e {
            for h in heat.iter_mut().take(j + 1).skip(i) {
                *h += 1;
            }
        }
    }
    let words = |k: usize| (sp[k].2 - sp[k].1) / 4;
    let main_words: u64 = (0..sp.len()).map(|k| words(k) as u64).sum();
    println!(
        "{}: {} events ({} outside the text, {} entering out-of-line code), {:.0} frames; main pass {} words, {} host words in all",
        a[1],
        model.len(),
        outside,
        odd,
        frames,
        main_words,
        t.words.len()
    );
    if !syms.is_empty() {
        let per = |k: u32| *rt_n.get(&k).unwrap_or(&0) as f64 / frames;
        println!(
            "runtime trips/frame: read {:.0} write {:.0} update {:.0} gte {:.0} ({:.0} accesses/frame in SKIP); psxrt at {rt_base:08X}, {} bytes per function at most",
            per(0),
            per(1),
            per(2),
            per(3),
            skipped as f64 / frames,
            rt_k
        );
    }
    // Translated misses and runtime misses apart.
    let report = |name: &str, c: &Cache, rt: u64, lines: usize| {
        println!(
            "{name:28} misses {:7.0}/frame (translated {:7.0}, runtime {:7.0}), lines touched {:6} ({} KB)",
            c.misses as f64 / frames,
            (c.misses - rt) as f64 / frames,
            rt as f64 / frames,
            lines,
            lines * LINE as usize / 1024
        );
    };
    let run_rt = |c: &mut Cache, k: usize| {
        let m = c.misses;
        for &(s, e) in &paths[k] {
            c.range(s, e);
        }
        c.misses - m
    };
    // The translator's layout.
    let mut c = Cache::new();
    let mut rt = 0u64;
    let mut by_path = vec![(0u64, 0u64); paths.len()];
    let mut seen = std::collections::HashSet::new();
    for e in &model {
        match *e {
            Ev::Rt(k) => {
                let m = run_rt(&mut c, k);
                rt += m;
                by_path[k].0 += 1;
                by_path[k].1 += m;
            }
            Ev::Run(cold, i, j) => {
                if let Some(h) = cold {
                    c.touch(h / LINE);
                    seen.insert(h / LINE);
                }
                if i <= j && j < sp.len() {
                    c.range(sp[i].1, sp[j].2);
                    for l in sp[i].1 / LINE..=(sp[j].2.max(sp[i].1 + 1) - 1) / LINE {
                        seen.insert(l);
                    }
                }
            }
        }
    }
    report("as translated", &c, rt, seen.len());
    let mut keys: Vec<_> = path_of.iter().collect();
    keys.sort_by_key(|&(_, &k)| std::cmp::Reverse(by_path[k].1));
    for (&(kind, dev), &k) in keys {
        let (n, m) = by_path[k];
        println!(
            "  path {} {:>8}: {:6.0} trips/frame, {:6.0} runtime misses/frame ({:.1} each)",
            ["read", "write", "update", "gte"][kind as usize],
            if kind == 3 { format!("cmd {dev:02X}") } else { format!("dev {dev}") },
            n as f64 / frames,
            m as f64 / frames,
            m as f64 / n.max(1) as f64
        );
    }
    let exec_words: u64 = (0..sp.len()).filter(|&k| heat[k] > 0).map(|k| words(k) as u64).sum();
    println!("executed main-pass words {exec_words} ({} KB): line use {:.0}%", exec_words * 4 / 1024, 100.0 * exec_words as f64 * 4.0 / (seen.len() as f64 * LINE as f64));
    // Hot spans packed in guest order at `base`, the others after them.
    for &hot in &hots {
        let mut at_new = vec![0u32; sp.len()];
        let mut p = base;
        for pass in [true, false] {
            for k in 0..sp.len() {
                if (heat[k] >= hot) == pass {
                    at_new[k] = p;
                    p += words(k) * 4;
                }
            }
        }
        let mut c = Cache::new();
        let mut rt = 0u64;
        let mut seen = std::collections::HashSet::new();
        for e in &model {
            let (cold, i, j) = match *e {
                Ev::Rt(k) => {
                    rt += run_rt(&mut c, k);
                    continue;
                }
                Ev::Run(cold, i, j) => (cold, i, j),
            };
            if let Some(h) = cold {
                c.touch(h / LINE);
            }
            // Contiguous while the spans are on the same side.
            let mut k = i;
            while k <= j && k < sp.len() {
                let mut e = k;
                while e < j && at_new[e + 1] == at_new[e] + words(e) * 4 {
                    e += 1;
                }
                let (s, end) = (at_new[k], at_new[e] + words(e) * 4);
                c.range(s, end);
                if end > s {
                    for l in s / LINE..=(end - 1) / LINE {
                        seen.insert(l);
                    }
                }
                k = e + 1;
            }
        }
        let hot_words: u64 = (0..sp.len()).filter(|&k| heat[k] >= hot).map(|k| words(k) as u64).sum();
        report(&format!("hot >= {hot} packed ({} KB hot)", hot_words * 4 / 1024), &c, rt, seen.len());
    }
}
