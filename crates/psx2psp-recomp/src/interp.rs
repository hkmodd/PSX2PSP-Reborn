//! A MIPS interpreter for both sides of the translation: the oracle of the differential tests.
//!
//! [`Isa::R3000A`] runs PS1 code as the console does: one load delay slot (a load's value reaches
//! the register after the next instruction, and that instruction's own write to the register
//! cancels the load), `add`/`addi`/`sub` overflow traps, the R3000A's results for a division by
//! zero or of `i32::MIN` by -1.
//!
//! [`Isa::Allegrex`] runs translated code: loads are interlocked, and anything whose result on the
//! Allegrex is not documented (division by zero or overflow, the trapping `add`/`addi`/`sub`)
//! stops with [`Trap::Undefined`], so a differential test can never pass on behaviour the real
//! CPU might not have.
//!
//! A step that stops leaves the CPU as it was before it: `pc` is the stopping instruction, no
//! register or memory has changed (the bus is only written by the last action of a store).

use crate::insn::{Insn, Isa, Kind, decode};

/// Width of a memory access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Byte = 1,
    Half = 2,
    Word = 4,
}

/// Why execution stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    /// Fetch or data access to an address the bus does not map.
    Bus { addr: u32 },
    /// Halfword or word access, or fetch, at a misaligned address.
    Unaligned { addr: u32 },
    /// A word that does not decode for this CPU.
    Invalid { word: u32 },
    /// R3000A `add`/`addi`/`sub` overflow.
    Overflow,
    Syscall { code: u32 },
    Break { code: u32 },
    /// Coprocessor 2 access on a bus that provides no GTE.
    NoCop2,
    /// An Allegrex operation whose result is not documented.
    Undefined,
}

/// A trap and the address of the instruction that raised it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stop {
    pub pc: u32,
    pub trap: Trap,
}

/// Memory and coprocessor 2 as seen by the CPU. Addresses are virtual: the bus decides the map
/// (PS1 mirrors and segments, or the PSP's own layout). Alignment is checked by the CPU.
pub trait Bus {
    /// Reads `size` bytes at `addr`, zero-extended.
    fn load(&mut self, addr: u32, size: Size) -> Result<u32, Trap>;
    /// Writes the low `size` bytes of `value` at `addr`.
    fn store(&mut self, addr: u32, size: Size, value: u32) -> Result<(), Trap>;
    /// Reads the instruction word at `addr`.
    fn fetch(&mut self, addr: u32) -> Result<u32, Trap> {
        self.load(addr, Size::Word)
    }
    /// GTE register: 0-31 data, 32-63 control.
    fn cop2_read(&mut self, _reg: usize) -> Result<u32, Trap> {
        Err(Trap::NoCop2)
    }
    fn cop2_write(&mut self, _reg: usize, _value: u32) -> Result<(), Trap> {
        Err(Trap::NoCop2)
    }
    /// GTE command (the instruction's low 25 bits).
    fn cop2_command(&mut self, _command: u32) -> Result<(), Trap> {
        Err(Trap::NoCop2)
    }
}

/// CPU state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub isa: Isa,
    pub gpr: [u32; 32],
    pub hi: u32,
    pub lo: u32,
    /// The instruction executed by the next step.
    pub pc: u32,
    /// The one after it (differs from `pc + 4` while `pc` is a delay slot).
    pub next_pc: u32,
    /// COP0 registers (R3000A: SR = 12, CAUSE = 13, EPC = 14).
    pub cop0: [u32; 32],
    /// R3000A load in flight: register and value, written after the next instruction.
    pub pending: Option<(usize, u32)>,
}

/// What an executed instruction leaves behind, applied once nothing can fail any more.
enum Out {
    None,
    Reg(usize, u32),
    Load(usize, u32),
    HiLo(u32, u32),
}

impl Cpu {
    pub fn new(isa: Isa, pc: u32) -> Self {
        Cpu {
            isa,
            gpr: [0; 32],
            hi: 0,
            lo: 0,
            pc,
            next_pc: pc.wrapping_add(4),
            cop0: [0; 32],
            pending: None,
        }
    }

