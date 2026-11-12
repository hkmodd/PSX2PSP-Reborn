//! PSX2PSP-Reborn — desktop GUI (iced 0.14 + wgpu).
//!
//! Elm architecture: `App` is the single source of truth, `update` is the only mutator, `view`
//! is a pure function of state. Every disk/CPU-heavy operation runs in `worker` threads.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod style;
mod worker;
mod xmb;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iced::widget::{
    button, center, column, container, image, pin, progress_bar, responsive, row, scrollable, shader, slider, space, stack, text,
    text_input,
};
use iced::{Alignment, Color, Element, Fill, Font, Length, Size, Subscription, Task, Theme, event, window};
use psx2psp_engine::core::Donor;
use psx2psp_engine::media::images::{self, Slot};
use psx2psp_engine::media::tools::{self, At3Encoder, Ffmpeg, Snd0Request};
use psx2psp_engine::{self as engine, Artwork, ConvertRequest, DEFAULT_LEVEL, DiscInput, DiscSummary};

use worker::JobEvent;
use xmb::{Pixels, Screen, Xmb};

const MAX_DISCS: usize = 5;
const MAX_LOG: usize = 2000;
const TOAST_TTL: Duration = Duration::from_secs(6);
const WAVE: Color = iced::color!(0x2a55c8);

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "bmp", "gif", "webp"];
const DISC_EXT: &[&str] = &["cue", "bin", "img"];
const MEDIA_EXT: &[&str] = &["mp3", "wav", "flac", "ogg", "opus", "m4a", "aac", "mp4", "mkv", "webm"];

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
    discs: Vec<Disc>,
    next_key: u64,
    donor: Option<PathBuf>,
    donor_boot: Option<Arc<Pixels>>,
    /// Indexed by `slot_ix` (ICON0, PIC0, PIC1, BOOT).
    art: [Option<Art>; 4],
    icon1: Option<PathBuf>,
    snd0: Option<PathBuf>,
    snd0_start: f32,
    level: u32,
    output: Option<PathBuf>,
    screen: Screen,
    job: Option<Job>,
    log: Vec<String>,
    log_open: bool,
    toasts: Vec<Toast>,
    tools: Option<Tools>,
}

struct Disc {
    key: u64,
    path: PathBuf,
    summary: Option<DiscSummary>,
    error: Option<String>,
    /// User override; empty = use the probed ID.
    id: String,
    ppf: Option<PathBuf>,
}

