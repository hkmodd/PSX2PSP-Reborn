//! Differential tests of the emitter: each program runs on the R3000A interpreter (the oracle)
//! and, translated, on the Allegrex interpreter; registers and memory must match. The test plays
//! the runtime: it serves host calls, dispatch misses and the interpreter hand-over.

use std::cell::Cell;
use std::collections::BTreeMap;

use psx2psp_recomp::emit::RESUME;
use psx2psp_recomp::hooks::HOOK;
use psx2psp_recomp::layout::{self, K0, MISS, RT_CALL, ctx};
use psx2psp_recomp::{
    Bus, Cpu, Discovery, Hazards, Isa, PsxExe, Reason, Size, Stop, Translation, Trap, translate,
};

const BASE: u32 = 0x8001_0000;
const CODE: u32 = 0x08A0_0000;
const HI: u32 = layout::SCRATCH;
const IO: u32 = 0x1F80_1000;
/// Cycle budget the runtime side refills: no test loop exhausts it.
const FULL_BUDGET: u32 = 0x4000_0000;

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
}

/// Byte offset of the GTE registers ([`layout::GTE`]) in [`Machine::hi`]: both sides keep them
/// there, the oracle with the runtime's write rules and a stand-in command that mixes them.
const GTE: usize = (layout::GTE - HI) as usize;

