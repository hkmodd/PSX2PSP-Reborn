//! Differential tests of the emitter: each program runs on the R3000A interpreter (the oracle)
//! and, translated, on the Allegrex interpreter; registers and memory must match. The test plays
//! the runtime: it serves host calls, dispatch misses and the interpreter hand-over.

use std::collections::BTreeMap;

use psx2psp_recomp::emit::RESUME;
use psx2psp_recomp::layout::{self, K0, MISS, RT_CALL, ctx};
use psx2psp_recomp::{
    Bus, Cpu, Discovery, Hazards, Isa, PsxExe, Reason, Size, Stop, Translation, Trap, translate,
};

const BASE: u32 = 0x8001_0000;
const CODE: u32 = 0x08A0_0000;
const HI: u32 = layout::SCRATCH;
const IO: u32 = 0x1F80_1000;

const ZERO: u32 = 0;
const AT: u32 = 1;
const V0: u32 = 2;
const V1: u32 = 3;
const A0: u32 = 4;
const A1: u32 = 5;
const A2: u32 = 6;
const A3: u32 = 7;
const T0: u32 = 8;
const T1: u32 = 9;
const T2: u32 = 10;
const T3: u32 = 11;
const T4: u32 = 12;
const T5: u32 = 13;
const T6: u32 = 14;
const T7: u32 = 15;
const S0: u32 = 16;
const S1: u32 = 17;
const S2: u32 = 18;
const S3: u32 = 19;
const S4: u32 = 20;
const S5: u32 = 21;
const S6: u32 = 22;
const S7: u32 = 23;
const T8: u32 = 24;
const T9: u32 = 25;
const K0G: u32 = 26;
const K1G: u32 = 27;
const SP: u32 = 29;
const RA: u32 = 31;
const NOP: u32 = 0;

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: i32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (imm as u32 & 0xFFFF)
}
const fn at(n: u32) -> u32 {
    BASE + 4 * n
}
/// `j`/`jal` to word `n`.
const fn j(op: u32, n: u32) -> u32 {
    (op << 26) | ((at(n) >> 2) & 0x03FF_FFFF)
}
/// Conditional branch at word `from` to word `to`.
const fn b(op: u32, rs: u32, rt: u32, from: u32, to: u32) -> u32 {
    i(op, rs, rt, to as i32 - from as i32 - 1)
}
const fn addiu(rt: u32, rs: u32, imm: i32) -> u32 {
    i(9, rs, rt, imm)
}
const fn addu(rd: u32, rs: u32, rt: u32) -> u32 {
    r(0x21, rs, rt, rd, 0)
}
const fn lui(rt: u32, imm: u32) -> u32 {
    i(0xF, 0, rt, imm as i32)
}
const fn ori(rt: u32, rs: u32, imm: u32) -> u32 {
    i(0xD, rs, rt, imm as i32)
}
/// `mfc0` (`rs` = 0) or `mtc0` (`rs` = 4).
const fn cop0(rs: u32, rt: u32, rd: u32) -> u32 {
    (0x10 << 26) | (rs << 21) | (rt << 16) | (rd << 11)
}
const fn mem(op: u32, rt: u32, off: i32, base: u32) -> u32 {
    i(op, base, rt, off)
}
const LB: u32 = 0x20;
const LH: u32 = 0x21;
const LWL: u32 = 0x22;
const LW: u32 = 0x23;
const LBU: u32 = 0x24;
const LHU: u32 = 0x25;
const LWR: u32 = 0x26;
const SB: u32 = 0x28;
const SH: u32 = 0x29;
const SWL: u32 = 0x2A;
const SW: u32 = 0x2B;
const SWR: u32 = 0x2E;