impl Disc {
    fn effective_id(&self) -> Option<String> {
        let id = self.id.trim();
        if id.is_empty() { self.summary.as_ref().and_then(|s| s.disc_id.clone()) } else { Some(id.to_owned()) }
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

#[derive(Debug, Clone)]
struct Tools {
    ffmpeg: Option<PathBuf>,
    at3: Option<PathBuf>,
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
    PickDonor,
    DonorPicked(Option<PathBuf>),
    DonorLoaded(PathBuf, Result<Arc<Pixels>, String>),
    PickArt(Slot),
    ArtPicked(Slot, Option<PathBuf>),
    ArtLoaded(Slot, Result<Art, String>),
    ClearArt(Slot),
    PickIcon1,
    Icon1Picked(Option<PathBuf>),
    ClearIcon1,
    PickSnd0,
    Snd0Picked(Option<PathBuf>),
    ClearSnd0,
    Snd0Start(f32),
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

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

// ───────────────────────────────── update ─────────────────────────────────

impl App {
    fn boot() -> (Self, Task<Message>) {
        let app = Self {
            theme: style::theme(),
            title: String::new(),
            discs: Vec::new(),
            next_key: 1,
            donor: None,
            donor_boot: None,
            art: Default::default(),
            icon1: None,
            snd0: None,
            snd0_start: 0.0,
            level: DEFAULT_LEVEL,
            output: None,
            screen: Screen::Game,
            job: None,
            log: Vec::new(),
            log_open: false,
            toasts: Vec::new(),
            tools: None,
        };
        let detect = worker::blocking(
            || Ok(Tools { ffmpeg: Ffmpeg::detect().map(|f| f.path), at3: At3Encoder::detect().map(|e| e.path().to_path_buf()) }),
            Message::ToolsDetected,
        );
        // Paths on the command line ("Open with", drag onto the exe) behave like dropped files.
        let opened = std::env::args_os().skip(1).map(|a| Task::done(Message::FileDropped(PathBuf::from(a))));
        (app, Task::batch(std::iter::once(detect).chain(opened)))
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

    fn add_discs(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        let room = MAX_DISCS - self.discs.len();
        let mut tasks = Vec::new();
        if paths.len() > room {
            tasks.push(self.toast(Kind::Error, format!("A PBP holds at most {MAX_DISCS} discs; extra files ignored")));
        }
        for path in paths.into_iter().take(room) {
            let key = self.next_key;
            self.next_key += 1;
            self.discs.push(Disc { key, path: path.clone(), summary: None, error: None, id: String::new(), ppf: None });
            tasks.push(worker::blocking(move || engine::probe_disc(&path).map_err(|e| e.to_string()), move |r| Message::DiscProbed(key, r)));
        }
        Task::batch(tasks)
    }

    fn disc_mut(&mut self, key: u64) -> Option<&mut Disc> {
        self.discs.iter_mut().find(|d| d.key == key)
    }

    fn output_path(&self) -> Option<PathBuf> {
        self.output.clone().or_else(|| {
            let d = self.discs.first()?;
            Some(engine::default_output(d.path.parent()?, &d.effective_id()?))
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Title(t) => self.title = t,
            Message::AddDiscs => {
                let d = rfd::AsyncFileDialog::new().set_title("Add PS1 disc image(s)").add_filter("Disc image", DISC_EXT);
                return Task::perform(async move { d.pick_files().await }, |files| {
                    Message::DiscsPicked(files.unwrap_or_default().iter().map(|h| h.path().to_path_buf()).collect())
                });
            }
            Message::DiscsPicked(paths) => return self.add_discs(paths),
            Message::DiscProbed(key, r) => {
                let first = self.discs.first().is_some_and(|d| d.key == key);
                let Some(d) = self.disc_mut(key) else { return Task::none() };
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
                        let volume = s.volume_id.clone();
                        d.summary = Some(s);
                        d.error = None;
                        self.log(line);
                        if first && self.title.trim().is_empty() {
                            self.title = volume.unwrap_or_default();
                        }
                    }
                    Err(e) => {
                        d.error = Some(e.clone());
                        let name = file_name(&d.path);
                        return self.toast(Kind::Error, format!("{name}: {e}"));
                    }
                }
            }
            Message::RemoveDisc(key) => self.discs.retain(|d| d.key != key),
            Message::MoveDiscUp(key) => {
                if let Some(i) = self.discs.iter().position(|d| d.key == key).filter(|&i| i > 0) {
                    self.discs.swap(i - 1, i);
                }
            }
            Message::DiscId(key, id) => {
                if let Some(d) = self.disc_mut(key) {
                    d.id = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(12).collect::<String>().to_ascii_uppercase();
                }
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
            Message::PickDonor => {
                return Task::perform(pick_file("Donor PS1 Classic EBOOT.PBP", "EBOOT", &["pbp"]), Message::DonorPicked);
            }
            Message::DonorPicked(Some(p)) => return self.load_donor(p),
            Message::DonorPicked(None) => {}
            Message::DonorLoaded(path, r) => match r {
                Ok(boot) => {
                    self.donor_boot = Some(boot);
                    self.log(format!("Donor: {}", path.display()));
                    self.donor = Some(path);
                }
                Err(e) => return self.toast(Kind::Error, format!("Donor rejected: {e}")),
            },
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
            Message::PickIcon1 => return Task::perform(pick_file("ICON1.PMF", "PSP movie", &["pmf"]), Message::Icon1Picked),
            Message::Icon1Picked(p) => self.icon1 = p.or(self.icon1.take()),
            Message::ClearIcon1 => self.icon1 = None,
            Message::PickSnd0 => return Task::perform(pick_file("SND0 source audio", "Audio / video", MEDIA_EXT), Message::Snd0Picked),
            Message::Snd0Picked(p) => self.snd0 = p.or(self.snd0.take()),
            Message::ClearSnd0 => self.snd0 = None,
            Message::Snd0Start(s) => self.snd0_start = s,
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
            Message::Screen(s) => self.screen = s,
            Message::Convert => return self.convert(),
            Message::Cancel => {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Relaxed);
                    self.log("Cancelling…");
                }
            }
            Message::Job(ev) => match ev {
                JobEvent::Progress(p) => {
                    if let Some(job) = &mut self.job {
                        job.progress = p;
                    }
                }
                JobEvent::Log(l) => self.log(l),
                JobEvent::Done(r) => {
                    let label = self.job.take().map_or("Job", |j| j.label);
                    return match r {
                        Ok(msg) => self.toast(Kind::Success, format!("{label} complete — {msg}")),
                        Err(e) => self.toast(Kind::Error, format!("{label} failed: {e}")),
                    };
                }
            },
            Message::ImportPbp => {
                return Task::perform(pick_file("Import EBOOT.PBP (title, ID, artwork)", "EBOOT", &["pbp"]), Message::ImportPicked);
            }
            Message::ImportPicked(Some(p)) => return worker::blocking(move || import(p), Message::Imported),
            Message::ImportPicked(None) => {}
            Message::Imported(r) => match r {
                Ok(imp) => {
                    if let Some(t) = imp.title {
                        self.title = t;
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
                    let mut tasks = vec![self.toast(Kind::Info, msg)];
                    if self.donor.is_none() && imp.discs > 0 {
                        // A working PS1 Classic EBOOT is itself a valid donor.
                        tasks.push(self.load_donor(imp.path));
                    }
                    return Task::batch(tasks);
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
                    "ppf" => match self.discs.last().map(|d| d.key) {
                        Some(key) => Task::done(Message::PpfPicked(key, Some(p))),
                        None => self.toast(Kind::Error, "Add a disc before dropping a PPF patch"),
                    },
                    e if DISC_EXT.contains(&e) => self.add_discs(vec![p]),
                    e if IMAGE_EXT.contains(&e) => match art_slot_for(&p) {
                        Some(slot) => Task::done(Message::ArtPicked(slot, Some(p))),
                        None => self.toast(Kind::Error, format!("{}: name it ICON0 / PIC0 / PIC1 / BOOT, or use Browse", file_name(&p))),
                    },
                    _ => self.toast(Kind::Error, format!("{}: drop a .cue/.bin disc, a .ppf, an EBOOT.PBP or artwork", file_name(&p))),
                };
            }
            Message::ToggleLog => self.log_open = !self.log_open,
            Message::Dismiss(id) => self.toasts.retain(|t| t.id != id),
            Message::ToolsDetected(r) => {
                if let Ok(t) = r {
                    self.log(format!(
                        "Tools: ffmpeg {} · ATRAC3 encoder {}",
                        t.ffmpeg.as_deref().map_or("not found".into(), |p| p.display().to_string()),
                        t.at3.as_deref().map_or("not found".into(), |p| p.display().to_string())
                    ));
                    self.tools = Some(t);
                }
            }
        }
        Task::none()
    }

    fn load_donor(&mut self, p: PathBuf) -> Task<Message> {
        let path = p.clone();
        worker::blocking(
            move || {
                let d = Donor::load(&p).map_err(|e| e.to_string())?;
                let si = images::prepare(Slot::Boot, &d.boot_png).map_err(|e| format!("boot image: {e}"))?;
                Ok(Pixels::new(si.width, si.height, si.rgba))
            },
            move |r| Message::DonorLoaded(path.clone(), r),
        )
    }

    fn convert(&mut self) -> Task<Message> {
        if self.job.is_some() {
            return self.toast(Kind::Error, "A job is already running");
        }
        let problem = if self.discs.is_empty() {
            Some("Add at least one disc image (.cue or .bin)".to_owned())
        } else if let Some(d) = self.discs.iter().find(|d| d.effective_id().is_none()) {
            Some(format!("{}: no game ID detected — type it (e.g. SLUS00594)", file_name(&d.path)))
        } else if self.donor.is_none() {
            Some("Pick a donor PS1 Classic EBOOT.PBP (provides DATA.PSP and the boot trailer)".to_owned())
        } else if self.title.trim().is_empty() {
            Some("Enter a title".to_owned())
        } else if self.snd0.is_some() && self.tools.as_ref().is_some_and(|t| t.ffmpeg.is_none() || t.at3.is_none()) {
            Some("SND0 needs ffmpeg and at3tool/atracdenc next to the app or on PATH".to_owned())
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
                .map(|d| DiscInput { path: d.path.clone(), disc_id: Some(d.id.clone()).filter(|i| !i.is_empty()), ppf: d.ppf.clone() })
                .collect(),
            title: self.title.trim().to_owned(),
            output,
            donor: self.donor.clone().unwrap_or_default(),
            artwork: Artwork { icon0: png(Slot::Icon0), pic0: png(Slot::Pic0), pic1: png(Slot::Pic1), boot: png(Slot::Boot), icon1_pmf: None, snd0: None },
            level: self.level,
        };
        let (icon1, snd0, start) = (self.icon1.clone(), self.snd0.clone(), self.snd0_start as f64);
        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(Job { label: "Build", progress: 0.0, cancel: cancel.clone() });
        self.log_open = true;
        worker::job(cancel, move |m| build(req, icon1, snd0, start, m), Message::Job)
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

/// Worker side of "Build": media → EBOOT.PBP.
fn build(mut req: ConvertRequest, icon1: Option<PathBuf>, snd0: Option<PathBuf>, start: f64, m: &dyn psx2psp_engine::core::Monitor) -> Result<String, String> {
    if let Some(p) = &icon1 {
        let pmf = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
        m.log(&format!("ICON1.PMF: {} ({} B, passthrough)", p.display(), pmf.len()));
        req.artwork.icon1_pmf = Some(pmf);
    }
    if let Some(src) = &snd0 {
        let ff = Ffmpeg::detect().ok_or("ffmpeg not found")?;
        let enc = At3Encoder::detect().ok_or("ATRAC3 encoder (at3tool/atracdenc) not found")?;
        let used = req.artwork.icon1_pmf.as_ref().map_or(0, |v| v.len() as u64);
        let work = engine::work_dir(&req.output);
        std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
        m.log(&format!("SND0.AT3: encoding {} from {start:.1}s with {}", src.display(), enc.path().display()));
        let r = tools::make_snd0(&ff, &enc, &Snd0Request { input: src, start_secs: start, kbps: 132, budget: tools::SND0_BUDGET.saturating_sub(used) }, &work);
        let _ = std::fs::remove_dir_all(&work);
        let at3 = r.map_err(|e| e.to_string())?;
        m.log(&format!("SND0.AT3: {} B", at3.len()));
        req.artwork.snd0 = Some(at3);
    }
    let rep = engine::convert(&req, m).map_err(|e| e.to_string())?;
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
            discs = discs.push(
                container(
                    column![
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
                            ppf,
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    ]
                    .spacing(8),
                )
                .padding(10)
                .style(style::well),
            );
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
        let hint = if self.art[slot_ix(Slot::Boot)].is_none() && self.donor_boot.is_some() { "BOOT falls back to the donor's image" } else { "Any image format; auto-fitted to the PSP slot" };
        card("Artwork", style::RED, column![col, label(hint)].spacing(10))
    }

    fn media_card(&self) -> Element<'_, Message> {
        let status = match &self.tools {
            None => label("Detecting ffmpeg / ATRAC3 encoder…"),
            Some(t) => {
                let ok = t.ffmpeg.is_some() && t.at3.is_some();
                text(format!(
                    "ffmpeg {} · ATRAC3 encoder {}",
                    if t.ffmpeg.is_some() { "✓" } else { "✗" },
                    if t.at3.is_some() { "✓" } else { "✗ (at3tool / atracdenc)" }
                ))
                .size(12)
                .color(if ok { style::GREEN } else { style::AMBER })
            }
        };
        card(
            "Media",
            style::PINK,
            column![
                label("ICON1.PMF — animated icon (ready-made PMF)"),
                file_row(self.icon1.as_deref().map_or("none".into(), file_name), Message::PickIcon1, self.icon1.as_ref().map(|_| Message::ClearIcon1)),
                label("SND0.AT3 — XMB music from any audio / video"),
                file_row(self.snd0.as_deref().map_or("none".into(), file_name), Message::PickSnd0, self.snd0.as_ref().map(|_| Message::ClearSnd0)),
                row![label(format!("Start at {:.0}s", self.snd0_start)).width(110), slider(0.0..=600.0, self.snd0_start, Message::Snd0Start).step(1.0)]
                    .spacing(10)
                    .align_y(Alignment::Center),
                status,
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
                label("Donor PS1 Classic EBOOT (DATA.PSP + boot trailer)"),
                file_row(self.donor.as_deref().map_or("required".into(), file_name), Message::PickDonor, None),
                label("Output"),
                file_row(output, Message::PickOutput, self.output.as_ref().map(|_| Message::ResetOutput)),
                row![
                    label(format!("Compression {}", self.level)).width(110),
                    slider(1..=9, self.level, Message::Level),
                    label(match self.level {
                        1..=3 => "fast",
                        4..=7 => "balanced",
                        _ => "smallest",
                    })
                    .width(60),
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
        let empty = self.discs.is_empty() && layers.iter().all(Option::is_none);
        let preview = responsive(move |size| {
            let k = (size.width / 480.0).min(size.height / 272.0).max(0.1);
            let (w, h) = (480.0 * k, 272.0 * k);
            let at = |x: f32, y: f32, e: Element<'static, Message>| -> Element<'static, Message> { pin(e).x(x * k).y(y * k).into() };
            let mut layers_ui: Vec<Element<'static, Message>> =
                vec![shader(Xmb { screen, layers: layers.clone(), accent: WAVE }).width(w).height(h).into()];
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
                text(format!("{} {:>5.1}%", job.label, job.progress * 100.0)).size(13).width(120),
                progress_bar(0.0..=1.0, job.progress).girth(10),
                button(text("Cancel").size(13)).padding([8, 18]).style(style::solid(style::RED)).on_press(Message::Cancel),
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
}
