//! External tool adapters (FFmpeg, ATRAC3 encoders, yt-dlp), invoked as child processes.
//!
//! FFmpeg 8.x has **no ATRAC3 encoder** (decoders only) and no PMF muxer, so SND0.AT3 needs a
//! dedicated encoder: Sony `at3tool` or the open-source `atracdenc`. FFmpeg normalises any input
//! to 44.1 kHz stereo PCM first.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::MediaError;

/// Max combined size of ICON1.PMF + SND0.AT3 accepted by the XMB.
pub const SND0_BUDGET: u64 = 500 * 1024;

fn command(program: &Path) -> Command {
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

fn run(program: &Path, args: &[&OsStr]) -> Result<Output, MediaError> {
    let out = command(program)
        .args(args)
        .output()
        .map_err(|e| MediaError::Io(program.display().to_string(), e))?;
    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stderr).lines().rev().take(6).collect::<Vec<_>>().join(" | ");
        return Err(MediaError::Tool { tool: program.display().to_string(), detail: tail });
    }
    Ok(out)
}

/// Finds an executable on PATH (also next to our own binary, for portable bundles).
pub fn which(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(&exe)));
    beside
        .into_iter()
        .chain(std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).map(|d| d.join(&exe)).collect::<Vec<_>>()))
        .find(|p| p.is_file())
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

    pub fn encode(&self, wav: &Path, out: &Path, kbps: u32) -> Result<(), MediaError> {
        let br = kbps.to_string();
        match self {
            Self::At3Tool(p) => run(p, &["-e".as_ref(), "-br".as_ref(), br.as_ref(), wav.as_os_str(), out.as_os_str()]),
            Self::Atracdenc(p) => run(
                p,
                &["-e".as_ref(), "atrac3".as_ref(), "-i".as_ref(), wav.as_os_str(), "-o".as_ref(), out.as_os_str(), "--bitrate".as_ref(), br.as_ref()],
            ),
        }?;
        Ok(())
    }
}

/// Checks a RIFF/WAVE container carries ATRAC3 (wFormatTag 0x0270).
pub fn is_riff_atrac3(b: &[u8]) -> bool {
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return false;
    }
    let mut p = 12;
    while p + 8 <= b.len() {
        let id = &b[p..p + 4];
        let len = u32::from_le_bytes(b[p + 4..p + 8].try_into().unwrap()) as usize;
        if id == b"fmt " {
            return b.get(p + 8..p + 10) == Some(&[0x70, 0x02]);
        }
        p += 8 + len + (len & 1);
    }
    false
}

#[derive(Debug, Clone)]
pub struct Ffmpeg {
    pub path: PathBuf,
}

impl Ffmpeg {
    pub fn detect() -> Option<Self> {
        which("ffmpeg").map(|path| Self { path })
    }

    /// Decodes any audio (or video's audio track) to 44.1 kHz stereo s16 WAV, trimmed to `max_secs`,
    /// with a short fade-out so the loop point doesn't click.
    pub fn to_wav(&self, input: &Path, out: &Path, start_secs: f64, max_secs: f64) -> Result<(), MediaError> {
        let fade_start = (max_secs - 1.0).max(0.0);
        let ss = format!("{start_secs:.3}");
        let t = format!("{max_secs:.3}");
        let af = format!("afade=t=out:st={fade_start:.3}:d=1");
        run(
            &self.path,
            &[
                "-hide_banner".as_ref(), "-loglevel".as_ref(), "error".as_ref(), "-y".as_ref(),
                "-ss".as_ref(), ss.as_ref(), "-t".as_ref(), t.as_ref(), "-i".as_ref(), input.as_os_str(),
                "-vn".as_ref(), "-ac".as_ref(), "2".as_ref(), "-ar".as_ref(), "44100".as_ref(),
                "-sample_fmt".as_ref(), "s16".as_ref(), "-af".as_ref(), af.as_ref(), out.as_os_str(),
            ],
        )?;
        Ok(())
    }
}

/// yt-dlp: downloads the best audio stream of a single video; returns the produced file.
pub fn yt_dlp_audio(yt_dlp: &Path, url: &str, dir: &Path) -> Result<PathBuf, MediaError> {
    let template = dir.join("snd0_src.%(ext)s");
    let out = run(
        yt_dlp,
        &[
            "--no-playlist".as_ref(), "-f".as_ref(), "bestaudio".as_ref(), "-o".as_ref(), template.as_os_str(),
            "--print".as_ref(), "after_move:filepath".as_ref(), url.as_ref(),
        ],
    )?;
    let path = String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or_default().trim().to_string();
    let p = PathBuf::from(path);
    if p.is_file() { Ok(p) } else { Err(MediaError::Tool { tool: "yt-dlp".into(), detail: "no output file reported".into() }) }
}

#[derive(Debug, Clone)]
pub struct Snd0Request<'a> {
    pub input: &'a Path,
    pub start_secs: f64,
    /// ATRAC3 bitrate: 66, 105 or 132 kbps.
    pub kbps: u32,
    /// Bytes left in the 500 KB budget (minus ICON1.PMF).
    pub budget: u64,
}

/// Full SND0.AT3 pipeline: FFmpeg → WAV → ATRAC3, sized to fit the XMB budget.
pub fn make_snd0(ffmpeg: &Ffmpeg, enc: &At3Encoder, req: &Snd0Request<'_>, work: &Path) -> Result<Vec<u8>, MediaError> {
    // ATRAC3 is CBR: duration that fits = budget·8 / bitrate, minus 2% for the RIFF header/rounding.
    let max_secs = (req.budget as f64 * 8.0 / (req.kbps as f64 * 1000.0) * 0.98).clamp(1.0, 60.0);
    let wav = work.join("snd0.wav");
    let at3 = work.join("SND0.AT3");
    ffmpeg.to_wav(req.input, &wav, req.start_secs, max_secs)?;
    enc.encode(&wav, &at3, req.kbps)?;
    let bytes = std::fs::read(&at3).map_err(|e| MediaError::Io(at3.display().to_string(), e))?;
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::remove_file(&at3);
    if !is_riff_atrac3(&bytes) {
        return Err(MediaError::Tool { tool: enc.path().display().to_string(), detail: "output is not RIFF/ATRAC3 (fmt 0x0270)".into() });
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
        b[20] = 0x01; // PCM
        assert!(!is_riff_atrac3(&b));
        assert!(!is_riff_atrac3(b"nope"));
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
        ff.to_wav(&dir.path().join("in.wav"), &wav, 0.0, 2.0).unwrap();
        let b = std::fs::read(&wav).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert!(b.len() > 44100 * 4, "≈2 s of stereo s16");
    }
}
