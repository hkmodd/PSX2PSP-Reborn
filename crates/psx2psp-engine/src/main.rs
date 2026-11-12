//! `psx2psp` — headless front-end (scripting, CI, batch conversion).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use psx2psp_engine::media::images::{self, Slot};
use psx2psp_engine::media::tools::{self, At3Encoder, Ffmpeg, Snd0Request};
use psx2psp_engine::{Artwork, ConvertRequest, DEFAULT_LEVEL, DiscInput, convert, core::Monitor, extract, inspect, probe_disc};

const USAGE: &str = "\
psx2psp — PS1 → PSP/PS Vita (POPS) EBOOT.PBP builder

USAGE:
  psx2psp convert --donor BASE.PBP [options] <disc.cue|disc.bin>...
  psx2psp extract <EBOOT.PBP> <out-dir>
  psx2psp info    <EBOOT.PBP | disc.cue>

CONVERT OPTIONS:
  --donor <pbp>      PS1 Classic EBOOT providing DATA.PSP + STARTDAT trailer (required)
  -o, --out <path>   output EBOOT.PBP (default: ./<DISC_ID>/EBOOT.PBP)
  --title <text>     XMB title (default: volume label)
  --id <code>        product code override for disc 1, e.g. SLUS00594
  --ppf <file>       PPF patch for disc 1 (repeat after each extra disc via --ppfN, N=2..5)
  --icon0/--pic0/--pic1/--boot <image>   artwork (any format; auto-fitted)
  --icon1 <pmf>      ready-made ICON1.PMF
  --snd0 <audio>     any audio/video → SND0.AT3 (needs ffmpeg + at3tool/atracdenc)
  --snd0-start <s>   start offset in seconds (default 0)
  --level <1-9>      deflate level (default 9)
";

struct Cli {
    last: AtomicU64,
}

impl Monitor for Cli {
    fn progress(&self, done: u64, total: u64) {
        let pct = done * 1000 / total.max(1);
        if self.last.swap(pct, Ordering::Relaxed) != pct {
            eprint!("\r  {:>5.1}%  {:>8.1} / {:.1} MB", pct as f64 / 10.0, done as f64 / 1e6, total as f64 / 1e6);
            let _ = std::io::stderr().flush();
        }
    }
    fn log(&self, m: &str) {
        eprintln!("\r{m}");
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("convert") => cmd_convert(&args[1..]),
        Some("extract") if args.len() == 3 => cmd_extract(Path::new(&args[1]), Path::new(&args[2])),
        Some("info") if args.len() == 2 => cmd_info(Path::new(&args[1])),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\nerror: {e}");
            ExitCode::FAILURE
        }
    }
}

type R = Result<(), Box<dyn std::error::Error>>;

fn slot(slot: Slot, p: &Option<PathBuf>) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
    Ok(match p {
        Some(p) => Some(images::prepare_file(slot, p)?.png),
        None => None,
    })
}

