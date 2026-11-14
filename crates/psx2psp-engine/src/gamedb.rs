//! Built-in PS1 game database: product code → title, shared save ID and video standard.
//!
//! Source: the "Popstation GUI GameID Database" (SeattleMan, 2007) shipped as `gameInfo.db`
//! with PSX2PSP v1.4.2 — 10 109 serials. Converted to a zlib-compressed TSV
//! (`resources/gamedb.tsv.z`, ~130 KB) with the region tags (`[NTSC-U]`, `[PAL-M6]`…) stripped,
//! like PSX2PSP's default `hideGameRegion=1`. Decompressed once, on first lookup (~5 ms).

use std::collections::HashMap;
use std::sync::OnceLock;

static RAW: &[u8] = include_bytes!("../../../resources/gamedb.tsv.z");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameEntry {
    /// XMB title without region tag; multi-disc sets share one title.
    pub title: String,
    /// Save-data ID: disc 2+ of a set points at disc 1 so all discs share saves.
    pub save_id: String,
    pub pal: bool,
}

fn db() -> &'static HashMap<String, GameEntry> {
    static DB: OnceLock<HashMap<String, GameEntry>> = OnceLock::new();
    DB.get_or_init(|| {
        let text = String::from_utf8(crate::inflate(RAW)).expect("embedded game DB is UTF-8");
        text.lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                let id = f.next()?.to_owned();
                let save_id = f.next()?.to_owned();
                let title = f.next()?.to_owned();
                Some((id, GameEntry { title, save_id, pal: f.next()? == "P" }))
            })
            .collect()
    })
}

/// Looks up `SLES-02895`, `SLES_028.95`, `sles02895`… (separators and case ignored).
pub fn lookup(id: &str) -> Option<&'static GameEntry> {
    let key: String = id.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    db().get(&key)
}

/// Number of serials in the database.
pub fn len() -> usize {
    db().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_titles_and_shared_saves() {
        assert!(len() > 10_000);
        let sheep = lookup("SLES_028.95").unwrap();
        assert_eq!(sheep.title, "Sheep, Dog 'n' Wolf");
        assert!(sheep.pal);
        let d2 = lookup("slus-01377").unwrap();
        assert_eq!((d2.title.as_str(), d2.save_id.as_str(), d2.pal), ("Alone in the Dark - The New Nightmare", "SLUS01201", false));
        assert_eq!(lookup("SCES03015").unwrap().title, "Disney's Ein Königreich für ein Lama");
        assert!(lookup("ABCD99999").is_none());
    }
}