fn exe(words: &[u32]) -> PsxExe {
    let mut f = vec![0u8; 0x800];
    f[..8].copy_from_slice(b"PS-X EXE");
    for (o, v) in [(0x10, BASE), (0x18, BASE), (0x1C, 4 * words.len() as u32)] {
        f[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    f.extend(words.iter().flat_map(|w| w.to_le_bytes()));
    PsxExe::parse(&f).unwrap()
}

/// PS1 memory plus the host-only area (context, dispatch table) above the scratchpad.
#[derive(Clone, PartialEq, Eq)]
struct Machine {
    ram: Vec<u8>,
    /// `layout::SCRATCH .. layout::END`: scratchpad first.
    hi: Vec<u8>,
    io: Vec<u8>,
    code: Vec<u32>,
}

impl Machine {
    fn new(x: &PsxExe) -> Self {
        let mut m = Machine {
            ram: vec![0; 0x20_0000],
            hi: vec![0; (layout::END - HI) as usize],
            io: vec![0; 0x2000],
            code: Vec::new(),
        };
        let o = (x.text_addr & 0x1F_FFFF) as usize;
        m.ram[o..o + x.text.len()].copy_from_slice(&x.text);
        m
    }
    fn word(&self, host: u32) -> u32 {
        let o = (host - HI) as usize;
        u32::from_le_bytes(self.hi[o..o + 4].try_into().unwrap())
    }
    fn set_word(&mut self, host: u32, v: u32) {
        let o = (host - HI) as usize;
        self.hi[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn ctx(&self, off: u32) -> u32 {
        self.word(layout::CTX + off)
    }
    fn set_ctx(&mut self, off: u32, v: u32) {
        self.set_word(layout::CTX + off, v)
    }
    /// The PS1 side of the memory, for comparisons.
    fn ps1(&self) -> (&[u8], &[u8], &[u8]) {
        (&self.ram, &self.hi[..0x400], &self.io)
    }
}

fn rd(b: &[u8], o: usize, size: Size) -> u32 {
    (0..size as usize).fold(0, |v, k| v | (b[o + k] as u32) << (8 * k))
}
fn wr(b: &mut [u8], o: usize, size: Size, v: u32) {
    for k in 0..size as usize {
        b[o + k] = (v >> (8 * k)) as u8;
    }
}

/// The machine as the PS1 sees it.
struct Ps1<'a>(&'a mut Machine);

impl Ps1<'_> {
    fn map(&mut self, addr: u32) -> Option<(&mut [u8], usize)> {
        if addr >= 0xC000_0000 {
            return None;
        }
        let p = addr & 0x1FFF_FFFF;
        match p {
            0..0x80_0000 => Some((&mut self.0.ram, (p & 0x1F_FFFF) as usize)),
            0x1F80_0000..0x1F80_0400 => Some((&mut self.0.hi, (p & 0x3FF) as usize)),
            0x1F80_1000..0x1F80_3000 => Some((&mut self.0.io, (p - IO) as usize)),
            _ => None,
        }
    }
}

impl Bus for Ps1<'_> {
    fn load(&mut self, addr: u32, size: Size) -> Result<u32, Trap> {
        let (b, o) = self.map(addr).ok_or(Trap::Bus { addr })?;
        Ok(rd(b, o, size))
    }
    fn store(&mut self, addr: u32, size: Size, value: u32) -> Result<(), Trap> {
        let (b, o) = self.map(addr).ok_or(Trap::Bus { addr })?;
        wr(b, o, size, value);
        Ok(())
    }
}

/// The machine as the translated code sees it.
struct Psp<'a>(&'a mut Machine);

impl Bus for Psp<'_> {
    fn load(&mut self, addr: u32, size: Size) -> Result<u32, Trap> {
        let m = &mut *self.0;
        match addr {
            layout::RAM..0x09A0_0000 => Ok(rd(&m.ram, (addr - layout::RAM) as usize, size)),
            HI..layout::END => Ok(rd(&m.hi, (addr - HI) as usize, size)),
            a if a >= CODE && ((a - CODE) / 4) < m.code.len() as u32 && size == Size::Word => {
                Ok(m.code[((a - CODE) / 4) as usize])
            }
            _ => Err(Trap::Bus { addr }),
        }
    }
    fn store(&mut self, addr: u32, size: Size, value: u32) -> Result<(), Trap> {
        let m = &mut *self.0;
        match addr {
            layout::RAM..0x09A0_0000 => wr(&mut m.ram, (addr - layout::RAM) as usize, size, value),
            HI..layout::END => wr(&mut m.hi, (addr - HI) as usize, size, value),
            _ => return Err(Trap::Bus { addr }),
        }
        Ok(())
    }
}

