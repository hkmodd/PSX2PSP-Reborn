//! psx2psp-sim: runs a PS1 disc on the psxrt core's interpreter (the same C core the PSP runtime
//! links), printing the guest's TTY and the core's log and saving the screen as PNG.
//!
//! ```text
//! psx2psp-sim <disc.cue|disc.bin> [--frames N] [--every N] [--out DIR] [--press FRAME:BUTTON,...]
//!             [--poke FRAME:ADDR:HEX,...] [--dump FRAME,...]
//! ```
//! Buttons: select start up right down left l2 r2 l1 r1 triangle circle cross square; each press
//! holds for 6 frames. A poke writes the bytes `HEX` at the RAM address `ADDR` (hex) before that
//! frame runs, like a cheat code; a dump writes the 2 MB of RAM as `ramNNNNN.bin` after it.

use std::ffi::{c_char, c_int, c_void};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[repr(C)]
struct Sim {
    _opaque: [u8; 0],
}

type ReadFn = extern "C" fn(*mut c_void, u32, *mut u8) -> c_int;
type TextFn = extern "C" fn(*mut c_void, *const c_char, c_int);

unsafe extern "C" {
    fn sim_new(user: *mut c_void, rd: ReadFn, disc_end: u32, tty: TextFn, log: TextFn) -> *mut Sim;
    fn sim_free(s: *mut Sim);
    fn sim_boot(s: *mut Sim) -> c_int;
    fn sim_frame(s: *mut Sim) -> c_int;
    fn sim_vram(s: *const Sim) -> *const u16;
    fn sim_ram(s: *mut Sim) -> *mut u8;
    fn sim_pad(s: *mut Sim, buttons: u16);
    fn sim_display(s: *const Sim, out: *mut c_int);
    fn sim_status(s: *const Sim, out: *mut u32);
    fn sim_trace(s: *mut Sim, flags: c_int);
    #[cfg(test)]
    fn sim_poll_bound(s: *mut Sim, pc: u32, gpr: *const u32, raw: u32, room: u32) -> u32;
}

struct Disc {
    file: File,
    sectors: u32,
}

const SECTOR: u64 = 2352;
const PREGAP: u32 = 150;

extern "C" fn read_sector(user: *mut c_void, lba: u32, raw: *mut u8) -> c_int {
    // SAFETY: `user` is the `Disc` owned by `main` for the simulator's whole life, and `raw` points
    // at a 2352-byte buffer the core owns.
    let disc = unsafe { &mut *(user as *mut Disc) };
    let buf = unsafe { std::slice::from_raw_parts_mut(raw, SECTOR as usize) };
    if lba < PREGAP || lba - PREGAP >= disc.sectors {
        return -1;
    }
    let ok = disc.file.seek(SeekFrom::Start(u64::from(lba - PREGAP) * SECTOR)).is_ok() && disc.file.read_exact(buf).is_ok();
    if ok { 0 } else { -1 }
}

fn text<'a>(s: *const c_char, n: c_int) -> &'a [u8] {
    // SAFETY: the core passes `n` valid bytes at `s`.
    unsafe { std::slice::from_raw_parts(s as *const u8, n.max(0) as usize) }
}

extern "C" fn tty(_user: *mut c_void, s: *const c_char, n: c_int) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text(s, n));
    let _ = out.flush();
}

extern "C" fn log(_user: *mut c_void, s: *const c_char, n: c_int) {
    println!("[psxrt] {}", String::from_utf8_lossy(text(s, n)));
}

fn bin_path(arg: &Path) -> Result<PathBuf, String> {
    if !arg.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue")) {
        return Ok(arg.to_path_buf());
    }
    let cue = std::fs::read_to_string(arg).map_err(|e| format!("{}: {e}", arg.display()))?;
    let name = cue
        .lines()
        .find_map(|l| l.trim().strip_prefix("FILE ").and_then(|r| r.split('"').nth(1)))
        .ok_or("cue sheet without a FILE line")?;
    Ok(arg.with_file_name(name))
}

fn button(name: &str) -> Option<u16> {
    const NAMES: [&str; 16] = [
        "select", "l3", "r3", "start", "up", "right", "down", "left", "l2", "r2", "l1", "r1", "triangle", "circle", "cross", "square",
    ];
    NAMES.iter().position(|n| *n == name).map(|i| 1 << i)
}

