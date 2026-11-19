//! `cargo run --release -p psx2psp-engine --example profile -- EXE COUNTS FRAMES [TOP [IDLE,...]]`: the flat
//! profile of the translated code of EXE by guest function, from the guest instructions the
//! interpreter ran at each RAM word over FRAMES frames (`psx2psp-sim --trace`'s FILE.cnt).
//!
//! A function runs from a discovered entry or a PsyQ signature to the next one. Per function:
//! guest instructions, main-pass host words executed (counted as `psx2psp-recomp --example cost`
//! does: a span's words at its first guest word), runtime calls (COP0, GTE, syscalls, hooks: host
//! calls), indirect jumps (`jr`, `jalr`: the dispatcher when the target is not linked), and the
//! main-pass bytes it occupies. Then how few of the hottest functions hold 50-95% of the host
//! words, and their bytes against the PSP's 16 KB I-cache: the order of the 60 fps work.
//!
//! IDLE (hex function addresses, e.g. PsyQ's VSync wait loop that polls the vblank counter) is
//! guest spinning the runtime's vblank stall skips: listed with `*`, left out of the work shares.

use psx2psp_engine::native_code::psyq;
use psx2psp_recomp::{Discovery, Hazards, Insn, Isa, Kind, PsxExe, decode, translate};
use std::collections::BTreeMap;

