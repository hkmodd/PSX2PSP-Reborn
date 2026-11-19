//! Self-update from GitHub Releases, shared by the GUI and the CLI.
//!
//! Contract with the release workflow: tags `vMAJOR.MINOR.PATCH` (the workspace version), one
//! raw executable per program and platform named `<program>-<os>-<arch>[.exe]`, and a
//! `SHA256SUMS.txt` with `<lowercase hex>  <file name>` lines. Nothing is installed unless the
//! downloaded bytes match their line in `SHA256SUMS.txt`.
//!
//! HTTPS goes through `ureq` + `rustls` with the Mozilla root set compiled in: no OpenSSL, no
//! curl, no platform certificate store, nothing downloaded besides the new executable.
//!
//! Install: the new file is downloaded next to the running executable (same volume, so the swap
//! is a rename). Windows cannot overwrite a running image, so the running exe is renamed to
//! `<exe>.old` first and the new file moved into its place; if that second step fails the rename
//! is rolled back. The `.old` file is deleted by the next start ([`cleanup_leftovers`]). Unix
//! renames the new file over the old one atomically (the running process keeps its inode).

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use psx2psp_core::Monitor;
use sha2::{Digest, Sha256};

pub const REPO: &str = "hkmodd/PSX2PSP-Reborn";
/// The running version (workspace `version`).
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const API_LATEST: &str = "https://api.github.com/repos/hkmodd/PSX2PSP-Reborn/releases/latest";
const SUMS: &str = "SHA256SUMS.txt";
/// Sanity cap on the executable download (the GUI is ~30 MB).
const MAX_DOWNLOAD: u64 = 512 << 20;

// ─────────────────────────────── versions ───────────────────────────────

/// `MAJOR.MINOR.PATCH[-pre]`; a pre-release sorts below its release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    /// Strict parse of `1.2.3`, `v1.2.3` or `v1.2.3-rc.1`; anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') => (c, Some(p.to_owned())),
            Some(_) => return None,
            None => (s, None),
        };
        let mut it = core.split('.');
        let mut num = || it.next().filter(|n| !n.is_empty() && n.len() <= 18 && n.bytes().all(|b| b.is_ascii_digit())).and_then(|n| n.parse().ok());
        let v = Self { major: num()?, minor: num()?, patch: num()?, pre };
        it.next().is_none().then_some(v)
    }

    pub fn current() -> Self {
        Self::parse(CURRENT).expect("the workspace version is MAJOR.MINOR.PATCH")
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        (self.major, self.minor, self.patch).cmp(&(o.major, o.minor, o.patch)).then_with(|| match (&self.pre, &o.pre) {
            (None, None) => Equal,
            (None, Some(_)) => Greater,
            (Some(_), None) => Less,
            (Some(a), Some(b)) => a.cmp(b),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        match &self.pre {
            Some(p) => write!(f, "-{p}"),
            None => Ok(()),
        }
    }
}

// ─────────────────────────────── assets ───────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    /// `psx2psp-reborn`, the desktop app.
    Gui,
    /// `psx2psp`, the command line.
    Cli,
}

