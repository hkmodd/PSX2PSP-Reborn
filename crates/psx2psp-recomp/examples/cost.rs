//! `cargo run --release -p psx2psp-recomp --example cost -- EXE COUNTS FRAMES`: the main-pass host
//! words the translated code of EXE executes, from the guest instructions the interpreter ran at
//! each RAM word over FRAMES frames (`psx2psp-sim --trace`'s FILE.cnt), by guest instruction kind.
//! A group's words count at its first access (its span); out-of-line code is not counted.

use psx2psp_recomp::{Discovery, Hazards, Insn, Isa, PsxExe, decode, translate};
use std::collections::BTreeMap;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let exe = PsxExe::parse(&std::fs::read(&a[1]).expect("read EXE")).expect("PS-X EXE");
    let raw = std::fs::read(&a[2]).expect("read counts");
    let frames: f64 = a[3].parse().expect("frames");
    let count = |pc: u32| {
        let o = ((pc & 0x1F_FFFF) >> 2) as usize * 4;
        u32::from_le_bytes(raw[o..o + 4].try_into().unwrap()) as u64
    };
    let d = Discovery::run(&exe);
    let t = translate(&exe, &d, &Hazards::check(&exe, &d), 0x0900_0000).expect("translate");
    let (lo, hi) = (exe.text_addr, exe.text_addr + exe.text.len() as u32);
    let guest = |pc: u32| {
        let o = (pc - exe.text_addr) as usize;
        u32::from_le_bytes(exe.text[o..o + 4].try_into().unwrap())
    };
    // Guest instructions run inside and outside the text, and those of the text with no span.
    let all: u64 = (0..raw.len() / 4).map(|k| count(0x8000_0000 + 4 * k as u32)).sum();
    let text: u64 = (lo..hi).step_by(4).map(count).sum();
    let mut by: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let (mut gsum, mut hsum) = (0u64, 0u64);
    for &(pc, s, e) in &t.spans {
        let n = count(pc);
        if n == 0 {
            continue;
        }
        let w = ((e - s) / 4) as u64;
        let kind = format!("{:?}", decode(Insn(guest(pc)), Isa::R3000A));
        let k = by.entry(kind).or_default();
        k.0 += n;
        k.1 += n * w;
        gsum += n;
        hsum += n * w;
    }
    println!(
        "{}: {:.0} guest insns/frame run ({:.0} in the text, {:.0} at span starts), {:.0} main-pass host words/frame ({:.2} per guest)",
        a[1],
        all as f64 / frames,
        text as f64 / frames,
        gsum as f64 / frames,
        hsum as f64 / frames,
        hsum as f64 / gsum.max(1) as f64
    );
    let mut v: Vec<_> = by.into_iter().collect();
    v.sort_by_key(|(_, (_, h))| std::cmp::Reverse(*h));
    for (k, (g, h)) in v.iter().take(24) {
        println!(
            "{k:8} {:9.0} guest/frame {:9.0} host/frame ({:4.1}% of host, {:.2} each)",
            *g as f64 / frames,
            *h as f64 / frames,
            100.0 * *h as f64 / hsum.max(1) as f64,
            *h as f64 / (*g).max(1) as f64
        );
    }
}
