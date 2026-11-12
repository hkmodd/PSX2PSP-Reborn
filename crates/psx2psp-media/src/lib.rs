//! # psx2psp-media
//!
//! * [`images`] — in-process XMB artwork pipeline (decode → fit → PNG), zero external deps.
//! * [`tools`]  — FFmpeg / ATRAC3 encoder / yt-dlp process adapters.
//! * [`psmf`]   — native PSMF muxer/parser (ICON1.PMF).
//! * [`clip`]   — file/URL → trimmed ICON1.PMF + SND0.AT3 pipeline.
//! * [`provision`] — one-click install of the external tools into `<exe>/tools`.

pub mod clip;
pub mod images;
pub mod provision;
pub mod psmf;
pub mod tools;

pub use images::{Slot, SlotImage};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("image: {0}")]
    Image(String),
    #[error("{0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("{tool} failed: {detail}")]
    Tool { tool: String, detail: String },
    #[error("{0} not found — install it or put it next to the executable")]
    Missing(&'static str),
    #[error("SND0.AT3 is {size} bytes, over the {budget}-byte XMB budget")]
    Budget { size: u64, budget: u64 },
    #[error("format: {0}")]
    Format(String),
}