struct Args {
    disc: PathBuf,
    frames: u32,
    every: u32,
    out: PathBuf,
    presses: Vec<(u32, u16)>,
    pokes: Vec<(u32, u32, Vec<u8>)>,
    dumps: Vec<u32>,
}

const RAM: usize = 2 << 20;

/// `FRAME:ADDR:HEX` with ADDR in hex (KSEG0/1 or physical), HEX an even run of hex digits.
fn poke(p: &str) -> Result<(u32, u32, Vec<u8>), String> {
    let bad = || format!("bad poke {p}");
    let mut it = p.split(':');
    let (f, a, h) = (it.next().ok_or_else(bad)?, it.next().ok_or_else(bad)?, it.next().ok_or_else(bad)?);
    let f = f.parse::<u32>().map_err(|_| bad())?;
    let a = u32::from_str_radix(a.trim_start_matches("0x"), 16).map_err(|_| bad())? & 0x1F_FFFF;
    if h.is_empty() || h.len() % 2 != 0 || a as usize + h.len() / 2 > RAM {
        return Err(bad());
    }
    let bytes = (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16)).collect::<Result<Vec<_>, _>>().map_err(|_| bad())?;
    Ok((f, a, bytes))
}

fn args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        disc: PathBuf::new(),
        frames: 600,
        every: 60,
        out: PathBuf::from("D:/Temp/PSX2PSP-Reborn/sim"),
        presses: Vec::new(),
        pokes: Vec::new(),
        dumps: Vec::new(),
    };
    let num = |v: Option<String>, what: &str| v.and_then(|s| s.parse::<u32>().ok()).ok_or(format!("{what} needs a number"));
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => a.frames = num(it.next(), "--frames")?,
            "--every" => a.every = num(it.next(), "--every")?,
            "--out" => a.out = PathBuf::from(it.next().ok_or("--out needs a directory")?),
            "--press" => {
                for p in it.next().ok_or("--press needs FRAME:BUTTON,...")?.split(',') {
                    let (f, b) = p.split_once(':').ok_or(format!("bad press {p}"))?;
                    let f = f.parse::<u32>().map_err(|_| format!("bad frame in {p}"))?;
                    a.presses.push((f, button(b).ok_or(format!("unknown button {b}"))?));
                }
            }
            "--poke" => {
                for p in it.next().ok_or("--poke needs FRAME:ADDR:HEX,...")?.split(',') {
                    a.pokes.push(poke(p)?);
                }
            }
            "--dump" => {
                for f in it.next().ok_or("--dump needs FRAME,...")?.split(',') {
                    a.dumps.push(f.parse::<u32>().map_err(|_| format!("bad dump frame {f}"))?);
                }
            }
            _ if a.disc.as_os_str().is_empty() => a.disc = PathBuf::from(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    if a.disc.as_os_str().is_empty() {
        return Err("usage: psx2psp-sim <disc.cue|disc.bin> [--frames N] [--every N] [--out DIR] [--press FRAME:BUTTON,...]".into());
    }
    Ok(a)
}

/// The display area as RGB8, reading 15-bit or 24-bit VRAM.
fn screen(vram: &[u16], d: &[c_int; 6]) -> image::RgbImage {
    let (x0, y0, w, h, rgb24) = (d[0] as usize, d[1] as usize, d[2] as u32, d[3] as u32, d[4] != 0);
    image::RgbImage::from_fn(w, h, |x, y| {
        let row = (y0 + y as usize) & 511;
        if rgb24 {
            let byte = x0 * 2 + x as usize * 3;
            let b = |i: usize| {
                let px = vram[row * 1024 + ((i / 2) & 1023)];
                if i & 1 == 1 { (px >> 8) as u8 } else { px as u8 }
            };
            image::Rgb([b(byte), b(byte + 1), b(byte + 2)])
        } else {
            let px = vram[row * 1024 + ((x0 + x as usize) & 1023)];
            let c = |s: u16| (((px >> s) & 31) << 3 | ((px >> s) & 31) >> 2) as u8;
            image::Rgb([c(0), c(5), c(10)])
        }
    })
}

fn vram_image(vram: &[u16]) -> image::RgbImage {
    image::RgbImage::from_fn(1024, 512, |x, y| {
        let px = vram[y as usize * 1024 + x as usize];
        let c = |s: u16| (((px >> s) & 31) << 3) as u8;
        image::Rgb([c(0), c(5), c(10)])
    })
}

