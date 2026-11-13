//! Built-in updater: a quiet background check at start (at most once a day), a compact banner when
//! a release is newer, and the manual "check" chip in the header. The network, checksum and
//! self-replace logic lives in [`psx2psp_engine::update`] (shared with `psx2psp update`).
//!
//! Automatic checks never report failures (offline, no release yet, rate limit…); a check the user
//! asked for always answers in the banner.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iced::widget::{button, column, container, progress_bar, row, space, text, tooltip};
use iced::{Alignment, Element, Fill, Task};
use psx2psp_engine::patchdb;
use psx2psp_engine::update::{self, CURRENT, Check, Program, Release, UpdateError, Version};

use crate::style;
use crate::worker::{self, JobEvent};

/// Minimum time between automatic checks.
const INTERVAL_SECS: u64 = 24 * 3600;
/// Next to the user's patches.ini (portable beside the exe, else the per-user config folder).
const PREFS_FILE: &str = "updater.cfg";
/// How long the new process waits for the old one to release `<exe>.old`.
const CLEANUP_WAIT: Duration = Duration::from_secs(10);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ───────────────────────────────── prefs ─────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct Prefs {
    auto: bool,
    /// Unix seconds of the last automatic check.
    last_check: u64,
    /// A version the user dismissed with "Later": not offered again by automatic checks.
    skip: Option<String>,
    /// Where to save; `None` keeps the prefs in memory (tests).
    path: Option<PathBuf>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { auto: true, last_check: 0, skip: None, path: None }
    }
}

impl Prefs {
    fn load() -> Self {
        let path = patchdb::user_path().and_then(|p| p.parent().map(|d| d.join(PREFS_FILE)));
        let text = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Self { path, ..Self::parse(&text) }
    }

    fn parse(text: &str) -> Self {
        let mut p = Self::default();
        for (k, v) in text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v.trim())) {
            match k {
                "auto_check" => p.auto = v != "false",
                "last_check" => p.last_check = v.parse().unwrap_or(0),
                "skip_version" if !v.is_empty() => p.skip = Some(v.to_owned()),
                _ => {}
            }
        }
        p
    }

    fn render(&self) -> String {
        format!(
            "# PSX2PSP-Reborn updater\nauto_check={}\nlast_check={}\nskip_version={}\n",
            self.auto,
            self.last_check,
            self.skip.as_deref().unwrap_or("")
        )
    }

    /// Best effort: a read-only config folder only means the next start checks again.
    fn save(&self) {
        if let Some(p) = &self.path {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(p, self.render());
        }
    }

    /// An automatic check is allowed (a clock that went backwards counts as due).
    fn due(&self, now: u64) -> bool {
        self.auto && (now < self.last_check || now - self.last_check >= INTERVAL_SECS)
    }
}

// ───────────────────────────────── state ─────────────────────────────────

#[derive(Debug)]
enum State {
    Idle,
    /// A check the user asked for.
    Checking,
    /// Answer to a manual check, or a failure.
    Notice { text: String, error: bool },
    Available(Box<Release>),
    Downloading { release: Box<Release>, progress: f32 },
    /// Installed; the restart waits for the user while a build is running.
    Ready { version: Version, exe: PathBuf },
}

pub struct Updater {
    prefs: Prefs,
    state: State,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Checked { manual: bool, result: Result<Check, UpdateError> },
    CheckNow,
    Install,
    WhatsNew,
    /// Skip this version.
    Later,
    Dismiss,
    ToggleAuto,
    Cancel,
    Download(JobEvent<PathBuf>),
    Restart,
}

fn check_task(manual: bool) -> Task<Msg> {
    worker::blocking(
        || Ok(update::check(Program::Gui)),
        move |r| Msg::Checked { manual, result: r.unwrap_or_else(|e| Err(UpdateError::Io(e))) },
    )
}

/// Opens `url` in the default browser through the OS opener.
fn open_url(url: &str) {
    let (program, pre): (&str, &[&str]) = if cfg!(windows) {
        ("rundll32.exe", &["url.dll,FileProtocolHandler"])
    } else if cfg!(target_os = "macos") {
        ("open", &[])
    } else {
        ("xdg-open", &[])
    };
    let _ = std::process::Command::new(program).args(pre).arg(url).spawn();
}

