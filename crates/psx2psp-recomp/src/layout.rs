//! Where translated code finds the PS1 machine in PSP memory. The runtime (`psp/psxrt`) uses the
//! same numbers.
//!
//! | Host address | Size | Content |
//! |---|---|---|
//! | [`RAM`] | 2 MB | PS1 main RAM (every KUSEG/KSEG0/KSEG1 mirror maps here) |
//! | [`SCRATCH`] | 1 KB | PS1 scratchpad |
//! | [`CTX`] | 0x100 | Guest state the code does not keep in registers ([`ctx`]) |
//! | [`GTE`] | 0x100 | GTE registers: 32 data words, then 32 control words ([`gte`]) |
//! | [`RT_CALL`] | | Runtime entry for host calls (written by the runtime) |
//! | [`IO_READ`] | | Runtime entry for a device read (written by the runtime) |
//! | [`IO_WRITE`] | | Runtime entry for a device write (written by the runtime) |
//! | [`MISS`] | | Runtime entry for a dispatch-table miss (written by the runtime) |
//! | [`GTE_CMD`] | | Runtime entry for a GTE command (written by the runtime) |
//! | [`GTE_NCLIP`] .. [`GTE_RTPT`] | | Runtime entries for the commands of [`GTE_OWN`] |
//! | [`GTE_MVMVA`], [`GTE_GPF`], [`GTE_DPCS`], [`GTE_SQR`] | | Runtime entries for the commands of [`GTE_WORD`] |
//! | [`TABLE`] | 2 MB | Dispatch table: host address of the code for guest `addr & 0x1FFFFF` |
//!
//! Allegrex `k0` holds [`K0`] at all times: it is the base for [`CTX`] and [`TABLE`], and its
//! low 11 bits are the top 11 bits of [`RAM`], so `ins k1,k0,21,11` turns a guest address in
//! `k1` into its host address. Allegrex `k1` is the translator's scratch register. The guest's
//! own `k0`/`k1` live in [`ctx::GPR`].