impl Machine {
    fn new(x: &PsxExe) -> Self {
        let mut m = Machine {
            ram: vec![0; 0x20_0000],
            hi: vec![0; (layout::END - HI) as usize],
            io: vec![0; 0x2000],
        };
        let o = (x.text_addr & 0x1F_FFFF) as usize;
        m.ram[o..o + x.text.len()].copy_from_slice(&x.text);
        m
    }
    /// Points every dispatch-table entry at the translation, or at [`MISS`].
    fn table(&mut self, t: &Translation) {
        for g in (0..0x20_0000).step_by(4) {
            self.set_word(layout::table_entry(g), MISS);
        }
        for (e, v) in t.table() {
            self.set_word(e, v);
        }
        self.set_ctx(ctx::BUDGET, FULL_BUDGET);
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
    /// Where the PS1 side of two machines differs, RAM in `skip` (word-aligned offsets) aside;
    /// a RAM word `x` where the oracle has `y` passes when `same(x, y)`.
    fn diff(&self, o: &Machine, skip: std::ops::Range<usize>, same: &dyn Fn(u32, u32) -> bool) -> Option<String> {
        let at = |a: &[u8], b: &[u8], base: u32| {
            a.iter().zip(b).position(|(x, y)| x != y).map(|k| base + k as u32)
        };
        let ram = (0..self.ram.len()).step_by(4).filter(|k| !skip.contains(k)).find_map(|k| {
            let (x, y) = (rd(&self.ram, k, Size::Word), rd(&o.ram, k, Size::Word));
            (x != y && !same(x, y)).then_some((0x8000_0000 + k as u32, x, y))
        });
        if let Some((a, x, y)) = ram {
            return Some(format!("RAM at {a:#010x}: word {x:#010x}, oracle {y:#010x}"));
        }
        if let Some(a) = at(&self.hi[..0x400], &o.hi[..0x400], 0x1F80_0000) {
            return Some(format!("scratchpad at {a:#010x}"));
        }
        if let Some(a) = at(&self.io, &o.io, IO) {
            return Some(format!("I/O at {a:#010x}"));
        }
        let gte = |m: &Machine| m.hi[GTE..GTE + 0x100].to_vec();
        (gte(self) != gte(o)).then(|| "GTE registers".into())
    }
    /// GTE register `r` (data 0-31, control 32-63) as stored.
    fn gte(&self, r: usize) -> u32 {
        rd(&self.hi, GTE + 4 * r, Size::Word)
    }
    fn set_gte(&mut self, r: usize, v: u32) {
        wr(&mut self.hi, GTE + 4 * r, Size::Word, v)
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
    /// The runtime's rules (psp/psxrt/core/gte.c).
    fn cop2_read(&mut self, reg: usize) -> Result<u32, Trap> {
        let m = &*self.0;
        Ok(match reg {
            15 => m.gte(14),
            28 | 29 => (0..3).fold(0, |o, k| o | ((m.gte(9 + k) as i32 >> 7).clamp(0, 0x1F) as u32) << (5 * k)),
            r => m.gte(r),
        })
    }
    fn cop2_write(&mut self, reg: usize, v: u32) -> Result<(), Trap> {
        let m = &mut *self.0;
        match reg {
            1 | 3 | 5 | 8..=11 | 36 | 44 | 52 | 58 | 59 | 61 | 62 => m.set_gte(reg, v as i16 as u32),
            7 | 16..=19 => m.set_gte(reg, v & 0xFFFF),
            15 => {
                m.set_gte(12, m.gte(13));
                m.set_gte(13, m.gte(14));
                m.set_gte(14, v);
            }
            28 => (0..3).for_each(|k| m.set_gte(9 + k, (v >> (5 * k) & 0x1F) << 7)),
            29 | 31 => {}
            30 => {
                m.set_gte(30, v);
                m.set_gte(31, if (v as i32) < 0 { !v } else { v }.leading_zeros());
            }
            63 => {
                let f = v & 0x7FFF_F000;
                m.set_gte(63, if f & 0x7F87_E000 != 0 { f | 0x8000_0000 } else { f });
            }
            r => m.set_gte(r, v),
        }
        Ok(())
    }
    fn cop2_command(&mut self, command: u32) -> Result<(), Trap> {
        let m = &mut *self.0;
        // A command with an entry of its own is the same whatever its word's other bits.
        let command = match layout::GTE_OWN.iter().find(|&&(_, m, v)| command & m == v) {
            Some(&(_, m, _)) => command & m,
            None => command,
        };
        for k in 0..32 {
            let v = m.gte(k).rotate_left(5) ^ m.gte(32 + (k + command as usize) % 32) ^ command;
            m.set_gte(k, v);
        }
        Ok(())
    }
}

/// The machine as the translated code sees it.
struct Psp<'a>(&'a mut Machine, &'a [u32]);

impl Bus for Psp<'_> {
    fn load(&mut self, addr: u32, size: Size) -> Result<u32, Trap> {
        let m = &mut *self.0;
        match addr {
            layout::RAM..0x09A0_0000 => Ok(rd(&m.ram, (addr - layout::RAM) as usize, size)),
            HI..layout::END => Ok(rd(&m.hi, (addr - HI) as usize, size)),
            a if a >= CODE && ((a - CODE) / 4) < self.1.len() as u32 && size == Size::Word => {
                Ok(self.1[((a - CODE) / 4) as usize])
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

/// What the test's BIOS returns in `v0` for a call to the A0/B0/C0 vector `target`, or `None`
/// for any other address.
fn bios(target: u32, c: &Cpu) -> Option<u32> {
    let v = target & 0x1FFF_FFFF;
    matches!(v, 0xA0 | 0xB0 | 0xC0)
        .then(|| (v << 8) ^ c.gpr[T1 as usize].wrapping_mul(0x9E37) ^ c.gpr[A0 as usize])
}

/// Where a run stops: a guest address for the R3000A, a host address for the translation (the
/// return address of a translated function under test is a host address).
#[derive(Clone, Copy)]
struct End {
    guest: u32,
    host: u32,
    /// End of the translated code: the interpreter hands back to it at a host address.
    code: u32,
}

/// Why the oracle could not run a program to its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Trap(Trap),
    /// A jump into the BIOS area other than its call vectors (a null function pointer).
    Kernel(u32),
    Budget,
}

fn arrived(c: &Cpu, a: u32) -> bool {
    c.pc == a && c.next_pc == a.wrapping_add(4)
}

/// Runs the guest from `c.pc` on the R3000A interpreter, at most `budget` steps, until `end` or a
/// host code address (where the runtime's interpreter hands back to translated code). Returns
/// the steps taken.
fn run_ps1(m: &mut Machine, c: &mut Cpu, end: End, budget: usize) -> Result<usize, Fault> {
    for n in 0..budget {
        if arrived(c, end.guest) || arrived(c, end.host) || (CODE..end.code).contains(&c.pc) {
            if let Some((r, v)) = c.pending.take() {
                c.gpr[r] = v;
            }
            return Ok(n);
        }
        if c.pc & 0x1FFF_FFFF < 0x1_0000 && bios(c.pc, c).is_none() {
            return Err(Fault::Kernel(c.pc));
        }
        if let Some(v) = bios(c.pc, c) {
            if let Some((r, v)) = c.pending.take() {
                c.gpr[r] = v;
            }
            c.gpr[V0 as usize] = v;
            c.jump(c.gpr[RA as usize]);
            continue;
        }
        match c.step(&mut Ps1(m)) {
            Ok(()) => {}
            Err(Stop { pc, trap: Trap::Syscall { .. } }) => {
                syscall(c);
                c.jump(pc + 4);
            }
            Err(s) => return Err(Fault::Trap(s.trap)),
        }
    }
    Err(Fault::Budget)
}

/// What happened on the translated side besides plain execution.
#[derive(Debug, Default)]
struct Log {
    calls: BTreeMap<u32, usize>,
    polls: usize,
    resumed: Option<u32>,
    /// Dispatch misses the interpreter ran (guest targets).
    missed: Vec<u32>,
    /// GTE commands through [`layout::GTE_CMD`].
    gte: usize,
}

/// Runs the translation from guest `init.pc` to `end` on the Allegrex interpreter, at most
/// `budget` steps; `m` holds the dispatch table ([`Machine::table`]). Returns the guest state at
/// the end.
fn run_psp(
    m: &mut Machine,
    t: &Translation,
    init: &Cpu,
    end: End,
    budget: usize,
) -> Result<(Cpu, Log), String> {
    let code = &t.words[..];
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
    for _ in 0..budget {
        if arrived(&h, end.host) {
            to_guest(&h, m, &mut g);
            return Ok((g, log));
        }
        if h.pc == MISS {
            let target = m.ctx(ctx::TARGET);
            if let Some(v) = bios(target, &h) {
                h.gpr[V0 as usize] = v;
                h.jump(h.gpr[RA as usize]);
                continue;
            }
            // The runtime interprets from the target until the code returns to a host address.
            log.missed.push(target);
            to_guest(&h, m, &mut g);
            g.jump(target);
            run_ps1(m, &mut g, end, budget)
                .map_err(|f| format!("interpreter stopped after a miss to {target:#010x}: {f:x?}"))?;
            if arrived(&g, end.guest) || arrived(&g, end.host) {
                return Ok((g, log));
            }
            back(&g, &mut h, m);
            h.jump(g.pc);
            continue;
        }
        let own = layout::GTE_OWN.iter().find(|e| e.0 == h.pc).map(|e| e.2);
        let word = layout::GTE_WORD.iter().any(|e| e.0 == h.pc);
        if h.pc == layout::GTE_CMD || word || own.is_some() {
            // The entry runs the command (in k1 for GTE_CMD and GTE_WORD), restores the guest ra and returns
            // through k1.
            let ret = h.gpr[RA as usize];
            log.gte += 1;
            Ps1(m).cop2_command(own.unwrap_or(h.gpr[K1G as usize] & 0x01FF_FFFF)).unwrap();
            if own.is_none() {
                m.set_ctx(ctx::GTE_RET, ret);
            }
            h.gpr[RA as usize] = m.ctx(ctx::GPR + 4 * RA);
            h.gpr[K1G as usize] = ret;
            h.jump(ret);
            continue;
        }
        // IO_READ is a host call that leaves every guest register in CTX for the site's reload;
        // IO_WRITE one that finds the stored register in CTX.
        let io = h.pc == layout::IO_READ;
        let io_w = h.pc == layout::IO_WRITE;
        if h.pc == RT_CALL || io || io_w {
            let ra = h.gpr[RA as usize];
            let word_at = |a: u32| code.get(((a - CODE) / 4) as usize).copied().unwrap();
            let (pc, word) = (word_at(ra), word_at(ra + 4));
            if io {
                let a = h.gpr[(word >> 21 & 31) as usize].wrapping_add(word as i16 as u32);
                assert_eq!(h.gpr[K1G as usize], a, "IO_READ address at {pc:08x}");
            }
            if io_w {
                let a = h.gpr[(word >> 21 & 31) as usize].wrapping_add(word as i16 as u32);
                assert_eq!(h.gpr[K1G as usize], a, "IO_WRITE address at {pc:08x}");
                let rt = word >> 16 & 31;
                if rt != 0 && rt != RA {
                    assert_eq!(m.ctx(ctx::GPR + 4 * rt), h.gpr[rt as usize], "IO_WRITE value at {pc:08x}");
                }
            }
            h.gpr[RA as usize] = m.ctx(ctx::GPR + 4 * RA);
            to_guest(&h, m, &mut g);
            // A hook's recipe does what its code does, so the interpreter runs that code instead.
            if pc & (RESUME | HOOK) != 0 {
                log.resumed = Some(pc & !(RESUME | HOOK));
                g.jump(pc & !(RESUME | HOOK));
                run_ps1(m, &mut g, end, budget).map_err(|f| format!("interpreter stopped: {f:x?}"))?;
                return Ok((g, log));
            }
            if (m.ctx(ctx::BUDGET) as i32) < 0 {
                m.set_ctx(ctx::BUDGET, FULL_BUDGET);
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
            back(&g, &mut h, m);
            if io {
                (1..32).for_each(|r| m.set_ctx(ctx::GPR + 4 * r, g.gpr[r as usize]));
            }
            h.jump(ra + 8);
            continue;
        }
        h.step(&mut Psp(m, code)).map_err(|s| format!("translated code stopped: {s:x?}"))?;
    }
    Err("translated code did not reach the end".into())
}

/// The guest state `g` back into the translated code's registers and context.
fn back(g: &Cpu, h: &mut Cpu, m: &mut Machine) {
    h.gpr = g.gpr;
    h.gpr[K0G as usize] = K0;
    for r in [K0G, K1G] {
        m.set_ctx(ctx::GPR + 4 * r, g.gpr[r as usize]);
    }
    h.hi = g.hi;
    h.lo = g.lo;
}

/// Registers and memory where the translated run `g`/`m2` differs from the oracle's `c`/`m1`;
/// `ra` and the RAM in `skip` (offsets) are not compared, registers and RAM words pass `same`
/// (`Machine::diff`).
fn differs(
    c: &Cpu,
    m1: &Machine,
    g: &Cpu,
    m2: &Machine,
    ra: bool,
    skip: std::ops::Range<usize>,
    same: &dyn Fn(u32, u32) -> bool,
) -> Option<String> {
    for r in (1..32).filter(|&r| ra || r != RA as usize) {
        if g.gpr[r] != c.gpr[r] && !same(g.gpr[r], c.gpr[r]) {
            return Some(format!("r{r}: translated {:#x}, oracle {:#x}", g.gpr[r], c.gpr[r]));
        }
    }
    if (g.hi, g.lo) != (c.hi, c.lo) {
        return Some(format!("hi/lo: translated {:#x}/{:#x}, oracle {:#x}/{:#x}", g.hi, g.lo, c.hi, c.lo));
    }
    m2.diff(m1, skip, same)
}

struct Run {
    t: Translation,
    log: Log,
}

/// Translates `words`, runs both sides from `BASE` (registers from `init`) to word `end` and
/// compares them.
fn check(words: &[u32], end: u32, init: impl Fn(&mut Cpu), irq: bool) -> Run {
    check_budget(words, end, init, if irq { u32::MAX } else { FULL_BUDGET })
}

/// [`check`] starting with `budget` in [`ctx::BUDGET`].
fn check_budget(words: &[u32], end: u32, init: impl Fn(&mut Cpu), budget: u32) -> Run {
    let x = exe(words);
    let d = Discovery::run(&x);
    let hz = Hazards::check(&x, &d);
    let t = translate(&x, &d, &hz, CODE).unwrap();
    let mut c = Cpu::new(Isa::R3000A, BASE);
    init(&mut c);
    let start = c.clone();
    let mut m1 = Machine::new(&x);
    let mut m2 = m1.clone();
    m2.table(&t);
    let end = End { guest: at(end), host: t.host(at(end)).unwrap(), code: CODE + 4 * t.words.len() as u32 };
    run_ps1(&mut m1, &mut c, end, 100_000).unwrap();
    m2.set_ctx(ctx::BUDGET, budget);
    let (g, log) = run_psp(&mut m2, &t, &start, end, 200_000).unwrap();
    if let Some(d) = differs(&c, &m1, &g, &m2, true, 0..0, &|_, _| false) {
        panic!("{d}");
    }
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

#[test]
fn a_group_crossing_the_2_mb_mirror_takes_the_checked_copy() {
    let w = [
        lui(A0, 0x8020),         // 0
        addiu(A0, A0, -8),       // 1  a0 = 0x801FFFF8
        addiu(T0, ZERO, 5),      // 2
        mem(SW, T0, 0, A0),      // 3  group: 0x801FFFF8 ..
        mem(SW, T0, 4, A0),      // 4
        mem(SW, T0, 8, A0),      // 5  .. 0x80200000, the mirror of 0x80000000
        mem(LW, T1, 8, A0),      // 6
        lui(A1, 0x8000),         // 7
        mem(LW, T2, 0, A1),      // 8  5
        NOP,                     // 9
        j(2, 10),                // 10 end
        NOP,                     // 11
    ];
    let run = check(&w, 10, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
}

#[test]
fn a_jump_into_a_group_lands_in_its_checked_copy() {
    let w = [
        lui(A0, 0x8002),             // 0
        lui(T9, 0x8001),             // 1
        ori(T9, T9, 0x18),           // 2  at(6)
        mem(SW, T9, 0x10, A0),       // 3  group 3..7, base a0
        addiu(T0, ZERO, 7),          // 4
        mem(SW, T0, 0, A0),          // 5
        addiu(T0, T0, 1),            // 6  entered again through the table
        mem(SW, T0, 4, A0),          // 7
        b(5, T1, ZERO, 8, 14),       // 8  bne t1,zero,end
        addiu(T1, ZERO, 1),          // 9
        mem(LW, T9, 0x10, A0),       // 10
        NOP,                         // 11
        r(0x08, T9, 0, 0, 0),        // 12 jr t9 (the discovery cannot know where)
        NOP,                         // 13
        j(2, 14),                    // 14 end
        NOP,                         // 15
    ];
    let run = check(&w, 14, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
    // Word 6 is translated twice: inside the group, and out of line where the table points.
    assert!(run.t.host(at(6)).unwrap() > run.t.host(at(8)).unwrap());
}

/// `mfc2` (`rs` = 0), `cfc2` (2), `mtc2` (4), `ctc2` (6).
const fn cop2(rs: u32, rt: u32, rd: u32) -> u32 {
    (0x12 << 26) | (rs << 21) | (rt << 16) | (rd << 11)
}
const fn gte_cmd(cmd: u32) -> u32 {
    (0x12 << 26) | (1 << 25) | cmd
}
const LWC2: u32 = 0x32;
const SWC2: u32 = 0x3A;

#[test]
fn gte_moves_loads_stores_and_commands_run_inline() {
    let w = [
        lui(A0, 0x8002),             // 0
        addiu(T0, ZERO, -2),         // 1
        lui(T1, 0x1234),             // 2
        ori(T1, T1, 0x8765),         // 3  t1 = 0x12348765
        cop2(4, T1, 0),              // 4  mtc2 VXY0
        cop2(4, T1, 1),              // 5  mtc2 VZ0: sign-extended
        cop2(4, T1, 7),              // 6  mtc2 OTZ: zero-extended
        cop2(4, T0, 15),             // 7  mtc2 SXYP: pushes
        cop2(4, T1, 15),             // 8
        cop2(4, T1, 29),             // 9  ORGB: ignored
        cop2(4, T1, 30),             // 10 LZCS: runtime
        cop2(4, T1, 28),             // 11 IRGB: runtime
        cop2(6, T1, 4),              // 12 ctc2 RT33: sign-extended
        cop2(6, T0, 0),              // 13 ctc2
        cop2(6, T1, 31),             // 14 ctc2 FLAG: runtime
        gte_cmd(0x0018_0001),        // 15
        cop2(0, T2, 15),             // 16 mfc2 SXYP reads SXY2
        cop2(0, T3, 1),              // 17
        cop2(0, T4, 29),             // 18 ORGB: runtime
        cop2(2, T5, 4),              // 19 cfc2
        cop2(0, ZERO, 3),            // 20
        mem(SW, T1, 0, A0),          // 21
        mem(LWC2, 1, 0, A0),         // 22 sign-extended
        mem(LWC2, 16, 0, A0),        // 23 zero-extended
        mem(LWC2, 2, 0, A0),         // 24
        mem(LWC2, 30, 0, A0),        // 25 runtime
        mem(SWC2, 15, 4, A0),        // 26 SXY2
        mem(SWC2, 1, 8, A0),         // 27
        mem(SWC2, 28, 12, A0),       // 28 runtime
        lui(A1, 0x1F80),             // 29 scratchpad
        mem(SW, T1, 0x10, A1),       // 30
        mem(LWC2, 3, 0x10, A1),      // 31
        mem(SWC2, 9, 0x20, A1),      // 32
        addu(AT, A0, ZERO),          // 33
        mem(SWC2, 2, 0x30, AT),      // 34 base at: parks v0 instead
        ori(A2, A1, 0x1000),         // 35 I/O
        mem(SWC2, 2, 0, A2),         // 36
        mem(LWC2, 4, 0, A2),         // 37
        j(3, 43),                    // 38 jal f
        gte_cmd(0x0040_0012),        // 39 command in the slot
        addu(RA, ZERO, ZERO),        // 40 (host and guest return addresses differ)
        j(2, 46),                    // 41 end
        NOP,                         // 42
        mem(SWC2, 0, 0x40, A0),      // 43 f
        r(8, RA, 0, 0, 0),           // 44 jr ra
        cop2(0, V1, 14),             // 45 mfc2 in the slot
        j(2, 46),                    // 46
        NOP,                         // 47
    ];
    let run = check(&w, 41, |c| c.gpr[V0 as usize] = 0x0BAD_F00D, false);
    let rt: Vec<u32> = run.t.runtime.keys().copied().collect();
    assert_eq!(rt, [at(10), at(11), at(14), at(18), at(25), at(28)]);
    assert!(run.t.runtime.values().all(|&r| r == Reason::Cop2));
    assert_eq!(run.log.gte, 2);
    let io: Vec<u32> = run.log.calls.keys().copied().filter(|&a| !rt.contains(&a)).collect();
    assert_eq!(io, [at(36), at(37)]);
}

/// NCLIP, AVSZ3 (with bits it ignores), AVSZ4, RTPS and RTPT through their own entries, one in
/// a `jal` slot, next to commands through [`layout::GTE_CMD`].
#[test]
fn gte_commands_with_entries_of_their_own() {
    let w = [
        lui(T1, 0x1234),                       // 0
        ori(T1, T1, 0x8765),                   // 1
        cop2(4, T1, 12),                       // 2  SXY0
        cop2(4, T1, 17),                       // 3  SZ1
        gte_cmd(0x0140_0006),                  // 4  NCLIP
        gte_cmd(0x0158_002D | 1 << 19),        // 5  AVSZ3
        gte_cmd(0x0040_0012),                  // 6  MVMVA: GTE_MVMVA, word in k1
        gte_cmd(0x0028_0030),                  // 7  RTPT
        gte_cmd(0x0018_0401),                  // 8  RTPS with lm: GTE_CMD
        gte_cmd(0x0018_0001),                  // 9  RTPS
        j(3, 17),                              // 10 jal f
        gte_cmd(0x0168_002E),                  // 11 AVSZ4 in the slot
        addu(RA, ZERO, ZERO),                  // 12 (host and guest return addresses differ)
        cop2(0, T2, 24),                       // 13 MAC0
        cop2(0, T3, 7),                        // 14 OTZ
        j(2, 20),                              // 15 end
        NOP,                                   // 16
        cop2(2, T4, 31),                       // 17 f: FLAG
        r(8, RA, 0, 0, 0),                     // 18 jr ra
        gte_cmd(0x0140_0006),                  // 19 NCLIP in the slot
        j(2, 20),                              // 20
        NOP,                                   // 21
    ];
    let run = check(&w, 15, |_| {}, false);
    assert_eq!(run.log.gte, 8);
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
fn back_edges_charge_the_loop_cost() {
    let w = [
        addiu(T0, ZERO, 100),     // 0
        addiu(T0, T0, -1),        // 1  loop: 3 words, no memory access
        b(5, T0, ZERO, 2, 1),     // 2  bne t0,zero,loop
        NOP,                      // 3
        NOP,                      // 4  end
    ];
    // 100 passes cost 300: a budget of 300 lasts, 299 runs out on the last pass.
    assert_eq!(check_budget(&w, 4, |_| {}, 300).log.polls, 0);
    assert_eq!(check_budget(&w, 4, |_| {}, 299).log.polls, 1);
}

#[test]
fn back_edges_do_not_charge_what_a_pass_can_skip() {
    let w = [
        addiu(T0, ZERO, 100),     // 0
        addiu(T0, T0, -1),        // 1  loop
        b(5, T1, ZERO, 2, 6),     // 2  bne t1,zero,6: a path that skips 4 and 5
        NOP,                      // 3
        addiu(T2, T2, 1),         // 4
        addiu(T2, T2, 1),         // 5
        b(5, T0, ZERO, 6, 1),     // 6  bne t0,zero,loop
        NOP,                      // 7
        NOP,                      // 8  end
    ];
    // 100 passes of words 1, 2, 3, 6 and 7 cost 500.
    assert_eq!(check_budget(&w, 8, |c| c.gpr[T1 as usize] = 1, 500).log.polls, 0);
    assert_eq!(check_budget(&w, 8, |c| c.gpr[T1 as usize] = 1, 499).log.polls, 1);
}

#[test]
fn back_edges_charge_ram_reads_but_not_writes() {
    let w = [
        addiu(T0, ZERO, 100),     // 0
        addiu(T0, T0, -1),        // 1  loop
        mem(LW, T1, 0, ZERO),     // 2  a RAM read: 6 more cycles
        NOP,                      // 3  load delay
        mem(SW, T1, 4, ZERO),     // 4  a write goes to the write queue: none
        b(5, T0, ZERO, 5, 1),     // 5  bne t0,zero,loop
        NOP,                      // 6
        NOP,                      // 7  end
    ];
    // 100 passes of 6 words and one read cost 1200.
    assert_eq!(check_budget(&w, 7, |_| {}, 1200).log.polls, 0);
    assert_eq!(check_budget(&w, 7, |_| {}, 1199).log.polls, 1);
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

/// [`check`] with the words `out` taken out of the discovered code, as a floating unit's
/// neighbours are: RAM still holds them and the interpreter runs them after a dispatch miss.
fn check_cut(words: &[u32], out: std::ops::Range<u32>, end: u32, init: impl Fn(&mut Cpu)) -> Run {
    let x = exe(words);
    let mut d = Discovery::run(&x);
    let gone = |a: &u32| (at(out.start)..at(out.end)).contains(a);
    d.code.retain(|a| !gone(a));
    d.delay_slots.retain(|a, _| !gone(a));
    d.jumps.retain(|a, _| !gone(a));
    let hz = Hazards::check(&x, &d);
    let t = translate(&x, &d, &hz, CODE).unwrap();
    let mut c = Cpu::new(Isa::R3000A, BASE);
    init(&mut c);
    let start = c.clone();
    let mut m1 = Machine::new(&x);
    let mut m2 = m1.clone();
    m2.table(&t);
    let end = End { guest: at(end), host: t.host(at(end)).unwrap(), code: CODE + 4 * t.words.len() as u32 };
    run_ps1(&mut m1, &mut c, end, 100_000).unwrap();
    let (g, log) = run_psp(&mut m2, &t, &start, end, 200_000).unwrap();
    if let Some(d) = differs(&c, &m1, &g, &m2, true, 0..0, &|_, _| false) {
        panic!("{d}");
    }
    Run { t, log }
}

/// A branch to code that is not the translation's leaves through the table, its slot run on the
/// path it takes and on the one it does not; neither goes to the runtime.
#[test]
fn a_branch_out_of_the_code_goes_through_the_table() {
    let prog = |branch: u32| {
        [
            branch,                // 0  to 8, out of the code
            addiu(V0, ZERO, 5),    // 1  slot
            addiu(V1, V0, 1),      // 2  not taken
            j(2, 6),               // 3
            NOP,                   // 4
            NOP,                   // 5
            j(2, 6),               // 6  end
            NOP,                   // 7
            addiu(A1, V0, 3),      // 8  out of the code
            j(2, 6),               // 9
            NOP,                   // 10
        ]
    };
    let beq = b(4, A0, ZERO, 0, 8);
    let bgez = b(1, A0, 1, 0, 8);
    let always = b(4, ZERO, ZERO, 0, 8);
    for (branch, a0, taken) in [(beq, 0, true), (beq, 1, false), (bgez, 1, true), (bgez, -1i32 as u32, false), (always, 1, true)] {
        let run = check_cut(&prog(branch), 8..11, 6, |c| c.gpr[A0 as usize] = a0);
        assert!(run.t.runtime.is_empty(), "{branch:08x}: {:?}", run.t.runtime);
        assert_eq!(run.log.missed, if taken { vec![at(8)] } else { vec![] }, "{branch:08x} a0 {a0}");
    }
    // Backward, out of the code: the back edge still polls.
    let w = [
        j(2, 5),                   // 0
        NOP,                       // 1
        addiu(A1, V0, 3),          // 2  out of the code
        j(2, 8),                   // 3
        NOP,                       // 4
        b(5, A0, ZERO, 5, 2),      // 5  bne a0,zero,2
        addiu(V0, ZERO, 5),        // 6  slot
        addiu(V1, V0, 1),          // 7
        j(2, 8),                   // 8  end
        NOP,                       // 9
    ];
    for a0 in [0, 1] {
        let run = check_cut(&w, 2..5, 8, |c| c.gpr[A0 as usize] = a0);
        assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
        assert_eq!(run.log.missed, if a0 != 0 { vec![at(2)] } else { vec![] });
    }
}

/// A code address built in `ra` (a hand-made return point, as Crash 2's) is the host address of
/// its translation: the translated `jr ra` that takes it stays native.
#[test]
fn a_code_address_built_in_ra_is_a_host_return_point() {
    let w = [
        lui(RA, 0x8001),          // 0
        addiu(RA, RA, 0x1C),      // 1  ra = word 7
        addiu(T0, ZERO, 2),       // 2
        b(5, ZERO, ZERO, 3, 7),   // 3  bne zero,zero,7 (never taken: 7 is code)
        NOP,                      // 4
        r(8, RA, 0, 0, 0),        // 5  jr ra
        NOP,                      // 6
        addiu(T0, T0, -1),        // 7  the return point
        b(5, T0, ZERO, 8, 12),    // 8  bne t0,zero,12
        NOP,                      // 9
        j(2, 15),                 // 10 j end
        addiu(RA, ZERO, 0),       // 11 slot: ra no longer compared as an address
        r(8, RA, 0, 0, 0),        // 12 jr ra (again to 7)
        addiu(T1, T1, 1),         // 13 slot
        NOP,                      // 14
        j(2, 15),                 // 15 end
        NOP,                      // 16
    ];
    let run = check(&w, 15, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
}

/// A return point built in `ra` in the middle of a straight line (no branch goes there, as Sheep
/// Dog's WHEEL.EXE 8001C108 -> 80036330) starts a block of its own.
#[test]
fn a_return_point_built_in_ra_mid_block_gets_a_label() {
    let w = [
        lui(RA, 0x8001),          // 0
        addiu(RA, RA, 0x18),      // 1  ra = word 6
        addiu(T0, ZERO, 2),       // 2
        addiu(T1, T1, 1),         // 3
        addiu(T2, T2, 1),         // 4
        NOP,                      // 5
        addiu(T0, T0, -1),        // 6  the return point, reached by falling through
        b(5, T0, ZERO, 7, 11),    // 7  bne t0,zero,11
        NOP,                      // 8
        j(2, 13),                 // 9  j end
        addiu(RA, ZERO, 0),       // 10 slot: ra no longer compared as an address
        r(8, RA, 0, 0, 0),        // 11 jr ra (to 6)
        NOP,                      // 12
        j(2, 13),                 // 13 end
        NOP,                      // 14
    ];
    let run = check(&w, 13, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
    assert!(run.t.host(at(6)).is_some());
}

/// A return address kept in memory and taken back through another register (Spyro 2's 8001E3E8:
/// `sw ra,8(at)` ... `lw t7,8(at)` ... `jr t7`) is the host address the translated call wrote: the
/// `jr t7` goes there as it is, not through the table slot that shares its low 21 bits.
#[test]
fn a_host_return_address_through_another_register_goes_as_it_is() {
    let w = [
        lui(A0, 0x8002),          // 0
        j(3, 6),                  // 1  jal 6
        NOP,                      // 2
        addiu(T7, ZERO, 0),       // 3  the return point: t7 no longer an address
        j(2, 14),                 // 4  j end
        mem(SW, ZERO, 0, A0),     // 5  slot: the saved ra cleared
        mem(SW, RA, 0, A0),       // 6  the callee keeps ra in memory
        addiu(RA, ZERO, 0),       // 7  and uses ra as data
        mem(LW, T7, 0, A0),       // 8
        NOP,                      // 9
        r(8, T7, 0, 0, 0),        // 10 jr t7
        addiu(T1, T1, 1),         // 11 slot
        NOP,                      // 12
        NOP,                      // 13
        j(2, 14),                 // 14 end
        NOP,                      // 15
    ];
    let run = check(&w, 14, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
    assert!(run.log.missed.is_empty(), "{:x?}", run.log.missed);
}

/// A guest return address an interpreted prologue saved (Silent Hill's 80073E3C, called from a
/// switch case with ra 80071C10) and a translated `lw ra; jr ra` takes back goes through the
/// dispatch table: a native `jr ra` jumped to PSP 0x80071C10. A leaf's `jr ra` stays native.
#[test]
fn a_guest_return_address_loaded_into_ra_goes_through_the_table() {
    let w = [
        lui(A0, 0x8002),          // 0
        lui(T0, 0x8001),          // 1
        addiu(T0, T0, 0x2C),      // 2  t0 = word 11
        mem(SW, T0, 0, A0),       // 3  the saved guest ra
        b(5, ZERO, ZERO, 4, 11),  // 4  bne zero,zero,11 (never taken: 11 is code)
        NOP,                      // 5
        mem(LW, RA, 0, A0),       // 6
        NOP,                      // 7
        r(8, RA, 0, 0, 0),        // 8  jr ra
        addiu(T1, T1, 1),         // 9  slot
        NOP,                      // 10
        addiu(T2, T2, 1),         // 11 the return point
        j(2, 14),                 // 12 j end
        NOP,                      // 13
        j(2, 14),                 // 14 end
        NOP,                      // 15
    ];
    let run = check(&w, 14, |_| {}, false);
    assert!(run.t.runtime.is_empty(), "{:?}", run.t.runtime);
}

/// A `jalr` linking another register than `ra` writes the guest address 8 past it: Crash 2's
/// GOOL code ends its native pieces with `jalr s5,ra`, and s5 is the bytecode pointer.
#[test]
fn jalr_links_a_guest_address_into_a_register_other_than_ra() {
    let w = [
        lui(T0, 0x8001),          // 0
        addiu(T0, T0, 0x20),      // 1  t0 = word 8
        r(9, T0, 0, S5, 0),       // 2  jalr s5,t0
        NOP,                      // 3
        addiu(T2, T2, 1),         // 4  the return point
        j(2, 12),                 // 5  j end
        NOP,                      // 6
        NOP,                      // 7
        addiu(T1, T1, 1),         // 8  the callee
        r(8, S5, 0, 0, 0),        // 9  jr s5
        NOP,                      // 10
        NOP,                      // 11
        j(2, 12),                 // 12 end
        NOP,                      // 13
    ];
    check(&w, 12, |_| {}, false);
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
    // Bases alternate (a0, zero: both RAM), so no two stores make a group and each keeps its
    // own check: the code must stay long.
    w.extend((0..N).map(|k| mem(SW, T0, 4 * k as i32 % 0x7FFC, if k % 2 == 0 { A0 } else { ZERO }))); // 5 ..
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
    println!("exe | guest code bytes | host bytes | ratio | runtime calls | interpreter hand-overs | cold words | cold blocks | hot words per block");
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
            "{} | {g} | {} | {:.2} | {exec} | {resume} | {:.0}% | {} | {:.0}",
            path.file_name().unwrap().to_string_lossy(),
            4 * t.words.len(),
            (4 * t.words.len()) as f64 / g as f64,
            100.0 * t.cold.0 as f64 / t.words.len() as f64,
            t.cold.1,
            (t.words.len() - t.cold.0) as f64 / t.cold.1.max(1) as f64,
        );
        guest += g;
        host += 4 * t.words.len();
    }
    println!("total | {guest} | {host} | {:.2}", host as f64 / guest as f64);
    println!("{total:?}");
}

/// Name of a fault's kind, for counting.
fn kind(f: Fault) -> String {
    let s = format!("{f:?}");
    s.trim_start_matches("Trap(").split(|c: char| !c.is_alphanumeric()).next().unwrap().to_string()
}

/// Runs every function of a real executable from a synthetic state (pointer arguments into a
/// zeroed buffer, BIOS calls served alike on both sides), on the oracle and translated, and
/// compares registers and memory at the return:
/// `PSX2PSP_EXES=<dir> cargo test --release -p psx2psp-recomp --test emit real_functions -- --ignored --nocapture`
/// (`PSX2PSP_EXE` picks the executable, LEV00.EXE by default). Functions the oracle cannot run
/// to their return (bus errors on synthetic pointers, waits on hardware) are counted, not
/// compared.
#[test]
#[ignore = "needs PSX2PSP_EXES"]
fn real_functions_match_the_oracle() {
    /// Return address given to the function: BIOS RAM, never code.
    const RETURN: u32 = 0x8000_0F00;
    /// Its host counterpart, below the translated code.
    const HOST_RETURN: u32 = 0x0880_0000;
    const STACK_TOP: u32 = 0x801F_FF00;
    /// The stack below `STACK_TOP` is dead after the return, and holds host return addresses
    /// on the translated side: not compared.
    const STACK: u32 = 0x801E_0000;
    let dir = std::env::var("PSX2PSP_EXES").expect("PSX2PSP_EXES");
    let name = std::env::var("PSX2PSP_EXE").unwrap_or_else(|_| "LEV00.EXE".into());
    let x = PsxExe::parse(&std::fs::read(std::path::Path::new(&dir).join(&name)).unwrap()).unwrap();
    let d = Discovery::run(&x);
    let hz = Hazards::check(&x, &d);
    let t = translate(&x, &d, &hz, CODE).unwrap();
    let mut base = Machine::new(&x);
    base.table(&t);
    let top = (x.text_addr + x.text.len() as u32).max(x.bss.0 + x.bss.1);
    let buf = (top + 0xFFF) & !0xFFF;
    assert!(buf + 0x2000 <= STACK, "no room for the argument buffer");
    let end = End { guest: RETURN, host: HOST_RETURN, code: CODE + 4 * t.words.len() as u32 };
    let skip = (STACK & 0x1F_FFFF) as usize..(STACK_TOP & 0x1F_FFFF) as usize;
    let mut faults = BTreeMap::<String, usize>::new();
    let (mut identical, mut steps) = (0usize, 0usize);
    let mut bad = Vec::new();
    let mut missed = std::collections::BTreeSet::new();
    // Words that are the same value seen from each side. A return address the code stores
    // (setjmp's jmp_buf) is the host address of the same return point: longjmp returns through
    // it with `jr ra`, which stays native. A word read from stack nobody initialised may carry
    // the stale upper half of an old return address: garbage on any system, so the same
    // garbage translated (low half equal, upper halves in guest and host code).
    let half = |a: u32, n: u32| (a >> 16)..=((a + n - 1) >> 16);
    let (guest_hi, host_hi) = (half(x.text_addr, x.text.len() as u32), half(CODE, 4 * t.words.len() as u32));
    let (returns, stale) = (Cell::new(false), Cell::new(false));
    let same = |w: u32, o: u32| {
        if o == RETURN && w == HOST_RETURN || t.host(o) == Some(w) {
            returns.set(true);
        } else if w & 0xFFFF == o & 0xFFFF && guest_hi.contains(&(o >> 16)) && host_hi.contains(&(w >> 16)) {
            stale.set(true);
        } else {
            return false;
        }
        true
    };
    let (mut with_returns, mut with_stale) = (Vec::new(), Vec::new());
    let funcs: Vec<u32> = d.entries.keys().copied().filter(|&f| t.host(f).is_some()).collect();
    for &f in &funcs {
        let mut c = Cpu::new(Isa::R3000A, f);
        for r in 1..32 {
            c.gpr[r] = buf + 0x40 * r as u32;
        }
        c.gpr[28] = x.gp0;
        c.gpr[SP as usize] = STACK_TOP;
        c.gpr[RA as usize] = RETURN;
        let mut start = c.clone();
        start.gpr[RA as usize] = HOST_RETURN;
        let mut m1 = base.clone();
        let n = match run_ps1(&mut m1, &mut c, end, 200_000) {
            Ok(n) => n,
            Err(e) => {
                *faults.entry(kind(e)).or_default() += 1;
                continue;
            }
        };
        steps += n;
        let mut m2 = base.clone();
        returns.set(false);
        stale.set(false);
        let why = match run_psp(&mut m2, &t, &start, end, 40 * n + 10_000) {
            Ok((g, log)) => {
                missed.extend(log.missed);
                differs(&c, &m1, &g, &m2, false, skip.clone(), &same)
            }
            Err(e) => Some(e),
        };
        match why {
            None => identical += 1,
            Some(w) => bad.push((f, w)),
        }
        if returns.get() {
            with_returns.push(f);
        }
        if stale.get() {
            with_stale.push(f);
        }
    }
    println!(
        "{name}: {} functions; {} compared ({steps} guest steps): {identical} identical, {} different; \
         not run by the oracle: {faults:?}",
        funcs.len(),
        identical + bad.len(),
        bad.len(),
    );
    println!("dispatch misses run by the interpreter: {missed:x?}");
    println!("stored host return addresses: {with_returns:x?}");
    println!("stale return-address halves from uninitialised stack: {with_stale:x?}");
    for (f, why) in bad.iter().take(30) {
        println!("  {f:#010x}: {why}");
    }
    assert!(bad.is_empty(), "{} functions differ", bad.len());
}
