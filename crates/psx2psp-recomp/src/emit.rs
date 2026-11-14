//! Allegrex emitter: the discovered code of one executable translated into PSP machine code.
//!
//! Guest registers are host registers (identity), except `k0`/`k1`: see [`crate::layout`]. Most
//! instructions are copied as they are; the rest expand:
//!
//! * `add`/`addi`/`sub` become `addu`/`addiu`/`subu` (games do not rely on the overflow trap).
//! * `div`/`divu` get explicit guards: the R3000A's results for a zero divisor and for
//!   `i32::MIN / -1` are defined, the Allegrex's are not.
//! * Loads and stores compute the guest address in `k1`. RAM (bit 23 clear) maps with one
//!   `ins`; anything else takes an out-of-line path: the scratchpad is addressed from `k0`, and
//!   I/O, BIOS and unmapped addresses become a host call that runs the access in the runtime.
//! * Calls are host calls (`jal` writes a host `ra`, `jr ra` returns natively). A `jal` outside
//!   the image, `jr` and `jalr` go through the dispatch table.
//! * COP0, the GTE, `syscall`, `break` and anything touching the guest's `k0`/`k1` run in the
//!   runtime through a host call and continue after it.
//! * Anything the translator does not model hands the guest over to the runtime's interpreter
//!   ("resume"), which comes back to translated code through the dispatch table.
//! * Loop back-edges charge the loop's estimated PS1 cycles to [`ctx::BUDGET`] first and enter
//!   the runtime when it goes negative: the runtime advances the PS1 clock by what was spent
//!   and serves interrupts (it also forces the budget negative to deliver one).
//!
//! Host call, five words: `sw ra,GPR31(k0); jal RT_CALL; nop; .word pc; .word insn`. The runtime
//! saves the guest state to [`CTX`], services pending interrupts, executes `insn` as the guest
//! instruction at `pc`, restores the state and returns past the two data words. With
//! [`RESUME`] set in `pc` it interprets from `pc` instead and never returns there.
//!
//! Delay slots: a slot that is one plain host instruction stays in the host slot. Otherwise the
//! slot is moved before the transfer when it does not write a register the transfer reads, or
//! duplicated on both paths of a conditional branch, or (register jumps) run between saving and
//! reloading the host target.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::discover::{Discovery, JumpReg};
use crate::exe::PsxExe;
use crate::hazard::Hazards;
use crate::insn::{Insn, Isa, Kind, Uses, decode};
use crate::layout::{self, RT_CALL, SCRATCH, ctx, k0_off};

/// Flag in a host call's `pc` word: interpret from `pc`, do not return.
pub const RESUME: u32 = 1;

const ZR: u32 = 0;
const K0R: u32 = 26;
const K1R: u32 = 27;
const RA: u32 = 31;
const GUEST_K: u32 = (1 << K0R) | (1 << K1R);
const NOP: u32 = 0;
/// Upper bound of a loop's charge ([`Em::loop_cost`]): a long body is mostly skipped paths.
const MAX_LOOP_COST: u32 = 1024;

/// Why the runtime handles a guest instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// Executed by the runtime, then translated code continues.
    Cop0,
    Cop2,
    Syscall,
    Break,
    /// Reads or writes the guest's `k0`/`k1`.
    GuestK,
    /// From here on the runtime's interpreter runs the guest.
    Invalid,
    /// The path leaves the discovered code.
    Unreached,
    /// A branch or jump to an address outside the executable.
    Outside,
    /// A transfer in a delay slot.
    BranchInSlot,
    /// The delay slot uses the link register its call writes.
    LinkInSlot,
    /// The delay slot of a linking branch writes a register the branch reads.
    SlotClobbers,
    /// A load whose delay slot the code depends on (see [`crate::hazard`]).
    Hazard,
}

impl Reason {
    /// True if the interpreter takes over instead of the code continuing.
    pub const fn resumes(self) -> bool {
        !matches!(self, Reason::Cop0 | Reason::Cop2 | Reason::Syscall | Reason::Break | Reason::GuestK)
    }
}