/// What the test's "BIOS" does for `syscall`: v0 = a0 ^ 0x5C.
fn syscall(c: &mut Cpu) {
    c.gpr[V0 as usize] = c.gpr[A0 as usize] ^ 0x5C;
}

/// Runs the guest from `c.pc` to `end` on the R3000A interpreter.
fn run_ps1(m: &mut Machine, c: &mut Cpu, end: u32) -> Result<(), String> {
    for _ in 0..100_000 {
        if c.pc == end && c.next_pc == end + 4 {
            if let Some((r, v)) = c.pending.take() {
                c.gpr[r] = v;
            }
            return Ok(());
        }
        match c.step(&mut Ps1(m)) {
            Ok(()) => {}
            Err(Stop { pc, trap: Trap::Syscall { .. } }) => {
                syscall(c);
                c.jump(pc + 4);
            }
            Err(s) => return Err(format!("oracle stopped: {s:x?}")),
        }
    }
    Err("oracle did not reach the end".into())
}

/// What happened on the translated side besides plain execution.
#[derive(Debug, Default)]
struct Log {
    calls: BTreeMap<u32, usize>,
    polls: usize,
    resumed: Option<u32>,
}

/// Runs the translation from guest `entry` to guest `end` on the Allegrex interpreter. Returns
/// the guest state at the end.
fn run_psp(m: &mut Machine, t: &Translation, init: &Cpu, end: u32) -> Result<(Cpu, Log), String> {
    m.code = t.words.clone();
    for g in (0..0x20_0000).step_by(4) {
        m.set_word(layout::table_entry(g), MISS);
    }
    for (e, v) in t.table() {
        m.set_word(e, v);
    }
    let end_host = t.host(end).ok_or("end not translated")?;
    let mut h = Cpu::new(Isa::Allegrex, t.host(init.pc).ok_or("entry not translated")?);
    h.gpr = init.gpr;
    h.hi = init.hi;
    h.lo = init.lo;
    for r in [K0G, K1G] {
        m.set_ctx(ctx::GPR + 4 * r, init.gpr[r as usize]);
    }
    h.gpr[K0G as usize] = K0;
    h.gpr[K1G as usize] = 0;
    // Guest-side CPU for instructions the runtime executes (keeps COP0 across calls).
    let mut g = init.clone();
    let mut log = Log::default();
    let to_guest = |h: &Cpu, m: &Machine, g: &mut Cpu| {
        g.gpr = h.gpr;
        g.gpr[K0G as usize] = m.ctx(ctx::GPR + 4 * K0G);
        g.gpr[K1G as usize] = m.ctx(ctx::GPR + 4 * K1G);
        g.hi = h.hi;
        g.lo = h.lo;
    };
    for _ in 0..200_000 {
        if h.pc == end_host && h.next_pc == end_host + 4 {
            to_guest(&h, m, &mut g);
            return Ok((g, log));
        }
        if h.pc == MISS {
            return Err(format!("dispatch miss to {:#010x}", m.ctx(ctx::TARGET)));
        }
        if h.pc == RT_CALL {
            let ra = h.gpr[RA as usize];
            let (pc, word) = (Psp(m).load(ra, Size::Word).unwrap(), Psp(m).load(ra + 4, Size::Word).unwrap());
            h.gpr[RA as usize] = m.ctx(ctx::GPR + 4 * RA);
            to_guest(&h, m, &mut g);
            if pc & RESUME != 0 {
                log.resumed = Some(pc & !RESUME);
                g.jump(pc & !RESUME);
                run_ps1(m, &mut g, end)?;
                return Ok((g, log));
            }
            if m.ctx(ctx::IRQ) != 0 {
                m.set_ctx(ctx::IRQ, 0);
                log.polls += 1;
            }
            if word != NOP {
                *log.calls.entry(pc).or_default() += 1;
                assert_eq!(Ps1(m).load(pc, Size::Word).unwrap(), word, "host call word");
                g.jump(pc);
                match g.step(&mut Ps1(m)) {
                    Ok(()) => {}
                    Err(Stop { trap: Trap::Syscall { .. }, .. }) => syscall(&mut g),
                    Err(s) => return Err(format!("runtime stopped: {s:x?}")),
                }
                if let Some((r, v)) = g.pending.take() {
                    g.gpr[r] = v;
                }
            }
            h.gpr = g.gpr;
            h.gpr[K0G as usize] = K0;
            for r in [K0G, K1G] {
                m.set_ctx(ctx::GPR + 4 * r, g.gpr[r as usize]);
            }
            h.hi = g.hi;
            h.lo = g.lo;
            h.jump(ra + 8);
            continue;
        }
        h.step(&mut Psp(m)).map_err(|s| format!("translated code stopped: {s:x?}"))?;
    }
    Err("translated code did not reach the end".into())
}

