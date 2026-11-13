//! Clip pipeline: a local file or a YouTube (any yt-dlp) URL, trimmed to `[start, end)`, becomes an
//! animated `ICON1.PMF` (144×80 H.264 in a native PSMF), a `SND0.AT3`, or both.
//!
//! ```text
//! URL ──yt-dlp (section only, frame-exact cut)──┐
//! file ─────────────────────────────────────────┴─► FFmpeg ─┬─► WAV ─► ATRAC3 ─► SND0.AT3
//!                                                           └─► x264 ES ─► psmf::mux ─► ICON1.PMF
//! ```
//!
//! ICON1 + SND0 share the XMB's 500 KiB budget: SND0 is CBR so it is made first, and ICON1's bitrate
//! is fitted to what is left (re-encoding at a lower rate until the muxed file fits).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use psx2psp_core::Monitor;

use crate::MediaError;
use crate::psmf;
use crate::tools::{self, At3Encoder, Ffmpeg, Snd0Request, YtDlp};

/// Smallest ICON1 worth making; SND0 never eats into it when both are requested.
pub const MIN_ICON1: u64 = 48 * 1024;
/// ATRAC3 rate used when SND0 shares the budget with a video.
pub const SHARED_KBPS: u32 = 66;
const MIN_VIDEO_KBPS: f64 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipMode {
    /// Video → ICON1.PMF and its soundtrack → SND0.AT3.
    #[default]
    Both,
    /// Only the animated icon.
    Icon1,
    /// Only the soundtrack, detached from the video.
    Snd0,
}

impl ClipMode {
    pub const ALL: [Self; 3] = [Self::Both, Self::Icon1, Self::Snd0];

    pub fn video(self) -> bool {
        self != Self::Snd0
    }

    pub fn audio(self) -> bool {
        self != Self::Icon1
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Both => "ICON1 + SND0",
            Self::Icon1 => "ICON1 only",
            Self::Snd0 => "SND0 only",
        }
    }
}

impl std::fmt::Display for ClipMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// External tools the pipeline drives.
#[derive(Debug, Clone)]
pub struct ClipTools {
    pub ffmpeg: Ffmpeg,
    pub at3: Option<At3Encoder>,
    pub yt_dlp: Option<YtDlp>,
}

impl ClipTools {
    pub fn detect() -> Result<Self, MediaError> {
        Ok(Self { ffmpeg: Ffmpeg::detect().ok_or(MediaError::Missing("ffmpeg"))?, at3: At3Encoder::detect(), yt_dlp: YtDlp::detect() })
    }
}

#[derive(Debug, Clone)]
pub struct ClipRequest<'a> {
    /// Local path or `http(s)://` URL.
    pub source: &'a str,
    pub start: f64,
    pub end: f64,
    pub mode: ClipMode,
    /// SND0 bitrate when it is the only output (66 / 105 / 132); [`SHARED_KBPS`] otherwise.
    pub snd0_kbps: u32,
    /// Bytes available to ICON1 + SND0.
    pub budget: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Clip {
    pub icon1: Option<Vec<u8>>,
    pub snd0: Option<Vec<u8>>,
}

pub fn is_url(s: &str) -> bool {
    let s = s.trim_start();
    s.starts_with("https://") || s.starts_with("http://")
}

/// Source length in seconds (and a title for URLs). Cancellable through `mon`.
pub fn probe(tools: &ClipTools, source: &str, mon: &dyn Monitor) -> Result<(Option<f64>, Option<String>), MediaError> {
    if is_url(source) {
        let i = tools.yt_dlp.as_ref().ok_or(MediaError::Missing("yt-dlp"))?.info(source.trim(), mon)?;
        Ok((i.duration, Some(i.title)))
    } else {
        Ok((Some(tools.ffmpeg.duration(Path::new(source), mon)?), None))
    }
}

fn bad(msg: impl Into<String>) -> MediaError {
    MediaError::Format(msg.into())
}

/// Maps one stage's own `0..=1` progress onto its slice `[lo, hi]` of the whole job.
struct Stage<'a> {
    mon: &'a dyn Monitor,
    lo: f64,
    hi: f64,
}

impl Stage<'_> {
    fn at(&self, f: f64) {
        let p = self.lo + (self.hi - self.lo) * f.clamp(0.0, 1.0);
        self.mon.progress((p * 1000.0) as u64, 1000);
    }
}

