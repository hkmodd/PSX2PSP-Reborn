//! Raw CD images (BIN/CUE): track layout, a seekable concatenating reader, and ISO9660 game-ID probing.
//!
//! A disc is modelled as an ordered list of *segments* — byte ranges of BIN files or virtual
//! zero-filled gaps (CUE `PREGAP`, which is not stored in the BIN) — that together form the
//! raw 2352-byte/sector image POPS expects.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const RAW_SECTOR: u64 = 2352;
/// 2-second lead-in: absolute MSF time = LBA + 150.
pub const LEAD_IN_FRAMES: u32 = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Mode1,
    Mode2,
    Audio,
}

impl TrackKind {
    pub fn is_data(self) -> bool {
        !matches!(self, Self::Audio)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub number: u8,
    pub kind: TrackKind,
    /// Absolute LBA of INDEX 01 (start of track data proper).
    pub start: u32,
    /// Absolute LBA of INDEX 00 when present (start of the pregap stored in the image).
    pub index0: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    File { path: PathBuf, offset: u64, len: u64 },
    Zero { len: u64 },
}

impl Segment {
    fn len(&self) -> u64 {
        match self {
            Self::File { len, .. } | Self::Zero { len } => *len,
        }
    }
}

/// Region inferred from the product code prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    NtscU,
    NtscJ,
    Pal,
    Unknown,
}

impl Region {
    pub fn from_disc_id(id: &str) -> Self {
        match id.get(..4).map(str::to_ascii_uppercase).as_deref() {
            Some("SLUS" | "SCUS" | "PAPX" | "LSP0") => Self::NtscU,
            Some("SLES" | "SCES" | "SCED" | "SLED") => Self::Pal,
            Some("SLPS" | "SCPS" | "SLPM" | "SIPS" | "SCPM" | "PCPX" | "SLKA" | "SCKA") => Self::NtscJ,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NtscU => "NTSC-U",
            Self::NtscJ => "NTSC-J",
            Self::Pal => "PAL",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscImage {
    /// The file the user picked (CUE or BIN); used for display and relative lookups.
    pub source: PathBuf,
    pub tracks: Vec<Track>,
    segments: Vec<Segment>,
    sectors: u32,
}

/// Information probed from the ISO9660 filesystem of the first data track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscInfo {
    /// Normalised product code, e.g. `SLUS00594`.
    pub disc_id: Option<String>,
    /// PVD volume identifier, trimmed.
    pub volume_id: Option<String>,
}

fn parse_msf(s: &str) -> Result<u32> {
    let mut it = s.split(':').map(|p| p.trim().parse::<u32>());
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some(Ok(m)), Some(Ok(sec)), Some(Ok(f)), None) if sec < 60 && f < 75 => Ok((m * 60 + sec) * 75 + f),
        _ => Err(Error::format("CUE", format!("bad MSF timestamp `{s}`"))),
    }
}

/// Splits a CUE line into tokens honouring double quotes.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

