//! `cargo run --release -p psx2psp-recomp --example hostdump -- EXE START END`: the host words the
//! translator writes for each guest instruction of START..END (hex), main pass only, and the
//! totals of the executable: how much host code a guest word costs. `KINDS=1`: the range's
//! main-pass words by guest instruction kind, the 20 largest.

use psx2psp_recomp::{Discovery, Hazards, PsxExe, translate};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex address");
    let exe = PsxExe::parse(&std::fs::read(&a[1]).expect("read EXE")).expect("PS-X EXE");
    let (start, end) = (hex(&a[2]), hex(&a[3]));
    let d = Discovery::run(&exe);
    // BASE=<hex>: the host address the image has in the EBOOT (the runtime log's "code at").
    let base = std::env::var("BASE").map_or(0x0900_0000, |b| hex(&b));
    let t = translate(&exe, &d, &Hazards::check(&exe, &d), base).expect("translate");
    let word = |h: u32| t.words[((h - base) / 4) as usize];
    let guest = |pc: u32| {
        let o = (pc - exe.text_addr) as usize;
        u32::from_le_bytes(exe.text[o..o + 4].try_into().unwrap())
    };
    let (mut gn, mut hn) = (0u32, 0u32);
    let pcs: Vec<u32> = t.map.range(start..end).map(|(&p, _)| p).collect();
    for (i, &pc) in pcs.iter().enumerate() {
        let h = t.map[&pc];
        let next = pcs.get(i + 1).map(|p| t.map[p]).unwrap_or(h + 4);
        let n = if next > h { (next - h) / 4 } else { 1 };
        let ws: Vec<String> = (0..n.min(24)).map(|k| format!("{:08X}", word(h + 4 * k))).collect();
        println!("{pc:08X} {:08X} -> {h:08X} {:2} {}", guest(pc), n, ws.join(" "));
        gn += 1;
        hn += n;
    }
    println!("range: {gn} guest words, {hn} host words ({:.2} per guest word)", hn as f64 / gn.max(1) as f64);
    // KINDS=1: main-pass words by guest instruction kind over the range (Translation::spans; a
    // group's span is its first word's, out-of-line code is not counted).
    if std::env::var("KINDS").is_ok() {
        let mut by: std::collections::BTreeMap<String, (u32, u32)> = Default::default();
        for &(pc, s, e) in t.spans.iter().filter(|s| (start..end).contains(&s.0)) {
            let kind = format!("{:?}", psx2psp_recomp::decode(psx2psp_recomp::Insn(guest(pc)), psx2psp_recomp::Isa::R3000A));
            let k = by.entry(kind).or_default();
            k.0 += 1;
            k.1 += (e - s) / 4;
        }
        let mut v: Vec<_> = by.into_iter().collect();
        v.sort_by_key(|(_, (_, w))| std::cmp::Reverse(*w));
        for (k, (n, w)) in v.iter().take(20) {
            println!("{k:8} {n:7} guest {w:8} host ({:.2} each)", *w as f64 / *n as f64);
        }
    }
    println!(
        "exe: {} guest words discovered, {} host words ({:.2} each), cold {} words in {} blocks",
        d.code.len(),
        t.words.len(),
        t.words.len() as f64 / d.code.len().max(1) as f64,
        t.cold.0,
        t.cold.1
    );
}