impl Updater {
    /// Loads the prefs; the task removes what a previous update left behind and, when due, runs
    /// the automatic check.
    pub fn new() -> (Self, Task<Msg>) {
        let prefs = Prefs::load();
        let cleanup = worker::blocking(
            || {
                update::cleanup_leftovers(Program::Gui, CLEANUP_WAIT);
                Ok(())
            },
            |_| (),
        )
        .discard();
        let check = if prefs.due(now()) { check_task(false) } else { Task::none() };
        (Self { prefs, state: State::Idle, cancel: Arc::default() }, Task::batch([cleanup, check]))
    }

    /// `busy`: a build is running, so a finished update waits for "Restart now".
    pub fn update(&mut self, msg: Msg, busy: bool) -> Task<Msg> {
        match msg {
            Msg::Checked { manual, result } => {
                if !manual {
                    self.prefs.last_check = now();
                    self.prefs.save();
                }
                // Never replace a download, an installed update or a manual check on screen.
                let free = match self.state {
                    State::Idle => true,
                    State::Checking | State::Notice { .. } | State::Available(_) => manual,
                    State::Downloading { .. } | State::Ready { .. } => false,
                };
                if free {
                    self.state = self.answer(manual, result);
                }
            }
            Msg::CheckNow => {
                if matches!(self.state, State::Checking | State::Downloading { .. } | State::Ready { .. }) {
                    return Task::none();
                }
                self.state = State::Checking;
                return check_task(true);
            }
            Msg::Install => {
                let State::Available(release) = &self.state else { return Task::none() };
                let release = release.clone();
                self.cancel = Arc::default();
                self.state = State::Downloading { release: release.clone(), progress: 0.0 };
                return worker::job(
                    self.cancel.clone(),
                    move |mon| {
                        let exe = std::env::current_exe().map_err(|e| format!("can't locate the program: {e}"))?;
                        let dir = exe.parent().ok_or("the program has no folder")?;
                        let file = update::download(&release, dir, mon).map_err(|e| e.to_string())?;
                        update::install(&file, &exe).map_err(|e| e.to_string())?;
                        Ok(exe)
                    },
                    Msg::Download,
                );
            }
            Msg::Download(JobEvent::Progress(p)) => {
                if let State::Downloading { progress, .. } = &mut self.state {
                    *progress = p;
                }
            }
            Msg::Download(JobEvent::Log(_)) => {}
            Msg::Download(JobEvent::Done(r)) => {
                let State::Downloading { release, .. } = std::mem::replace(&mut self.state, State::Idle) else { return Task::none() };
                match r {
                    Ok(exe) => {
                        self.state = State::Ready { version: release.version, exe };
                        if !busy {
                            return self.update(Msg::Restart, busy);
                        }
                    }
                    // Stopped by the user: offer it again.
                    Err(e) if e == UpdateError::Cancelled.to_string() => self.state = State::Available(release),
                    Err(e) => self.state = State::Notice { text: format!("Update failed: {e}"), error: true },
                }
            }
            Msg::Restart => {
                let State::Ready { version, exe } = &self.state else { return Task::none() };
                match update::relaunch(exe) {
                    Ok(()) => return iced::exit(),
                    Err(e) => {
                        let text = format!("v{version} is installed — reopen PSX2PSP-Reborn to use it ({e})");
                        self.state = State::Notice { text, error: false };
                    }
                }
            }
            Msg::WhatsNew => {
                if let State::Available(r) | State::Downloading { release: r, .. } = &self.state {
                    open_url(&r.page);
                }
            }
            Msg::Later => {
                if let State::Available(r) = &self.state {
                    self.prefs.skip = Some(r.version.to_string());
                    self.prefs.save();
                }
                self.state = State::Idle;
            }
            Msg::Dismiss => {
                if matches!(self.state, State::Checking | State::Notice { .. }) {
                    self.state = State::Idle;
                }
            }
            Msg::ToggleAuto => {
                self.prefs.auto = !self.prefs.auto;
                self.prefs.save();
            }
            Msg::Cancel => self.cancel.store(true, Ordering::Relaxed),
        }
        Task::none()
    }