fn main() -> ExitCode {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let bin = match bin_path(&a.disc) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let file = match File::open(&bin) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}: {e}", bin.display());
            return ExitCode::from(2);
        }
    };
    let sectors = (file.metadata().map(|m| m.len()).unwrap_or(0) / SECTOR) as u32;
    if let Err(e) = std::fs::create_dir_all(&a.out) {
        eprintln!("{}: {e}", a.out.display());
        return ExitCode::from(2);
    }
    let mut disc = Box::new(Disc { file, sectors });
    let user = &mut *disc as *mut Disc as *mut c_void;
    // SAFETY: `disc` outlives the simulator, which is freed before main returns.
    let sim = unsafe { sim_new(user, read_sector, sectors + PREGAP, tty, log) };
    if sim.is_null() {
        eprintln!("out of memory");
        return ExitCode::FAILURE;
    }
    let started = std::time::Instant::now();
    let mut code = ExitCode::SUCCESS;
    // SAFETY: `sim` is valid until sim_free below; VRAM is 1024x512 halfwords.
    unsafe {
        // PSXRT_TRACE: bit 0 BIOS calls, bit 1 CD commands, bit 2 DMA.
        let trace = std::env::var("PSXRT_TRACE").ok().and_then(|v| v.parse::<c_int>().ok()).unwrap_or(0);
        sim_trace(sim, trace);
        if sim_boot(sim) != 0 {
            eprintln!("boot failed");
            sim_free(sim);
            return ExitCode::FAILURE;
        }
        let vram = std::slice::from_raw_parts(sim_vram(sim), 1024 * 512);
        let ram = sim_ram(sim);
        let mut held: Vec<(u32, u16)> = Vec::new();
        for frame in 1..=a.frames {
            for (_, addr, bytes) in a.pokes.iter().filter(|p| p.0 == frame) {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ram.add(*addr as usize), bytes.len());
                println!("[sim] frame {frame} poke {addr:06X} {} B", bytes.len());
            }
            if a.dumps.contains(&frame.wrapping_sub(1)) && frame > 1 {
                let path = a.out.join(format!("ram{:05}.bin", frame - 1));
                if let Err(e) = std::fs::write(&path, std::slice::from_raw_parts(ram, RAM)) {
                    eprintln!("{}: {e}", path.display());
                }
            }
            for &(f, b) in &a.presses {
                if f == frame {
                    held.push((frame + 6, b));
                }
            }
            held.retain(|&(until, _)| frame < until);
            sim_pad(sim, !held.iter().fold(0u16, |m, &(_, b)| m | b));
            let exit = sim_frame(sim);
            let last = exit != 0 || frame == a.frames;
            if (a.every != 0 && frame % a.every == 0) || last {
                let mut d = [0 as c_int; 6];
                sim_display(sim, d.as_mut_ptr());
                let mut st = [0u32; 4];
                sim_status(sim, st.as_mut_ptr());
                let path = a.out.join(format!("frame{frame:05}.png"));
                if let Err(e) = screen(vram, &d).save(&path) {
                    eprintln!("{}: {e}", path.display());
                }
                println!(
                    "[sim] frame {frame} pc {:08X} {} Mcycles imask {:03X} display {}x{} at {},{}{}{}",
                    st[0],
                    st[1],
                    st[3],
                    d[2],
                    d[3],
                    d[0],
                    d[1],
                    if d[4] != 0 { " 24-bit" } else { "" },
                    if d[5] != 0 { "" } else { " (off)" }
                );
            }
            if exit != 0 {
                println!("[sim] guest stopped ({})", if exit == 1 { "exit" } else { "fatal" });
                code = ExitCode::FAILURE;
                break;
            }
        }
        let path = a.out.join("vram.png");
        if let Err(e) = vram_image(vram).save(&path) {
            eprintln!("{}: {e}", path.display());
        }
        sim_free(sim);
    }
    println!("[sim] {:.1} s wall, output in {}", started.elapsed().as_secs_f64(), a.out.display());
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn no_disc(_user: *mut c_void, _lba: u32, _raw: *mut u8) -> c_int {
        -1
    }

    /// Sheep Dog 'n' Wolf (SLES_028.95): the wait for the next streamed frame, and GetRCnt.
    const WAIT: (u32, [u32; 36]) = (
        0x8010_1738,
        [
            0x27BDFFD8, 0xAFBF0018, 0xAFB10014, 0x3C02F200, 0xAFB00010, 0x34440001, 0x3405FFFF, 0x24062000,
            0x0C0407E4, 0x3C100080, 0x3C02F200, 0x0C040819, 0x34440001, 0x3C02F200, 0x0C04080B, 0x34440001,
            0x00408821, 0x3C02F200, 0x0C04080B, 0x34440001, 0x00511023, 0x28412711, 0x14200003, 0x00000000,
            0x10000044, 0x00001021, 0x2610FFFF, 0x16000003, 0x00000000, 0x1000003F, 0x00001021, 0x27A50024,
            0x0C040C4C, 0x27A40020, 0x1440FFEE, 0x00000000,
        ],
    );
    const GET_RCNT: (u32, [u32; 14]) = (
        0x8010_202C,
        [
            0x3083FFFF, 0x28620003, 0x10400008, 0x00031900, 0x3C028012, 0x8C4284F4, 0x00000000, 0x00621821,
            0x94620000, 0x08040817, 0x00000000, 0x00001021, 0x03E00008, 0x00000000,
        ],
    );
    const LHU: u32 = 0x8010_204C; // GetRCnt's read of the counter

    /// poll_bound on `code` placed in RAM, with s1 = `t0` and ra = `ra`.
    fn bound(code: &[(u32, &[u32])], ra: u32, t0: u32, pc: u32, raw: u32) -> u32 {
        // SAFETY: the simulator lives for this call; RAM is RAM_SIZE bytes and every write is in it.
        unsafe {
            let s = sim_new(std::ptr::null_mut(), no_disc, 0, tty, log);
            let ram = std::slice::from_raw_parts_mut(sim_ram(s), 0x20_0000);
            for &(at, words) in code {
                for (i, w) in words.iter().enumerate() {
                    let p = (at & 0x1F_FFFF) as usize + 4 * i;
                    ram[p..p + 4].copy_from_slice(&w.to_le_bytes());
                }
            }
            let mut gpr = [0u32; 32];
            gpr[17] = t0;
            gpr[31] = ra;
            let t = sim_poll_bound(s, pc, gpr.as_ptr(), raw, 0xFFFF - raw);
            sim_free(s);
            t
        }
    }

    #[test]
    fn the_stream_wait_jumps_to_its_timeout() {
        let code: [(u32, &[u32]); 2] = [(WAIT.0, &WAIT.1), (GET_RCNT.0, &GET_RCNT.1)];
        // GetRCnt() - t0 > 10000 leaves the loop: from 1100 with t0 = 1000, at 11001.
        assert_eq!(bound(&code, 0x8010_1788, 1000, LHU, 1100), 9901);
        // A timeout past the counter's wrap: up to the wrap.
        assert_eq!(bound(&code, 0x8010_1788, 60000, LHU, 60100), 0xFFFF - 60100);
        // At the timeout already: the comparison does not flip again before the wrap.
        assert_eq!(bound(&code, 0x8010_1788, 1000, LHU, 12000), 0xFFFF - 12000);
        // The read of t0 (kept in s1 across a call): one tick at a time.
        assert_eq!(bound(&code, 0x8010_1778, 1000, LHU, 1100), 0);
    }

    #[test]
    fn exact_values_and_delay_slots_move_one_tick() {
        let at = 0x8001_0000;
        let nop = 0u32;
        // while (*cnt != v1);
        let equal: [(u32, &[u32]); 1] = [(at - 4, &[nop, 0x9482_0000, nop, 0x1443_FFFD, nop])];
        assert_eq!(bound(&equal, 0, 0, at, 100), 0);
        // do { a = *cnt; b = *cnt; } while (a != b);  (VSync's stable read)
        let pair: [(u32, &[u32]); 1] = [(at - 4, &[nop, 0x9482_0000, 0x9483_0000, nop, 0x1443_FFFC, nop])];
        assert_eq!(bound(&pair, 0, 0, at, 100), 0);
        // The read in a branch's delay slot.
        let slot: [(u32, &[u32]); 1] = [(at - 4, &[0x1000_0003, 0x9482_0000, nop, 0x2842_0010, nop])];
        assert_eq!(bound(&slot, 0, 0, at, 100), 0);
        // The same comparison without the branch: x < 16 flips at 16.
        let less: [(u32, &[u32]); 1] = [(at - 4, &[nop, 0x9482_0000, nop, 0x2842_0010, nop, 0x0000_1021])];
        assert_eq!(bound(&less, 0, 0, at, 10), 6);
    }
}