/// Makes the clip. `mon` gets log lines and whole-job progress, and can cancel at any point:
/// running tools are killed at once.
pub fn make_clip(tools: &ClipTools, req: &ClipRequest<'_>, work: &Path, mon: &dyn Monitor) -> Result<Clip, MediaError> {
    let log = |s: &str| mon.log(s);
    let dur = req.end - req.start;
    if !(req.start >= 0.0 && dur >= 1.0) {
        return Err(bad(format!("clip {}–{}: needs start ≥ 0 and at least 1 s", tools::fmt_hms(req.start), tools::fmt_hms(req.end))));
    }
    let at3 = if req.mode.audio() { Some(tools.at3.as_ref().ok_or(MediaError::Missing("ATRAC3 encoder (atracdenc/at3tool)"))?) } else { None };
    if req.mode.video() && !tools.ffmpeg.has_x264() {
        return Err(MediaError::Missing("FFmpeg build with libx264"));
    }
    std::fs::create_dir_all(work).map_err(|e| MediaError::Io(work.display().to_string(), e))?;

    // Progress split: download (URLs only), then SND0, then ICON1.
    let url = is_url(req.source);
    let dl_end = if url { 0.5 } else { 0.0 };
    let snd0_end = match (req.mode.audio(), req.mode.video()) {
        (true, true) => dl_end + (1.0 - dl_end) * 0.2,
        (true, false) => 1.0,
        _ => dl_end,
    };
    mon.progress(0, 1000);

    let (input, start, downloaded) = if url {
        let yt = tools.yt_dlp.as_ref().ok_or(MediaError::Missing("yt-dlp"))?;
        log(&format!("Downloading {}–{} of {}", tools::fmt_hms(req.start), tools::fmt_hms(req.end), req.source.trim()));
        let stage = Stage { mon, lo: 0.0, hi: dl_end };
        let section = tools::Section { url: req.source.trim(), start: req.start, end: req.end, video: req.mode.video() };
        (yt.fetch_section(&tools.ffmpeg, &section, work, mon, &|f| stage.at(f))?, 0.0, true)
    } else {
        (PathBuf::from(req.source), req.start, false)
    };

    let result = (|| {
        let mut clip = Clip::default();
        if let Some(enc) = at3 {
            let (kbps, budget) = if req.mode.video() { (SHARED_KBPS, req.budget.saturating_sub(MIN_ICON1)) } else { (req.snd0_kbps, req.budget) };
            log(&format!("SND0.AT3: ATRAC3 {kbps} kbps"));
            let snd0 = tools::make_snd0(&tools.ffmpeg, enc, &Snd0Request { input: &input, start_secs: start, max_secs: Some(dur), kbps, budget }, work, mon)?;
            log(&format!("SND0.AT3: {} bytes", snd0.len()));
            Stage { mon, lo: 0.0, hi: snd0_end }.at(1.0);
            clip.snd0 = Some(snd0);
        }
        if req.mode.video() {
            let budget = req.budget.saturating_sub(clip.snd0.as_ref().map_or(0, |s| s.len() as u64));
            let stage = Stage { mon, lo: snd0_end, hi: 1.0 };
            let pmf = encode_icon1(&tools.ffmpeg, &input, start, dur, budget, work, mon, &|f| stage.at(f))?;
            clip.icon1 = Some(pmf);
        }
        Ok(clip)
    })();
    if downloaded {
        let _ = std::fs::remove_file(&input);
    }
    result
}

/// Encodes `[start, start + dur)` of `input` to a 144×80 29.97 fps H.264 and muxes it into an
/// ICON1.PMF no larger than `budget` bytes.
#[allow(clippy::too_many_arguments)]
pub fn encode_icon1(
    ff: &Ffmpeg,
    input: &Path,
    start: f64,
    dur: f64,
    budget: u64,
    work: &Path,
    mon: &dyn Monitor,
    progress: &dyn Fn(f64),
) -> Result<Vec<u8>, MediaError> {
    let log = |s: &str| mon.log(s);
    let es = work.join("icon1.h264");
    // ~6% goes to pack headers, AU tables and the last pack's padding.
    let mut kbps = budget as f64 * 8.0 * 0.94 / dur / 1000.0;
    // Progress only moves forward: the first pass takes 90% of the bar, each retry half of what's left.
    let mut base = 0.0;
    for attempt in 0..5 {
        if kbps < MIN_VIDEO_KBPS {
            break;
        }
        let k = kbps.floor() as u32;
        log(&format!("ICON1.PMF: H.264 144×80 @ {k} kbps"));
        let (b, share) = (base, if attempt == 0 { 0.9 } else { 0.5 });
        encode_h264(ff, input, start, dur, k, &es, mon, &|f| progress(b + (1.0 - b) * share * f))?;
        base = b + (1.0 - b) * share;
        let raw = std::fs::read(&es).map_err(|e| MediaError::Io(es.display().to_string(), e))?;
        let _ = std::fs::remove_file(&es);
        let pmf = psmf::mux_icon1(&raw)?;
        if pmf.len() as u64 <= budget {
            log(&format!("ICON1.PMF: {} bytes", pmf.len()));
            progress(1.0);
            return Ok(pmf);
        }
        kbps *= budget as f64 / pmf.len() as f64 * 0.93;
    }
    Err(MediaError::Budget { size: (kbps * 1000.0 * dur / 8.0) as u64, budget })
}

