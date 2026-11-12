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
use psx2psp_core::{DiscImage, DiscSpec, Donor, ImagePatch, Monitor, PbpFile, PbpSections, PsarJob, PsarStats, Region, Section, Sfo};
use psx2psp_patching::PpfPatch;

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
/// + `\0PGD` trailer, verified offsets: PSAR 0x50000, STARTDAT at p1 0x100000).
pub const BUILTIN_BASE_PBP: &[u8] = include_bytes!("../../../resources/BASE.PBP");
/// Built-in loader: the 19 002-byte ELF `DATA.PSP` that popstation/PSX2PSP and PSXPackager write
/// in preference to the donor's own signed module.
pub const BUILTIN_DATA_PSP: &[u8] = include_bytes!("../../../resources/DATA.PSP");

/// The donor used when the user supplies none: everything needed ships inside the binary.
pub fn builtin_donor() -> Result<Donor> {
    let mut donor = Donor::from_pbp(&mut PbpFile::open(std::io::Cursor::new(BUILTIN_BASE_PBP))?)?;
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

/// PSAR alignment used by every known working tool (PSXPackager, popstation).
pub const PSAR_ALIGN: u64 = 0x10000;
/// Default deflate level.
pub const DEFAULT_LEVEL: u32 = 9;

#[derive(Debug, Clone, Default)]
pub struct DiscInput {
    pub path: PathBuf,
    /// Overrides the ID probed from SYSTEM.CNF.
    pub disc_id: Option<String>,
    /// PPF patch applied in-stream to this disc.
    pub ppf: Option<PathBuf>,
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
        if output.exists() {
            std::fs::remove_file(output).map_err(psx2psp_core::Error::io_path(output))?;
        }
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
    let mut ppfs: Vec<Option<PpfPatch>> = Vec::with_capacity(req.discs.len());
    for (n, d) in req.discs.iter().enumerate() {
        let img = DiscImage::open(&d.path)?;
        let id = match &d.disc_id {
            Some(id) if !id.trim().is_empty() => normalise_id(id),
            _ => img.probe().disc_id.ok_or_else(|| {
                EngineError::Input(format!("disc {}: no SYSTEM.CNF game ID found — enter it manually", n + 1))
            })?,
        };
        monitor.log(&format!("Disc {}: {} — {} tracks, {:.1} MB, ID {id}", n + 1, d.path.display(), img.tracks.len(), img.size_bytes() as f64 / 1e6));
        let ppf = match &d.ppf {
            Some(p) => {
                let bytes = std::fs::read(p).map_err(psx2psp_core::Error::io_path(p))?;
                let patch = PpfPatch::parse(&bytes).map_err(|source| EngineError::Ppf { path: p.display().to_string(), source })?;
                if let Some((at, block)) = &patch.validation {
                    let mut rd = img.reader();
                    let mut buf = vec![0u8; block.len()];
                    rd.seek(SeekFrom::Start(*at))?;
                    rd.read_exact(&mut buf)?;
                    if !patch.validates(&buf) {
                        return Err(EngineError::Input(format!("{}: PPF validation block does not match disc {} — wrong image/version", p.display(), n + 1)));
                    }
                }
                monitor.log(&format!("Disc {}: {}", n + 1, patch.describe()));
                Some(patch)
            }
            None => None,
        };
        images.push(img);
        ids.push(id);
        ppfs.push(ppf);
    }

    let multi = images.len() > 1;
    let job = PsarJob {
        discs: images
            .iter()
            .zip(&ids)
            .zip(&ppfs)
            .enumerate()
            .map(|(i, ((image, id), ppf))| DiscSpec {
                image,
                disc_id: id.clone(),
                title: if multi { format!("{} (Disc {})", req.title, i + 1) } else { req.title.clone() },
                patches: ppf.iter().map(|p| p as &dyn ImagePatch).collect(),
            })
            .collect(),
        title: req.title.clone(),
        level: req.level,
        boot_png: req.artwork.boot.as_deref().unwrap_or(&donor.boot_png),
        donor: &donor,
    };

    let sfo = Sfo::pops(&req.title, &ids[0]);
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
    monitor.log(&format!("Compressing {} disc(s) at level {} on {} threads", images.len(), req.level.clamp(1, 9), std::thread::available_parallelism().map_or(1, |n| n.get())));
    write_pbp(&mut w, &sections, PSAR_ALIGN, |w, base| {
        stats = psar::write_psar(w, base, &job, monitor)?;
        Ok(())
    })?;
    let file = w.into_inner().map_err(|e| EngineError::from(e.into_error()))?;
    file.sync_all()?;
    let file_size = file.metadata()?.len();
    drop(file);
    part.commit(&req.output)?;

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
pub fn extract(path: &Path, out_dir: &Path, monitor: &dyn Monitor) -> Result<Vec<PathBuf>> {
    let f = File::open(path).map_err(psx2psp_core::Error::io_path(path))?;
    let mut pbp = PbpFile::open(BufReader::with_capacity(1 << 20, f))?;
    std::fs::create_dir_all(out_dir).map_err(psx2psp_core::Error::io_path(out_dir))?;
    let mut produced = Vec::new();
    for s in Section::ALL {
        if s == Section::DataPsar {
            continue;
        }
        let data = pbp.read(s)?;
        if !data.is_empty() {
            let p = out_dir.join(s.file_name());
            std::fs::write(&p, &data).map_err(psx2psp_core::Error::io_path(&p))?;
            produced.push(p);
        }
    }
    let psar_at = pbp.range(Section::DataPsar).start;
    let discs = psar::read_discs(pbp.inner_mut(), psar_at)?;
    let multi = discs.len() > 1;
    for (i, d) in discs.iter().enumerate() {
        let stem = if multi { format!("{} (Disc {})", d.disc_id, i + 1) } else { d.disc_id.clone() };
        let bin = out_dir.join(format!("{stem}.bin"));
        let part = PartFile::new(&bin);
        let mut w = BufWriter::with_capacity(8 << 20, File::create(&part.part).map_err(psx2psp_core::Error::io_path(&part.part))?);
        monitor.log(&format!("Extracting disc {} ({}) → {}", i + 1, d.disc_id, bin.display()));
        psar::extract_disc(pbp.inner_mut(), psar_at, d, &mut w, monitor)?;
        w.flush()?;
        drop(w);
        part.commit(&bin)?;
        let cue = out_dir.join(format!("{stem}.cue"));
        std::fs::write(&cue, psar::toc_to_cue(&format!("{stem}.bin"), &d.tracks())).map_err(psx2psp_core::Error::io_path(&cue))?;
        produced.push(bin);
        produced.push(cue);
    }
    Ok(produced)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
