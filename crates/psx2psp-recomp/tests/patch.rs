//! The patch table (GOAL.MD S20d): when a game rewrites the immediate or shift amount of a
//! simple instruction in code already translated, writing [`repatch`]'s word at each host address
//! of [`Translation::patch`] must give the code a fresh translation of the rewritten program would
//! have, bit for bit. The fresh translation keeps the first one's Discovery and Hazards, as the
//! runtime does.

use psx2psp_recomp::emit::repatch;
use psx2psp_recomp::{Discovery, Hazards, PsxExe, Translation, translate};

const BASE: u32 = 0x8001_0000;
const CODE: u32 = 0x08A0_0000;

const ZERO: u32 = 0;
const V0: u32 = 2;
const V1: u32 = 3;
const A0: u32 = 4;
const T0: u32 = 8;
const T1: u32 = 9;
const T2: u32 = 10;
const T3: u32 = 11;
const T4: u32 = 12;
const T5: u32 = 13;
const T6: u32 = 14;
const T7: u32 = 15;
const S0: u32 = 16;
const T9: u32 = 25;
const RA: u32 = 31;

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: i32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (imm as u32 & 0xFFFF)
}
const fn at(n: u32) -> u32 {
    BASE + 4 * n
}
const fn j(op: u32, n: u32) -> u32 {
    (op << 26) | ((at(n) >> 2) & 0x03FF_FFFF)
}
const fn b(op: u32, rs: u32, rt: u32, from: u32, to: u32) -> u32 {
    i(op, rs, rt, to as i32 - from as i32 - 1)
}

