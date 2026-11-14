//! MIPS instruction words: fields, decoding and register use for the two CPUs involved.
//!
//! The R3000A (PS1) and the Allegrex (PSP) share the MIPS I encodings, so most translated
//! instructions are the original words. The Allegrex adds MIPS32r2-style instructions the emitter
//! uses (`ext`, `ins`, `seb`, `seh`, `min`, `max`, `clz`, `clo`, `movz`, `movn`, branch-likely) and
//! gives coprocessor 2 to the VFPU where the R3000A has the GTE.
//!
//! Decoding is strict: a word is only recognised in its canonical form (unused fields zero).
//! Several non-canonical R3000A words mean something else on the Allegrex (`srl` with `rs` = 1 is
//! `rotr`, `srlv` with `sa` = 1 is `rotrv`), and an instruction copied as it is must mean the same
//! thing on both CPUs. Everything else is [`Kind::Invalid`]: the translator refuses it.

/// The CPU an instruction word is decoded for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Isa {
    /// PS1 CPU (MIPS I, one load delay slot, GTE as coprocessor 2).
    R3000A,
    /// PSP CPU (interlocked loads, VFPU as coprocessor 2).
    Allegrex,
}

/// One instruction word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Insn(pub u32);

impl Insn {
    pub const fn op(self) -> u32 {
        self.0 >> 26
    }
    pub const fn rs(self) -> usize {
        ((self.0 >> 21) & 31) as usize
    }
    pub const fn rt(self) -> usize {
        ((self.0 >> 16) & 31) as usize
    }
    pub const fn rd(self) -> usize {
        ((self.0 >> 11) & 31) as usize
    }
    pub const fn sa(self) -> u32 {
        (self.0 >> 6) & 31
    }
    pub const fn funct(self) -> u32 {
        self.0 & 63
    }
    /// Immediate, zero-extended.
    pub const fn imm(self) -> u32 {
        self.0 & 0xFFFF
    }
    /// Immediate, sign-extended.
    pub const fn simm(self) -> u32 {
        self.0 as u16 as i16 as i32 as u32
    }
    /// `syscall`/`break` code field.
    pub const fn code(self) -> u32 {
        (self.0 >> 6) & 0xF_FFFF
    }
    /// Target of a branch at `pc`: the delay slot plus the offset in words.
    pub const fn branch_target(self, pc: u32) -> u32 {
        pc.wrapping_add(4).wrapping_add(self.simm() << 2)
    }
    /// Target of a `j`/`jal` at `pc`, inside the 256 MB region of its delay slot.
    pub const fn jump_target(self, pc: u32) -> u32 {
        (pc.wrapping_add(4) & 0xF000_0000) | ((self.0 & 0x03FF_FFFF) << 2)
    }
}

/// What an instruction does. Operands are read from the [`Insn`] fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Sll,
    Srl,
    Sra,
    Sllv,
    Srlv,
    Srav,
    Jr,
    Jalr,
    Syscall,
    Break,
    Mfhi,
    Mthi,
    Mflo,
    Mtlo,
    Mult,
    Multu,
    Div,
    Divu,
    Add,
    Addu,
    Sub,
    Subu,
    And,
    Or,
    Xor,
    Nor,
    Slt,
    Sltu,
    Bltz,
    Bgez,
    Bltzal,
    Bgezal,
    J,
    Jal,
    Beq,
    Bne,
    Blez,
    Bgtz,
    Addi,
    Addiu,
    Slti,
    Sltiu,
    Andi,
    Ori,
    Xori,
    Lui,
    Lb,
    Lh,
    Lwl,
    Lw,
    Lbu,
    Lhu,
    Lwr,
    Sb,
    Sh,
    Swl,
    Sw,
    Swr,
    /// R3000A only: move from COP0 (`rt` <- cop0 `rd`).
    Mfc0,
    /// R3000A only: move to COP0.
    Mtc0,
    /// R3000A only: return from exception (pops the SR mode stack).
    Rfe,
    /// R3000A only: GTE data register to `rt`.
    Mfc2,
    /// R3000A only: GTE control register to `rt`.
    Cfc2,
    /// R3000A only: `rt` to GTE data register.
    Mtc2,
    /// R3000A only: `rt` to GTE control register.
    Ctc2,
    /// R3000A only: GTE command (low 25 bits).
    Gte,
    /// R3000A only: memory to GTE data register `rt`.
    Lwc2,
    /// R3000A only: GTE data register `rt` to memory.
    Swc2,
    /// Allegrex only.
    Beql,
    Bnel,
    Blezl,
    Bgtzl,
    Movz,
    Movn,
    Clz,
    Clo,
    Max,
    Min,
    /// `rt` <- `size` (= rd + 1) bits of `rs` from bit `pos` (= sa).
    Ext,
    /// Bits `pos` (= sa) ..= `msb` (= rd) of `rt` <- low bits of `rs`.
    Ins,
    Seb,
    Seh,
    Invalid,
}