#[derive(Default)]
struct Func {
    guest: u64,
    host: u64,
    rt: u64,
    ind: u64,
    bytes: u32,
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let exe = PsxExe::parse(&std::fs::read(&a[1]).expect("read EXE")).expect("PS-X EXE");
    let raw = std::fs::read(&a[2]).expect("read counts");
    let frames: f64 = a[3].parse().expect("frames");
    let top: usize = a.get(4).map_or(30, |t| t.parse().expect("TOP"));
    let idle: Vec<u32> = a.get(5).map_or(vec![], |s| s.split(',').map(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).expect("IDLE")).collect());
    let count = |pc: u32| {
        let o = ((pc & 0x1F_FFFF) >> 2) as usize * 4;
        u64::from(u32::from_le_bytes(raw[o..o + 4].try_into().unwrap()))
    };
    let word = |pc: u32| {
        let o = (pc - exe.text_addr) as usize;
        u32::from_le_bytes(exe.text[o..o + 4].try_into().unwrap())
    };
    let d = Discovery::run(&exe);
    let t = translate(&exe, &d, &Hazards::check(&exe, &d), 0x0900_0000).expect("translate");
    let mut names: BTreeMap<u32, String> = d.entries.keys().map(|&e| (e, format!("fn_{e:08x}"))).collect();
    for h in psyq().scan(&exe) {
        names.insert(h.addr, format!("{}{}", h.sig.name, if h.rivals.is_empty() { "" } else { "?" }));
    }
    let starts: Vec<u32> = names.keys().copied().collect();
    let of = |pc: u32| starts.partition_point(|&s| s <= pc).checked_sub(1).map(|i| starts[i]);
    let mut fs: BTreeMap<u32, Func> = BTreeMap::new();
    let mut code = d.code.clone();
    code.extend(d.delay_slots.keys());
    for &pc in &code {
        let (Some(f), n) = (of(pc), count(pc)) else { continue };
        let e = fs.entry(f).or_default();
        e.guest += n;
        e.rt += if t.runtime.contains_key(&pc) { n } else { 0 };
        e.ind += if matches!(decode(Insn(word(pc)), Isa::R3000A), Kind::Jr | Kind::Jalr) { n } else { 0 };
    }
    for &(pc, s, end) in &t.spans {
        let Some(f) = of(pc) else { continue };
        let e = fs.entry(f).or_default();
        e.host += count(pc) * u64::from((end - s) / 4);
        e.bytes += end - s;
    }
    let all: u64 = (0..raw.len() / 4).map(|k| count(4 * k as u32)).sum();
    // Loads and stores of the work: alone (their own check), a group's first (one check for the
    // group) or inside a group (one host word each, no span of their own).
    let heads: BTreeMap<u32, u32> = t.spans.iter().map(|&(pc, s, e)| (pc, (e - s) / 4)).collect();
    let (mut alone, mut first, mut inside, mut slot, mut alone_words) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for &pc in &code {
        let f = of(pc).unwrap_or(0);
        if idle.contains(&f) || !matches!(decode(Insn(word(pc)), Isa::R3000A), Kind::Lb | Kind::Lh | Kind::Lwl | Kind::Lw | Kind::Lbu | Kind::Lhu | Kind::Lwr | Kind::Sb | Kind::Sh | Kind::Swl | Kind::Sw | Kind::Swr) {
            continue;
        }
        let n = count(pc);
        match heads.get(&pc) {
            None if d.delay_slots.contains_key(&pc) => slot += n,
            None => inside += n,
            Some(_) if !heads.contains_key(&(pc + 4)) && code.contains(&(pc + 4)) => first += n,
            Some(&w) => (alone, alone_words) = (alone + n, alone_words + n * u64::from(w)),
        }
    }
    let (guest, host) = fs.values().fold((0, 0), |(g, h), f| (g + f.guest, h + f.host));
    let per = |v: u64| v as f64 / frames;
    println!(
        "{}: {:.0} guest insns/frame in RAM, {:.0} ({:.1}%) in this text's code; {:.0} main-pass host words/frame ({:.2} per guest); {} functions",
        a[1],
        per(all),
        per(guest),
        100.0 * guest as f64 / all.max(1) as f64,
        per(host),
        host as f64 / guest.max(1) as f64,
        fs.len()
    );
    let mut v: Vec<(u32, &Func)> = fs.iter().filter(|f| f.1.guest > 0).map(|(&k, f)| (k, f)).collect();
    let (ig, ih) = v.iter().filter(|(k, _)| idle.contains(k)).fold((0, 0), |(g, h), (_, f)| (g + f.guest, h + f.host));
    let work = host - ih;
    if !idle.is_empty() {
        println!(
            "idle (*): {:.0} guest/frame, {:.0} host words/frame ({:.1}%); work: {:.0} guest/frame, {:.0} host words/frame",
            per(ig),
            per(ih),
            100.0 * ih as f64 / host.max(1) as f64,
            per(guest - ig),
            per(work)
        );
    }
    v.sort_by_key(|(_, f)| std::cmp::Reverse(f.host));
    println!("  #   address  function                    guest/fr   host/fr  host%  cum%  h/g   rt/fr  ind/fr  bytes");
    let mut cum = 0u64;
    for (i, (k, f)) in v.iter().enumerate().take(top) {
        cum += f.host;
        println!(
            "{:3}{} {k:08x} {:26} {:9.0} {:9.0} {:5.1} {:5.1} {:4.2} {:7.0} {:7.0} {:6}",
            i + 1,
            if idle.contains(k) { '*' } else { ' ' },
            names[k].chars().take(26).collect::<String>(),
            per(f.guest),
            per(f.host),
            100.0 * f.host as f64 / host.max(1) as f64,
            100.0 * cum as f64 / host.max(1) as f64,
            f.host as f64 / f.guest.max(1) as f64,
            per(f.rt),
            per(f.ind),
            f.bytes
        );
    }
    for p in [50u64, 80, 90, 95] {
        let (mut c, mut b, mut n) = (0u64, 0u32, 0usize);
        for (_, f) in v.iter().filter(|(k, _)| !idle.contains(k)) {
            if c * 100 >= work * p {
                break;
            }
            (c, b, n) = (c + f.host, b + f.bytes, n + 1);
        }
        println!("{p}% of the work's host words: {n} functions, {:.1} KB of main-pass code (I-cache 16 KB)", f64::from(b) / 1024.0);
    }
    println!(
        "loads/stores of the work: {:.0}/frame; alone {:.0} ({:.1} host words each), first of a group {:.0}, inside a group {:.0} (1 word), in a delay slot {:.0}",
        per(alone + first + inside + slot),
        per(alone),
        alone_words as f64 / alone.max(1) as f64,
        per(first),
        per(inside),
        per(slot)
    );
    let lib: Vec<_> = v.iter().filter(|(k, _)| !names[k].starts_with("fn_")).collect();
    let lh: u64 = lib.iter().map(|(_, f)| f.host).sum();
    let lr: u64 = lib.iter().map(|(_, f)| f.rt).sum();
    println!(
        "PsyQ-recognised: {} functions run, {:.1}% of the work's host words, {:.0} runtime calls/frame of {:.0}",
        lib.len(),
        100.0 * lh as f64 / work.max(1) as f64,
        per(lr),
        per(v.iter().map(|(_, f)| f.rt).sum())
    );
}
