//! # psx2psp-engine
//!
//! Use cases orchestrating core + patching + media. Every operation is synchronous, streaming,
//! cancellable through [`Monitor`] and *atomic*: output is written to `*.part` and renamed only
//! on success, so a crash or cancel never leaves a truncated EBOOT behind.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use psx2psp_core::pbp::write_pbp;
use psx2psp_core::psar::{self, PsarDisc};
use psx2psp_core::subq;
use psx2psp_core::{DiscImage, DiscSpec, Donor, ImagePatch, Monitor, PbpFile, PbpSections, PsarJob, PsarStats, Region, Section, Sfo};
use psx2psp_patching::exe_crc::{self, SectorWrites};
use psx2psp_patching::sector::{self, SECTOR};
use psx2psp_patching::{PatternPatch, PpfPatch};

pub mod gamedb;
pub mod md5;
pub mod patchdb;
pub mod update;

use patchdb::{Kind, Patch, PatchDb};

pub use psx2psp_core as core;
pub use psx2psp_media as media;
pub use psx2psp_patching as patching;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Core(#[from] psx2psp_core::Error),
    #[error(transparent)]
    Media(#[from] psx2psp_media::MediaError),
    #[error("{path}: {source}")]
    Ppf { path: String, source: psx2psp_patching::PpfError },
    #[error("{0}")]
    Input(String),
}

impl From<std::io::Error> for EngineError {
    fn from(e: std::io::Error) -> Self {
        Self::Core(e.into())
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Built-in donor: the PS1 Classic `BASE.PBP` every PSXPackager release ships (STARTDAT boot PNG
/// and `\0PGD` trailer, verified offsets: PSAR 0x50000, STARTDAT at p1 0x100000).
/// Stored zlib-compressed by `build.rs` (1.4 MB → ~60 KB) and inflated once, on first use.
pub fn builtin_base_pbp() -> &'static [u8] {
    static PBP: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PBP.get_or_init(|| inflate(include_bytes!(concat!(env!("OUT_DIR"), "/BASE.PBP.z"))))
}

/// Inflates a zlib resource compressed by `build.rs`.
pub(crate) fn inflate(z: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(z).read_to_end(&mut out).expect("built-in resource is valid zlib");
    out
}
/// Built-in loader: the 19 002-byte ELF `DATA.PSP` that popstation/PSX2PSP and PSXPackager write
/// in preference to the donor's own signed module.
pub const BUILTIN_DATA_PSP: &[u8] = include_bytes!("../../../resources/DATA.PSP");

/// The donor used when the user supplies none: everything needed ships inside the binary.
pub fn builtin_donor() -> Result<Donor> {
    let mut donor = Donor::from_pbp(&mut PbpFile::open(std::io::Cursor::new(builtin_base_pbp()))?)?;
    donor.data_psp = BUILTIN_DATA_PSP.to_vec();
    Ok(donor)
}

/// `Some(path)` → that PS1 Classic EBOOT; `None` → [`builtin_donor`].
pub fn load_donor(path: Option<&Path>) -> Result<Donor> {
    match path {
        Some(p) => Ok(Donor::load(p)?),
        None => builtin_donor(),
    }
}

// ───────────────────────────── disc selection ─────────────────────────────

fn ext_lower(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase()
}

/// Identity of a file on disk: canonical path when it exists (resolves case, `..`, links).
fn path_key(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Files a `.cue` references, as identity keys (empty if the sheet can't be read).
fn cue_members(cue: &Path) -> Vec<PathBuf> {
    let base = cue.parent().unwrap_or(Path::new(""));
    std::fs::read(cue)
        .map(|t| psx2psp_core::disc::cue_file_names(&String::from_utf8_lossy(&t)).iter().map(|n| path_key(&base.join(n))).collect())
        .unwrap_or_default()
}

/// The `.cue` next to `bin` that references it: same stem first, then any other sheet in the folder.
pub fn companion_cue(bin: &Path) -> Option<PathBuf> {
    let key = path_key(bin);
    let dir = bin.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stem = bin.file_stem()?.to_string_lossy().to_lowercase();
    let mut cues: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| ext_lower(p) == "cue").collect();
    cues.sort_by_key(|c| (c.file_stem().map(|s| s.to_string_lossy().to_lowercase()) != Some(stem.clone()), natural_key(c)));
    cues.into_iter().find(|c| cue_members(c).contains(&key))
}

/// Entries of an `.m3u` playlist (comments and blank lines skipped), relative to the playlist.
pub fn read_m3u(path: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read(path).map_err(|e| EngineError::Input(format!("{}: {e}", path.display())))?;
    let base = path.parent().unwrap_or(Path::new(""));
    Ok(String::from_utf8_lossy(&text)
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{feff}'))
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| base.join(l))
        .collect())
}

/// Sort key that orders `Disc 2` before `Disc 10`.
fn natural_key(p: &Path) -> Vec<(u64, String)> {
    let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut out = Vec::new();
    let (mut num, mut text) = (String::new(), String::new());
    for c in name.chars() {
        if c.is_ascii_digit() {
            if !text.is_empty() {
                out.push((0, std::mem::take(&mut text)));
            }
            num.push(c);
        } else {
            if !num.is_empty() {
                out.push((num.parse().unwrap_or(u64::MAX), String::new()));
                num.clear();
            }
            text.push(c);
        }
    }
    if !num.is_empty() {
        out.push((num.parse().unwrap_or(u64::MAX), String::new()));
    }
    if !text.is_empty() {
        out.push((0, text));
    }
    out
}

/// Result of [`resolve_discs`]: one path per disc, in order, plus human-readable notes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscPlan {
    pub discs: Vec<PathBuf>,
    pub notes: Vec<String>,
}

/// Turns whatever the user picked or dropped into discs, the way a person means it:
/// - a `.cue` absorbs every BIN it references, so BIN + CUE together are **one** disc;
/// - a lone `.bin`/`.img` is upgraded to the `.cue` beside it that references it (that sheet
///   carries the track layout, CDDA audio tracks included);
/// - an `.m3u` expands to its discs, in playlist order;
/// - the same disc picked twice (or already listed in `existing`) is kept once.
///
/// `existing` keeps its order; `added` is ordered naturally (`Disc 2` before `Disc 10`) unless it
/// comes from a playlist.
pub fn resolve_discs(existing: &[PathBuf], added: &[PathBuf]) -> DiscPlan {
    let mut plan = DiscPlan::default();
    let mut added: Vec<PathBuf> = added.to_vec();
    added.sort_by_key(|p| natural_key(p));
    let name = |p: &Path| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());

    // Expand playlists in place.
    let mut items: Vec<(PathBuf, bool)> = existing.iter().map(|p| (p.clone(), true)).collect();
    for p in added {
        if ext_lower(&p) == "m3u" {
            match read_m3u(&p) {
                Ok(list) => {
                    plan.notes.push(format!("{}: {} disc(s)", name(&p), list.len()));
                    items.extend(list.into_iter().map(|e| (e, false)));
                }
                Err(e) => plan.notes.push(format!("{}: {e}", name(&p))),
            }
        } else {
            items.push((p, false));
        }
    }

    // BINs explicitly covered by a CUE in the same selection.
    let covered: Vec<(PathBuf, PathBuf)> = items
        .iter()
        .filter(|(p, _)| ext_lower(p) == "cue")
        .flat_map(|(c, _)| cue_members(c).into_iter().map(move |m| (m, c.clone())))
        .collect();

    let mut keys: Vec<PathBuf> = Vec::new();
    for (p, was_listed) in items {
        let disc = if ext_lower(&p) == "cue" {
            p
        } else if let Some((_, cue)) = covered.iter().find(|(m, _)| *m == path_key(&p)) {
            if !was_listed {
                plan.notes.push(format!("{}: part of {}, not a separate disc", name(&p), name(cue)));
            }
            cue.clone()
        } else if let Some(cue) = companion_cue(&p) {
            plan.notes.push(format!("{}: using {} for the track layout", name(&p), name(&cue)));
            cue
        } else {
            p
        };
        let key = path_key(&disc);
        if keys.contains(&key) {
            continue;
        }
        keys.push(key);
        plan.discs.push(disc);
    }
    plan
}

/// PSAR alignment used by every known working tool (PSXPackager, popstation).
pub const PSAR_ALIGN: u64 = 0x10000;
/// Default deflate level, as PSXPackager. Measured over 0x9300 blocks of a real disc: 5 is
/// within 0.1% of 9's size at half the compression time, and inflate time (the PSP's cost)
/// does not depend on the level. Level 0 (stored) is not offered: such EBOOTs are reported
/// not to boot under POPS.
pub const DEFAULT_LEVEL: u32 = 5;

#[derive(Debug, Clone, Default)]
pub struct DiscInput {
    pub path: PathBuf,
    /// Overrides the ID probed from SYSTEM.CNF.
    pub disc_id: Option<String>,
    /// PPF patch applied in-stream to this disc.
    pub ppf: Option<PathBuf>,
    /// Apply the generic PAL→NTSC preset ([`patching::pal_to_ntsc`]) after the PPF.
    pub pal_ntsc: bool,
    /// "Game fixes" switch: the POPS config and the `fix` patches [`patchdb`] has for this ID.
    pub title_fixes: bool,
    /// "Unlock fps" switch: the title's `fps` patches.
    pub fps_unlock: bool,
    /// "60 Hz" switch: the title's `60hz` patches (its own NTSC mode).
    pub ntsc_60hz: bool,
    /// Per-patch choices by name (case-insensitive), over the switches; [`patchdb::CONFIG`]
    /// names the POPS config.
    pub patch_choice: Vec<(String, bool)>,
}

impl DiscInput {
    pub fn switches(&self) -> patchdb::Switches {
        patchdb::Switches { fixes: self.title_fixes, fps: self.fps_unlock, ntsc60: self.ntsc_60hz }
    }
}

/// Slot-ready PNGs (see [`media::images::prepare`]). `None` = slot omitted (boot falls back to donor's).
#[derive(Debug, Clone, Default)]
pub struct Artwork {
    pub icon0: Option<Vec<u8>>,
    pub pic0: Option<Vec<u8>>,
    pub pic1: Option<Vec<u8>>,
    pub boot: Option<Vec<u8>>,
    /// Ready-made ICON1.PMF, passed through verbatim.
    pub icon1_pmf: Option<Vec<u8>>,
    /// Ready-made SND0.AT3 (see [`media::tools::make_snd0`]).
    pub snd0: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct ConvertRequest {
    pub discs: Vec<DiscInput>,
    /// Per-game POPS configs and patches ([`patchdb::load`] = built-in + user file).
    pub patches: PatchDb,
    pub title: String,
    pub output: PathBuf,
    /// Custom donor EBOOT; `None` uses the built-in one.
    pub donor: Option<PathBuf>,
    pub artwork: Artwork,
    pub level: u32,
}

#[derive(Debug, Clone)]
pub struct ConvertReport {
    pub output: PathBuf,
    pub disc_ids: Vec<String>,
    pub stats: PsarStats,
    pub file_size: u64,
    pub elapsed: Duration,
}

/// Probed facts about a disc image, for pre-filling the UI.
#[derive(Debug, Clone)]
pub struct DiscSummary {
    pub path: PathBuf,
    pub disc_id: Option<String>,
    pub volume_id: Option<String>,
    pub region: Region,
    /// Title from the built-in [`gamedb`], if the ID is known.
    pub title: Option<String>,
    pub tracks: usize,
    pub audio_tracks: usize,
    pub size: u64,
}

pub fn probe_disc(path: &Path) -> Result<DiscSummary> {
    let img = DiscImage::open(path)?;
    let info = img.probe();
    Ok(DiscSummary {
        path: path.to_path_buf(),
        region: info.disc_id.as_deref().map_or(Region::Unknown, Region::from_disc_id),
        title: info.disc_id.as_deref().and_then(gamedb::lookup).map(|g| g.title.clone()),
        disc_id: info.disc_id,
        volume_id: info.volume_id,
        tracks: img.tracks.len(),
        audio_tracks: img.tracks.iter().filter(|t| !t.kind.is_data()).count(),
        size: img.size_bytes(),
    })
}

/// `<dir>/<DISC_ID>/EBOOT.PBP` — the layout `ms0:/PSP/GAME/` expects.
pub fn default_output(dir: &Path, disc_id: &str) -> PathBuf {
    dir.join(disc_id).join("EBOOT.PBP")
}

/// Scratch directory for media intermediates (SND0 encoding), placed next to `output` so heavy
/// temporaries land on the destination volume instead of the system temp folder.
pub fn work_dir(output: &Path) -> PathBuf {
    output.parent().unwrap_or(Path::new(".")).join(format!(".psx2psp-work-{}", std::process::id()))
}

fn normalise_id(id: &str) -> String {
    id.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_uppercase()
}

/// Atomic output: everything goes to `<output>.part`; renamed on success, removed on failure.
struct PartFile {
    part: PathBuf,
    done: bool,
}

impl PartFile {
    fn new(output: &Path) -> Self {
        let mut p = output.as_os_str().to_owned();
        p.push(".part");
        Self { part: PathBuf::from(p), done: false }
    }

    fn commit(mut self, output: &Path) -> Result<()> {
        // `rename` replaces an existing target atomically (MoveFileExW REPLACE_EXISTING on
        // Windows, rename(2) on Unix): the old EBOOT survives if the swap fails.
        std::fs::rename(&self.part, output).map_err(psx2psp_core::Error::io_path(output))?;
        self.done = true;
        Ok(())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.done {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

/// `std::fs::write` through a [`PartFile`]: a failed write never leaves a truncated `path`.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let part = PartFile::new(path);
    std::fs::write(&part.part, data).map_err(psx2psp_core::Error::io_path(&part.part))?;
    part.commit(path)
}

/// Hits per rule of each of `ps` over the whole image (one read), with the `base` PPFs applied
/// first, in order; nothing is written.
fn dry_run(img: &DiscImage, base: &[&PpfPatch], ps: &[PatternPatch]) -> Result<Vec<Vec<usize>>> {
    const CHUNK: usize = 512 * patching::sector::SECTOR;
    let (mut rd, total, mut off) = (img.reader(), img.size_bytes(), 0u64);
    let mut buf = vec![0u8; CHUNK];
    let mut hits: Vec<Vec<usize>> = ps.iter().map(|p| vec![0; p.rules.len()]).collect();
    while off < total {
        let len = CHUNK.min((total - off) as usize);
        rd.read_exact(&mut buf[..len])?;
        base.iter().for_each(|p| p.apply(off, &mut buf[..len]));
        for (h, p) in hits.iter_mut().zip(ps) {
            h.iter_mut().zip(p.hits(&buf[..len])).for_each(|(h, k)| *h += k);
        }
        off += len as u64;
    }
    Ok(hits)
}

/// Hits of each of `patches` over the image (0 for a PPF patch).
fn patch_hits(img: &DiscImage, base: &[&PpfPatch], patches: &[&Patch]) -> Result<Vec<usize>> {
    Ok(patchdb::hits_per_patch(patches, &dry_run(img, base, &patchdb::patterns("", patches))?))
}

/// Whether `patch`'s PPF2/3 validation block (if any) matches the unpatched image.
fn ppf_validates(img: &DiscImage, patch: &PpfPatch) -> Result<bool> {
    let Some((at, block)) = &patch.validation else { return Ok(true) };
    let mut rd = img.reader();
    let mut buf = vec![0u8; block.len()];
    rd.seek(SeekFrom::Start(*at))?;
    rd.read_exact(&mut buf)?;
    Ok(patch.validates(&buf))
}

/// Parses and validates disc `n`'s PPF against its image.
fn load_ppf(img: &DiscImage, p: &Path, n: usize) -> Result<PpfPatch> {
    let bytes = std::fs::read(p).map_err(psx2psp_core::Error::io_path(p))?;
    let patch = PpfPatch::parse(&bytes).map_err(|source| EngineError::Ppf { path: p.display().to_string(), source })?;
    if !ppf_validates(img, &patch)? {
        return Err(EngineError::Input(format!("{}: PPF validation block does not match disc {} — wrong image/version", p.display(), n + 1)));
    }
    Ok(patch)
}

/// MD5 of track 1's raw sectors, from its INDEX 01 to the next track's first index (or the end
/// of the image): the key pop-fe gives its per-dump PPF fixes (bchunk `md5(1)`). Reads the
/// whole track.
pub fn track1_md5(img: &DiscImage) -> Result<String> {
    let start = img.tracks.first().map_or(0, |t| t.start);
    let end = img.tracks.get(1).map_or(img.sectors(), |t| t.index0.unwrap_or(t.start));
    let mut rd = img.reader();
    rd.seek(SeekFrom::Start(u64::from(start) * SECTOR as u64))?;
    let (mut h, mut left) = (md5::Md5::default(), u64::from(end.saturating_sub(start)) * SECTOR as u64);
    let mut buf = vec![0u8; 512 * SECTOR];
    while left > 0 {
        let n = buf.len().min(left as usize);
        rd.read_exact(&mut buf[..n])?;
        h.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(h.hex())
}

/// [`track1_md5`], only if one of `patches` is limited to certain dumps.
fn dump_md5(img: &DiscImage, patches: &[&Patch]) -> Result<Option<String>> {
    patches.iter().any(|p| !p.md5.is_empty()).then(|| track1_md5(img)).transpose()
}

/// Why database patch `p` does not fit this dump, if it does not.
fn misfit(img: &DiscImage, p: &Patch, md5: Option<&str>) -> Result<Option<&'static str>> {
    if !p.for_dump(|| md5.expect("computed when a patch lists MD5s").to_owned()) {
        return Ok(Some("made for another dump of the game (track 1 MD5)"));
    }
    match p.ppf() {
        Some(f) if !ppf_validates(img, &f)? => Ok(Some("its PPF validation block does not match this image")),
        _ => Ok(None),
    }
}

/// Hits of every patch `db` has for `id` on this disc, in [`PatchDb::patches_for`] order: what
/// the GUI shows next to each patch. A PPF patch counts 1 when it fits the dump; the patterns
/// are matched with those PPFs and then the user's `ppf` applied. Reads the whole image: call it
/// off the UI thread.
pub fn scan_patches(path: &Path, ppf: Option<&Path>, id: &str, db: &PatchDb) -> Result<Vec<(String, usize)>> {
    let ps = db.patches_for(&db.pops_id(id));
    if ps.is_empty() {
        return Ok(Vec::new());
    }
    let img = DiscImage::open(path)?;
    let user = ppf.map(|p| load_ppf(&img, p, 0)).transpose()?;
    let md5 = dump_md5(&img, &ps)?;
    let mut fits = Vec::with_capacity(ps.len());
    for p in &ps {
        fits.push(misfit(&img, p, md5.as_deref())?.is_none());
    }
    let db_ppfs: Vec<PpfPatch> = ps.iter().zip(&fits).filter(|(_, f)| **f).filter_map(|(p, _)| p.ppf()).collect();
    let base: Vec<&PpfPatch> = db_ppfs.iter().chain(&user).collect();
    let hits = patch_hits(&img, &base, &ps)?;
    Ok(ps.iter().zip(fits).zip(hits).map(|((p, fit), h)| (p.name.clone(), if p.pattern().is_none() { usize::from(fit) } else if fit { h } else { 0 })).collect())
}

/// What [`title_fixes_for`] selected for a disc.
struct TitleFixes {
    config: Option<Vec<u8>>,
    /// Database PPF fixes, in database order; applied before the user's PPF.
    ppfs: Vec<PpfPatch>,
    patterns: Vec<PatternPatch>,
}

/// Per-game data for disc `n` ([`patchdb`]): the POPS config (a `<image>.pspconfig` beside the
/// disc wins over the database) and the selected patches. PPF fixes must fit the dump (track-1
/// MD5, validation block); patterns are verified by a dry run over the image with those PPFs and
/// then the user's `ppf` applied, and logged with their hit count. Patches that do not fit or hit
/// nothing (another build of the game) are dropped.
fn title_fixes_for(d: &DiscInput, db: &PatchDb, img: &DiscImage, ppf: Option<&PpfPatch>, id: &str, n: usize, monitor: &dyn Monitor) -> Result<TitleFixes> {
    let disc = n + 1;
    let sw = d.switches();
    let origin = |o| if o == patchdb::Origin::User { " [user]" } else { "" };
    let sidecar = d.path.with_extension("pspconfig");
    let config = if sidecar.is_file() {
        let c = std::fs::read(&sidecar).map_err(psx2psp_core::Error::io_path(&sidecar))?;
        if c.is_empty() || c.len() > psar::POPS_CONFIG_MAX {
            return Err(EngineError::Input(format!("{}: a POPS config is 1..={} bytes, this is {}", sidecar.display(), psar::POPS_CONFIG_MAX, c.len())));
        }
        monitor.log(&format!("Disc {disc}: POPS config from {} ({} B)", sidecar.display(), c.len()));
        Some(c)
    } else if let Some(g) = db.game(id).filter(|g| g.config.is_some()) {
        let c = g.config.clone().expect("filtered");
        if patchdb::config_wanted(sw, &d.patch_choice) {
            monitor.log(&format!("Disc {disc}: POPS config for {id} — {} ({} B){}", g.source, c.len(), origin(g.origin)));
            Some(c)
        } else {
            monitor.log(&format!("Disc {disc}: POPS config for {id} available but off"));
            None
        }
    } else {
        None
    };

    let all = db.patches_for(id);
    for (name, _) in &d.patch_choice {
        if !name.eq_ignore_ascii_case(patchdb::CONFIG) && !all.iter().any(|p| p.name.eq_ignore_ascii_case(name)) {
            monitor.log(&format!("Disc {disc}: no patch named “{name}” for {id}"));
        }
    }
    for (on, kind, what) in [(sw.fps, Kind::Fps, "frame-rate unlock"), (sw.ntsc60, Kind::Ntsc60, "native 60 Hz switch")] {
        if on && !all.iter().any(|p| p.kind == kind) {
            monitor.log(&format!("Disc {disc}: no {what} is known for {id}"));
        }
    }
    let (wanted, off): (Vec<&Patch>, Vec<&Patch>) = all.into_iter().partition(|p| p.wanted(sw, &d.patch_choice));
    for p in off {
        monitor.log(&format!("Disc {disc}: patch available, off — {} ({}){}", p.name, p.kind, origin(p.origin)));
    }
    if wanted.is_empty() {
        return Ok(TitleFixes { config, ppfs: Vec::new(), patterns: Vec::new() });
    }
    let md5 = dump_md5(img, &wanted)?;
    if let Some(m) = &md5 {
        monitor.log(&format!("Disc {disc}: track 1 MD5 {m}"));
    }
    let (mut ppfs, mut fitting) = (Vec::new(), Vec::new());
    for p in wanted {
        if let Some(why) = misfit(img, p, md5.as_deref())? {
            monitor.log(&format!("Disc {disc}: patch skipped — {}: {why}", p.name));
        } else if let Some(f) = p.ppf() {
            monitor.log(&format!("Disc {disc}: patch — {} (PPF, {} records){}", p.name, f.record_count(), origin(p.origin)));
            ppfs.push(f);
        } else {
            fitting.push(p);
        }
    }
    let base: Vec<&PpfPatch> = ppfs.iter().chain(ppf).collect();
    let mut applied = Vec::new();
    for (p, h) in fitting.iter().zip(patch_hits(img, &base, &fitting)?) {
        if h > 0 {
            monitor.log(&format!("Disc {disc}: patch — {} ×{h}{}", p.name, origin(p.origin)));
            applied.push(*p);
        } else {
            monitor.log(&format!("Disc {disc}: patch NOT found — {} (different build of the game?)", p.name));
        }
    }
    // A 60 Hz patch leaves the POPS config as it is. pop-fe's `force_ntsc` bit (0x0B/0x8F |=
    // 0x10) is meant for PAL games left at 50 Hz; on top of a game that already draws its own
    // 240-line NTSC picture it shifts the image again in Full Screen (bottom cut, garbage on top:
    // real PSP, Sheep Dog 'n' Wolf).
    Ok(TitleFixes { config, ppfs, patterns: patchdb::patterns(&format!("{id} patches"), &applied) })
}

/// PAL→NTSC for disc `n`, decided per game: a dry run over the image (with its PPFs applied)
/// counts every rule, and the preset is applied only when it can switch the video mode
/// ([`patching::pal_to_ntsc_applies`]). `None` = disc left as is (logged with the reason).
fn pal_ntsc_for(img: &DiscImage, base: &[&PpfPatch], id: &str, n: usize, monitor: &dyn Monitor) -> Result<Option<PatternPatch>> {
    if Region::from_disc_id(id) != Region::Pal {
        monitor.log(&format!("Disc {}: PAL→NTSC requested on a non-PAL ID ({id})", n + 1));
    }
    let p = patching::pal_to_ntsc();
    let hits = dry_run(img, base, std::slice::from_ref(&p))?.swap_remove(0);
    let found: Vec<String> = patching::PAL_TO_NTSC_RULES.iter().zip(&hits).filter(|(_, h)| **h > 0).map(|(r, h)| format!("{r} ×{h}")).collect();
    let found = if found.is_empty() { "no rule matches".to_owned() } else { found.join(", ") };
    if patching::pal_to_ntsc_applies(&hits) {
        monitor.log(&format!("Disc {}: PAL→NTSC applied — {found}", n + 1));
        Ok(Some(p))
    } else {
        monitor.log(&format!(
            "Disc {}: PAL→NTSC NOT applied — {found}. Without a video-mode fix the Y fix alone would \
             only shift a 50 Hz picture; this game needs a title-specific patch",
            n + 1
        ));
        Ok(None)
    }
}

/// Executable CRCs of disc `n` that its patches (PPFs first, then `code`, in stream order) would
/// break ([`patching::exe_crc`]): the loader of some games refuses such an executable and retries
/// forever. `None` when nothing needs fixing.
fn exe_crc_for(img: &DiscImage, base: &[&PpfPatch], code: Vec<&PatternPatch>, n: usize, monitor: &dyn Monitor) -> Result<Option<SectorWrites>> {
    if base.is_empty() && code.is_empty() {
        return Ok(None);
    }
    let fix = exe_crc::plan(img, |lba, s| {
        base.iter().for_each(|p| p.apply(u64::from(lba) * SECTOR as u64, s));
        if let Some(r) = sector::user_data_range(sector::mode(s)) {
            code.iter().for_each(|p| _ = p.apply_user(&mut s[r.clone()]));
        }
    })?;
    if fix.files.is_empty() {
        return Ok(None);
    }
    let names: Vec<&str> = fix.files.iter().map(|f| f.trim_start_matches('/')).collect();
    monitor.log(&format!("Disc {}: loader CRC updated in {} patched executable(s) — {}", n + 1, names.len(), names.join(", ")));
    Ok(Some(fix.writes))
}

/// The disc's LibCrypt subchannel table. A `.sbi`/`.lsd` beside the image is the dump of the
/// real disc and wins: the magic word only says which bits are set, and some discs (MediEvil
/// SCES-01494) damage other sectors than the 32 the magic-word model assumes.
fn subchannels_for(d: &DiscInput, img: &DiscImage, magic: Option<u16>, n: usize, monitor: &dyn Monitor) -> Result<Option<Vec<u8>>> {
    if let Some(r) = subq::load_beside(&d.path, &img.tracks)? {
        let name = r.path.file_name().map_or_else(|| r.path.display().to_string(), |f| f.to_string_lossy().into_owned());
        monitor.log(&format!("Disc {}: LibCrypt — subchannel table from {name} ({} sectors, {} damaged)", n + 1, r.sectors, r.damaged));
        return Ok(Some(r.table));
    }
    Ok(magic.map(|magic| {
        monitor.log(&format!(
            "Disc {}: LibCrypt — subchannel table for magic word {magic:#06x}. No .sbi beside the disc: if the game \
             freezes after it starts, put the redump .sbi next to the CUE and rebuild",
            n + 1
        ));
        psar::libcrypt_subchannels(magic)
    }))
}

/// ConvertIsoToPbp: discs (+patches, +media) → EBOOT.PBP.
pub fn convert(req: &ConvertRequest, monitor: &dyn Monitor) -> Result<ConvertReport> {
    let t0 = Instant::now();
    if req.discs.is_empty() {
        return Err(EngineError::Input("no disc image selected".into()));
    }
    if req.title.trim().is_empty() {
        return Err(EngineError::Input("title is empty".into()));
    }
    monitor.log(&match &req.donor {
        Some(p) => format!("Loading donor {}", p.display()),
        None => "Using built-in donor".to_owned(),
    });
    let donor = load_donor(req.donor.as_deref())?;
    monitor.log(&format!("Donor OK — DATA.PSP {} B, boot PNG {} B, PGD {} B", donor.data_psp.len(), donor.boot_png.len(), donor.pgd.len()));

    let mut images = Vec::with_capacity(req.discs.len());
    let mut ids = Vec::with_capacity(req.discs.len());
    let mut ppfs: Vec<Vec<PpfPatch>> = Vec::with_capacity(req.discs.len());
    let mut pals: Vec<Option<PatternPatch>> = Vec::with_capacity(req.discs.len());
    let mut fixes: Vec<Vec<PatternPatch>> = Vec::with_capacity(req.discs.len());
    let mut configs: Vec<Option<Vec<u8>>> = Vec::with_capacity(req.discs.len());
    let mut subqs: Vec<Option<Vec<u8>>> = Vec::with_capacity(req.discs.len());
    let mut crcs: Vec<Option<SectorWrites>> = Vec::with_capacity(req.discs.len());
    for (n, d) in req.discs.iter().enumerate() {
        let img = DiscImage::open(&d.path)?;
        let disc_id = match &d.disc_id {
            Some(id) if !id.trim().is_empty() => normalise_id(id),
            _ => img.probe().disc_id.ok_or_else(|| {
                EngineError::Input(format!("disc {}: no SYSTEM.CNF game ID found — enter it manually", n + 1))
            })?,
        };
        monitor.log(&format!("Disc {}: {} — {} tracks, {:.1} MB, ID {disc_id}", n + 1, d.path.display(), img.tracks.len(), img.size_bytes() as f64 / 1e6));
        let id = req.patches.pops_id(&disc_id);
        if id != disc_id {
            monitor.log(&format!("Disc {}: built as {id}, the ID POPS needs for this game (patch database)", n + 1));
        }
        let user_ppf = match &d.ppf {
            Some(p) => {
                let patch = load_ppf(&img, p, n)?;
                monitor.log(&format!("Disc {}: {}", n + 1, patch.describe()));
                Some(patch)
            }
            None => None,
        };
        let TitleFixes { config, ppfs: mut ppf, patterns: fix } = title_fixes_for(d, &req.patches, &img, user_ppf.as_ref(), &id, n, monitor)?;
        // Database PPF fixes first (they expect the original dump), then the user's.
        ppf.extend(user_ppf);
        let base: Vec<&PpfPatch> = ppf.iter().collect();
        let pal = if d.pal_ntsc { pal_ntsc_for(&img, &base, &id, n, monitor)? } else { None };
        crcs.push(exe_crc_for(&img, &base, pal.iter().chain(&fix).collect(), n, monitor)?);
        subqs.push(subchannels_for(d, &img, req.patches.libcrypt(&id), n, monitor)?);
        pals.push(pal);
        configs.push(config);
        fixes.push(fix);
        images.push(img);
        ids.push(id);
        ppfs.push(ppf);
    }

    let multi = images.len() > 1;
    let job = PsarJob {
        discs: (0..images.len())
            .map(|i| DiscSpec {
                image: &images[i],
                disc_id: ids[i].clone(),
                title: if multi { format!("{} (Disc {})", req.title, i + 1) } else { req.title.clone() },
                // PPFs first (they fix the image), then the code patches that expect their
                // result, then the executable CRCs computed over all of them.
                patches: ppfs[i]
                    .iter()
                    .map(|p| p as &dyn ImagePatch)
                    .chain(pals[i].iter().map(|p| p as &dyn ImagePatch))
                    .chain(fixes[i].iter().map(|p| p as &dyn ImagePatch))
                    .chain(crcs[i].iter().map(|p| p as &dyn ImagePatch))
                    .collect(),
                pops_config: configs[i].as_deref(),
                subchannels: subqs[i].as_deref(),
            })
            .collect(),
        title: req.title.clone(),
        level: req.level,
        boot_png: req.artwork.boot.as_deref().unwrap_or(&donor.boot_png),
        donor: &donor,
    };

    // Like PSX2PSP: the XMB/save ID comes from the game DB, so disc 2+ of a set and compilation
    // parts share disc 1's save folder.
    let save_id = gamedb::lookup(&ids[0]).map(|g| g.save_id.clone()).filter(|s| s.len() == 9 && s[..4].bytes().all(|b| b.is_ascii_uppercase()) && s[4..].bytes().all(|b| b.is_ascii_digit())).unwrap_or_else(|| ids[0].clone());
    if save_id != ids[0] {
        monitor.log(&format!("Save ID {save_id} (shared with the first disc of the set, per game DB)"));
    }
    let sfo = Sfo::pops(&req.title, &save_id);
    let sections = PbpSections {
        param_sfo: sfo.to_bytes(),
        icon0: req.artwork.icon0.clone(),
        icon1: req.artwork.icon1_pmf.clone(),
        pic0: req.artwork.pic0.clone(),
        pic1: req.artwork.pic1.clone(),
        snd0: req.artwork.snd0.clone(),
        data_psp: donor.data_psp.clone(),
    };

    if let Some(dir) = req.output.parent() {
        std::fs::create_dir_all(dir).map_err(psx2psp_core::Error::io_path(dir))?;
    }
    let part = PartFile::new(&req.output);
    let file = File::create(&part.part).map_err(psx2psp_core::Error::io_path(&part.part))?;
    let mut w = BufWriter::with_capacity(8 << 20, file);
    let mut stats = PsarStats::default();
    monitor.log(&format!("Compressing {} disc(s) at level {} on {} threads", images.len(), req.level.clamp(1, 9), std::thread::available_parallelism().map_or(1, |n| n.get())));
    write_pbp(&mut w, &sections, PSAR_ALIGN, |w, base| {
        stats = psar::write_psar(w, base, &job, monitor)?;
        Ok(())
    })?;
    let file = w.into_inner().map_err(|e| EngineError::from(e.into_error()))?;
    file.sync_all()?;
    let file_size = file.metadata()?.len();
    drop(file);
    part.commit(&req.output)?;
    for (n, p) in pals.iter().enumerate() {
        if let Some(p) = p {
            let k = p.sectors_patched();
            monitor.log(&if k == 0 {
                format!("Disc {}: PAL→NTSC matched nothing — this title needs a game-specific PPF", n + 1)
            } else {
                format!("Disc {}: PAL→NTSC patched {k} sector(s)", n + 1)
            });
        }
    }
    for (n, f) in fixes.iter().enumerate().filter(|(_, f)| !f.is_empty()) {
        monitor.log(&format!("Disc {}: game patches changed {} sector(s)", n + 1, f.iter().map(PatternPatch::sectors_patched).sum::<usize>()));
    }

    let report = ConvertReport { output: req.output.clone(), disc_ids: ids, stats, file_size, elapsed: t0.elapsed() };
    monitor.log(&format!(
        "Done: {} ({:.1} MB, {:.1}% of raw) in {:.2}s",
        report.output.display(),
        file_size as f64 / 1e6,
        stats.stored_bytes as f64 * 100.0 / stats.raw_bytes.max(1) as f64,
        report.elapsed.as_secs_f64()
    ));
    Ok(report)
}

/// What's inside an existing EBOOT (for the UI's import and the CLI's `info`).
#[derive(Debug, Clone)]
pub struct PbpSummary {
    pub sfo: Sfo,
    pub sections: Vec<(Section, u64)>,
    pub discs: Vec<PsarDisc>,
    pub icon0: Option<Vec<u8>>,
    pub pic0: Option<Vec<u8>>,
    pub pic1: Option<Vec<u8>>,
    pub boot: Option<Vec<u8>>,
}

pub fn inspect(path: &Path) -> Result<PbpSummary> {
    let f = File::open(path).map_err(psx2psp_core::Error::io_path(path))?;
    let mut pbp = PbpFile::open(BufReader::new(f))?;
    let sfo = Sfo::parse(&pbp.read(Section::ParamSfo)?)?;
    let sections = Section::ALL.iter().map(|&s| (s, pbp.range(s).end - pbp.range(s).start)).collect();
    let opt = |v: Vec<u8>| (!v.is_empty()).then_some(v);
    let icon0 = opt(pbp.read(Section::Icon0)?);
    let pic0 = opt(pbp.read(Section::Pic0)?);
    let pic1 = opt(pbp.read(Section::Pic1)?);
    let psar_at = pbp.range(Section::DataPsar).start;
    let discs = psar::read_discs(pbp.inner_mut(), psar_at).unwrap_or_default();
    pbp.inner_mut().rewind()?;
    let boot = Donor::from_pbp(&mut pbp).ok().map(|d| d.boot_png);
    Ok(PbpSummary { sfo, sections, discs, icon0, pic0, pic1, boot })
}

/// ExtractPbp: EBOOT.PBP → `<id>.bin`/`.cue` per disc + every embedded asset.
///
/// Never overwrites: if any file it would write already exists in `out_dir`, nothing is written.
/// Every file goes through a `.part` file, so a failure or cancel leaves no truncated output.
pub fn extract(path: &Path, out_dir: &Path, monitor: &dyn Monitor) -> Result<Vec<PathBuf>> {
    let f = File::open(path).map_err(psx2psp_core::Error::io_path(path))?;
    let mut pbp = PbpFile::open(BufReader::with_capacity(1 << 20, f))?;
    let mut assets = Vec::new();
    for s in Section::ALL {
        if s == Section::DataPsar {
            continue;
        }
        let data = pbp.read(s)?;
        if !data.is_empty() {
            assets.push((out_dir.join(s.file_name()), data));
        }
    }
    let psar_at = pbp.range(Section::DataPsar).start;
    let discs = psar::read_discs(pbp.inner_mut(), psar_at)?;
    let multi = discs.len() > 1;
    let stems: Vec<String> =
        discs.iter().enumerate().map(|(i, d)| if multi { format!("{} (Disc {})", d.disc_id, i + 1) } else { d.disc_id.clone() }).collect();

    let targets = assets.iter().map(|(p, _)| p.clone()).chain(stems.iter().flat_map(|s| ["bin", "cue"].map(|e| out_dir.join(format!("{s}.{e}")))));
    if let Some(p) = targets.into_iter().find(|p| p.exists()) {
        return Err(EngineError::Input(format!("{} already exists: extract into an empty folder, or move it away first", p.display())));
    }

    std::fs::create_dir_all(out_dir).map_err(psx2psp_core::Error::io_path(out_dir))?;
    let mut produced = Vec::new();
    for (p, data) in assets {
        write_atomic(&p, &data)?;
        produced.push(p);
    }
    for ((i, d), stem) in discs.iter().enumerate().zip(&stems) {
        let bin = out_dir.join(format!("{stem}.bin"));
        let part = PartFile::new(&bin);
        let mut w = BufWriter::with_capacity(8 << 20, File::create(&part.part).map_err(psx2psp_core::Error::io_path(&part.part))?);
        monitor.log(&format!("Extracting disc {} ({}) → {}", i + 1, d.disc_id, bin.display()));
        psar::extract_disc(pbp.inner_mut(), psar_at, d, &mut w, monitor)?;
        w.flush()?;
        drop(w);
        part.commit(&bin)?;
        let cue = out_dir.join(format!("{stem}.cue"));
        write_atomic(&cue, psar::toc_to_cue(&format!("{stem}.bin"), &d.tracks()).as_bytes())?;
        produced.push(bin);
        produced.push(cue);
    }
    Ok(produced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn bin_and_cue_together_are_one_disc() {
        let t = tempfile::tempdir().unwrap();
        let bin = touch(t.path(), "Sheep Dog.bin", "");
        let cue = touch(t.path(), "Sheep Dog.cue", "FILE \"Sheep Dog.bin\" BINARY\n  TRACK 01 MODE2/2352\n");
        for order in [vec![bin.clone(), cue.clone()], vec![cue.clone(), bin.clone()]] {
            assert_eq!(resolve_discs(&[], &order).discs, vec![cue.clone()]);
        }
        // Dropped one at a time (one event per file): the second drop adds nothing.
        let first = resolve_discs(&[], std::slice::from_ref(&bin));
        assert_eq!(first.discs, vec![cue.clone()], "a lone BIN is upgraded to its CUE");
        assert_eq!(resolve_discs(&first.discs, std::slice::from_ref(&cue)).discs, vec![cue.clone()]);
    }

    #[test]
    fn multi_track_bins_collapse_and_discs_sort_naturally() {
        let t = tempfile::tempdir().unwrap();
        let mut picked = Vec::new();
        for d in [10, 2, 1] {
            let sheet = format!("FILE \"G (Disc {d}) (Track 1).bin\" BINARY\nFILE \"G (Disc {d}) (Track 2).bin\" BINARY\n");
            picked.push(touch(t.path(), &format!("G (Disc {d}).cue"), &sheet));
            picked.push(touch(t.path(), &format!("G (Disc {d}) (Track 1).bin"), ""));
            picked.push(touch(t.path(), &format!("G (Disc {d}) (Track 2).bin"), ""));
        }
        let names: Vec<String> = resolve_discs(&[], &picked).discs.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["G (Disc 1).cue", "G (Disc 2).cue", "G (Disc 10).cue"]);
    }

    #[test]
    fn m3u_keeps_playlist_order_and_plain_bins_stay() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "b.cue", "FILE b.bin BINARY\n");
        let b = touch(t.path(), "a.cue", "FILE a.bin BINARY\n");
        let m3u = touch(t.path(), "game.m3u", "# discs\nb.cue\n\na.cue\n");
        let lone = touch(t.path(), "lone.img", "");
        let plan = resolve_discs(&[], &[m3u, lone.clone()]);
        assert_eq!(plan.discs, vec![t.path().join("b.cue"), t.path().join("a.cue"), lone]);
        assert_eq!((path_key(&plan.discs[0]), path_key(&plan.discs[1])), (path_key(&a), path_key(&b)));
    }

    #[test]
    fn builtin_donor_is_complete() {
        let d = builtin_donor().unwrap();
        assert!(d.data_psp.starts_with(b"\x7FELF") && d.data_psp.len() == 19_002);
        assert!(d.boot_png.starts_with(b"\x89PNG") && d.boot_png.len() == 0x2CE1);
        assert!(d.pgd.starts_with(b"\0PGD"));
        // 0x50 + PNG = the constant 0x2D31 popstation adds to p1 for p2.
        assert_eq!(0x50 + d.boot_png.len(), 0x2D31);
    }

    #[test]
    fn outputs_follow_psp_layout_and_work_dir_stays_beside_them() {
        let out = default_output(Path::new("D:/games"), "SLUS00594");
        assert_eq!(out, Path::new("D:/games/SLUS00594/EBOOT.PBP"));
        assert_eq!(work_dir(&out).parent(), Some(Path::new("D:/games/SLUS00594")));
    }

    /// 40-sector PAL disc with the Y Fix #2 sequence in sector 30 (plus Graphic Fix #2 when
    /// `video_mode`), converted with/without PAL→NTSC and extracted back.
    fn pal_ntsc_run(video_mode: bool) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use patching::sector::build_mode2_form1;
        const YFIX: [u8; 8] = [0x13, 0x00, 0x90, 0x24, 0x10, 0x00, 0x90, 0x24];
        const GFIX: [u8; 8] = [0x06, 0x80, 0x02, 0x3C, 0x18, 0xF5, 0x42, 0x8C];
        let t = tempfile::tempdir().unwrap();
        let mut raw = Vec::new();
        for lba in 0..40u32 {
            let mut user = [0u8; 2048];
            user[..4].copy_from_slice(&lba.to_le_bytes());
            if lba == 30 {
                user[128..136].copy_from_slice(&YFIX);
                if video_mode {
                    user[512..520].copy_from_slice(&GFIX);
                }
            }
            raw.extend_from_slice(&build_mode2_form1(lba, &user));
        }
        let bin = t.path().join("g.bin");
        std::fs::write(&bin, &raw).unwrap();
        let cue = touch(t.path(), "g.cue", "FILE \"g.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n");
        let run = |pal_ntsc: bool, name: &str| {
            let output = t.path().join(name).join("EBOOT.PBP");
            let disc = DiscInput { path: cue.clone(), disc_id: Some("SLES00001".into()), pal_ntsc, ..Default::default() };
            let req = ConvertRequest { discs: vec![disc], patches: PatchDb::default(), title: "T".into(), output: output.clone(), donor: None, artwork: Artwork::default(), level: 1 };
            convert(&req, &()).unwrap();
            let out = t.path().join(name).join("x");
            extract(&output, &out, &()).unwrap();
            std::fs::read(out.join("SLES00001.bin")).unwrap()
        };
        let (plain, patched) = (run(false, "a"), run(true, "b"));
        assert_eq!(std::fs::read(&bin).unwrap(), raw, "source BIN untouched");
        (raw, plain, patched)
    }

    #[test]
    fn pal_ntsc_is_applied_in_the_pbp_and_source_stays_intact() {
        use patching::sector::{SECTOR, verify_edc};
        let (raw, plain, patched) = pal_ntsc_run(true);
        assert_eq!(&plain[..raw.len()], &raw[..], "no patch → bit-identical");
        let s30: &[u8; SECTOR] = patched[30 * SECTOR..31 * SECTOR].try_into().unwrap();
        assert_eq!(&s30[24 + 128..24 + 136], &[0x00, 0x00, 0x90, 0x24, 0x00, 0x00, 0x90, 0x24]);
        assert_eq!(&s30[24 + 512..24 + 520], &[0x00, 0x00, 0x02, 0x20, 0, 0, 0, 0]);
        assert!(verify_edc(s30), "EDC/ECC regenerated");
        assert_eq!(&patched[..30 * SECTOR], &raw[..30 * SECTOR]);
        assert_eq!(&patched[31 * SECTOR..raw.len()], &raw[31 * SECTOR..]);
    }

    #[test]
    fn pal_ntsc_skips_y_fix_without_video_mode_code() {
        let (raw, _, patched) = pal_ntsc_run(false);
        assert_eq!(&patched[..raw.len()], &raw[..], "Sheep Dog case: requested but not applicable → bit-identical");
    }

    /// Sheep Dog's two code sites in sector 20 and its boot flags in sector 21 of a fake
    /// SLES-02895, converted with title fixes, fps unlock and 60 Hz: the PBP carries the built-in
    /// POPS config with its NTSC flag (or the `.pspconfig` sidecar) and the extracted image has
    /// exactly the three instructions rewritten.
    #[test]
    fn title_fixes_patch_code_and_embed_pops_config() {
        use patching::sector::{SECTOR, build_mode2_form1, verify_edc};
        const CLAMP: [u8; 16] = [0xAB, 0x00, 0x81, 0x28, 0x03, 0x00, 0x20, 0x14, 0, 0, 0, 0, 0xAA, 0x00, 0x04, 0x24];
        const CAP: [u8; 24] = [0xC0, 0x47, 0x02, 0x0C, 0x02, 0x00, 0x04, 0x24, 0x03, 0x00, 0x00, 0x10, 0, 0, 0, 0, 0xC0, 0x47, 0x02, 0x0C, 0x21, 0x20, 0x00, 0x00];
        const BOOT: [u8; 16] = [1, 0, 8, 0x24, 0xFE, 0xFF, 4, 0x24, 1, 0, 5, 0x24, 0x10, 0, 3, 0x24];
        let t = tempfile::tempdir().unwrap();
        let mut raw = Vec::new();
        for lba in 0..30u32 {
            let mut user = [0u8; 2048];
            user[..4].copy_from_slice(&lba.to_le_bytes());
            if lba == 20 {
                user[0x1E4..0x1F4].copy_from_slice(&CLAMP);
                user[0x754..0x76C].copy_from_slice(&CAP);
            }
            if lba == 21 {
                user[8..24].copy_from_slice(&BOOT);
            }
            raw.extend_from_slice(&build_mode2_form1(lba, &user));
        }
        std::fs::write(t.path().join("s.bin"), &raw).unwrap();
        let cue = touch(t.path(), "s.cue", "FILE \"s.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n");
        let run = |name: &str, title_fixes: bool, fps_unlock: bool, ntsc_60hz: bool| {
            let output = t.path().join(name).join("EBOOT.PBP");
            let disc = DiscInput { path: cue.clone(), disc_id: Some("SLES-02895".into()), title_fixes, fps_unlock, ntsc_60hz, ..Default::default() };
            convert(&ConvertRequest { discs: vec![disc], patches: PatchDb::builtin().clone(), title: "T".into(), output: output.clone(), donor: None, artwork: Artwork::default(), level: 1 }, &()).unwrap();
            let out = t.path().join(name).join("x");
            extract(&output, &out, &()).unwrap();
            let pbp = std::fs::read(&output).unwrap();
            let psar = PbpFile::open(std::io::Cursor::new(&pbp)).unwrap().range(Section::DataPsar).start as usize;
            let config = pbp[psar + psar::POPS_CONFIG_AT..psar + 0x800].to_vec();
            (std::fs::read(out.join("SLES02895.bin")).unwrap(), config)
        };

        let (off, cfg_off) = run("off", false, false, false);
        assert_eq!(&off[..raw.len()], &raw[..], "fixes off → bit-identical");
        assert!(cfg_off.iter().all(|&b| b == 0), "fixes off → no config");

        let (on, cfg_on) = run("on", true, true, true);
        let db_config = PatchDb::builtin().game("SLES02895").unwrap().config.clone().unwrap();
        assert_eq!(&cfg_on[..db_config.len()], &db_config[..], "60 Hz patch: config untouched, no POPS NTSC bit");
        // POPS config word 19 (cycle shift) at blob 0x0C + 4 * 19 and its mirror 0x84 further on.
        assert_eq!((&cfg_on[0x58..0x5C], &cfg_on[0xDC..0xE0]), (&[5u8, 0, 0, 0][..], &[5u8, 0, 0, 0][..]), "CPU cycle shift 5");
        let s20: &[u8; SECTOR] = on[20 * SECTOR..21 * SECTOR].try_into().unwrap();
        let w = |o: usize| u32::from_le_bytes(s20[24 + o..28 + o].try_into().unwrap());
        assert_eq!((w(0x1E4), w(0x1F0)), (0x2881_0100, 0x2404_00FF), "clamp 0xFF");
        assert_eq!(w(0x758), 0x2404_0000, "VSync(0)");
        assert!(verify_edc(s20));
        let s21: &[u8; SECTOR] = on[21 * SECTOR..22 * SECTOR].try_into().unwrap();
        assert_eq!(&s21[24 + 16..24 + 20], &[0, 0, 5, 0x24], "addiu a1,zero,0 (NTSC)");
        assert!(verify_edc(s21));
        let diff = on[..raw.len()].iter().zip(&raw).filter(|(a, b)| a != b).count();
        assert!(diff <= 6 + 2 * (4 + 276), "only 6 code bytes + EDC/ECC change ({diff} bytes)");

        // Without 60 Hz the config is the built-in one, untouched.
        let (_, cfg_50) = run("50", true, false, false);
        assert_eq!(&cfg_50[..272], &db_config[..]);

        // A sidecar config overrides the built-in one, even with the switch off.
        std::fs::write(t.path().join("s.pspconfig"), b"\x70custom").unwrap();
        let (_, cfg_side) = run("side", false, false, false);
        assert_eq!(&cfg_side[..7], b"\x70custom");
        assert!(cfg_side[7..].iter().all(|&b| b == 0));
    }

    /// pop-fe's per-game data on a fake two-track disc dumped as SCES-99998: the disc is built as
    /// its `pops_id`, carries the LibCrypt subchannel table of that ID's magic word, and of two
    /// PPF fixes only the one whose MD5 is this dump's track 1 (data track, not the audio after
    /// it) is applied.
    #[test]
    fn pops_id_libcrypt_and_md5_gated_ppf() {
        use patching::sector::build_mode2_form1;
        let t = tempfile::tempdir().unwrap();
        let mut raw: Vec<u8> = (0..20u32).flat_map(|lba| build_mode2_form1(lba, &[lba as u8; 2048])).collect();
        raw.extend((0..10 * SECTOR).map(|i| (i * 7) as u8)); // track 2: audio, pregap at 20, INDEX 01 at 22
        std::fs::write(t.path().join("g.bin"), &raw).unwrap();
        let cue = touch(t.path(), "g.cue", "FILE \"g.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 00 00:00:20\n    INDEX 01 00:00:22\n");
        let mut h = md5::Md5::default();
        h.update(&raw[..20 * SECTOR]);
        let md5 = h.hex();
        assert_eq!(track1_md5(&DiscImage::open(&cue).unwrap()).unwrap(), md5);

        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ");
        let (ours, theirs) = (10 * SECTOR + 24, 11 * SECTOR + 24);
        let ini = format!(
            "[game SCES-99998]\npops_id = SCPS-99998\n[game SCPS99998]\nlibcrypt = 0xA5C3\n\
             [patch SCPS99998]\nname = this dump\nkind = fix\nmd5 = {md5} {}\nppf = {}\n\
             [patch SCPS99998]\nname = another dump\nkind = fix\nmd5 = {}\nppf = {}\n",
            "0".repeat(32),
            hex(&patchdb::tests::ppf_bytes(&[(ours as u64, b"OURS")])),
            "f".repeat(32),
            hex(&patchdb::tests::ppf_bytes(&[(theirs as u64, b"THEIRS")])),
        );
        let db = PatchDb::parse(&ini, patchdb::Origin::User).unwrap();
        let output = t.path().join("o").join("EBOOT.PBP");
        let disc = DiscInput { path: cue.clone(), disc_id: Some("SCES-99998".into()), title_fixes: true, ..Default::default() };
        convert(&ConvertRequest { discs: vec![disc], patches: db.clone(), title: "T".into(), output: output.clone(), donor: None, artwork: Artwork::default(), level: 1 }, &()).unwrap();
        assert_eq!(scan_patches(&cue, None, "SCES99998", &db).unwrap().iter().map(|(_, h)| *h).collect::<Vec<_>>(), [1, 0]);

        let s = inspect(&output).unwrap();
        assert_eq!(s.discs[0].disc_id, "SCPS99998", "built as the pops_id");
        assert_eq!(s.discs[0].subchannels, psar::libcrypt_subchannels(0xA5C3));
        extract(&output, &t.path().join("x"), &()).unwrap();
        let out = std::fs::read(t.path().join("x").join("SCPS99998.bin")).unwrap();
        assert_eq!(&out[ours..ours + 4], b"OURS");
        assert_eq!(&out[theirs..theirs + 6], &raw[theirs..theirs + 6], "other dump's fix skipped");
        let diff = out[..raw.len()].iter().zip(&raw).filter(|(a, b)| a != b).count();
        assert_eq!(diff, 4, "only this dump's PPF bytes change");

        // A second extract into the same folder refuses before writing anything.
        let again = extract(&output, &t.path().join("x"), &()).unwrap_err().to_string();
        assert!(again.contains("already exists"), "{again}");
        assert_eq!(std::fs::read(t.path().join("x").join("SCPS99998.bin")).unwrap(), out);
        assert!(std::fs::read_dir(t.path().join("x")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".part")));

        // A redump .sbi beside the CUE is the real disc and wins over the magic word (MediEvil
        // SCES-01494 damages one sector of some pairs plus a second set the model lacks).
        std::fs::write(t.path().join("g.sbi"), b"SBI\0\x00\x02\x10\x01AAAAAAAAAA\x00\x02\x05\x01AAAAAAAAAA").unwrap();
        let disc = DiscInput { path: cue.clone(), disc_id: Some("SCES-99998".into()), ..Default::default() };
        let out2 = t.path().join("s").join("EBOOT.PBP");
        convert(&ConvertRequest { discs: vec![disc], patches: db, title: "T".into(), output: out2.clone(), donor: None, artwork: Artwork::default(), level: 1 }, &()).unwrap();
        let tracks = DiscImage::open(&cue).unwrap().tracks;
        let expect = subq::load_beside(&cue, &tracks).unwrap().unwrap();
        assert_eq!((expect.sectors, expect.damaged), (2, 2));
        assert_eq!(inspect(&out2).unwrap().discs[0].subchannels, expect.table);
        // LBA 155 (00:02:05) answers 154: rel 00:00:04, abs 00:02:04; sorted before LBA 160.
        assert_eq!(&expect.table[12..24], &[5, 0, 0, 0, 1, 1, 0, 0, 4, 0, 2, 4]);
    }

    /// The real-PSP hang of every patched Sheep Dog build: its loader checks two CRC words in each
    /// executable header. An ISO9660 disc with two executables carrying the frame cap: LEV00.EXE
    /// uses the loader CRC, OTHER.EXE does not. After the fps patch LEV00's words match its new
    /// contents and OTHER's header is left alone.
    #[test]
    fn patched_executables_keep_a_valid_loader_crc() {
        use patching::exe_crc::loader_crc;
        use patching::sector::build_mode2_form1;
        const CAP: [u8; 24] = [0xC0, 0x47, 0x02, 0x0C, 0x02, 0x00, 0x04, 0x24, 0x03, 0x00, 0x00, 0x10, 0, 0, 0, 0, 0xC0, 0x47, 0x02, 0x0C, 0x21, 0x20, 0x00, 0x00];
        const T_SIZE: usize = 0x1000;
        let exe = |crc: bool| {
            let mut f = vec![0u8; 0x800 + T_SIZE];
            f[..8].copy_from_slice(b"PS-X EXE");
            f[0x1C..0x20].copy_from_slice(&(T_SIZE as u32).to_le_bytes());
            f[0x800 + 0x954..0x800 + 0x954 + 24].copy_from_slice(&CAP);
            let (h, b) = if crc { (loader_crc(&f[..0x100]), loader_crc(&f[0x800..])) } else { (0xDEAD_BEEF, 7) };
            f[0x100..0x104].copy_from_slice(&h.to_le_bytes());
            f[0x104..0x108].copy_from_slice(&b.to_le_bytes());
            f
        };
        let record = |name: &[u8], lba: u32, size: u32| {
            let mut r = vec![0u8; 33 + name.len() + (1 - name.len() % 2)];
            r[0] = r.len() as u8;
            r[2..6].copy_from_slice(&lba.to_le_bytes());
            r[10..14].copy_from_slice(&size.to_le_bytes());
            r[32] = name.len() as u8;
            r[33..33 + name.len()].copy_from_slice(name);
            r
        };
        let mut user = vec![[0u8; 2048]; 40];
        user[16][..6].copy_from_slice(b"\x01CD001");
        user[16][156..156 + 34].copy_from_slice(&record(b"\0", 18, 2048)[..34]);
        let dir: Vec<u8> = [record(b"\0", 18, 2048), record(b"\x01", 18, 2048), record(b"LEV00.EXE;1", 20, 0x1800), record(b"OTHER.EXE;1", 24, 0x1800)].concat();
        user[18][..dir.len()].copy_from_slice(&dir);
        for (lba, f) in [(20, exe(true)), (24, exe(false))] {
            for (i, c) in f.chunks(2048).enumerate() {
                user[lba + i].copy_from_slice(c);
            }
        }
        let raw: Vec<u8> = user.iter().enumerate().flat_map(|(lba, u)| build_mode2_form1(lba as u32, u)).collect();
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("s.bin"), &raw).unwrap();
        let cue = touch(t.path(), "s.cue", "FILE \"s.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n");
        let output = t.path().join("o").join("EBOOT.PBP");
        let disc = DiscInput { path: cue, disc_id: Some("SLES-02895".into()), fps_unlock: true, ..Default::default() };
        convert(&ConvertRequest { discs: vec![disc], patches: PatchDb::builtin().clone(), title: "T".into(), output: output.clone(), donor: None, artwork: Artwork::default(), level: 1 }, &()).unwrap();
        extract(&output, &t.path().join("x"), &()).unwrap();
        let out = std::fs::read(t.path().join("x").join("SLES02895.bin")).unwrap();
        let file = |lba: usize| -> Vec<u8> { (lba..lba + 3).flat_map(|s| out[s * SECTOR + 24..s * SECTOR + 24 + 2048].to_vec()).collect() };
        let w = |f: &[u8], at: usize| u32::from_le_bytes(f[at..at + 4].try_into().unwrap());

        let lev = file(20);
        assert_eq!(w(&lev, 0x800 + 0x958), 0x2404_0000, "VSync(0) applied");
        assert_ne!(lev[0x800..], exe(true)[0x800..]);
        assert_eq!(w(&lev, 0x100), loader_crc(&lev[..0x100]), "header CRC valid");
        assert_eq!(w(&lev, 0x104), loader_crc(&lev[0x800..0x800 + T_SIZE]), "body CRC matches the patched code");
        assert!((20..23).all(|s| patching::sector::verify_edc(out[s * SECTOR..(s + 1) * SECTOR].try_into().unwrap())));

        let other = file(24);
        assert_eq!(w(&other, 0x800 + 0x958), 0x2404_0000, "patched too");
        assert_eq!(other[..0x800], exe(false)[..0x800], "no loader CRC scheme → header untouched");
    }
}