/// Release asset of `program` for a target (`std::env::consts::{OS, ARCH}` spelling), if published.
pub fn asset_name(program: Program, os: &str, arch: &str) -> Option<String> {
    let stem = match program {
        Program::Gui => "psx2psp-reborn",
        Program::Cli => "psx2psp",
    };
    // Windows, Linux and macOS, each on x86_64 and aarch64: the six builds of release.yml.
    let ext = match (os, arch) {
        ("windows", "x86_64" | "aarch64") => ".exe",
        ("linux" | "macos", "x86_64" | "aarch64") => "",
        _ => return None,
    };
    Some(format!("{stem}-{os}-{arch}{ext}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    /// Release page (notes) on github.com.
    pub page: String,
    pub notes: String,
    /// The executable for this program and platform.
    pub asset: Asset,
    pub sums: Option<Asset>,
}

#[derive(Debug, Clone)]
pub enum Check {
    /// The latest release is not newer than the running version.
    UpToDate(Version),
    Available(Box<Release>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    /// No published release (also what GitHub answers for a missing or private repository).
    #[error("no release has been published yet")]
    NoRelease,
    #[error("GitHub's request limit for this network was reached — try again in an hour")]
    RateLimited,
    #[error("can't reach GitHub ({0})")]
    Offline(String),
    #[error("GitHub answered HTTP {0}")]
    Http(u16),
    #[error("release v{0} has no build for this platform ({1})")]
    NoAsset(String, String),
    #[error("unexpected release data: {0}")]
    Invalid(String),
    #[error("{0} — the update was not installed")]
    Checksum(String),
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Io(String),
}

// ──────────────────────────── release parsing ────────────────────────────

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(serde::Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Interprets a `releases/latest` response for `program` on `os`/`arch`.
pub fn parse_latest(json: &str, program: Program, os: &str, arch: &str, current: &Version) -> Result<Check, UpdateError> {
    let r: GhRelease = serde_json::from_str(json).map_err(|e| UpdateError::Invalid(e.to_string()))?;
    // `/latest` never returns these; a stable build never offers them either.
    let Some(version) = Version::parse(&r.tag_name).filter(|v| v.pre.is_none() && !r.draft && !r.prerelease) else {
        return Err(UpdateError::NoRelease);
    };
    if version <= *current {
        return Ok(Check::UpToDate(version));
    }
    let find = |name: &str| {
        r.assets
            .iter()
            .find(|a| a.name == name && a.browser_download_url.starts_with("https://"))
            .map(|a| Asset { name: a.name.clone(), url: a.browser_download_url.clone(), size: a.size })
    };
    let target = format!("{os}-{arch}");
    let asset = asset_name(program, os, arch).and_then(|n| find(&n)).ok_or_else(|| UpdateError::NoAsset(version.to_string(), target))?;
    Ok(Check::Available(Box::new(Release {
        // The tag parsed as a version, so it is safe to put in a URL.
        page: format!("https://github.com/{REPO}/releases/tag/{}", r.tag_name.trim()),
        tag: r.tag_name.trim().to_owned(),
        notes: r.body.unwrap_or_default(),
        sums: find(SUMS),
        asset,
        version,
    })))
}

/// `SHA256SUMS.txt` → `(hex, file name)` pairs; malformed lines are skipped. Accepts the
/// `sha256sum` binary-mode marker (`<hex> *<name>`).
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (hex, name) = l.trim().split_once(char::is_whitespace)?;
            let name = name.trim_start();
            let name = name.strip_prefix('*').unwrap_or(name).trim_end();
            (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty()).then(|| (hex.to_ascii_lowercase(), name.to_owned()))
        })
        .collect()
}

/// The expected digest of `name`, or why there is none.
pub fn expected_sum(sums: &str, name: &str) -> Result<String, UpdateError> {
    parse_sums(sums)
        .into_iter()
        .find(|(_, n)| n == name)
        .map(|(h, _)| h)
        .ok_or_else(|| UpdateError::Checksum(format!("{SUMS} has no entry for {name}")))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn verify(actual_hex: &str, expected_hex: &str) -> Result<(), UpdateError> {
    if actual_hex.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(UpdateError::Checksum(format!("SHA-256 mismatch (expected {expected_hex}, got {actual_hex})")))
    }
}

// ──────────────────────────────── network ────────────────────────────────

fn agent(global: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(format!("PSX2PSP-Reborn/{CURRENT}"))
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_per_read(Some(Duration::from_secs(30)))
        .timeout_global(global)
        .build()
        .into()
}

fn net(e: ureq::Error) -> UpdateError {
    UpdateError::Offline(e.to_string())
}

/// Maps a non-200 answer from GitHub.
fn status_error(r: &ureq::http::Response<ureq::Body>) -> UpdateError {
    let header = |k: &str| r.headers().get(k).and_then(|v| v.to_str().ok());
    match r.status().as_u16() {
        404 => UpdateError::NoRelease,
        429 => UpdateError::RateLimited,
        403 if header("x-ratelimit-remaining") == Some("0") || header("retry-after").is_some() => UpdateError::RateLimited,
        s => UpdateError::Http(s),
    }
}

fn get(agent: &ureq::Agent, url: &str, accept: &str) -> Result<ureq::http::Response<ureq::Body>, UpdateError> {
    let r = agent.get(url).header("Accept", accept).call().map_err(net)?;
    if r.status().as_u16() == 200 { Ok(r) } else { Err(status_error(&r)) }
}

/// Asks GitHub for the latest release of `program` for this platform.
pub fn check(program: Program) -> Result<Check, UpdateError> {
    let agent = agent(Some(Duration::from_secs(30)));
    let mut r = get(&agent, API_LATEST, "application/vnd.github+json")?;
    let json = r.body_mut().with_config().limit(4 << 20).read_to_string().map_err(net)?;
    parse_latest(&json, program, std::env::consts::OS, std::env::consts::ARCH, &Version::current())
}

/// Where [`download`] puts the new executable: hidden, next to the running one.
pub fn download_path(dir: &Path, asset: &str) -> PathBuf {
    dir.join(format!(".{asset}.download"))
}

/// Downloads the release's executable into `dir` (streamed, cancellable via `mon`) and verifies it
/// against `SHA256SUMS.txt`. Returns the verified file; nothing is left behind on failure.
pub fn download(release: &Release, dir: &Path, mon: &dyn Monitor) -> Result<PathBuf, UpdateError> {
    let sums = release.sums.as_ref().ok_or_else(|| UpdateError::Checksum(format!("release v{} has no {SUMS}", release.version)))?;
    let asset = &release.asset;
    if asset.size > MAX_DOWNLOAD {
        return Err(UpdateError::Invalid(format!("{} is {} MB", asset.name, asset.size >> 20)));
    }
    let agent = agent(None);
    mon.log(&format!("Fetching {SUMS}"));
    let text = get(&agent, &sums.url, "application/octet-stream")?.body_mut().with_config().limit(1 << 20).read_to_string().map_err(net)?;
    let expected = expected_sum(&text, &asset.name)?;

    let dest = download_path(dir, &asset.name);
    let r = fetch_verified(&agent, asset, &expected, &dest, mon);
    if r.is_err() {
        let _ = fs::remove_file(&dest);
    }
    r.map(|()| dest)
}

fn fetch_verified(agent: &ureq::Agent, asset: &Asset, expected: &str, dest: &Path, mon: &dyn Monitor) -> Result<(), UpdateError> {
    let io_err = |e: io::Error| UpdateError::Io(format!("can't write {}: {e}", dest.display()));
    let mut out = File::create(dest).map_err(|e| UpdateError::Io(format!("can't write next to the program ({}): {e}", dest.parent().unwrap_or(dest).display())))?;
    mon.log(&format!("Downloading {} ({:.1} MB)", asset.name, asset.size as f64 / 1e6));
    let mut r = get(agent, &asset.url, "application/octet-stream")?;
    let total = r.body().content_length().filter(|&n| n > 0).unwrap_or(asset.size);
    let mut body = r.body_mut().as_reader();
    let (mut hasher, mut done, mut buf) = (Sha256::new(), 0u64, vec![0u8; 64 << 10]);
    loop {
        if mon.cancelled() {
            return Err(UpdateError::Cancelled);
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(UpdateError::Offline(format!("download interrupted: {e}"))),
        };
        done += n as u64;
        if done > MAX_DOWNLOAD {
            return Err(UpdateError::Invalid(format!("{} is larger than announced", asset.name)));
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(io_err)?;
        mon.progress(done, total.max(done));
    }
    out.sync_all().map_err(io_err)?;
    if asset.size > 0 && done != asset.size {
        return Err(UpdateError::Offline(format!("download truncated ({done} of {} bytes)", asset.size)));
    }
    verify(&hex(&hasher.finalize()), expected)?;
    mon.log("SHA-256 verified");
    Ok(())
}

// ──────────────────────────────── install ────────────────────────────────

/// `<exe>.old`: the previous executable, kept until the next start (Windows).
pub fn backup_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Moves the running `exe` aside to [`backup_path`] and `new` into its place; if the second
/// rename fails the first is undone, so `exe` always exists afterwards.
pub fn swap_with_backup(new: &Path, exe: &Path) -> io::Result<()> {
    let old = backup_path(exe);
    // A leftover from an earlier update whose cleanup has not run yet.
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)?;
    if let Err(e) = fs::rename(new, exe) {
        return match fs::rename(&old, exe) {
            Ok(()) => Err(e),
            Err(e2) => Err(io::Error::other(format!("{e}; restoring the previous version failed too ({e2}) — it is at {}", old.display()))),
        };
    }
    Ok(())
}

/// Replaces `exe` (normally `std::env::current_exe()`) with the verified file `new`.
pub fn install(new: &Path, exe: &Path) -> Result<(), UpdateError> {
    let fail = |e: io::Error| {
        let _ = fs::remove_file(new);
        UpdateError::Io(format!("can't replace {}: {e}", exe.display()))
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(exe).map(|m| m.permissions().mode()).unwrap_or(0o755) | 0o111;
        fs::set_permissions(new, fs::Permissions::from_mode(mode)).map_err(fail)?;
        fs::rename(new, exe).map_err(fail)
    }
    #[cfg(not(unix))]
    {
        swap_with_backup(new, exe).map_err(fail)
    }
}

/// Starts `exe` with this process's arguments (the caller then exits).
pub fn relaunch(exe: &Path) -> io::Result<()> {
    std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn().map(drop)
}

/// Deletes what an earlier update left next to the running executable: `<exe>.old` (retried for up
/// to `wait`, since the previous process may still be exiting) and any interrupted download.
pub fn cleanup_leftovers(program: Program, wait: Duration) {
    let Ok(exe) = std::env::current_exe() else { return };
    if let Some(name) = asset_name(program, std::env::consts::OS, std::env::consts::ARCH)
        && let Some(dir) = exe.parent()
    {
        let _ = fs::remove_file(download_path(dir, &name));
    }
    let old = backup_path(&exe);
    let deadline = std::time::Instant::now() + wait;
    while old.exists() && fs::remove_file(&old).is_err() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn versions_parse_strictly_and_order_like_semver() {
        assert_eq!(v("v1.2.3"), Version { major: 1, minor: 2, patch: 3, pre: None });
        assert_eq!(v("0.10.0").to_string(), "0.10.0");
        assert_eq!(v("V2.0.0-rc.1").pre.as_deref(), Some("rc.1"));
        for bad in ["", "v", "1.2", "1.2.3.4", "1..3", "a.b.c", "v1.2.3-", "1.2.+3", " v-1.2.3", "latest", "1.2.3 beta", "１.2.3", "99999999999999999999.0.0"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert!(v("0.10.0") > v("0.9.9"), "numeric, not lexicographic");
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.2") > v("1.0.0-rc.1"));
        assert!(v("1.0.1-alpha") > v("1.0.0"));
        assert_eq!(v("v1.2.3").cmp(&v("1.2.3")), std::cmp::Ordering::Equal);
        assert!(Version::current().pre.is_none());
    }

    #[test]
    fn asset_names_follow_the_release_contract() {
        let n = |p, os, arch| asset_name(p, os, arch);
        assert_eq!(n(Program::Gui, "windows", "x86_64").as_deref(), Some("psx2psp-reborn-windows-x86_64.exe"));
        assert_eq!(n(Program::Gui, "linux", "x86_64").as_deref(), Some("psx2psp-reborn-linux-x86_64"));
        assert_eq!(n(Program::Gui, "macos", "aarch64").as_deref(), Some("psx2psp-reborn-macos-aarch64"));
        assert_eq!(n(Program::Cli, "windows", "x86_64").as_deref(), Some("psx2psp-windows-x86_64.exe"));
        assert_eq!(n(Program::Cli, "linux", "x86_64").as_deref(), Some("psx2psp-linux-x86_64"));
        assert_eq!(n(Program::Cli, "macos", "aarch64").as_deref(), Some("psx2psp-macos-aarch64"));
        assert_eq!(n(Program::Gui, "windows", "aarch64").as_deref(), Some("psx2psp-reborn-windows-aarch64.exe"));
        assert_eq!(n(Program::Cli, "windows", "aarch64").as_deref(), Some("psx2psp-windows-aarch64.exe"));
        assert_eq!(n(Program::Gui, "linux", "aarch64").as_deref(), Some("psx2psp-reborn-linux-aarch64"));
        assert_eq!(n(Program::Cli, "linux", "aarch64").as_deref(), Some("psx2psp-linux-aarch64"));
        assert_eq!(n(Program::Gui, "macos", "x86_64").as_deref(), Some("psx2psp-reborn-macos-x86_64"));
        assert_eq!(n(Program::Cli, "macos", "x86_64").as_deref(), Some("psx2psp-macos-x86_64"));
        for (os, arch) in [("windows", "x86"), ("linux", "arm"), ("linux", "riscv64"), ("freebsd", "x86_64")] {
            assert_eq!(n(Program::Gui, os, arch), None, "{os}-{arch}");
        }
    }

    fn release_json(tag: &str, extra: &str, assets: &[&str]) -> String {
        let assets: Vec<String> = assets
            .iter()
            .map(|a| format!(r#"{{"name":"{a}","size":1234,"browser_download_url":"https://github.com/{REPO}/releases/download/{tag}/{a}"}}"#))
            .collect();
        format!(r#"{{"tag_name":"{tag}","html_url":"x","body":"notes",{extra}"assets":[{}]}}"#, assets.join(","))
    }

    #[test]
    fn latest_release_is_interpreted() {
        let cur = v("0.1.0");
        let all = ["psx2psp-reborn-windows-x86_64.exe", "psx2psp-linux-x86_64", SUMS];
        let json = release_json("v0.2.0", "", &all);
        let Ok(Check::Available(r)) = parse_latest(&json, Program::Gui, "windows", "x86_64", &cur) else { panic!() };
        assert_eq!((r.version.to_string(), r.tag.as_str(), r.asset.name.as_str(), r.asset.size), ("0.2.0".into(), "v0.2.0", all[0], 1234));
        assert_eq!(r.page, format!("https://github.com/{REPO}/releases/tag/v0.2.0"));
        assert_eq!(r.sums.unwrap().name, SUMS);
        assert_eq!(r.notes, "notes");

        let Ok(Check::Available(r)) = parse_latest(&json, Program::Cli, "linux", "x86_64", &cur) else { panic!() };
        assert_eq!(r.asset.name, all[1]);
        assert_eq!(parse_latest(&json, Program::Gui, "macos", "aarch64", &cur).unwrap_err(), UpdateError::NoAsset("0.2.0".into(), "macos-aarch64".into()));
        assert!(matches!(parse_latest(&json, Program::Gui, "windows", "x86_64", &v("0.2.0")), Ok(Check::UpToDate(l)) if l == v("0.2.0")));
        assert!(matches!(parse_latest(&json, Program::Gui, "windows", "x86_64", &v("1.0.0")), Ok(Check::UpToDate(_))));

        // Pre-releases, drafts and garbage tags are never offered.
        for (tag, extra) in [("v0.3.0-beta.1", ""), ("v0.3.0", r#""prerelease":true,"#), ("v0.3.0", r#""draft":true,"#), ("nightly", ""), ("v0.3", "")] {
            let j = release_json(tag, extra, &all);
            assert_eq!(parse_latest(&j, Program::Gui, "windows", "x86_64", &cur).unwrap_err(), UpdateError::NoRelease, "{tag} {extra}");
        }
        assert!(matches!(parse_latest("<html>", Program::Gui, "windows", "x86_64", &cur), Err(UpdateError::Invalid(_))));
        // No SHA256SUMS.txt: still offered, but `download` refuses it.
        let j = release_json("v0.2.0", "", &all[..1]);
        assert!(matches!(parse_latest(&j, Program::Gui, "windows", "x86_64", &cur), Ok(Check::Available(r)) if r.sums.is_none()));
    }

    #[test]
    fn sums_parse_and_verify() {
        let a = sha256_hex(b"abc");
        assert_eq!(a, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let text = format!(
            "{a}  psx2psp-reborn-windows-x86_64.exe\r\n\n{}  psx2psp-linux-x86_64\nnot a line\n{} *psx2psp-macos-aarch64\nabc  short-hash\n",
            sha256_hex(b"linux"),
            sha256_hex(b"mac").to_uppercase()
        );
        let sums = parse_sums(&text);
        assert_eq!(sums.len(), 3);
        assert_eq!(expected_sum(&text, "psx2psp-reborn-windows-x86_64.exe").unwrap(), a);
        assert_eq!(expected_sum(&text, "psx2psp-macos-aarch64").unwrap(), sha256_hex(b"mac"), "binary marker, lower-cased");
        assert!(matches!(expected_sum(&text, "psx2psp-reborn-linux-x86_64"), Err(UpdateError::Checksum(_))), "missing entry refuses");
        assert!(matches!(expected_sum(&text, "short-hash"), Err(UpdateError::Checksum(_))));

        assert_eq!(verify(&sha256_hex(b"abc"), &a.to_uppercase()), Ok(()));
        assert!(matches!(verify(&sha256_hex(b"abd"), &a), Err(UpdateError::Checksum(_))));
    }

    #[test]
    fn swap_replaces_and_keeps_a_backup() {
        let d = tempfile::tempdir().unwrap();
        let (exe, new) = (d.path().join("app.exe"), download_path(d.path(), "app-windows-x86_64.exe"));
        fs::write(&exe, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        swap_with_backup(&new, &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(fs::read(backup_path(&exe)).unwrap(), b"old");
        assert_eq!(backup_path(&exe), d.path().join("app.exe.old"));
        assert!(!new.exists());

        // A second update replaces the stale backup.
        fs::write(&new, b"newer").unwrap();
        swap_with_backup(&new, &exe).unwrap();
        assert_eq!((fs::read(&exe).unwrap(), fs::read(backup_path(&exe)).unwrap()), (b"newer".to_vec(), b"new".to_vec()));
    }

    #[test]
    fn swap_rolls_back_when_the_new_file_cannot_be_moved() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("app.exe");
        fs::write(&exe, b"old").unwrap();
        assert!(swap_with_backup(&d.path().join("missing"), &exe).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old", "the running exe is back in place");
        assert!(!backup_path(&exe).exists());
    }

    #[test]
    fn install_puts_the_new_file_in_place() {
        let d = tempfile::tempdir().unwrap();
        let (exe, new) = (d.path().join("tool"), d.path().join(".tool.download"));
        fs::write(&exe, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        install(&new, &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert!(!new.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0, "executable bit");
        }
        // A failed install removes the download and leaves the exe untouched.
        assert!(install(&d.path().join("missing"), &exe).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"new");
    }

    /// The Windows premise: a running executable can be renamed (not overwritten), so the swap
    /// works on the live image and `.old` can only be deleted once it has exited.
    #[cfg(windows)]
    #[test]
    fn swap_works_on_a_running_executable() {
        let Some(ping) = std::env::var_os("SystemRoot").map(|r| PathBuf::from(r).join("System32").join("PING.EXE")).filter(|p| p.is_file()) else { return };
        let d = tempfile::tempdir().unwrap();
        let (exe, new) = (d.path().join("app.exe"), d.path().join(".app.download"));
        fs::copy(&ping, &exe).unwrap();
        fs::write(&new, b"new").unwrap();
        let mut child = std::process::Command::new(&exe).args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(fs::write(&exe, b"x").is_err(), "a running image can't be overwritten");
        let r = swap_with_backup(&new, &exe);
        assert!(fs::remove_file(backup_path(&exe)).is_err(), "still mapped by the running process");
        let _ = child.kill();
        let _ = child.wait();
        r.unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        fs::remove_file(backup_path(&exe)).unwrap();
    }

    /// Serves `routes` (path → body) over plain HTTP on localhost; returns the base URL.
    fn serve(routes: Vec<(&'static str, Vec<u8>)>) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut req = Vec::new();
                let mut b = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&b[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&req).lines().next().unwrap_or_default().to_owned();
                let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
                let (status, body) = routes.iter().find(|(p, _)| *p == path).map_or(("404 Not Found", &[][..]), |(_, b)| ("200 OK", &b[..]));
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(body);
            }
        });
        base
    }

    struct Stop(bool);
    impl Monitor for Stop {
        fn cancelled(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn download_verifies_then_installs_and_refuses_bad_files() {
        let exe_bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let name = "psx2psp-reborn-windows-x86_64.exe";
        let good = format!("{}  {name}\n", sha256_hex(&exe_bytes));
        let bad = format!("{}  {name}\n", sha256_hex(b"something else"));
        let other = format!("{}  psx2psp-linux-x86_64\n", sha256_hex(&exe_bytes));
        let base = serve(vec![("/exe", exe_bytes.clone()), ("/good", good.into()), ("/bad", bad.into()), ("/other", other.into())]);
        let release = |sums: Option<&str>| Release {
            version: v("9.9.9"),
            tag: "v9.9.9".into(),
            page: String::new(),
            notes: String::new(),
            asset: Asset { name: name.into(), url: format!("{base}/exe"), size: exe_bytes.len() as u64 },
            sums: sums.map(|p| Asset { name: SUMS.into(), url: format!("{base}{p}"), size: 0 }),
        };
        let d = tempfile::tempdir().unwrap();
        let leftovers = || fs::read_dir(d.path()).unwrap().count();

        for (sums, why) in [(Some("/bad"), "mismatch"), (Some("/other"), "no entry"), (None, "no SHA256SUMS.txt"), (Some("/missing"), "SHA256SUMS.txt 404")] {
            assert!(download(&release(sums), d.path(), &()).is_err(), "{why}");
            assert_eq!(leftovers(), 0, "{why}: nothing left behind");
        }
        assert_eq!(download(&release(Some("/good")), d.path(), &Stop(true)).unwrap_err(), UpdateError::Cancelled);
        assert_eq!(leftovers(), 0);

        let file = download(&release(Some("/good")), d.path(), &()).unwrap();
        assert_eq!(file, download_path(d.path(), name));
        assert_eq!(fs::read(&file).unwrap(), exe_bytes);
        let exe = d.path().join("app.exe");
        fs::write(&exe, b"old").unwrap();
        install(&file, &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), exe_bytes);
    }

    /// Live: the repository has no release yet, so this must be the quiet "no release" path
    /// (or offline / rate limited on a disconnected or busy machine).
    #[test]
    #[ignore = "network"]
    fn live_check_reaches_github() {
        match check(Program::Gui) {
            Err(UpdateError::NoRelease) => eprintln!("no release published"),
            other => eprintln!("{other:?}"),
        }
    }
}
