//! PSX2PSP-Reborn — desktop GUI (iced 0.14 + wgpu).
//!
//! Elm architecture: `App` is the single source of truth, `update` is the only mutator, `view`
//! is a pure function of state. Every disk/CPU-heavy operation runs in `worker` threads.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod style;
mod worker;
mod xmb;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iced::widget::{
    button, center, column, container, image, pin, progress_bar, responsive, row, scrollable, shader, slider, space, stack, text,
    text_input, tooltip,
};
use iced::{Alignment, Color, Element, Fill, Font, Length, Size, Subscription, Task, Theme, event, window};
use psx2psp_engine::media::images::{self, Slot};
use psx2psp_engine::core::Monitor;
use psx2psp_engine::media::clip::{self, ClipMode, ClipRequest, ClipTools};
use psx2psp_engine::media::provision::{self, Asset};
use psx2psp_engine::media::psmf;
use psx2psp_engine::media::tools::{self, At3Encoder, Ffmpeg, YtDlp};
use psx2psp_engine::patchdb::{self, Patch, PatchDb};
use psx2psp_engine::{self as engine, Artwork, ConvertRequest, DEFAULT_LEVEL, DiscInput, DiscSummary};

use worker::JobEvent;
use xmb::{Movie, Pixels, Screen, Xmb};

const MAX_DISCS: usize = 5;
const MAX_LOG: usize = 2000;
const TOAST_TTL: Duration = Duration::from_secs(6);
const WAVE: Color = iced::color!(0x2a55c8);

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "bmp", "gif", "webp"];
const DISC_EXT: &[&str] = &["cue", "bin", "img", "m3u"];
const MEDIA_EXT: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v", "mp3", "wav", "flac", "ogg", "opus", "m4a", "aac"];
/// Clip length when no end is given (capped at the source's length).
const CLIP_DEFAULT_SECS: f64 = 30.0;
/// Frames decoded for the animated ICON1 preview (30 s at 29.97 fps, ~41 MB of RGBA at 144×80).
const PREVIEW_FRAMES: usize = 900;
const JOB_CLIP: &str = "Clip";
const JOB_INSTALL: &str = "Install tools";
/// Error text of a job the user stopped.
const CANCELLED: &str = "cancelled";

fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window(window::Settings {
            size: Size::new(1320.0, 820.0),
            min_size: Some(Size::new(1000.0, 640.0)),
            ..Default::default()
        })
        .antialiasing(true)
        .run()
}

// ───────────────────────────────── state ─────────────────────────────────

struct App {
    theme: Theme,
    title: String,
    /// The title was filled from the game DB / volume label (not typed): it follows disc 1.
    title_auto: bool,
    discs: Vec<Disc>,
    next_key: u64,
    donor_boot: Option<Arc<Pixels>>,
    /// Indexed by `slot_ix` (ICON0, PIC0, PIC1, BOOT).
    art: [Option<Art>; 4],
    icon1: Option<Icon1>,
    snd0: Option<Snd0>,
    /// SND0 preview output; its position drives the ICON1 movie (lip-sync).
    audio: audio::Player,
    muted: bool,
    clip: ClipForm,
    level: u32,
    output: Option<PathBuf>,
    screen: Screen,
    job: Option<Job>,
    log: Vec<String>,
    log_open: bool,
    toasts: Vec<Toast>,
    tools: Option<Tools>,
    /// Built-in patch database with the user's patches.ini merged on top.
    patches: PatchDb,
}

struct Disc {
    key: u64,
    path: PathBuf,
    summary: Option<DiscSummary>,
    error: Option<String>,
    /// User override; empty = use the probed ID.
    id: String,
    ppf: Option<PathBuf>,
    /// PAL→NTSC preset: opt-in per disc (generic rules break some games); the engine applies
    /// it only where its video-mode fix matches.
    pal_ntsc: bool,
    /// "Game fixes" switch: the POPS config and the `fix` patches the database has for this ID.
    title_fixes: bool,
    /// "Unlock fps" switch: the game's `fps` patches.
    fps_unlock: bool,
    /// "60 Hz" switch: the game's `60hz` patches.
    ntsc_60hz: bool,
    /// The ID whose database defaults the switches were last set from ([`Disc::seed`]).
    seeded_for: Option<String>,
    /// Per-patch choices by name ([`patchdb::CONFIG`] = the POPS config); they win over the
    /// switches. Cleared for a kind when its switch is toggled.
    patch_choice: Vec<(String, bool)>,
}

impl Disc {
    fn new(key: u64, path: PathBuf) -> Self {
        Self {
            key,
            path,
            summary: None,
            error: None,
            id: String::new(),
            ppf: None,
            pal_ntsc: false,
            title_fixes: true,
            fps_unlock: false,
            ntsc_60hz: false,
            seeded_for: None,
            patch_choice: Vec::new(),
        }
    }

    /// When the disc's ID changes (probe, or typed by hand), its switches start over from that
    /// game's defaults in the database ("Game fixes" only, unless verified on hardware).
    fn seed(&mut self, db: &PatchDb) {
        let id = self.db_id(db);
        if id == self.seeded_for {
            return;
        }
        let s = id.as_deref().map_or(patchdb::Switches::RECOMMENDED, |i| db.default_switches(i));
        (self.title_fixes, self.fps_unlock, self.ntsc_60hz) = (s.fixes, s.fps, s.ntsc60);
        self.patch_choice.clear();
        self.seeded_for = id;
    }

    fn effective_id(&self) -> Option<String> {
        let id = self.id.trim();
        if id.is_empty() { self.summary.as_ref().and_then(|s| s.disc_id.clone()) } else { Some(id.to_owned()) }
    }

    /// The ID the database knows this game by (its `pops_id`, as the converter builds it).
    fn db_id(&self, db: &PatchDb) -> Option<String> {
        self.effective_id().map(|i| db.pops_id(&i))
    }

    fn switches(&self) -> patchdb::Switches {
        patchdb::Switches { fixes: self.title_fixes, fps: self.fps_unlock, ntsc60: self.ntsc_60hz }
    }

    /// The database's POPS config for this game, if any.
    fn config<'a>(&self, db: &'a PatchDb) -> Option<&'a patchdb::Game> {
        self.db_id(db).and_then(|id| db.game(&id)).filter(|g| g.config.is_some())
    }

    /// The database's patches for this game, sorted by kind.
    fn patches<'a>(&self, db: &'a PatchDb) -> Vec<&'a Patch> {
        self.db_id(db).map(|id| db.patches_for(&id)).unwrap_or_default()
    }

    fn has_kind(&self, db: &PatchDb, k: patchdb::Kind) -> bool {
        self.patches(db).iter().any(|p| p.kind == k)
    }

    /// The game has its own 60 Hz switch, which replaces the generic PAL→NTSC preset.
    fn native_60hz(&self, db: &PatchDb) -> bool {
        self.has_kind(db, patchdb::Kind::Ntsc60)
    }

    fn patch_on(&self, p: &Patch) -> bool {
        p.wanted(self.switches(), &self.patch_choice)
    }

    fn config_on(&self) -> bool {
        patchdb::config_wanted(self.switches(), &self.patch_choice)
    }

    fn choose(&mut self, name: &str, on: bool) {
        self.patch_choice.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.patch_choice.push((name.to_owned(), on));
    }

    /// What the current selection will produce, in one line.
    fn outcome(&self, db: &PatchDb) -> String {
        let ps = self.patches(db);
        let on = |k| ps.iter().any(|p| p.kind == k && self.patch_on(p));
        let video = if on(patchdb::Kind::Ntsc60) { "60 Hz (NTSC)" } else { "50 Hz (PAL, as shipped)" };
        let fps = match (self.has_kind(db, patchdb::Kind::Fps), on(patchdb::Kind::Fps)) {
            (true, true) => " · frame cap off",
            (true, false) => " · stock frame cap",
            (false, _) => "",
        };
        let cfg = self.config(db).is_some();
        let total = ps.len() + usize::from(cfg);
        let n = ps.iter().filter(|p| self.patch_on(p)).count() + usize::from(cfg && self.config_on());
        format!("Result: {video}{fps} · {n} of {total} on")
    }

    /// Every switch this game offers is on and nothing is turned off by hand.
    fn all_on(&self, db: &PatchDb) -> bool {
        self.title_fixes
            && (self.fps_unlock || !self.has_kind(db, patchdb::Kind::Fps))
            && (self.ntsc_60hz || !self.native_60hz(db))
            && self.patch_choice.iter().all(|(_, on)| *on)
    }
}

#[derive(Debug, Clone)]
struct Art {
    source: String,
    png: Vec<u8>,
    pixels: Arc<Pixels>,
    thumb: image::Handle,
}

impl Art {
    fn from_slot_image(source: String, si: images::SlotImage) -> Self {
        let thumb = image::Handle::from_rgba(si.width, si.height, si.rgba.clone());
        Self { source, png: si.png, pixels: Pixels::new(si.width, si.height, si.rgba), thumb }
    }
}

struct Job {
    label: &'static str,
    progress: f32,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Info,
    Success,
    Error,
}

struct Toast {
    id: u64,
    kind: Kind,
    text: String,
}

