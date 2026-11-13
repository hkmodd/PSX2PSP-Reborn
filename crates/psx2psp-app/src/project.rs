//! Project persistence: continuous autosave plus named projects ("slots").
//!
//! **Model.** The app always has a *current slot*: `_autosave` for an unnamed session, or the
//! slug of a named project. Every change is written there ~1 s after the last edit (and on
//! window close), so a crash or an accidental close never loses work — a finished YouTube clip
//! (ICON1.PMF + SND0.AT3, slow to make) included. "Save as…" names the session (or forks a
//! named one); from then on that project is the one kept up to date. On launch the slot that was
//! open last is reopened automatically (see [`boot`]).
//!
//! **Storage** (under [`root`]): `projects/<slug>/project.json` (versioned, [`VERSION`]) next to
//! content-addressed media files (`icon1-<fnv64>.pmf`, `snd0-….at3`, `icon0-….png`, …), and
//! `projects/session.json` naming the slot that was open. A save writes new media files first,
//! then commits by atomically replacing `project.json`, then deletes media no longer referenced:
//! whatever fails or crashes midway, the previous `project.json` and every file it names stay
//! intact. All project I/O goes through one lock with per-slot sequence numbers, so an older
//! snapshot can never overwrite a newer one.
//!
//! **Artwork** is stored as the normalised slot PNG — the exact bytes the EBOOT gets — rather
//! than re-derived from its source path: imported art has no path, the GUI keeps only the file
//! name, and sources move. They are small (≤ a few hundred KB). Generated media (ICON1/SND0) is
//! stored verbatim; its preview (frames, PCM) is re-decoded on load.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iced::widget::{button, column, container, image, row, scrollable, text, text_input};
use iced::{Alignment, Color, Element, Fill, Subscription, Task, window};
use psx2psp_engine::media::clip::ClipMode;
use psx2psp_engine::media::images::{self, Slot};
use psx2psp_engine::{self as engine, patchdb};
use serde::{Deserialize, Serialize};

use crate::{
    App, Art, ClipForm, Disc, Icon1, Kind, LocalTrack, Media, Message, Origin, Snd0, style, worker,
};

/// Format version written into every `project.json`.
pub const VERSION: u32 = 1;
const AUTOSAVE: &str = "_autosave";
const PROJECT_FILE: &str = "project.json";
const SESSION_FILE: &str = "session.json";
const DEBOUNCE: Duration = Duration::from_secs(1);
const NAME_MAX: usize = 80;
/// Media files: (kind, extension); art kinds are indexed like `App::art`.
const ART_KINDS: [&str; 4] = ["icon0", "pic0", "pic1", "boot"];

// ───────────────────────────────── data ─────────────────────────────────

/// One disc as configured (the probe result is re-derived on load).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscFile {
    pub path: PathBuf,
    pub id: String,
    pub ppf: Option<PathBuf>,
    pub pal_ntsc: bool,
    pub title_fixes: bool,
    pub fps_unlock: bool,
    pub ntsc_60hz: bool,
    pub seeded_for: Option<String>,
    pub patch_choice: Vec<(String, bool)>,
}

impl Default for DiscFile {
    fn default() -> Self {
        Self::from(&Disc::new(0, PathBuf::new()))
    }
}

impl From<&Disc> for DiscFile {
    fn from(d: &Disc) -> Self {
        Self {
            path: d.path.clone(),
            id: d.id.clone(),
            ppf: d.ppf.clone(),
            pal_ntsc: d.pal_ntsc,
            title_fixes: d.title_fixes,
            fps_unlock: d.fps_unlock,
            ntsc_60hz: d.ntsc_60hz,
            seeded_for: d.seeded_for.clone(),
            patch_choice: d.patch_choice.clone(),
        }
    }
}

impl DiscFile {
    fn to_disc(&self, key: u64) -> Disc {
        let mut d = Disc::new(key, self.path.clone());
        d.id.clone_from(&self.id);
        d.ppf.clone_from(&self.ppf);
        (d.pal_ntsc, d.title_fixes, d.fps_unlock, d.ntsc_60hz) = (self.pal_ntsc, self.title_fixes, self.fps_unlock, self.ntsc_60hz);
        // Restoring `seeded_for` keeps the probe from resetting the switches to the DB defaults.
        d.seeded_for.clone_from(&self.seeded_for);
        d.patch_choice.clone_from(&self.patch_choice);
        d
    }
}

/// The clip editor, including the last probe (re-probing a URL needs the network).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipFile {
    /// "web" or "files".
    pub origin: String,
    pub video: LocalTrack,
    pub music: LocalTrack,
    pub source: String,
    pub start: String,
    pub end: String,
    pub mode: String,
    pub kbps: u32,
    pub len: Option<f64>,
    pub title: Option<String>,
    pub probed: bool,
}

impl Default for ClipFile {
    fn default() -> Self {
        Self::from(&ClipForm::default())
    }
}

impl From<&ClipForm> for ClipFile {
    fn from(c: &ClipForm) -> Self {
        Self {
            origin: if c.origin == Origin::Files {
                "files"
            } else {
                "web"
            }
            .into(),
            video: c.video.clone(),
            music: c.music.clone(),
            source: c.source.clone(),
            start: c.start.clone(),
            end: c.end.clone(),
            mode: mode_str(c.mode).into(),
            kbps: c.kbps,
            len: c.len,
            title: c.title.clone(),
            probed: c.probed,
        }
    }
}

impl ClipFile {
    fn to_form(&self) -> ClipForm {
        ClipForm {
            origin: if self.origin == "files" {
                Origin::Files
            } else {
                Origin::Web
            },
            video: self.video.clone(),
            music: self.music.clone(),
            source: self.source.clone(),
            start: self.start.clone(),
            end: self.end.clone(),
            mode: parse_mode(&self.mode),
            kbps: self.kbps,
            len: self.len,
            title: self.title.clone(),
            probed: self.probed,
        }
    }
}

fn mode_str(m: ClipMode) -> &'static str {
    match m {
        ClipMode::Both => "both",
        ClipMode::Icon1 => "icon1",
        ClipMode::Snd0 => "snd0",
    }
}

fn parse_mode(s: &str) -> ClipMode {
    ClipMode::ALL.into_iter().find(|&m| mode_str(m) == s).unwrap_or_default()
}

/// A media payload kept in the slot folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Blob {
    /// What the GUI shows as its origin (file name or clip label).
    pub source: String,
    pub bytes: Arc<Vec<u8>>,
}

impl From<&Media> for Blob {
    fn from(m: &Media) -> Self {
        Self { source: m.source.clone(), bytes: m.bytes.clone() }
    }
}

/// Everything the user configured, in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// Display name (empty for the unnamed session).
    pub name: String,
    pub title: String,
    pub title_auto: bool,
    pub discs: Vec<DiscFile>,
    /// Normalised slot PNGs, indexed like `App::art` (ICON0, PIC0, PIC1, BOOT).
    pub art: [Option<Blob>; 4],
    pub icon1: Option<Blob>,
    pub snd0: Option<Blob>,
    pub clip: ClipFile,
    pub output: Option<PathBuf>,
    pub level: u32,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            name: String::new(),
            title: String::new(),
            title_auto: true,
            discs: Vec::new(),
            art: Default::default(),
            icon1: None,
            snd0: None,
            clip: ClipFile::default(),
            output: None,
            level: engine::DEFAULT_LEVEL,
        }
    }
}

impl Project {
    /// Nothing worth restoring (output/level alone don't count).
    pub fn is_blank(&self) -> bool {
        self.title.trim().is_empty()
            && self.discs.is_empty()
            && self.art.iter().all(Option::is_none)
            && self.icon1.is_none()
            && self.snd0.is_none()
            && self.clip.source.trim().is_empty()
            && !self.clip.video.is_set()
            && !self.clip.music.is_set()
    }
}

// ─────────────────────────────── on disk ───────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlobRef {
    source: String,
    file: String,
    size: u64,
    fnv: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ArtRefs {
    icon0: Option<BlobRef>,
    pic0: Option<BlobRef>,
    pic1: Option<BlobRef>,
    boot: Option<BlobRef>,
}