struct Run {
    t: Translation,
    log: Log,
}

/// Translates `words`, runs both sides from `BASE` (registers from `init`) to word `end` and
/// compares them.
fn check(words: &[u32], end: u32, init: impl Fn(&mut Cpu), irq: bool) -> Run {
    let x = exe(words);
    let d = Discovery::run(&x);
    let hz = Hazards::check(&x, &d);
    let t = translate(&x, &d, &hz, CODE).unwrap();
    let mut c = Cpu::new(Isa::R3000A, BASE);
    init(&mut c);
    let start = c.clone();
    let mut m1 = Machine::new(&x);
    let mut m2 = m1.clone();
    run_ps1(&mut m1, &mut c, at(end)).unwrap();
    if irq {
        m2.set_ctx(ctx::IRQ, 1);
    }
    let (g, log) = run_psp(&mut m2, &t, &start, at(end)).unwrap();
    for r in 1..32 {
        assert_eq!(g.gpr[r], c.gpr[r], "r{r}: translated {:#x}, oracle {:#x}", g.gpr[r], c.gpr[r]);
    }
    assert_eq!((g.hi, g.lo), (c.hi, c.lo), "hi/lo");
    let ((r1, s1, i1), (r2, s2, i2)) = (m1.ps1(), m2.ps1());
    assert!(r1 == r2, "RAM differs");
    assert!(s1 == s2, "scratchpad differs");
    assert!(i1 == i2, "I/O differs");
    Run { t, log }
}

