//! Sector-level patching for raw PS1 images.
//!
//! All patches implement [`psx2psp_core::ImagePatch`] and are applied in-stream by the PSAR
//! writer, block by block, before compression — the source BIN is never written to.

pub mod exe_crc;
pub mod pattern;
pub mod ppf;
pub mod sector;

pub use pattern::{PAL_TO_NTSC_RULES, PatternPatch, PatternRule, pal_to_ntsc, pal_to_ntsc_applies};
pub use ppf::{PpfError, PpfPatch};
