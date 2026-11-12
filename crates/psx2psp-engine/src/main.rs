//! `psx2psp` — headless front-end (scripting, CI, batch conversion).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use psx2psp_engine::media::images::{self, Slot};
use psx2psp_engine::media::clip::{self, Clip, ClipMode, ClipRequest, ClipTools};
use psx2psp_engine::media::provision::{self, Tool};
use psx2psp_engine::media::tools::{self, At3Encoder, Ffmpeg, Snd0Request};
use psx2psp_engine::{Artwork, ConvertRequest, DEFAULT_LEVEL, DiscInput, convert, core::Monitor, extract, inspect, probe_disc};

const USAGE: &str = "\
psx2psp — PS1 → PSP/PS Vita (POPS) EBOOT.PBP builder

USAGE:
  psx2psp convert [options] <disc.cue|disc.bin>...
  psx2psp extract <EBOOT.PBP> <out-dir>
  psx2psp info    <EBOOT.PBP | disc.cue>
  psx2psp clip    <video|audio|URL> [clip options] [-o <dir>]   → ICON1.PMF / SND0.AT3
  psx2psp tools   [install]          show / download ffmpeg, yt-dlp, atracdenc into <exe>/tools

CONVERT OPTIONS:
  --donor <pbp>      custom PS1 Classic donor EBOOT (default: built-in BASE.PBP + DATA.PSP)
  -o, --out <path>   output EBOOT.PBP (default: ./<DISC_ID>/EBOOT.PBP)
  --title <text>     XMB title (default: volume label)
  --id <code>        product code override for disc 1, e.g. SLUS00594
  --ppf <file>       PPF patch for disc 1 (repeat after each extra disc via --ppfN, N=2..5)
  --icon0/--pic0/--pic1/--boot <image>   artwork (any format; auto-fitted)
  --icon1 <pmf>      ready-made ICON1.PMF
  --snd0 <audio>     any audio/video → SND0.AT3 (needs ffmpeg + at3tool/atracdenc)
  --snd0-start <s>   start offset in seconds (default 0)
  --level <1-9>      deflate level (default 9)
  --clip <src>       video/audio file or YouTube URL → ICON1.PMF + SND0.AT3 (see CLIP OPTIONS)

CLIP OPTIONS:
  --start <t>        clip start, seconds or [h:]m:s (default 0)
  --end <t>          clip end (default start + 30 s, capped at the source length)
  --mode <m>         both | icon1 | snd0   (default both; snd0 = soundtrack only, no video)
  --snd0-kbps <k>    ATRAC3 rate when SND0 is alone: 66 | 105 | 132 (default 132)
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
        Some("clip") => cmd_clip(&args[1..]),
        Some("tools") if args.len() == 1 => cmd_tools(false),
        Some("tools") if args.len() == 2 && args[1] == "install" => cmd_tools(true),
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

#[derive(Debug)]
struct ClipArgs {
    source: Option<String>,
    start: f64,
    end: Option<f64>,
    mode: ClipMode,
    kbps: u32,
}

impl Default for ClipArgs {
    fn default() -> Self {
        Self { source: None, start: 0.0, end: None, mode: ClipMode::Both, kbps: 132 }
    }
}

