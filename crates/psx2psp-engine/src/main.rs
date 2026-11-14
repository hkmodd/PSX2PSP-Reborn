//! `psx2psp` — headless front-end (scripting, CI, batch conversion).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use psx2psp_engine::media::images::{self, Slot};
use psx2psp_engine::media::clip::{self, Clip, ClipMode, ClipRequest, ClipTools};
use psx2psp_engine::media::provision::{self, Tool};
use psx2psp_engine::media::tools::{self, At3Encoder, Ffmpeg, Snd0Request};
use psx2psp_engine::patchdb::{self, PatchDb};
use psx2psp_engine::update;
use psx2psp_engine::{Artwork, ConvertRequest, DEFAULT_LEVEL, DiscInput, convert, core::Monitor, extract, inspect, probe_disc};

const USAGE: &str = "\
psx2psp — PS1 → PSP/PS Vita (POPS) EBOOT.PBP builder

USAGE:
  psx2psp convert [options] <disc.cue|disc.bin>...
  psx2psp extract <EBOOT.PBP> <out-dir>
  psx2psp info    <EBOOT.PBP | disc.cue>
  psx2psp patches <disc.cue|disc.bin> [--id <code>] [--ppf <file>]
                                     the patch database's entries for the game, with hits
  psx2psp clip    <video|audio|URL> [clip options] [-o <dir>]   → ICON1.PMF / SND0.AT3
  psx2psp art     <image> [--slot icon0|pic0|pic1|boot] [-o <dir>]
                                     frame any picture into the XMB slots (all by default)
                                     as the converter would; prints the crop chosen
  psx2psp tools   [install]          show / download ffmpeg, yt-dlp, atracdenc into <exe>/tools
  psx2psp update  [--check]          replace this executable with the latest GitHub release
                                     (SHA-256 verified); --check only reports

CONVERT OPTIONS:
  --donor <pbp>      custom PS1 Classic donor EBOOT (default: built-in BASE.PBP + DATA.PSP)
  -o, --out <path>   output EBOOT.PBP (default: ./<DISC_ID>/EBOOT.PBP)
  --title <text>     XMB title (default: volume label)
  --id <code>        product code override for disc 1, e.g. SLUS00594
  --ppf <file>       PPF patch for disc 1 (repeat after each extra disc via --ppfN, N=2..5)
  --pal-ntsc         try the generic PAL→NTSC preset on every disc (applied only where its
                     video-mode fix matches; the log lists every rule hit)
  --no-title-fixes   skip the game's \"fix\" patches and POPS config from the patch database;
                     a <image>.pspconfig beside the disc always applies
  --fps-unlock       apply the game's \"fps\" patches (frame-rate unlock)
  --60hz             apply the game's \"60hz\" patches (its own 60 Hz NTSC mode)
  --no-fps-unlock, --50hz   turn those off (on by default for games verified on hardware;
                     psx2psp patches <disc> shows the defaults)
  --patch <name>     turn one patch on by name, even one that is off by default; the name
                     \"POPS config\" selects the config (list: psx2psp patches <disc>)
  --no-patch <name>  turn one patch off by name
  --icon0/--pic0/--pic1/--boot <image>   artwork (any format; auto-fitted)
  --icon1 <pmf>      ready-made ICON1.PMF
  --snd0 <audio>     any audio/video → SND0.AT3 (needs ffmpeg + at3tool/atracdenc)
  --snd0-start <s>   start offset in seconds (default 0)
  --level <1-9>      deflate level (default 5, recommended; 1 builds faster and larger, 9 is
                     ~0.1% smaller and twice as slow). Games that hang on a real PSP below
                     some level are raised to it (patch database min_level, e.g. MediEvil)
  --clip <src>       video/audio file or YouTube URL → ICON1.PMF + SND0.AT3 (see CLIP OPTIONS)

CLIP OPTIONS:
  --start <t>        clip start, seconds or [h:]m:s (default 0)
  --end <t>          clip end (default start + 30 s, capped at the source length)
  --mode <m>         both | icon1 | snd0   (default both; snd0 = soundtrack only, no video)
  --snd0-kbps <k>    ATRAC3 rate when SND0 is alone: 66 | 105 | 132 (default 132)

PATCH DATABASE:
  Built in, with your own patches.ini merged on top (same name for the same game wins): a
  patches.ini next to the executable if present, else the one in the user config folder
  (psx2psp patches prints its path). The format is documented in the built-in file.
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