impl ArtRefs {
    fn into_array(self) -> [Option<BlobRef>; 4] {
        [self.icon0, self.pic0, self.pic1, self.boot]
    }
}

/// `project.json`, format v1. Unknown fields are ignored and missing ones take their defaults,
/// so additive changes don't need a version bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct FileV1 {
    version: u32,
    app: String,
    name: String,
    /// Unix seconds.
    saved_at: u64,
    title: String,
    title_auto: bool,
    discs: Vec<DiscFile>,
    art: ArtRefs,
    icon1: Option<BlobRef>,
    snd0: Option<BlobRef>,
    clip: ClipFile,
    output: Option<PathBuf>,
    level: u32,
}

impl Default for FileV1 {
    fn default() -> Self {
        let p = Project::default();
        Self {
            version: 0,
            app: String::new(),
            name: p.name,
            saved_at: 0,
            title: p.title,
            title_auto: p.title_auto,
            discs: p.discs,
            art: ArtRefs::default(),
            icon1: None,
            snd0: None,
            clip: p.clip,
            output: p.output,
            level: p.level,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionFile {
    version: u32,
    current: String,
}

/// FNV-1a 64: stable across builds (names the media files, checks them on load).
fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01b3))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn io_err(p: &Path, e: std::io::Error) -> String {
    format!("{}: {e}", p.display())
}

fn tmp_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(".{}.tmp", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())))
}

/// Writes `bytes` to a sibling temp file, flushes it to disk, then renames it over `path`:
/// readers see the old file or the new one, never a torn one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = tmp_path(path);
    let r = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r.map_err(|e| io_err(path, e))
}

/// Serialises every write to project storage; per slot folder, the sequence number of the last
/// operation applied. Older snapshots arriving late are dropped.
fn gated<T>(dir: &Path, seq: u64, f: impl FnOnce() -> Result<T, String>) -> Result<Option<T>, String> {
    static LAST: Mutex<BTreeMap<PathBuf, u64>> = Mutex::new(BTreeMap::new());
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.get(dir).is_some_and(|&s| s >= seq) {
        return Ok(None);
    }
    let r = f()?;
    last.insert(dir.to_path_buf(), seq);
    Ok(Some(r))
}

/// A media file name we wrote: `<kind>-<16 hex>.<ext>` (or a leftover temp file).
fn is_ours(name: &str) -> bool {
    if name.starts_with('.') && name.ends_with(".tmp") {
        return true;
    }
    let Some((stem, ext)) = name.rsplit_once('.') else { return false };
    let Some((kind, hash)) = stem.split_once('-') else { return false };
    let known = ART_KINDS.contains(&kind) && ext == "png" || kind == "icon1" && ext == "pmf" || kind == "snd0" && ext == "at3";
    known && hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Saves `p` into the slot folder `dir` (see the module docs for the commit order). `Ok(false)`:
/// skipped because a newer operation on this slot already ran.
pub fn save(dir: &Path, p: &Project, seq: u64) -> Result<bool, String> {
    gated(dir, seq, || write_project(dir, p)).map(|r| r.is_some())
}

fn write_project(dir: &Path, p: &Project) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let mut keep = vec![PROJECT_FILE.to_owned()];
    let mut put = |kind: &str, ext: &str, b: &Option<Blob>| -> Result<Option<BlobRef>, String> {
        let Some(b) = b else { return Ok(None) };
        let h = fnv(&b.bytes);
        let file = format!("{kind}-{h:016x}.{ext}");
        let path = dir.join(&file);
        // Content-addressed: an existing file of the right size already holds these bytes.
        if fs::metadata(&path).map_or(true, |m| m.len() != b.bytes.len() as u64) {
            write_atomic(&path, &b.bytes)?;
        }
        keep.push(file.clone());
        Ok(Some(BlobRef { source: b.source.clone(), file, size: b.bytes.len() as u64, fnv: format!("{h:016x}") }))
    };
    let art = ArtRefs {
        icon0: put(ART_KINDS[0], "png", &p.art[0])?,
        pic0: put(ART_KINDS[1], "png", &p.art[1])?,
        pic1: put(ART_KINDS[2], "png", &p.art[2])?,
        boot: put(ART_KINDS[3], "png", &p.art[3])?,
    };
    let icon1 = put("icon1", "pmf", &p.icon1)?;
    let snd0 = put("snd0", "at3", &p.snd0)?;
    let file = FileV1 {
        version: VERSION,
        app: concat!("psx2psp-reborn ", env!("CARGO_PKG_VERSION")).into(),
        name: p.name.clone(),
        saved_at: now_secs(),
        title: p.title.clone(),
        title_auto: p.title_auto,
        discs: p.discs.clone(),
        art,
        icon1,
        snd0,
        clip: p.clip.clone(),
        output: p.output.clone(),
        level: p.level,
    };
    let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
    write_atomic(&dir.join(PROJECT_FILE), &json)?;
    // Committed: media the new project.json no longer names can go.
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !keep.contains(&n) && is_ours(&n) {
            let _ = fs::remove_file(e.path());
        }
    }
    Ok(())
}

/// Deletes a slot folder (through the same gate as saves).
fn remove(dir: &Path, seq: u64) -> Result<(), String> {
    gated(dir, seq, || match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_err(dir, e)),
        _ => Ok(()),
    })
    .map(drop)
}

fn parse(bytes: &[u8]) -> Result<FileV1, String> {
    #[derive(Deserialize)]
    struct Head {
        version: Option<u32>,
    }
    let head: Head = serde_json::from_slice(bytes).map_err(|e| format!("not a project file ({e})"))?;
    match head.version {
        None | Some(0) => Err("not a PSX2PSP-Reborn project (no format version)".into()),
        Some(v) if v > VERSION => Err(format!("saved by a newer PSX2PSP-Reborn (format v{v}; this build reads up to v{VERSION}) — update the app")),
        Some(_) => serde_json::from_slice(bytes).map_err(|e| format!("damaged project file ({e})")),
    }
}

fn read_file(dir: &Path) -> Result<FileV1, String> {
    let path = dir.join(PROJECT_FILE);
    parse(&fs::read(&path).map_err(|e| io_err(&path, e))?)
}

/// Loads a slot. Missing or damaged pieces don't fail the load: they are left out (a disc whose
/// image moved is kept, so the GUI shows it in red) and described in the returned warnings.
pub fn load(dir: &Path) -> Result<(Project, Vec<String>), String> {
    let f = read_file(dir)?;
    let mut warn = Vec::new();
    let mut get = |what: &str, r: Option<BlobRef>| -> Option<Blob> {
        let r = r?;
        // Only plain file names inside the slot folder (a hand-edited file can't reach elsewhere).
        if Path::new(&r.file).file_name().is_none_or(|n| n != r.file.as_str()) || r.file.starts_with('.') {
            warn.push(format!("{what}: invalid file name in the project"));
            return None;
        }
        match fs::read(dir.join(&r.file)) {
            Ok(b) if b.len() as u64 == r.size && format!("{:016x}", fnv(&b)) == r.fnv => Some(Blob { source: r.source, bytes: Arc::new(b) }),
            Ok(_) => {
                warn.push(format!("{what} ({}) is damaged and was left out", r.source));
                None
            }
            Err(_) => {
                warn.push(format!("{what} ({}) is missing from the project folder", r.source));
                None
            }
        }
    };
    let names = ["ICON0", "PIC0", "PIC1", "BOOT"];
    let art_refs = f.art.into_array();
    let mut art: [Option<Blob>; 4] = Default::default();
    for (i, r) in art_refs.into_iter().enumerate() {
        art[i] = get(names[i], r);
    }
    let icon1 = get("ICON1.PMF", f.icon1);
    let snd0 = get("SND0.AT3", f.snd0);
    let mut discs = Vec::new();
    for mut d in f.discs.into_iter().filter(|d| !d.path.as_os_str().is_empty()) {
        if !d.path.is_file() {
            warn.push(format!("disc image not found: {}", d.path.display()));
        }
        if let Some(p) = d.ppf.take_if(|p| !p.is_file()) {
            warn.push(format!("PPF patch not found, removed: {}", p.display()));
        }
        discs.push(d);
    }
    let p = Project { name: f.name, title: f.title, title_auto: f.title_auto, discs, art, icon1, snd0, clip: f.clip, output: f.output, level: f.level.clamp(1, 9) };
    Ok((p, warn))
}