/// A finished media payload, embedded verbatim.
#[derive(Debug, Clone)]
struct Media {
    source: String,
    bytes: Arc<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct Icon1 {
    media: Media,
    secs: f64,
    /// Decoded frames for the XMB preview (absent when no ffmpeg is installed).
    movie: Option<Arc<Movie>>,
}

#[derive(Debug, Clone)]
struct Snd0 {
    media: Media,
    /// The SND0.AT3 bytes decoded back to PCM (absent when no ffmpeg is installed).
    sound: Option<Arc<audio::Sound>>,
}

/// What a clip job produced (only the requested parts).
#[derive(Debug, Clone)]
struct ClipOut {
    icon1: Option<Icon1>,
    snd0: Option<Snd0>,
}

/// Clip editor: URL or file → `[start, end)` → ICON1.PMF and/or SND0.AT3.
struct ClipForm {
    source: String,
    start: String,
    end: String,
    mode: ClipMode,
    kbps: u32,
    /// Probe result for `source`.
    len: Option<f64>,
    title: Option<String>,
    probed: bool,
}

impl Default for ClipForm {
    fn default() -> Self {
        Self { source: String::new(), start: String::new(), end: String::new(), mode: ClipMode::Both, kbps: 132, len: None, title: None, probed: false }
    }
}

#[derive(Debug, Clone)]
struct Tools {
    ffmpeg: Option<PathBuf>,
    x264: bool,
    at3: Option<PathBuf>,
    yt_dlp: Option<PathBuf>,
    /// Missing tools with a published binary for this OS/CPU.
    missing: Vec<Asset>,
}

fn detect_tools() -> Tools {
    let ff = Ffmpeg::detect();
    Tools {
        x264: ff.as_ref().is_some_and(Ffmpeg::has_x264),
        ffmpeg: ff.map(|f| f.path),
        at3: At3Encoder::detect().map(|e| e.path().to_path_buf()),
        yt_dlp: YtDlp::detect().map(|y| y.path),
        missing: provision::missing(),
    }
}

fn detect_tools_task() -> Task<Message> {
    worker::blocking(|| Ok(detect_tools()), Message::ToolsDetected)
}

#[derive(Debug, Clone)]
struct Imported {
    path: PathBuf,
    title: Option<String>,
    disc_id: Option<String>,
    discs: usize,
    art: Vec<(Slot, Art)>,
}

/// Slot implied by a PBP-style file name (`ICON0.PNG`, `pic1.jpg`, `STARTDAT.PNG`…).
fn art_slot_for(p: &Path) -> Option<Slot> {
    match p.file_stem()?.to_str()?.to_ascii_uppercase().as_str() {
        "ICON0" => Some(Slot::Icon0),
        "PIC0" => Some(Slot::Pic0),
        "PIC1" => Some(Slot::Pic1),
        "BOOT" | "STARTDAT" => Some(Slot::Boot),
        _ => None,
    }
}

fn slot_ix(s: Slot) -> usize {
    match s {
        Slot::Icon0 => 0,
        Slot::Pic0 => 1,
        Slot::Pic1 => 2,
        Slot::Boot => 3,
    }
}

// ──────────────────────────────── messages ────────────────────────────────

#[derive(Debug, Clone)]
enum Message {
    Title(String),
    AddDiscs,
    DiscsPicked(Vec<PathBuf>),
    DiscProbed(u64, Result<DiscSummary, String>),
    RemoveDisc(u64),
    MoveDiscUp(u64),
    DiscId(u64, String),
    PickPpf(u64),
    PpfPicked(u64, Option<PathBuf>),
    ClearPpf(u64),
    TogglePalNtsc(u64),
    ToggleTitleFixes(u64),
    ToggleFpsUnlock(u64),
    Toggle60Hz(u64),
    /// One patch (or [`patchdb::CONFIG`]) of a disc, by name.
    TogglePatch(u64, String),
    /// Turn on every fix the game offers.
    AllFixes(u64),
    PickArt(Slot),
    ArtPicked(Slot, Option<PathBuf>),
    ArtLoaded(Slot, Result<Art, String>),
    ClearArt(Slot),
    ClipSource(String),
    PickClipSource,
    ClipSourcePicked(Option<PathBuf>),
    ProbeClip,
    ClipProbed(String, Result<(Option<f64>, Option<String>), String>),
    ClipStart(String),
    ClipEnd(String),
    ClipMode(ClipMode),
    ClipKbps(u32),
    MakeClip,
    ClipJob(JobEvent<ClipOut>),
    PickIcon1,
    Icon1Picked(Option<PathBuf>),
    Icon1Loaded(Result<Icon1, String>),
    ClearIcon1,
    PickSnd0,
    Snd0Picked(Option<PathBuf>),
    Snd0Loaded(Result<Snd0, String>),
    ClearSnd0,
    ToggleMute,
    InstallTools,
    Level(u32),
    PickOutput,
    OutputPicked(Option<PathBuf>),
    ResetOutput,
    Screen(Screen),
    Convert,
    Cancel,
    Job(JobEvent),
    ImportPbp,
    ImportPicked(Option<PathBuf>),
    Imported(Result<Imported, String>),
    ExtractPbp,
    ExtractPicked(Option<(PathBuf, PathBuf)>),
    FileDropped(PathBuf),
    ToggleLog,
    Dismiss(u64),
    ToolsDetected(Result<Tools, String>),
}

// ───────────────────────────────── dialogs ────────────────────────────────

fn pick_file(title: &str, name: &str, ext: &[&str]) -> impl Future<Output = Option<PathBuf>> + use<> {
    let d = rfd::AsyncFileDialog::new().set_title(title).add_filter(name, ext).add_filter("All files", &["*"]);
    async move { d.pick_file().await.map(|h| h.path().to_path_buf()) }
}

/// Standard PS1 boot splash (built into the binary), shown until the user picks a BOOT image.
fn builtin_boot() -> Option<Arc<Pixels>> {
    let d = engine::builtin_donor().ok()?;
    let si = images::prepare(Slot::Boot, &d.boot_png).ok()?;
    Some(Pixels::new(si.width, si.height, si.rgba))
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

// ───────────────────────────────── update ─────────────────────────────────

impl App {
    fn boot() -> (Self, Task<Message>) {
        let loaded = patchdb::load();
        let mut app = Self {
            theme: style::theme(),
            title: String::new(),
            title_auto: true,
            discs: Vec::new(),
            next_key: 1,
            donor_boot: builtin_boot(),
            art: Default::default(),
            icon1: None,
            snd0: None,
            audio: audio::Player::new(),
            muted: false,
            clip: ClipForm::default(),
            level: DEFAULT_LEVEL,
            output: None,
            screen: Screen::Game,
            job: None,
            log: Vec::new(),
            log_open: false,
            toasts: Vec::new(),
            tools: None,
            patches: loaded.db,
        };
        app.audio.set_active(app.screen == Screen::Game);
        let detect = detect_tools_task();
        // A broken user patches.ini is left out; the built-in database still applies.
        let warn = loaded.error.map(|e| app.toast(Kind::Error, format!("{e} — your patches.ini is ignored")));
        // Paths on the command line ("Open with", drag onto the exe) behave like dropped files.
        let opened = std::env::args_os().skip(1).map(|a| Task::done(Message::FileDropped(PathBuf::from(a))));
        (app, Task::batch(std::iter::once(detect).chain(warn).chain(opened)))
    }

    fn title(&self) -> String {
        if self.title.trim().is_empty() { "PSX2PSP Reborn".into() } else { format!("{} — PSX2PSP Reborn", self.title.trim()) }
    }

    fn theme(&self) -> Theme {
        self.theme.clone()
    }

    fn subscription(&self) -> Subscription<Message> {
        event::listen_with(|e, _, _| match e {
            iced::Event::Window(window::Event::FileDropped(p)) => Some(Message::FileDropped(p)),
            _ => None,
        })
    }

    fn log(&mut self, line: impl Into<String>) {
        if self.log.len() >= MAX_LOG {
            self.log.drain(..MAX_LOG / 4);
        }
        self.log.push(line.into());
    }

    fn toast(&mut self, kind: Kind, text: impl Into<String>) -> Task<Message> {
        let text = text.into();
        self.log(match kind {
            Kind::Error => format!("error: {text}"),
            _ => text.clone(),
        });
        let id = self.next_key;
        self.next_key += 1;
        self.toasts.push(Toast { id, kind, text });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        worker::blocking(
            || {
                std::thread::sleep(TOAST_TTL);
                Ok(())
            },
            move |_| Message::Dismiss(id),
        )
    }

    /// Merges picked/dropped files into the disc list: BIN+CUE are one disc, a lone BIN is upgraded
    /// to its CUE, `.m3u` expands, duplicates collapse (see [`engine::resolve_discs`]). Synchronous on
    /// purpose: it only reads a directory listing and a few KB of CUE text, and running it inline
    /// means two files dropped together can never race into two entries.
    fn add_discs(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        let existing: Vec<PathBuf> = self.discs.iter().map(|d| d.path.clone()).collect();
        let plan = engine::resolve_discs(&existing, &paths);
        for note in plan.notes {
            self.log(note);
        }
        let mut old = std::mem::take(&mut self.discs);
        let (mut tasks, mut dropped) = (Vec::new(), 0);
        for path in plan.discs {
            if let Some(i) = old.iter().position(|d| d.path == path) {
                self.discs.push(old.remove(i));
                continue;
            }
            if self.discs.len() >= MAX_DISCS {
                dropped += 1;
                continue;
            }
            let key = self.next_key;
            self.next_key += 1;
            // A CUE replacing a BIN that was already listed inherits its PPF, typed ID and switches.
            let disc = match old.iter().find(|d| engine::companion_cue(&d.path).as_deref() == Some(path.as_path())) {
                Some(d) => Disc { key, path: path.clone(), summary: None, error: None, id: d.id.clone(), ppf: d.ppf.clone(), patch_choice: d.patch_choice.clone(), seeded_for: d.seeded_for.clone(), ..*d },
                None => Disc::new(key, path.clone()),
            };
            self.discs.push(disc);
            tasks.push(worker::blocking(move || engine::probe_disc(&path).map_err(|e| e.to_string()), move |r| Message::DiscProbed(key, r)));
        }
        if dropped > 0 {
            tasks.push(self.toast(Kind::Error, format!("A PBP holds at most {MAX_DISCS} discs; {dropped} ignored")));
        }
        Task::batch(tasks)
    }

    /// Fills the title from disc 1: game DB first, then the ISO volume label. Never overwrites a
    /// title the user typed.
    fn auto_title(&mut self) {
        if !self.title_auto && !self.title.trim().is_empty() {
            return;
        }
        let Some(d) = self.discs.first() else { return };
        let from_db = d.effective_id().and_then(|id| engine::gamedb::lookup(&id)).map(|g| g.title.clone());
        if let Some(t) = from_db.or_else(|| d.summary.as_ref().and_then(|s| s.volume_id.clone())) {
            self.title = t;
            self.title_auto = true;
        }
    }

    fn disc_mut(&mut self, key: u64) -> Option<&mut Disc> {
        self.discs.iter_mut().find(|d| d.key == key)
    }

    /// Flips one kind's switch and drops the per-patch choices of that kind, so the switch
    /// decides again ("Game fixes" also covers the POPS config).
    fn toggle_kind(&mut self, key: u64, kind: patchdb::Kind) {
        let db = &self.patches;
        let Some(d) = self.discs.iter_mut().find(|d| d.key == key) else { return };
        let mut names: Vec<String> = d.patches(db).iter().filter(|p| p.kind == kind).map(|p| p.name.clone()).collect();
        match kind {
            patchdb::Kind::Fix => {
                d.title_fixes = !d.title_fixes;
                names.push(patchdb::CONFIG.to_owned());
            }
            patchdb::Kind::Fps => d.fps_unlock = !d.fps_unlock,
            patchdb::Kind::Ntsc60 => d.ntsc_60hz = !d.ntsc_60hz,
            patchdb::Kind::Other => {}
        }
        d.patch_choice.retain(|(n, _)| !names.iter().any(|m| m.eq_ignore_ascii_case(n)));
    }

    fn output_path(&self) -> Option<PathBuf> {
        self.output.clone().or_else(|| {
            let d = self.discs.first()?;
            Some(engine::default_output(d.path.parent()?, &d.effective_id()?))
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Title(t) => {
                self.title = t;
                self.title_auto = false;
            }
            Message::AddDiscs => {
                let d = rfd::AsyncFileDialog::new().set_title("Add PS1 disc image(s)").add_filter("Disc image", DISC_EXT);
                return Task::perform(async move { d.pick_files().await }, |files| {
                    Message::DiscsPicked(files.unwrap_or_default().iter().map(|h| h.path().to_path_buf()).collect())
                });
            }
            Message::DiscsPicked(paths) => return self.add_discs(paths),
            Message::DiscProbed(key, r) => {
                let db = &self.patches;
                let Some(d) = self.discs.iter_mut().find(|d| d.key == key) else { return Task::none() };
                match r {
                    Ok(s) => {
                        let line = format!(
                            "Disc {}: {} — {} [{}], {} track(s), {:.1} MB",
                            file_name(&d.path),
                            s.disc_id.as_deref().unwrap_or("no ID"),
                            s.region.label(),
                            s.volume_id.as_deref().unwrap_or("-"),
                            s.tracks,
                            s.size as f64 / 1e6
                        );
                        d.summary = Some(s);
                        d.error = None;
                        d.seed(db);
                        self.log(line);
                        self.auto_title();
                    }
                    Err(e) => {
                        d.error = Some(e.clone());
                        let name = file_name(&d.path);
                        return self.toast(Kind::Error, format!("{name}: {e}"));
                    }
                }
            }
            Message::RemoveDisc(key) => {
                self.discs.retain(|d| d.key != key);
                self.auto_title();
            }
            Message::MoveDiscUp(key) => {
                if let Some(i) = self.discs.iter().position(|d| d.key == key).filter(|&i| i > 0) {
                    self.discs.swap(i - 1, i);
                }
                self.auto_title();
            }
            Message::DiscId(key, id) => {
                let db = &self.patches;
                if let Some(d) = self.discs.iter_mut().find(|d| d.key == key) {
                    d.id = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(12).collect::<String>().to_ascii_uppercase();
                    d.seed(db);
                }
                self.auto_title();
            }
            Message::PickPpf(key) => {
                return Task::perform(pick_file("PPF patch", "PPF patch", &["ppf"]), move |p| Message::PpfPicked(key, p));
            }
            Message::PpfPicked(key, Some(p)) => {
                if let Some(d) = self.disc_mut(key) {
                    d.ppf = Some(p);
                }
            }
            Message::PpfPicked(_, None) => {}
            Message::ClearPpf(key) => {
                if let Some(d) = self.disc_mut(key) {
                    d.ppf = None;
                }
            }
            Message::TogglePalNtsc(key) => {
                if let Some(d) = self.disc_mut(key) {
                    d.pal_ntsc = !d.pal_ntsc;
                }
            }
            Message::ToggleTitleFixes(key) => self.toggle_kind(key, patchdb::Kind::Fix),
            Message::ToggleFpsUnlock(key) => self.toggle_kind(key, patchdb::Kind::Fps),
            Message::Toggle60Hz(key) => self.toggle_kind(key, patchdb::Kind::Ntsc60),
            Message::AllFixes(key) => {
                if let Some(d) = self.disc_mut(key) {
                    (d.title_fixes, d.fps_unlock, d.ntsc_60hz) = (true, true, true);
                    d.patch_choice.retain(|(_, on)| *on);
                }
            }
            Message::TogglePatch(key, name) => {
                let db = &self.patches;
                if let Some(d) = self.discs.iter_mut().find(|d| d.key == key) {
                    let on = if name.eq_ignore_ascii_case(patchdb::CONFIG) {
                        d.config_on()
                    } else {
                        d.patches(db).iter().find(|p| p.name.eq_ignore_ascii_case(&name)).is_some_and(|p| d.patch_on(p))
                    };
                    d.choose(&name, !on);
                }
            }
            Message::PickArt(slot) => {
                let (w, h) = slot.size();
                return Task::perform(pick_file(&format!("{} ({w}×{h})", slot.label()), "Image", IMAGE_EXT), move |p| Message::ArtPicked(slot, p));
            }
            Message::ArtPicked(slot, Some(p)) => {
                return worker::blocking(
                    move || images::prepare_file(slot, &p).map(|si| Art::from_slot_image(file_name(&p), si)).map_err(|e| e.to_string()),
                    move |r| Message::ArtLoaded(slot, r),
                );
            }
            Message::ArtPicked(_, None) => {}
            Message::ArtLoaded(slot, r) => match r {
                Ok(art) => {
                    self.log(format!("{}: {} ({} B PNG)", slot.label(), art.source, art.png.len()));
                    self.art[slot_ix(slot)] = Some(art);
                    self.screen = if slot == Slot::Boot { Screen::Boot } else { Screen::Game };
                }
                Err(e) => return self.toast(Kind::Error, format!("{}: {e}", slot.label())),
            },
            Message::ClearArt(slot) => self.art[slot_ix(slot)] = None,
            Message::ClipSource(src) => {
                if src.trim() != self.clip.source.trim() {
                    (self.clip.len, self.clip.title, self.clip.probed) = (None, None, false);
                }
                self.clip.source = src;
            }
            Message::PickClipSource => {
                return Task::perform(pick_file("Clip source (video or audio)", "Audio / video", MEDIA_EXT), Message::ClipSourcePicked);
            }
            Message::ClipSourcePicked(Some(p)) => {
                self.clip.source = p.display().to_string();
                (self.clip.len, self.clip.title, self.clip.probed) = (None, None, false);
                return self.probe_clip();
            }
            Message::ClipSourcePicked(None) => {}
            Message::ProbeClip => return self.probe_clip(),
            Message::ClipProbed(src, r) => {
                if src != self.clip.source.trim() {
                    return Task::none(); // the source changed while probing
                }
                match r {
                    Ok((len, title)) => {
                        self.log(format!("Clip source: {} · {}", title.as_deref().unwrap_or(&src), len.map_or("length unknown".into(), tools::fmt_hms)));
                        (self.clip.len, self.clip.title, self.clip.probed) = (len, title, true);
                    }
                    Err(e) => return self.toast(Kind::Error, format!("Can't read the clip source: {e}")),
                }
            }
            Message::ClipStart(t) => self.clip.start = t,
            Message::ClipEnd(t) => self.clip.end = t,
            Message::ClipMode(m) => self.clip.mode = m,
            Message::ClipKbps(k) => self.clip.kbps = k,
            Message::MakeClip => return self.make_clip(),
            Message::ClipJob(ev) => {
                if let Some((_, r)) = self.job_event(ev) {
                    return match r {
                        Ok(out) => {
                            let mut made = Vec::new();
                            if let Some(i) = out.icon1 {
                                made.push(format!("ICON1.PMF {:.1} s, {} KB", i.secs, i.media.bytes.len().div_ceil(1024)));
                                self.icon1 = Some(i);
                                self.set_screen(Screen::Game);
                            }
                            if let Some(a) = out.snd0 {
                                made.push(format!("SND0.AT3 {} KB", a.media.bytes.len().div_ceil(1024)));
                                self.set_snd0(Some(a));
                                self.set_screen(Screen::Game);
                            }
                            // Movie and soundtrack start together, as when the icon gets focus.
                            self.audio.restart();
                            self.toast(Kind::Success, format!("Clip ready — {}", made.join(" + ")))
                        }
                        Err(e) => self.job_failed("Clip", &e),
                    };
                }
            }
            Message::PickIcon1 => return Task::perform(pick_file("ICON1.PMF", "PSP movie", &["pmf"]), Message::Icon1Picked),
            Message::Icon1Picked(Some(p)) => return worker::blocking(move || load_icon1(&p), Message::Icon1Loaded),
            Message::Icon1Picked(None) => {}
            Message::Icon1Loaded(r) => match r {
                Ok(i) => {
                    self.log(format!("ICON1.PMF: {} ({:.1} s, {} B)", i.media.source, i.secs, i.media.bytes.len()));
                    self.icon1 = Some(i);
                    self.set_screen(Screen::Game);
                    self.audio.restart();
                }
                Err(e) => return self.toast(Kind::Error, format!("ICON1.PMF rejected: {e}")),
            },
            Message::ClearIcon1 => self.icon1 = None,
            Message::PickSnd0 => return Task::perform(pick_file("SND0.AT3", "ATRAC3 audio", &["at3"]), Message::Snd0Picked),
            Message::Snd0Picked(Some(p)) => return worker::blocking(move || load_snd0(&p), Message::Snd0Loaded),
            Message::Snd0Picked(None) => {}
            Message::Snd0Loaded(r) => match r {
                Ok(m) => {
                    self.log(format!("SND0.AT3: {} ({} B)", m.media.source, m.media.bytes.len()));
                    self.set_snd0(Some(m));
                    self.set_screen(Screen::Game);
                }
                Err(e) => return self.toast(Kind::Error, format!("SND0.AT3 rejected: {e}")),
            },
            Message::ClearSnd0 => self.set_snd0(None),
            Message::ToggleMute => {
                self.muted = !self.muted;
                self.audio.set_muted(self.muted);
            }
            Message::InstallTools => return self.install_tools(),
            Message::Level(l) => self.level = l,
            Message::PickOutput => {
                let mut d = rfd::AsyncFileDialog::new().set_title("Save EBOOT.PBP").set_file_name("EBOOT.PBP").add_filter("EBOOT", &["pbp"]);
                if let Some(dir) = self.output_path().as_deref().and_then(Path::parent) {
                    d = d.set_directory(dir);
                }
                return Task::perform(async move { d.save_file().await.map(|h| h.path().to_path_buf()) }, Message::OutputPicked);
            }
            Message::OutputPicked(p) => self.output = p.or(self.output.take()),
            Message::ResetOutput => self.output = None,
            Message::Screen(s) => self.set_screen(s),
            Message::Convert => return self.convert(),
            Message::Cancel => {
                if let Some(job) = &self.job
                    && !job.cancel.swap(true, Ordering::Relaxed)
                {
                    self.log(format!("{}: cancelling…", job.label));
                }
            }
            Message::Job(ev) => {
                if let Some((label, r)) = self.job_event(ev) {
                    let toast = match r {
                        Ok(msg) => self.toast(Kind::Success, format!("{label} complete — {msg}")),
                        Err(e) => self.job_failed(label, &e),
                    };
                    // Re-detect after an install, whether it fully succeeded or not.
                    return if label == JOB_INSTALL { Task::batch([toast, detect_tools_task()]) } else { toast };
                }
            }
            Message::ImportPbp => {
                return Task::perform(pick_file("Import EBOOT.PBP (title, ID, artwork)", "EBOOT", &["pbp"]), Message::ImportPicked);
            }
            Message::ImportPicked(Some(p)) => return worker::blocking(move || import(p), Message::Imported),
            Message::ImportPicked(None) => {}
            Message::Imported(r) => match r {
                Ok(imp) => {
                    if let Some(t) = imp.title {
                        self.title = t;
                        self.title_auto = false;
                    }
                    for (slot, art) in imp.art {
                        self.art[slot_ix(slot)] = Some(art);
                    }
                    let msg = format!(
                        "Imported {} — {} disc(s), ID {}",
                        file_name(&imp.path),
                        imp.discs,
                        imp.disc_id.as_deref().unwrap_or("?")
                    );
                    return self.toast(Kind::Info, msg);
                }
                Err(e) => return self.toast(Kind::Error, format!("Import failed: {e}")),
            },
            Message::ExtractPbp => {
                return Task::perform(
                    async {
                        let pbp = rfd::AsyncFileDialog::new().set_title("EBOOT.PBP to extract").add_filter("EBOOT", &["pbp"]).pick_file().await?;
                        let mut d = rfd::AsyncFileDialog::new().set_title("Extract into folder");
                        if let Some(dir) = pbp.path().parent() {
                            d = d.set_directory(dir);
                        }
                        let out = d.pick_folder().await?;
                        Some((pbp.path().to_path_buf(), out.path().to_path_buf()))
                    },
                    Message::ExtractPicked,
                );
            }
            Message::ExtractPicked(Some((pbp, out))) => {
                if self.job.is_some() {
                    return self.toast(Kind::Error, "Another job is running");
                }
                let cancel = Arc::new(AtomicBool::new(false));
                self.job = Some(Job { label: "Extract", progress: 0.0, cancel: cancel.clone() });
                self.log_open = true;
                return worker::job(
                    cancel,
                    move |m| {
                        let files = engine::extract(&pbp, &out, m).map_err(|e| e.to_string())?;
                        Ok(format!("{} file(s) in {}", files.len(), out.display()))
                    },
                    Message::Job,
                );
            }
            Message::ExtractPicked(None) => {}
            Message::FileDropped(p) => {
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
                return match ext.as_str() {
                    "pbp" => worker::blocking(move || import(p), Message::Imported),
                    "pmf" => Task::done(Message::Icon1Picked(Some(p))),
                    "at3" => Task::done(Message::Snd0Picked(Some(p))),
                    e if MEDIA_EXT.contains(&e) => Task::done(Message::ClipSourcePicked(Some(p))),
                    "ppf" => match self.discs.last().map(|d| d.key) {
                        Some(key) => Task::done(Message::PpfPicked(key, Some(p))),
                        None => self.toast(Kind::Error, "Add a disc before dropping a PPF patch"),
                    },
                    e if DISC_EXT.contains(&e) => self.add_discs(vec![p]),
                    e if IMAGE_EXT.contains(&e) => match art_slot_for(&p) {
                        Some(slot) => Task::done(Message::ArtPicked(slot, Some(p))),
                        None => self.toast(Kind::Error, format!("{}: name it ICON0 / PIC0 / PIC1 / BOOT, or use Browse", file_name(&p))),
                    },
                    _ => self.toast(Kind::Error, format!("{}: drop a .cue/.bin disc, a .ppf, an EBOOT.PBP, artwork, a video/audio clip, a .pmf or an .at3", file_name(&p))),
                };
            }
            Message::ToggleLog => self.log_open = !self.log_open,
            Message::Dismiss(id) => self.toasts.retain(|t| t.id != id),
            Message::ToolsDetected(r) => {
                if let Ok(t) = r {
                    let at = |p: &Option<PathBuf>| p.as_deref().map_or("not found".into(), |p| p.display().to_string());
                    self.log(format!(
                        "Tools: ffmpeg {}{} · ATRAC3 {} · yt-dlp {}",
                        at(&t.ffmpeg),
                        if t.ffmpeg.is_some() && !t.x264 { " (no libx264: ICON1 disabled)" } else { "" },
                        at(&t.at3),
                        at(&t.yt_dlp)
                    ));
                    self.tools = Some(t);
                }
            }
        }
        Task::none()
    }

    fn convert(&mut self) -> Task<Message> {
        if self.job.is_some() {
            return self.toast(Kind::Error, "A job is already running");
        }
        let problem = if self.discs.is_empty() {
            Some("Add at least one disc image (.cue or .bin)".to_owned())
        } else if let Some(d) = self.discs.iter().find(|d| d.effective_id().is_none()) {
            Some(format!("{}: no game ID detected — type it (e.g. SLUS00594)", file_name(&d.path)))
        } else if self.title.trim().is_empty() {
            Some("Enter a title".to_owned())
        } else if self.media_bytes() > tools::SND0_BUDGET {
            Some(format!("ICON1 + SND0 take {} KB; the XMB allows 500 KB — make a shorter clip", self.media_bytes().div_ceil(1024)))
        } else {
            None
        };
        if let Some(p) = problem {
            return self.toast(Kind::Error, p);
        }
        let Some(output) = self.output_path() else { return Task::none() };
        let png = |s: Slot| self.art[slot_ix(s)].as_ref().map(|a| a.png.clone());
        let req = ConvertRequest {
            discs: self
                .discs
                .iter()
                .map(|d| DiscInput {
                    path: d.path.clone(),
                    disc_id: Some(d.id.clone()).filter(|i| !i.is_empty()),
                    ppf: d.ppf.clone(),
                    pal_ntsc: d.pal_ntsc && !d.native_60hz(&self.patches),
                    title_fixes: d.title_fixes,
                    fps_unlock: d.fps_unlock,
                    ntsc_60hz: d.ntsc_60hz,
                    patch_choice: d.patch_choice.clone(),
                })
                .collect(),
            patches: self.patches.clone(),
            title: self.title.trim().to_owned(),
            output,
            donor: None,
            artwork: Artwork {
                icon0: png(Slot::Icon0),
                pic0: png(Slot::Pic0),
                pic1: png(Slot::Pic1),
                boot: png(Slot::Boot),
                icon1_pmf: self.icon1.as_ref().map(|i| i.media.bytes.to_vec()),
                snd0: self.snd0.as_ref().map(|m| m.media.bytes.to_vec()),
            },
            level: self.level,
        };
        let Some(cancel) = self.start_job("Build") else { return Task::none() };
        worker::job(cancel, move |m| build(&req, m), Message::Job)
    }

    /// Shared progress/log handling; yields the label and result once the job is done.
    fn job_event<T>(&mut self, ev: JobEvent<T>) -> Option<(&'static str, Result<T, String>)> {
        match ev {
            JobEvent::Progress(p) => {
                if let Some(job) = &mut self.job {
                    job.progress = p;
                }
                None
            }
            JobEvent::Log(l) => {
                self.log(l);
                None
            }
            JobEvent::Done(r) => {
                let job = self.job.take();
                // Whatever error a stopped tool produced, the user asked for this.
                let r = match (r, job.as_ref().is_some_and(|j| j.cancel.load(Ordering::Relaxed))) {
                    (Err(_), true) => Err(CANCELLED.into()),
                    (r, _) => r,
                };
                Some((job.map_or("Job", |j| j.label), r))
            }
        }
    }

    fn job_failed(&mut self, label: &str, e: &str) -> Task<Message> {
        if e == CANCELLED {
            self.log(format!("{label}: cancelled"));
            self.toast(Kind::Info, format!("{label} cancelled"))
        } else {
            self.toast(Kind::Error, format!("{label} failed: {e}"))
        }
    }

    fn start_job(&mut self, label: &'static str) -> Option<Arc<AtomicBool>> {
        if self.job.is_some() {
            return None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(Job { label, progress: 0.0, cancel: cancel.clone() });
        self.log_open = true;
        Some(cancel)
    }

    fn media_bytes(&self) -> u64 {
        (self.icon1.as_ref().map_or(0, |i| i.media.bytes.len()) + self.snd0.as_ref().map_or(0, |m| m.media.bytes.len())) as u64
    }

    /// `[start, end)` in seconds: empty start = 0, empty end = start + 30 s (capped at the source).
    fn clip_span(&self) -> Option<(f64, f64)> {
        let c = &self.clip;
        let start = if c.start.trim().is_empty() { 0.0 } else { tools::parse_hms(&c.start)? };
        let end = if c.end.trim().is_empty() {
            let e = start + CLIP_DEFAULT_SECS;
            c.len.map_or(e, |l| e.min(l))
        } else {
            tools::parse_hms(&c.end)?
        };
        (end - start >= 1.0 && c.len.is_none_or(|l| start < l)).then_some((start, end))
    }

    fn probe_clip(&mut self) -> Task<Message> {
        let src = self.clip.source.trim().to_owned();
        if src.is_empty() {
            return Task::none();
        }
        let key = src.clone();
        worker::blocking(
            move || {
                let tools = ClipTools::detect().map_err(|e| e.to_string())?;
                clip::probe(&tools, &src).map_err(|e| e.to_string())
            },
            move |r| Message::ClipProbed(key.clone(), r),
        )
    }

    fn make_clip(&mut self) -> Task<Message> {
        let source = self.clip.source.trim().to_owned();
        let Some((start, end)) = self.clip_span() else {
            return self.toast(Kind::Error, "Clip times: seconds or m:ss, end at least 1 s after start and start inside the source");
        };
        let (mode, kbps) = (self.clip.mode, self.clip.kbps);
        // The part not being re-made keeps its share of the 500 KB.
        let kept = match mode {
            ClipMode::Both => 0,
            ClipMode::Icon1 => self.snd0.as_ref().map_or(0, |m| m.media.bytes.len()),
            ClipMode::Snd0 => self.icon1.as_ref().map_or(0, |i| i.media.bytes.len()),
        } as u64;
        let budget = tools::SND0_BUDGET.saturating_sub(kept);
        let name = self.clip.title.clone().unwrap_or_else(|| if clip::is_url(&source) { "web clip".into() } else { file_name(Path::new(&source)) });
        let label = format!("{name} [{}–{}]", tools::fmt_hms(start), tools::fmt_hms(end));
        let Some(cancel) = self.start_job(JOB_CLIP) else { return self.toast(Kind::Error, "A job is already running") };
        self.log(format!("Clip: {label} → {mode}"));
        worker::job(cancel, move |m| run_clip(&ClipRequest { source: &source, start, end, mode, snd0_kbps: kbps, budget }, label, m), Message::ClipJob)
    }

    fn install_tools(&mut self) -> Task<Message> {
        let assets = self.tools.as_ref().map_or_else(Vec::new, |t| t.missing.clone());
        if assets.is_empty() {
            return Task::none();
        }
        let Some(cancel) = self.start_job(JOB_INSTALL) else { return self.toast(Kind::Error, "A job is already running") };
        worker::job(
            cancel,
            move |m| {
                for (i, a) in assets.iter().enumerate() {
                    if m.cancelled() {
                        return Err(CANCELLED.into());
                    }
                    m.progress(i as u64, assets.len() as u64);
                    provision::install(a, &|s: &str| m.log(s)).map_err(|e| e.to_string())?;
                }
                m.progress(1, 1);
                let names: Vec<_> = assets.iter().map(|a| a.tool.exe()).collect();
                Ok(format!("{} → {}", names.join(", "), tools::tools_dir().map_or_else(String::new, |d| d.display().to_string())))
            },
            Message::Job,
        )
    }

    /// Switches the preview screen. SND0 plays on the Game screen only and restarts each time
    /// it is entered, like the XMB when the game icon gets focus.
    fn set_screen(&mut self, s: Screen) {
        if s == Screen::Game && self.screen != Screen::Game {
            self.audio.restart();
        }
        self.screen = s;
        self.audio.set_active(s == Screen::Game);
    }

    fn set_snd0(&mut self, s: Option<Snd0>) {
        if let Some(a) = &s {
            match &a.sound {
                Some(snd) => self.log(format!("SND0 preview: {:.1} s decoded from the AT3 itself, audio output {}", snd.secs(), self.audio.status())),
                None => self.log("SND0 preview unavailable: install ffmpeg to hear it"),
            }
        }
        self.audio.set(s.as_ref().and_then(|a| a.sound.clone()));
        self.snd0 = s;
    }

    fn layers(&self) -> [Option<Arc<Pixels>>; 4] {
        let px = |s: Slot| self.art[slot_ix(s)].as_ref().map(|a| a.pixels.clone());
        let mut l = [None, None, None, None];
        l[xmb::PIC1] = px(Slot::Pic1);
        l[xmb::PIC0] = px(Slot::Pic0);
        l[xmb::ICON0] = px(Slot::Icon0);
        l[xmb::BOOT] = px(Slot::Boot).or_else(|| self.donor_boot.clone());
        l
    }
}

/// Private scratch folder for one media job (small: a clip section, a WAV, an H.264 stream).
fn scratch(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!("psx2psp-reborn-{}-{tag}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

/// Validates an ICON1.PMF and decodes its frames for the animated preview (needs ffmpeg).
fn icon1_from(source: String, bytes: Vec<u8>) -> Result<Icon1, String> {
    let info = psmf::parse(&bytes).map_err(|e| e.to_string())?;
    let work = scratch("icon1");
    let movie = Ffmpeg::detect().and_then(|ff| clip::icon1_frames(&ff, &bytes, &work, PREVIEW_FRAMES).ok()).map(|(w, h, f)| Movie::new(w, h, f));
    let _ = std::fs::remove_dir_all(&work);
    Ok(Icon1 { secs: info.duration_secs(), media: Media { source, bytes: Arc::new(bytes) }, movie })
}

fn load_icon1(p: &Path) -> Result<Icon1, String> {
    icon1_from(file_name(p), std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?)
}

fn load_snd0(p: &Path) -> Result<Snd0, String> {
    let bytes = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
    match tools::riff_atrac3_block_align(&bytes) {
        None => return Err(format!("{}: not a RIFF ATRAC3 file (OMA/EA3 and raw ATRAC3 are not accepted by the XMB)", file_name(p))),
        Some(_) if !tools::is_psp_atrac3(&bytes) => {
            return Err(format!("{}: non-standard ATRAC3 frame size — re-encode at 66, 105 or 132 kbps", file_name(p)));
        }
        Some(_) => {}
    }
    if bytes.len() as u64 > tools::SND0_BUDGET {
        return Err(format!("{}: {} KB, the XMB allows 500 KB", file_name(p), bytes.len().div_ceil(1024)));
    }
    snd0_from(file_name(p), bytes)
}

/// Decodes the SND0.AT3 bytes themselves for the preview (needs ffmpeg). A file ffmpeg cannot
/// decode is rejected: what the preview plays is what goes in the EBOOT.
fn snd0_from(source: String, bytes: Vec<u8>) -> Result<Snd0, String> {
    let sound = match Ffmpeg::detect() {
        Some(ff) => {
            let work = scratch("snd0");
            let pcm = clip::snd0_pcm(&ff, &bytes, &work);
            let _ = std::fs::remove_dir_all(&work);
            Some(audio::Sound::new(clip::SND0_RATE, pcm.map_err(|e| format!("{source}: ffmpeg cannot decode it ({e})"))?))
        }
        None => None,
    };
    Ok(Snd0 { media: Media { source, bytes: Arc::new(bytes) }, sound })
}

/// Worker side of "Make clip": source → ICON1.PMF and/or SND0.AT3 (+ decoded preview).
fn run_clip(req: &ClipRequest<'_>, label: String, m: &dyn Monitor) -> Result<ClipOut, String> {
    let tools = ClipTools::detect().map_err(|e| e.to_string())?;
    let work = scratch("clip");
    let r = clip::make_clip(&tools, req, &work, m);
    let _ = std::fs::remove_dir_all(&work);
    let c = r.map_err(|e| e.to_string())?;
    if m.cancelled() {
        return Err(CANCELLED.into());
    }
    Ok(ClipOut {
        icon1: c.icon1.map(|b| icon1_from(format!("{label}.pmf"), b)).transpose()?,
        snd0: c.snd0.map(|b| snd0_from(format!("{label}.at3"), b)).transpose()?,
    })
}

/// Worker side of "Build": EBOOT.PBP with the prepared artwork and media.
fn build(req: &ConvertRequest, m: &dyn Monitor) -> Result<String, String> {
    for (name, b) in [("ICON1.PMF", &req.artwork.icon1_pmf), ("SND0.AT3", &req.artwork.snd0)] {
        if let Some(b) = b {
            m.log(&format!("{name}: {} B", b.len()));
        }
    }
    let rep = engine::convert(req, m).map_err(|e| e.to_string())?;
    Ok(format!(
        "{} ({:.1} MB, {:.1}% of raw) in {:.1}s",
        rep.output.display(),
        rep.file_size as f64 / 1e6,
        rep.stats.stored_bytes as f64 * 100.0 / rep.stats.raw_bytes.max(1) as f64,
        rep.elapsed.as_secs_f64()
    ))
}

/// Worker side of "Import": title, ID and artwork of an existing EBOOT.
fn import(path: PathBuf) -> Result<Imported, String> {
    let s = engine::inspect(&path).map_err(|e| e.to_string())?;
    let mut art = Vec::new();
    for (slot, png) in [(Slot::Icon0, &s.icon0), (Slot::Pic0, &s.pic0), (Slot::Pic1, &s.pic1), (Slot::Boot, &s.boot)] {
        if let Some(png) = png {
            // Embedded art that fails to decode (APNG, odd sizes) is skipped, not fatal.
            if let Ok(si) = images::prepare(slot, png) {
                art.push((slot, Art::from_slot_image(format!("{} (imported)", file_name(&path)), si)));
            }
        }
    }
    Ok(Imported {
        title: s.sfo.get_str("TITLE").map(str::to_owned),
        disc_id: s.sfo.get_str("DISC_ID").map(str::to_owned),
        discs: s.discs.len(),
        art,
        path,
    })
}

// ────────────────────────────────── view ──────────────────────────────────

fn label<'a>(s: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(s).size(12).color(style::MUTED)
}

fn heading<'a>(s: &'a str, accent: Color) -> Element<'a, Message> {
    row![container(space().width(4).height(16)).style(move |_| container::Style::default().background(accent)), text(s).size(15)]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
}

fn card<'a>(title: &'a str, accent: Color, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(column![heading(title, accent), body.into()].spacing(14)).padding(16).width(Fill).style(style::card).into()
}

fn ghost(s: &str) -> button::Button<'_, Message> {
    button(text(s).size(13)).padding([6, 12]).style(style::ghost)
}

/// `content` with a hover explanation above it.
fn hint<'a>(content: impl Into<Element<'a, Message>>, explanation: String) -> Element<'a, Message> {
    let bubble = container(text(explanation).size(12)).padding([6, 10]).max_width(340).style(style::toast(style::BLUE));
    tooltip(content, bubble, tooltip::Position::Top).gap(6).into()
}