impl ClipArgs {
    /// Consumes a clip flag; `false` if `flag` is not one.
    fn take(&mut self, flag: &str, val: &mut dyn FnMut() -> Result<String, String>) -> Result<bool, Box<dyn std::error::Error>> {
        let time = |s: String| tools::parse_hms(&s).ok_or_else(|| format!("{flag}: bad time {s:?} (use seconds or m:ss)"));
        match flag {
            "--clip" => self.source = Some(val()?),
            "--start" | "--clip-start" => self.start = time(val()?)?,
            "--end" | "--clip-end" => self.end = Some(time(val()?)?),
            "--mode" | "--clip-mode" => {
                self.mode = match val()?.to_ascii_lowercase().as_str() {
                    "both" => ClipMode::Both,
                    "icon1" | "video" => ClipMode::Icon1,
                    "snd0" | "audio" => ClipMode::Snd0,
                    m => return Err(format!("--mode {m}: use both, icon1 or snd0").into()),
                }
            }
            "--snd0-kbps" => {
                self.kbps = val()?.parse()?;
                if ![66, 105, 132].contains(&self.kbps) {
                    return Err("--snd0-kbps: ATRAC3 rates are 66, 105, 132".into());
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn run(&self, work: &Path, budget: u64) -> Result<Clip, Box<dyn std::error::Error>> {
        let src = self.source.as_deref().ok_or("no clip source")?;
        let tools = ClipTools::detect()?;
        let (len, title) = clip::probe(&tools, src)?;
        if let Some(t) = title {
            eprintln!("Source: {t}");
        }
        let end = self.end.unwrap_or(self.start + 30.0);
        let end = len.map_or(end, |l| end.min(l));
        eprintln!("Clip {} → {} ({}), {}", tools::fmt_hms(self.start), tools::fmt_hms(end), self.mode, src);
        let req = ClipRequest { source: src, start: self.start, end, mode: self.mode, snd0_kbps: self.kbps, budget };
        let r = clip::make_clip(&tools, &req, work, &|m| eprintln!("{m}"));
        let _ = std::fs::remove_dir_all(work);
        Ok(r?)
    }
}

fn cmd_clip(args: &[String]) -> R {
    let mut it = args.iter();
    let mut c = ClipArgs::default();
    let mut out = PathBuf::from(".");
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
        if c.take(a, &mut val)? {
            continue;
        }
        match a.as_str() {
            "-o" | "--out" => out = PathBuf::from(val()?),
            s if s.starts_with("--") => return Err(format!("unknown flag {s}").into()),
            s => c.source = Some(s.to_string()),
        }
    }
    std::fs::create_dir_all(&out)?;
    let clip = c.run(&psx2psp_engine::work_dir(&out.join("EBOOT.PBP")), tools::SND0_BUDGET)?;
    for (name, data) in [("ICON1.PMF", &clip.icon1), ("SND0.AT3", &clip.snd0)] {
        if let Some(d) = data {
            let p = out.join(name);
            std::fs::write(&p, d)?;
            println!("{} ({} bytes)", p.display(), d.len());
        }
    }
    Ok(())
}

fn cmd_tools(install: bool) -> R {
    for t in Tool::ALL {
        let state = if t.present() { "ok" } else if t.asset().is_some() { "missing (installable)" } else { "missing" };
        println!("{:<10} {state}", t.exe());
    }
    if !install {
        return Ok(());
    }
    for t in Tool::ALL.into_iter().filter(|t| !t.present()) {
        match t.asset() {
            Some(a) => {
                provision::install(&a, &|m| eprintln!("{m}"))?;
            }
            None => eprintln!("{}: no prebuilt binary for this platform — {}", t.exe(), t.manual_hint()),
        }
    }
    Ok(())
}

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
    let mut clip_args = ClipArgs::default();
    let mut level = DEFAULT_LEVEL;
    let mut discs: Vec<DiscInput> = Vec::new();
    let mut ppfs: Vec<(usize, PathBuf)> = Vec::new();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
        if clip_args.take(a, &mut val)? {
            continue;
        }
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

    let mut clip = Clip::default();
    if clip_args.source.is_some() {
        if icon1.is_some() || snd0.is_some() {
            return Err("--clip makes ICON1/SND0: don't combine it with --icon1/--snd0".into());
        }
        clip = clip_args.run(&psx2psp_engine::work_dir(&output), tools::SND0_BUDGET)?;
    }
    let snd0 = match snd0 {
        Some(src) => {
            let ff = Ffmpeg::detect().ok_or(psx2psp_engine::media::MediaError::Missing("ffmpeg"))?;
            let enc = At3Encoder::detect().ok_or(psx2psp_engine::media::MediaError::Missing("at3tool/atracdenc"))?;
            let icon1_len = icon1.as_ref().and_then(|p: &PathBuf| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
            let work = psx2psp_engine::work_dir(&output);
            std::fs::create_dir_all(&work)?;
            let r = tools::make_snd0(&ff, &enc, &Snd0Request { input: &src, start_secs: snd0_start, max_secs: None, kbps: 132, budget: tools::SND0_BUDGET.saturating_sub(icon1_len) }, &work);
            let _ = std::fs::remove_dir_all(&work);
            Some(r?)
        }
        None => clip.snd0,
    };
    let artwork = Artwork {
        icon0: slot(Slot::Icon0, &icon0)?,
        pic0: slot(Slot::Pic0, &pic0)?,
        pic1: slot(Slot::Pic1, &pic1)?,
        boot: slot(Slot::Boot, &boot)?,
        icon1_pmf: icon1.map(std::fs::read).transpose()?.or(clip.icon1),
        snd0,
    };
    let req = ConvertRequest { discs, title, output, donor, artwork, level };
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
