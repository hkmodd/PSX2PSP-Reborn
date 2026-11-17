//! Code the game places where it wants (Crash 2's native GOOL code, position-independent):
//! translated once at one guest and host address, then moved, it must be what the translation at
//! the new addresses is, word for word.

use psx2psp_recomp::emit::{Reloc, translate_floating};
use psx2psp_recomp::{Discovery, Hazards, PsxExe, Translation};

const T0: u32 = 8;
const T2: u32 = 10;
const T3: u32 = 11;
const S5: u32 = 21;
const A0: u32 = 4;
const SP: u32 = 29;
const RA: u32 = 31;
const NOP: u32 = 0;

const fn r(funct: u32, rs: u32, rt: u32, rd: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: i32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (imm as u32 & 0xFFFF)
}
/// Conditional branch at word `from` to word `to`.
const fn b(op: u32, rs: u32, rt: u32, from: i32, to: i32) -> u32 {
    i(op, rs, rt, to - from - 1)
}
/// `bal` (`bgezal zero`) at word `from` to word `to`.
const fn bal(from: i32, to: i32) -> u32 {
    i(1, 0, 0x11, to - from - 1)
}
const fn addiu(rt: u32, rs: u32, imm: i32) -> u32 {
    i(9, rs, rt, imm)
}
/// `j`/`jal` to an absolute guest address.
const fn j(op: u32, a: u32) -> u32 {
    (op << 26) | ((a >> 2) & 0x03FF_FFFF)
}

/// Position-independent code: every transfer inside it is relative; it leaves through absolute
/// `j`/`jal`, a `bal` past its end, registers and a branch out.
fn program() -> Vec<u32> {
    vec![
        addiu(SP, SP, -8),            // 0
        i(0x2B, SP, RA, 4),           // 1  sw ra,4(sp)
        i(0x23, A0, T2, 0),           // 2  lw t2,0(a0): RAM or I/O
        bal(3, 12),                   // 3  bal to a function of its own
        NOP,                          // 4
        addiu(T0, 0, 3),              // 5
        addiu(T0, T0, -1),            // 6  loop
        b(5, T0, 0, 7, 6),            // 7  bne t0,zero,loop
        NOP,                          // 8
        j(3, 0x8003_0000),            // 9  jal to the game's code
        NOP,                          // 10
        b(4, T2, 0, 11, 40),          // 11 beq t2,zero past the end ... in the slot of nothing
        NOP,                          // 12 the function bal calls: also the slot above
        r(9, T3, 0, S5),              // 13 jalr s5,t3
        NOP,                          // 14
        bal(15, 60),                  // 15 bal past the end
        NOP,                          // 16
        i(0x23, SP, RA, 4),           // 17 lw ra,4(sp)
        addiu(SP, SP, 8),             // 18
        r(8, RA, 0, 0),               // 19 jr ra
        NOP,                          // 20
        j(2, 0x8004_0000),            // 21 j to the game's code
        NOP,                          // 22
    ]
}

fn fragment(words: &[u32], at: u32) -> PsxExe {
    PsxExe {
        pc0: at,
        gp0: 0,
        text_addr: at,
        text: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        bss: (0, 0),
        stack: (0, 0),
    }
}

fn floating(words: &[u32], guest: u32, host: u32) -> Translation {
    let x = fragment(words, guest);
    let mut d = Discovery::run(&x);
    // Every word is an entry, as the game may jump to any of them.
    let hz = Hazards::check(&x, &d);
    let t = translate_floating(&x, &d, &hz, host).unwrap();
    d.code.clear();
    t
}

#[test]
fn moved_code_is_the_code_translated_there() {
    let w = program();
    let a = floating(&w, 0x8010_0000, 0x08A0_0000);
    for (guest, host) in [(0x8012_3450u32, 0x08B4_5670u32), (0x8000_1000, 0x0880_0400), (0x801F_F000, 0x08FF_0000)] {
        let m = a.moved(guest.wrapping_sub(0x8010_0000), host);
        let d = floating(&w, guest, host);
        assert_eq!(m.words.len(), d.words.len(), "{guest:08X}");
        for (k, (x, y)) in m.words.iter().zip(&d.words).enumerate() {
            assert_eq!(x, y, "{guest:08X}: word {k} at {:08X}", host + 4 * k as u32);
        }
        assert_eq!(m.map, d.map);
        assert_eq!(m.runtime, d.runtime);
        assert_eq!(m.patch, d.patch);
        assert_eq!(m.spans, d.spans);
        assert_eq!(m.relocs, d.relocs);
    }
}

#[test]
fn each_kind_of_relocation_floating_code_has_is_recorded() {
    // Host calls' pcs, the links of jalr s5 and of the bal past the end, the target of the beq
    // past the end, the host jal of the bal inside. No host address in a lui/ori pair: those are
    // ra constants, never in floating code.
    let t = floating(&program(), 0x8010_0000, 0x08A0_0000);
    let n = |k: Reloc| t.relocs.iter().filter(|&&(_, r)| r == k).count();
    assert!(n(Reloc::Guest) > 0 && n(Reloc::Jump) > 0, "{:?}", t.relocs);
    assert_eq!((n(Reloc::GuestPair), n(Reloc::HostPair)), (3, 0), "{:?}", t.relocs);
    // The beq past the end leaves through the table, not the runtime.
    assert!(!t.runtime.contains_key(&(0x8010_0000 + 11 * 4)), "{:?}", t.runtime);
}

#[test]
fn an_absolute_jal_into_the_fragment_is_not_taken_as_its_own() {
    // At 0x80030000 the `jal 0x80030010` of word 0 names word 4 of the fragment, by chance.
    let mut w = program();
    w[9] = j(3, 0x8003_0010);
    let t = floating(&w, 0x8003_0000, 0x08A0_0000);
    let host = |g: u32| t.host(g).unwrap();
    let jal_at = host(0x8003_0000 + 9 * 4);
    let k = ((jal_at - t.base) / 4) as usize;
    // Not a host `jal` (op 3) to the translation: the target goes through the table.
    assert!(t.words[k..k + 6].iter().all(|&x| x >> 26 != 3 || x & 0x03FF_FFFF != (host(0x8003_0010) >> 2) & 0x03FF_FFFF));
}