    /// Moves execution to `pc` (not in a delay slot).
    pub fn jump(&mut self, pc: u32) {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
    }

    /// Steps until `pc` reaches `until` outside a delay slot, at most `max` steps. Returns the
    /// number of steps, or the stop. Running out of steps is reported as `Trap::Break` with code
    /// `u32::MAX` at the current pc.
    pub fn run_until(&mut self, bus: &mut impl Bus, until: u32, max: u64) -> Result<u64, Stop> {
        let mut n = 0;
        while !(self.pc == until && self.next_pc == until.wrapping_add(4)) {
            if n == max {
                return Err(Stop { pc: self.pc, trap: Trap::Break { code: u32::MAX } });
            }
            self.step(bus)?;
            n += 1;
        }
        Ok(n)
    }

    /// Executes one instruction.
    pub fn step(&mut self, bus: &mut impl Bus) -> Result<(), Stop> {
        let pc = self.pc;
        let next = self.next_pc;
        let stop = |trap| Stop { pc, trap };
        if pc & 3 != 0 {
            return Err(stop(Trap::Unaligned { addr: pc }));
        }
        let i = Insn(bus.fetch(pc).map_err(stop)?);
        let k = decode(i, self.isa);
        self.pc = next;
        self.next_pc = next.wrapping_add(4);
        let in_flight = self.pending.take();
        let out = match self.exec(i, k, pc, in_flight, bus) {
            Ok(out) => out,
            Err(trap) => {
                self.pc = pc;
                self.next_pc = next;
                self.pending = in_flight;
                return Err(stop(trap));
            }
        };
        // The previous load lands now, unless this instruction wrote the same register, directly or
        // by a load of its own (a second load drops the first: the slot after it still reads the
        // value from before both, as cpu.c and DuckStation do).
        let mut landing = in_flight;
        match out {
            Out::None => {}
            Out::Reg(r, v) => {
                if landing.is_some_and(|(p, _)| p == r) {
                    landing = None;
                }
                self.gpr[r] = v;
            }
            Out::HiLo(hi, lo) => {
                self.hi = hi;
                self.lo = lo;
            }
            Out::Load(r, v) => match self.isa {
                Isa::R3000A => {
                    if landing.is_some_and(|(p, _)| p == r) {
                        landing = None;
                    }
                    self.pending = Some((r, v));
                }
                Isa::Allegrex => self.gpr[r] = v,
            },
        }
        if let Some((r, v)) = landing {
            self.gpr[r] = v;
        }
        self.gpr[0] = 0;
        if self.pending.is_some_and(|(r, _)| r == 0) {
            self.pending = None;
        }
        Ok(())
    }

