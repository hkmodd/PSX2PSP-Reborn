//! `cargo run --release -p psx2psp-recomp --example icache -- EXE TRACE FRAMES [HOT...]`: the PSP
//! I-cache (16 KB, 2 ways, 64-byte lines, LRU) on the translated code of EXE, driven by a trace
//! of the guest's straight-line runs (`psx2psp-sim --trace`, FRAMES frames long). Misses of the
//! layout the translator writes, then of layouts that pack the main-pass code executed at least
//! HOT times (default 1) apart from the rest, in guest order: what hot/cold placement can save.
//! The runtime's own code and the out-of-line slow paths are not in the model.

use psx2psp_recomp::{Discovery, Hazards, PsxExe, translate};

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

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let exe = PsxExe::parse(&std::fs::read(&a[1]).expect("read EXE")).expect("PS-X EXE");
    let raw = std::fs::read(&a[2]).expect("read trace");
    let frames: f64 = a[3].parse().expect("frames");
    let hots: Vec<u64> = if a.len() > 4 { a[4..].iter().map(|s| s.parse().expect("hot count")).collect() } else { vec![1] };
    let runs: Vec<(u32, u32)> = raw
        .chunks_exact(8)
        .map(|c| (u32::from_le_bytes(c[..4].try_into().unwrap()), u32::from_le_bytes(c[4..].try_into().unwrap())))
        .collect();
    let d = Discovery::run(&exe);
    let base = 0x0900_0000;
    let t = translate(&exe, &d, &Hazards::check(&exe, &d), base).expect("translate");
    let sp = &t.spans;
    let (lo, hi) = (exe.text_addr, exe.text_addr + exe.text.len() as u32);
    // Span index of the guest pc: the last span at or before it.
    let at = |pc: u32| sp.partition_point(|s| s.0 <= pc).checked_sub(1);
    // Each run as span indices: the first span it starts in at its start (or None: it enters
    // out-of-line code at `pc`, then the main pass from the span after), the last.
    let mut model: Vec<(Option<u32>, usize, usize)> = Vec::with_capacity(runs.len());
    let (mut outside, mut odd) = (0u64, 0u64);
    for &(f, l) in &runs {
        if f < lo || l >= hi || l < f {
            outside += 1;
            continue;
        }
        let (Some(i), Some(j)) = (at(f), at(l)) else {
            outside += 1;
            continue;
        };
        if sp[i].0 == f {
            model.push((None, i, j));
        } else {
            odd += 1;
            model.push((t.map.get(&f).copied(), i + 1, j));
        }
    }
    // Heat: how many runs go through each span.
    let mut heat = vec![0u64; sp.len()];
    for &(_, i, j) in &model {
        for h in heat.iter_mut().take(j + 1).skip(i) {
            *h += 1;
        }
    }
    let words = |k: usize| (sp[k].2 - sp[k].1) / 4;
    let main_words: u64 = (0..sp.len()).map(|k| words(k) as u64).sum();
    println!(
        "{}: {} runs ({} outside the text, {} entering out-of-line code), {:.0} frames; main pass {} words, {} host words in all",
        a[1],
        runs.len(),
        outside,
        odd,
        frames,
        main_words,
        t.words.len()
    );
    let report = |name: &str, c: &Cache, lines: usize| {
        println!(
            "{name:28} misses {:9} = {:7.0}/frame, line fetches {:10}, lines touched {:6} ({} KB)",
            c.misses,
            c.misses as f64 / frames,
            c.fetches,
            lines,
            lines * LINE as usize / 1024
        );
    };
    // The translator's layout.
    let mut c = Cache::new();
    let mut seen = std::collections::HashSet::new();
    for &(cold, i, j) in &model {
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
    report("as translated", &c, seen.len());
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
        let mut seen = std::collections::HashSet::new();
        for &(cold, i, j) in &model {
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
        report(&format!("hot >= {hot} packed ({} KB hot)", hot_words * 4 / 1024), &c, seen.len());
    }
}
