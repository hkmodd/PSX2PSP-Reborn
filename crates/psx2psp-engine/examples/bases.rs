//! `cargo run --release -p psx2psp-engine --example bases -- EXE COUNTS FRAMES [IDLE,...]`: the loads
//! and stores of EXE's translated code by what is known of their base register, weighted by the
//! guest instructions the interpreter ran at each RAM word over FRAMES frames (`psx2psp-sim
//! --trace`'s FILE.cnt): how many of the emitter's RAM checks a static proof could remove.
//!
//! Classes, from a backward scan inside the access's straight-line code (stopping at a transfer
//! target or a call): `sp`, `gp`, `const` (the base is a `lui` chain: RAM or not), `again` (the
//! same definition of the base was already accessed earlier in the scan: its check is repeated),
//! `other`. Each by where the emitter puts it: alone (its own six-word check), first of a group
//! (one check for the group), inside a group (one word, nothing to remove), delay slot.
//! Then every instruction of the program that writes `sp` or `gp` other than `addiu sp,sp,imm`:
//! what a proof that they stay in RAM has to cover.

use psx2psp_recomp::{Discovery, Hazards, Insn, Isa, JumpReg, Kind, PsxExe, Uses, decode, translate};
use std::collections::{BTreeMap, BTreeSet};

