//! External tool adapters (FFmpeg, ATRAC3 encoders, yt-dlp), invoked as child processes.
//!
//! FFmpeg 8.x has **no ATRAC3 encoder** (decoders only) and no PMF muxer, so SND0.AT3 needs a
//! dedicated encoder: Sony `at3tool` or the open-source `atracdenc`. FFmpeg normalises any input
//! to 44.1 kHz stereo PCM first.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use psx2psp_core::Monitor;

use crate::MediaError;
use crate::proc::run_watched;

/// Max combined size of ICON1.PMF + SND0.AT3 accepted by the XMB.
pub const SND0_BUDGET: u64 = 500 * 1024;

pub(crate) fn command(program: &Path) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Runs a tool to completion: no cancellation, but the process tree is still contained.
pub(crate) fn run(program: &Path, args: &[&OsStr]) -> Result<Output, MediaError> {
    run_watched(program, args, &(), &mut |_| {}, None)
}

/// Calls `progress(0..=1)` as FFmpeg reports its position within a `secs`-long output.
pub(crate) fn ffmpeg_progress(secs: f64, progress: &dyn Fn(f64)) -> impl FnMut(&str) + '_ {
    move |l: &str| {
        if let Some(t) = crate::proc::progress_secs(l) {
            progress((t / secs.max(0.001)).clamp(0.0, 1.0));
        }
    }
}

pub(crate) fn exe_name(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// `<our executable's dir>/tools`: where [`crate::provision`] installs missing tools.
pub fn tools_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("tools")))
}

/// Every existing `name` executable, in priority order: next to our binary, in `tools/`, on PATH.
pub fn candidates(name: &str) -> Vec<PathBuf> {
    let exe = exe_name(name);
    let mut dirs: Vec<PathBuf> = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).into_iter().collect();
    dirs.extend(tools_dir());
    dirs.extend(std::env::var_os("PATH").iter().flat_map(std::env::split_paths));
    let mut out: Vec<PathBuf> = Vec::new();
    for p in dirs.into_iter().map(|d| d.join(&exe)) {
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// First [`candidates`] match.
pub fn which(name: &str) -> Option<PathBuf> {
    candidates(name).into_iter().next()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum At3Encoder {
    /// Sony `at3tool`: `at3tool -e -br <kbps> in.wav out.at3`.
    At3Tool(PathBuf),
    /// `atracdenc`: `atracdenc -e atrac3 -i in.wav -o out.at3 --bitrate <kbps>`.
    Atracdenc(PathBuf),
}

impl At3Encoder {
    pub fn detect() -> Option<Self> {
        which("at3tool").map(Self::At3Tool).or_else(|| which("atracdenc").map(Self::Atracdenc))
    }

    pub fn from_path(p: PathBuf) -> Self {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_ascii_lowercase();
        if stem.contains("atracdenc") { Self::Atracdenc(p) } else { Self::At3Tool(p) }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::At3Tool(p) | Self::Atracdenc(p) => p,
        }
    }

    /// Encodes `wav` to a RIFF ATRAC3 file at one of the Sony rates (66/105/132 kbps).
    ///
    /// atracdenc quirks, measured on 0.2.3: it picks the container from the output extension
    /// case-sensitively (`.AT3` → EA3/OMA, `.at3` → RIFF), and maps `--bitrate` through its own
    /// thresholds (66 → 272-byte frames, 132 → 424: non-Sony sizes). `--bitrate 64/100/128`
    /// are the arguments that yield Sony's 192/304/384-byte frames.
    pub fn encode(&self, wav: &Path, out: &Path, kbps: u32, mon: &dyn Monitor) -> Result<(), MediaError> {
        let bad = || MediaError::Tool { tool: self.path().display().to_string(), detail: format!("{kbps} kbps is not an ATRAC3 rate (66, 105, 132)") };
        at3_block_align(kbps).ok_or_else(bad)?;
        let br = match self {
            Self::At3Tool(_) => kbps,
            Self::Atracdenc(_) => match kbps {
                66 => 64,
                105 => 100,
                _ => 128,
            },
        }
        .to_string();
        let (p, args): (&Path, Vec<&OsStr>) = match self {
            Self::At3Tool(p) => (p, vec!["-e".as_ref(), "-br".as_ref(), br.as_ref(), wav.as_os_str(), out.as_os_str()]),
            Self::Atracdenc(p) => (
                p,
                vec!["-e".as_ref(), "atrac3".as_ref(), "-i".as_ref(), wav.as_os_str(), "-o".as_ref(), out.as_os_str(), "--bitrate".as_ref(), br.as_ref()],
            ),
        };
        run_watched(p, &args, mon, &mut |_| {}, None)?;
        Ok(())
    }
}

/// Sony ATRAC3 frame size (stereo, 1024 samples @ 44.1 kHz) for a nominal rate — the only
/// three the PSP decodes: 66 kbps joint-stereo (192 B), 105 kbps (304 B), 132 kbps (384 B).
pub const fn at3_block_align(kbps: u32) -> Option<u16> {
    match kbps {
        66 => Some(192),
        105 => Some(304),
        132 => Some(384),
        _ => None,
    }
}

/// Exact bit rate of a Sony ATRAC3 frame size: `block · 44100 / 1024 · 8`.
pub fn at3_bps(block_align: u16) -> f64 {
    f64::from(block_align) * 44_100.0 * 8.0 / 1024.0
}

/// `block_align` of a RIFF/WAVE file whose fmt chunk is ATRAC3 (wFormatTag 0x0270).
pub fn riff_atrac3_block_align(b: &[u8]) -> Option<u16> {
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return None;
    }
    let mut p = 12usize;
    while p.saturating_add(8) <= b.len() {
        let len = u32::from_le_bytes(b[p + 4..p + 8].try_into().unwrap()) as usize;
        if &b[p..p + 4] == b"fmt " {
            let fmt = b.get(p + 8..p + 22)?;
            return (fmt[..2] == [0x70, 0x02]).then(|| u16::from_le_bytes([fmt[12], fmt[13]]));
        }
        p = p.saturating_add(8).saturating_add(len).saturating_add(len & 1);
    }
    None
}