#[allow(clippy::too_many_arguments)]
fn encode_h264(ff: &Ffmpeg, input: &Path, start: f64, dur: f64, kbps: u32, out: &Path, mon: &dyn Monitor, progress: &dyn Fn(f64)) -> Result<(), MediaError> {
    let (ss, t) = (format!("{start:.3}"), format!("{dur:.3}"));
    let (b, max, buf) = (format!("{kbps}k"), format!("{}k", kbps * 3 / 2), format!("{}k", kbps * 2));
    // PSP Media Engine profile: Main@2.1, no B-frames, AUD before every picture, SPS/PPS and HRD
    // SEI on every IDR, closed one-second GOPs (each becomes a PSMF group with its AU table).
    let x264 = "aud=1:repeat-headers=1:nal-hrd=vbr:bframes=0:ref=2:keyint=30:min-keyint=30:scenecut=0:force-cfr=1";
    let vf = "scale=144:80:force_original_aspect_ratio=increase:flags=lanczos,crop=144:80,setsar=1,fps=30000/1001,format=yuv420p";
    let args: &[&OsStr] = &[
        "-hide_banner".as_ref(), "-loglevel".as_ref(), "error".as_ref(), "-nostdin".as_ref(), "-y".as_ref(),
        "-nostats".as_ref(), "-progress".as_ref(), "pipe:2".as_ref(), "-stats_period".as_ref(), "0.25".as_ref(),
        "-ss".as_ref(), ss.as_ref(), "-t".as_ref(), t.as_ref(), "-i".as_ref(), input.as_os_str(),
        "-map".as_ref(), "0:v:0".as_ref(), "-an".as_ref(), "-sn".as_ref(), "-dn".as_ref(), "-vf".as_ref(), vf.as_ref(),
        "-c:v".as_ref(), "libx264".as_ref(), "-preset".as_ref(), "veryslow".as_ref(), "-profile:v".as_ref(), "main".as_ref(),
        "-level:v".as_ref(), "2.1".as_ref(), "-b:v".as_ref(), b.as_ref(), "-maxrate".as_ref(), max.as_ref(), "-bufsize".as_ref(), buf.as_ref(),
        "-x264-params".as_ref(), x264.as_ref(), "-f".as_ref(), "h264".as_ref(), out.as_os_str(),
    ];
    ff.run_watched(args, mon, &mut tools::ffmpeg_progress(dur, progress))?;
    Ok(())
}

/// Decodes an ICON1.PMF into RGBA frames (width × height × 4 bytes each) for the animated preview.
pub fn icon1_frames(ff: &Ffmpeg, pmf: &[u8], work: &Path, max_frames: usize) -> Result<(u32, u32, Vec<Vec<u8>>), MediaError> {
    let info = psmf::parse(pmf)?;
    std::fs::create_dir_all(work).map_err(|e| MediaError::Io(work.display().to_string(), e))?;
    let es = work.join("icon1_preview.h264");
    std::fs::write(&es, &info.video).map_err(|e| MediaError::Io(es.display().to_string(), e))?;
    let n = max_frames.to_string();
    let r = ff.run(&[
        "-hide_banner".as_ref(), "-loglevel".as_ref(), "error".as_ref(), "-f".as_ref(), "h264".as_ref(), "-i".as_ref(), es.as_os_str(),
        "-frames:v".as_ref(), n.as_ref(), "-f".as_ref(), "rawvideo".as_ref(), "-pix_fmt".as_ref(), "rgba".as_ref(), "-".as_ref(),
    ]);
    let _ = std::fs::remove_file(&es);
    let frame = (info.width * info.height * 4) as usize;
    let frames: Vec<Vec<u8>> = r?.stdout.chunks_exact(frame).map(<[u8]>::to_vec).collect();
    if frames.is_empty() {
        return Err(bad("ICON1.PMF decoded to zero frames"));
    }
    Ok((info.width, info.height, frames))
}

/// Sample rate of [`snd0_pcm`] output (the only rate the XMB plays SND0 at).
pub const SND0_RATE: u32 = 44_100;