fn cmd_convert(args: &[String]) -> R {
    let mut it = args.iter();
    let (mut donor, mut out, mut title, mut id) = (None, None, None, None);
    let (mut icon0, mut pic0, mut pic1, mut boot, mut icon1, mut snd0) = (None, None, None, None, None, None);
    let mut snd0_start = 0.0f64;
    let mut level = DEFAULT_LEVEL;
    let mut discs: Vec<DiscInput> = Vec::new();
    let mut ppfs: Vec<(usize, PathBuf)> = Vec::new();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--donor" => donor = Some(PathBuf::from(val()?)),
            "-o" | "--out" => out = Some(PathBuf::from(val()?)),
            "--title" => title = Some(val()?),
            "--id" => id = Some(val()?),
            "--icon0" => icon0 = Some(PathBuf::from(val()?)),
            "--pic0" => pic0 = Some(PathBuf::from(val()?)),
            "--pic1" => pic1 = Some(PathBuf::from(val()?)),
            "--boot" => boot = Some(PathBuf::from(val()?)),
            "--icon1" => icon1 = Some(PathBuf::from(val()?)),
            "--snd0" => snd0 = Some(PathBuf::from(val()?)),
            "--snd0-start" => snd0_start = val()?.parse()?,
            "--level" => level = val()?.parse()?,
            "--ppf" => ppfs.push((0, PathBuf::from(val()?))),
            s if s.starts_with("--ppf") => {
                let n: usize = s[5..].parse().map_err(|_| format!("bad flag {s}"))?;
                ppfs.push((n.checked_sub(1).ok_or("disc numbers start at 1")?, PathBuf::from(val()?)));
            }
            s if s.starts_with('-') => return Err(format!("unknown flag {s}").into()),
            s => discs.push(DiscInput { path: PathBuf::from(s), ..Default::default() }),
        }
    }
    let donor = donor.ok_or("--donor is required (a PS1 Classic EBOOT.PBP, e.g. BASE.PBP)")?;
    if discs.is_empty() {
        return Err("no disc image given".into());
    }
    let count = discs.len();
    for (n, p) in ppfs {
        discs.get_mut(n).ok_or(format!("--ppf for disc {} but only {count} discs", n + 1))?.ppf = Some(p);
    }
    discs[0].disc_id = id;

    let first = probe_disc(&discs[0].path)?;
    let disc_id = discs[0].disc_id.clone().or(first.disc_id.clone()).ok_or("cannot detect game ID; pass --id")?;
    let title = title.or(first.volume_id.clone()).unwrap_or_else(|| disc_id.clone());
    let output = out.unwrap_or_else(|| psx2psp_engine::default_output(Path::new("."), &disc_id));

    let snd0 = match snd0 {
        Some(src) => {
            let ff = Ffmpeg::detect().ok_or(psx2psp_engine::media::MediaError::Missing("ffmpeg"))?;
            let enc = At3Encoder::detect().ok_or(psx2psp_engine::media::MediaError::Missing("at3tool/atracdenc"))?;
            let icon1_len = icon1.as_ref().and_then(|p: &PathBuf| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
            let work = psx2psp_engine::work_dir(&output);
            std::fs::create_dir_all(&work)?;
            let r = tools::make_snd0(&ff, &enc, &Snd0Request { input: &src, start_secs: snd0_start, kbps: 132, budget: tools::SND0_BUDGET.saturating_sub(icon1_len) }, &work);
            let _ = std::fs::remove_dir_all(&work);
            Some(r?)
        }
        None => None,
    };
    let artwork = Artwork {
        icon0: slot(Slot::Icon0, &icon0)?,
        pic0: slot(Slot::Pic0, &pic0)?,
        pic1: slot(Slot::Pic1, &pic1)?,
        boot: slot(Slot::Boot, &boot)?,
        icon1_pmf: icon1.map(std::fs::read).transpose()?,
        snd0,
    };
    let req =ConvertRequest { discs, title, output, donor, artwork, level };
    let rep = convert(&req, &Cli { last: AtomicU64::new(u64::MAX) })?;
    println!("{}", rep.output.display());
    Ok(())
}

fn cmd_extract(pbp: &Path, out: &Path) -> R {
    for p in extract(pbp, out, &Cli { last: AtomicU64::new(u64::MAX) })? {
        println!("{}", p.display());
    }
    Ok(())
}

fn cmd_info(p: &Path) -> R {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if ext != "pbp" {
        let d = probe_disc(p)?;
        println!("disc     {}", d.path.display());
        println!("id       {}", d.disc_id.as_deref().unwrap_or("?"));
        println!("volume   {}", d.volume_id.as_deref().unwrap_or("?"));
        println!("region   {}", d.region.label());
        println!("tracks   {} ({} audio)", d.tracks, d.audio_tracks);
        println!("size     {} bytes", d.size);
        return Ok(());
    }
    let s = inspect(p)?;
    for e in s.sfo.entries() {
        println!("SFO  {:<16} {:?}", e.key, e.value);
    }
    for (sec, len) in &s.sections {
        println!("PBP  {:<10} {len:>12} B", sec.file_name());
    }
    for (i, d) in s.discs.iter().enumerate() {
        println!("DISC {} {} \"{}\" {} blocks, {} tracks, {} B", i + 1, d.disc_id, d.title, d.index.len(), d.tracks().len(), d.image_size());
    }
    Ok(())
}