impl Kind {
    /// A load from memory or a coprocessor into a GPR: on the R3000A its value is not visible to
    /// the next instruction.
    pub const fn is_delayed_load(self) -> bool {
        matches!(
            self,
            Kind::Lb
                | Kind::Lh
                | Kind::Lwl
                | Kind::Lw
                | Kind::Lbu
                | Kind::Lhu
                | Kind::Lwr
                | Kind::Mfc0
                | Kind::Mfc2
                | Kind::Cfc2
        )
    }

    /// A branch or jump: the next instruction is its delay slot.
    pub const fn has_delay_slot(self) -> bool {
        matches!(
            self,
            Kind::Jr
                | Kind::Jalr
                | Kind::Bltz
                | Kind::Bgez
                | Kind::Bltzal
                | Kind::Bgezal
                | Kind::J
                | Kind::Jal
                | Kind::Beq
                | Kind::Bne
                | Kind::Blez
                | Kind::Bgtz
                | Kind::Beql
                | Kind::Bnel
                | Kind::Blezl
                | Kind::Bgtzl
        )
    }
}

/// Decodes `i` for `isa`. Non-canonical and unknown words are [`Kind::Invalid`].
pub fn decode(i: Insn, isa: Isa) -> Kind {
    use Kind::*;
    let ps1 = isa == Isa::R3000A;
    let psp = !ps1;
    let (rs0, rt0, rd0, sa0) = (i.rs() == 0, i.rt() == 0, i.rd() == 0, i.sa() == 0);
    let low11 = i.0 & 0x7FF == 0;
    match i.op() {
        0 => match i.funct() {
            0 if rs0 => Sll,
            2 if rs0 => Srl,
            3 if rs0 => Sra,
            4 if sa0 => Sllv,
            6 if sa0 => Srlv,
            7 if sa0 => Srav,
            8 if rt0 && rd0 && sa0 => Jr,
            9 if rt0 && sa0 => Jalr,
            0x0A if psp && sa0 => Movz,
            0x0B if psp && sa0 => Movn,
            0x0C => Syscall,
            0x0D => Break,
            0x10 | 0x12 if rs0 && rt0 && sa0 => {
                if i.funct() == 0x10 {
                    Mfhi
                } else {
                    Mflo
                }
            }
            0x11 | 0x13 if rt0 && rd0 && sa0 => {
                if i.funct() == 0x11 {
                    Mthi
                } else {
                    Mtlo
                }
            }
            0x16 if psp && rt0 && sa0 => Clz,
            0x17 if psp && rt0 && sa0 => Clo,
            0x18 if rd0 && sa0 => Mult,
            0x19 if rd0 && sa0 => Multu,
            0x1A if rd0 && sa0 => Div,
            0x1B if rd0 && sa0 => Divu,
            0x20 if sa0 => Add,
            0x21 if sa0 => Addu,
            0x22 if sa0 => Sub,
            0x23 if sa0 => Subu,
            0x24 if sa0 => And,
            0x25 if sa0 => Or,
            0x26 if sa0 => Xor,
            0x27 if sa0 => Nor,
            0x2A if sa0 => Slt,
            0x2B if sa0 => Sltu,
            0x2C if psp && sa0 => Max,
            0x2D if psp && sa0 => Min,
            _ => Invalid,
        },
        1 => match i.rt() {
            0 => Bltz,
            1 => Bgez,
            0x10 => Bltzal,
            0x11 => Bgezal,
            _ => Invalid,
        },
        2 => J,
        3 => Jal,
        4 => Beq,
        5 => Bne,
        6 if rt0 => Blez,
        7 if rt0 => Bgtz,
        8 => Addi,
        9 => Addiu,
        0x0A => Slti,
        0x0B => Sltiu,
        0x0C => Andi,
        0x0D => Ori,
        0x0E => Xori,
        0x0F if rs0 => Lui,
        0x10 if ps1 => match i.rs() {
            0 if low11 => Mfc0,
            4 if low11 => Mtc0,
            0x10 if i.0 & 0x01FF_FFFF == 0x10 => Rfe,
            _ => Invalid,
        },
        0x12 if ps1 => match i.rs() {
            0 if low11 => Mfc2,
            2 if low11 => Cfc2,
            4 if low11 => Mtc2,
            6 if low11 => Ctc2,
            r if r & 0x10 != 0 => Gte,
            _ => Invalid,
        },
        0x14 if psp => Beql,
        0x15 if psp => Bnel,
        0x16 if psp && rt0 => Blezl,
        0x17 if psp && rt0 => Bgtzl,
        0x1F if psp => match i.funct() {
            0 => Ext,
            4 => Ins,
            0x20 if rs0 && i.sa() == 0x10 => Seb,
            0x20 if rs0 && i.sa() == 0x18 => Seh,
            _ => Invalid,
        },
        0x20 => Lb,
        0x21 => Lh,
        0x22 => Lwl,
        0x23 => Lw,
        0x24 => Lbu,
        0x25 => Lhu,
        0x26 => Lwr,
        0x28 => Sb,
        0x29 => Sh,
        0x2A => Swl,
        0x2B => Sw,
        0x2E => Swr,
        0x32 if ps1 => Lwc2,
        0x3A if ps1 => Swc2,
        _ => Invalid,
    }
}