fn exe(words: &[u32]) -> PsxExe {
    let mut f = vec![0u8; 0x800];
    f[..8].copy_from_slice(b"PS-X EXE");
    for (o, v) in [(0x10, BASE), (0x18, BASE), (0x1C, 4 * words.len() as u32)] {
        f[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    f.extend(words.iter().flat_map(|w| w.to_le_bytes()));
    PsxExe::parse(&f).unwrap()
}

fn word(x: &PsxExe, pc: u32) -> u32 {
    let o = (pc - x.text_addr) as usize;
    u32::from_le_bytes(x.text[o..o + 4].try_into().unwrap())
}

fn set_word(x: &mut PsxExe, pc: u32, w: u32) {
    let o = (pc - x.text_addr) as usize;
    x.text[o..o + 4].copy_from_slice(&w.to_le_bytes());
}

/// Applies the rewrite of `pc` to `t`'s words if every host copy takes it.
fn apply(t: &mut Translation, pc: u32, new: u32) -> Patched {
    let Some(hosts) = t.patch.get(&pc) else { return Patched::NotSimple };
    let words: Option<Vec<u32>> =
        hosts.iter().map(|&h| repatch(t.words[((h - t.base) / 4) as usize], new)).collect();
    let Some(words) = words else { return Patched::Form };
    for (&h, w) in hosts.iter().zip(words) {
        t.words[((h - t.base) / 4) as usize] = w;
    }
    Patched::Yes(hosts.len())
}

#[derive(Debug, PartialEq, Eq)]
enum Patched {
    /// Rewritten at this many host addresses.
    Yes(usize),
    /// Not translated as one plain word: the block goes to the interpreter.
    NotSimple,
    /// Same instruction kind but another form (registers, opcode): to the interpreter.
    Form,
}

#[test]
fn rewritten_immediates_patch_to_a_fresh_translation() {
    let p = [
        i(9, ZERO, T0, 5),                // 0 addiu
        i(0xD, T0, T1, 0x10),             // 1 ori
        r(0, 0, T1, T2, 3),               // 2 sll
        j(3, 12),                         // 3 jal 12
        i(0xA, T2, T3, 7),                // 4 slti, jal slot
        b(4, T0, T1, 5, 8),               // 5 beq -> 8
        i(0xF, 0, T4, 0x1234),            // 6 lui, branch slot and branch target
        i(0xC, T4, T5, 0xFF),             // 7 andi
        b(5, T5, ZERO, 8, 6),             // 8 bne -> 6 (into a delay slot)
        r(3, 0, T5, T6, 2),               // 9 sra, slot
        j(2, 15),                         // 10 j 15
        i(0xE, T6, T7, 3),                // 11 xori, j slot
        i(8, A0, V0, 1),                  // 12 addi (callee)
        r(8, RA, 0, 0, 0),                // 13 jr ra
        r(2, 0, V0, V1, 4),               // 14 srl, jr ra slot
        r(8, T9, 0, 0, 0),                // 15 jr t9
        i(9, S0, S0, -1),                 // 16 addiu, jr slot
    ];
    let x = exe(&p);
    let d = Discovery::run(&x);
    let hz = Hazards::check(&x, &d);
    let mut t = translate(&x, &d, &hz, CODE).unwrap();
    let mut x2 = x.clone();
    for n in 0..p.len() as u32 {
        let w = p[n as usize];
        let new = match w >> 26 {
            8..=0xF => w ^ 0x5A5A,
            0 if matches!(w & 0x3F, 0 | 2 | 3) => w ^ 0x15 << 6,
            _ => continue,
        };
        set_word(&mut x2, at(n), new);
        let got = apply(&mut t, at(n), new);
        assert!(matches!(got, Patched::Yes(_)), "word {n}: {got:?}");
        if n == 6 {
            assert_eq!(got, Patched::Yes(2), "the slot is also jumped to: two copies");
        }
    }
    let t2 = translate(&x2, &d, &hz, CODE).unwrap();
    assert_eq!(t.words, t2.words);
}

#[test]
fn repatch_keeps_the_form() {
    let addiu = i(9, T0, T1, 5);
    assert_eq!(repatch(addiu, i(9, T0, T1, -3)), Some(i(9, T0, T1, -3)));
    assert_eq!(repatch(addiu, i(8, T0, T1, -3)), Some(i(9, T0, T1, -3)), "addi is addiu on host");
    assert_eq!(repatch(addiu, i(9, T0, T2, 5)), None, "another register");
    assert_eq!(repatch(addiu, i(0xD, T0, T1, 5)), None, "another opcode");
    let sll = r(0, 0, T1, T2, 3);
    assert_eq!(repatch(sll, r(0, 0, T1, T2, 7)), Some(r(0, 0, T1, T2, 7)));
    assert_eq!(repatch(sll, r(2, 0, T1, T2, 3)), None, "sll became srl");
    let addu = r(0x21, T0, T1, T2, 0);
    assert_eq!(repatch(addu, r(0x21, T0, T1, T3, 0)), None, "register operands are not patched");
}

/// The rewrites Sheep Dog's LEV02 makes to its own code, from a RAM dump taken in the level:
/// `PSX2PSP_EXES=<dir> PSX2PSP_RAM=<dump> cargo test -p psx2psp-recomp --test patch -- --ignored --nocapture`
/// (`PSX2PSP_EXE` picks the executable, LEV02.EXE by default). Every rewrite of translated code
/// is either patched, and the patched words equal a fresh translation of the rewritten text, or
/// counted as a block for the interpreter.
#[test]
#[ignore = "needs PSX2PSP_EXES and PSX2PSP_RAM"]
fn real_rewrites_patch_to_a_fresh_translation() {
    let dir = std::env::var("PSX2PSP_EXES").expect("PSX2PSP_EXES");
    let name = std::env::var("PSX2PSP_EXE").unwrap_or_else(|_| "LEV02.EXE".into());
    let ram = std::fs::read(std::env::var("PSX2PSP_RAM").expect("PSX2PSP_RAM")).unwrap();
    let x = PsxExe::parse(&std::fs::read(std::path::Path::new(&dir).join(&name)).unwrap()).unwrap();
    let d = Discovery::run(&x);
    let hz = Hazards::check(&x, &d);
    let mut t = translate(&x, &d, &hz, CODE).unwrap();
    let mut x2 = x.clone();
    let o = (x.text_addr & 0x1F_FFFF) as usize;
    let (mut data, mut patched, mut copies, mut not_simple, mut form) = (0, 0, 0, 0, 0);
    for pc in (x.text_addr..x.text_addr + x.text.len() as u32).step_by(4) {
        let k = o + (pc - x.text_addr) as usize;
        let new = u32::from_le_bytes(ram[k..k + 4].try_into().unwrap());
        let old = word(&x, pc);
        if new == old {
            continue;
        }
        if !d.code.contains(&pc) {
            data += 1;
            continue;
        }
        match apply(&mut t, pc, new) {
            Patched::Yes(n) => {
                set_word(&mut x2, pc, new);
                patched += 1;
                copies += n;
            }
            Patched::NotSimple => {
                println!("{pc:08X} {old:08X} -> {new:08X}: not one plain word");
                not_simple += 1;
            }
            Patched::Form => {
                println!("{pc:08X} {old:08X} -> {new:08X}: another form");
                form += 1;
            }
        }
    }
    println!(
        "{name}: code words rewritten {} (patched {patched} at {copies} host words, not simple \
         {not_simple}, another form {form}); data words {data}",
        patched + not_simple + form
    );
    assert!(patched > 0, "the dump has no rewrite of translated code");
    let t2 = translate(&x2, &d, &hz, CODE).unwrap();
    assert_eq!(t.words, t2.words);
}