/// Checks a RIFF/WAVE container carries ATRAC3 (wFormatTag 0x0270).
pub fn is_riff_atrac3(b: &[u8]) -> bool {
    riff_atrac3_block_align(b).is_some()
}

/// True when the file is RIFF ATRAC3 at a frame size the PSP can play (192/304/384).
pub fn is_psp_atrac3(b: &[u8]) -> bool {
    riff_atrac3_block_align(b).is_some_and(|a| [66, 105, 132].iter().any(|&k| at3_block_align(k) == Some(a)))
}

#[derive(Debug, Clone)]
pub struct Ffmpeg {
    pub path: PathBuf,
}

impl Ffmpeg {
    /// The first FFmpeg that can encode H.264 (needed for ICON1.PMF), else the first one found.
    pub fn detect() -> Option<Self> {
        let all: Vec<Self> = candidates("ffmpeg").into_iter().map(|path| Self { path }).collect();
        all.iter().position(Self::has_x264).map_or_else(|| all.first().cloned(), |i| Some(all[i].clone()))
    }

    /// Whether this build includes libx264 (LGPL-only builds don't).
    pub fn has_x264(&self) -> bool {
        command(&self.path)
            .args(["-hide_banner", "-encoders"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libx264"))
    }

    pub(crate) fn run(&self, args: &[&OsStr]) -> Result<Output, MediaError> {
        run(&self.path, args)
    }

    /// Like [`Self::run`], cancellable through `mon`. Each stderr line goes to `on_line`.
    pub(crate) fn run_watched(&self, args: &[&OsStr], mon: &dyn Monitor, on_line: &mut dyn FnMut(&str)) -> Result<Output, MediaError> {
        run_watched(&self.path, args, mon, on_line, None)
    }

    /// Media duration in seconds, parsed from `ffmpeg -i` (no ffprobe needed).
    pub fn duration(&self, input: &Path) -> Result<f64, MediaError> {
        let out = command(&self.path)
            .args(["-hide_banner".as_ref(), "-i".as_ref(), input.as_os_str()])
            .output()
            .map_err(|e| MediaError::Io(self.path.display().to_string(), e))?;
        let err = String::from_utf8_lossy(&out.stderr);
        err.lines()
            .find_map(|l| l.trim().strip_prefix("Duration: ").and_then(|r| parse_hms(r.split(',').next()?)))
            .ok_or_else(|| MediaError::Tool { tool: "ffmpeg".into(), detail: format!("no duration for {}", input.display()) })
    }

    /// Decodes any audio (or video's audio track) to 44.1 kHz stereo s16 WAV, trimmed to `max_secs`,
    /// with a short fade-out so the loop point doesn't click.
    pub fn to_wav(&self, input: &Path, out: &Path, start_secs: f64, max_secs: f64, mon: &dyn Monitor) -> Result<(), MediaError> {
        let fade_start = (max_secs - 1.0).max(0.0);
        let ss = format!("{start_secs:.3}");
        let t = format!("{max_secs:.3}");
        let af = format!("afade=t=out:st={fade_start:.3}:d=1");
        self.run_watched(
            &[
                "-hide_banner".as_ref(), "-loglevel".as_ref(), "error".as_ref(), "-nostdin".as_ref(), "-y".as_ref(),
                "-ss".as_ref(), ss.as_ref(), "-t".as_ref(), t.as_ref(), "-i".as_ref(), input.as_os_str(),
                "-vn".as_ref(), "-ac".as_ref(), "2".as_ref(), "-ar".as_ref(), "44100".as_ref(),
                "-sample_fmt".as_ref(), "s16".as_ref(), "-af".as_ref(), af.as_ref(), out.as_os_str(),
            ],
            mon,
            &mut |_| {},
        )?;
        Ok(())
    }
}

/// `HH:MM:SS.ss`, `MM:SS` or plain seconds → seconds.
pub fn parse_hms(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut secs = 0.0;
    for part in s.split(':') {
        let v: f64 = part.trim().parse().ok()?;
        if !v.is_finite() || v < 0.0 {
            return None;
        }
        secs = secs * 60.0 + v;
    }
    Some(secs)
}

/// Seconds → `M:SS.s` for display.
pub fn fmt_hms(secs: f64) -> String {
    let t = secs.max(0.0);
    format!("{}:{:04.1}", (t / 60.0) as u64, t % 60.0)
}

/// Title and duration of an online video, without downloading it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteInfo {
    pub title: String,
    pub duration: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct YtDlp {
    pub path: PathBuf,
}

impl YtDlp {
    pub fn detect() -> Option<Self> {
        which("yt-dlp").map(|path| Self { path })
    }

    /// `--js-runtimes` for the first JavaScript runtime found (next to us, in `tools/`, on PATH).
    /// Without one, yt-dlp cannot solve YouTube's player challenges: it sees fewer formats, and
    /// the DASH ones it does get are throttled to a crawl.
    pub fn js_runtime() -> Option<(&'static str, PathBuf)> {
        ["deno", "node", "bun", "qjs"].into_iter().find_map(|n| which(n).map(|p| (if n == "qjs" { "quickjs" } else { n }, p)))
    }

    fn base_args(js: &Option<(&'static str, PathBuf)>) -> Vec<std::ffi::OsString> {
        let mut a: Vec<std::ffi::OsString> = vec!["--no-playlist".into(), "--ignore-config".into(), "--socket-timeout".into(), "20".into()];
        if let Some((name, path)) = js {
            let mut spec = std::ffi::OsString::from(format!("{name}:"));
            spec.push(path);
            a.extend(["--js-runtimes".into(), spec]);
        }
        a
    }

    pub fn info(&self, url: &str) -> Result<RemoteInfo, MediaError> {
        let mut args = Self::base_args(&Self::js_runtime());
        args.extend(["--no-warnings".into(), "--print".into(), "%(duration)s|%(title)s".into(), url.into()]);
        let args: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        let out = run_watched(&self.path, &args, &(), &mut |_| {}, Some(YTDLP_IDLE))?;
        let line = String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or_default().trim().to_string();
        let (d, t) = line.split_once('|').unwrap_or(("NA", &line));
        Ok(RemoteInfo { title: t.to_string(), duration: d.parse().ok() })
    }

    /// Downloads `[start, end)` of a single video into `dir` (audio only when `video` is false),
    /// cut on exact frames by FFmpeg. Returns the produced file.
    ///
    /// `progress(0..=1)` follows the downloaded span. yt-dlp's notes go to `mon.log`, and
    /// cancelling through `mon` stops yt-dlp and its FFmpeg at once.
    pub fn fetch_section(
        &self,
        ffmpeg: &Ffmpeg,
        req: &Section<'_>,
        dir: &Path,
        mon: &dyn Monitor,
        progress: &dyn Fn(f64),
    ) -> Result<PathBuf, MediaError> {
        let template = dir.join("clip_src.%(ext)s");
        let section = format!("*{:.3}-{:.3}", req.start, req.end);
        // HLS first: FFmpeg seeks an HLS playlist by segment, so only the requested seconds are
        // fetched, and these streams are not throttled. ≤360p is plenty for a 144×80 icon.
        let format = if req.video {
            "bv*[height<=360][protocol^=m3u8]+ba[protocol^=m3u8]/b[height<=480][protocol^=m3u8]/bv*[height<=480]+ba/b[height<=480]/bv*+ba/b"
        } else {
            "ba[protocol^=m3u8]/ba/b"
        };
        let js = Self::js_runtime();
        match &js {
            Some((name, _)) => mon.log(&format!("yt-dlp: JavaScript runtime {name}")),
            None => mon.log("yt-dlp: no JavaScript runtime (deno/node/bun) found; YouTube may offer fewer formats"),
        }
        let mut args = Self::base_args(&js);
        args.extend(
            [
                "--no-part", "--no-mtime", "--newline", "-f", format, "--download-sections", &section, "--force-keyframes-at-cuts",
                "--print", "after_move:filepath",
            ]
            .map(std::ffi::OsString::from),
        );
        args.extend(["--ffmpeg-location".into(), ffmpeg.path.clone().into_os_string(), "-o".into(), template.into_os_string(), req.url.into()]);
        let args: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        let span = req.end - req.start;
        let mut on_line = |l: &str| {
            if let Some(t) = crate::proc::progress_secs(l) {
                progress((t / span.max(0.001)).clamp(0.0, 1.0));
            } else if l.starts_with("WARNING") || l.starts_with("ERROR") || l.starts_with("[info]") || l.starts_with("[youtube]") {
                mon.log(&format!("yt-dlp: {l}"));
            }
        };
        let out = run_watched(&self.path, &args, mon, &mut on_line, Some(YTDLP_IDLE))?;
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or_default().trim());
        if p.is_file() { Ok(p) } else { Err(MediaError::Tool { tool: "yt-dlp".into(), detail: "no output file reported".into() }) }
    }
}

/// yt-dlp is stopped when it prints nothing for this long (a stalled connection, a hung extractor).
const YTDLP_IDLE: std::time::Duration = std::time::Duration::from_secs(90);

/// A span of an online video.
#[derive(Debug, Clone)]
pub struct Section<'a> {
    pub url: &'a str,
    pub start: f64,
    pub end: f64,
    /// Video + audio when true, audio only otherwise.
    pub video: bool,
}

#[derive(Debug, Clone)]
pub struct Snd0Request<'a> {
    pub input: &'a Path,
    pub start_secs: f64,
    /// Clip length; `None` = as much as the budget allows (≤ 60 s).
    pub max_secs: Option<f64>,
    /// ATRAC3 bitrate: 66, 105 or 132 kbps.
    pub kbps: u32,
    /// Bytes left in the 500 KB budget (minus ICON1.PMF).
    pub budget: u64,
}