/// General-purpose registers an instruction reads and writes, as bit masks (bit n = register n;
/// register 0 is never included). HI/LO and coprocessor registers are not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Uses {
    pub read: u32,
    pub write: u32,
}

impl Uses {
    /// The register uses of `i` decoded as `k`.
    pub fn of(i: Insn, k: Kind) -> Self {
        use Kind::*;
        let b = |r: usize| 1u32 << r;
        let (s, t, d) = (b(i.rs()), b(i.rt()), b(i.rd()));
        let (read, write) = match k {
            Sll | Srl | Sra | Seb | Seh => (t, d),
            Sllv | Srlv | Srav | Add | Addu | Sub | Subu | And | Or | Xor | Nor | Slt | Sltu
            | Max | Min => (s | t, d),
            Movz | Movn => (s | t | d, d),
            Clz | Clo => (s, d),
            Jr => (s, 0),
            Jalr => (s, d),
            Mfhi | Mflo => (0, d),
            Mthi | Mtlo => (s, 0),
            Mult | Multu | Div | Divu => (s | t, 0),
            Bltz | Bgez | Blez | Bgtz | Blezl | Bgtzl => (s, 0),
            Bltzal | Bgezal => (s, b(31)),
            Jal => (0, b(31)),
            Beq | Bne | Beql | Bnel => (s | t, 0),
            Addi | Addiu | Slti | Sltiu | Andi | Ori | Xori | Ext => (s, t),
            Ins => (s | t, t),
            Lui | Mfc0 | Mfc2 | Cfc2 => (0, t),
            Mtc0 | Mtc2 | Ctc2 => (t, 0),
            Lb | Lh | Lw | Lbu | Lhu => (s, t),
            Lwl | Lwr => (s | t, t),
            Sb | Sh | Swl | Sw | Swr => (s | t, 0),
            Lwc2 | Swc2 => (s, 0),
            J | Syscall | Break | Rfe | Gte | Invalid => (0, 0),
        };
        Uses { read: read & !1, write: write & !1 }
    }
}