const CLASSES: [&str; 6] = ["sp", "gp", "const RAM", "const other", "again", "other"];
const PLACES: [&str; 4] = ["alone", "first", "inside", "slot"];

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let exe = PsxExe::parse(&std::fs::read(&a[1]).expect("read EXE")).expect("PS-X EXE");
    let raw = std::fs::read(&a[2]).expect("read counts");
    let frames: f64 = a[3].parse().expect("frames");
    let idle: Vec<u32> = a.get(4).map_or(vec![], |s| s.split(',').map(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).expect("IDLE")).collect());
    let count = |pc: u32| {
        let o = ((pc & 0x1F_FFFF) >> 2) as usize * 4;
        u64::from(u32::from_le_bytes(raw[o..o + 4].try_into().unwrap()))
    };
    let at = |pc: u32| {
        let i = Insn(exe.word(pc).unwrap_or(0));
        (i, decode(i, Isa::R3000A))
    };
    let d = Discovery::run(&exe);
    let t = translate(&exe, &d, &Hazards::check(&exe, &d), 0x0900_0000).expect("translate");
    let mut code = d.code.clone();
    code.extend(d.delay_slots.keys());
    // Where control can arrive other than by falling through.
    let mut targets: BTreeSet<u32> = d.entries.keys().copied().collect();
    targets.extend(d.calls.keys());
    targets.extend(d.indirect_calls.values().flatten());
    for j in d.jumps.values() {
        match j {
            JumpReg::Table { targets: ts, .. } => targets.extend(ts),
            JumpReg::Const(c) => {
                targets.insert(*c);
            }
            JumpReg::Unresolved(_) => {}
        }
    }
    use Kind::*;
    for &p in &code {
        match at(p) {
            (i, J | Jal) => {
                targets.insert(i.jump_target(p));
            }
            (i, Beq | Bne | Blez | Bgtz | Bltz | Bgez | Bltzal | Bgezal) => {
                targets.insert(i.branch_target(p));
                targets.insert(p + 8);
            }
            _ => {}
        }
    }
    let mut starts: Vec<u32> = d.entries.keys().copied().collect();
    starts.sort();
    let func = |pc: u32| starts.partition_point(|&s| s <= pc).checked_sub(1).map(|i| starts[i]);
    let mem = |k: Kind| matches!(k, Lb | Lh | Lwl | Lw | Lbu | Lhu | Lwr | Sb | Sh | Swl | Sw | Swr);
    let call = |k: Kind| matches!(k, Jal | Jalr | Bltzal | Bgezal);
    // The value of `r` just before `pc`, when a lui chain in the straight-line code gives it.
    let konst = |mut pc: u32, r: usize| -> Option<u32> {
        let mut want = r;
        let mut add: u32 = 0;
        for _ in 0..64 {
            if targets.contains(&pc) {
                return None;
            }
            pc -= 4;
            let (i, k) = at(pc);
            if call(k) || !code.contains(&pc) {
                return None;
            }
            if Uses::of(i, k).write & (1 << want) == 0 {
                continue;
            }
            match k {
                Lui => return Some((i.imm() << 16).wrapping_add(add)),
                Addiu | Addi if i.rt() == want => {
                    add = add.wrapping_add(i.simm());
                    want = i.rs();
                }
                Ori if i.rt() == want && i.rs() == want => add |= i.imm(),
                _ => return None,
            }
            if want == 0 {
                return Some(add);
            }
        }
        None
    };
    // True when the definition of `r` reaching `pc` was already the base of an access after it.
    let again = |mut pc: u32, r: usize| -> bool {
        for _ in 0..256 {
            if targets.contains(&pc) {
                return false;
            }
            pc -= 4;
            if !code.contains(&pc) {
                return false;
            }
            let (i, k) = at(pc);
            if call(k) {
                return false;
            }
            let u = Uses::of(i, k);
            if mem(k) && i.rs() == r {
                // A load into its own base defines it after the access.
                return u.write & (1 << r) == 0;
            }
            if u.write & (1 << r) != 0 {
                return false;
            }
        }
        false
    };
    let heads: BTreeMap<u32, u32> = t.spans.iter().map(|&(pc, s, e)| (pc, (e - s) / 4)).collect();
    let mut tab = [[0u64; 4]; 6];
    let mut words = [[0u64; 4]; 6];
    for &pc in &code {
        let (i, k) = at(pc);
        if !mem(k) || func(pc).is_some_and(|f| idle.contains(&f)) {
            continue;
        }
        let n = count(pc);
        if n == 0 {
            continue;
        }
        let place = match heads.get(&pc) {
            None if d.delay_slots.contains_key(&pc) => 3,
            None => 2,
            Some(_) if !heads.contains_key(&(pc + 4)) && code.contains(&(pc + 4)) => 1,
            Some(_) => 0,
        };
        let r = i.rs();
        let class = match r {
            29 => 0,
            28 => 1,
            _ => match konst(pc, r) {
                Some(c) => {
                    let addr = c.wrapping_add(i.simm());
                    if addr & 0x0080_0000 == 0 && matches!(addr >> 29, 0 | 4 | 5) { 2 } else { 3 }
                }
                None if again(pc, r) => 4,
                None => 5,
            },
        };
        tab[class][place] += n;
        words[class][place] += n * u64::from(heads.get(&pc).copied().unwrap_or(1));
    }
    let per = |v: u64| v as f64 / frames;
    let all: u64 = tab.iter().flatten().sum();
    println!("{}: {:.0} loads/stores per frame (idle left out)", a[1], per(all));
    println!("  class          {:>9} {:>9} {:>9} {:>9}   host words/frame", PLACES[0], PLACES[1], PLACES[2], PLACES[3]);
    for (c, row) in tab.iter().enumerate() {
        println!(
            "  {:13} {:9.0} {:9.0} {:9.0} {:9.0}   {:9.0}",
            CLASSES[c],
            per(row[0]),
            per(row[1]),
            per(row[2]),
            per(row[3]),
            per(words[c].iter().sum())
        );
    }
    println!("writes of sp/gp other than addiu sp,sp,imm:");
    for &pc in &code {
        let (i, k) = at(pc);
        let w = Uses::of(i, k).write;
        for r in [28usize, 29] {
            if w & (1 << r) != 0 && !(r == 29 && matches!(k, Addiu | Addi) && i.rs() == 29) {
                println!(
                    "  {pc:08x} {:08x} {k:?} rs={} rt={} imm={:04x} {} runs {:.1}/frame",
                    i.0,
                    i.rs(),
                    i.rt(),
                    i.imm(),
                    if r == 29 { "sp" } else { "gp" },
                    per(count(pc))
                );
            }
        }
    }
}
