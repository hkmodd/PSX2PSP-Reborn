//! Static recompilation of PS1 games for the PSP (`docs/recomp/PLAN.md`).
//!
//! The translator reads the game's PS-X executables from the user's disc, discovers their code
//! and emits Allegrex machine code for the native runtime (`psp/psxrt`). This crate holds the
//! pieces that need no PSP: instruction decoding for both CPUs, the executable loader and an
//! interpreter that runs either side, used as the oracle of the differential tests.

pub mod exe;
pub mod insn;
pub mod interp;

pub use exe::{ExeError, PsxExe};
pub use insn::{Insn, Isa, Kind, Uses, decode};
pub use interp::{Bus, Cpu, Size, Stop, Trap};
