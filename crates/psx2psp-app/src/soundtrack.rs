//! The app's own soundtrack, "Firmware Glitch": a quiet loop under the session, on by default at
//! a low volume, one click to silence. The choice is remembered in `music.cfg`, next to the
//! user's patches.ini (portable beside the exe, else the per-user config folder).

use std::path::PathBuf;

use psx2psp_engine::patchdb;

use crate::audio::Player;

pub const TRACK: &[u8] = include_bytes!("../../../resources/Firmware Glitch.mp3");
const PREFS_FILE: &str = "music.cfg";
/// Out of 100: about −21 dB, background level under any system volume.
pub const DEFAULT_VOLUME: u8 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Music {
    pub on: bool,
    /// 0–100, perceptual (see [`gain`]).
    pub volume: u8,
    /// Where to save; `None` keeps the choice in memory (tests).
    path: Option<PathBuf>,
}

impl Default for Music {
    fn default() -> Self {
        Self {
            on: true,
            volume: DEFAULT_VOLUME,
            path: None,
        }
    }
}

impl Music {
    /// Loads the saved choice and starts the soundtrack on `player`. Unit tests stay silent and
    /// never touch the user's files.
    pub fn start(player: &Player) -> Self {
        if cfg!(test) {
            return Self::default();
        }
        let path = patchdb::user_path().and_then(|p| p.parent().map(|d| d.join(PREFS_FILE)));
        let text = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let music = Self {
            path,
            ..Self::parse(&text)
        };
        music.apply(player);
        player.load_music(TRACK);
        music
    }

    pub fn apply(&self, player: &Player) {
        player.set_music(self.on, gain(self.volume));
    }

    fn parse(text: &str) -> Self {
        let mut m = Self::default();
        for (k, v) in text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
        {
            match k {
                "music" => m.on = v != "off",
                "volume" => {
                    m.volume = v
                        .parse::<u32>()
                        .map_or(DEFAULT_VOLUME, |v| v.min(100) as u8)
                }
                _ => {}
            }
        }
        m
    }

    fn render(&self) -> String {
        format!(
            "# PSX2PSP-Reborn soundtrack\nmusic={}\nvolume={}\n",
            if self.on { "on" } else { "off" },
            self.volume
        )
    }

    /// Best effort: an unwritable config folder only means the default next time.
    pub fn save(&self) {
        if let Some(p) = &self.path {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(p, self.render());
        }
    }
}

/// Slider position (0–100) to linear gain: squared, so the slider feels even to the ear.
pub fn gain(volume: u8) -> f32 {
    let v = f32::from(volume.min(100)) / 100.0;
    v * v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_roundtrip_and_defaults() {
        assert_eq!(Music::parse(""), Music::default());
        let m = Music {
            on: false,
            volume: 12,
            path: None,
        };
        assert_eq!(Music::parse(&m.render()), m);
        assert_eq!(Music::parse("volume=900").volume, 100);
        assert_eq!(Music::parse("volume=loud").volume, DEFAULT_VOLUME);
    }

    #[test]
    fn default_is_quiet() {
        let db = 20.0 * gain(DEFAULT_VOLUME).log10();
        assert!((-24.0..=-18.0).contains(&db), "{db} dB");
        assert_eq!(gain(0), 0.0);
        assert_eq!(gain(100), 1.0);
    }
}
