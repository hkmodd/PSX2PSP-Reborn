//! One-click provisioning of the external tools into `<exe dir>/tools/`, where
//! [`crate::tools::which`] looks after the executable's own folder.
//!
//! Official release binaries only: yt-dlp (Unlicense), FFmpeg GPL static builds by BtbN (include
//! libx264) and, for macOS, which BtbN does not build, Martin Riedl's signed static builds (also with
//! libx264), pinned and checked by SHA-256. Downloads go through the OS's own `curl`
//! and archives are unpacked by `tar` (bsdtar on Windows 10+ reads `.zip`), so no HTTP/TLS/zip
//! code is linked into the app.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::MediaError;
use crate::tools::{self, Ffmpeg, YtDlp};

/// The ATRAC3 encoder is built in ([`crate::atrac3`]), so only these two are ever downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
}

impl Tool {
    pub const ALL: [Self; 2] = [Self::Ffmpeg, Self::YtDlp];

    pub fn exe(self) -> &'static str {
        match self {
            Self::YtDlp => "yt-dlp",
            Self::Ffmpeg => "ffmpeg",
        }
    }

    /// Whether a usable copy is already installed (FFmpeg must include libx264).
    pub fn present(self) -> bool {
        match self {
            Self::YtDlp => YtDlp::detect().is_some(),
            Self::Ffmpeg => Ffmpeg::detect().is_some_and(|f| f.has_x264()),
        }
    }

    /// The release asset for this OS/CPU, if one is published.
    pub fn asset(self) -> Option<Asset> {
        let (os, arm) = (std::env::consts::OS, std::env::consts::ARCH == "aarch64");
        let gh = |repo: &str, tag: &str, file: &str| format!("https://github.com/{repo}/releases/{tag}/{file}");
        // Pinned rather than `redirect/latest/...`, which answered 404 on alternate requests
        // (2026-11-19); the sums are the ones published beside each zip. FFmpeg 9.0.2, x264 0.165.
        let mr = |build: &str| format!("https://ffmpeg.martin-riedl.de/download/macos/{build}/ffmpeg.zip");
        let (url, archive, mb, sha256) = match (self, os, arm) {
            (Self::YtDlp, "windows", false) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp.exe"), false, 23, None),
            (Self::YtDlp, "windows", true) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp_arm64.exe"), false, 23, None),
            (Self::YtDlp, "linux", false) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp_linux"), false, 34, None),
            (Self::YtDlp, "linux", true) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp_linux_aarch64"), false, 34, None),
            (Self::YtDlp, "macos", _) => (gh("yt-dlp/yt-dlp", "latest/download", "yt-dlp_macos"), false, 35, None),
            (Self::Ffmpeg, "windows", false) => (gh("BtbN/FFmpeg-Builds", "download/latest", "ffmpeg-master-latest-win64-gpl.zip"), true, 193, None),
            (Self::Ffmpeg, "windows", true) => (gh("BtbN/FFmpeg-Builds", "download/latest", "ffmpeg-master-latest-winarm64-gpl.zip"), true, 136, None),
            (Self::Ffmpeg, "linux", false) => (gh("BtbN/FFmpeg-Builds", "download/latest", "ffmpeg-master-latest-linux64-gpl.tar.xz"), true, 150, None),
            (Self::Ffmpeg, "linux", true) => (gh("BtbN/FFmpeg-Builds", "download/latest", "ffmpeg-master-latest-linuxarm64-gpl.tar.xz"), true, 126, None),
            (Self::Ffmpeg, "macos", false) => (mr("amd64/1789931006_9.0.2"), true, 34, Some("7c6b4125b191cbf773832dc51f424cf2b6bb7da43007d1e066f95909e47cacd4")),
            (Self::Ffmpeg, "macos", true) => (mr("arm64/1789931890_9.0.2"), true, 29, Some("c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924")),
            _ => return None,
        };
        Some(Asset { tool: self, url, archive, approx_mb: mb, sha256 })
    }

    /// What to do on platforms without a published binary.
    pub fn manual_hint(self) -> &'static str {
        match self {
            Self::YtDlp => "pip install yt-dlp",
            Self::Ffmpeg => "brew install ffmpeg (or your package manager's ffmpeg with libx264)",
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
    /// Expected SHA-256 (lower-case hex) of the download, for files pinned outside GitHub releases.
    pub sha256: Option<&'static str>,
}

fn sha256_file(p: &Path) -> Result<String, MediaError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(p).map_err(io(p))?;
    let (mut h, mut buf) = (Sha256::new(), vec![0u8; 64 << 10]);
    loop {
        let n = f.read(&mut buf).map_err(io(p))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
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
    install_into(asset, &tools::tools_dir().ok_or(MediaError::Missing("executable directory"))?, log)
}

fn install_into(asset: &Asset, dir: &Path, log: &dyn Fn(&str)) -> Result<PathBuf, MediaError> {
    let tmp = dir.join(format!(".dl-{}", asset.tool.exe()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(io(&tmp))?;
    let r = (|| {
        let file = tmp.join(asset.url.rsplit('/').next().unwrap_or("download"));
        log(&format!("Downloading {} (~{} MB) from {}", asset.tool.exe(), asset.approx_mb, asset.url));
        let curl = tools::which("curl").ok_or(MediaError::Missing("curl"))?;
        tools::run(&curl, &["-fsSL".as_ref(), "--retry".as_ref(), "3".as_ref(), "-o".as_ref(), file.as_os_str(), asset.url.as_ref()])?;
        if let Some(want) = asset.sha256 {
            let got = sha256_file(&file)?;
            if got != want {
                return Err(MediaError::Tool { tool: asset.tool.exe().into(), detail: format!("download has SHA-256 {got}, expected {want}") });
            }
        }
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
        // The executable plus any DLLs beside it (shared builds keep their runtime next to the .exe).
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
                let release = a.url.starts_with("https://github.com/") && a.url.contains("/releases/");
                let pinned = a.url.starts_with("https://") && a.sha256.is_some_and(|s| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
                assert!(release || pinned, "{}: a GitHub release, or pinned by its SHA-256", a.url);
                assert_eq!(a.archive, a.url.ends_with(".zip") || a.url.ends_with(".tar.xz"), "{}", a.url);
            }
        }
        // Every target the releases ship for (release.yml).
        if cfg!(all(any(windows, target_os = "linux", target_os = "macos"), any(target_arch = "x86_64", target_arch = "aarch64"))) {
            assert!(Tool::ALL.iter().all(|t| t.asset().is_some()), "every shipped OS/CPU is fully provisionable");
        }
    }

    #[test]
    fn sha256_of_a_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("abc");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(sha256_file(&f).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    /// Downloads and runs the real FFmpeg for this OS/CPU (CI runs it on macOS, where the build is
    /// pinned outside GitHub): checksum, unpack, the binary starts, and it has libx264.
    #[test]
    #[ignore = "downloads ~30-190 MB"]
    fn live_ffmpeg_install() {
        let a = Tool::Ffmpeg.asset().expect("FFmpeg is provisionable here");
        let d = tempfile::tempdir().unwrap();
        let exe = install_into(&a, d.path(), &|m| eprintln!("{m}")).unwrap();
        assert!(Ffmpeg { path: exe }.has_x264(), "installed FFmpeg lacks libx264");
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
