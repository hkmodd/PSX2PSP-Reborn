//! Decoder, executable loader and interpreter semantics.

use psx2psp_recomp::{Bus, Cpu, ExeError, Insn, Isa, Kind, PsxExe, Size, Stop, Trap, Uses, decode};

const CODE: u32 = 0x1000;
const DATA: u32 = 0x2000;
const ZERO: u32 = 0;
const V0: u32 = 2;
const V1: u32 = 3;
const A0: u32 = 4;
const A1: u32 = 5;

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | imm as u32
}
const NOP: u32 = 0;
const fn lw(rt: u32, off: u16, base: u32) -> u32 {
    i(0x23, base, rt, off)
}
const fn addiu(rt: u32, rs: u32, imm: u16) -> u32 {
    i(9, rs, rt, imm)
}
const fn addu(rd: u32, rs: u32, rt: u32) -> u32 {
    r(0x21, rs, rt, rd, 0)
}

/// 64 KB of RAM at 0, nothing else.
struct Ram(Vec<u8>);

impl Bus for Ram {
    fn load(&mut self, addr: u32, size: Size) -> Result<u32, Trap> {
        let a = addr as usize;
        let b = self.0.get(a..a + size as usize).ok_or(Trap::Bus { addr })?;
        Ok(b.iter().rev().fold(0, |v, &x| (v << 8) | x as u32))
    }
    fn store(&mut self, addr: u32, size: Size, value: u32) -> Result<(), Trap> {
        let a = addr as usize;
        let b = self.0.get_mut(a..a + size as usize).ok_or(Trap::Bus { addr })?;
        b.copy_from_slice(&value.to_le_bytes()[..size as usize]);
        Ok(())
    }
}

fn setup(isa: Isa, code: &[u32]) -> (Cpu, Ram) {
    let mut ram = Ram(vec![0; 0x10000]);
    for (n, w) in code.iter().enumerate() {
        ram.store(CODE + 4 * n as u32, Size::Word, *w).unwrap();
    }
    for n in 0..8u32 {
        ram.store(DATA + n, Size::Byte, n * 0x11).unwrap();
    }
    (Cpu::new(isa, CODE), ram)
}

fn steps(cpu: &mut Cpu, ram: &mut Ram, n: usize) {
    for _ in 0..n {
        cpu.step(ram).unwrap();
    }
}

#[test]
fn r3000a_load_lands_after_the_delay_slot_allegrex_at_once() {
    let code = [lw(V0, 0, A0), addu(V1, V0, ZERO), addu(A1, V0, ZERO), NOP];
    let word = 0x3322_1100;
    for (isa, seen_in_slot) in [(Isa::R3000A, 7), (Isa::Allegrex, word)] {
        let (mut cpu, mut ram) = setup(isa, &code);
        cpu.gpr[V0 as usize] = 7;
        cpu.gpr[A0 as usize] = DATA;
        steps(&mut cpu, &mut ram, 3);
        assert_eq!(cpu.gpr[V1 as usize], seen_in_slot, "{isa:?}");
        assert_eq!(cpu.gpr[A1 as usize], word, "{isa:?}");
    }
}

#[test]
fn r3000a_write_in_the_delay_slot_cancels_the_load() {
    let (mut cpu, mut ram) = setup(Isa::R3000A, &[lw(V0, 0, A0), addiu(V0, ZERO, 5), NOP]);
    cpu.gpr[A0 as usize] = DATA;
    steps(&mut cpu, &mut ram, 3);
    assert_eq!(cpu.gpr[V0 as usize], 5);
    assert_eq!(cpu.pending, None);
}

#[test]
fn r3000a_second_load_to_the_same_register_drops_the_first() {
    let code = [lw(V0, 0, A0), lw(V0, 4, A0), addu(V1, V0, ZERO), addu(A1, V0, ZERO), NOP];
    let (mut cpu, mut ram) = setup(Isa::R3000A, &code);
    cpu.gpr[V0 as usize] = 7;
    cpu.gpr[A0 as usize] = DATA;
    steps(&mut cpu, &mut ram, 4);
    assert_eq!((cpu.gpr[V1 as usize], cpu.gpr[A1 as usize]), (7, 0x7766_5544));
}

#[test]
fn lwr_lwl_pair_reads_an_unaligned_word_on_both_cpus() {
    let code = [i(0x26, A0, V0, 1), i(0x22, A0, V0, 4), NOP, NOP];
    for isa in [Isa::R3000A, Isa::Allegrex] {
        let (mut cpu, mut ram) = setup(isa, &code);
        cpu.gpr[V0 as usize] = 0xDEAD_BEEF;
        cpu.gpr[A0 as usize] = DATA;
        steps(&mut cpu, &mut ram, 3);
        assert_eq!(cpu.gpr[V0 as usize], 0x4433_2211, "{isa:?}");
    }
}

