//! `cargo run --release -p psx2psp-recomp --example basegroups -- EXE`: how the guest's loads and
//! stores group by base register inside straight-line runs (no branch, branch target or write to
//! the base in between) - what a shared range check per run would save.

use psx2psp_recomp::{Discovery, Flow, Insn, Isa, Kind, PsxExe, Uses, decode, discover::flow};
use std::collections::{BTreeMap, BTreeSet};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let exe = PsxExe::parse(&std::fs::read(&a[1]).expect("read EXE")).expect("PS-X EXE");
    let d = Discovery::run(&exe);
    let strict = std::env::var("STRICT").map_or(true, |v| v != "0");
    let word = |pc: u32| {
        let o = (pc - exe.text_addr) as usize;
        Insn(u32::from_le_bytes(exe.text[o..o + 4].try_into().unwrap()))
    };
    let mut starts: BTreeSet<u32> = d.entries.keys().copied().collect();
    for &pc in &d.code {
        let i = word(pc);
        match flow(i, decode(i, Isa::R3000A), pc) {
            Flow::Next => {}
            Flow::Branch(t) | Flow::Goto(t) => {
                starts.insert(t);
                starts.insert(pc + 8);
            }
            _ => {
                starts.insert(pc + 8);
            }
        }
    }
    use Kind::*;
    let mem = |k: Kind| matches!(k, Lb | Lh | Lw | Lbu | Lhu | Lwl | Lwr | Sb | Sh | Sw | Swl | Swr | Lwc2 | Swc2);
    // open run per base register: its length so far
    let mut open: BTreeMap<usize, u32> = BTreeMap::new();
    let mut sizes: BTreeMap<u32, u32> = BTreeMap::new();
    let mut by_base = [0u32; 32];
    let (mut total, mut prev) = (0u32, 0u32);
    let close = |open: &mut BTreeMap<usize, u32>, sizes: &mut BTreeMap<u32, u32>, r: Option<usize>| match r {
        Some(r) => {
            if let Some(n) = open.remove(&r) {
                *sizes.entry(n).or_default() += 1;
            }
        }
        None => {
            for (_, n) in std::mem::take(open) {
                *sizes.entry(n).or_default() += 1;
            }
        }
    };
    for &pc in &d.code {
        if starts.contains(&pc) || pc != prev + 4 {
            close(&mut open, &mut sizes, None);
        }
        prev = pc;
        let i = word(pc);
        let k = decode(i, Isa::R3000A);
        // k1 holds the translated base only while nothing else in between needs it: plain ALU
        // instructions are copied as they are, anything else (another base, div, GTE, the
        // runtime) ends the run. STRICT=0 lets runs of different bases interleave.
        let alu = matches!(k, Sll | Srl | Sra | Sllv | Srlv | Srav | Mfhi | Mthi | Mflo | Mtlo | Mult | Multu | Add | Addu | Sub | Subu | And | Or | Xor | Nor | Slt | Sltu | Addi | Addiu | Slti | Sltiu | Andi | Ori | Xori | Lui);
        if mem(k) {
            total += 1;
            by_base[i.rs()] += 1;
            if strict && !open.contains_key(&i.rs()) {
                close(&mut open, &mut sizes, None);
            }
            *open.entry(i.rs()).or_default() += 1;
        } else if strict && !alu {
            close(&mut open, &mut sizes, None);
        }
        let w = Uses::of(i, k).write;
        for r in 1..32 {
            if w & (1 << r) != 0 {
                close(&mut open, &mut sizes, Some(r));
            }
        }
    }
    close(&mut open, &mut sizes, None);
    println!("{} guest words, {} loads/stores", d.code.len(), total);
    // Accesses whose base some `lui r,0x1F80` in the executable sets: the scratchpad (estimate).
    let spad: u32 = (1..32)
        .filter(|&r| d.code.iter().any(|&p| word(p).0 == (0x0F << 26) | (r as u32) << 16 | 0x1F80))
        .map(|r| by_base[r])
        .sum();
    println!("accesses through a register some lui 0x1F80 sets: {spad}");
    let mut bases: Vec<(u32, usize)> = by_base.iter().enumerate().map(|(r, &n)| (n, r)).collect();
    bases.sort_unstable_by(|a, b| b.cmp(a));
    let top: Vec<String> = bases.iter().take(8).map(|(n, r)| format!("r{r} {n}")).collect();
    println!("by base: {}", top.join(", "));
    let mut cum = 0u32;
    for (&n, &c) in &sizes {
        cum += n * c;
        if n <= 8 || c > 0 && n % 4 == 0 {
            println!("run of {n:2}: {c:6} runs, {:6} accesses (cumulative {:.1}%)", n * c, 100.0 * cum as f64 / total as f64);
        }
    }
    // host words: 6 per access today; grouped: 5 for the run's check + 1 per access, runs >= 2
    let today = 6 * total;
    let grouped: u32 = sizes.iter().map(|(&n, &c)| c * if n >= 2 { 5 + n } else { 6 }).sum();
    println!("access fast-path words: today {today}, grouped (runs >= 2) {grouped}, saved {}", today - grouped);
}