fn file_row<'a>(name: String, browse: Message, clear: Option<Message>) -> Element<'a, Message> {
    row![
        container(text(name).size(13).wrapping(text::Wrapping::None)).padding([7, 10]).width(Fill).clip(true).style(style::well),
        ghost("Browse").on_press(browse),
        ghost("✕").on_press_maybe(clear),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

impl App {
    fn view(&self) -> Element<'_, Message> {
        let sidebar = scrollable(column![self.game_card(), self.art_card(), self.media_card(), self.build_card()].spacing(14).padding(16))
            .width(440)
            .height(Fill);
        let main = column![self.header(), self.preview(), self.footer()].spacing(14).padding(16).width(Fill);
        let body = row![container(sidebar).style(style::panel).height(Fill), main];
        stack![body, self.toasts()].into()
    }

    fn game_card(&self) -> Element<'_, Message> {
        let mut discs = column![].spacing(8);
        for (i, d) in self.discs.iter().enumerate() {
            let probed = d.summary.as_ref();
            let info = match (probed, &d.error) {
                (_, Some(e)) => text(e.as_str()).size(12).color(style::RED),
                (Some(s), None) => label(format!(
                    "{} · {} track(s){} · {:.0} MB",
                    s.region.label(),
                    s.tracks,
                    if s.audio_tracks > 0 { format!(" ({} CDDA)", s.audio_tracks) } else { String::new() },
                    s.size as f64 / 1e6
                )),
                (None, None) => label("Probing…"),
            };
            let placeholder = probed.and_then(|s| s.disc_id.clone()).unwrap_or_else(|| "GAME ID".into());
            let ppf = match &d.ppf {
                Some(p) => row![text(format!("PPF {}", file_name(p))).size(12).color(style::GREEN), ghost("✕").on_press(Message::ClearPpf(d.key))]
                    .spacing(6)
                    .align_y(Alignment::Center),
                None => row![ghost("+ PPF").on_press(Message::PickPpf(d.key))],
            };
            let key = d.key;
            let switch = |on: bool, name: &str, msg: Message| {
                button(text(if on { format!("✓ {name}") } else { name.to_owned() }).size(12).wrapping(text::Wrapping::None))
                    .padding([6, 10])
                    .style(style::tab(on))
                    .on_press(msg)
            };
            // The generic preset only where the game has no 60 Hz switch of its own.
            let db = &self.patches;
            let pal: Element<'_, Message> = if d.native_60hz(db) {
                space().into()
            } else {
                hint(
                    switch(d.pal_ntsc, "PAL→NTSC", Message::TogglePalNtsc(key)),
                    "Generic PAL→NTSC (PSX2PSP Graphic Fix / Y Fix). Applied only if this game's code matches                      the patterns; otherwise the disc is left untouched and the log says why."
                        .into(),
                )
            };
            let mut body = column![
                row![
                    text(format!("{}", i + 1)).size(20).color(style::BLUE),
                    column![text(file_name(&d.path)).size(13).wrapping(text::Wrapping::None), info].spacing(2).width(Fill),
                    ghost("↑").on_press_maybe((i > 0).then_some(Message::MoveDiscUp(key))),
                    ghost("✕").on_press(Message::RemoveDisc(key)),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
                row![
                    text_input(&placeholder, &d.id).on_input(move |s| Message::DiscId(key, s)).size(13).padding(7).width(140).style(style::input),
                    space().width(Fill),
                    pal,
                    ppf,
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ]
            .spacing(8);
            // Only for IDs the patch database knows (POPS config and/or patches).
            let (game, ps) = (d.config(db), d.patches(db));
            if game.is_some() || !ps.is_empty() {
                let names = |k: patchdb::Kind| ps.iter().filter(|p| p.kind == k).map(|p| p.name.as_str()).collect::<Vec<_>>().join("; ");
                let mut what = names(patchdb::Kind::Fix);
                if game.is_some() {
                    what = if what.is_empty() { patchdb::CONFIG.to_owned() } else { format!("{}; {what}", patchdb::CONFIG) };
                }
                let mut fixes = row![
                    label("Patches for this game"),
                    space().width(Fill),
                    hint(switch(d.title_fixes, "Game fixes", Message::ToggleTitleFixes(key)), format!("Recommended: {what}. Patches marked \"off by default\" stay off until you pick them below."))
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                if d.has_kind(db, patchdb::Kind::Fps) {
                    fixes = fixes.push(hint(
                        switch(d.fps_unlock, "Unlock fps", Message::ToggleFpsUnlock(key)),
                        format!("{}. The game's timing follows real elapsed time, so speed stays correct.", names(patchdb::Kind::Fps)),
                    ));
                }
                if d.native_60hz(db) {
                    fixes = fixes.push(hint(
                        switch(d.ntsc_60hz, "60 Hz", Message::Toggle60Hz(key)),
                        format!("{}. Uses the game's own NTSC code: no generic PAL→NTSC, POPS config left as is.", names(patchdb::Kind::Ntsc60)),
                    ));
                }
                // One line per entry: its own toggle (wins over the switches), kind and origin.
                let entry = |on: bool, name: &str, tag: String, note: String| -> Element<'_, Message> {
                    row![
                        hint(switch(on, name, Message::TogglePatch(key, name.to_owned())), note),
                        space().width(Fill),
                        text(tag).size(11).color(style::MUTED),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .into()
                };
                let mut list = column![].spacing(4);
                if let Some(g) = game {
                    let size = g.config.as_ref().map_or(0, Vec::len);
                    let note = format!("POPS emulator settings for this game ({size} B, {}). {}", g.source, g.note);
                    let origin = if g.origin == patchdb::Origin::User { " · user" } else { "" };
                    list = list.push(entry(d.config_on(), patchdb::CONFIG, format!("Game fix{origin}"), note));
                }
                for p in &ps {
                    let mut tag = p.kind.to_string();
                    if !p.default_on {
                        tag.push_str(" · off by default");
                    }
                    if p.origin == patchdb::Origin::User {
                        tag.push_str(" · user");
                    }
                    list = list.push(entry(d.patch_on(p), &p.name, tag, p.note.clone()));
                }
                let mut result = row![text(d.outcome(db)).size(12).color(style::GREEN), space().width(Fill)].align_y(Alignment::Center);
                if !d.all_on(db) {
                    result = result.push(hint(ghost("★ All on").on_press(Message::AllFixes(key)), "Turn on every switch this game offers (patches off by default stay off).".into()));
                }
                body = body.push(fixes).push(list).push(result);
            }
            discs = discs.push(container(body).padding(10).style(style::well));
        }
        let add = button(text(if self.discs.is_empty() { "+ Add disc image (.cue / .bin) — or drop it here" } else { "+ Add disc" }).size(13))
            .padding([8, 12])
            .width(Fill)
            .style(style::ghost)
            .on_press_maybe((self.discs.len() < MAX_DISCS).then_some(Message::AddDiscs));
        card(
            "Game",
            style::BLUE,
            column![
                label("XMB title"),
                text_input("Game title", &self.title).on_input(Message::Title).size(14).padding(9).style(style::input),
                label(format!("Discs ({}/{MAX_DISCS})", self.discs.len())),
                discs,
                add,
            ]
            .spacing(8),
        )
    }

    fn art_card(&self) -> Element<'_, Message> {
        let mut col = column![].spacing(10);
        for slot in Slot::ALL {
            let (w, h) = slot.size();
            let art = self.art[slot_ix(slot)].as_ref();
            let thumb: Element<'_, Message> = match art {
                Some(a) => image(a.thumb.clone()).width(72).height(41).content_fit(iced::ContentFit::Contain).into(),
                None => container(text("—").size(12).color(style::MUTED)).center(Length::Fixed(41.0)).width(72).style(style::well).into(),
            };
            let name = art.map_or_else(|| format!("{w}×{h} · none"), |a| a.source.clone());
            col = col.push(
                row![
                    thumb,
                    column![text(slot.label()).size(13), label(name).wrapping(text::Wrapping::None)].spacing(2).width(Fill),
                    ghost("Browse").on_press(Message::PickArt(slot)),
                    ghost("✕").on_press_maybe(art.map(|_| Message::ClearArt(slot))),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            );
        }
        let hint = if self.art[slot_ix(Slot::Boot)].is_none() && self.donor_boot.is_some() { "No BOOT image: the standard PS1 boot screen is used" } else { "Any image format; auto-fitted to the PSP slot" };
        card("Artwork", style::RED, column![col, label(hint)].spacing(10))
    }

    fn media_card(&self) -> Element<'_, Message> {
        let c = &self.clip;
        let idle = self.job.is_none();
        let tools: Element<'_, Message> = match &self.tools {
            None => label("Detecting ffmpeg / x264 / ATRAC3 / yt-dlp…").into(),
            Some(t) => {
                let mark = |ok: bool| if ok { "✓" } else { "✗" };
                let all = t.ffmpeg.is_some() && t.x264 && t.at3.is_some() && t.yt_dlp.is_some();
                let line = text(format!(
                    "ffmpeg {} · x264 {} · ATRAC3 {} · yt-dlp {}",
                    mark(t.ffmpeg.is_some()),
                    mark(t.x264),
                    mark(t.at3.is_some()),
                    mark(t.yt_dlp.is_some())
                ))
                .size(12)
                .color(if all { style::GREEN } else { style::AMBER });
                if t.missing.is_empty() {
                    line.into()
                } else {
                    let mb: u32 = t.missing.iter().map(|a| a.approx_mb).sum();
                    row![
                        line.width(Fill),
                        button(text(format!("Install missing (~{mb} MB)")).size(12))
                            .padding([5, 10])
                            .style(style::ghost)
                            .on_press_maybe(idle.then_some(Message::InstallTools)),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .into()
                }
            }
        };
        let source_info = if c.probed {
            let name = c.title.clone().unwrap_or_else(|| file_name(Path::new(c.source.trim())));
            label(format!("{name} · {}", c.len.map_or("length unknown".into(), tools::fmt_hms))).wrapping(text::Wrapping::None)
        } else {
            label("Paste a YouTube (or any yt-dlp) link, browse or drop a video/audio file")
        };
        let span = self.clip_span();
        let end_hint = span.map_or_else(|| "end".into(), |(_, e)| tools::fmt_hms(e));
        let tab = |t: String, on: bool, msg: Message| button(text(t).size(12)).padding([5, 10]).style(style::tab(on)).on_press(msg);
        let modes = ClipMode::ALL.iter().fold(row![].spacing(4), |r, &m| r.push(tab(m.label().into(), c.mode == m, Message::ClipMode(m))));
        let audio: Element<'_, Message> = match c.mode {
            ClipMode::Snd0 => [66, 105, 132]
                .into_iter()
                .fold(row![label("ATRAC3").width(50)].spacing(4).align_y(Alignment::Center), |r, k| {
                    r.push(tab(format!("{k} kbps"), c.kbps == k, Message::ClipKbps(k)))
                })
                .into(),
            ClipMode::Both => label(format!("SND0 at {} kbps; the video gets the rest of the 500 KB", clip::SHARED_KBPS)).into(),
            ClipMode::Icon1 => label("144×80 H.264 at 29.97 fps, bitrate fitted to the free budget").into(),
        };
        let can_make = idle && !c.source.trim().is_empty() && span.is_some();
        let make = button(text(format!("Make {}", c.mode.label())).size(13))
            .padding([8, 16])
            .style(style::solid(style::PINK))
            .on_press_maybe(can_make.then_some(Message::MakeClip));
        let used = self.media_bytes();
        let budget = text(format!("{} / 500 KB used by ICON1 + SND0", used.div_ceil(1024)))
            .size(12)
            .color(if used > tools::SND0_BUDGET { style::RED } else { style::MUTED });
        let icon1 = self.icon1.as_ref().map_or_else(
            || "none".into(),
            |i| format!("{} · {:.1} s · {} KB{}", i.media.source, i.secs, i.media.bytes.len().div_ceil(1024), if i.movie.is_some() { "" } else { " · no preview" }),
        );
        let snd0 = self.snd0.as_ref().map_or_else(
            || "none".into(),
            |m| match &m.sound {
                Some(s) => format!("{} · {:.1} s · {} KB", m.media.source, s.secs(), m.media.bytes.len().div_ceil(1024)),
                None => format!("{} · {} KB · no preview", m.media.source, m.media.bytes.len().div_ceil(1024)),
            },
        );
        card(
            "Media",
            style::PINK,
            column![
                label("Clip source"),
                row![
                    text_input("https://youtu.be/…  or  a video / audio file", &c.source)
                        .on_input(Message::ClipSource)
                        .on_submit(Message::ProbeClip)
                        .size(13)
                        .padding(7)
                        .width(Fill)
                        .style(style::input),
                    ghost("Browse").on_press(Message::PickClipSource),
                    ghost("Probe").on_press_maybe((!c.source.trim().is_empty()).then_some(Message::ProbeClip)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                source_info,
                row![
                    label("From"),
                    text_input("0:00", &c.start).on_input(Message::ClipStart).size(13).padding(7).width(78).style(style::input),
                    label("to"),
                    text_input(&end_hint, &c.end).on_input(Message::ClipEnd).size(13).padding(7).width(78).style(style::input),
                    space().width(Fill),
                    label(span.map_or_else(|| "invalid span".into(), |(s, e)| format!("{:.1} s", e - s))),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                modes,
                audio,
                row![space().width(Fill), make],
                label("ICON1.PMF — animated icon (made above, or a ready .pmf)"),
                file_row(icon1, Message::PickIcon1, self.icon1.as_ref().map(|_| Message::ClearIcon1)),
                label("SND0.AT3 — XMB music (made above, or a ready .at3)"),
                file_row(snd0, Message::PickSnd0, self.snd0.as_ref().map(|_| Message::ClearSnd0)),
                budget,
                tools,
            ]
            .spacing(8),
        )
    }

    fn build_card(&self) -> Element<'_, Message> {
        let output = self.output_path().map_or_else(|| "<disc folder>/<GAME ID>/EBOOT.PBP".into(), |p| p.display().to_string());
        card(
            "Build",
            style::GREEN,
            column![
                label("Output"),
                file_row(output, Message::PickOutput, self.output.as_ref().map(|_| Message::ResetOutput)),
                row![
                    label(format!("Compression {}", self.level)).width(110),
                    slider(1..=9, self.level, Message::Level),
                    label(match self.level {
                        1..=4 => "faster, larger",
                        5..=6 => "recommended",
                        _ => "slower, ~same size",
                    })
                    .width(130),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
                row![
                    ghost("Import EBOOT…").on_press(Message::ImportPbp).width(Fill),
                    ghost("Extract EBOOT…").on_press_maybe(self.job.is_none().then_some(Message::ExtractPbp)).width(Fill),
                ]
                .spacing(8),
            ]
            .spacing(8),
        )
    }

    fn header(&self) -> Element<'_, Message> {
        let tabs = Screen::ALL.iter().fold(row![].spacing(4), |r, &s| {
            r.push(button(text(s.label()).size(13)).padding([6, 18]).style(style::tab(self.screen == s)).on_press(Message::Screen(s)))
        });
        row![
            column![text("PSX2PSP Reborn").size(22), label("PS1 → PSP / PS Vita EBOOT.PBP studio")].spacing(2),
            space().width(Fill),
            self.snd0.as_ref().filter(|s| s.sound.is_some()).map(|_| {
                button(text(if self.muted { "Sound off" } else { "Sound on" }).size(13))
                    .padding([6, 14])
                    .style(style::tab(!self.muted))
                    .on_press(Message::ToggleMute)
            }),
            container(tabs).padding(4).style(style::well),
        ]
        .align_y(Alignment::Center)
        .into()
    }

    fn preview(&self) -> Element<'_, Message> {
        let layers = self.layers();
        let screen = self.screen;
        let title = if self.title.trim().is_empty() { "Untitled".to_owned() } else { self.title.trim().to_owned() };
        let subtitle = self.discs.first().and_then(Disc::effective_id).unwrap_or_default();
        let movie = self.icon1.as_ref().and_then(|i| i.movie.clone());
        let clock = self.snd0.as_ref().and_then(|s| s.sound.as_ref()).map(|_| self.audio.clock());
        let empty = self.discs.is_empty() && layers.iter().all(Option::is_none) && movie.is_none();
        let preview = responsive(move |size| {
            let k = (size.width / 480.0).min(size.height / 272.0).max(0.1);
            let (w, h) = (480.0 * k, 272.0 * k);
            let at = |x: f32, y: f32, e: Element<'static, Message>| -> Element<'static, Message> { pin(e).x(x * k).y(y * k).into() };
            let mut layers_ui: Vec<Element<'static, Message>> =
                vec![shader(Xmb { screen, layers: layers.clone(), movie: movie.clone(), clock: clock.clone(), accent: WAVE }).width(w).height(h).into()];
            match screen {
                Screen::Game => {
                    layers_ui.push(at(172.0, 112.0, text(title.clone()).size(13.0 * k).color(Color::WHITE).into()));
                    layers_ui.push(at(172.0, 130.0, text(subtitle.clone()).size(8.5 * k).color(Color::from_rgba(1.0, 1.0, 1.0, 0.7)).into()));
                }
                Screen::Save => {
                    layers_ui.push(at(184.0, 104.0, text(title.clone()).size(12.0 * k).color(Color::WHITE).into()));
                    layers_ui.push(at(184.0, 122.0, text("Memory Card 1 · Saved Data").size(8.5 * k).color(Color::from_rgba(1.0, 1.0, 1.0, 0.7)).into()));
                    layers_ui.push(at(184.0, 136.0, text(subtitle.clone()).size(8.5 * k).color(Color::from_rgba(1.0, 1.0, 1.0, 0.55)).into()));
                }
                Screen::Boot => {}
            }
            if empty {
                layers_ui.push(at(
                    120.0,
                    232.0,
                    text("Drop a .cue / .bin or an EBOOT.PBP to begin").size(10.0 * k).color(Color::from_rgba(1.0, 1.0, 1.0, 0.8)).into(),
                ));
            }
            center(stack(layers_ui).width(w).height(h)).into()
        });
        container(preview).width(Fill).height(Fill).into()
    }

    fn footer(&self) -> Element<'_, Message> {
        let bar: Element<'_, Message> = match &self.job {
            Some(job) => row![
                text(if job.progress > 0.0 { format!("{} {:>5.1}%", job.label, job.progress * 100.0) } else { format!("{}…", job.label) })
                    .size(13)
                    .width(120),
                progress_bar(0.0..=1.0, job.progress).girth(10),
                {
                    let stopping = job.cancel.load(Ordering::Relaxed);
                    button(text(if stopping { "Stopping…" } else { "Cancel" }).size(13))
                        .padding([8, 18])
                        .style(style::solid(style::RED))
                        .on_press_maybe((!stopping).then_some(Message::Cancel))
                },
            ]
            .spacing(14)
            .align_y(Alignment::Center)
            .into(),
            None => row![
                label(self.output_path().map_or_else(|| "Add a disc to choose the output".into(), |p| format!("→ {}", p.display())))
                    .wrapping(text::Wrapping::None)
                    .width(Fill),
                ghost(if self.log_open { "Hide log" } else { "Show log" }).on_press(Message::ToggleLog),
                button(text("Build EBOOT.PBP").size(14)).padding([10, 22]).style(style::solid(style::GREEN)).on_press(Message::Convert),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into(),
        };
        let mut col = column![bar].spacing(10);
        if self.log_open {
            let lines = self.log.iter().fold(column![].spacing(2), |c, l| c.push(text(l.as_str()).size(12).font(Font::MONOSPACE).color(style::MUTED)));
            col = col.push(container(scrollable(lines).anchor_bottom().width(Fill).height(170)).padding(10).width(Fill).style(style::well));
        }
        container(col).padding(14).style(style::card).into()
    }

    fn toasts(&self) -> Element<'_, Message> {
        let list = self.toasts.iter().fold(column![].spacing(8).width(380), |c, t| {
            let accent = match t.kind {
                Kind::Info => style::BLUE,
                Kind::Success => style::GREEN,
                Kind::Error => style::RED,
            };
            c.push(
                button(container(text(t.text.as_str()).size(13)).padding(12).width(Fill).style(style::toast(accent)))
                    .padding(0)
                    .style(|_, _| button::Style::default())
                    .on_press(Message::Dismiss(t.id)),
            )
        });
        container(list).padding(20).align_right(Fill).align_bottom(Fill).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_names_map_to_slots() {
        assert_eq!(art_slot_for(Path::new("x/ICON0.PNG")), Some(Slot::Icon0));
        assert_eq!(art_slot_for(Path::new("pic0.jpg")), Some(Slot::Pic0));
        assert_eq!(art_slot_for(Path::new("Pic1.webp")), Some(Slot::Pic1));
        assert_eq!(art_slot_for(Path::new("STARTDAT.PNG")), Some(Slot::Boot));
        assert_eq!(art_slot_for(Path::new("boot.png")), Some(Slot::Boot));
        assert_eq!(art_slot_for(Path::new("cover.png")), None);
    }

    #[test]
    fn clip_span_defaults_and_validation() {
        let mut app = App::boot().0;
        assert_eq!(app.clip_span(), Some((0.0, 30.0)), "no probe: start + 30 s");
        app.clip.len = Some(12.0);
        assert_eq!(app.clip_span(), Some((0.0, 12.0)), "capped at the source");
        (app.clip.start, app.clip.end) = ("0:05".into(), "1:00.5".into());
        assert_eq!(app.clip_span(), Some((5.0, 60.5)));
        app.clip.end = "0:05.5".into();
        assert_eq!(app.clip_span(), None, "under 1 s");
        app.clip.end = "x".into();
        assert_eq!(app.clip_span(), None);
        (app.clip.start, app.clip.end) = ("20".into(), String::new());
        assert_eq!(app.clip_span(), None, "start past the end of the source");
    }

    /// A Sony ICON1.PMF loads, validates and (with ffmpeg) decodes into preview frames.
    #[test]
    fn sony_icon1_loads_with_preview() {
        let p = Path::new(r"E:\Programmi VARI\Consoles TOOLS\PSP\PSX2PSPv1.4.2_BASE\Pics\icon1.pmf");
        if !p.is_file() {
            return;
        }
        let i = load_icon1(p).unwrap();
        assert!((i.secs - 605.0 * 1001.0 / 30000.0).abs() < 0.05, "{}", i.secs);
        if let Some(m) = &i.movie {
            assert_eq!((m.width, m.height, m.frames.len()), (144, 80, 605));
        }
        assert!(load_snd0(p).is_err(), "a PMF is not an AT3");
    }
}