    fn exec(
        &mut self,
        i: Insn,
        k: Kind,
        pc: u32,
        in_flight: Option<(usize, u32)>,
        bus: &mut impl Bus,
    ) -> Result<Out, Trap> {
        use Kind::*;
        let ps1 = self.isa == Isa::R3000A;
        let s = self.gpr[i.rs()];
        let t = self.gpr[i.rt()];
        let (rd, rt) = (i.rd(), i.rt());
        let ea = s.wrapping_add(i.simm());
        let link = pc.wrapping_add(8);
        let out = match k {
            Sll => Out::Reg(rd, t << i.sa()),
            Srl => Out::Reg(rd, t >> i.sa()),
            Sra => Out::Reg(rd, ((t as i32) >> i.sa()) as u32),
            Sllv => Out::Reg(rd, t << (s & 31)),
            Srlv => Out::Reg(rd, t >> (s & 31)),
            Srav => Out::Reg(rd, ((t as i32) >> (s & 31)) as u32),
            Jr => {
                self.next_pc = s;
                Out::None
            }
            Jalr => {
                self.next_pc = s;
                Out::Reg(rd, link)
            }
            Syscall => return Err(Trap::Syscall { code: i.code() }),
            Break => return Err(Trap::Break { code: i.code() }),
            Mfhi => Out::Reg(rd, self.hi),
            Mflo => Out::Reg(rd, self.lo),
            Mthi => Out::HiLo(s, self.lo),
            Mtlo => Out::HiLo(self.hi, s),
            Mult => {
                let p = (s as i32 as i64) * (t as i32 as i64);
                Out::HiLo((p >> 32) as u32, p as u32)
            }
            Multu => {
                let p = (s as u64) * (t as u64);
                Out::HiLo((p >> 32) as u32, p as u32)
            }
            Div => {
                let (n, d) = (s as i32, t as i32);
                if d == 0 || (n == i32::MIN && d == -1) {
                    if !ps1 {
                        return Err(Trap::Undefined);
                    }
                    if d == 0 {
                        Out::HiLo(s, if n >= 0 { u32::MAX } else { 1 })
                    } else {
                        Out::HiLo(0, s)
                    }
                } else {
                    Out::HiLo((n % d) as u32, (n / d) as u32)
                }
            }
            Divu => {
                if t == 0 {
                    if !ps1 {
                        return Err(Trap::Undefined);
                    }
                    Out::HiLo(s, u32::MAX)
                } else {
                    Out::HiLo(s % t, s / t)
                }
            }
            Add | Sub | Addi => {
                if !ps1 {
                    return Err(Trap::Undefined);
                }
                let (a, b) = (s as i32, if k == Addi { i.simm() as i32 } else { t as i32 });
                let r = if k == Sub { a.checked_sub(b) } else { a.checked_add(b) };
                let r = r.ok_or(Trap::Overflow)? as u32;
                Out::Reg(if k == Addi { rt } else { rd }, r)
            }
            Addu => Out::Reg(rd, s.wrapping_add(t)),
            Subu => Out::Reg(rd, s.wrapping_sub(t)),
            And => Out::Reg(rd, s & t),
            Or => Out::Reg(rd, s | t),
            Xor => Out::Reg(rd, s ^ t),
            Nor => Out::Reg(rd, !(s | t)),
            Slt => Out::Reg(rd, ((s as i32) < (t as i32)) as u32),
            Sltu => Out::Reg(rd, (s < t) as u32),
            Bltz | Bgez | Bltzal | Bgezal | Beq | Bne | Blez | Bgtz => {
                let taken = match k {
                    Bltz | Bltzal => (s as i32) < 0,
                    Bgez | Bgezal => (s as i32) >= 0,
                    Beq => s == t,
                    Bne => s != t,
                    Blez => (s as i32) <= 0,
                    _ => (s as i32) > 0,
                };
                if taken {
                    self.next_pc = i.branch_target(pc);
                }
                if matches!(k, Bltzal | Bgezal) { Out::Reg(31, link) } else { Out::None }
            }
            Beql | Bnel | Blezl | Bgtzl => {
                let taken = match k {
                    Beql => s == t,
                    Bnel => s != t,
                    Blezl => (s as i32) <= 0,
                    _ => (s as i32) > 0,
                };
                if taken {
                    self.next_pc = i.branch_target(pc);
                } else {
                    // The delay slot is annulled.
                    self.pc = link;
                    self.next_pc = link.wrapping_add(4);
                }
                Out::None
            }
            J => {
                self.next_pc = i.jump_target(pc);
                Out::None
            }
            Jal => {
                self.next_pc = i.jump_target(pc);
                Out::Reg(31, link)
            }
            Addiu => Out::Reg(rt, s.wrapping_add(i.simm())),
            Slti => Out::Reg(rt, ((s as i32) < (i.simm() as i32)) as u32),
            Sltiu => Out::Reg(rt, (s < i.simm()) as u32),
            Andi => Out::Reg(rt, s & i.imm()),
            Ori => Out::Reg(rt, s | i.imm()),
            Xori => Out::Reg(rt, s ^ i.imm()),
            Lui => Out::Reg(rt, i.imm() << 16),
            Lb => Out::Load(rt, load(bus, ea, Size::Byte)? as u8 as i8 as i32 as u32),
            Lbu => Out::Load(rt, load(bus, ea, Size::Byte)?),
            Lh => Out::Load(rt, load(bus, ea, Size::Half)? as u16 as i16 as i32 as u32),
            Lhu => Out::Load(rt, load(bus, ea, Size::Half)?),
            Lw => Out::Load(rt, load(bus, ea, Size::Word)?),
            Lwl | Lwr => {
                let mem = load(bus, ea & !3, Size::Word)?;
                // The R3000A merges with a load still in flight to the same register.
                let old = match in_flight {
                    Some((r, v)) if r == rt => v,
                    _ => t,
                };
                let sh = (ea & 3) * 8;
                let v = if k == Lwl {
                    (old & (0x00FF_FFFF >> sh)) | (mem << (24 - sh))
                } else {
                    (old & (0xFFFF_FF00u32 << (24 - sh))) | (mem >> sh)
                };
                Out::Load(rt, v)
            }
            Sb => store(bus, ea, Size::Byte, t)?,
            Sh => store(bus, ea, Size::Half, t)?,
            Sw => store(bus, ea, Size::Word, t)?,
            Swl | Swr => {
                let a = ea & !3;
                let mem = load(bus, a, Size::Word)?;
                let sh = (ea & 3) * 8;
                let v = if k == Swl {
                    (mem & (0xFFFF_FF00u32 << sh)) | (t >> (24 - sh))
                } else {
                    (mem & (0x00FF_FFFF >> (24 - sh))) | (t << sh)
                };
                store(bus, a, Size::Word, v)?
            }
            Mfc0 => Out::Load(rt, self.cop0[rd]),
            Mtc0 => {
                self.cop0[rd] = t;
                Out::None
            }
            Rfe => {
                let sr = self.cop0[12];
                self.cop0[12] = (sr & !0xF) | ((sr >> 2) & 0xF);
                Out::None
            }
            Mfc2 => Out::Load(rt, bus.cop2_read(rd)?),
            Cfc2 => Out::Load(rt, bus.cop2_read(rd + 32)?),
            Mtc2 => {
                bus.cop2_write(rd, t)?;
                Out::None
            }
            Ctc2 => {
                bus.cop2_write(rd + 32, t)?;
                Out::None
            }
            Gte => {
                bus.cop2_command(i.0 & 0x01FF_FFFF)?;
                Out::None
            }
            Lwc2 => {
                let v = load(bus, ea, Size::Word)?;
                bus.cop2_write(rt, v)?;
                Out::None
            }
            Swc2 => {
                let v = bus.cop2_read(rt)?;
                store(bus, ea, Size::Word, v)?
            }
            Movz => {
                if t == 0 { Out::Reg(rd, s) } else { Out::None }
            }
            Movn => {
                if t != 0 { Out::Reg(rd, s) } else { Out::None }
            }
            Clz => Out::Reg(rd, s.leading_zeros()),
            Clo => Out::Reg(rd, s.leading_ones()),
            Max => Out::Reg(rd, (s as i32).max(t as i32) as u32),
            Min => Out::Reg(rd, (s as i32).min(t as i32) as u32),
            Ext => {
                let (pos, size) = (i.sa(), rd as u32 + 1);
                if pos + size > 32 {
                    return Err(Trap::Undefined);
                }
                Out::Reg(rt, (s >> pos) & mask(size))
            }
            Ins => {
                let (pos, msb) = (i.sa(), rd as u32);
                if msb < pos {
                    return Err(Trap::Undefined);
                }
                let m = mask(msb - pos + 1) << pos;
                Out::Reg(rt, (t & !m) | ((s << pos) & m))
            }
            Seb => Out::Reg(rd, t as u8 as i8 as i32 as u32),
            Seh => Out::Reg(rd, t as u16 as i16 as i32 as u32),
            Invalid => return Err(Trap::Invalid { word: i.0 }),
        };
        Ok(out)
    }
}

const fn mask(bits: u32) -> u32 {
    if bits >= 32 { u32::MAX } else { (1 << bits) - 1 }
}

fn load(bus: &mut impl Bus, addr: u32, size: Size) -> Result<u32, Trap> {
    if addr & (size as u32 - 1) != 0 {
        return Err(Trap::Unaligned { addr });
    }
    bus.load(addr, size)
}

fn store(bus: &mut impl Bus, addr: u32, size: Size, value: u32) -> Result<Out, Trap> {
    if addr & (size as u32 - 1) != 0 {
        return Err(Trap::Unaligned { addr });
    }
    bus.store(addr, size, value)?;
    Ok(Out::None)
}