#[test]
fn swl_swr_pair_writes_an_unaligned_word() {
    let code = [i(0x2E, A0, A1, 3), i(0x2A, A0, A1, 6), NOP];
    let (mut cpu, mut ram) = setup(Isa::R3000A, &code);
    cpu.gpr[A0 as usize] = DATA;
    cpu.gpr[A1 as usize] = 0xAABB_CCDD;
    steps(&mut cpu, &mut ram, 2);
    let bytes: Vec<u32> = (0..8).map(|n| ram.load(DATA + n, Size::Byte).unwrap()).collect();
    assert_eq!(bytes, [0x00, 0x11, 0x22, 0xDD, 0xCC, 0xBB, 0xAA, 0x77]);
}

#[test]
fn branch_runs_its_delay_slot_and_jal_links_past_it() {
    let code = [i(4, 0, 0, 2), addiu(V0, ZERO, 1), addiu(V0, ZERO, 2), addiu(V1, ZERO, 3)];
    let (mut cpu, mut ram) = setup(Isa::R3000A, &code);
    steps(&mut cpu, &mut ram, 3);
    assert_eq!((cpu.gpr[V0 as usize], cpu.gpr[V1 as usize], cpu.pc), (1, 3, CODE + 16));

    let (mut cpu, mut ram) = setup(Isa::R3000A, &[(3 << 26) | (0x1100 >> 2), NOP]);
    steps(&mut cpu, &mut ram, 2);
    assert_eq!((cpu.pc, cpu.gpr[31]), (0x1100, CODE + 8));
}

#[test]
fn allegrex_branch_likely_annuls_its_delay_slot_when_not_taken() {
    let code = [i(0x14, A0, ZERO, 2), addiu(V0, ZERO, 1), addiu(V1, ZERO, 2)];
    let (mut cpu, mut ram) = setup(Isa::Allegrex, &code);
    cpu.gpr[A0 as usize] = 1;
    steps(&mut cpu, &mut ram, 2);
    assert_eq!((cpu.gpr[V0 as usize], cpu.gpr[V1 as usize]), (0, 2));
    assert_eq!(decode(Insn(code[0]), Isa::R3000A), Kind::Invalid);
}

#[test]
fn division_by_zero_is_the_r3000a_result_and_undefined_on_the_allegrex() {
    let div = [r(0x1A, A0, A1, 0, 0), NOP];
    for (n, lo) in [(7u32, u32::MAX), ((-7i32) as u32, 1)] {
        let (mut cpu, mut ram) = setup(Isa::R3000A, &div);
        cpu.gpr[A0 as usize] = n;
        steps(&mut cpu, &mut ram, 1);
        assert_eq!((cpu.hi, cpu.lo), (n, lo));
    }
    let (mut cpu, mut ram) = setup(Isa::R3000A, &div);
    cpu.gpr[A0 as usize] = 0x8000_0000;
    cpu.gpr[A1 as usize] = u32::MAX;
    steps(&mut cpu, &mut ram, 1);
    assert_eq!((cpu.hi, cpu.lo), (0, 0x8000_0000));

    let (mut cpu, mut ram) = setup(Isa::Allegrex, &div);
    let before = cpu.clone();
    assert_eq!(cpu.step(&mut ram), Err(Stop { pc: CODE, trap: Trap::Undefined }));
    assert_eq!(cpu, before);
}

#[test]
fn a_stop_leaves_the_cpu_as_it_was() {
    let code = [lw(V0, 0, A0), r(0x20, A0, A1, V1, 0), NOP];
    let (mut cpu, mut ram) = setup(Isa::R3000A, &code);
    cpu.gpr[A0 as usize] = 0x0010_0000;
    cpu.gpr[A1 as usize] = 1;
    // The load faults first (outside RAM): nothing moves.
    let before = cpu.clone();
    assert_eq!(cpu.step(&mut ram), Err(Stop { pc: CODE, trap: Trap::Bus { addr: 0x0010_0000 } }));
    assert_eq!(cpu, before);
    cpu.gpr[A0 as usize] = DATA + 1;
    assert_eq!(cpu.step(&mut ram), Err(Stop { pc: CODE, trap: Trap::Unaligned { addr: DATA + 1 } }));
    assert_eq!(cpu, Cpu { gpr: { let mut g = before.gpr; g[A0 as usize] = DATA + 1; g }, ..before });

    cpu.gpr[A0 as usize] = DATA;
    steps(&mut cpu, &mut ram, 1);
    cpu.gpr[A0 as usize] = 0x7FFF_FFFF;
    let before = cpu.clone();
    assert!(before.pending.is_some());
    assert_eq!(cpu.step(&mut ram), Err(Stop { pc: CODE + 4, trap: Trap::Overflow }));
    assert_eq!(cpu, before);
}