#[test]
fn arithmetic_and_division_guards() {
    let w = [
        lui(T0, 0x8000),          // 0  t0 = i32::MIN
        addiu(T1, ZERO, -1),      // 1
        r(0x1A, T0, T1, 0, 0),    // 2  div t0,t1  (MIN / -1)
        r(0x12, 0, 0, S0, 0),     // 3  mflo s0
        r(0x10, 0, 0, S1, 0),     // 4  mfhi s1
        addiu(T2, ZERO, 7),       // 5
        r(0x1A, T2, ZERO, 0, 0),  // 6  div t2,zero
        r(0x12, 0, 0, S2, 0),     // 7
        r(0x10, 0, 0, S3, 0),     // 8
        addiu(T3, ZERO, -9),      // 9
        r(0x1A, T3, ZERO, 0, 0),  // 10 div t3,zero
        r(0x12, 0, 0, S4, 0),     // 11
        r(0x1B, T2, ZERO, 0, 0),  // 12 divu t2,zero
        r(0x12, 0, 0, S5, 0),     // 13
        r(0x10, 0, 0, S6, 0),     // 14
        addiu(A0, ZERO, 100),     // 15
        addiu(A1, ZERO, -7),      // 16
        r(0x1A, A0, A1, 0, 0),    // 17 div a0,a1
        r(0x12, 0, 0, S7, 0),     // 18
        r(0x10, 0, 0, V0, 0),     // 19
        r(0x20, A0, A1, V1, 0),   // 20 add v1,a0,a1
        i(8, A0, A2, -50),        // 21 addi a2,a0,-50
        r(0x22, A0, A1, A3, 0),   // 22 sub a3,a0,a1
        r(0x19, A0, A1, 0, 0),    // 23 multu a0,a1
        r(0x10, 0, 0, T4, 0),     // 24
        r(0x12, 0, 0, T5, 0),     // 25
        r(0x07, A0, A1, T6, 0),   // 26 srav t6,a1,a0
        i(0xB, A1, T7, 5),        // 27 sltiu t7,a1,5
        r(0x27, A0, A1, T8, 0),   // 28 nor t8,a0,a1
        r(0x1B, A0, A1, 0, 0),    // 29 divu a0,a1
        r(0x12, 0, 0, T9, 0),     // 30
        j(2, 31),                 // 31 end: j end
        NOP,                      // 32
    ];
    let run = check(&w, 31, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
}

#[test]
fn loads_and_stores_through_every_segment() {
    let w = [
        lui(A0, 0x8002),           // 0  RAM, KSEG0
        lui(A1, 0x1F80),           // 1  scratchpad
        ori(A2, A1, 0x1000),       // 2  I/O
        lui(A3, 0xA002),           // 3  KSEG1 mirror of a0
        lui(T9, 0x0062),           // 4  KUSEG mirror at 6 MB
        addiu(T0, ZERO, 0x1234),   // 5
        mem(SW, T0, 0, A0),        // 6
        mem(SH, T0, 6, A0),        // 7
        mem(SB, T0, 9, A0),        // 8
        mem(LW, T1, 0, A3),        // 9
        mem(LHU, T2, 6, T9),       // 10
        mem(LB, T3, 9, A0),        // 11
        lui(T4, 0x89AB),           // 12
        ori(T4, T4, 0xCDEF),       // 13
        mem(SW, T4, 16, A0),       // 14
        mem(LWL, T5, 19, A0),      // 15
        mem(LWR, T5, 17, A0),      // 16
        mem(SWL, T4, 33, A0),      // 17
        mem(SWR, T4, 30, A0),      // 18
        mem(SW, T0, 0x3FC, A1),    // 19 scratchpad, last word
        mem(LW, T6, 0x3FC, A1),    // 20
        mem(SW, T4, 4, A1),        // 21
        mem(LH, T7, 6, A1),        // 22
        mem(SW, T4, 0, A2),        // 23 I/O store
        mem(LW, T8, 0, A2),        // 24 I/O load
        mem(LBU, S0, 1, A2),       // 25
        mem(SWL, T4, 0x3FE, A1),   // 26 scratchpad, unaligned
        NOP,                       // 27
        j(2, 28),                  // 28 end
        NOP,                       // 29
    ];
    let run = check(&w, 28, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
    // Only the I/O accesses reach the runtime.
    let io: Vec<u32> = run.log.calls.keys().copied().collect();
    assert_eq!(io, [at(23), at(24), at(25)]);
}

/// Calls, returns with a memory access in the slot, a counted loop, a switch through a jump
/// table, `jalr`, a branch whose slot rewrites its own operand, and `bgezal`.
fn control_program() -> Vec<u32> {
    const F: u32 = 46;
    const G: u32 = 49;
    const H: u32 = 52;
    const TABLE: u32 = 55;
    let mut w = vec![NOP; 57];
    let code = [
        addiu(SP, SP, -16),                  // 0
        mem(SW, RA, 12, SP),                 // 1
        addiu(S0, ZERO, 0),                  // 2
        addiu(S1, ZERO, 5),                  // 3
        j(3, F),                             // 4  loop: jal f
        addu(A0, S1, ZERO),                  // 5
        addu(S0, S0, V0),                    // 6
        addiu(S1, S1, -1),                   // 7
        b(5, S1, ZERO, 8, 4),                // 8  bne s1,zero,loop
        mem(SW, S0, 0, SP),                  // 9  memory slot, hoisted
        addiu(A1, ZERO, 1),                  // 10
        i(0xB, A1, AT, 2),                   // 11 sltiu at,a1,2
        b(4, AT, ZERO, 12, 26),              // 12 beq at,zero,default
        r(0, 0, A1, T0, 2),                  // 13 sll t0,a1,2
        lui(T1, at(TABLE) >> 16),            // 14
        addu(T1, T1, T0),                    // 15
        mem(LW, T1, (at(TABLE) & 0xFFFF) as i32, T1), // 16
        NOP,                                 // 17
        r(8, T1, 0, 0, 0),                   // 18 jr t1
        NOP,                                 // 19
        addiu(V1, ZERO, 10),                 // 20 case 0
        j(2, 27),                            // 21
        NOP,                                 // 22
        addiu(V1, ZERO, 20),                 // 23 case 1
        j(2, 27),                            // 24
        mem(LW, T2, 0, SP),                  // 25 memory slot of a jump
        addiu(V1, ZERO, 30),                 // 26 default
        lui(T3, at(G) >> 16),                // 27 join
        ori(T3, T3, at(G) & 0xFFFF),         // 28
        r(9, T3, 0, RA, 0),                  // 29 jalr t3
        NOP,                                 // 30
        b(4, V1, ZERO, 31, 35),              // 31 beq v1,zero,35
        mem(LW, V1, 0, SP),                  // 32 slot writes the branch operand
        NOP,                                 // 33
        addiu(V1, V1, 1),                    // 34
        i(1, S1, 0x11, H as i32 - 36),       // 35 bgezal s1,h
        NOP,                                 // 36
        mem(LW, RA, 12, SP),                 // 37
        addiu(SP, SP, 16),                   // 38
        j(2, 41),                            // 39 j end
        NOP,                                 // 40
        j(2, 41),                            // 41 end
        NOP,                                 // 42
    ];
    w[..code.len()].copy_from_slice(&code);
    w[F as usize] = r(0, 0, A0, V0, 1); //  f: sll v0,a0,1
    w[F as usize + 1] = r(8, RA, 0, 0, 0); //   jr ra
    w[F as usize + 2] = mem(SW, V0, 4, SP); //  sw v0,4(sp)
    w[G as usize] = addiu(S2, ZERO, 77); //    g
    w[G as usize + 1] = r(8, RA, 0, 0, 0);
    w[G as usize + 2] = NOP;
    w[H as usize] = addiu(S3, S3, 1); //       h
    w[H as usize + 1] = r(8, RA, 0, 0, 0);
    w[H as usize + 2] = NOP;
    w[TABLE as usize] = at(20);
    w[TABLE as usize + 1] = at(23);
    w
}

#[test]
fn calls_jumps_and_delay_slots() {
    let run = check(&control_program(), 41, |c| c.gpr[SP as usize] = 0x801F_FF00, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
    assert_eq!(run.log.polls, 0);
    assert!(run.log.calls.is_empty());
}

#[test]
fn pending_interrupt_is_served_at_a_back_edge() {
    let run = check(&control_program(), 41, |c| c.gpr[SP as usize] = 0x801F_FF00, true);
    assert_eq!(run.log.polls, 1);
}

#[test]
fn delay_slot_that_is_also_a_target() {
    let w = [
        addiu(T0, ZERO, 2),       // 0
        addiu(T1, ZERO, 0),       // 1
        addiu(T0, T0, -1),        // 2  loop
        b(5, T0, ZERO, 3, 7),     // 3  bne t0,zero,7 (into a delay slot)
        NOP,                      // 4
        addiu(T2, ZERO, 1),       // 5
        j(2, 10),                 // 6  j end
        addiu(T1, T1, 10),        // 7  slot of 6, target of 3
        j(2, 2),                  // 8  j loop
        NOP,                      // 9
        j(2, 10),                 // 10 end
        NOP,                      // 11
    ];
    let run = check(&w, 10, |_| {}, false);
    assert!(run.t.runtime.is_empty());
    assert!(run.t.host(at(7)).is_some());
}

/// Branches whose host distance exceeds 16 bits, forward and backward, go through a `j`.
#[test]
fn far_branches_are_relaxed() {
    const N: u32 = 3000;
    let mut w = vec![
        lui(A0, 0x8010),       // 0
        addiu(T0, ZERO, 2),    // 1
        addiu(T0, T0, -1),     // 2  loop
        b(4, T0, ZERO, 3, N + 6), // 3  beq t0,zero,out (forward, far)
        NOP,                   // 4
    ];
    w.extend((0..N).map(|k| mem(SW, T0, 4 * k as i32 % 0x7FFC, A0))); // 5 ..
    w.extend([
        b(4, ZERO, ZERO, N + 5, 2), // N+5 b loop (backward, far)
        NOP,                        // N+6 out
        j(2, N + 7),                // N+7 end
        NOP,
    ]);
    // N+6 is the slot of the backward branch and the forward branch's target.
    let run = check(&w, N + 7, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
    let gap = run.t.host(at(N + 5)).unwrap() - run.t.host(at(5)).unwrap();
    assert!(gap / 4 > 0x8000, "the test must need relaxation ({} words)", gap / 4);
}

#[test]
fn runtime_runs_cop0_syscall_guest_k_and_takes_over_at_a_hazard() {
    let w = [
        addiu(K0G, ZERO, 0x55),          // 0  guest k0
        addu(K1G, K0G, K0G),             // 1  guest k1
        cop0(4, K1G, 12),                // 2  mtc0 k1,$12
        addiu(T0, ZERO, 3),              // 3
        cop0(4, T0, 13),                 // 4  mtc0 t0,$13
        cop0(0, T1, 12),                 // 5  mfc0 t1,$12
        NOP,                             // 6
        addiu(A0, ZERO, 9),              // 7
        0x0000_000C,                     // 8  syscall
        addu(S0, V0, T1),                // 9
        lui(A1, 0x8002),                 // 10
        mem(SW, S0, 0, A1),              // 11
        mem(LW, T2, 0, A1),              // 12 hazard: 13 reads t2 in the delay slot
        addu(T3, T2, ZERO),              // 13
        addu(T4, T2, ZERO),              // 14
        j(2, 15),                        // 15 end
        NOP,                             // 16
    ];
    let run = check(&w, 15, |_| {}, false);
    let rt = &run.t.runtime;
    assert_eq!(rt[&at(0)], Reason::GuestK);
    assert_eq!(rt[&at(1)], Reason::GuestK);
    assert_eq!(rt[&at(2)], Reason::GuestK);
    assert_eq!(rt[&at(4)], Reason::Cop0);
    assert_eq!(rt[&at(5)], Reason::Cop0);
    assert_eq!(rt[&at(8)], Reason::Syscall);
    assert_eq!(rt[&at(12)], Reason::Hazard);
    assert_eq!(run.log.resumed, Some(at(12)));
}

/// Translates every executable in `PSX2PSP_EXES`:
/// `PSX2PSP_EXES=<dir> cargo test -p psx2psp-recomp --test emit -- --ignored --nocapture`.
#[test]
#[ignore = "needs PSX2PSP_EXES"]
fn real_executables_translate() {
    let dir = std::env::var("PSX2PSP_EXES").expect("PSX2PSP_EXES");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    paths.sort();
    println!("exe | guest code bytes | host bytes | ratio | runtime calls | interpreter hand-overs");
    let mut total = BTreeMap::<Reason, usize>::new();
    let (mut guest, mut host) = (0usize, 0usize);
    for path in &paths {
        let x = PsxExe::parse(&std::fs::read(path).unwrap()).unwrap();
        let d = Discovery::run(&x);
        let hz = Hazards::check(&x, &d);
        let t = translate(&x, &d, &hz, CODE).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let g = 4 * d.code.len();
        let (mut exec, mut resume) = (0, 0);
        for r in t.runtime.values() {
            *total.entry(*r).or_default() += 1;
            if r.resumes() { resume += 1 } else { exec += 1 }
        }
        println!(
            "{} | {g} | {} | {:.2} | {exec} | {resume}",
            path.file_name().unwrap().to_string_lossy(),
            4 * t.words.len(),
            (4 * t.words.len()) as f64 / g as f64,
        );
        guest += g;
        host += 4 * t.words.len();
    }
    println!("total | {guest} | {host} | {:.2}", host as f64 / guest as f64);
    println!("{total:?}");
}