impl DiscImage {
    /// Opens a `.cue` sheet or a raw `.bin`/`.img` (treated as one MODE2/2352 data track).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        match ext.as_str() {
            "cue" => Self::from_cue(path),
            "bin" | "img" | "raw" => Self::from_bin(path),
            other => Err(Error::Unsupported(format!(
                "disc image extension `.{other}` (expected .cue, .bin or .img with 2352-byte sectors)"
            ))),
        }
    }

    pub fn from_bin(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let len = std::fs::metadata(path).map_err(Error::io_path(path))?.len();
        if len == 0 || len % RAW_SECTOR != 0 {
            return Err(Error::format("BIN", format!("size {len} is not a multiple of 2352 (raw MODE2/2352 required)")));
        }
        let sectors = u32::try_from(len / RAW_SECTOR).map_err(|_| Error::format("BIN", "image too large"))?;
        Ok(Self {
            source: path.to_path_buf(),
            tracks: vec![Track { number: 1, kind: TrackKind::Mode2, start: 0, index0: None }],
            segments: vec![Segment::File { path: path.to_path_buf(), offset: 0, len }],
            sectors,
        })
    }

    pub fn from_cue(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read(path).map_err(Error::io_path(path))?;
        let text = String::from_utf8_lossy(&text);
        Self::from_cue_str(&text, path, |p| std::fs::metadata(p).map(|m| m.len()))
    }

    /// CUE parser with an injectable file-size oracle (pure + unit-testable).
    pub fn from_cue_str(
        text: &str,
        cue_path: &Path,
        mut file_len: impl FnMut(&Path) -> io::Result<u64>,
    ) -> Result<Self> {
        let base = cue_path.parent().unwrap_or(Path::new(""));
        let mut segments = Vec::new();
        let mut tracks: Vec<Track> = Vec::new();
        // Absolute sector at which the current FILE's first byte lands.
        let mut file_base: u32 = 0;
        let mut file_sectors: u32 = 0;
        let mut have_file = false;
        // Pending pregap for the current track (inserted before its INDEX 01 when no INDEX 00 exists).
        let mut pending_pregap: u32 = 0;

        for (lineno, raw) in text.lines().enumerate() {
            let tok = tokenize(raw);
            let Some(cmd) = tok.first().map(|s| s.to_ascii_uppercase()) else { continue };
            let err = |d: &str| Error::format("CUE", format!("line {}: {d}", lineno + 1));
            match cmd.as_str() {
                "FILE" => {
                    let name = tok.get(1).ok_or_else(|| err("FILE without name"))?;
                    let kind = tok.get(2).map(|s| s.to_ascii_uppercase()).unwrap_or_else(|| "BINARY".into());
                    if kind != "BINARY" {
                        return Err(Error::Unsupported(format!("CUE FILE type {kind} (only BINARY)")));
                    }
                    let fpath = base.join(name);
                    let len = file_len(&fpath).map_err(Error::io_path(&fpath))?;
                    if len % RAW_SECTOR != 0 {
                        return Err(err(&format!("{} size {len} not a multiple of 2352", fpath.display())));
                    }
                    if have_file {
                        file_base += file_sectors;
                    }
                    file_sectors = u32::try_from(len / RAW_SECTOR).map_err(|_| err("file too large"))?;
                    have_file = true;
                    segments.push(Segment::File { path: fpath, offset: 0, len });
                }
                "TRACK" => {
                    if !have_file {
                        return Err(err("TRACK before FILE"));
                    }
                    let number: u8 = tok.get(1).and_then(|n| n.parse().ok()).ok_or_else(|| err("bad track number"))?;
                    let kind = match tok.get(2).map(|s| s.to_ascii_uppercase()).as_deref() {
                        Some("MODE2/2352") => TrackKind::Mode2,
                        Some("MODE1/2352") => TrackKind::Mode1,
                        Some("AUDIO") => TrackKind::Audio,
                        Some(other) => {
                            return Err(Error::Unsupported(format!("track mode {other} (only */2352 raw images)")));
                        }
                        None => return Err(err("TRACK without mode")),
                    };
                    if number == 0 || number > 99 || tracks.last().is_some_and(|t| t.number >= number) {
                        return Err(err("track numbers must be increasing 1..=99"));
                    }
                    tracks.push(Track { number, kind, start: u32::MAX, index0: None });
                    pending_pregap = 0;
                }
                "PREGAP" => {
                    if tracks.is_empty() {
                        return Err(err("PREGAP before TRACK"));
                    }
                    pending_pregap = parse_msf(tok.get(1).ok_or_else(|| err("PREGAP without length"))?)?;
                }
                "INDEX" => {
                    let t = tracks.last_mut().ok_or_else(|| err("INDEX before TRACK"))?;
                    let idx: u8 = tok.get(1).and_then(|n| n.parse().ok()).ok_or_else(|| err("bad index"))?;
                    let rel = parse_msf(tok.get(2).ok_or_else(|| err("INDEX without time"))?)?;
                    if rel >= file_sectors && file_sectors > 0 {
                        return Err(err("INDEX beyond end of FILE"));
                    }
                    match idx {
                        0 => t.index0 = Some(file_base + rel),
                        1 => {
                            if pending_pregap > 0 {
                                // Virtual silence/zero sectors must physically precede INDEX 01 in the
                                // stream: split the current FILE segment at `rel` and insert zeros.
                                let Some(Segment::File { path, offset, len }) = segments.pop() else {
                                    return Err(err("PREGAP must follow a FILE"));
                                };
                                let cut = rel as u64 * RAW_SECTOR - offset;
                                if cut > 0 {
                                    segments.push(Segment::File { path: path.clone(), offset, len: cut });
                                }
                                segments.push(Segment::Zero { len: pending_pregap as u64 * RAW_SECTOR });
                                segments.push(Segment::File { path, offset: offset + cut, len: len - cut });
                                t.index0 = Some(file_base + rel);
                                file_base += pending_pregap;
                                pending_pregap = 0;
                            }
                            t.start = file_base + rel;
                        }
                        _ => {}
                    }
                }
                _ => {} // REM, CATALOG, PERFORMER, TITLE, FLAGS, ISRC, POSTGAP...
            }
        }
        if tracks.is_empty() {
            return Err(Error::format("CUE", "no tracks"));
        }
        if let Some(t) = tracks.iter().find(|t| t.start == u32::MAX) {
            return Err(Error::format("CUE", format!("track {} has no INDEX 01", t.number)));
        }
        if !tracks[0].kind.is_data() {
            return Err(Error::Unsupported("first track is AUDIO — not a PlayStation disc".into()));
        }
        let total: u64 = segments.iter().map(Segment::len).sum();
        let sectors = u32::try_from(total / RAW_SECTOR).map_err(|_| Error::format("CUE", "image too large"))?;
        Ok(Self { source: cue_path.to_path_buf(), tracks, segments, sectors })
    }

    pub fn sectors(&self) -> u32 {
        self.sectors
    }

    pub fn size_bytes(&self) -> u64 {
        self.sectors as u64 * RAW_SECTOR
    }

    /// Paths of every BIN referenced (deduplicated, in order).
    pub fn files(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = Vec::new();
        for s in &self.segments {
            if let Segment::File { path, .. } = s
                && v.last() != Some(&path.as_path())
            {
                v.push(path);
            }
        }
        v
    }

    pub fn reader(&self) -> DiscReader {
        DiscReader { segments: self.segments.clone(), pos: 0, open: None, len: self.size_bytes() }
    }

    /// Reads one raw 2352-byte sector.
    pub fn read_sector(&self, reader: &mut DiscReader, lba: u32, buf: &mut [u8; RAW_SECTOR as usize]) -> Result<()> {
        if lba >= self.sectors {
            return Err(Error::format("disc", format!("LBA {lba} beyond end ({})", self.sectors)));
        }
        reader.seek(SeekFrom::Start(lba as u64 * RAW_SECTOR))?;
        reader.read_exact(buf)?;
        Ok(())
    }

    /// Reads the 2048-byte user data of a Mode1 or Mode2/Form1 sector.
    fn read_user(&self, reader: &mut DiscReader, lba: u32) -> Result<[u8; 2048]> {
        let mut raw = [0u8; RAW_SECTOR as usize];
        self.read_sector(reader, lba, &mut raw)?;
        let off = if raw[15] == 1 { 16 } else { 24 };
        Ok(raw[off..off + 2048].try_into().unwrap())
    }

    /// Probes the ISO9660 filesystem for `SYSTEM.CNF` → BOOT executable → product code.
    pub fn probe(&self) -> DiscInfo {
        self.probe_inner().unwrap_or_default()
    }

    fn probe_inner(&self) -> Result<DiscInfo> {
        let mut rd = self.reader();
        let pvd = self.read_user(&mut rd, 16)?;
        if &pvd[1..6] != b"CD001" {
            return Err(Error::format("ISO9660", "no primary volume descriptor"));
        }
        // a-characters padded with spaces per ECMA-119; some mastering tools pad with NUL.
        let volume_id = String::from_utf8_lossy(&pvd[40..72]).trim_matches(|c: char| c == ' ' || c == '\0').to_string();
        let root = &pvd[156..156 + 34];
        let root_lba = u32::from_le_bytes(root[2..6].try_into().unwrap());
        let root_len = u32::from_le_bytes(root[10..14].try_into().unwrap()).min(64 * 2048);

        let mut info = DiscInfo { disc_id: None, volume_id: (!volume_id.is_empty()).then_some(volume_id) };
        let Some((cnf_lba, cnf_len)) = self.find_in_dir(&mut rd, root_lba, root_len, "SYSTEM.CNF")? else {
            return Ok(info);
        };
        let mut cnf = Vec::new();
        for i in 0..cnf_len.div_ceil(2048).min(4) {
            cnf.extend_from_slice(&self.read_user(&mut rd, cnf_lba + i)?);
        }
        cnf.truncate(cnf_len as usize);
        info.disc_id = parse_system_cnf(&String::from_utf8_lossy(&cnf));
        Ok(info)
    }

    fn find_in_dir(&self, rd: &mut DiscReader, lba: u32, len: u32, name: &str) -> Result<Option<(u32, u32)>> {
        for s in 0..len.div_ceil(2048) {
            let sec = self.read_user(rd, lba + s)?;
            let mut p = 0usize;
            while p < 2048 {
                let rl = sec[p] as usize;
                if rl == 0 || p + rl > 2048 || rl < 34 {
                    break;
                }
                let nl = sec[p + 32] as usize;
                let n = &sec[p + 33..(p + 33 + nl).min(p + rl)];
                let n = n.split(|&c| c == b';').next().unwrap_or_default();
                if n.eq_ignore_ascii_case(name.as_bytes()) {
                    let flba = u32::from_le_bytes(sec[p + 2..p + 6].try_into().unwrap());
                    let flen = u32::from_le_bytes(sec[p + 10..p + 14].try_into().unwrap());
                    return Ok(Some((flba, flen)));
                }
                p += rl;
            }
        }
        Ok(None)
    }
}