    /// What a finished check shows: automatic checks stay silent unless a new, not skipped,
    /// version exists.
    fn answer(&self, manual: bool, result: Result<Check, UpdateError>) -> State {
        let notice = |text: String, error| State::Notice { text, error };
        match result {
            Ok(Check::Available(r)) if manual || self.prefs.skip.as_deref() != Some(r.version.to_string().as_str()) => State::Available(r),
            _ if !manual => State::Idle,
            Ok(Check::Available(_)) | Ok(Check::UpToDate(_)) => notice(format!("You're on the latest version (v{CURRENT})"), false),
            Err(UpdateError::NoRelease) => notice(format!("No release has been published yet — you're on v{CURRENT}"), false),
            Err(e) => notice(format!("Update check failed: {e}"), true),
        }
    }

    // ─────────────────────────────── view ───────────────────────────────

    /// The banner for the top of the left column; `None` when there is nothing to say.
    pub fn banner(&self) -> Option<Element<'_, Msg>> {
        let small = |s: &'static str| button(text(s).size(12)).padding([5, 12]).style(style::ghost);
        let (body, accent): (Element<'_, Msg>, _) = match &self.state {
            State::Idle => return None,
            State::Checking => (text("Checking for updates…").size(13).color(style::MUTED).into(), style::BLUE),
            State::Notice { text: t, error } => (
                row![
                    text(t.as_str()).size(13).width(Fill),
                    small(if self.prefs.auto { "Auto-check: on" } else { "Auto-check: off" }).on_press(Msg::ToggleAuto),
                    small("✕").on_press(Msg::Dismiss),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
                if *error { style::RED } else { style::BLUE },
            ),
            State::Available(r) => (
                column![
                    column![
                        text(format!("PSX2PSP-Reborn v{} is available", r.version)).size(14),
                        text(format!("You have v{CURRENT}")).size(12).color(style::MUTED),
                    ]
                    .spacing(2),
                    row![
                        button(text("Update now").size(13)).padding([6, 14]).style(style::solid(style::BLUE)).on_press(Msg::Install),
                        small("What's new").on_press(Msg::WhatsNew),
                        space().width(Fill),
                        small("Later").on_press(Msg::Later),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                ]
                .spacing(10)
                .into(),
                style::BLUE,
            ),
            State::Downloading { release, progress } => {
                let stopping = self.cancel.load(Ordering::Relaxed);
                (
                    column![
                        row![
                            text(format!("Downloading v{}…", release.version)).size(13).width(Fill),
                            text(format!("{:.0}%", progress * 100.0)).size(12).color(style::MUTED),
                        ],
                        progress_bar(0.0..=1.0, *progress).girth(6),
                        row![space().width(Fill), small(if stopping { "Stopping…" } else { "Cancel" }).on_press_maybe((!stopping).then_some(Msg::Cancel))],
                    ]
                    .spacing(8)
                    .into(),
                    style::BLUE,
                )
            }
            State::Ready { version, .. } => (
                row![
                    text(format!("v{version} is installed — restart to finish")).size(13).width(Fill),
                    button(text("Restart now").size(13)).padding([6, 14]).style(style::solid(style::GREEN)).on_press(Msg::Restart),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
                style::GREEN,
            ),
        };
        Some(container(body).padding(12).width(Fill).style(style::toast(accent)).into())
    }

    /// Version chip for the brand header; clicking it checks for updates.
    pub fn version_button(&self) -> Element<'_, Msg> {
        let idle = !matches!(self.state, State::Checking | State::Downloading { .. } | State::Ready { .. });
        let label = text(format!("v{CURRENT}")).size(11).font(style::BOLD).wrapping(text::Wrapping::None);
        let chip = button(label).padding([2, 9]).style(style::chip_button(style::BLUE)).on_press_maybe(idle.then_some(Msg::CheckNow));
        let bubble = container(text("Check for updates").size(12)).padding([6, 10]).style(style::toast(style::BLUE));
        tooltip(chip, bubble, tooltip::Position::Bottom).gap(6).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx2psp_engine::update::Asset;

    fn updater() -> Updater {
        Updater { prefs: Prefs::default(), state: State::Idle, cancel: Arc::default() }
    }

    fn release(v: &str) -> Box<Release> {
        let asset = Asset { name: "a".into(), url: "https://example.invalid/a".into(), size: 1 };
        Box::new(Release { version: Version::parse(v).unwrap(), tag: format!("v{v}"), page: String::new(), notes: String::new(), asset, sums: None })
    }

    #[test]
    fn prefs_round_trip_and_schedule() {
        let p = Prefs { auto: false, last_check: 1_700_000_000, skip: Some("0.2.0".into()), path: None };
        assert_eq!(Prefs::parse(&p.render()), p);
        assert_eq!(Prefs::parse("garbage\nlast_check=x\n"), Prefs::default());

        let p = Prefs { last_check: 1_000_000, ..Prefs::default() };
        assert!(!p.due(1_000_000 + INTERVAL_SECS - 1));
        assert!(p.due(1_000_000 + INTERVAL_SECS));
        assert!(p.due(10), "clock moved backwards");
        assert!(!Prefs { auto: false, ..p }.due(u64::MAX));
    }

    #[test]
    fn automatic_checks_are_silent_except_for_news() {
        let mut u = updater();
        for e in [UpdateError::NoRelease, UpdateError::RateLimited, UpdateError::Offline("dns".into()), UpdateError::Http(500)] {
            let _ = u.update(Msg::Checked { manual: false, result: Err(e) }, false);
            assert!(matches!(u.state, State::Idle) && u.banner().is_none());
        }
        let _ = u.update(Msg::Checked { manual: false, result: Ok(Check::UpToDate(Version::current())) }, false);
        assert!(matches!(u.state, State::Idle));
        assert_ne!(u.prefs.last_check, 0, "the attempt is recorded");

        let _ = u.update(Msg::Checked { manual: false, result: Ok(Check::Available(release("9.0.0"))) }, false);
        assert!(matches!(u.state, State::Available(_)) && u.banner().is_some());
        let _ = u.update(Msg::Later, false);
        assert_eq!(u.prefs.skip.as_deref(), Some("9.0.0"));
        assert!(matches!(u.state, State::Idle));

        // The skipped version stays hidden from automatic checks, a newer one shows, a manual check shows it anyway.
        let _ = u.update(Msg::Checked { manual: false, result: Ok(Check::Available(release("9.0.0"))) }, false);
        assert!(matches!(u.state, State::Idle));
        let _ = u.update(Msg::Checked { manual: true, result: Ok(Check::Available(release("9.0.0"))) }, false);
        assert!(matches!(u.state, State::Available(_)));
        let _ = u.update(Msg::Later, false);
        let _ = u.update(Msg::Checked { manual: false, result: Ok(Check::Available(release("9.0.1"))) }, false);
        assert!(matches!(&u.state, State::Available(r) if r.version.to_string() == "9.0.1"));
    }

    #[test]
    fn manual_checks_always_answer() {
        let mut u = updater();
        let cases = [
            (Ok(Check::UpToDate(Version::current())), false),
            (Err(UpdateError::NoRelease), false),
            (Err(UpdateError::RateLimited), true),
            (Err(UpdateError::Offline("dns".into())), true),
        ];
        for (result, error) in cases {
            let _ = u.update(Msg::Checked { manual: true, result }, false);
            assert!(matches!(u.state, State::Notice { error: e, .. } if e == error), "{:?}", u.state);
            assert!(u.banner().is_some());
        }
        assert_eq!(u.prefs.last_check, 0, "manual checks don't move the schedule");
        let _ = u.update(Msg::Dismiss, false);
        assert!(matches!(u.state, State::Idle));
    }

    #[test]
    fn download_outcomes() {
        let mut u = updater();
        u.state = State::Downloading { release: release("9.0.0"), progress: 0.0 };
        let _ = u.update(Msg::Download(JobEvent::Progress(0.5)), false);
        assert!(matches!(u.state, State::Downloading { progress, .. } if progress == 0.5));
        // A late automatic answer doesn't hide the download.
        let _ = u.update(Msg::Checked { manual: false, result: Err(UpdateError::NoRelease) }, false);
        assert!(matches!(u.state, State::Downloading { .. }));

        let _ = u.update(Msg::Download(JobEvent::Done(Err(UpdateError::Cancelled.to_string()))), false);
        assert!(matches!(u.state, State::Available(_)), "cancel offers the update again");

        u.state = State::Downloading { release: release("9.0.0"), progress: 0.9 };
        let _ = u.update(Msg::Download(JobEvent::Done(Err("SHA-256 mismatch".into()))), false);
        assert!(matches!(&u.state, State::Notice { error: true, text } if text.contains("mismatch")));

        // Installed during a build: the restart waits for the user.
        u.state = State::Downloading { release: release("9.0.0"), progress: 1.0 };
        let _ = u.update(Msg::Download(JobEvent::Done(Ok(PathBuf::from("x")))), true);
        assert!(matches!(u.state, State::Ready { .. }));
    }
}
