//! One-click provisioning of the external tools into `<exe dir>/tools/`, where
//! [`crate::tools::which`] looks after the executable's own folder.
//!
//! Official release binaries only: yt-dlp (Unlicense), FFmpeg GPL static builds by BtbN (include
//! libx264) and atracdenc (LGPL-2.1, Windows builds only). Downloads go through the OS's own `curl`
//! and archives are unpacked by `tar` (bsdtar on Windows 10+ reads `.zip`), so no HTTP/TLS/zip
//! code is linked into the app.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::MediaError;
use crate::tools::{self, At3Encoder, Ffmpeg, YtDlp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    Atracdenc,
}

impl Tool {
    pub const ALL: [Self; 3] = [Self::Ffmpeg, Self::YtDlp, Self::Atracdenc];

    pub fn exe(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp",
            Self::Ffmpeg => "ffmpeg",
            Self::Atracdenc => "atracdenc",
        }
    }

    /// Whether a usable copy is already installed (FFmpeg must include libx264).
    pub fn present(self) -> bool {
        match self {
            Self::YtDlp => YtDlp::detect().is_some(),
            Self::Ffmpeg => Ffmpeg::detect().is_some_and(|f| f.has_x264()),
            Self::Atracdenc => At3Encoder::detect().is_some(),
        }
    }

    /// The release asset for this OS/CPU, if one is published.
    pub fn asset(self) -> Option<Asset> {
        let (os, arm) = (std::env::consts::OS, std::env::consts::ARCH == "aarch64");
        let gh = |repo: &str, tag: &str, file: &str| format!("https://github.com/{repo}/releases/{tag}/{file}");
        let (url, archive, mb) = match (self, os, arm) {
            (Self::YtDlp, "windows", false) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp.exe"), false, 23),
            (Self::YtDlp, "windows", true) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp_arm64.exe"), false, 23),
            (Self::YtDlp, "linux", false) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp_linux"), false, 34),
            (Self::YtDlp, "linux", true) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp_linux_aarch64"), false, 34),
            (Self::YtDlp, "macos", _) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp_macos"), false, 35),
            (Self::Ffmpeg, "windows", false) => (gh("BtbN/FFmpeg-Builds", "download/latest", "ffmpeg-master-latest-win64-gpl.zip"), true, 193),
            (Self::Ffmpeg, "windows", true) => (gh("BtbN/FFmpeg-Builds", "download/latest", "ffmpeg-master-latest-winarm64-gpl.zip"), true, 136),
            (Self::Ffmpeg, "linux", false) => (gh("BtbN/FFmpeg-Builds", "download/latest", "ffmpeg-master-latest-linux64-gpl.tar.xz"), true, 150),
            (Self::Ffmpeg, "linux", true) => (gh("BtbN/FFmpeg-Builds", "download/latest", "ffmpeg-master-latest-linuxarm64-gpl.tar.xz"), true, 126),
            // x86 build; runs on x64 and on Windows-on-ARM through emulation.
            (Self::Atracdenc, "windows", _) => (gh("dcherednik/atracdenc", "download/0.2.3", "atracdenc-win-x86_0.2.3.zip"), true, 1),
            _ => return None,
        };
        Some(Asset { tool: self, url, archive, approx_mb: mb })
    }

    /// What to do on platforms without a published binary.
    pub fn manual_hint(self) -> &'static str {
        match self {
            Self::YtDlp => "pip install yt-dlp",
            Self::Ffmpeg => "brew install ffmpeg (or your package manager's ffmpeg with libx264)",
            Self::Atracdenc => "build atracdenc from github.com/dcherednik/atracdenc (cmake) and put it on PATH",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub tool: Tool,
    pub url: String,
    pub archive: bool,
    /// Download size, for the confirmation prompt.
    pub approx_mb: u32,
}

/// Tools that are missing and installable here.
pub fn missing() -> Vec<Asset> {
    Tool::ALL.into_iter().filter(|t| !t.present()).filter_map(Tool::asset).collect()
}

fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return Some(p);
        }
    }
    None
}

fn io(p: &Path) -> impl FnOnce(std::io::Error) -> MediaError + '_ {
    move |e| MediaError::Io(p.display().to_string(), e)
}

/// Downloads and installs one tool into [`tools::tools_dir`]; returns the installed executable.
pub fn install(asset: &Asset, log: &dyn Fn(&str)) -> Result<PathBuf, MediaError> {
    let dir = tools::tools_dir().ok_or(MediaError::Missing("executable directory"))?;
    let tmp = dir.join(format!(".dl-{}", asset.tool.exe()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(io(&tmp))?;
    let r = (|| {
        let file = tmp.join(asset.url.rsplit('/').next().unwrap_or("download"));
        log(&format!("Downloading {} (~{} MB) from {}", asset.tool.exe(), asset.approx_mb, asset.url));
        let curl = tools::which("curl").ok_or(MediaError::Missing("curl"))?;
        tools::run(&curl, &["-fsSL".as_ref(), "--retry".as_ref(), "3".as_ref(), "-o".as_ref(), file.as_os_str(), asset.url.as_ref()])?;
        let exe = tools::exe_name(asset.tool.exe());
        let src = if asset.archive {
            log(&format!("Unpacking {}", file.display()));
            let x = tmp.join("x");
            std::fs::create_dir_all(&x).map_err(io(&x))?;
            let tar = tools::which("tar").ok_or(MediaError::Missing("tar"))?;
            tools::run(&tar, &["-xf".as_ref(), file.as_os_str(), "-C".as_ref(), x.as_os_str()])?;
            find(&x, &exe).ok_or_else(|| MediaError::Tool { tool: asset.tool.exe().into(), detail: format!("{exe} not found in the archive") })?
        } else {
            file
        };
        // The executable plus any DLLs beside it (atracdenc ships its runtime next to the .exe).
        let mut files = vec![src.clone()];
        if asset.archive && cfg!(windows) {
            let parent = src.parent().unwrap_or(&tmp);
            files.extend(std::fs::read_dir(parent).map_err(io(parent))?.flatten().map(|e| e.path()).filter(|p| p.extension() == Some(OsStr::new("dll"))));
        }
        for f in &files {
            let name = if f == &src { OsStr::new(&exe) } else { f.file_name().unwrap_or_default() };
            let to = dir.join(name);
            std::fs::copy(f, &to).map_err(io(&to))?;
        }
        let installed = dir.join(&exe);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).map_err(io(&installed))?;
        }
        log(&format!("Installed {}", installed.display()));
        Ok(installed)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_point_at_official_releases() {
        for t in Tool::ALL {
            if let Some(a) = t.asset() {
                assert!(a.url.starts_with("https://github.com/") && a.url.contains("/releases/"), "{}", a.url);
                assert_eq!(a.archive, a.url.ends_with(".zip") || a.url.ends_with(".tar.xz"), "{}", a.url);
            }
        }
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert!(Tool::ALL.iter().all(|t| t.asset().is_some()), "Windows x64 is fully provisionable");
        }
    }

    #[test]
    fn find_is_recursive_and_case_insensitive() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("a/b/bin");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("FFmpeg.exe"), b"").unwrap();
        assert_eq!(find(d.path(), "ffmpeg.exe"), Some(deep.join("FFmpeg.exe")));
        assert_eq!(find(d.path(), "nope"), None);
    }
}
