//! PARAM.SFO — Sony's key/value metadata table.
//!
//! Layout (little endian):
//! ```text
//! 0x00 "\0PSF"   0x04 version (0x0101)   0x08 key_table_start   0x0C data_table_start   0x10 count
//! 0x14 index[count] { u16 key_off, u16 fmt, u32 len, u32 max_len, u32 data_off }   (16 bytes each)
//! key table: NUL-terminated ASCII keys, sorted, padded to 4 bytes
//! data table: each value occupies `max_len` bytes (4-aligned)
//! ```

use crate::error::{Error, Result};

pub const SFO_MAGIC: [u8; 4] = *b"\0PSF";
const HEADER_LEN: usize = 0x14;
const INDEX_LEN: usize = 0x10;

/// `utf8-s`: special mode string, not NUL-terminated.
pub const FMT_UTF8_S: u16 = 0x0004;
/// `utf8`: NUL-terminated string.
pub const FMT_UTF8: u16 = 0x0204;
/// `int32`: little endian u32.
pub const FMT_INT32: u16 = 0x0404;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SfoValue {
    /// NUL-terminated UTF-8 (fmt 0x0204).
    Str(String),
    /// Raw special-mode bytes (fmt 0x0004), kept verbatim for lossless round-trips.
    Raw(Vec<u8>),
    Int(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SfoEntry {
    pub key: String,
    pub value: SfoValue,
    /// Reserved bytes for the value in the data table (4-aligned).
    pub max_len: u32,
}

impl SfoEntry {
    fn fmt(&self) -> u16 {
        match self.value {
            SfoValue::Str(_) => FMT_UTF8,
            SfoValue::Raw(_) => FMT_UTF8_S,
            SfoValue::Int(_) => FMT_INT32,
        }
    }

    fn encoded(&self) -> Vec<u8> {
        match &self.value {
            SfoValue::Str(s) => {
                let mut v = s.as_bytes().to_vec();
                v.push(0);
                v
            }
            SfoValue::Raw(b) => b.clone(),
            SfoValue::Int(i) => i.to_le_bytes().to_vec(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sfo {
    /// Always sorted by key (the PSP firmware binary-searches the table).
    entries: Vec<SfoEntry>,
}

const fn align4(n: usize) -> usize {
    (n + 3) & !3
}

impl Sfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// The canonical PARAM.SFO of a PS1 Classic (POPS) EBOOT.
    ///
    /// `disc_id` is the 9-char product code without separators (e.g. `SLUS00594`).
    pub fn pops(title: &str, disc_id: &str) -> Self {
        let mut sfo = Self::new();
        sfo.set_int("BOOTABLE", 1);
        sfo.set_str("CATEGORY", "ME", 4);
        sfo.set_str("DISC_ID", disc_id, 16);
        sfo.set_str("DISC_VERSION", "1.00", 8);
        sfo.set_str("LICENSE", "Copyright(C) Sony Computer Entertainment America Inc.", 512);
        sfo.set_int("PARENTAL_LEVEL", 1);
        sfo.set_str("PSP_SYSTEM_VER", "3.01", 8);
        sfo.set_int("REGION", 0x8000);
        sfo.set_str("TITLE", title, 128);
        sfo
    }

    pub fn entries(&self) -> &[SfoEntry] {
        &self.entries
    }

    pub fn get(&self, key: &str) -> Option<&SfoValue> {
        self.find(key).ok().map(|i| &self.entries[i].value)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            SfoValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn get_int(&self, key: &str) -> Option<u32> {
        match self.get(key)? {
            SfoValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Inserts or replaces a string. The value is truncated (on a char boundary) so that it plus
    /// its NUL terminator fits in `max_len`.
    pub fn set_str(&mut self, key: &str, value: &str, max_len: u32) {
        let max_len = align4(max_len.max(4) as usize) as u32;
        let mut end = value.len().min(max_len as usize - 1);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.upsert(SfoEntry { key: key.into(), value: SfoValue::Str(value[..end].into()), max_len });
    }

    pub fn set_int(&mut self, key: &str, value: u32) {
        self.upsert(SfoEntry { key: key.into(), value: SfoValue::Int(value), max_len: 4 });
    }

    pub fn remove(&mut self, key: &str) -> Option<SfoEntry> {
        self.find(key).ok().map(|i| self.entries.remove(i))
    }

    fn find(&self, key: &str) -> Result<usize, usize> {
        self.entries.binary_search_by(|e| e.key.as_str().cmp(key))
    }

    fn upsert(&mut self, entry: SfoEntry) {
        match self.find(&entry.key) {
            Ok(i) => self.entries[i] = entry,
            Err(i) => self.entries.insert(i, entry),
        }
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        let rd32 = |o: usize| -> Result<u32> {
            buf.get(o..o + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .ok_or_else(|| Error::format("PARAM.SFO", format!("truncated at {o:#x}")))
        };
        let rd16 = |o: usize| -> Result<u16> {
            buf.get(o..o + 2)
                .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
                .ok_or_else(|| Error::format("PARAM.SFO", format!("truncated at {o:#x}")))
        };
        if buf.get(..4) != Some(&SFO_MAGIC[..]) {
            return Err(Error::format("PARAM.SFO", "bad magic"));
        }
        let key_start = rd32(0x08)? as usize;
        let data_start = rd32(0x0C)? as usize;
        let count = rd32(0x10)? as usize;
        if count > 4096 {
            return Err(Error::format("PARAM.SFO", format!("absurd entry count {count}")));
        }
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let base = HEADER_LEN + i * INDEX_LEN;
            let key_off = key_start + rd16(base)? as usize;
            let fmt = rd16(base + 2)?;
            let len = rd32(base + 4)? as usize;
            let max_len = rd32(base + 8)?;
            let data_off = data_start + rd32(base + 12)? as usize;

            let key_bytes = buf
                .get(key_off..)
                .and_then(|k| k.iter().position(|&c| c == 0).map(|n| &k[..n]))
                .ok_or_else(|| Error::format("PARAM.SFO", "unterminated key"))?;
            let key = String::from_utf8_lossy(key_bytes).into_owned();
            let data = buf
                .get(data_off..data_off + len)
                .ok_or_else(|| Error::format("PARAM.SFO", format!("value of {key} out of bounds")))?;
            let value = match fmt {
                FMT_INT32 => SfoValue::Int(rd32(data_off)?),
                FMT_UTF8 => {
                    let s = data.split(|&c| c == 0).next().unwrap_or_default();
                    SfoValue::Str(String::from_utf8_lossy(s).into_owned())
                }
                _ => SfoValue::Raw(data.to_vec()),
            };
            entries.push(SfoEntry { key, value, max_len });
        }
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(Self { entries })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let n = self.entries.len();
        let key_start = HEADER_LEN + n * INDEX_LEN;
        let key_table_len = align4(self.entries.iter().map(|e| e.key.len() + 1).sum());
        let data_start = key_start + key_table_len;
        let data_len: usize = self.entries.iter().map(|e| e.max_len as usize).sum();

        let mut out = vec![0u8; data_start + data_len];
        out[..4].copy_from_slice(&SFO_MAGIC);
        out[4..8].copy_from_slice(&0x0101u32.to_le_bytes());
        out[8..12].copy_from_slice(&(key_start as u32).to_le_bytes());
        out[12..16].copy_from_slice(&(data_start as u32).to_le_bytes());
        out[16..20].copy_from_slice(&(n as u32).to_le_bytes());

        let (mut key_off, mut data_off) = (0usize, 0usize);
        for (i, e) in self.entries.iter().enumerate() {
            let value = e.encoded();
            let len = value.len().min(e.max_len as usize);
            let idx = HEADER_LEN + i * INDEX_LEN;
            out[idx..idx + 2].copy_from_slice(&(key_off as u16).to_le_bytes());
            out[idx + 2..idx + 4].copy_from_slice(&e.fmt().to_le_bytes());
            out[idx + 4..idx + 8].copy_from_slice(&(len as u32).to_le_bytes());
            out[idx + 8..idx + 12].copy_from_slice(&e.max_len.to_le_bytes());
            out[idx + 12..idx + 16].copy_from_slice(&(data_off as u32).to_le_bytes());

            let k = key_start + key_off;
            out[k..k + e.key.len()].copy_from_slice(e.key.as_bytes());
            key_off += e.key.len() + 1;

            let d = data_start + data_off;
            out[d..d + len].copy_from_slice(&value[..len]);
            data_off += e.max_len as usize;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pops_roundtrip_is_lossless() {
        let sfo = Sfo::pops("Final Fantasy VII (Disc 1)", "SCUS94163");
        let bytes = sfo.to_bytes();
        assert_eq!(&bytes[..4], b"\0PSF");
        assert_eq!(bytes.len() % 4, 0);
        let back = Sfo::parse(&bytes).unwrap();
        assert_eq!(back, sfo);
        assert_eq!(back.get_str("CATEGORY"), Some("ME"));
        assert_eq!(back.get_int("REGION"), Some(0x8000));
        assert_eq!(back.to_bytes(), bytes, "re-serialisation must be bit-identical");
    }

    #[test]
    fn keys_are_sorted_and_tables_aligned() {
        let mut sfo = Sfo::new();
        sfo.set_str("TITLE", "x", 128);
        sfo.set_int("BOOTABLE", 1);
        sfo.set_str("CATEGORY", "ME", 4);
        let keys: Vec<_> = sfo.entries().iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["BOOTABLE", "CATEGORY", "TITLE"]);
        let b = sfo.to_bytes();
        let data_start = u32::from_le_bytes(b[12..16].try_into().unwrap());
        assert_eq!(data_start % 4, 0);
    }

    #[test]
    fn long_title_truncated_on_char_boundary() {
        let mut sfo = Sfo::new();
        let t = "é".repeat(100); // 200 bytes
        sfo.set_str("TITLE", &t, 128);
        let s = sfo.get_str("TITLE").unwrap();
        assert!(s.len() <= 127);
        assert!(s.chars().all(|c| c == 'é'));
    }

    #[test]
    fn hostile_input_never_panics() {
        for len in 0..64 {
            let mut b = vec![0xFFu8; len];
            if len >= 4 {
                b[..4].copy_from_slice(b"\0PSF");
            }
            let _ = Sfo::parse(&b);
        }
    }
}