/// Full SND0.AT3 pipeline: FFmpeg → WAV → ATRAC3, sized to fit the XMB budget.
pub fn make_snd0(ffmpeg: &Ffmpeg, enc: &At3Encoder, req: &Snd0Request<'_>, work: &Path, mon: &dyn Monitor) -> Result<Vec<u8>, MediaError> {
    let tool_err = |detail: String| MediaError::Tool { tool: enc.path().display().to_string(), detail };
    let block = at3_block_align(req.kbps).ok_or_else(|| tool_err(format!("{} kbps is not an ATRAC3 rate (66, 105, 132)", req.kbps)))?;
    // ATRAC3 is CBR: size = header + frames. Reserve 512 B for RIFF chunks and 3 frames for the
    // encoder's delay/padding (measured overhead: ~430 B at 132 kbps), the rest is audio.
    let overhead = 512 + 3 * u64::from(block);
    let fit = (req.budget.saturating_sub(overhead) as f64 * 8.0 / at3_bps(block)).clamp(1.0, 60.0);
    let max_secs = req.max_secs.map_or(fit, |s| s.clamp(1.0, fit));
    let wav = work.join("snd0.wav");
    // Lower-case extension: atracdenc writes EA3/OMA instead of RIFF for `.AT3`.
    let at3 = work.join("snd0.at3");
    ffmpeg.to_wav(req.input, &wav, req.start_secs, max_secs, mon)?;
    enc.encode(&wav, &at3, req.kbps, mon)?;
    let bytes = std::fs::read(&at3).map_err(|e| MediaError::Io(at3.display().to_string(), e))?;
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::remove_file(&at3);
    match riff_atrac3_block_align(&bytes) {
        None => {
            let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4)]).into_owned();
            return Err(tool_err(format!("output is not RIFF/ATRAC3 (starts with {head:?})")));
        }
        Some(a) if a != block => return Err(tool_err(format!("wrote {a}-byte ATRAC3 frames, {} kbps needs {block} (not playable on PSP)", req.kbps))),
        Some(_) => {}
    }
    if bytes.len() as u64 > req.budget {
        return Err(MediaError::Budget { size: bytes.len() as u64, budget: req.budget });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn riff_atrac3_detection() {
        let mut b = b"RIFF\0\0\0\0WAVEfmt \x20\0\0\0".to_vec();
        b.extend([0x70, 0x02, 2, 0]);
        b.resize(64, 0);
        assert!(is_riff_atrac3(&b));
        b[32..34].copy_from_slice(&272u16.to_le_bytes()); // atracdenc `--bitrate 66` size
        assert_eq!(riff_atrac3_block_align(&b), Some(272));
        assert!(!is_psp_atrac3(&b));
        b[32..34].copy_from_slice(&192u16.to_le_bytes());
        assert!(is_psp_atrac3(&b));
        b[20] = 0x01; // PCM
        assert!(!is_riff_atrac3(&b));
        assert!(!is_riff_atrac3(b"nope"));
        assert!(!is_riff_atrac3(&[0x45, 0x41, 0x33, 1, 0, 0x60, 0xFF, 0xFF, 0, 0, 0, 0])); // EA3/OMA
        assert!((at3_bps(192) - 66_150.0).abs() < 1e-6);
    }

    #[test]
    fn encoder_kind_from_path() {
        assert!(matches!(At3Encoder::from_path("C:/x/atracdenc.exe".into()), At3Encoder::Atracdenc(_)));
        assert!(matches!(At3Encoder::from_path("/usr/bin/at3tool".into()), At3Encoder::At3Tool(_)));
    }

    #[test]
    fn ffmpeg_wav_if_available() {
        let Some(ff) = Ffmpeg::detect() else { return };
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("x.wav");
        // lavfi sine as input proves the argument vector is valid for this FFmpeg build.
        let out = command(&ff.path)
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "sine=f=440:d=3"])
            .arg(dir.path().join("in.wav"))
            .output()
            .unwrap();
        assert!(out.status.success());
        ff.to_wav(&dir.path().join("in.wav"), &wav, 0.0, 2.0, &()).unwrap();
        let b = std::fs::read(&wav).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert!(b.len() > 44100 * 4, "≈2 s of stereo s16");
    }

    /// Real encoder, all three rates: RIFF (not OMA) with Sony frame sizes, sized to the budget.
    #[test]
    fn make_snd0_sony_frames_if_available() {
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/tools").join(exe_name("atracdenc"));
        let Some(enc) = At3Encoder::detect().or_else(|| bundled.is_file().then_some(At3Encoder::Atracdenc(bundled))) else { return };
        let Some(ff) = Ffmpeg::detect() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let ok = command(&ff.path)
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "sine=f=440:d=4", "-ac", "2", "-ar", "44100"])
            .arg(&src)
            .status()
            .unwrap();
        assert!(ok.success());
        for kbps in [66, 105, 132] {
            let req = Snd0Request { input: &src, start_secs: 0.0, max_secs: Some(3.0), kbps, budget: SND0_BUDGET };
            let b = make_snd0(&ff, &enc, &req, dir.path(), &()).unwrap_or_else(|e| panic!("{kbps} kbps: {e}"));
            assert_eq!(riff_atrac3_block_align(&b), at3_block_align(kbps), "{kbps} kbps");
            let expect = at3_bps(at3_block_align(kbps).unwrap()) / 8.0 * 3.0;
            assert!((b.len() as f64) < expect * 1.15 && (b.len() as f64) > expect * 0.9, "{kbps} kbps: {} B", b.len());
            // What the preview plays: the AT3 bytes decoded back. ~3 s of a 440 Hz sine at the
            // lavfi default amplitude 1/8 (RMS ≈ 2900 in s16).
            let pcm = crate::clip::snd0_pcm(&ff, &b, dir.path()).unwrap_or_else(|e| panic!("{kbps} kbps decode: {e}"));
            let secs = pcm.len() as f64 / 2.0 / f64::from(crate::clip::SND0_RATE);
            assert!((2.8..3.3).contains(&secs), "{kbps} kbps: decoded {secs:.2} s");
            let mid = &pcm[pcm.len() / 4..pcm.len() * 3 / 4];
            let rms = (mid.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / mid.len() as f64).sqrt();
            assert!((2000.0..3800.0).contains(&rms), "{kbps} kbps: RMS {rms:.0}");
        }
        let tight = Snd0Request { input: &src, start_secs: 0.0, max_secs: None, kbps: 132, budget: 20_000 };
        assert!(make_snd0(&ff, &enc, &tight, dir.path(), &()).unwrap().len() <= 20_000);
    }
}