// ─────────────────────────────── storage ───────────────────────────────

/// `projects/` beside the executable when that folder exists or `patches.ini` sits there
/// (portable install), else in the per-user config folder — the same rule as `patches.ini`
/// ([`patchdb::user_path`]): `%APPDATA%\PSX2PSP-Reborn\projects` on Windows.
pub fn root() -> Option<PathBuf> {
    if let Some(p) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("projects")))
        && p.is_dir()
    {
        return Some(p);
    }
    Some(patchdb::user_path()?.parent()?.join("projects"))
}

/// Folder-safe slug: lowercase ASCII letters/digits and single dashes, ≤ 48 chars. Never starts
/// with `_` (reserved for `_autosave`) nor is a Windows device name; a name with no ASCII
/// letters/digits gets a stable hash-based slug.
pub fn slugify(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        if s.len() >= 48 {
            break;
        }
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-');
    if s.is_empty() {
        return format!("project-{:08x}", fnv(name.trim().as_bytes()) as u32);
    }
    let device = matches!(s, "con" | "prn" | "aux" | "nul")
        || (s.len() == 4 && (s.starts_with("com") || s.starts_with("lpt")) && s.as_bytes()[3].is_ascii_digit());
    if device { format!("{s}-project") } else { s.to_owned() }
}

/// A slug as found on disk or in a message: `_autosave` or what [`slugify`] produces.
fn valid_slug(s: &str) -> bool {
    s == AUTOSAVE || (!s.is_empty() && s.len() <= 64 && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

/// Display name: trimmed, single-spaced, no control characters, ≤ 80 chars.
pub fn clean_name(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ").chars().take(NAME_MAX).collect()
}

fn read_session(root: &Path) -> Option<String> {
    let s: SessionFile = serde_json::from_slice(&fs::read(root.join(SESSION_FILE)).ok()?).ok()?;
    valid_slug(&s.current).then_some(s.current)
}

fn write_session(root: &Path, slug: &str) -> Result<(), String> {
    fs::create_dir_all(root).map_err(|e| io_err(root, e))?;
    let json = serde_json::to_vec_pretty(&SessionFile { version: VERSION, current: slug.into() }).map_err(|e| e.to_string())?;
    write_atomic(&root.join(SESSION_FILE), &json)
}

/// Saves (or, for `None`, deletes) a slot, then records it as the open one if asked.
fn persist(root: &Path, slot: &str, project: Option<Project>, seq: u64, session: Option<String>) -> Result<(), String> {
    let dir = root.join(slot);
    match project {
        Some(p) => drop(save(&dir, &p, seq)?),
        None => remove(&dir, seq)?,
    }
    session.map_or(Ok(()), |s| write_session(root, &s))
}

/// A named project in the Open list.
#[derive(Debug, Clone)]
pub struct Entry {
    pub slug: String,
    pub name: String,
    pub title: String,
    pub saved_at: u64,
    pub discs: usize,
    pub media: bool,
    pub thumb: Option<image::Handle>,
    /// Why it can't be opened (e.g. saved by a newer version); it can still be deleted.
    pub error: Option<String>,
}

/// Named projects, newest first.
pub fn list(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        let slug = e.file_name().to_string_lossy().into_owned();
        if slug == AUTOSAVE || !valid_slug(&slug) || !e.path().join(PROJECT_FILE).is_file() {
            continue;
        }
        let dir = e.path();
        out.push(match read_file(&dir) {
            Ok(f) => Entry {
                name: if f.name.trim().is_empty() { slug.clone() } else { f.name },
                title: f.title,
                saved_at: f.saved_at,
                discs: f.discs.len(),
                media: f.icon1.is_some() || f.snd0.is_some(),
                thumb: f.art.icon0.and_then(|r| fs::read(dir.join(r.file)).ok()).map(image::Handle::from_bytes),
                error: None,
                slug,
            },
            Err(err) => Entry { name: slug.clone(), title: String::new(), saved_at: 0, discs: 0, media: false, thumb: None, error: Some(err), slug },
        });
    }
    out.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then_with(|| a.name.cmp(&b.name)));
    out
}

fn unique_slug(root: &Path, base: &str) -> String {
    let mut s = base.to_owned();
    let mut n = 2;
    while root.join(&s).exists() {
        s = format!("{base}-{n}");
        n += 1;
    }
    s
}

/// Turns the unnamed session into a named project ("Recovered — <title>") instead of letting
/// the new session overwrite it. Returns the new name.
fn preserve_autosave(root: &Path) -> Result<String, String> {
    let dir = root.join(AUTOSAVE);
    let path = dir.join(PROJECT_FILE);
    let raw = fs::read(&path).map_err(|e| io_err(&path, e))?;
    let mut value: Option<serde_json::Value> = serde_json::from_slice(&raw).ok();
    let title = value.as_ref().and_then(|v| v.get("title")).and_then(|t| t.as_str()).map(clean_name).unwrap_or_default();
    let name = if title.is_empty() { "Recovered session".to_owned() } else { clean_name(&format!("Recovered — {title}")) };
    // The JSON is edited as a value so fields of a newer format survive.
    if let Some(o) = value.as_mut().and_then(|v| v.as_object_mut()) {
        o.insert("name".into(), name.clone().into());
        write_atomic(&path, &serde_json::to_vec_pretty(o).map_err(|e| e.to_string())?)?;
    }
    let target = root.join(unique_slug(root, &slugify(&name)));
    fs::rename(&dir, &target).map_err(|e| io_err(&dir, e))?;
    write_session(root, AUTOSAVE)?;
    Ok(name)
}