#[test]
fn allegrex_ext_ins_seb() {
    let ext = (0x1F << 26) | (A0 << 21) | (V0 << 16) | (7 << 11) | (4 << 6);
    let ins = (0x1F << 26) | (A0 << 21) | (V1 << 16) | (11 << 11) | (8 << 6) | 4;
    let seb = (0x1F << 26) | (A0 << 16) | (A1 << 11) | (0x10 << 6) | 0x20;
    let (mut cpu, mut ram) = setup(Isa::Allegrex, &[ext, ins, seb]);
    cpu.gpr[A0 as usize] = 0xABCD_1284;
    cpu.gpr[V1 as usize] = u32::MAX;
    steps(&mut cpu, &mut ram, 3);
    assert_eq!(cpu.gpr[V0 as usize], 0x28);
    assert_eq!(cpu.gpr[V1 as usize], 0xFFFF_F4FF);
    assert_eq!(cpu.gpr[A1 as usize], 0xFFFF_FF84);
}

#[test]
fn decoding_is_strict_where_the_two_cpus_differ() {
    let srl = r(2, 0, A0, V0, 3);
    let rotr = r(2, 1, A0, V0, 3);
    let srlv_sa = r(6, A1, A0, V0, 1);
    for isa in [Isa::R3000A, Isa::Allegrex] {
        assert_eq!(decode(Insn(srl), isa), Kind::Srl);
        assert_eq!(decode(Insn(rotr), isa), Kind::Invalid);
        assert_eq!(decode(Insn(srlv_sa), isa), Kind::Invalid);
    }
    let lwc2 = i(0x32, A0, 3, 8);
    let gte_rtps = 0x4A18_0001;
    let movz = r(0x0A, A0, A1, V0, 0);
    assert_eq!(decode(Insn(lwc2), Isa::R3000A), Kind::Lwc2);
    assert_eq!(decode(Insn(lwc2), Isa::Allegrex), Kind::Invalid);
    assert_eq!(decode(Insn(gte_rtps), Isa::R3000A), Kind::Gte);
    assert_eq!(decode(Insn(gte_rtps), Isa::Allegrex), Kind::Invalid);
    assert_eq!(decode(Insn(movz), Isa::R3000A), Kind::Invalid);
    assert_eq!(decode(Insn(movz), Isa::Allegrex), Kind::Movz);

    let lwl = Insn(i(0x22, A0, V0, 4));
    let u = Uses::of(lwl, decode(lwl, Isa::R3000A));
    assert_eq!(u, Uses { read: (1 << A0) | (1 << V0), write: 1 << V0 });
    assert!(decode(lwl, Isa::R3000A).is_delayed_load());
}

/// Real executables, not in the repo: `PSX2PSP_EXES=<folder of a game's PS-X EXEs> cargo test
/// -p psx2psp-recomp -- --ignored`. For Sheep Dog 'n' Wolf (SLES-02895) it also checks a known
/// instruction (the physics clamp of `resources/patches.ini`).
#[test]
#[ignore = "needs PSX2PSP_EXES"]
fn real_executables_parse() {
    let dir = std::env::var("PSX2PSP_EXES").expect("PSX2PSP_EXES");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let path = e.unwrap().path();
        let exe = PsxExe::parse(&std::fs::read(&path).unwrap())
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let entry = Insn(exe.word(exe.pc0).unwrap());
        assert_ne!(decode(entry, Isa::R3000A), Kind::Invalid, "{}", path.display());
        if path.file_name().unwrap() == "LEV00.EXE" {
            let clamp = Insn(exe.word(0x8003_61E4).unwrap());
            assert_eq!(decode(clamp, Isa::R3000A), Kind::Slti);
            assert_eq!((clamp.rs(), clamp.rt(), clamp.imm()), (A0 as usize, 1, 0xAB));
        }
        n += 1;
    }
    assert!(n > 0);
    println!("{n} executables");
}

#[test]
fn psx_exe_header_and_text() {
    let mut f = vec![0u8; 0x800 + 8];
    f[..8].copy_from_slice(b"PS-X EXE");
    for (o, v) in [(0x10, 0x8001_0004u32), (0x14, 0x8009_0000), (0x18, 0x8001_0000), (0x1C, 8)] {
        f[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    f[0x804..0x808].copy_from_slice(&0x2402_0001u32.to_le_bytes());
    let exe = PsxExe::parse(&f).unwrap();
    assert_eq!((exe.pc0, exe.gp0, exe.text_end()), (0x8001_0004, 0x8009_0000, 0x8001_0008));
    assert_eq!(exe.word(0x8001_0004), Some(0x2402_0001));
    assert_eq!(exe.word(0x8001_0008), None);

    assert_eq!(PsxExe::parse(&f[..0x804]), Err(ExeError::Truncated { need: 0x808, have: 0x804 }));
    f[0x10..0x14].copy_from_slice(&0x8002_0000u32.to_le_bytes());
    assert_eq!(PsxExe::parse(&f), Err(ExeError::Entry(0x8002_0000)));
    f[0] = b'X';
    assert_eq!(PsxExe::parse(&f), Err(ExeError::Magic));
}