/// `BOOT = cdrom:\SLUS_005.94;1` → `SLUS00594`.
pub fn parse_system_cnf(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.trim_start().to_ascii_uppercase().starts_with("BOOT"))?;
    let value = line.split_once('=')?.1.trim();
    let exe = value.rsplit(['\\', '/', ':']).next()?;
    let exe = exe.split(';').next()?;
    let id: String = exe.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_uppercase();
    (id.len() == 9 && id[..4].chars().all(|c| c.is_ascii_alphabetic()) && id[4..].chars().all(|c| c.is_ascii_digit()))
        .then_some(id)
}

/// Seekable reader over the concatenated segments of a [`DiscImage`].
pub struct DiscReader {
    segments: Vec<Segment>,
    pos: u64,
    open: Option<(PathBuf, File)>,
    len: u64,
}

impl DiscReader {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn locate(&self, pos: u64) -> Option<(usize, u64)> {
        let mut base = 0;
        for (i, s) in self.segments.iter().enumerate() {
            if pos < base + s.len() {
                return Some((i, pos - base));
            }
            base += s.len();
        }
        None
    }
}

impl Read for DiscReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some((idx, within)) = self.locate(self.pos) else { return Ok(0) };
        let seg = &self.segments[idx];
        let n = (seg.len() - within).min(buf.len() as u64) as usize;
        match seg {
            Segment::Zero { .. } => buf[..n].fill(0),
            Segment::File { path, offset, .. } => {
                if !matches!(&self.open, Some((p, _)) if p == path) {
                    self.open = Some((path.clone(), File::open(path)?));
                }
                let f = &mut self.open.as_mut().unwrap().1;
                f.seek(SeekFrom::Start(offset + within))?;
                f.read_exact(&mut buf[..n])?;
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for DiscReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let p = match to {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if p < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes<'a>(map: &'a [(&'a str, u64)]) -> impl FnMut(&Path) -> io::Result<u64> + 'a {
        move |p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            map.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| *s)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_string()))
        }
    }

    #[test]
    fn multi_bin_redump_cue() {
        let cue = r#"FILE "Game (Track 1).bin" BINARY
  TRACK 01 MODE2/2352
    INDEX 01 00:00:00
FILE "Game (Track 2).bin" BINARY
  TRACK 02 AUDIO
    INDEX 00 00:00:00
    INDEX 01 00:02:00
"#;
        let d = DiscImage::from_cue_str(
            cue,
            Path::new("dir/Game.cue"),
            sizes(&[("Game (Track 1).bin", 1000 * RAW_SECTOR), ("Game (Track 2).bin", 500 * RAW_SECTOR)]),
        )
        .unwrap();
        assert_eq!(d.sectors(), 1500);
        assert_eq!(d.tracks[0], Track { number: 1, kind: TrackKind::Mode2, start: 0, index0: None });
        assert_eq!(d.tracks[1], Track { number: 2, kind: TrackKind::Audio, start: 1150, index0: Some(1000) });
        assert_eq!(d.files().len(), 2);
    }

    #[test]
    fn single_bin_with_virtual_pregap() {
        let cue = "FILE game.bin BINARY\nTRACK 01 MODE2/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nPREGAP 00:02:00\nINDEX 01 00:10:00\n";
        let d = DiscImage::from_cue_str(cue, Path::new("game.cue"), sizes(&[("game.bin", 2000 * RAW_SECTOR)])).unwrap();
        // 150 zero sectors inserted before track 2 INDEX 01 (which was at relative sector 750).
        assert_eq!(d.sectors(), 2150);
        assert_eq!(d.tracks[1].index0, Some(750));
        assert_eq!(d.tracks[1].start, 900);
        assert_eq!(
            d.segments,
            vec![
                Segment::File { path: PathBuf::from("game.bin"), offset: 0, len: 750 * RAW_SECTOR },
                Segment::Zero { len: 150 * RAW_SECTOR },
                Segment::File { path: PathBuf::from("game.bin"), offset: 750 * RAW_SECTOR, len: 1250 * RAW_SECTOR },
            ]
        );
    }

    #[test]
    fn rejects_cooked_and_audio_first() {
        let bad = "FILE a.bin BINARY\nTRACK 01 MODE1/2048\nINDEX 01 00:00:00\n";
        assert!(matches!(
            DiscImage::from_cue_str(bad, Path::new("a.cue"), sizes(&[("a.bin", RAW_SECTOR)])),
            Err(Error::Unsupported(_))
        ));
        let audio = "FILE a.bin BINARY\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n";
        assert!(DiscImage::from_cue_str(audio, Path::new("a.cue"), sizes(&[("a.bin", RAW_SECTOR)])).is_err());
    }

    #[test]
    fn system_cnf_variants() {
        assert_eq!(parse_system_cnf("BOOT = cdrom:\\SLUS_005.94;1\r\nTCB = 4\r\n").as_deref(), Some("SLUS00594"));
        assert_eq!(parse_system_cnf("BOOT=cdrom:SCES_021.05;1").as_deref(), Some("SCES02105"));
        assert_eq!(parse_system_cnf("BOOT = cdrom:\\DATA\\SLPS_012.34;1").as_deref(), Some("SLPS01234"));
        assert_eq!(parse_system_cnf("BOOT = cdrom:\\PSX.EXE;1"), None);
        assert_eq!(Region::from_disc_id("SCES02105"), Region::Pal);
    }

    #[test]
    fn reader_spans_segments_and_zero_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        std::fs::write(&a, vec![0xAAu8; 2 * RAW_SECTOR as usize]).unwrap();
        let cue = "FILE a.bin BINARY\nTRACK 01 MODE2/2352\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nPREGAP 00:00:01\nINDEX 01 00:00:01\n";
        let d = DiscImage::from_cue_str(cue, &dir.path().join("a.cue"), |p| std::fs::metadata(p).map(|m| m.len())).unwrap();
        let mut all = Vec::new();
        d.reader().read_to_end(&mut all).unwrap();
        let s = RAW_SECTOR as usize;
        assert_eq!(all.len(), 3 * s);
        assert!(all[..s].iter().all(|&b| b == 0xAA));
        assert!(all[s..2 * s].iter().all(|&b| b == 0));
        assert!(all[2 * s..].iter().all(|&b| b == 0xAA));
    }
}