/// `YYYY-MM-DD` (UTC) of a Unix time.
fn date(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn ago(secs: u64, now: u64) -> String {
    match now.saturating_sub(secs) {
        d if d < 60 => "just now".into(),
        d if d < 3_600 => format!("{} min ago", d / 60),
        d if d < 86_400 => format!("{} h ago", d / 3_600),
        d if d < 30 * 86_400 => format!("{} d ago", d / 86_400),
        _ => date(secs),
    }
}

// ─────────────────────────────── restore ───────────────────────────────

/// A loaded slot with its previews decoded (off the UI thread).
#[derive(Debug, Clone)]
pub struct Restored {
    slug: String,
    project: Project,
    art: [Option<Art>; 4],
    icon1: Option<Icon1>,
    snd0: Option<Snd0>,
    warnings: Vec<String>,
}

fn open_slot(root: &Path, slug: &str) -> Result<Restored, String> {
    let (project, mut warnings) = load(&root.join(slug))?;
    let mut art: [Option<Art>; 4] = Default::default();
    for slot in Slot::ALL {
        let i = crate::slot_ix(slot);
        if let Some(b) = &project.art[i] {
            match images::prepare(slot, &b.bytes) {
                Ok(si) => art[i] = Some(Art::from_slot_image(b.source.clone(), si)),
                Err(e) => warnings.push(format!("{}: {e}", slot.label())),
            }
        }
    }
    let icon1 = project.icon1.as_ref().and_then(|b| crate::icon1_from(b.source.clone(), b.bytes.to_vec()).map_err(|e| warnings.push(format!("ICON1.PMF: {e}"))).ok());
    let snd0 = project.snd0.as_ref().and_then(|b| crate::snd0_from(b.source.clone(), b.bytes.to_vec()).map_err(|e| warnings.push(format!("SND0.AT3: {e}"))).ok());
    Ok(Restored { slug: slug.into(), project, art, icon1, snd0, warnings })
}

/// What launching found.
#[derive(Debug, Clone)]
pub enum Boot {
    Nothing,
    Restore(Box<Restored>),
    /// The unnamed session was kept as a named project (name, why it wasn't reopened).
    Kept(String, Option<String>),
    /// The last named project couldn't be reopened (it is left untouched).
    Failed(String),
}

fn boot_load(root: &Path, fresh: bool) -> Result<Boot, String> {
    let current = read_session(root).unwrap_or_else(|| AUTOSAVE.into());
    if !root.join(&current).join(PROJECT_FILE).is_file() {
        return Ok(Boot::Nothing);
    }
    let why = if fresh {
        None
    } else {
        match open_slot(root, &current) {
            Ok(r) => return Ok(Boot::Restore(Box::new(r))),
            Err(e) if current != AUTOSAVE => return Ok(Boot::Failed(format!("Couldn't reopen project \"{current}\": {e}"))),
            Err(e) => Some(e),
        }
    };
    if current != AUTOSAVE {
        return Ok(Boot::Nothing); // a named project is safe where it is
    }
    preserve_autosave(root).map(|name| Boot::Kept(name, why))
}

// ──────────────────────────────── state ────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
enum Phase {
    /// Looking for the last session; autosave waits so it can't overwrite it.
    #[default]
    Booting,
    Ready,
    /// No storage (no user folder, or the last session couldn't be set aside).
    Off(String),
}

#[derive(Debug, Clone)]
enum Pending {
    New,
    Open(String),
}

#[derive(Debug, Clone, Default)]
enum Panel {
    #[default]
    None,
    SaveAs(String),
    /// The second click on ✕ deletes.
    Open(Option<String>),
    Discard(Pending),
}

/// Project-bar state (one field on `App`).
#[derive(Default)]
pub struct State {
    root: Option<PathBuf>,
    /// Slot autosave writes to: [`AUTOSAVE`] or a named project's slug.
    current: String,
    name: String,
    phase: Phase,
    /// Fingerprint of the last saved (or restored) state.
    fp: u64,
    boot_fp: u64,
    dirty: bool,
    changed: Option<Instant>,
    timer: bool,
    in_flight: u32,
    seq: u64,
    error: Option<String>,
    /// The slot `session.json` is known to name.
    session: Option<String>,
    closing: bool,
    busy: Option<&'static str>,
    panel: Panel,
    entries: Option<Vec<Entry>>,
}

impl State {
    fn named(&self) -> bool {
        self.current != AUTOSAVE
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

#[derive(Debug, Clone)]
pub enum Msg {
    Booted(Result<Boot, String>),
    Tick,
    Saved(Result<(), String>),
    New,
    ShowOpen,
    ShowSaveAs,
    ClosePanel,
    Listed(Vec<Entry>),
    Open(String),
    Opened(Result<Box<Restored>, String>),
    Discard,
    Name(String),
    SaveAs,
    /// Previous (slot, name), restored if the save failed.
    SavedAs((String, String), Result<(), String>),
    Delete(String),
    Deleted(String, Result<Vec<Entry>, String>),
    CloseRequested,
}

fn msg(m: Msg) -> Message {
    Message::Project(m)
}

/// Cheap identity of everything a project stores: strings and flags, plus pointer + length of
/// the media buffers (they are replaced, never mutated). Runs after every message.
fn fingerprint(app: &App) -> u64 {
    let mut h = DefaultHasher::new();
    (&app.title, app.title_auto, app.level, &app.output, app.discs.len()).hash(&mut h);
    for d in &app.discs {
        (&d.path, &d.id, &d.ppf, d.pal_ntsc, d.title_fixes, d.fps_unlock, d.ntsc_60hz, &d.seeded_for, &d.patch_choice).hash(&mut h);
    }
    for a in &app.art {
        a.as_ref().map(|a| (Arc::as_ptr(&a.pixels) as usize, &a.source, a.png.len())).hash(&mut h);
    }
    let media = |m: &Media| (Arc::as_ptr(&m.bytes) as usize, m.source.clone(), m.bytes.len());
    app.icon1.as_ref().map(|i| media(&i.media)).hash(&mut h);
    app.snd0.as_ref().map(|s| media(&s.media)).hash(&mut h);
    let c = &app.clip;
    (
        &c.source,
        &c.start,
        &c.end,
        mode_str(c.mode),
        c.kbps,
        c.len.map(f64::to_bits),
        &c.title,
        c.probed,
    )
        .hash(&mut h);
    c.origin.hash(&mut h);
    for t in [&c.video, &c.music] {
        (&t.path, &t.start, &t.end, t.len.map(f64::to_bits)).hash(&mut h);
    }
    h.finish()
}

fn snapshot(app: &App) -> Project {
    Project {
        name: app.project.name.clone(),
        title: app.title.clone(),
        title_auto: app.title_auto,
        discs: app.discs.iter().map(DiscFile::from).collect(),
        art: std::array::from_fn(|i| app.art[i].as_ref().map(|a| Blob { source: a.source.clone(), bytes: Arc::new(a.png.clone()) })),
        icon1: app.icon1.as_ref().map(|i| Blob::from(&i.media)),
        snd0: app.snd0.as_ref().map(|s| Blob::from(&s.media)),
        clip: ClipFile::from(&app.clip),
        output: app.output.clone(),
        level: app.level,
    }
}

fn is_blank(app: &App) -> bool {
    app.title.trim().is_empty()
        && app.discs.is_empty()
        && app.art.iter().all(Option::is_none)
        && app.icon1.is_none()
        && app.snd0.is_none()
        && app.clip.is_blank()
}

/// Replaces the editable state with `p` (+ its decoded previews); probes the discs again.
fn apply(app: &mut App, p: &Project, art: [Option<Art>; 4], icon1: Option<Icon1>, snd0: Option<Snd0>) -> Task<Message> {
    app.title.clone_from(&p.title);
    app.title_auto = p.title_auto;
    app.discs.clear();
    let mut probes = Vec::new();
    for d in &p.discs {
        let key = app.next_key;
        app.next_key += 1;
        app.discs.push(d.to_disc(key));
        let path = d.path.clone();
        probes.push(worker::blocking(move || engine::probe_disc(&path).map_err(|e| e.to_string()), move |r| Message::DiscProbed(key, r)));
    }
    app.art = art;
    app.icon1 = icon1;
    app.set_snd0(snd0);
    app.clip = p.clip.to_form();
    app.output.clone_from(&p.output);
    app.level = p.level;
    app.set_screen(crate::xmb::Screen::Game);
    app.audio.restart();
    Task::batch(probes)
}

/// The current state counts as saved.
fn settle(app: &mut App) {
    app.project.fp = fingerprint(app);
    app.project.dirty = false;
}

// ──────────────────────────────── hooks ────────────────────────────────

/// Startup: finds the last session off the UI thread. Autosave stays off until it is resolved.
pub fn boot(app: &mut App) -> Task<Message> {
    let fp = fingerprint(app);
    let st = &mut app.project;
    (st.fp, st.boot_fp, st.current) = (fp, fp, AUTOSAVE.into());
    // Unit tests must never touch the user's real projects.
    st.root = if cfg!(test) { None } else { root() };
    let Some(root) = st.root.clone() else {
        st.phase = Phase::Off("no user folder for projects".into());
        return Task::none();
    };
    // Files given on the command line ("Open with") start a new project.
    let fresh = std::env::args_os().len() > 1;
    // Lazy: the worker starts when the runtime runs the task, not when `boot` builds it.
    Task::done(()).then(move |()| worker::blocking({
        let root = root.clone();
        move || boot_load(&root, fresh)
    }, |r| msg(Msg::Booted(r))))
}

/// `App::update` followed by change detection (installed as the iced update function).
pub fn update(app: &mut App, m: Message) -> Task<Message> {
    let task = app.update(m);
    let fp = fingerprint(app);
    if fp == app.project.fp {
        return task;
    }
    app.project.fp = fp;
    match touch(&mut app.project) {
        Some(t) => Task::batch([task, t]),
        None => task,
    }
}

/// `App::subscription` plus window-close requests (the window closes after a final save).
pub fn subscription(app: &App) -> Subscription<Message> {
    Subscription::batch([app.subscription(), window::close_requests().map(|_| msg(Msg::CloseRequested))])
}

fn sleep(d: Duration) -> Task<Message> {
    worker::blocking(
        move || {
            std::thread::sleep(d);
            Ok(())
        },
        |_| msg(Msg::Tick),
    )
}

/// Marks a change; arms the debounce timer unless one is pending.
fn touch(st: &mut State) -> Option<Task<Message>> {
    st.dirty = true;
    st.changed = Some(Instant::now());
    arm(st)
}

fn arm(st: &mut State) -> Option<Task<Message>> {
    if st.timer || !st.dirty || st.phase != Phase::Ready || st.closing {
        return None;
    }
    st.timer = true;
    Some(sleep(DEBOUNCE))
}

/// Writes `project` (or deletes the slot for `None`) on a worker; reports `Msg::Saved`.
fn spawn_persist(st: &mut State, slot: String, project: Option<Project>, session: bool) -> Task<Message> {
    let Some(root) = st.root.clone() else { return Task::none() };
    let seq = st.next_seq();
    st.in_flight += 1;
    let session = (session && st.session.as_deref() != Some(slot.as_str())).then(|| slot.clone());
    if session.is_some() {
        st.session = Some(slot.clone());
    }
    worker::blocking(move || persist(&root, &slot, project, seq, session), |r| msg(Msg::Saved(r)))
}

fn save_now(app: &mut App) -> Task<Message> {
    let snap = snapshot(app);
    let st = &mut app.project;
    st.dirty = false;
    // An empty unnamed session is not worth restoring: its slot is removed instead.
    let project = (st.named() || !snap.is_blank()).then_some(snap);
    spawn_persist(st, st.current.clone(), project, true)
}

/// After any write finished: close if closing, else re-arm for changes made meanwhile.
fn after_write(app: &mut App, r: Result<(), String>) -> Task<Message> {
    let st = &mut app.project;
    st.in_flight = st.in_flight.saturating_sub(1);
    let toast = match r {
        Ok(()) => {
            st.error = None;
            None
        }
        Err(e) => {
            st.dirty = true;
            st.session = None;
            let first = st.error.is_none();
            st.error = Some(e.clone());
            first.then_some(e)
        }
    };
    let failed = toast.is_some() || st.error.is_some();
    let toast = toast.map(|e| app.toast(Kind::Error, format!("Autosave failed: {e}")));
    let next = if app.project.closing {
        if failed { iced::exit() } else { finish_close(app) }
    } else if failed {
        Task::none() // the next edit retries; no retry loop on a full or read-only disk
    } else {
        arm(&mut app.project).unwrap_or_else(Task::none)
    };
    Task::batch(toast.into_iter().chain([next]))
}

fn finish_close(app: &mut App) -> Task<Message> {
    let st = &app.project;
    if st.phase != Phase::Ready {
        return iced::exit();
    }
    if st.in_flight > 0 || st.busy.is_some() {
        return Task::none(); // their completion calls back here
    }
    if st.dirty { save_now(app) } else { iced::exit() }
}

fn warn_toast(app: &mut App, warnings: &[String]) -> Option<Task<Message>> {
    if warnings.is_empty() {
        return None;
    }
    let mut s = warnings.iter().take(3).cloned().collect::<Vec<_>>().join(" · ");
    if warnings.len() > 3 {
        s.push_str(&format!(" · and {} more (see the log)", warnings.len() - 3));
    }
    for w in warnings {
        app.log(format!("project: {w}"));
    }
    Some(app.toast(Kind::Error, s))
}

/// Puts a loaded slot on screen and makes it the current one.
fn restore(app: &mut App, r: Restored, verb: &str) -> Task<Message> {
    let Restored { slug, project, art, icon1, snd0, warnings } = r;
    let probes = apply(app, &project, art, icon1, snd0);
    let st = &mut app.project;
    st.name.clone_from(&project.name);
    st.current.clone_from(&slug);
    st.session = Some(slug);
    st.busy = None;
    st.panel = Panel::None;
    settle(app);
    let text = if app.project.named() { format!("{verb} \"{}\"", project.name) } else { "Restored your unsaved session from last time".into() };
    let ok = app.toast(Kind::Info, text);
    let warn = warn_toast(app, &warnings);
    Task::batch([probes, ok].into_iter().chain(warn))
}

fn list_task(st: &State) -> Task<Message> {
    let Some(root) = st.root.clone() else { return Task::none() };
    worker::blocking(move || Ok(list(&root)), |r: Result<Vec<Entry>, String>| msg(Msg::Listed(r.unwrap_or_default())))
}

fn new_project(app: &mut App) -> Task<Message> {
    let mut tasks = Vec::new();
    if app.project.named() && app.project.dirty {
        let snap = snapshot(app);
        let slot = app.project.current.clone();
        tasks.push(spawn_persist(&mut app.project, slot, Some(snap), false));
    }
    tasks.push(apply(app, &Project::default(), Default::default(), None, None));
    let st = &mut app.project;
    st.current = AUTOSAVE.into();
    st.name.clear();
    st.panel = Panel::None;
    settle(app);
    tasks.push(spawn_persist(&mut app.project, AUTOSAVE.into(), None, true));
    Task::batch(tasks)
}

fn open(app: &mut App, slug: String) -> Task<Message> {
    let Some(root) = app.project.root.clone() else { return Task::none() };
    let flush = (app.project.named() && app.project.dirty).then(|| (app.project.current.clone(), snapshot(app)));
    let st = &mut app.project;
    let discard = !st.named();
    let seq = st.next_seq();
    st.busy = Some("Opening…");
    st.panel = Panel::None;
    worker::blocking(
        move || {
            // Pending edits of the project being left are written first; if that fails, stay.
            if let Some((slot, p)) = flush {
                save(&root.join(slot), &p, seq).map_err(|e| format!("couldn't save the current project first: {e}"))?;
            }
            let r = open_slot(&root, &slug)?;
            if discard {
                remove(&root.join(AUTOSAVE), seq)?;
            }
            write_session(&root, &slug)?;
            Ok(Box::new(r))
        },
        |r| msg(Msg::Opened(r)),
    )
}

/// New / Open, after asking whether to drop an unnamed session.
fn guarded(app: &mut App, p: Pending, confirmed: bool) -> Task<Message> {
    if app.job.is_some() {
        return app.toast(Kind::Error, "Wait for the running job to finish");
    }
    if !confirmed && !app.project.named() && !is_blank(app) {
        app.project.panel = Panel::Discard(p);
        return Task::none();
    }
    match p {
        Pending::New => new_project(app),
        Pending::Open(slug) => open(app, slug),
    }
}

fn save_as(app: &mut App) -> Task<Message> {
    let Panel::SaveAs(name) = &app.project.panel else { return Task::none() };
    let name = clean_name(name);
    let Some(root) = app.project.root.clone().filter(|_| !name.is_empty()) else { return Task::none() };
    let slug = slugify(&name);
    let prev = (app.project.current.clone(), std::mem::replace(&mut app.project.name, name));
    let snap = snapshot(app);
    let st = &mut app.project;
    st.current.clone_from(&slug);
    st.panel = Panel::None;
    st.dirty = false;
    st.in_flight += 1;
    st.session = Some(slug.clone());
    let seq = st.next_seq();
    let drop_autosave = prev.0 == AUTOSAVE;
    worker::blocking(
        move || {
            save(&root.join(&slug), &snap, seq)?;
            if drop_autosave {
                remove(&root.join(AUTOSAVE), seq)?;
            }
            write_session(&root, &slug)
        },
        move |r| msg(Msg::SavedAs(prev.clone(), r)),
    )
}

/// The `Message::Project` arm of `App::update`.
pub fn handle(app: &mut App, m: Msg) -> Task<Message> {
    match m {
        Msg::Booted(r) => booted(app, r),
        Msg::Tick => {
            let st = &mut app.project;
            st.timer = false;
            if !st.dirty || st.phase != Phase::Ready || st.in_flight > 0 || st.busy.is_some() || st.closing {
                return Task::none(); // a finishing save (or open) re-arms
            }
            let left = DEBOUNCE.saturating_sub(st.changed.map_or(DEBOUNCE, |c| c.elapsed()));
            if !left.is_zero() {
                st.timer = true;
                return sleep(left);
            }
            save_now(app)
        }
        Msg::Saved(r) => after_write(app, r),
        Msg::New => guarded(app, Pending::New, false),
        Msg::Open(slug) if valid_slug(&slug) && slug != AUTOSAVE => guarded(app, Pending::Open(slug), false),
        Msg::Open(_) => Task::none(),
        Msg::Discard => match std::mem::take(&mut app.project.panel) {
            Panel::Discard(p) => guarded(app, p, true),
            other => {
                app.project.panel = other;
                Task::none()
            }
        },
        Msg::ShowOpen => {
            let st = &mut app.project;
            if matches!(st.panel, Panel::Open(_)) {
                st.panel = Panel::None;
                return Task::none();
            }
            st.panel = Panel::Open(None);
            st.entries = None;
            list_task(st)
        }
        Msg::ShowSaveAs => {
            let name = if app.project.named() { app.project.name.clone() } else { clean_name(&app.title) };
            app.project.panel = Panel::SaveAs(name);
            list_task(&app.project)
        }
        Msg::ClosePanel => {
            app.project.panel = Panel::None;
            Task::none()
        }
        Msg::Listed(e) => {
            app.project.entries = Some(e);
            Task::none()
        }
        Msg::Opened(Ok(r)) => {
            let t = restore(app, *r, "Opened");
            if app.project.closing { Task::batch([t, finish_close(app)]) } else { t }
        }
        Msg::Opened(Err(e)) => {
            app.project.busy = None;
            let t = app.toast(Kind::Error, format!("Couldn't open the project: {e}"));
            let next = if app.project.closing { finish_close(app) } else { arm(&mut app.project).unwrap_or_else(Task::none) };
            Task::batch([t, next])
        }
        Msg::Name(s) => {
            if let Panel::SaveAs(name) = &mut app.project.panel {
                *name = s.chars().take(NAME_MAX).collect();
            }
            Task::none()
        }
        Msg::SaveAs => save_as(app),
        Msg::SavedAs(prev, r) => {
            let ok = match &r {
                Ok(()) => Some(app.toast(Kind::Success, format!("Saved \"{}\" — changes are now saved to it automatically", app.project.name))),
                Err(_) => {
                    (app.project.current, app.project.name) = prev;
                    None
                }
            };
            let t = after_write(app, r);
            Task::batch(ok.into_iter().chain([t]))
        }
        Msg::Delete(slug) => {
            let st = &mut app.project;
            let Panel::Open(confirm) = &mut st.panel else { return Task::none() };
            if confirm.as_deref() != Some(slug.as_str()) {
                *confirm = Some(slug);
                return Task::none();
            }
            *confirm = None;
            let Some(root) = st.root.clone().filter(|_| valid_slug(&slug) && slug != AUTOSAVE) else { return Task::none() };
            let name = st.entries.iter().flatten().find(|e| e.slug == slug).map_or_else(|| slug.clone(), |e| e.name.clone());
            let seq = st.next_seq();
            // Deleting the open project keeps the work on screen as an unnamed session.
            let rearm = if slug == st.current {
                st.current = AUTOSAVE.into();
                st.name.clear();
                st.session = None;
                touch(st)
            } else {
                None
            };
            let del = worker::blocking(
                move || {
                    remove(&root.join(&slug), seq)?;
                    Ok(list(&root))
                },
                move |r| msg(Msg::Deleted(name.clone(), r)),
            );
            Task::batch([del].into_iter().chain(rearm))
        }
        Msg::Deleted(name, r) => match r {
            Ok(e) => {
                app.project.entries = Some(e);
                app.toast(Kind::Info, format!("Deleted \"{name}\""))
            }
            Err(e) => app.toast(Kind::Error, format!("Couldn't delete \"{name}\": {e}")),
        },
        Msg::CloseRequested => {
            if app.project.closing {
                return iced::exit(); // asked twice: don't wait any longer
            }
            app.project.closing = true;
            finish_close(app)
        }
    }
}

fn booted(app: &mut App, r: Result<Boot, String>) -> Task<Message> {
    let pristine = fingerprint(app) == app.project.boot_fp;
    let st = &mut app.project;
    let task = match r {
        Ok(Boot::Restore(r)) if pristine => {
            st.phase = Phase::Ready;
            restore(app, *r, "Reopened")
        }
        Ok(Boot::Restore(r)) if r.slug == AUTOSAVE => {
            // The user started something before the restore arrived: keep both.
            let Some(root) = st.root.clone() else { return Task::none() };
            return worker::blocking(
                move || preserve_autosave(&root).map(|n| Boot::Kept(n, Some("you had already started a new one".into()))),
                |r| msg(Msg::Booted(r)),
            );
        }
        Ok(Boot::Restore(r)) => {
            st.phase = Phase::Ready;
            app.toast(Kind::Info, format!("\"{}\" was not reopened because you started a new project — it is under Open", r.project.name))
        }
        Ok(Boot::Kept(name, why)) => {
            st.phase = Phase::Ready;
            let why = why.map_or_else(String::new, |w| format!(" ({w})"));
            app.toast(Kind::Info, format!("Your previous unsaved session was kept as \"{name}\"{why} — it is under Open"))
        }
        Ok(Boot::Failed(e)) => {
            st.phase = Phase::Ready;
            app.toast(Kind::Error, e)
        }
        Ok(Boot::Nothing) => {
            st.phase = Phase::Ready;
            Task::none()
        }
        Err(e) => {
            st.phase = Phase::Off(e.clone());
            app.toast(Kind::Error, format!("Autosave is off: {e}"))
        }
    };
    // Edits made while booting are saved now.
    let fp = fingerprint(app);
    let st = &mut app.project;
    let rearm = if fp != st.fp {
        st.fp = fp;
        touch(st)
    } else {
        arm(st)
    };
    Task::batch([task].into_iter().chain(rearm))
}

// ───────────────────────────────── view ─────────────────────────────────

fn small(s: &str, on: Option<Msg>) -> Element<'_, Message> {
    button(text(s).size(12)).padding([5, 10]).style(style::ghost).on_press_maybe(on.map(msg)).into()
}

fn status(st: &State, blank: bool) -> (String, Color) {
    match (&st.phase, st.busy, &st.error) {
        (Phase::Booting, ..) => ("Looking for your last session…".into(), style::MUTED),
        (Phase::Off(e), ..) => (format!("Autosave off — {e}"), style::RED),
        (_, Some(b), _) => (b.into(), style::MUTED),
        (_, _, Some(e)) => (format!("Save failed — {e}"), style::RED),
        _ if st.in_flight > 0 => ("Saving…".into(), style::MUTED),
        _ if st.dirty => ("Edited".into(), style::MUTED),
        _ if st.named() => ("All changes saved".into(), style::GREEN),
        _ if blank => ("Autosave on — nothing to save yet".into(), style::MUTED),
        _ => ("Autosaved — not named yet".into(), style::AMBER),
    }
}

/// The compact project bar (top of the left panel) with its drop-down panels.
pub fn view(app: &App) -> Element<'_, Message> {
    let st = &app.project;
    let ready = st.phase == Phase::Ready && st.busy.is_none();
    let idle = ready && app.job.is_none();
    let (line, color) = status(st, is_blank(app));
    let name = if st.named() { st.name.as_str() } else { "Untitled project" };
    let open_label = if matches!(st.panel, Panel::Open(_)) { "Close" } else { "Open…" };
    let head = row![
        column![text(name).size(14).wrapping(text::Wrapping::None), text(line).size(11).color(color).wrapping(text::Wrapping::None)].spacing(1).width(Fill).clip(true),
        small("New", idle.then_some(Msg::New)),
        small(open_label, idle.then_some(Msg::ShowOpen)),
        small("Save as…", ready.then_some(Msg::ShowSaveAs)),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    let mut col = column![head].spacing(10);
    match &st.panel {
        Panel::None => {}
        Panel::SaveAs(name) => {
            let slug = slugify(&clean_name(name));
            let exists = slug != st.current && st.entries.iter().flatten().any(|e| e.slug == slug);
            let go = (!clean_name(name).is_empty()).then_some(Msg::SaveAs).map(msg);
            col = col.push(
                row![
                    text_input("Project name", name).on_input(|s| msg(Msg::Name(s))).on_submit_maybe(go.clone()).size(13).padding(7).width(Fill).style(style::input),
                    button(text(if exists { "Replace" } else { "Save" }).size(12))
                        .padding([6, 12])
                        .style(style::solid(if exists { style::RED } else { style::BLUE }))
                        .on_press_maybe(go),
                    small("Cancel", Some(Msg::ClosePanel)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            );
            if exists {
                col = col.push(text("A project with this name exists and will be replaced").size(11).color(style::RED));
            }
        }
        Panel::Discard(_) => {
            col = col.push(
                row![
                    text("Discard this unsaved project?").size(12).color(style::AMBER).width(Fill),
                    button(text("Discard").size(12)).padding([6, 12]).style(style::solid(style::RED)).on_press(msg(Msg::Discard)),
                    small("Save as…", Some(Msg::ShowSaveAs)),
                    small("Cancel", Some(Msg::ClosePanel)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            );
        }
        Panel::Open(confirm) => col = col.push(open_list(st, confirm.as_deref(), idle)),
    }
    container(col).padding([10, 12]).width(Fill).style(style::card).into()
}

fn open_list<'a>(st: &'a State, confirm: Option<&str>, idle: bool) -> Element<'a, Message> {
    let Some(entries) = &st.entries else { return text("Reading projects…").size(12).color(style::MUTED).into() };
    if entries.is_empty() {
        return text("No saved projects yet — use Save as… to keep this one").size(12).color(style::MUTED).into();
    }
    let now = now_secs();
    let rows = entries.iter().fold(column![].spacing(4), |c, e| {
        let thumb: Element<'_, Message> = match &e.thumb {
            Some(h) => image(h.clone()).width(54).height(30).content_fit(iced::ContentFit::Contain).into(),
            None => container(text("—").size(11).color(style::MUTED)).center_x(54).center_y(30).style(style::well).into(),
        };
        let detail = match &e.error {
            Some(err) => err.clone(),
            None => {
                let mut d = vec![if e.title.trim().is_empty() { "no title".to_owned() } else { e.title.clone() }, format!("{} disc(s)", e.discs)];
                if e.media {
                    d.push("clip".into());
                }
                d.push(ago(e.saved_at, now));
                d.join(" · ")
            }
        };
        let info = column![
            text(e.name.as_str()).size(13).wrapping(text::Wrapping::None),
            text(detail).size(11).color(if e.error.is_some() { style::RED } else { style::MUTED }).wrapping(text::Wrapping::None),
        ]
        .spacing(1)
        .width(Fill)
        .clip(true);
        let pick = button(row![thumb, info].spacing(10).align_y(Alignment::Center))
            .padding(6)
            .width(Fill)
            .style(style::tab(e.slug == st.current))
            .on_press_maybe((idle && e.error.is_none() && e.slug != st.current).then(|| msg(Msg::Open(e.slug.clone()))));
        let del: Element<'_, Message> = if confirm == Some(e.slug.as_str()) {
            button(text("Delete?").size(12)).padding([5, 10]).style(style::solid(style::RED)).on_press(msg(Msg::Delete(e.slug.clone()))).into()
        } else {
            small("✕", idle.then(|| Msg::Delete(e.slug.clone())))
        };
        c.push(row![pick, del].spacing(6).align_y(Alignment::Center))
    });
    container(scrollable(rows)).max_height(280).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(source: &str, bytes: &[u8]) -> Option<Blob> {
        Some(Blob { source: source.into(), bytes: Arc::new(bytes.to_vec()) })
    }

    /// A project whose disc and PPF files exist under `dir`.
    fn sample(dir: &Path) -> Project {
        let cue = dir.join("Game (Disc 1).cue");
        let ppf = dir.join("fix.ppf");
        fs::write(&cue, b"FILE x BINARY").unwrap();
        fs::write(&ppf, b"PPF30").unwrap();
        Project {
            name: "Final Fantasy VII".into(),
            title: "FINAL FANTASY VII".into(),
            title_auto: false,
            discs: vec![
                DiscFile {
                    path: cue,
                    id: "SCES00867".into(),
                    ppf: Some(ppf),
                    pal_ntsc: true,
                    title_fixes: false,
                    fps_unlock: true,
                    ntsc_60hz: true,
                    seeded_for: Some("SCES00867".into()),
                    patch_choice: vec![("Skip intro".into(), false), ("POPS config".into(), true)],
                },
                DiscFile { path: dir.join("Game (Disc 1).cue"), ..DiscFile::default() },
            ],
            art: [blob("icon.png", b"\x89PNG icon"), None, blob("bg.jpg", b"\x89PNG background"), None],
            icon1: blob("Trailer [0:05-0:35].pmf", &[0x50, 0x53, 0x4d, 0x46, 0, 1, 2, 3]),
            snd0: blob("Trailer [0:05-0:35].at3", b"RIFF....WAVEfmt "),
            clip: ClipFile {
                origin: "files".into(),
                video: LocalTrack {
                    path: dir.join("trailer.mp4").display().to_string(),
                    start: "0:02".into(),
                    end: String::new(),
                    len: Some(95.25),
                },
                music: LocalTrack {
                    path: dir.join("song.mp3").display().to_string(),
                    start: "1:00".into(),
                    end: "1:30".into(),
                    len: None,
                },
                source: "https://youtu.be/xyz".into(),
                start: "0:05".into(),
                end: "0:35".into(),
                mode: "both".into(),
                kbps: 105,
                len: Some(212.5),
                title: Some("Trailer".into()),
                probed: true,
            },
            output: Some(dir.join("out").join("EBOOT.PBP")),
            level: 9,
        }
    }

    #[test]
    fn round_trip_keeps_everything() {
        let t = tempfile::tempdir().unwrap();
        let p = sample(t.path());
        let dir = t.path().join("projects").join("ff7");
        assert!(save(&dir, &p, 1).unwrap());
        let (q, warn) = load(&dir).unwrap();
        assert_eq!(q, p);
        assert!(warn.is_empty(), "{warn:?}");
        let json: serde_json::Value = serde_json::from_slice(&fs::read(dir.join(PROJECT_FILE)).unwrap()).unwrap();
        assert_eq!(json["version"], VERSION);
        // Four media files + project.json, nothing else.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 5);
    }

    #[test]
    fn resave_replaces_changed_media_and_drops_stale_files() {
        let t = tempfile::tempdir().unwrap();
        let mut p = sample(t.path());
        let dir = t.path().join("slot");
        save(&dir, &p, 1).unwrap();
        p.icon1 = blob("other.pmf", b"PSMF other");
        p.art[0] = None;
        save(&dir, &p, 2).unwrap();
        let names: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.iter().filter(|n| n.starts_with("icon1-")).count(), 1, "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("icon0-")), "{names:?}");
        assert_eq!(load(&dir).unwrap().0, p);
    }

    #[test]
    fn older_snapshot_never_overwrites_newer() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("slot");
        let p = Project { title: "new".into(), ..Project::default() };
        assert!(save(&dir, &p, 5).unwrap());
        assert!(!save(&dir, &Project { title: "old".into(), ..Project::default() }, 3).unwrap());
        assert_eq!(load(&dir).unwrap().0.title, "new");
        remove(&dir, 6).unwrap();
        assert!(!dir.exists());
        assert!(!save(&dir, &p, 4).unwrap(), "a save older than the delete must not recreate the slot");
        assert!(!dir.exists());
    }

    #[test]
    fn schema_versions() {
        assert!(parse(br#"{"version": 2, "title": "x"}"#).unwrap_err().contains("newer"));
        assert!(parse(br#"{"title": "x"}"#).unwrap_err().contains("no format version"));
        assert!(parse(br#"{"version": 0}"#).is_err());
        assert!(parse(b"\x00garbage").is_err());
        assert!(parse(br#"{"version": 1, "discs": "nope"}"#).unwrap_err().contains("damaged"));
        // Minimal v1 and unknown fields: defaults fill in.
        let f = parse(br#"{"version": 1, "future_field": [1, 2]}"#).unwrap();
        assert!(f.title_auto);
        assert_eq!(f.level, engine::DEFAULT_LEVEL);
        assert_eq!(f.clip, ClipFile::default());
        assert_eq!(parse_mode("icon1"), ClipMode::Icon1);
        assert_eq!(parse_mode("bogus"), ClipMode::Both);
    }

    #[test]
    fn atomic_write_failure_keeps_old_file() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("project.json");
        write_atomic(&path, b"old").unwrap();
        // The temp file can't be created (a folder is in the way): the write fails, old stays.
        fs::create_dir(tmp_path(&path)).unwrap();
        assert!(write_atomic(&path, b"new").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        fs::remove_dir(tmp_path(&path)).unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn failed_save_leaves_the_previous_project_loadable() {
        let t = tempfile::tempdir().unwrap();
        let p = sample(t.path());
        let dir = t.path().join("slot");
        save(&dir, &p, 1).unwrap();
        fs::create_dir(tmp_path(&dir.join(PROJECT_FILE))).unwrap();
        let changed = Project { title: "changed".into(), icon1: blob("new.pmf", b"PSMF new"), ..p.clone() };
        assert!(save(&dir, &changed, 2).is_err());
        let (q, warn) = load(&dir).unwrap();
        assert_eq!(q, p, "old project.json and all its media intact");
        assert!(warn.is_empty(), "{warn:?}");
    }

    #[test]
    fn missing_and_damaged_files_are_tolerated() {
        let t = tempfile::tempdir().unwrap();
        let mut p = sample(t.path());
        p.discs[0].path = t.path().join("moved.cue");
        fs::remove_file(p.discs[0].ppf.as_ref().unwrap()).unwrap();
        let dir = t.path().join("slot");
        save(&dir, &p, 1).unwrap();
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("icon1-") {
                fs::remove_file(e.path()).unwrap();
            } else if n.starts_with("snd0-") {
                fs::write(e.path(), b"RIFF....WAVEfmX ").unwrap(); // same size, other bytes
            }
        }
        let (q, warn) = load(&dir).unwrap();
        assert_eq!(q.discs.len(), 2, "a disc that moved is kept (shown in red by the GUI)");
        assert_eq!(q.discs[0].ppf, None, "a missing PPF is dropped");
        assert!(q.icon1.is_none() && q.snd0.is_none());
        assert_eq!(q.art, p.art);
        assert_eq!(warn.len(), 4, "{warn:?}");
        assert!(warn.iter().any(|w| w.contains("moved.cue")));
        assert!(warn.iter().any(|w| w.contains("damaged")));
    }

    #[test]
    fn media_names_cannot_escape_the_slot() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("slot");
        fs::create_dir_all(&dir).unwrap();
        fs::write(t.path().join("secret.pmf"), b"x").unwrap();
        let json = br#"{"version":1,"icon1":{"source":"a","file":"../secret.pmf","size":1,"fnv":"af63bd4c8601b7be"}}"#;
        fs::write(dir.join(PROJECT_FILE), json).unwrap();
        let (q, warn) = load(&dir).unwrap();
        assert!(q.icon1.is_none());
        assert!(warn[0].contains("invalid file name"));
    }

    #[test]
    fn slugs_and_names() {
        assert_eq!(slugify("Final Fantasy VII (Disc 1)"), "final-fantasy-vii-disc-1");
        assert_eq!(slugify("  --Crash!!  Bandicoot--  "), "crash-bandicoot");
        assert_eq!(slugify("CON"), "con-project");
        assert_eq!(slugify("lpt1"), "lpt1-project");
        assert_eq!(slugify("_autosave"), "autosave");
        assert_eq!(slugify(&"x".repeat(200)).len(), 48);
        let jp = slugify("ファイナルファンタジー");
        assert!(jp.starts_with("project-") && valid_slug(&jp), "{jp}");
        assert_eq!(jp, slugify("ファイナルファンタジー"), "stable");
        assert_ne!(jp, slugify("ドラゴンクエスト"));
        for s in ["a", "final-fantasy", AUTOSAVE, &jp] {
            assert!(valid_slug(s), "{s}");
        }
        for s in ["", "..", "a/b", "A", "-x", "_x", "a b", &"a".repeat(65)] {
            assert!(!valid_slug(s), "{s}");
        }
        assert_eq!(clean_name("  Tekken \t 3\n"), "Tekken 3");
        assert_eq!(clean_name(&"y".repeat(300)).chars().count(), NAME_MAX);
        assert!(is_ours("icon1-0123456789abcdef.pmf") && is_ours(".project.json.tmp") && is_ours("pic1-0123456789abcdef.png"));
        assert!(!is_ours("project.json") && !is_ours("icon1-xyz.pmf") && !is_ours("notes.txt") && !is_ours("icon1-0123456789abcdef.png"));
    }

    #[test]
    fn listing_session_and_recovery() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("projects");
        let mut p = sample(t.path());
        save(&root.join("ff7"), &p, 1).unwrap();
        p.name = "Tekken 3".into();
        p.art = Default::default();
        save(&root.join("tekken-3"), &p, 1).unwrap();
        save(&root.join(AUTOSAVE), &Project { title: "Crash Bandicoot".into(), ..Project::default() }, 1).unwrap();
        fs::create_dir_all(root.join("newer")).unwrap();
        fs::write(root.join("newer").join(PROJECT_FILE), br#"{"version": 99}"#).unwrap();
        let l = list(&root);
        let names: Vec<&str> = l.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(!names.contains(&"Crash Bandicoot") && !l.iter().any(|e| e.slug == AUTOSAVE));
        let ff7 = l.iter().find(|e| e.slug == "ff7").unwrap();
        assert!(ff7.thumb.is_some() && ff7.media && ff7.discs == 2 && ff7.error.is_none());
        assert!(l.iter().find(|e| e.slug == "tekken-3").unwrap().thumb.is_none());
        assert!(l.iter().find(|e| e.slug == "newer").unwrap().error.as_deref().unwrap().contains("newer"));

        assert_eq!(read_session(&root), None);
        write_session(&root, "ff7").unwrap();
        assert_eq!(read_session(&root).as_deref(), Some("ff7"));
        fs::write(root.join(SESSION_FILE), br#"{"version":1,"current":"../etc"}"#).unwrap();
        assert_eq!(read_session(&root), None, "a bad slug is ignored");

        // Launching with files: the unnamed session becomes a named project.
        write_session(&root, AUTOSAVE).unwrap();
        let Boot::Kept(name, None) = boot_load(&root, true).unwrap() else { panic!() };
        assert_eq!(name, "Recovered — Crash Bandicoot");
        assert!(!root.join(AUTOSAVE).exists());
        let kept = load(&root.join("recovered-crash-bandicoot")).unwrap().0;
        assert_eq!((kept.name.as_str(), kept.title.as_str()), (name.as_str(), "Crash Bandicoot"));
        assert!(matches!(boot_load(&root, true).unwrap(), Boot::Nothing));
        // A second recovery with the same title gets its own folder.
        save(&root.join(AUTOSAVE), &Project { title: "Crash Bandicoot".into(), ..Project::default() }, 2).unwrap();
        preserve_autosave(&root).unwrap();
        assert!(root.join("recovered-crash-bandicoot-2").is_dir());
    }

    #[test]
    fn dates_and_ages() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_763_000_000), "2025-11-13");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(ago(100, 110), "just now");
        assert_eq!(ago(0, 7_200), "2 h ago");
        assert_eq!(ago(0, 1_763_000_000), "1970-01-01");
    }

    /// GUI state → project → GUI state is lossless, and only project edits change the fingerprint.
    #[test]
    fn app_snapshot_apply_and_fingerprint() {
        let t = tempfile::tempdir().unwrap();
        let mut app = App::boot().0;
        assert!(app.project.root.is_none(), "tests never touch real storage");
        let empty = fingerprint(&app);
        assert!(snapshot(&app).is_blank() && is_blank(&app));
        assert_eq!(snapshot(&app), Project::default(), "Project::default matches a fresh App");
        let mut p = sample(t.path());
        (p.art, p.icon1, p.snd0, p.name) = (Default::default(), None, None, String::new());
        let _ = apply(&mut app, &p, Default::default(), None, None);
        assert_eq!(snapshot(&app), p);
        let fp = fingerprint(&app);
        assert_ne!(fp, empty);
        app.log_open = !app.log_open;
        app.log("noise");
        assert_eq!(fingerprint(&app), fp, "view-only state is not project state");
        app.discs[0].fps_unlock = !app.discs[0].fps_unlock;
        assert_ne!(fingerprint(&app), fp);
    }
}