/// Monitor for media jobs, whose progress is a plain fraction (no byte counts).
struct Pct {
    last: AtomicU64,
}

impl Monitor for Pct {
    fn progress(&self, done: u64, total: u64) {
        let pct = done * 100 / total.max(1);
        if self.last.swap(pct, Ordering::Relaxed) != pct {
            eprint!("\r  {pct:>3}%");
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
        Some("art") => cmd_art(&args[1..]),
        Some("patches") => cmd_patches(&args[1..]),
        Some("tools") if args.len() == 1 => cmd_tools(false),
        Some("tools") if args.len() == 2 && args[1] == "install" => cmd_tools(true),
        Some("update") if args.len() == 1 => cmd_update(false),
        Some("update") if args.len() == 2 && args[1] == "--check" => cmd_update(true),
        // Asked-for help goes to stdout with success; a wrong command line stays an error (2).
        Some("-h" | "--help" | "help") if args.len() == 1 => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") if args.len() == 1 => {
            println!("psx2psp {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
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
        let (len, title) = clip::probe(&tools, src, &())?;
        if let Some(t) = title {
            eprintln!("Source: {t}");
        }
        let end = self.end.unwrap_or(self.start + 30.0);
        let end = len.map_or(end, |l| end.min(l));
        eprintln!("Clip {} → {} ({}), {}", tools::fmt_hms(self.start), tools::fmt_hms(end), self.mode, src);
        let req = ClipRequest { source: src, start: self.start, end, mode: self.mode, snd0_kbps: self.kbps, budget };
        let r = clip::make_clip(&tools, &req, work, &Pct { last: AtomicU64::new(u64::MAX) });
        eprintln!();
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

fn cmd_art(args: &[String]) -> R {
    let mut it = args.iter();
    let (mut src, mut out, mut slots) = (None, PathBuf::from("."), Slot::ALL.to_vec());
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "-o" | "--out" => out = PathBuf::from(val()?),
            "--slot" => {
                let v = val()?;
                slots = vec![*Slot::ALL.iter().find(|s| s.label().eq_ignore_ascii_case(&v)).ok_or_else(|| format!("unknown slot {v}"))?];
            }
            s if s.starts_with("--") => return Err(format!("unknown flag {s}").into()),
            s => src = Some(PathBuf::from(s)),
        }
    }
    let src = src.ok_or("art needs an image")?;
    let bytes = std::fs::read(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    let img = images::decode(&bytes)?;
    std::fs::create_dir_all(&out)?;
    let stem = src.file_stem().map_or("art".into(), |s| s.to_string_lossy().into_owned());
    for slot in slots {
        let t = std::time::Instant::now();
        let f = psx2psp_engine::media::framing::plan(slot, &img);
        let pic = psx2psp_engine::media::framing::render(slot, &img, &f);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let p = out.join(format!("{stem}.{}.png", slot.label()));
        std::fs::write(&p, images::encode_png(&pic)?)?;
        let (c, d) = (f.crop, f.dest);
        println!(
            "{:<5} crop {:.0},{:.0} {:.0}x{:.0} of {}x{} → {:.0},{:.0} {:.0}x{:.0}  fill {:?}  {ms:.1} ms  {}",
            slot.label(),
            c.x,
            c.y,
            c.w,
            c.h,
            img.width(),
            img.height(),
            d.x,
            d.y,
            d.w,
            d.h,
            f.fill,
            p.display()
        );
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

fn cmd_update(check_only: bool) -> R {
    update::cleanup_leftovers(update::Program::Cli, std::time::Duration::ZERO);
    println!("psx2psp v{}", update::CURRENT);
    let release = match update::check(update::Program::Cli) {
        Ok(update::Check::UpToDate(latest)) => {
            println!("Up to date (latest release: v{latest}).");
            return Ok(());
        }
        Err(update::UpdateError::NoRelease) => {
            println!("No release has been published yet.");
            return Ok(());
        }
        Ok(update::Check::Available(r)) => r,
        Err(e) => return Err(e.into()),
    };
    println!("v{} is available: {}", release.version, release.page);
    if check_only {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or("the executable has no parent directory")?;
    let file = update::download(&release, dir, &Cli { last: AtomicU64::new(u64::MAX) })?;
    update::install(&file, &exe)?;
    println!("\nUpdated to v{} ({}).", release.version, exe.display());
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
    // Switches the user did not set follow the game's defaults in the patch database.
    let (mut pal_ntsc, mut title_fixes, mut fps_unlock, mut ntsc_60hz) = (false, None, None, None);
    let mut patch_choice: Vec<(String, bool)> = Vec::new();
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
            "--level" => {
                level = val()?.parse()?;
                if !psx2psp_engine::LEVELS.contains(&level) {
                    return Err(format!("--level {level}: use 1..=9").into());
                }
            }
            "--pal-ntsc" => pal_ntsc = true,
            "--no-title-fixes" => title_fixes = Some(false),
            "--fps-unlock" => fps_unlock = Some(true),
            "--no-fps-unlock" => fps_unlock = Some(false),
            "--60hz" => ntsc_60hz = Some(true),
            "--50hz" => ntsc_60hz = Some(false),
            "--patch" => patch_choice.push((val()?, true)),
            "--no-patch" => patch_choice.push((val()?, false)),
            "--ppf" => ppfs.push((0, PathBuf::from(val()?))),
            s if s.starts_with("--ppf") => {
                let n: usize = s[5..].parse().map_err(|_| format!("bad flag {s}"))?;
                ppfs.push((n.checked_sub(1).ok_or("disc numbers start at 1")?, PathBuf::from(val()?)));
            }
            s if s.starts_with('-') => return Err(format!("unknown flag {s}").into()),
            s => discs.push(DiscInput { path: PathBuf::from(s), ..Default::default() }),
        }
    }
    let given: Vec<PathBuf> = discs.into_iter().map(|d| d.path).collect();
    let plan = psx2psp_engine::resolve_discs(&[], &given);
    for note in &plan.notes {
        eprintln!("{note}");
    }
    let mut discs: Vec<DiscInput> = plan.discs.into_iter().map(|path| DiscInput { path, ..Default::default() }).collect();
    if discs.is_empty() {
        return Err("no disc image given".into());
    }
    let count = discs.len();
    for (n, p) in ppfs {
        discs.get_mut(n).ok_or(format!("--ppf for disc {} but only {count} discs", n + 1))?.ppf = Some(p);
    }
    discs[0].disc_id = id;
    let patches = load_patches();

    let first = probe_disc(&discs[0].path)?;
    let disc_id = discs[0].disc_id.clone().or(first.disc_id.clone()).ok_or("cannot detect game ID; pass --id")?;
    // One set of switches for the whole game, from disc 1 (each disc still gets only the
    // patches its own ID has).
    let def = patches.default_switches(&patches.pops_id(&disc_id));
    for d in &mut discs {
        (d.pal_ntsc, d.title_fixes, d.fps_unlock, d.ntsc_60hz) =
            (pal_ntsc, title_fixes.unwrap_or(def.fixes), fps_unlock.unwrap_or(def.fps), ntsc_60hz.unwrap_or(def.ntsc60));
        d.patch_choice.clone_from(&patch_choice);
    }
    let title = title.or_else(|| psx2psp_engine::gamedb::lookup(&disc_id).map(|g| g.title.clone())).or(first.volume_id.clone()).unwrap_or_else(|| disc_id.clone());
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
            let r = tools::make_snd0(&ff, &enc, &Snd0Request { input: &src, start_secs: snd0_start, max_secs: None, kbps: 132, budget: tools::SND0_BUDGET.saturating_sub(icon1_len) }, &work, &());
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
    let req = ConvertRequest { discs, patches, title, output, donor, artwork, level };
    let rep = convert(&req, &Cli { last: AtomicU64::new(u64::MAX) })?;
    println!("{}", rep.output.display());
    Ok(())
}

/// The patch database with the user file merged on top; a broken user file is reported and
/// left out (the built-in database still applies).
fn load_patches() -> PatchDb {
    let l = patchdb::load();
    if let Some(e) = &l.error {
        eprintln!("warning: {e} — your patches.ini is ignored");
    }
    l.db
}

fn cmd_patches(args: &[String]) -> R {
    let (mut disc, mut id, mut ppf) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--id" => id = Some(val()?),
            "--ppf" => ppf = Some(PathBuf::from(val()?)),
            s if s.starts_with('-') => return Err(format!("unknown flag {s}").into()),
            s => disc = Some(PathBuf::from(s)),
        }
    }
    let l = patchdb::load();
    match (&l.user_path, &l.error) {
        (_, Some(e)) => println!("user patches: {e} (ignored)"),
        (Some(p), None) => println!("user patches: {}{}", p.display(), if p.is_file() { "" } else { " (not created yet)" }),
        (None, None) => println!("user patches: no config folder on this system"),
    }
    let given = [disc.ok_or("no disc image given")?];
    let disc = psx2psp_engine::resolve_discs(&[], &given).discs.into_iter().next().ok_or("no disc image given")?;
    let disc_id = match id {
        Some(id) => patchdb::normalise_id(&id),
        None => probe_disc(&disc)?.disc_id.ok_or("cannot detect game ID; pass --id")?,
    };
    let id = l.db.pops_id(&disc_id);
    let game = l.db.game(&id);
    let built_as = if id == disc_id { String::new() } else { format!(" (built as {id})") };
    println!("{disc_id}{built_as}: {}", game.map_or("not in the patch database", |g| g.title.as_str()));
    match (psx2psp_engine::core::subq::find_beside(&disc), psx2psp_engine::sbidb::lookup(&disc_id), l.db.libcrypt(&id)) {
        (Some(f), ..) => println!("  LibCrypt: the EBOOT carries the subchannel table of {}", f.display()),
        (None, [s], _) => println!("  LibCrypt: the EBOOT carries the built-in SBI for {} ({} sectors)", s.serial, s.entries.len()),
        (None, dumps, magic) => {
            if let [s, ..] = dumps {
                let titles = psx2psp_engine::sbidb::titles(dumps);
                println!("  LibCrypt: {} different built-in SBIs for {} ({titles}): put the .sbi of this dump beside the disc", dumps.len(), s.serial);
            }
            if let Some(magic) = magic {
                println!("  LibCrypt magic word 0x{magic:04X}: no .sbi beside the disc, table from the magic word");
            }
        }
    }
    // [on ]/[off] = what `convert` does without switches or --patch/--no-patch.
    let def = l.db.default_switches(&id);
    let mark = |on: bool| if on { "on " } else { "off" };
    if let Some(m) = l.db.min_level(&id) {
        println!("  Compression: level {m} or higher (lower levels hang this game on a real PSP; convert raises them)");
    }
    if let Some((g, c)) = l.db.config_game(&id).and_then(|g| g.config.as_ref().map(|c| (g, c))) {
        println!("  [{}] {:<40} fix   {} B, {}", mark(patchdb::config_wanted(def, &[])), patchdb::CONFIG, c.len(), g.source);
    }
    let hits = psx2psp_engine::scan_patches(&disc, ppf.as_deref(), &id, &l.db)?;
    for (p, (_, h)) in l.db.patches_for(&id).iter().zip(&hits) {
        let user = if p.origin == patchdb::Origin::User { "  [user]" } else { "" };
        println!("  [{}] {:<40} {:<5} ×{h}{user}", mark(p.wanted(def, &[])), p.name, p.kind.key());
    }
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
        println!("title    {}", d.title.as_deref().unwrap_or("? (not in game DB)"));
        println!("volume   {}", d.volume_id.as_deref().unwrap_or("?"));
        println!("region   {}", d.region.label());
        println!("tracks   {} ({} audio)", d.tracks, d.audio_tracks);
        println!("size     {} bytes", d.size);
        let img = psx2psp_engine::core::DiscImage::open(p)?;
        println!("md5      {} (track 1, raw)", psx2psp_engine::track1_md5(&img)?);
        if let Some(r) = psx2psp_engine::core::subq::load_beside(p, &img.tracks)? {
            println!("subq     {} ({} sectors, {} damaged)", r.path.display(), r.sectors, r.damaged);
        } else {
            let dumps = d.disc_id.as_deref().map_or(&[][..], psx2psp_engine::sbidb::lookup);
            for s in dumps {
                println!("subq     built-in SBI for {} ({} sectors) — {}, {}", s.serial, s.entries.len(), s.title, s.region);
            }
            if dumps.len() > 1 {
                println!("         {} dumps share this ID: convert uses none, put the .sbi of this dump beside the disc", dumps.len());
            }
        }
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
        if !d.subchannels.is_empty() {
            println!("     subchannel table: {} entries (LibCrypt)", d.subchannels.len() / psx2psp_engine::core::psar::SUBQ_ENTRY);
        }
    }
    Ok(())
}