/// PS1 main RAM.
pub const RAM: u32 = 0x0980_0000;
/// PS1 scratchpad.
pub const SCRATCH: u32 = 0x09A0_0000;
/// Guest context.
pub const CTX: u32 = 0x09A0_0400;
/// GTE registers, as the runtime's GTE keeps them.
pub const GTE: u32 = 0x09A0_0500;
/// Host-call entry.
pub const RT_CALL: u32 = 0x09A0_0F00;
/// Device-read entry, for a plain load (`lb`/`lbu`/`lh`/`lhu`/`lw`; `rt` not 0, neither `rt`
/// nor `rs` `ra`) whose address is neither RAM nor the scratchpad: `sw ra,GPR31(k0); jal
/// IO_READ; addiu k1,rs,off; .word pc; .word insn; j back; lw rt,GPR(rt)(k0)`. The runtime
/// serves the read with the guest address in `k1`, or runs it as a host call (an interrupt
/// taken first comes back to the first word); either way the loaded value is in [`ctx::GPR`]
/// and every other guest register as it was when it returns past the two words.
pub const IO_READ: u32 = 0x09A0_0F08;
/// Device-write entry, for a plain store (`sb`/`sh`/`sw`; `rs` not `ra`) whose address is neither
/// RAM nor the scratchpad: `sw rt,GPR(rt)(k0)` (unless `rt` is 0 or `ra`); `sw ra,GPR31(k0); jal
/// IO_WRITE; addiu k1,rs,off; .word pc; .word insn; j back; nop`. The runtime serves the write
/// with the guest address in `k1` and the value in [`ctx::GPR`], or runs it as a host call (an
/// interrupt taken first comes back to the `sw ra`); either way every guest register is as it
/// was when it returns past the two words.
pub const IO_WRITE: u32 = 0x09A0_0F10;
/// Dispatch-miss entry: the guest target is in [`ctx::TARGET`].
pub const MISS: u32 = 0x09A0_0F40;
/// GTE command entry: `sw ra,GPR31(k0); lui k1,hi; jal GTE_CMD; ori k1,k1,lo` with the
/// command's low 25 bits in `k1`. Runs the command on [`GTE`] and returns with every guest
/// register (`ra` from [`ctx::GPR`]) as it was; `k1` is clobbered.
pub const GTE_CMD: u32 = 0x09A0_0FA0;
/// NCLIP entry: `sw ra,GPR31(k0); jal GTE_NCLIP; nop`. Same contract as [`GTE_CMD`], for the
/// words [`GTE_OWN`] gives it, without the runtime's C.
pub const GTE_NCLIP: u32 = 0x09A0_0FD0;
/// AVSZ3 entry, as [`GTE_NCLIP`].
pub const GTE_AVSZ3: u32 = 0x09A0_0FD8;
/// AVSZ4 entry, as [`GTE_NCLIP`].
pub const GTE_AVSZ4: u32 = 0x09A0_0FE0;
/// RTPS entry (sf = 1, lm = 0), as [`GTE_NCLIP`].
pub const GTE_RTPS: u32 = 0x09A0_0FE8;
/// RTPT entry (sf = 1, lm = 0), as [`GTE_NCLIP`].
pub const GTE_RTPT: u32 = 0x09A0_0FF0;
/// The GTE commands with an entry of their own, `(entry, mask, value)`: a command word `w` with
/// `w & mask == value` means the same as `value`, and goes to `entry`.
pub const GTE_OWN: [(u32, u32, u32); 5] = [
    (GTE_NCLIP, 0x3F, 0x06),
    (GTE_AVSZ3, 0x3F, 0x2D),
    (GTE_AVSZ4, 0x3F, 0x2E),
    (GTE_RTPS, 0x8_043F, 0x8_0001),
    (GTE_RTPT, 0x8_043F, 0x8_0030),
];
/// MVMVA entry: called as [`GTE_CMD`], with the word in `k1`, which it reads for its fields.
/// Words it has no fast path for go on to [`GTE_CMD`] from the runtime.
pub const GTE_MVMVA: u32 = 0x09A0_0FC8;
/// GPF entry, as [`GTE_MVMVA`].
pub const GTE_GPF: u32 = 0x09A0_0FF8;
/// DPCS entry, as [`GTE_MVMVA`].
pub const GTE_DPCS: u32 = 0x09A0_0FA8;
/// SQR entry, as [`GTE_MVMVA`].
pub const GTE_SQR: u32 = 0x09A0_0FB0;
/// The GTE commands with an entry that reads their word, `(entry, mask, value)`: a command word
/// `w` with `w & mask == value` goes to `entry` the way other words go to [`GTE_CMD`].
pub const GTE_WORD: [(u32, u32, u32); 4] = [
    (GTE_MVMVA, 0x3F, 0x12),
    (GTE_GPF, 0x3F, 0x3D),
    (GTE_DPCS, 0x3F, 0x10),
    (GTE_SQR, 0x3F, 0x28),
];
/// Dispatch table, one word per guest word address of RAM.
pub const TABLE: u32 = 0x09A0_1000;
/// End of everything above (exclusive).
pub const END: u32 = TABLE + 0x20_0000;
/// Value of Allegrex `k0`.
pub const K0: u32 = 0x09A0_004C;

const _: () = assert!((K0 & 0x7FF) << 21 == RAM);

/// Offsets inside [`CTX`].
pub mod ctx {
    /// 32 guest GPRs. Filled by the host-call trampoline; `k0`/`k1` (26, 27) always live here.
    pub const GPR: u32 = 0x00;
    pub const HI: u32 = 0x80;
    pub const LO: u32 = 0x84;
    /// Guest address of the last register jump or call.
    pub const TARGET: u32 = 0x88;
    /// Host address held across a delay slot that needs `k1`.
    pub const JUMP: u32 = 0x8C;
    /// PS1 cycles left (signed) before translated code enters the runtime: each loop back-edge
    /// subtracts its cost and makes a host call when the result is negative. The runtime refills
    /// it, and sets it negative to have the next back-edge serve an interrupt.
    pub const BUDGET: u32 = 0x90;
    /// Host return address held by the [`super::GTE_CMD`] entry.
    pub const GTE_RET: u32 = 0x94;
}

/// Offsets inside [`GTE`]: each register is a full word, as stored after the write rules
/// (sign or zero extension) the hardware applies.
pub mod gte {
    /// Data register `r`.
    pub const fn data(r: u32) -> u32 {
        r * 4
    }
    /// Control register `r`.
    pub const fn ctrl(r: u32) -> u32 {
        0x80 + r * 4
    }
}

/// `addr` as a 16-bit offset from `k0`.
pub const fn k0_off(addr: u32) -> u16 {
    let d = addr.wrapping_sub(K0) as i32;
    assert!(d >= i16::MIN as i32 && d <= i16::MAX as i32);
    d as u16
}

/// Host address of the dispatch-table entry for guest `addr`.
pub const fn table_entry(addr: u32) -> u32 {
    TABLE + (addr & 0x1F_FFFC)
}