/// Translated code of one executable.
#[derive(Clone, Debug, Default)]
pub struct Translation {
    /// Host address of `words[0]`.
    pub base: u32,
    pub words: Vec<u32>,
    /// Guest instruction -> host address of its translation. Every discovered instruction except
    /// delay slots that nothing jumps to; this is the content of the dispatch table.
    pub map: BTreeMap<u32, u32>,
    /// Guest instructions handled by the runtime, and why.
    pub runtime: BTreeMap<u32, Reason>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EmitError {
    #[error("guest {pc:#010x}: host branch needs {words} words, beyond the 16-bit range")]
    Range { pc: u32, words: i64 },
    #[error("guest {pc:#010x} transfers to {target:#010x}, which has no translation")]
    Target { pc: u32, target: u32 },
    #[error("code at {0:#010x} must be in the same 256 MB region as the runtime")]
    Base(u32),
}

/// Allegrex encodings used by the emitter.
mod enc {
    pub const fn r(rs: u32, rt: u32, rd: u32, sa: u32, funct: u32) -> u32 {
        (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
    }
    pub const fn i(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
        (op << 26) | (rs << 21) | (rt << 16) | imm as u32
    }
    pub const fn sll(rd: u32, rt: u32, sa: u32) -> u32 {
        r(0, rt, rd, sa, 0)
    }
    pub const fn srl(rd: u32, rt: u32, sa: u32) -> u32 {
        r(0, rt, rd, sa, 2)
    }
    pub const fn jr(rs: u32) -> u32 {
        r(rs, 0, 0, 0, 8)
    }
    pub const fn jalr(rd: u32, rs: u32) -> u32 {
        r(rs, 0, rd, 0, 9)
    }
    pub const fn mthi(rs: u32) -> u32 {
        r(rs, 0, 0, 0, 0x11)
    }
    pub const fn mtlo(rs: u32) -> u32 {
        r(rs, 0, 0, 0, 0x13)
    }
    pub const fn addu(rd: u32, rs: u32, rt: u32) -> u32 {
        r(rs, rt, rd, 0, 0x21)
    }
    pub const fn subu(rd: u32, rs: u32, rt: u32) -> u32 {
        r(rs, rt, rd, 0, 0x23)
    }
    pub const fn addiu(rt: u32, rs: u32, imm: u16) -> u32 {
        i(9, rs, rt, imm)
    }
    pub const fn andi(rt: u32, rs: u32, imm: u16) -> u32 {
        i(0xC, rs, rt, imm)
    }
    pub const fn ori(rt: u32, rs: u32, imm: u16) -> u32 {
        i(0xD, rs, rt, imm)
    }
    pub const fn xori(rt: u32, rs: u32, imm: u16) -> u32 {
        i(0xE, rs, rt, imm)
    }
    pub const fn lui(rt: u32, imm: u16) -> u32 {
        i(0xF, 0, rt, imm)
    }
    pub const fn lw(rt: u32, off: u16, base: u32) -> u32 {
        i(0x23, base, rt, off)
    }
    pub const fn sw(rt: u32, off: u16, base: u32) -> u32 {
        i(0x2B, base, rt, off)
    }
    /// Branches without their offset (filled by a fixup).
    pub const fn beq(rs: u32, rt: u32) -> u32 {
        i(4, rs, rt, 0)
    }
    pub const fn bne(rs: u32, rt: u32) -> u32 {
        i(5, rs, rt, 0)
    }
    pub const fn bltz(rs: u32) -> u32 {
        i(1, rs, 0, 0)
    }
    pub const fn jal(addr: u32) -> u32 {
        (3 << 26) | ((addr >> 2) & 0x03FF_FFFF)
    }
    /// `rt` <- `size` bits of `rs` from bit `pos`.
    pub const fn ext(rt: u32, rs: u32, pos: u32, size: u32) -> u32 {
        (0x1F << 26) | (rs << 21) | (rt << 16) | ((size - 1) << 11) | (pos << 6)
    }
    /// Bits `pos .. pos + size` of `rt` <- low bits of `rs`.
    pub const fn ins(rt: u32, rs: u32, pos: u32, size: u32) -> u32 {
        (0x1F << 26) | (rs << 21) | (rt << 16) | ((pos + size - 1) << 11) | (pos << 6) | 4
    }
}

use enc::*;

/// The host word for a guest instruction that translates to exactly itself (or its non-trapping
/// twin) and does not touch the guest's `k0`/`k1`.
fn simple(i: Insn, k: Kind) -> Option<u32> {
    use Kind::*;
    let u = Uses::of(i, k);
    if (u.read | u.write) & GUEST_K != 0 {
        return None;
    }
    match k {
        Sll | Srl | Sra | Sllv | Srlv | Srav | Mfhi | Mthi | Mflo | Mtlo | Mult | Multu | Addu
        | Subu | And | Or | Xor | Nor | Slt | Sltu | Addiu | Slti | Sltiu | Andi | Ori | Xori
        | Lui => Some(i.0),
        Add => Some((i.0 & !0x3F) | 0x21),
        Sub => Some((i.0 & !0x3F) | 0x23),
        Addi => Some((i.0 & 0x03FF_FFFF) | (9 << 26)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum To {
    Guest(u32),
    Label(usize),
}

#[derive(Clone, Copy)]
enum Fx {
    /// 16-bit branch offset.
    Branch,
    /// 26-bit `j`/`jal` target.
    Jump,
}

/// A piece of host code with unresolved branches.
#[derive(Default)]
struct Code {
    w: Vec<u32>,
    /// Word index, kind, destination, guest pc for errors.
    fix: Vec<(usize, Fx, To, u32)>,
    /// Label -> word index.
    defs: Vec<(usize, usize)>,
}

impl Code {
    fn push(&mut self, w: u32) {
        self.w.push(w);
    }
    fn branch(&mut self, w: u32, to: To, pc: u32) {
        self.fix.push((self.w.len(), Fx::Branch, to, pc));
        self.w.push(w & 0xFFFF_0000);
    }
    /// `j` (op 2) or `jal` (op 3).
    fn jump(&mut self, op: u32, to: To, pc: u32) {
        self.fix.push((self.w.len(), Fx::Jump, to, pc));
        self.w.push(op << 26);
    }
    fn def(&mut self, label: usize) {
        self.defs.push((label, self.w.len()));
    }
    fn append(&mut self, o: Code) {
        let b = self.w.len();
        self.w.extend(o.w);
        self.fix.extend(o.fix.into_iter().map(|(a, f, t, p)| (a + b, f, t, p)));
        self.defs.extend(o.defs.into_iter().map(|(l, a)| (l, a + b)));
    }
    /// Host call for the guest instruction `word` at `pc` (`pc | RESUME` to hand over).
    fn host_call(&mut self, pc: u32, word: u32) {
        self.push(sw(RA, k0_off(layout::CTX + ctx::GPR + 4 * RA), K0R));
        self.push(jal(RT_CALL));
        self.push(NOP);
        self.push(pc);
        self.push(word);
    }
}

/// Stubs are placed after the next unconditional transfer, or jumped over once the oldest
/// pending one could fall out of branch range.
const FLUSH_WORDS: usize = 0x4000;

struct Em<'a> {
    exe: &'a PsxExe,
    d: &'a Discovery,
    hz: &'a Hazards,
    out: Code,
    stubs: Vec<Code>,
    stub_words: usize,
    oldest: usize,
    labels: usize,
    map: BTreeMap<u32, usize>,
    runtime: BTreeMap<u32, Reason>,
    /// Conditional branches whose guest target is beyond 16 bits in host code.
    long: &'a BTreeSet<u32>,
}

enum Fail {
    /// Conditional branches (guest pc, host distance) to guest targets out of range.
    Far(Vec<(u32, i64)>),
    Error(EmitError),
}

/// Translates the discovered code of `exe` to host code at `base`.
///
/// A conditional branch takes its guest target directly when it is within 16 bits in host code;
/// the others go through a nearby `j`. Which ones need it is known only once the code is laid
/// out, so the translation repeats until every branch fits.
pub fn translate(
    exe: &PsxExe,
    d: &Discovery,
    hz: &Hazards,
    base: u32,
) -> Result<Translation, EmitError> {
    if base & 3 != 0 || (base ^ RT_CALL) & 0xF000_0000 != 0 {
        return Err(EmitError::Base(base));
    }
    let mut long = BTreeSet::new();
    loop {
        match emit(exe, d, hz, base, &long) {
            Ok(t) => return Ok(t),
            Err(Fail::Error(e)) => return Err(e),
            Err(Fail::Far(far)) => {
                let n = long.len();
                long.extend(far.iter().map(|&(pc, _)| pc));
                if long.len() == n {
                    let (pc, words) = far[0];
                    return Err(EmitError::Range { pc, words });
                }
            }
        }
    }
}

fn emit(
    exe: &PsxExe,
    d: &Discovery,
    hz: &Hazards,
    base: u32,
    long: &BTreeSet<u32>,
) -> Result<Translation, Fail> {
    let mut em = Em {
        exe,
        d,
        hz,
        long,
        out: Code::default(),
        stubs: Vec::new(),
        stub_words: 0,
        oldest: 0,
        labels: 0,
        map: BTreeMap::new(),
        runtime: BTreeMap::new(),
    };
    let targets = em.targets();
    for &a in d.code.iter().filter(|a| !d.delay_slots.contains_key(a)) {
        em.far_flush();
        em.map.insert(a, em.out.w.len());
        match em.insn(a) {
            Some(n) if em.first(n) => {}
            Some(n) => {
                let c = em.resume(n, Reason::Unreached);
                em.out.append(c);
                em.flush();
            }
            None => em.flush(),
        }
    }
    // Delay slots that are also jumped to get a copy of their own.
    for &s in d.delay_slots.keys().filter(|s| targets.contains(s) && d.code.contains(s)) {
        em.far_flush();
        em.map.insert(s, em.out.w.len());
        let c = match em.at(s) {
            Some((i, k)) if !k.has_delay_slot() && !em.hz.sites.contains_key(&s) => {
                let (mut c, falls) = em.plain(s, i, k);
                if falls {
                    if em.first(s + 4) {
                        c.jump(2, To::Guest(s + 4), s);
                        c.push(NOP);
                    } else {
                        c.append(em.resume(s + 4, Reason::Unreached));
                    }
                }
                c
            }
            _ => em.resume(s, Reason::Invalid),
        };
        em.out.append(c);
        em.flush();
    }
    em.flush();
    em.finish(base)
}

impl Em<'_> {
    fn at(&self, a: u32) -> Option<(Insn, Kind)> {
        let i = Insn(self.exe.word(a)?);
        Some((i, decode(i, Isa::R3000A)))
    }

    /// True if `a` is translated in the main pass (code, not a delay slot).
    fn first(&self, a: u32) -> bool {
        self.d.code.contains(&a) && !self.d.delay_slots.contains_key(&a)
    }

    fn label(&mut self) -> usize {
        self.labels += 1;
        self.labels - 1
    }

    /// Every address something can transfer to directly.
    fn targets(&self) -> BTreeSet<u32> {
        use Kind::*;
        let d = self.d;
        let mut t: BTreeSet<u32> = d.entries.keys().copied().collect();
        t.extend(d.calls.keys());
        t.extend(d.indirect_calls.values().flatten());
        for j in d.jumps.values() {
            match j {
                JumpReg::Table { targets, .. } => t.extend(targets),
                JumpReg::Const(c) => {
                    t.insert(*c);
                }
                JumpReg::Unresolved(_) => {}
            }
        }
        for &a in &d.code {
            match self.at(a) {
                Some((i, J | Jal)) => {
                    t.insert(i.jump_target(a));
                }
                Some((i, Beq | Bne | Blez | Bgtz | Bltz | Bgez | Bltzal | Bgezal)) => {
                    t.insert(i.branch_target(a));
                }
                _ => {}
            }
        }
        t
    }

    /// Where the conditional branch at `a` to guest `t` points: `t` itself, or a nearby `j t`
    /// when `t` is out of range.
    fn near(&mut self, a: u32, t: u32) -> To {
        if !self.long.contains(&a) {
            return To::Guest(t);
        }
        let l = self.label();
        let mut s = Code::default();
        s.def(l);
        s.jump(2, To::Guest(t), a);
        s.push(NOP);
        self.stub(s);
        To::Label(l)
    }

    fn stub(&mut self, s: Code) {
        if self.stubs.is_empty() {
            self.oldest = self.out.w.len();
        }
        self.stub_words += s.w.len();
        self.stubs.push(s);
    }

    fn flush(&mut self) {
        for s in std::mem::take(&mut self.stubs) {
            self.out.append(s);
        }
        self.stub_words = 0;
    }

    fn far_flush(&mut self) {
        if !self.stubs.is_empty() && self.out.w.len() - self.oldest + self.stub_words > FLUSH_WORDS {
            let over = self.label();
            self.out.jump(2, To::Label(over), 0);
            self.out.push(NOP);
            self.flush();
            self.out.def(over);
        }
    }

    fn exec(&mut self, pc: u32, word: u32, why: Reason) -> Code {
        self.runtime.insert(pc, why);
        let mut c = Code::default();
        c.host_call(pc, word);
        c
    }

    fn resume(&mut self, pc: u32, why: Reason) -> Code {
        self.runtime.insert(pc, why);
        let mut c = Code::default();
        c.host_call(pc | RESUME, self.exe.word(pc).unwrap_or(0));
        c
    }

    /// The main-pass instruction at `a` (with its delay slot). Returns where the guest continues
    /// in straight line, if it can.
    fn insn(&mut self, a: u32) -> Option<u32> {
        let Some((i, k)) = self.at(a) else {
            let c = self.resume(a, Reason::Outside);
            self.out.append(c);
            return None;
        };
        if k.has_delay_slot() {
            return self.transfer(a, i, k);
        }
        let (c, falls) = if self.hz.sites.contains_key(&a) {
            (self.resume(a, Reason::Hazard), false)
        } else {
            self.plain(a, i, k)
        };
        self.out.append(c);
        falls.then_some(a + 4)
    }

    /// A non-transfer instruction. Returns its code and whether it continues at the next one.
    fn plain(&mut self, pc: u32, i: Insn, k: Kind) -> (Code, bool) {
        use Kind::*;
        if let Some(w) = simple(i, k) {
            let mut c = Code::default();
            c.push(w);
            return (c, true);
        }
        let u = Uses::of(i, k);
        if (u.read | u.write) & GUEST_K != 0 && k != Invalid {
            return (self.exec(pc, i.0, Reason::GuestK), true);
        }
        let c = match k {
            Div | Divu => self.div(pc, i, k == Div),
            Lb | Lh | Lwl | Lw | Lbu | Lhu | Lwr | Sb | Sh | Swl | Sw | Swr => self.mem(pc, i),
            Mfc0 | Mtc0 | Rfe => self.exec(pc, i.0, Reason::Cop0),
            Mfc2 | Cfc2 | Mtc2 | Ctc2 | Gte | Lwc2 | Swc2 => self.exec(pc, i.0, Reason::Cop2),
            Syscall => self.exec(pc, i.0, Reason::Syscall),
            Break => self.exec(pc, i.0, Reason::Break),
            _ => return (self.resume(pc, Reason::Invalid), false),
        };
        (c, true)
    }

    /// `div`/`divu rs,rt` with the R3000A's results for the cases the Allegrex leaves undefined.
    fn div(&mut self, pc: u32, i: Insn, signed: bool) -> Code {
        let (rs, rt) = (i.rs() as u32, i.rt() as u32);
        let (zero, back) = (self.label(), self.label());
        let mut c = Code::default();
        let mut z = Code::default();
        c.branch(beq(rt, ZR), To::Label(zero), pc);
        z.def(zero);
        z.push(mthi(rs));
        if signed {
            let minus1 = self.label();
            c.push(addiu(K1R, rt, 1));
            c.branch(beq(K1R, ZR), To::Label(minus1), pc);
            c.push(NOP);
            // rs / 0: lo = rs < 0 ? 1 : -1
            z.push(srl(K1R, rs, 31));
            z.push(sll(K1R, K1R, 1));
            z.push(addiu(K1R, K1R, 0xFFFF));
            // rs / -1: lo = -rs (i32::MIN stays), hi = 0
            let mut m = Code::default();
            m.def(minus1);
            m.push(mthi(ZR));
            m.push(subu(K1R, ZR, rs));
            m.push(mtlo(K1R));
            m.jump(2, To::Label(back), pc);
            m.push(NOP);
            self.stub(m);
        } else {
            c.push(NOP);
            z.push(addiu(K1R, ZR, 0xFFFF));
        }
        z.push(mtlo(K1R));
        z.jump(2, To::Label(back), pc);
        z.push(NOP);
        self.stub(z);
        c.push(i.0);
        c.def(back);
        c
    }

    /// A load or store through the guest address in `k1`.
    fn mem(&mut self, pc: u32, i: Insn) -> Code {
        let (rs, off) = (i.rs() as u32, i.imm() as u16);
        // Same operation and data register, base k1.
        let op = (i.0 & 0xFC1F_0000) | (K1R << 21);
        let (slow, io, back) = (self.label(), self.label(), self.label());
        let mut c = Code::default();
        c.push(addiu(K1R, rs, off));
        c.push(sll(K1R, K1R, 8));
        c.branch(bltz(K1R), To::Label(slow), pc);
        c.push(srl(K1R, K1R, 8));
        c.push(ins(K1R, K0R, 21, 11));
        c.push(op);
        c.def(back);
        // Bit 23 set: scratchpad if bits 10-23 are 0x2000, else the runtime.
        let mut s = Code::default();
        s.def(slow);
        s.push(srl(K1R, K1R, 10));
        s.push(xori(K1R, K1R, 0x2000));
        s.branch(bne(K1R, ZR), To::Label(io), pc);
        s.push(addiu(K1R, rs, off));
        s.push(andi(K1R, K1R, 0x3FF));
        s.push(addu(K1R, K1R, K0R));
        s.push(op | k0_off(SCRATCH) as u32);
        s.jump(2, To::Label(back), pc);
        s.push(NOP);
        s.def(io);
        s.host_call(pc, i.0);
        s.jump(2, To::Label(back), pc);
        s.push(NOP);
        self.stub(s);
        c
    }

    /// PS1 cycles charged for one pass of the loop `t..=a` (`a` the back-edge): its words, plus
    /// two per memory access for the R3000A's load and store stalls. An estimate: what matters is
    /// that a counted wait loop (PsyQ's `VSync` timeout) lasts as long as on the PS1.
    fn loop_cost(&self, t: u32, a: u32) -> u16 {
        use Kind::*;
        let mut cost = 0u32;
        for w in (t..=a + 4).step_by(4).filter(|w| self.d.code.contains(w)) {
            let mem = self.at(w).is_some_and(|(_, k)| {
                matches!(k, Lb | Lh | Lwl | Lw | Lbu | Lhu | Lwr | Sb | Sh | Swl | Sw | Swr | Lwc2 | Swc2)
            });
            cost += if mem { 3 } else { 1 };
        }
        cost.clamp(1, MAX_LOOP_COST) as u16
    }

    /// `lw k1,BUDGET(k0); addiu k1,k1,-cost; bltz k1,irq; sw k1,BUDGET(k0)` with the host call
    /// out of line.
    fn poll(&mut self, pc: u32, cost: u16) {
        let (irq, back) = (self.label(), self.label());
        let budget = k0_off(layout::CTX + ctx::BUDGET);
        self.out.push(lw(K1R, budget, K0R));
        self.out.push(addiu(K1R, K1R, cost.wrapping_neg()));
        self.out.branch(bltz(K1R), To::Label(irq), pc);
        self.out.push(sw(K1R, budget, K0R));
        self.out.def(back);
        let mut s = Code::default();
        s.def(irq);
        s.host_call(pc, NOP);
        s.jump(2, To::Label(back), pc);
        s.push(NOP);
        self.stub(s);
    }

    /// `k1` <- host address for the guest address in `src` (also saved to `ctx::TARGET`).
    fn dispatch(&mut self, src: u32) {
        let o = &mut self.out;
        o.push(sw(src, k0_off(layout::CTX + ctx::TARGET), K0R));
        o.push(ext(K1R, src, 0, 21));
        o.push(addu(K1R, K1R, K0R));
        o.push(lw(K1R, k0_off(layout::TABLE), K1R));
    }

    /// A branch or jump at `a` and its delay slot.
    fn transfer(&mut self, a: u32, i: Insn, k: Kind) -> Option<u32> {
        use Kind::*;
        let s = a + 4;
        let Some((si, sk)) = self.at(s) else {
            let c = self.resume(a, Reason::Outside);
            self.out.append(c);
            return None;
        };
        let (bu, su) = (Uses::of(i, k), Uses::of(si, sk));
        let link = match k {
            Jal | Bltzal | Bgezal => 1 << RA,
            Jalr => 1 << i.rd(),
            _ => 0,
        };
        let simple_slot = simple(si, sk);
        let clobbers = su.write & bu.read != 0;
        let why = if sk.has_delay_slot() {
            Some(Reason::BranchInSlot)
        } else if self.hz.sites.contains_key(&s) {
            Some(Reason::Hazard)
        } else if (bu.read | bu.write) & GUEST_K != 0 {
            Some(Reason::GuestK)
        } else if (su.read | su.write) & link != 0 {
            Some(Reason::LinkInSlot)
        } else if link != 0 && clobbers && simple_slot.is_none() {
            // A conditional call cannot duplicate its slot: the copy would be the return point.
            Some(Reason::SlotClobbers)
        } else {
            None
        };
        if let Some(why) = why {
            let c = self.resume(a, why);
            self.out.append(c);
            return None;
        }
        // The slot's code when it is not one plain word.
        let slot = |em: &mut Self| em.plain(s, si, sk).0;
        let uncond = matches!(k, J) || (k == Beq && i.rs() == i.rt()) || (k == Bgez && i.rs() == 0);
        let bal = k == Bgezal && i.rs() == 0;
        match k {
            Jr if i.rs() == RA as usize => {
                if let Some(w) = simple_slot {
                    self.out.push(jr(RA));
                    self.out.push(w);
                } else if clobbers {
                    self.out.push(sw(RA, k0_off(layout::CTX + ctx::JUMP), K0R));
                    let c = slot(self);
                    self.out.append(c);
                    self.out.push(lw(K1R, k0_off(layout::CTX + ctx::JUMP), K0R));
                    self.out.push(jr(K1R));
                    self.out.push(NOP);
                } else {
                    let c = slot(self);
                    self.out.append(c);
                    self.out.push(jr(RA));
                    self.out.push(NOP);
                }
                None
            }
            Jr | Jalr => {
                let fin = if k == Jr { jr(K1R) } else { jalr(i.rd() as u32, K1R) };
                let rs = i.rs() as u32;
                if let Some(w) = simple_slot {
                    self.dispatch(rs);
                    self.out.push(fin);
                    self.out.push(w);
                } else if !clobbers {
                    let c = slot(self);
                    self.out.append(c);
                    self.dispatch(rs);
                    self.out.push(fin);
                    self.out.push(NOP);
                } else {
                    self.dispatch(rs);
                    self.out.push(sw(K1R, k0_off(layout::CTX + ctx::JUMP), K0R));
                    let c = slot(self);
                    self.out.append(c);
                    self.out.push(lw(K1R, k0_off(layout::CTX + ctx::JUMP), K0R));
                    self.out.push(fin);
                    self.out.push(NOP);
                }
                (k == Jalr).then_some(a + 8)
            }
            Jal | Bgezal if k == Jal || bal => {
                let t = if k == Jal { i.jump_target(a) } else { i.branch_target(a) };
                let inside = self.d.code.contains(&t);
                if simple_slot.is_none() {
                    let c = slot(self);
                    self.out.append(c);
                }
                if inside {
                    self.out.jump(3, To::Guest(t), a);
                } else {
                    self.out.push(lui(K1R, (t >> 16) as u16));
                    self.out.push(ori(K1R, K1R, t as u16));
                    self.dispatch(K1R);
                    self.out.push(jalr(RA, K1R));
                }
                self.out.push(simple_slot.unwrap_or(NOP));
                Some(a + 8)
            }
            _ => {
                let t = if k == J { i.jump_target(a) } else { i.branch_target(a) };
                if !self.d.code.contains(&t) {
                    let c = self.resume(a, Reason::Outside);
                    self.out.append(c);
                    return None;
                }
                if t <= a && link == 0 {
                    let cost = self.loop_cost(t, a);
                    self.poll(a, cost);
                }
                if uncond {
                    if simple_slot.is_none() {
                        let c = slot(self);
                        self.out.append(c);
                    }
                    self.out.jump(2, To::Guest(t), a);
                    self.out.push(simple_slot.unwrap_or(NOP));
                    return None;
                }
                let word = i.0;
                if let Some(w) = simple_slot {
                    let to = self.near(a, t);
                    self.out.branch(word, to, a);
                    self.out.push(w);
                } else if !clobbers {
                    let c = slot(self);
                    self.out.append(c);
                    let to = self.near(a, t);
                    self.out.branch(word, to, a);
                    self.out.push(NOP);
                } else {
                    let taken = self.label();
                    self.out.branch(word, To::Label(taken), a);
                    self.out.push(NOP);
                    let c = slot(self);
                    self.out.append(c);
                    let mut st = Code::default();
                    st.def(taken);
                    st.append(slot(self));
                    st.jump(2, To::Guest(t), a);
                    st.push(NOP);
                    self.stub(st);
                }
                Some(a + 8)
            }
        }
    }

    fn finish(self, base: u32) -> Result<Translation, Fail> {
        let Em { out, map, runtime, labels, .. } = self;
        let Code { mut w, fix, defs } = out;
        let mut at = vec![usize::MAX; labels];
        for (l, i) in defs {
            at[l] = i;
        }
        debug_assert!(!at.contains(&usize::MAX), "label used but never placed");
        let host = |i: usize| base + 4 * i as u32;
        let mut far = Vec::new();
        for (i, fx, to, pc) in fix {
            let t = match to {
                To::Guest(g) => *map
                    .get(&g)
                    .ok_or(Fail::Error(EmitError::Target { pc, target: g }))?,
                To::Label(l) => at[l],
            };
            match fx {
                Fx::Branch => {
                    let words = t as i64 - i as i64 - 1;
                    if words < i16::MIN as i64 || words > i16::MAX as i64 {
                        match to {
                            To::Guest(_) => far.push((pc, words)),
                            To::Label(_) => return Err(Fail::Error(EmitError::Range { pc, words })),
                        }
                    }
                    w[i] |= words as u16 as u32;
                }
                Fx::Jump => w[i] |= (host(t) >> 2) & 0x03FF_FFFF,
            }
        }
        if !far.is_empty() {
            return Err(Fail::Far(far));
        }
        Ok(Translation {
            base,
            words: w,
            map: map.into_iter().map(|(g, i)| (g, host(i))).collect(),
            runtime,
        })
    }
}

impl Translation {
    /// Host address of the translation of guest `pc`.
    pub fn host(&self, pc: u32) -> Option<u32> {
        self.map.get(&pc).copied()
    }

    /// Dispatch-table entries: (host address of the entry, value).
    pub fn table(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.map.iter().map(|(&g, &h)| (layout::table_entry(g), h))
    }
}