/// Decodes SND0.AT3 — the exact bytes embedded in the EBOOT, not the clip source — to
/// interleaved stereo s16 at 44.1 kHz for the preview. A file FFmpeg cannot decode is an error.
pub fn snd0_pcm(ff: &Ffmpeg, at3: &[u8], work: &Path) -> Result<Vec<i16>, MediaError> {
    std::fs::create_dir_all(work).map_err(|e| MediaError::Io(work.display().to_string(), e))?;
    let f = work.join("snd0_preview.at3");
    std::fs::write(&f, at3).map_err(|e| MediaError::Io(f.display().to_string(), e))?;
    let rate = SND0_RATE.to_string();
    let r = ff.run(&[
        "-hide_banner".as_ref(), "-loglevel".as_ref(), "error".as_ref(), "-i".as_ref(), f.as_os_str(),
        "-f".as_ref(), "s16le".as_ref(), "-ac".as_ref(), "2".as_ref(), "-ar".as_ref(), rate.as_ref(), "-".as_ref(),
    ]);
    let _ = std::fs::remove_file(&f);
    let pcm: Vec<i16> = r?.stdout.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    // Under 1024 frames (one ATRAC3 frame) means the decoder produced nothing useful.
    if pcm.len() < 2 * 1024 {
        return Err(bad("SND0.AT3 decoded to no audio"));
    }
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic 6 s A/V source → ICON1 via the real FFmpeg/x264 → PSMF parse + decode. Skipped
    /// when no x264-capable FFmpeg is installed.
    #[test]
    fn icon1_from_local_video_fits_and_decodes() {
        let Some(ff) = Ffmpeg::detect().filter(Ffmpeg::has_x264) else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mkv");
        ff.run(&[
            "-hide_banner".as_ref(), "-loglevel".as_ref(), "error".as_ref(), "-f".as_ref(), "lavfi".as_ref(),
            "-i".as_ref(), "testsrc2=size=640x360:rate=25:duration=6".as_ref(), "-f".as_ref(), "lavfi".as_ref(),
            "-i".as_ref(), "sine=f=440:d=6".as_ref(), "-c:v".as_ref(), "libx264".as_ref(), "-preset".as_ref(), "ultrafast".as_ref(),
            src.as_os_str(),
        ])
        .unwrap();
        let tools = ClipTools { ffmpeg: ff.clone(), at3: None, yt_dlp: None };
        assert!((probe(&tools, src.to_str().unwrap(), &()).unwrap().0.unwrap() - 6.0).abs() < 0.1);

        let budget = 120 * 1024;
        let req = ClipRequest { source: src.to_str().unwrap(), start: 1.0, end: 5.0, mode: ClipMode::Icon1, snd0_kbps: 132, budget };
        let clip = make_clip(&tools, &req, &dir.path().join("work"), &()).unwrap();
        assert!(clip.snd0.is_none());
        let pmf = clip.icon1.unwrap();
        assert!(pmf.len() as u64 <= budget);
        let info = psmf::parse(&pmf).unwrap();
        assert_eq!((info.width, info.height), (144, 80));
        assert!((119..=121).contains(&info.frames), "4 s at 29.97 fps, got {}", info.frames);
        assert_eq!(info.table_sizes, psmf::au_sizes(&info.video));
        assert!(info.groups >= 4, "one-second GOPs");
        let (w, h, frames) = icon1_frames(&ff, &pmf, &dir.path().join("work"), 1000).unwrap();
        assert_eq!((w, h, frames.len()), (144, 80, info.frames));
    }

    #[test]
    fn clip_validation() {
        let tools = ClipTools { ffmpeg: Ffmpeg { path: "ffmpeg".into() }, at3: None, yt_dlp: None };
        let req = ClipRequest { source: "x.mp4", start: 5.0, end: 5.5, mode: ClipMode::Icon1, snd0_kbps: 132, budget: 1 };
        assert!(matches!(make_clip(&tools, &req, Path::new("."), &()), Err(MediaError::Format(_))));
        let req = ClipRequest { mode: ClipMode::Snd0, end: 30.0, ..req };
        assert!(matches!(make_clip(&tools, &req, Path::new("."), &()), Err(MediaError::Missing(_))));
        assert!(is_url(" https://youtu.be/x") && !is_url("C:/v.mp4"));
        assert_eq!(tools::parse_hms("1:02.5"), Some(62.5));
        assert_eq!(tools::parse_hms("1:00:00"), Some(3600.0));
        assert_eq!(tools::parse_hms("x"), None);
    }
}
