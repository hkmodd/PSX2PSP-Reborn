//! Where translated code finds the PS1 machine in PSP memory. The runtime (`psp/psxrt`) uses the
//! same numbers.
//!
//! | Host address | Size | Content |
//! |---|---|---|
//! | [`RAM`] | 2 MB | PS1 main RAM (every KUSEG/KSEG0/KSEG1 mirror maps here) |
//! | [`SCRATCH`] | 1 KB | PS1 scratchpad |
//! | [`CTX`] | 0x100 | Guest state the code does not keep in registers ([`ctx`]) |
//! | [`RT_CALL`] | | Runtime entry for host calls (written by the runtime) |
//! | [`MISS`] | | Runtime entry for a dispatch-table miss (written by the runtime) |
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
/// Host-call entry.
pub const RT_CALL: u32 = 0x09A0_0F00;
/// Dispatch-miss entry: the guest target is in [`ctx::TARGET`].
pub const MISS: u32 = 0x09A0_0F40;
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
    /// Nonzero when an interrupt is pending; polled at loop back-edges.
    pub const IRQ: u32 = 0x90;
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
