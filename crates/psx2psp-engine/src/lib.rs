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
pub mod native_code;
pub mod patchdb;
pub mod sbidb;
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

/// Inflates a built-in zlib resource (`build.rs` output or a `resources/*.z` file).
pub(crate) fn inflate(z: &[u8]) -> Vec<u8> {
    miniz_oxide::inflate::decompress_to_vec_zlib(z).expect("built-in resource is valid zlib")
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

/// Files a `.cue` uses, as identity keys: its FILE names, or the files they resolve to when the
/// dump was renamed (empty if the sheet can't be read or resolved).
fn cue_members(cue: &Path) -> Vec<PathBuf> {
    psx2psp_core::disc::cue_files(cue).map(|f| f.iter().map(|p| path_key(p)).collect()).unwrap_or_default()
}

/// Opens disc `n` (1-based) and says which files stand in for the names its sheet gives.
fn open_disc(path: &Path, n: usize, monitor: &dyn Monitor) -> Result<DiscImage> {
    let img = DiscImage::open(path)?;
    for (named, used) in &img.renamed {
        monitor.log(&format!("Disc {n}: the cue names “{named}”, which is not in its folder: using “{used}”"));
    }
    Ok(img)
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
/// not to boot under POPS. Lower levels build faster and larger, and break some games on a
/// real PSP: those carry a `min_level` in the patch database, which [`convert`] enforces.
pub const DEFAULT_LEVEL: u32 = 5;
pub use psx2psp_core::psar::LEVELS;

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
    } else if let Some(g) = db.config_game(id) {
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
/// real disc and wins, then redump's SBI for the disc's product code from the built-in set
/// ([`sbidb`]): the magic word only says which bits are set, and some discs (MediEvil
/// SCES-01494) damage other sectors than the 32 the magic-word model assumes.
fn subchannels_for(d: &DiscInput, img: &DiscImage, disc_id: &str, magic: Option<u16>, n: usize, monitor: &dyn Monitor) -> Result<Option<Vec<u8>>> {
    if let Some(r) = subq::load_beside(&d.path, &img.tracks)? {
        let name = r.path.file_name().map_or_else(|| r.path.display().to_string(), |f| f.to_string_lossy().into_owned());
        monitor.log(&format!("Disc {}: LibCrypt — subchannel table from {name} ({} sectors, {} damaged)", n + 1, r.sectors, r.damaged));
        return Ok(Some(r.table));
    }
    match sbidb::lookup(disc_id) {
        [] => {}
        [s] => {
            monitor.log(&format!("Disc {}: LibCrypt — subchannel table from the built-in SBI for {} ({} sectors)", n + 1, s.serial, s.entries.len()));
            return Ok(Some(subq::table(&s.entries, &img.tracks)));
        }
        many => monitor.log(&format!(
            "Disc {}: LibCrypt — the built-in SBI set has {} different dumps for {} ({}) and cannot tell which one \
             this is: put the redump .sbi of this dump next to the CUE and rebuild",
            n + 1,
            many.len(),
            many[0].serial,
            sbidb::titles(many)
        )),
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

/// The XMB/save ID of a game whose first disc is `id`. Like PSX2PSP, it comes from the game DB,
/// so disc 2+ of a set and compilation parts share disc 1's save folder.
fn save_id(id: &str, monitor: &dyn Monitor) -> String {
    let save_id = gamedb::lookup(id).map(|g| g.save_id.clone()).filter(|s| s.len() == 9 && s[..4].bytes().all(|b| b.is_ascii_uppercase()) && s[4..].bytes().all(|b| b.is_ascii_digit())).unwrap_or_else(|| id.to_owned());
    if save_id != id {
        monitor.log(&format!("Save ID {save_id} (shared with the first disc of the set, per game DB)"));
    }
    save_id
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
        let img = open_disc(&d.path, n + 1, monitor)?;
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
        subqs.push(subchannels_for(d, &img, &disc_id, req.patches.libcrypt(&id), n, monitor)?);
        pals.push(pal);
        configs.push(config);
        fixes.push(fix);
        images.push(img);
        ids.push(id);
        ppfs.push(ppf);
    }

    // A game that hangs on a real PSP below some level (patch database `min_level`) raises the
    // level of the whole PSAR.
    let mut level = req.level.clamp(*LEVELS.start(), *LEVELS.end());
    for (n, id) in ids.iter().enumerate() {
        if let Some(min) = req.patches.min_level(id).filter(|&m| m > level) {
            monitor.log(&format!("Disc {}: compression raised from level {level} to {min}: {id} hangs on a real PSP below level {min} (patch database)", n + 1));
            level = min;
        }
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
        level,
        boot_png: req.artwork.boot.as_deref().unwrap_or(&donor.boot_png),
        donor: &donor,
    };

    let sfo = Sfo::pops(&req.title, &save_id(&ids[0], monitor));
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
    monitor.log(&format!("Compressing {} disc(s) at level {} on {} threads", images.len(), level, std::thread::available_parallelism().map_or(1, |n| n.get())));
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

// ───────────────────────────── native (recomp) output ─────────────────────────────

/// The native runtime (`psp/psxrt`, prebuilt user-mode PRX): DATA.PSP of a native EBOOT.
pub const NATIVE_RUNTIME: &[u8] = include_bytes!("../../../resources/psxrt.prx");
/// DATA.PSAR of a native EBOOT, read by psxrt (`psp/psxrt/disc.c`). Little endian, offsets from
/// the start of DATA.PSAR:
/// - the header, [`NATIVE_PSAR_HEADER_LEN`] bytes ([`native_psar_header`]);
/// - `blocks + 1` u32 offsets: block i spans `index[i]..index[i + 1]`;
/// - the blocks. Block i holds the image's raw 2352-byte sectors from `16 * i` (sector 0 is
///   00:02:00, LBA 150), fewer in the last one; it is raw deflate, or the sectors as they are when
///   deflate would not be smaller (its length is then that of the raw data);
/// - the translated code ([`native_code`]): for each executable its code and its dispatch-table
///   slice, each deflated or stored the same way, its patch table ([`native_code::patch_groups`]:
///   one (offset from the table, stored length, raw length) triple per group, then the groups,
///   each deflated or stored), then a directory of
///   [`NATIVE_CODE_ENTRY_LEN`]-byte entries ([`native_code_entry`]). None without code. One
///   directory serves every disc of the set: a disc change keeps the code that runs;
/// - with more than one disc, each further disc's index and blocks, laid out as the first's, then
///   the disc table: [`NATIVE_DISC_ENTRY_LEN`] bytes per disc, the first included (disc ID 16,
///   raw sectors 4, index offset 4, blocks 4, zero 4). The header's first-disc fields stay, so a
///   one-disc set has no table.
pub const NATIVE_PSAR_MAGIC: [u8; 8] = *b"PSXRTDAT";
pub const NATIVE_PSAR_VERSION: u32 = 5;
/// magic 8, version 4, header length 4, disc ID 16, title 128, disc sectors 4, sectors per block
/// 4, index offset 4, block count 4, code directory offset 4, code images 4, host address of the
/// code 4, code size 4 (these four written once the code is), flags 4 ([`NATIVE_FLAG_PAL60`]),
/// CPU shift 4, disc table offset 4 and discs 4 (both 0 for one disc).
pub const NATIVE_PSAR_HEADER_LEN: usize = 0xD0;
/// Offset of the code fields in the header.
const NATIVE_PSAR_CODE_FIELDS: u64 = 0xB0;
/// Offset of the flags in the header.
const NATIVE_PSAR_FLAGS: usize = 0xC0;
/// Offset of the disc table fields (table offset, discs) in the header.
const NATIVE_PSAR_DISC_FIELDS: u64 = 0xC8;
/// Bytes per entry of the disc table.
pub const NATIVE_DISC_ENTRY_LEN: usize = 0x20;
/// Discs of a native set at most (as a PSTITLEIMG).
pub const NATIVE_MAX_DISCS: usize = 5;
/// Flag: a PAL picture runs at 60 vblanks a second (patch database `pal60`, "60 Hz" switch on).
pub const NATIVE_FLAG_PAL60: u32 = 1;
/// Flag: the player changes the disc, the runtime does not (patch database `disc_swap = manual`).
pub const NATIVE_FLAG_MANUAL_DISC: u32 = 2;
/// Bytes per entry of the code directory.
pub const NATIVE_CODE_ENTRY_LEN: usize = 0x40;
/// Raw sectors per block of a native DATA.PSAR (as PSISOIMG: 0x9300 bytes).
pub const NATIVE_BLOCK_SECTORS: u32 = 16;

/// Header of a native DATA.PSAR; the index follows it. Disc ID and title are NUL-padded and cut
/// (on a char boundary) to leave room for their NUL.
pub fn native_psar_header(disc_id: &str, title: &str, disc_sectors: u32, flags: u32) -> Vec<u8> {
    fn field(out: &mut Vec<u8>, s: &str, len: usize) {
        let mut end = s.len().min(len - 1);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        out.extend_from_slice(&s.as_bytes()[..end]);
        out.resize(out.len() + len - end, 0);
    }
    let mut h = Vec::with_capacity(NATIVE_PSAR_HEADER_LEN);
    h.extend_from_slice(&NATIVE_PSAR_MAGIC);
    h.extend_from_slice(&NATIVE_PSAR_VERSION.to_le_bytes());
    h.extend_from_slice(&(NATIVE_PSAR_HEADER_LEN as u32).to_le_bytes());
    field(&mut h, disc_id, 16);
    field(&mut h, title, 128);
    let blocks = disc_sectors.div_ceil(NATIVE_BLOCK_SECTORS);
    for v in [disc_sectors, NATIVE_BLOCK_SECTORS, NATIVE_PSAR_HEADER_LEN as u32, blocks] {
        h.extend_from_slice(&v.to_le_bytes());
    }
    h.resize(NATIVE_PSAR_HEADER_LEN, 0);
    h[NATIVE_PSAR_FLAGS..NATIVE_PSAR_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
    h
}

/// Entry of the code directory: text address, text size, text hash, host address, code length,
/// code offset, stored code length, table offset, stored table length (offsets from the start
/// of DATA.PSAR; a stored length equal to the raw one means stored as is, the raw table length
/// is the text size rounded up to words), file name NUL padded to 16 bytes, hash of the first
/// [`native_code::HEAD_LEN`] bytes of the text, patch table offset and group count.
pub fn native_code_entry(img: &native_code::CodeImage, code: (u32, u32), table: (u32, u32), patch: (u32, u32)) -> [u8; NATIVE_CODE_ENTRY_LEN] {
    let mut e = [0u8; NATIVE_CODE_ENTRY_LEN];
    let words = [img.text_addr, img.text_size, img.hash, img.host_base, img.code.len() as u32, code.0, code.1, table.0, table.1];
    for (i, v) in words.iter().enumerate() {
        e[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    let name = img.name.as_bytes();
    let n = name.len().min(15);
    e[0x24..0x24 + n].copy_from_slice(&name[..n]);
    e[0x34..0x38].copy_from_slice(&img.head_hash.to_le_bytes());
    e[0x38..0x3C].copy_from_slice(&patch.0.to_le_bytes());
    e[0x3C..0x40].copy_from_slice(&patch.1.to_le_bytes());
    e
}

/// The PS-X executables of a disc's ISO9660 filesystem (file name, bytes) as they are after
/// `patches`, applied per sector in stream order; none when it has no readable filesystem.
pub fn disc_executables(img: &DiscImage, patches: &[&dyn ImagePatch]) -> Vec<(String, Vec<u8>)> {
    let Ok(files) = img.iso_files() else { return Vec::new() };
    let mut rd = img.reader();
    let mut out = Vec::new();
    for f in files.iter().filter(|f| f.size >= 0x800) {
        if !img.read_file(&mut rd, f, 8).is_ok_and(|h| h == b"PS-X EXE") {
            continue;
        }
        if let Ok(bytes) = read_patched_file(img, &mut rd, f, patches) {
            out.push((f.path.rsplit('/').next().unwrap_or(&f.path).to_string(), bytes));
        }
    }
    out
}

/// The files of the disc that could be overlays (S24): not a PS-X EXE, and from 0x800 to
/// [`native_code::OVERLAY_MAX`] bytes; the entries of that size of an archive of any size
/// ([`overlay::archive_entries`]), named `FILE+offset`; in a larger file with no such table
/// (Silent Hill's SILENT, 96 MB of rooms one after the other), each run of code
/// ([`overlay::code_runs`]), named `FILE+offset` too. With the game's `cipher` (database), the
/// code of each file so ciphered, deciphered from the sector it starts at to the next such start
/// ([`overlay::XorLcg::starts`]), named `FILE^offset` and listed first: a game's body before its
/// rooms when [`native_code::MAX_IMAGES`] cuts. Which of them are code, and where, is
/// [`native_code::build`]'s to find.
pub fn disc_overlay_files(img: &DiscImage, patches: &[&dyn ImagePatch], cipher: Option<psx2psp_recomp::overlay::XorLcg>) -> Vec<native_code::Overlay> {
    use native_code::Overlay;
    use psx2psp_recomp::overlay::{self, CODE_BLOCK};
    use std::sync::Arc;
    const HEAD: u32 = 0x1_0000;
    /// Sectors read at once from a file too large to read whole.
    const CHUNK: u32 = 0x200;
    /// Data blocks beside a run of code in a file too large to read whole that its discovery
    /// reads tables in: 128 KB, a room's tables of Silent Hill and its header.
    const AROUND: usize = 64;
    let Ok(files) = img.iso_files() else { return Vec::new() };
    let fits = |n: u32| (0x800..=native_code::OVERLAY_MAX).contains(&n);
    let mut rd = img.reader();
    let (mut first, mut out) = (Vec::new(), Vec::new());
    for f in files.iter().filter(|f| f.size >= 0x800) {
        if img.read_file(&mut rd, f, 8).is_ok_and(|h| h == b"PS-X EXE") {
            continue;
        }
        let name = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
        let whole = if fits(f.size) { read_patched_range(img, &mut rd, f, 0, f.size, patches).ok() } else { None };
        let mut read = |o: u32, n: u32| match &whole {
            Some(b) => Some(b[o as usize..(o + n) as usize].to_vec()),
            None => read_patched_range(img, &mut rd, f, o, n, patches).ok(),
        };
        let head = read(0, f.size.min(HEAD)).unwrap_or_default();
        let entries = overlay::archive_entries(&head, u64::from(f.size));
        for &(o, s) in entries.iter().filter(|e| fits(e.1)) {
            if let Some(bytes) = read(o, s) {
                out.push(Overlay::whole(format!("{name}+{o:X}"), bytes));
            }
        }
        // Each sector once: whether it is code, whether it starts a ciphered file.
        let big = whole.is_none() && entries.is_empty();
        let (mut code, mut starts) = (Vec::new(), Vec::new());
        if big || cipher.is_some() {
            for c in (0..f.size.div_ceil(2048)).step_by(CHUNK as usize) {
                let Some(b) = read(2048 * c, (f.size - 2048 * c).min(2048 * CHUNK)) else { break };
                for (i, s) in b.chunks_exact(CODE_BLOCK).enumerate() {
                    code.push(if big { overlay::code_block(s) } else { overlay::Block::Data });
                    if cipher.is_some_and(|k| k.starts(s)) {
                        starts.push(c + i as u32);
                    }
                }
            }
        }
        for (k, &s) in starts.iter().enumerate() {
            let (o, cipher) = (2048 * s, cipher.unwrap());
            let end = starts.get(k + 1).map_or(f.size, |&n| 2048 * n).min(o + native_code::OVERLAY_MAX);
            let Some(mut b) = read(o, end - o) else { continue };
            cipher.apply(&mut b);
            let b = Arc::new(b);
            first.extend(overlay::code_runs(&b).into_iter().map(|(a, e)| Overlay { name: format!("{name}^{:X}", o + a), bytes: b[a as usize..e as usize].to_vec(), file: Some((a, b.clone())) }));
        }
        // A run's file: the data blocks up to the runs beside it, AROUND at most on each side.
        let runs = overlay::block_runs(code.iter().copied());
        for (k, &(a, z)) in runs.iter().enumerate() {
            let n = 2048 * (z - a + 1) as u32;
            if n > native_code::OVERLAY_MAX {
                continue;
            }
            let lo = k.checked_sub(1).map_or(0, |p| runs[p].1 + 1).max(a.saturating_sub(AROUND));
            let hi = runs.get(k + 1).map_or(code.len(), |r| r.0).min(z + 1 + AROUND);
            let Some(b) = read(2048 * lo as u32, 2048 * (hi - lo) as u32) else { continue };
            let (b, at) = (Arc::new(b), 2048 * (a - lo));
            out.extend(overlay::code_runs(&b[at..at + n as usize]).into_iter().map(|(s, e)| {
                let (s, e) = (at + s as usize, at + e as usize);
                Overlay { name: format!("{name}+{:X}", 2048 * lo + s), bytes: b[s..e].to_vec(), file: Some((s as u32, b.clone())) }
            }));
        }
        if let Some(bytes) = whole {
            out.push(Overlay::whole(name, bytes));
        }
    }
    first.extend(out);
    first
}

/// Frames [`captured_code`] runs a disc for: a minute of game, ~13 s on the PC for Silent Hill.
pub const CAPTURE_FRAMES: u32 = 3600;
/// Frames between two looks at the RAM in [`captured_code`].
const CAPTURE_EVERY: u32 = 300;

/// The code a game puts in RAM by itself (S25): the disc, with `patches`, boots on the psxrt
/// core (psx2psp-sim) and runs [`CAPTURE_FRAMES`] frames, Start and Cross pressed in turn every
/// 150 frames from frame 600 (title screens and menus give way); every [`CAPTURE_EVERY`] frames
/// the code runs of RAM above the kernel area ([`overlay::code_runs`]) that are unchanged at the
/// next look are taken at their address. So is found the code a disc keeps compressed, encrypted
/// or in an archive of a format of its own: Silent Hill's body, decompressed from SILENT to
/// 8002DD08-800A8974. The core marks the words it runs: the stretches that ran between two
/// looks and are unchanged at the second are taken too, with the addresses where running them
/// started as entries ([`overlay::ran_runs`]), joined with those of the same bytes at earlier
/// looks: so is found code with no prologue (Crash 2's, in its NSF data at 8013A000-8016F000).
/// (name, address, bytes), each once; KUSEG addresses when `kuseg` (a game
/// that runs there, as Lilo & Stitch does).
pub fn captured_code(img: &DiscImage, patches: &[&dyn ImagePatch], kuseg: bool) -> Vec<native_code::Captured> {
    use psx2psp_recomp::overlay;
    use psx2psp_sim::Machine;
    let mut rd = img.reader();
    let read = move |lba: u32, buf: &mut [u8]| {
        let mut s = [0u8; SECTOR];
        if img.read_sector(&mut rd, lba, &mut s).is_err() {
            return false;
        }
        patches.iter().for_each(|p| p.apply(u64::from(lba) * SECTOR as u64, &mut s));
        buf.copy_from_slice(&s);
        true
    };
    let Some(mut m) = Machine::boot(img.sectors(), read) else { return Vec::new() };
    let mut out = Vec::new();
    let mut ran: Vec<native_code::Captured> = Vec::new();
    let seg = if kuseg { 0 } else { 0x8000_0000 };
    let mut seen = std::collections::HashSet::new();
    let mut last: Option<std::sync::Arc<Vec<u8>>> = None;
    for f in 1..=CAPTURE_FRAMES {
        let alive = m.frame(capture_pad(f));
        if f % CAPTURE_EVERY != 0 && alive {
            continue;
        }
        let ran_now = m.take_ran();
        let ram = m.ram();
        if let Some(before) = &last {
            for (s, e) in overlay::code_runs(before).into_iter().filter(|&(s, _)| s >= 0x1_0000) {
                let bytes = &before[s as usize..e as usize];
                if bytes != &ram[s as usize..e as usize] || !seen.insert((s, e, native_code::fnv1a32(bytes))) {
                    continue;
                }
                let at = seg | s;
                out.push(native_code::Captured { name: format!("RAM {at:08X}"), at, bytes: bytes.to_vec(), ram: before.clone(), entries: Vec::new(), seeds_only: false });
            }
            // The code that ran since the last look and stayed as it was: one capture per piece
            // of code, joined with the captures of the same bytes at earlier looks.
            let word = |m: &[u8], i: usize| m[4 * i..4 * i + 4].to_vec();
            for (s, e, entries) in overlay::ran_runs(before, &ran_now).into_iter().filter(|&(s, ..)| s >= 0x1_0000) {
                let (mut s, mut e) = (s as usize, e as usize);
                if before[s..e] != ram[s..e] {
                    // A variable among code that stayed (Crash 2's counter at 8003C13C, in its
                    // executable's text): the entries seed the images holding the code; no image
                    // is made of these bytes, whose hash RAM would seldom match.
                    if (s / 4..e / 4).all(|i| ran_now[i] == 0 || word(before, i) == word(ram, i)) {
                        let at = seg | s as u32;
                        let entries = entries.into_iter().map(|o| seg | o).collect();
                        ran.push(native_code::Captured { name: format!("RAM {at:08X}"), at, bytes: before[s..e].to_vec(), ram: before.clone(), entries, seeds_only: true });
                    }
                    continue;
                }
                let mut entries: std::collections::BTreeSet<u32> = entries.into_iter().map(|o| seg | o).collect();
                loop {
                    let joined = ran.iter().position(|c: &native_code::Captured| {
                        let (cs, ce) = ((c.at & 0x1FFF_FFFF) as usize, (c.at & 0x1FFF_FFFF) as usize + c.bytes.len());
                        !c.seeds_only && cs <= e && s <= ce && before[cs..ce] == c.bytes[..]
                    });
                    let Some(i) = joined else { break };
                    let c = ran.swap_remove(i);
                    let cs = (c.at & 0x1FFF_FFFF) as usize;
                    (s, e) = (s.min(cs), e.max(cs + c.bytes.len()));
                    entries.extend(c.entries);
                }
                let at = seg | s as u32;
                ran.push(native_code::Captured { name: format!("RAM {at:08X}"), at, bytes: before[s..e].to_vec(), ram: before.clone(), entries: entries.into_iter().collect(), seeds_only: false });
            }
        }
        last = Some(std::sync::Arc::new(ram.to_vec()));
        if !alive {
            break;
        }
    }
    out.extend(ran);
    out
}

/// Why an instruction that ran had no static translation held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Miss {
    /// A word of an image's text, as on the disc, that discovery did not translate.
    Untranslated,
    /// Translated in an image that did not hold it when it ran (its text did not hash as built).
    NotHeld,
    /// Inside an image's text with other bytes: code made or loaded at run time over it.
    Replaced,
    /// Outside every image: code loaded from a file the translation does not place, or made.
    Outside,
}

/// What [`static_coverage`] measured.
#[derive(Debug, Default)]
pub struct Coverage {
    /// Frames the game ran (fewer than asked when it stopped).
    pub frames: u32,
    /// Images (executables and overlays) the static translation made.
    pub images: usize,
    /// Distinct (address, instruction) the guest ran above the kernel area.
    pub ran: usize,
    /// Those a static image held, live and with a translation of that word, when they ran.
    pub covered: usize,
    /// Distinct addresses of the kernel area (below 0x10000) that ran: the BIOS's, not counted.
    pub kernel: usize,
    /// What ran without a static translation, by stretch of RAM of one [`Miss`]: (first
    /// address, words, frame it was first seen, cause).
    pub missing: Vec<(u32, u32, u32, Miss)>,
    /// Per image: name, text address, text bytes, translated words, first frame it was live.
    pub image_list: Vec<(String, u32, u32, usize, Option<u32>)>,
    /// Executables and overlays the translation left out, and why.
    pub skipped: Vec<(String, String)>,
    /// Images never live: name, checks, whether the head matched at the failed check shown (the
    /// last whose head did, else the last), words of RAM that differed from the disc text then,
    /// and the first of them (address, RAM word, disc word).
    pub mismatched: Vec<(String, u32, bool, usize, u32, u32, u32)>,
}

/// The goal's measure of the static analysis (AGENTS.md §1.1): the code a game runs on the
/// psxrt core in the sim, against what the translation of its disc finds without running it
/// (executables and overlays placed on the disc, [`native_code::build`] with no
/// [`captured_code`]). The game runs `frames` frames with the input of [`captured_code`]; each
/// instruction is covered when it ran while a static image held a translation of its word, as the
/// runtime takes images ([`psx2psp_sim::Machine::watch`]: checked at the first run in the text
/// after a load, held until a load over the word). Words rewritten inside a held image count as
/// covered (the runtime patches them or sends their block to the interpreter): a measure of
/// discovery, not of self-modifying code. `every` sets how often the maps are read.
pub fn static_coverage(img: &DiscImage, patches: &[&dyn ImagePatch], cipher: Option<psx2psp_recomp::overlay::XorLcg>, frames: u32, every: u32) -> std::result::Result<Coverage, String> {
    use psx2psp_sim::{Machine, RAM};
    let (exes, overlays) = (disc_executables(img, patches), disc_overlay_files(img, patches, cipher));
    // Each file's text as on the disc, by name: what an image holds where its words ran.
    let texts: std::collections::HashMap<String, Vec<u8>> = exes
        .iter()
        .filter_map(|(n, b)| psx2psp_recomp::PsxExe::parse(b).ok().map(|x| (n.clone(), x.text)))
        .chain(overlays.iter().map(|o| (o.name.clone(), o.bytes.clone())))
        .collect();
    let pack = native_code::build(exes, overlays, Vec::new())?;
    let mut rd = img.reader();
    let read = move |lba: u32, buf: &mut [u8]| {
        let mut s = [0u8; SECTOR];
        if img.read_sector(&mut rd, lba, &mut s).is_err() {
            return false;
        }
        patches.iter().for_each(|p| p.apply(u64::from(lba) * SECTOR as u64, &mut s));
        buf.copy_from_slice(&s);
        true
    };
    let mut m = Machine::boot(img.sectors(), read).ok_or("the disc does not boot on the core")?;
    let mut cov = Coverage { images: pack.images.len(), skipped: pack.skipped.clone(), ..Coverage::default() };
    let mut ids = Vec::new();
    for i in &pack.images {
        let (size, words) = (i.text_size & !3, &i.code_words[..i.text_size as usize / 4]);
        let head = i.text_size.min(native_code::HEAD_LEN as u32);
        let id = m.watch(i.text_addr & 0x1F_FFFF, size, head, i.head_hash, i.hash, words).ok_or_else(|| format!("{} ({:#010x}, {} bytes, image {}): the sim cannot watch it", i.name, i.text_addr, i.text_size, ids.len()))?;
        ids.push(id);
        cov.image_list.push((i.name.clone(), i.text_addr, i.text_size, words.iter().filter(|&&t| t != 0).count(), None));
    }
    let mut seen = std::collections::HashMap::new();
    let mut kernel = std::collections::BTreeSet::new();
    let every = every.max(1);
    for f in 1..=frames {
        let alive = m.frame(capture_pad(f));
        cov.frames = f;
        for (k, &id) in ids.iter().enumerate() {
            if cov.image_list[k].4.is_none() && m.watch_live(id) {
                cov.image_list[k].4 = Some(f);
            }
        }
        if f % every != 0 && f != frames && alive {
            continue;
        }
        let (ran, held) = m.take_ran_held();
        let ram = m.ram();
        for w in (0..RAM / 4).filter(|&w| ran[w] != 0) {
            let a = (w * 4) as u32;
            if a < 0x1_0000 {
                kernel.insert(a);
                continue;
            }
            // Per (address, instruction): the frame it first ran by, and whether it ever ran held.
            let e = seen.entry((a, u32::from_le_bytes(ram[w * 4..w * 4 + 4].try_into().unwrap()))).or_insert((f, false));
            e.1 |= held[w] != 0;
        }
        if !alive {
            break;
        }
    }
    for (k, (&id, i)) in ids.iter().zip(&pack.images).enumerate() {
        let (checks, miss) = m.watch_miss(id, i.text_size & !3);
        let (Some((snap, head)), Some(text), None) = (miss, texts.get(&i.name), cov.image_list[k].4) else { continue };
        let word = |b: &[u8], j: usize| b.get(4 * j..4 * j + 4).map_or(0, |w| u32::from_le_bytes(w.try_into().unwrap()));
        let diff: Vec<usize> = (0..snap.len() / 4).filter(|&j| word(&snap, j) != word(text, j)).collect();
        if let Some(&j) = diff.first() {
            cov.mismatched.push((i.name.clone(), checks, head, diff.len(), i.text_addr + 4 * j as u32, word(&snap, j), word(text, j)));
        }
    }
    cov.ran = seen.len();
    cov.covered = seen.values().filter(|v| v.1).count();
    // The cause of each miss: the least of those its images give (an image whose disc text has
    // that instruction there decides between Untranslated and NotHeld).
    let cause = |a: u32, insn: u32| {
        let mut c = Miss::Outside;
        for i in &pack.images {
            let Some(o) = a.checked_sub(i.text_addr & 0x1F_FFFF).filter(|&o| o < i.text_size) else { continue };
            let o = o as usize;
            let same = texts.get(&i.name).and_then(|t| t.get(o..o + 4)).is_some_and(|w| w == insn.to_le_bytes());
            let translated = i.code_words.get(o / 4).is_some_and(|&c| c != 0);
            c = c.min(match (same, translated) {
                (true, false) => Miss::Untranslated,
                (true, true) => Miss::NotHeld,
                (false, _) => Miss::Replaced,
            });
        }
        c
    };
    let mut missing = std::collections::BTreeMap::new();
    for (&(a, insn), &(f, _)) in seen.iter().filter(|(_, v)| !v.1) {
        let e = missing.entry(a).or_insert((f, Miss::Outside));
        *e = (e.0.min(f), e.1.min(cause(a, insn)));
    }
    cov.kernel = kernel.len();
    for (&a, &(f, c)) in &missing {
        match cov.missing.last_mut() {
            Some((s, n, f0, c0)) if *s + 4 * *n == a && *c0 == c => {
                *n += 1;
                *f0 = (*f0).min(f);
            }
            _ => cov.missing.push((a, 1, f, c)),
        }
    }
    Ok(cov)
}

/// The input of [`captured_code`] and [`static_coverage`] at frame `f`: Start and Cross pressed
/// in turn, 6 frames every 150 from frame 600.
fn capture_pad(f: u32) -> u16 {
    use psx2psp_sim::{CROSS, START};
    match f.checked_sub(600).filter(|k| k % 150 < 6).map(|k| k / 150) {
        Some(k) if k % 2 == 0 => START,
        Some(_) => CROSS,
        None => 0,
    }
}

/// An ISO9660 file of Mode 1 or Mode 2 Form 1 sectors, with `patches` applied to each sector.
fn read_patched_file(img: &DiscImage, rd: &mut psx2psp_core::disc::DiscReader, f: &psx2psp_core::IsoFile, patches: &[&dyn ImagePatch]) -> psx2psp_core::Result<Vec<u8>> {
    read_patched_range(img, rd, f, 0, f.size, patches)
}

/// `len` bytes from `at` of an ISO9660 file of Mode 1 or Mode 2 Form 1 sectors, with `patches`
/// applied to each sector.
fn read_patched_range(img: &DiscImage, rd: &mut psx2psp_core::disc::DiscReader, f: &psx2psp_core::IsoFile, at: u32, len: u32, patches: &[&dyn ImagePatch]) -> psx2psp_core::Result<Vec<u8>> {
    let (first, skip) = (at / 2048, (at % 2048) as usize);
    let mut out = Vec::with_capacity((skip + len as usize).next_multiple_of(2048));
    let mut s = [0u8; SECTOR];
    for i in first..(at + len).div_ceil(2048) {
        let lba = f.lba.saturating_add(i);
        img.read_sector(rd, lba, &mut s)?;
        patches.iter().for_each(|p| p.apply(u64::from(lba) * SECTOR as u64, &mut s));
        let d = if s[15] == 1 { 16 } else { 24 };
        out.extend_from_slice(&s[d..d + 2048]);
    }
    out.drain(..skip);
    out.truncate(len as usize);
    Ok(out)
}

/// One disc of a native set: its image, ID and the patches of POPS mode, in the same order (PPFs,
/// PAL→NTSC, the title's code patches, the loader CRCs). They change the disc the runtime reads
/// and the executables the translator sees, so the translations match the patched code in RAM.
struct NativeDisc {
    img: DiscImage,
    disc_id: String,
    ppf: Vec<PpfPatch>,
    pal: Option<PatternPatch>,
    fix: Vec<PatternPatch>,
    crc: Option<SectorWrites>,
}

impl NativeDisc {
    fn patches(&self) -> Vec<&dyn ImagePatch> {
        self.ppf
            .iter()
            .map(|p| p as &dyn ImagePatch)
            .chain(self.pal.iter().map(|p| p as &dyn ImagePatch))
            .chain(self.fix.iter().map(|p| p as &dyn ImagePatch))
            .chain(self.crc.iter().map(|p| p as &dyn ImagePatch))
            .collect()
    }
}

/// Opens disc `n` of a native set and works out its patches; `pal60` is the set's (its 60hz
/// patches stay off).
fn native_disc(req: &ConvertRequest, d: &DiscInput, n: usize, pal60: bool, monitor: &dyn Monitor) -> Result<NativeDisc> {
    let img = open_disc(&d.path, n + 1, monitor)?;
    let disc_id = match &d.disc_id {
        Some(id) if !id.trim().is_empty() => normalise_id(id),
        _ => img.probe().disc_id.ok_or_else(|| EngineError::Input(format!("disc {}: no SYSTEM.CNF game ID found — enter it manually", n + 1)))?,
    };
    monitor.log(&format!("Native: disc {} {} — ID {disc_id}", n + 1, d.path.display()));
    let id = req.patches.pops_id(&disc_id);
    let user_ppf = match &d.ppf {
        Some(p) => {
            let patch = load_ppf(&img, p, n)?;
            monitor.log(&format!("Disc {}: {}", n + 1, patch.describe()));
            Some(patch)
        }
        None => None,
    };
    let fixes_input = DiscInput { ntsc_60hz: d.ntsc_60hz && !pal60, ..d.clone() };
    let TitleFixes { config, ppfs: mut ppf, patterns: fix } = title_fixes_for(&fixes_input, &req.patches, &img, user_ppf.as_ref(), &id, n, monitor)?;
    if config.is_some() {
        monitor.log("Native: the POPS config does not apply to the native runtime");
    }
    ppf.extend(user_ppf);
    let base: Vec<&PpfPatch> = ppf.iter().collect();
    let pal = if d.pal_ntsc { pal_ntsc_for(&img, &base, &id, n, monitor)? } else { None };
    let crc = exe_crc_for(&img, &base, pal.iter().chain(&fix).collect(), n, monitor)?;
    Ok(NativeDisc { img, disc_id, ppf, pal, fix, crc })
}

/// Writes a disc's block index (zeros, filled later) and blocks at the current position of `w`,
/// with its patches applied and deflated at `level` in parallel: the index's offset from `psar`
/// and its words. `done` and `total` are the raw bytes of the set for the progress.
fn write_native_blocks<W: Write + Seek>(
    w: &mut W,
    psar: u64,
    disc: &NativeDisc,
    level: u8,
    stats: &mut PsarStats,
    total: u64,
    monitor: &dyn Monitor,
) -> psx2psp_core::Result<(u64, Vec<u8>)> {
    use rayon::prelude::*;
    const BATCH: usize = 64;
    let block_len = NATIVE_BLOCK_SECTORS as usize * SECTOR;
    let size = disc.img.size_bytes();
    let blocks = disc.img.sectors().div_ceil(NATIVE_BLOCK_SECTORS) as usize;
    let patches = disc.patches();
    let index_at = w.stream_position()?;
    w.write_all(&vec![0u8; (blocks + 1) * 4])?;
    let mut pos = index_at + (blocks as u64 + 1) * 4 - psar;
    let mut index = Vec::with_capacity((blocks + 1) * 4);
    let mut rd = disc.img.reader();
    let mut batch = vec![0u8; BATCH * block_len];
    for first in (0..blocks).step_by(BATCH) {
        if monitor.cancelled() {
            return Err(psx2psp_core::Error::Cancelled);
        }
        let len = (size - (first * block_len) as u64).min(batch.len() as u64) as usize;
        let buf = &mut batch[..len];
        rd.read_exact(buf)?;
        let at = (first * block_len) as u64;
        patches.iter().for_each(|p| p.apply(at, buf));
        let out: Vec<Vec<u8>> = buf
            .par_chunks(block_len)
            .map(|raw| {
                let z = miniz_oxide::deflate::compress_to_vec(raw, level);
                if z.len() < raw.len() { z } else { raw.to_vec() }
            })
            .collect();
        for data in &out {
            index.extend_from_slice(&psar_offset(pos)?.to_le_bytes());
            w.write_all(data)?;
            pos += data.len() as u64;
        }
        stats.blocks += out.len() as u64;
        stats.raw_bytes += len as u64;
        monitor.progress(stats.raw_bytes, total);
    }
    index.extend_from_slice(&psar_offset(pos)?.to_le_bytes());
    Ok((index_at - psar, index))
}

fn psar_offset(pos: u64) -> psx2psp_core::Result<u32> {
    u32::try_from(pos).map_err(|_| psx2psp_core::Error::Unsupported("disc data exceeds 4 GiB".into()))
}

/// Writes a native DATA.PSAR at `psar` (where `w` stands): header, the first disc's index and
/// blocks, the translated code, then the other discs' and the disc table.
fn write_native_psar<W: Write + Seek>(
    w: &mut W,
    psar: u64,
    discs: &[NativeDisc],
    title: &str,
    flags: u32,
    level: u8,
    code: &native_code::CodePack,
    monitor: &dyn Monitor,
) -> psx2psp_core::Result<PsarStats> {
    use rayon::prelude::*;
    let offset = psar_offset;
    let total: u64 = discs.iter().map(|d| d.img.size_bytes()).sum();
    let mut stats = PsarStats::default();
    w.write_all(&native_psar_header(&discs[0].disc_id, title, discs[0].img.sectors(), flags))?;
    let mut indexes = vec![write_native_blocks(w, psar, &discs[0], level, &mut stats, total, monitor)?];
    let mut pos = w.stream_position()? - psar;

    if !code.images.is_empty() {
        let pack = |raw: &[u8]| {
            let z = miniz_oxide::deflate::compress_to_vec(raw, level);
            if z.len() < raw.len() { z } else { raw.to_vec() }
        };
        let blobs: Vec<(Vec<u8>, Vec<u8>, Vec<Vec<u8>>)> =
            code.images.par_iter().map(|i| (pack(&i.code), pack(&i.table), i.patch.iter().map(|g| pack(g)).collect())).collect();
        let mut dir = Vec::with_capacity(code.images.len() * NATIVE_CODE_ENTRY_LEN);
        for (i, (c, t, p)) in code.images.iter().zip(&blobs) {
            let code_at = (offset(pos)?, c.len() as u32);
            w.write_all(c)?;
            pos += c.len() as u64;
            let table_at = (offset(pos)?, t.len() as u32);
            w.write_all(t)?;
            pos += t.len() as u64;
            let patch_at = (offset(pos)?, p.len() as u32);
            let mut at = 12 * p.len() as u32;
            for (g, raw) in p.iter().zip(&i.patch) {
                w.write_all(&[at, g.len() as u32, raw.len() as u32].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())?;
                at += g.len() as u32;
            }
            for g in p {
                w.write_all(g)?;
            }
            pos += u64::from(at);
            dir.extend_from_slice(&native_code_entry(i, code_at, table_at, patch_at));
        }
        let fields = [offset(pos)?, code.images.len() as u32, code.base, code.size];
        w.write_all(&dir)?;
        pos += dir.len() as u64;
        let end = w.stream_position()?;
        w.seek(SeekFrom::Start(psar + NATIVE_PSAR_CODE_FIELDS))?;
        w.write_all(&fields.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())?;
        w.seek(SeekFrom::Start(end))?;
    }
    if discs.len() > 1 {
        for d in &discs[1..] {
            indexes.push(write_native_blocks(w, psar, d, level, &mut stats, total, monitor)?);
        }
        pos = w.stream_position()? - psar;
        let mut table = Vec::with_capacity(discs.len() * NATIVE_DISC_ENTRY_LEN);
        for (d, (at, _)) in discs.iter().zip(&indexes) {
            let id = d.disc_id.as_bytes();
            let n = id.len().min(15);
            table.extend_from_slice(&id[..n]);
            table.resize(table.len() + 16 - n, 0);
            for v in [d.img.sectors(), offset(*at)?, d.img.sectors().div_ceil(NATIVE_BLOCK_SECTORS), 0] {
                table.extend_from_slice(&v.to_le_bytes());
            }
        }
        w.write_all(&table)?;
        let fields = [offset(pos)?, discs.len() as u32];
        pos += table.len() as u64;
        let end = w.stream_position()?;
        w.seek(SeekFrom::Start(psar + NATIVE_PSAR_DISC_FIELDS))?;
        w.write_all(&fields.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())?;
        w.seek(SeekFrom::Start(end))?;
    }
    stats.stored_bytes = pos;
    let end = w.stream_position()?;
    for (at, index) in &indexes {
        w.seek(SeekFrom::Start(psar + at))?;
        w.write_all(index)?;
    }
    w.seek(SeekFrom::Start(end))?;
    Ok(stats)
}

/// PARAM.SFO of a native EBOOT: a PSP game (category `MG`, what the pspsdk's mksfo writes),
/// with the title and ID of the PS1 disc.
pub fn native_sfo(title: &str, disc_id: &str) -> Sfo {
    let mut sfo = Sfo::new();
    sfo.set_int("BOOTABLE", 1);
    sfo.set_str("CATEGORY", "MG", 4);
    sfo.set_str("DISC_ID", disc_id, 16);
    sfo.set_str("DISC_VERSION", "1.00", 8);
    sfo.set_int("PARENTAL_LEVEL", 1);
    sfo.set_str("PSP_SYSTEM_VER", "1.00", 8);
    sfo.set_int("REGION", 0x8000);
    sfo.set_str("TITLE", title, 128);
    sfo
}

/// Output mode Native (recomp backend, `docs/recomp/PLAN.md`): an EBOOT that runs the native
/// runtime instead of POPS. Same title, ID and artwork as [`convert`]; DATA.PSAR carries the
/// whole disc for the runtime's CD ([`NATIVE_PSAR_MAGIC`]). Up to [`NATIVE_MAX_DISCS`] discs, the
/// runtime changing them on the player's word, with one translation of the code of them all;
/// donor and boot picture do not apply.
pub fn convert_native(req: &ConvertRequest, monitor: &dyn Monitor) -> Result<ConvertReport> {
    let t0 = Instant::now();
    if req.discs.is_empty() || req.discs.len() > NATIVE_MAX_DISCS {
        return Err(EngineError::Input(format!("native mode builds 1 to {NATIVE_MAX_DISCS} discs, {} selected", req.discs.len())));
    }
    if req.title.trim().is_empty() {
        return Err(EngineError::Input("title is empty".into()));
    }
    monitor.log(&format!("Native: {} disc(s), runtime psxrt {} B", req.discs.len(), NATIVE_RUNTIME.len()));
    // A `pal60` game keeps its PAL picture: the runtime gives it 60 Hz in place of its 60hz
    // patches. The first disc decides for the set.
    let d0 = &req.discs[0];
    let id0 = match &d0.disc_id {
        Some(id) if !id.trim().is_empty() => Some(normalise_id(id)),
        _ => DiscImage::open(&d0.path)?.probe().disc_id,
    };
    let pal60 = d0.ntsc_60hz && id0.as_ref().is_some_and(|id| req.patches.pal60(&req.patches.pops_id(id)));
    if pal60 {
        monitor.log("Native: the PAL picture runs at 60 Hz (PAL60), the 60hz patches stay off");
    }
    let manual_disc = req.discs.len() > 1 && id0.as_ref().is_some_and(|id| req.patches.manual_disc(&req.patches.pops_id(id)));
    if manual_disc {
        monitor.log("Native: the player changes the disc (L+R+SELECT), not the runtime (disc_swap = manual)");
    }
    let cipher = id0.as_ref().and_then(|id| req.patches.cipher(&req.patches.pops_id(id)));
    if cipher.is_some() {
        monitor.log("Native: the code of the game's ciphered files is looked for deciphered (cipher)");
    }
    let discs = req.discs.iter().enumerate().map(|(n, d)| native_disc(req, d, n, pal60, monitor)).collect::<Result<Vec<_>>>()?;
    let disc_id = discs[0].disc_id.clone();

    // The code of every disc, each piece once: a disc change keeps the translations that run.
    let mut exes: Vec<(String, Vec<u8>)> = Vec::new();
    let mut overlays: Vec<native_code::Overlay> = Vec::new();
    let mut captured: Vec<native_code::Captured> = Vec::new();
    let mut found = 0;
    for d in &discs {
        let patches = d.patches();
        let mine = disc_executables(&d.img, &patches);
        found += mine.len();
        for e in mine {
            if !exes.iter().any(|x| x.1 == e.1) {
                exes.push(e);
            }
        }
        for o in disc_overlay_files(&d.img, &patches, cipher) {
            if !overlays.iter().any(|x| x.bytes == o.bytes) {
                overlays.push(o);
            }
        }
    }
    let kuseg = exes.iter().any(|(_, b)| psx2psp_recomp::exe::PsxExe::parse(b).is_ok_and(|x| x.text_addr >> 29 == 0));
    for d in &discs {
        let started = Instant::now();
        let mine = captured_code(&d.img, &d.patches(), kuseg);
        monitor.log(&format!("Native: disc {}: {} code runs captured in {CAPTURE_FRAMES} frames ({:.1} s)", d.disc_id, mine.len(), started.elapsed().as_secs_f64()));
        for c in mine {
            if !captured.iter().any(|x| x.at == c.at && x.seeds_only == c.seeds_only && x.bytes == c.bytes) {
                captured.push(c);
            }
        }
    }
    let code = native_code::build(exes, overlays, captured).map_err(EngineError::Input)?;
    for (name, why) in &code.skipped {
        monitor.log(&format!("Native: {name} left to the interpreter: {why}"));
    }
    let code_bytes: usize = code.images.iter().map(|i| i.code.len()).sum();
    let overlays = code.images.iter().filter(|i| i.overlay).count();
    monitor.log(&format!(
        "Native: {} of {found} executables and {overlays} overlays translated, {code_bytes} B of code in {} B at {:#010x}",
        code.images.len() - overlays,
        code.size,
        code.base
    ));
    let sections = PbpSections {
        param_sfo: native_sfo(&req.title, &save_id(&disc_id, monitor)).to_bytes(),
        icon0: req.artwork.icon0.clone(),
        icon1: req.artwork.icon1_pmf.clone(),
        pic0: req.artwork.pic0.clone(),
        pic1: req.artwork.pic1.clone(),
        snd0: req.artwork.snd0.clone(),
        data_psp: NATIVE_RUNTIME.to_vec(),
    };

    if let Some(dir) = req.output.parent() {
        std::fs::create_dir_all(dir).map_err(psx2psp_core::Error::io_path(dir))?;
    }
    let part = PartFile::new(&req.output);
    let file = File::create(&part.part).map_err(psx2psp_core::Error::io_path(&part.part))?;
    let mut w = BufWriter::new(file);
    let level = req.level.clamp(*LEVELS.start(), *LEVELS.end()) as u8;
    let mut stats = PsarStats::default();
    write_pbp(&mut w, &sections, 0x10, |w, psar| {
        let flags = if pal60 { NATIVE_FLAG_PAL60 } else { 0 } | if manual_disc { NATIVE_FLAG_MANUAL_DISC } else { 0 };
        stats = write_native_psar(w, psar, &discs, &req.title, flags, level, &code, monitor)?;
        Ok(())
    })?;
    let file = w.into_inner().map_err(|e| EngineError::from(e.into_error()))?;
    file.sync_all()?;
    let file_size = file.metadata()?.len();
    drop(file);
    part.commit(&req.output)?;
    let report = ConvertReport { output: req.output.clone(), disc_ids: discs.iter().map(|d| d.disc_id.clone()).collect(), stats, file_size, elapsed: t0.elapsed() };
    monitor.log(&format!("Done: {} ({} B, native)", report.output.display(), file_size));
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

    #[test]
    fn min_level_raises_the_level_for_that_game_only() {
        // Level 1 EBOOTs hang MediEvil on a real PSP (kernel inflate): its `min_level = 5` must
        // win over the level asked for, and leave other games and higher levels alone.
        use patching::sector::build_mode2_form1;
        let t = tempfile::tempdir().unwrap();
        let mut x = 0x9e37_79b9_u32;
        let mut raw = Vec::new();
        for lba in 0..64u32 {
            // Small-alphabet noise: levels 1 and 5 give different deflate streams.
            let user: Vec<u8> = (0..2048).map(|_| { x ^= x << 13; x ^= x >> 17; x ^= x << 5; b"abcdefgh"[(x >> 29) as usize] }).collect();
            raw.extend_from_slice(&build_mode2_form1(lba, user.as_slice().try_into().unwrap()));
        }
        std::fs::write(t.path().join("g.bin"), &raw).unwrap();
        let cue = touch(t.path(), "g.cue", "FILE \"g.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n");
        let db = PatchDb::parse("[game SLES-00001]\nmin_level = 5\n", patchdb::Origin::User).unwrap();
        let build = |level: u32, patches: &PatchDb| {
            let output = t.path().join(format!("{level}-{}", patches.games.len())).join("EBOOT.PBP");
            let disc = DiscInput { path: cue.clone(), disc_id: Some("SLES00001".into()), ..Default::default() };
            convert(&ConvertRequest { discs: vec![disc], patches: patches.clone(), title: "T".into(), output: output.clone(), donor: None, artwork: Artwork::default(), level }, &()).unwrap();
            std::fs::read(output).unwrap()
        };
        let deflate = |l: u8| miniz_oxide::deflate::compress_to_vec(&raw[..0x9300], l);
        assert_ne!(deflate(1), deflate(5), "the sample must tell level 1 from 5");
        let none = PatchDb::default();
        let five = build(5, &none);
        assert_ne!(build(1, &none), five, "without min_level, level 1 is honoured");
        assert_eq!(build(1, &db), five);
        assert_eq!(build(4, &db), five);
        assert_eq!(build(9, &db), build(9, &none), "a minimum never lowers the level");
        assert_eq!(LEVELS, 1..=9);
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
        // LEV00.EXE libetc VSync, 0x80091F70..0x80091FDC.
        const VSYNC: [u32; 27] = [
            0x2402_0001, 0x1082_003A, 0, 0x1880_0007, 0, 0x3C02_800A, 0x8C42_76B4, 0, 0x2442_FFFF, 0x0802_47E9, 0x0044_1021,
            0x3C02_800A, 0x8C42_76B4, 0x1880_0002, 0x0000_2821, 0x2485_FFFF, 0x0C02_481E, 0x0040_2021, 0x3C02_800A,
            0x8C42_76A8, 0, 0x8C50_0000, 0x3C04_800B, 0x8C84_87E0, 0x2405_0001, 0x0C02_481E, 0x2484_0001,
        ];
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
            if lba == 22 {
                for (i, w) in VSYNC.iter().enumerate() {
                    user[0x670 + 4 * i..0x674 + 4 * i].copy_from_slice(&w.to_le_bytes());
                }
            }
            raw.extend_from_slice(&build_mode2_form1(lba, &user));
        }
        std::fs::write(t.path().join("s.bin"), &raw).unwrap();
        let cue = touch(t.path(), "s.cue", "FILE \"s.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n");
        let run = |name: &str, title_fixes: bool, fps_unlock: bool, ntsc_60hz: bool| {
            let output = t.path().join(name).join("EBOOT.PBP");
            let patch_choice = vec![("Adaptive VSync (a late frame does not wait)".to_owned(), fps_unlock)];
            let disc = DiscInput { path: cue.clone(), disc_id: Some("SLES-02895".into()), title_fixes, fps_unlock, ntsc_60hz, patch_choice, ..Default::default() };
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
        // Adaptive VSync: blez -> nop, sltiu v1,a0,1 / addu v0,v0,v1 (target last+n, at least 1),
        // budget a1 = n (at least 1), the second wait for the next vblank dropped.
        let s22: &[u8; SECTOR] = on[22 * SECTOR..23 * SECTOR].try_into().unwrap();
        let v = |i: usize| u32::from_le_bytes(s22[24 + 0x670 + 4 * i..28 + 0x670 + 4 * i].try_into().unwrap());
        let mut want = VSYNC;
        for (i, w) in [(3, 0), (7, 0x2C83_0001), (8, 0x0043_1021), (14, 0x2405_0001), (15, 0x0080_2821), (25, 0)] {
            want[i] = w;
        }
        assert_eq!((0..27).map(v).collect::<Vec<_>>(), want, "adaptive VSync");
        assert!(verify_edc(s22));
        let diff = on[..raw.len()].iter().zip(&raw).filter(|(a, b)| a != b).count();
        assert!(diff <= 6 + 24 + 3 * (4 + 276), "only the code bytes + EDC/ECC change ({diff} bytes)");

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
        convert(&ConvertRequest { discs: vec![disc], patches: db.clone(), title: "T".into(), output: out2.clone(), donor: None, artwork: Artwork::default(), level: 1 }, &()).unwrap();
        let tracks = DiscImage::open(&cue).unwrap().tracks;
        let expect = subq::load_beside(&cue, &tracks).unwrap().unwrap();
        assert_eq!((expect.sectors, expect.damaged), (2, 2));
        assert_eq!(inspect(&out2).unwrap().discs[0].subchannels, expect.table);
        // LBA 155 (00:02:05) answers 154: rel 00:00:04, abs 00:02:04; sorted before LBA 160.
        assert_eq!(&expect.table[12..24], &[5, 0, 0, 0, 1, 1, 0, 0, 4, 0, 2, 4]);

        // No file beside: the built-in SBI of the disc's product code; none when the set has two
        // dumps for it (Spyro: Year of the Dragon v1.0 / v1.1) and the database no magic word.
        std::fs::remove_file(t.path().join("g.sbi")).unwrap();
        for (id, name) in [("SCES-01494", "m"), ("SCES-02835", "y")] {
            let disc = DiscInput { path: cue.clone(), disc_id: Some(id.into()), ..Default::default() };
            let out3 = t.path().join(name).join("EBOOT.PBP");
            convert(&ConvertRequest { discs: vec![disc], patches: db.clone(), title: "T".into(), output: out3.clone(), donor: None, artwork: Artwork::default(), level: 1 }, &()).unwrap();
            let want = match sbidb::lookup(id) {
                [s] => subq::table(&s.entries, &tracks),
                _ => Vec::new(),
            };
            assert_eq!(want.len(), if name == "m" { 18 * psar::SUBQ_ENTRY } else { 0 }, "{id}");
            assert_eq!(inspect(&out3).unwrap().discs[0].subchannels, want, "{id}");
        }
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

    #[test]
    fn native_eboot_is_an_mg_game_running_the_runtime() {
        let t = tempfile::tempdir().unwrap();
        // 40 sectors = blocks of 16, 16 and 8; the second one is noise, which deflate cannot shrink.
        let mut raw: Vec<u8> = (0..40u32).flat_map(|lba| patching::sector::build_mode2_form1(lba, &[lba as u8; 2048])).collect();
        let mut x = 0x1234_5678u32;
        for b in &mut raw[16 * SECTOR..32 * SECTOR] {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        std::fs::write(t.path().join("g.bin"), &raw).unwrap();
        let cue = touch(t.path(), "g.cue", "FILE \"g.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n");
        let output = t.path().join("NATIVE").join("EBOOT.PBP");
        let icon0 = b"\x89PNG icon".to_vec();
        let req = ConvertRequest {
            discs: vec![DiscInput { path: cue.clone(), disc_id: Some("SLES-02895".into()), ..Default::default() }],
            patches: PatchDb::default(),
            title: "Sheep Dog 'n' Wolf".into(),
            output: output.clone(),
            donor: None,
            artwork: Artwork { icon0: Some(icon0.clone()), ..Default::default() },
            level: 1,
        };
        let rep = convert_native(&req, &()).unwrap();
        assert_eq!(rep.disc_ids, ["SLES02895"]);
        assert!(!output.with_extension("PBP.part").exists());

        let mut pbp = PbpFile::open(File::open(&output).unwrap()).unwrap();
        let sfo = Sfo::parse(&pbp.read(Section::ParamSfo).unwrap()).unwrap();
        assert_eq!(sfo.get_str("CATEGORY"), Some("MG"));
        assert_eq!(sfo.get_str("DISC_ID"), Some("SLES02895"));
        assert_eq!(sfo.get_str("TITLE"), Some("Sheep Dog 'n' Wolf"));
        assert_eq!(sfo.get_int("BOOTABLE"), Some(1));
        assert_eq!(pbp.read(Section::Icon0).unwrap(), icon0);
        assert!(pbp.read(Section::Pic1).unwrap().is_empty());

        // DATA.PSP: the runtime, a MIPS ELF of type PRX (0xFFA0), then padding up to DATA.PSAR.
        let psp = pbp.read(Section::DataPsp).unwrap();
        assert_eq!(psp[..NATIVE_RUNTIME.len()], *NATIVE_RUNTIME);
        assert!(psp[NATIVE_RUNTIME.len()..].iter().all(|&b| b == 0) && psp.len() - NATIVE_RUNTIME.len() < 0x10);
        let half = |at: usize| u16::from_le_bytes([psp[at], psp[at + 1]]);
        assert_eq!(psp[..6], *b"\x7FELF\x01\x01", "32-bit little-endian ELF");
        assert_eq!((half(0x10), half(0x12)), (0xFFA0, 8), "PRX for MIPS");

        let psar = pbp.read(Section::DataPsar).unwrap();
        assert_eq!(pbp.range(Section::DataPsar).start % 0x10, 0);
        let head = native_psar_header("SLES02895", "Sheep Dog 'n' Wolf", 40, 0);
        assert_eq!(psar[..NATIVE_PSAR_HEADER_LEN], head);
        assert_eq!(psar[..16], *b"PSXRTDAT\x05\0\0\0\xD0\0\0\0");
        assert_eq!(&psar[0x10..0x1A], b"SLES02895\0");
        assert_eq!(&psar[0x20..0x33], b"Sheep Dog 'n' Wolf\0");
        let word = |at: usize| u32::from_le_bytes(psar[at..at + 4].try_into().unwrap()) as usize;
        assert_eq!([word(0xA0), word(0xA4), word(0xA8), word(0xAC)], [40, 16, 0xD0, 3]);
        assert_eq!([word(0xB0), word(0xB4), word(0xB8), word(0xBC)], [0; 4], "no filesystem, no code");
        assert_eq!(word(0xC0), 0, "no flags");
        assert_eq!([word(0xC8), word(0xCC)], [0, 0], "one disc, no disc table");

        // The index bounds every block; each one gives back its sectors, the noise stored as is.
        let index: Vec<usize> = (0..4).map(|i| word(0xD0 + 4 * i)).collect();
        assert_eq!(index[0], 0xD0 + 16);
        assert_eq!(index[3], psar.len());
        let mut back = Vec::new();
        for (i, span) in index.windows(2).enumerate() {
            let data = &psar[span[0]..span[1]];
            let len = (40 - 16 * i).min(16) * SECTOR;
            if data.len() == len {
                assert_eq!(i, 1, "only the noise is stored");
                back.extend_from_slice(data);
            } else {
                back.extend(miniz_oxide::inflate::decompress_to_vec(data).unwrap());
            }
        }
        assert_eq!(back, raw);
        assert_eq!((rep.stats.blocks, rep.stats.raw_bytes, rep.stats.stored_bytes), (3, raw.len() as u64, psar.len() as u64));

        // PAL60: a `pal60` game with the 60 Hz switch on gets the flag; the switch alone does not.
        let pal60 = PatchDb::parse("[game SLES-02895]\npal60 = yes\n", patchdb::Origin::User).unwrap();
        let flags = |patches: &PatchDb, ntsc_60hz: bool| {
            let disc = DiscInput { ntsc_60hz, ..req.discs[0].clone() };
            let r = ConvertRequest { discs: vec![disc], patches: patches.clone(), title: req.title.clone(), output: output.clone(), donor: None, artwork: Artwork::default(), level: 1 };
            convert_native(&r, &()).unwrap();
            let psar = PbpFile::open(File::open(&output).unwrap()).unwrap().read(Section::DataPsar).unwrap();
            u32::from_le_bytes(psar[0xC0..0xC4].try_into().unwrap())
        };
        assert_eq!([flags(&pal60, true), flags(&pal60, false), flags(&PatchDb::default(), true)], [NATIVE_FLAG_PAL60, 0, 0]);

        // Two discs: the first as before, the second (20 sectors, its own ID) after it, and a table
        // of both. Each disc's index gives back its own sectors.
        let raw2: Vec<u8> = (0..20u32).flat_map(|lba| patching::sector::build_mode2_form1(lba, &[0xA0 | lba as u8; 2048])).collect();
        std::fs::write(t.path().join("h.bin"), &raw2).unwrap();
        let cue2 = touch(t.path(), "h.cue", "FILE \"h.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n");
        let disc2 = DiscInput { path: cue2, disc_id: Some("SLES-12895".into()), ..Default::default() };
        let two = ConvertRequest { discs: vec![req.discs[0].clone(), disc2], ..req.clone() };
        let rep = convert_native(&two, &()).unwrap();
        assert_eq!(rep.disc_ids, ["SLES02895", "SLES12895"]);
        let psar_flags = || u32::from_le_bytes(PbpFile::open(File::open(&output).unwrap()).unwrap().read(Section::DataPsar).unwrap()[0xC0..0xC4].try_into().unwrap());
        assert_eq!(psar_flags(), 0, "two discs change by themselves");
        let manual = ConvertRequest { patches: PatchDb::parse("[game SLES-02895]\ndisc_swap = manual\n", patchdb::Origin::User).unwrap(), ..two.clone() };
        convert_native(&manual, &()).unwrap();
        assert_eq!(psar_flags(), NATIVE_FLAG_MANUAL_DISC);
        convert_native(&two, &()).unwrap();
        let psar = PbpFile::open(File::open(&output).unwrap()).unwrap().read(Section::DataPsar).unwrap();
        let word = |at: usize| u32::from_le_bytes(psar[at..at + 4].try_into().unwrap()) as usize;
        assert_eq!(psar[..NATIVE_PSAR_HEADER_LEN - 8], head[..NATIVE_PSAR_HEADER_LEN - 8], "the first disc's header as for one disc");
        let (table, discs) = (word(0xC8), word(0xCC));
        assert_eq!((discs, table + 2 * NATIVE_DISC_ENTRY_LEN), (2, psar.len()));
        let back = |entry: usize| {
            let e = table + entry * NATIVE_DISC_ENTRY_LEN;
            let (sectors, index, blocks) = (word(e + 16), word(e + 20), word(e + 24));
            assert_eq!((blocks, word(e + 28)), (sectors.div_ceil(16), 0));
            let mut out = Vec::new();
            for i in 0..blocks {
                let (s, e) = (word(index + 4 * i), word(index + 4 * i + 4));
                let len = (sectors - 16 * i).min(16) * SECTOR;
                out.extend(if e - s == len { psar[s..e].to_vec() } else { miniz_oxide::inflate::decompress_to_vec(&psar[s..e]).unwrap() });
            }
            (psar[e..e + 16].to_vec(), out)
        };
        assert_eq!(word(table + 20), 0xD0, "entry 0 is the first disc");
        assert_eq!(back(0), (b"SLES02895\0\0\0\0\0\0\0".to_vec(), raw.clone()));
        assert_eq!(back(1), (b"SLES12895\0\0\0\0\0\0\0".to_vec(), raw2));
        assert_eq!((rep.stats.blocks, rep.stats.raw_bytes), (5, (raw.len() + 20 * SECTOR) as u64));

        let six = ConvertRequest { discs: vec![req.discs[0].clone(); NATIVE_MAX_DISCS + 1], ..req };
        assert!(convert_native(&six, &()).is_err(), "at most NATIVE_MAX_DISCS");
    }

    #[test]
    fn native_psar_header_cuts_long_fields() {
        let h = native_psar_header("ABCDEFGHIJKLMNOPQRS", &"é".repeat(100), 0, NATIVE_FLAG_PAL60);
        assert_eq!(h[0xC0..0xC4], NATIVE_FLAG_PAL60.to_le_bytes());
        assert_eq!(h.len(), NATIVE_PSAR_HEADER_LEN);
        assert_eq!(&h[0x10..0x20], b"ABCDEFGHIJKLMNO\0");
        // 63 two-byte chars fit in 127 bytes; the rest of the field is NUL.
        assert_eq!(std::str::from_utf8(&h[0x20..0x20 + 126]).unwrap(), "é".repeat(63));
        assert!(h[0x20 + 126..0xA0].iter().all(|&b| b == 0));
    }
}
