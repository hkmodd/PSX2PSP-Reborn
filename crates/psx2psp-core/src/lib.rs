//! # psx2psp-core
//!
//! Domain layer of PSX2PSP-Reborn: pure, dependency-light model of PS1 Classic (POPS) EBOOTs.
//!
//! * [`sfo`]  — PARAM.SFO parse/write (lossless round-trip)
//! * [`pbp`]  — EBOOT.PBP container
//! * [`disc`] — BIN/CUE images, virtual pregaps, ISO9660 game-ID probe
//! * [`psar`] — PSISOIMG / PSTITLEIMG streaming writer + extractor, TOC, donor blobs

pub mod disc;
pub mod error;
pub mod pbp;
pub mod psar;
pub mod sfo;

pub use disc::{DiscImage, DiscInfo, IsoFile, Region, Track, TrackKind};
pub use error::{Error, Result};
pub use pbp::{PbpFile, PbpHeader, PbpSections, Section};
pub use psar::{DiscSpec, Donor, PsarJob, PsarStats};
pub use sfo::{Sfo, SfoValue};

/// A transformation applied to the raw image *in stream*, block by block, before compression.
///
/// `offset` is the absolute byte offset of `block` in the raw (2352 B/sector) image; blocks are
/// always sector-aligned and a multiple of 2352 bytes long.
pub trait ImagePatch: Send + Sync {
    fn apply(&self, offset: u64, block: &mut [u8]);
    fn describe(&self) -> String;
}

/// Progress / cancellation sink for long-running operations. All methods are called from worker
/// threads, so implementations must be cheap and thread-safe.
pub trait Monitor: Sync {
    fn progress(&self, _done: u64, _total: u64) {}
    fn log(&self, _message: &str) {}
    fn cancelled(&self) -> bool {
        false
    }
}

impl Monitor for () {}

/// A bare cancel flag: no progress, no log.
impl Monitor for std::sync::atomic::AtomicBool {
    fn cancelled(&self) -> bool {
        self.load(std::sync::atomic::Ordering::Relaxed)
    }
}
