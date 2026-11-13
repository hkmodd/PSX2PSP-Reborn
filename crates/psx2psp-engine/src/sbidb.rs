//! Built-in LibCrypt subchannel data: product code → the redump `.sbi` of that disc.
//!
//! Source: the redump.org SBI files (<http://redump.org>) as collected by psxdatacenter.com
//! (<https://psxdatacenter.com/sbifiles.html>), packed by `tools/sbi_import.py` into
//! `resources/libcrypt_sbi.tsv.z` (~8 KB): one line per dump, serial, title, region, SBI bytes in
//! hex. Used when no `.sbi`/`.lsd` lies beside the disc: it is the same file redump publishes, so
//! the EBOOT gets the table of the real disc instead of the magic-word model. Decompressed and
//! parsed once, on first lookup.

use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

use psx2psp_core::subq::{self, Entries};

use crate::patchdb::normalise_id;

static RAW: &[u8] = include_bytes!("../../../resources/libcrypt_sbi.tsv.z");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sbi {
    /// `SLES-01226` form.
    pub serial: String,
    pub title: String,
    pub region: String,
    /// The parsed file, for [`subq::table`].
    pub entries: Entries,
}

fn db() -> &'static HashMap<String, Vec<Sbi>> {
    static DB: OnceLock<HashMap<String, Vec<Sbi>>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut text = String::new();
        flate2::read::ZlibDecoder::new(RAW).read_to_string(&mut text).expect("embedded SBI set is valid zlib UTF-8");
        let mut db: HashMap<String, Vec<Sbi>> = HashMap::new();
        for l in text.lines().filter(|l| !l.starts_with('#')) {
            let [serial, title, region, hex] = l.split('\t').collect::<Vec<_>>()[..] else { panic!("embedded SBI set: bad line {l}") };
            let entries = subq::parse(&unhex(hex)).unwrap_or_else(|e| panic!("embedded SBI of {serial}: {e}"));
            let sbi = Sbi { serial: serial.to_owned(), title: title.to_owned(), region: region.to_owned(), entries };
            db.entry(normalise_id(serial)).or_default().push(sbi);
        }
        db
    })
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("embedded SBI set: hex bytes")).collect()
}

/// The dumps of `id` (`SLES-01226`, `SLES_012.26`, `sles01226`…): none, one, or several when
/// versions of a disc share the product code but not the damaged sectors (Spyro: Year of the
/// Dragon SCES-02835 v1.0 / v1.1). The set has no way to tell which dump a disc is, so a caller
/// uses it only when there is exactly one.
pub fn lookup(id: &str) -> &'static [Sbi] {
    db().get(&normalise_id(id)).map_or(&[], Vec::as_slice)
}

/// Number of serials in the set.
pub fn len() -> usize {
    db().len()
}

/// `title` of each dump, `; `-joined: names the versions when a serial has several.
pub fn titles(dumps: &[Sbi]) -> String {
    dumps.iter().map(|s| s.title.as_str()).collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx2psp_core::{Track, TrackKind};

    #[test]
    fn set_loads_and_known_serials_resolve() {
        assert_eq!(len(), 196);
        assert_eq!(db().values().map(Vec::len).sum::<usize>(), 197);
        for s in db().values().flatten() {
            assert!(matches!(s.entries.len(), 16 | 32), "{}: {} sectors", s.serial, s.entries.len());
            assert!(s.entries.values().all(Option::is_none), "{}: an SBI only lists damaged sectors", s.serial);
        }
        let [hockey] = lookup("SLES_012.26") else { panic!("Actua Ice Hockey 2") };
        assert_eq!((hockey.serial.as_str(), hockey.title.as_str(), hockey.entries.len()), ("SLES-01226", "ACTUA ICE HOCKEY 2", 32));
        assert_eq!(lookup("sles-01226"), lookup("SLES01226"));
        assert_eq!(lookup("SCES-02835").len(), 2, "Spyro: Year of the Dragon v1.0 and v1.1");
        assert!(titles(lookup("SCES02835")).contains("V.1.1"));
        // CTR's EDC and no-EDC dumps carry the same SBI: one entry.
        assert_eq!(lookup("SCES-02105").len(), 1);
        assert!(lookup("SLES02895").is_empty());
        // The page lists Premier Manager 2000 (E) as SLES-02992, Silent Bomber's code.
        assert_eq!(lookup("SLES-02292")[0].title, "PREMIER MANAGER 2000");
        assert!(lookup("SLES-02992").is_empty());
    }

    /// Two independent sources: the bits each SBI damages are pop-fe's magic word for the code,
    /// for every dump pop-fe knows except Spyro: Year of the Dragon v1.1 (pop-fe has v1.0's).
    #[test]
    fn sbi_bits_are_the_pop_fe_magic_words() {
        use psx2psp_core::psar::{SUBQ_ENTRY, libcrypt_subchannels};
        // Pair sectors in bit order (15 first), read back from the all-bits magic-word table.
        let all = libcrypt_subchannels(0xFFFF);
        let pairs: Vec<u32> = all[SUBQ_ENTRY..all.len() - SUBQ_ENTRY].chunks(SUBQ_ENTRY).map(|e| u32::from_le_bytes(e[..4].try_into().unwrap())).collect();
        let patches = crate::patchdb::PatchDb::builtin();
        let (mut checked, mut differ) = (0, Vec::new());
        for s in db().values().flatten() {
            let word = pairs.chunks(2).zip((0..16).rev()).filter(|(p, _)| p.iter().any(|x| s.entries.contains_key(x))).fold(0u16, |w, (_, bit)| w | 1 << bit);
            if let Some(magic) = patches.libcrypt(&s.serial) {
                checked += 1;
                if magic != word {
                    differ.push(format!("{} {}: {magic:#06x} vs {word:#06x}", s.serial, s.title));
                }
            }
        }
        assert_eq!(checked, 197);
        assert_eq!(differ, ["SCES-02835 SPYRO - YEAR OF THE DRAGON (V.1.1) (PLATINUM): 0x369a vs 0xdd11"]);
    }

    /// MediEvil (I) SCES-01494 damages one sector of each pair the magic-word model assumes: the
    /// built-in dump gives the same table as redump's `MediEvil (Italy).sbi` read from disk.
    #[test]
    fn medievil_matches_the_redump_file() {
        const REDUMP_SBI: &str = "\
            534249000308050141010107060500230805031310014101010313100003531003152401410101031320000315040320\
            5601410101031857000320d60323170141010103231700036317032503014101010323130003250b0333560141010113\
            3156000b3356033451014101010132510043345109204501410101091865000920410925570141010109235300092577\
            092871014101010826710089287109333701410101293137000d3337093714014101010935040009371c093858014101\
            01493658000b38580947290141010109452b000947690948590141010109445900090859";
        let file = unhex(REDUMP_SBI);
        let [m] = lookup("SCES-01494") else { panic!("MediEvil (I)") };
        assert_eq!(m.title, "MEDIEVIL");
        let own = subq::parse(&file).unwrap();
        assert_eq!(m.entries, own);
        let sectors = [13955, 14335, 14499, 14906, 15092, 15228, 15881, 15951, 41895, 42282, 42521, 42862, 43139, 43258, 43904, 44009];
        assert_eq!(own.keys().copied().collect::<Vec<u32>>(), sectors, "one sector per pair, not the magic-word model's two");
        let tracks = [Track { number: 1, kind: TrackKind::Mode2, start: 0, index0: None }];
        assert_eq!(subq::table(&m.entries, &tracks), subq::table(&own, &tracks));
    }
}
