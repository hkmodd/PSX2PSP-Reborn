//! Code an executable unpacks into RAM before it runs it, unpacked without running the game.
//!
//! A packed executable (UPX's ps1/exe, a demo disc's cruncher, a game's own LZ stub) keeps its
//! code compressed in its text and starts at an unpacker that writes the code to RAM, flushes the
//! cache (BIOS A0:44) and jumps in, sometimes after moving itself out of the way first. The disc
//! text is the stub and noise; the code that runs is what the stub leaves.
//!
//! The executable is run on the R3000A interpreter twice, in lockstep, from its entry: the text
//! loaded as the BIOS loads it into an otherwise zero RAM, `gp`, `sp` and `fp` as the BIOS sets
//! them, every other register at a different random value in each run ([`crate::decipher`]'s).
//! While the paths and stores of both runs agree, what they write reads nothing but the text and
//! constants: it is what the game's own boot writes. The run follows execution into the words it
//! wrote and ends at the first trap, access outside RAM and scratchpad, BIOS call other than
//! FlushCache, or parting of the runs. What it wrote counts only when it ran some of it: an
//! ordinary start-up (clear the BSS, call the BIOS) writes but runs nothing it wrote.
//!
//! As with [`crate::decipher`], the result is a guess the runtime checks: its code is translated
//! as floating units, placed only where RAM holds their words.

use crate::decipher::cpu;
use crate::exe::PsxExe;
use crate::interp::{Bus, Cpu, Size, Trap};

/// Bytes of PS1 RAM.
const RAM: usize = 0x20_0000;
/// Physical addresses below it are the BIOS's and kernel's: a jump there is a BIOS call.
const KERNEL: u32 = 0x1_0000;
/// Most steps of one run.
const STEPS_MAX: u64 = 1 << 26;
/// The BIOS A0 function that flushes the I-cache: the one call an unpacker makes before it jumps.
const FLUSH_CACHE: u32 = 0x44;
/// The return address of the BIOS's Exec: the ROM, which the run never maps.
const BIOS_RA: u32 = 0xBFC0_0000;
/// `sp` when the header names no stack (psx-spx, BIOS Exec: the boot executable's default).
const SP: u32 = 0x801F_FF00;

/// What [`unpacked`] finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// The RAM as the run left it (offset 0 = physical address 0).
    pub ram: Vec<u8>,
    /// The words it wrote from [`KERNEL`] up: [start, end) physical addresses, in order, apart.
    pub spans: Vec<(u32, u32)>,
    /// Where execution went into words it wrote, from words it did not or by a jump or a branch
    /// taken, as the code addressed them; in order, once each.
    pub entries: Vec<u32>,
}

/// RAM and scratchpad as the only memory; the step's store (compared between the two runs) and
/// the bytes it wrote over.
struct Mem<'a> {
    ram: &'a mut [u8],
    scratch: &'a mut [u8; 0x400],
    stored: Option<(u32, u32, u32)>,
    over: Option<(bool, usize, Vec<u8>)>,
}

/// The physical RAM offset of `addr` in KUSEG, KSEG0 or KSEG1, when `size` bytes fit there.
fn in_ram(addr: u32, size: usize) -> Option<usize> {
    let p = (addr & 0x1FFF_FFFF) as usize;
    (addr < 0xC000_0000 && !(0x2000_0000..0x8000_0000).contains(&addr) && p + size <= RAM).then_some(p)
}

impl Mem<'_> {
    fn place(&mut self, addr: u32, size: Size) -> Result<(bool, usize), Trap> {
        let n = size as usize;
        if let Some(p) = in_ram(addr, n) {
            return Ok((false, p));
        }
        // The scratchpad, in KUSEG and KSEG0 only.
        let s = (addr & 0x7FFF_FFFF).wrapping_sub(0x1F80_0000) as usize;
        if addr < 0xA000_0000 && s + n <= 0x400 { Ok((true, s)) } else { Err(Trap::Bus { addr }) }
    }

    fn bytes(&mut self, scratch: bool) -> &mut [u8] {
        if scratch { &mut self.scratch[..] } else { self.ram }
    }
}

impl Bus for Mem<'_> {
    fn load(&mut self, addr: u32, size: Size) -> Result<u32, Trap> {
        let (s, o) = self.place(addr, size)?;
        Ok(self.bytes(s)[o..o + size as usize].iter().rev().fold(0, |v, &b| v << 8 | u32::from(b)))
    }

    fn store(&mut self, addr: u32, size: Size, value: u32) -> Result<(), Trap> {
        let (s, o) = self.place(addr, size)?;
        let n = size as usize;
        let b = self.bytes(s);
        let old = b[o..o + n].to_vec();
        b[o..o + n].copy_from_slice(&value.to_le_bytes()[..n]);
        self.over = Some((s, o, old));
        self.stored = Some((addr, n as u32, value));
        Ok(())
    }

    fn fetch(&mut self, addr: u32) -> Result<u32, Trap> {
        let o = in_ram(addr, 4).ok_or(Trap::Bus { addr })?;
        Ok(u32::from_le_bytes(self.ram[o..o + 4].try_into().unwrap()))
    }
}

/// A run's CPU at `exe`'s entry: the registers the BIOS's Exec sets, the rest from `seed`.
fn boot(exe: &PsxExe, seed: u64) -> Cpu {
    let mut c = cpu(exe.pc0, seed);
    let sp = if exe.stack.0 != 0 { exe.stack.0.wrapping_add(exe.stack.1) } else { SP };
    (c.gpr[28], c.gpr[29], c.gpr[30], c.gpr[31]) = (exe.gp0, sp, sp, BIOS_RA);
    c
}

/// The code `exe` unpacks into RAM and runs before anything else, or None when its start runs
/// none of what it writes.
pub fn unpacked(exe: &PsxExe) -> Option<Unpacked> {
    let base = in_ram(exe.text_addr, exe.text.len())?;
    if base < KERNEL as usize || exe.text_addr & 3 != 0 {
        return None;
    }
    let mut ra = vec![0u8; RAM];
    ra[base..base + exe.text.len()].copy_from_slice(&exe.text);
    let mut rb = ra.clone();
    let (mut sa, mut sb) = ([0u8; 0x400], [0u8; 0x400]);
    let mut written = vec![false; RAM / 4];
    let (mut a, mut b) = (boot(exe, 1), boot(exe, 2));
    let mut entries = Vec::new();
    let (mut inside, mut arrived) = (false, false);
    for _ in 0..STEPS_MAX {
        let pc = a.pc;
        if let Some(p) = in_ram(pc, 4).filter(|&p| p < KERNEL as usize) {
            // A BIOS call (j/jalr to A0, B0 or C0): FlushCache returns, any other ends the run.
            let back = (a.gpr[31] == b.gpr[31]).then_some(a.gpr[31]);
            match back {
                Some(ret) if p == 0xA0 && a.next_pc == pc + 4 && a.gpr[9] == FLUSH_CACHE && b.gpr[9] == FLUSH_CACHE => {
                    a.jump(ret);
                    b.jump(ret);
                    arrived = false;
                    continue;
                }
                _ => break,
            }
        }
        let here = in_ram(pc, 4).is_some_and(|p| written[p / 4]);
        if here && (!inside || arrived) {
            entries.push(pc);
        }
        inside = here;
        let jumping = a.next_pc != pc.wrapping_add(4);
        let mut ma = Mem { ram: &mut ra, scratch: &mut sa, stored: None, over: None };
        if a.step(&mut ma).is_err() {
            break;
        }
        let (st, over) = (ma.stored, ma.over);
        let mut mb = Mem { ram: &mut rb, scratch: &mut sb, stored: None, over: None };
        if b.step(&mut mb).is_err() || (a.pc, a.next_pc, st) != (b.pc, b.next_pc, mb.stored) {
            if let Some((s, o, old)) = over {
                let t: &mut [u8] = if s { &mut sa[..] } else { &mut ra[..] };
                t[o..o + old.len()].copy_from_slice(&old);
            }
            break;
        }
        if let Some((addr, n, _)) = st
            && let Some(p) = in_ram(addr, n as usize)
        {
            (p / 4..(p + n as usize).div_ceil(4)).for_each(|w| written[w] = true);
        }
        arrived = jumping;
    }
    if entries.is_empty() {
        return None;
    }
    entries.sort_unstable();
    entries.dedup();
    let mut spans: Vec<(u32, u32)> = Vec::new();
    for w in (KERNEL as usize / 4..RAM / 4).filter(|&w| written[w]) {
        let at = 4 * w as u32;
        match spans.last_mut() {
            Some(s) if s.1 == at => s.1 = at + 4,
            _ => spans.push((at, at + 4)),
        }
    }
    Some(Unpacked { ram: ra, spans, entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOP: u32 = 0;
    const T: u32 = 0x8001_0000;
    const DST: u32 = 0x8010_0000;
    const ZERO: u32 = 0;
    const A0: u32 = 4;
    const A1: u32 = 5;
    const A2: u32 = 6;
    const A3: u32 = 7;
    const T0: u32 = 8;
    const T1: u32 = 9;
    const T2: u32 = 10;
    const T3: u32 = 11;
    const T4: u32 = 12;
    const SPR: u32 = 29;
    const RA: u32 = 31;

    fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
        op << 26 | rs << 21 | rt << 16 | (imm & 0xFFFF)
    }
    fn r(rs: u32, rt: u32, rd: u32, f: u32) -> u32 {
        rs << 21 | rt << 16 | rd << 11 | f
    }
    /// `lui`/`addiu` of `x` into `rt`.
    fn li(rt: u32, x: u32) -> [u32; 2] {
        [i(0x0F, 0, rt, (x >> 16) + (x >> 15 & 1)), i(0x09, rt, rt, x)]
    }
    fn bios(f: u32) -> [u32; 3] {
        [i(0x09, ZERO, T2, 0xA0), r(T2, 0, RA, 9), i(0x09, ZERO, T1, f)]
    }
    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
    fn exe(text: Vec<u8>, pc0: u32) -> PsxExe {
        PsxExe { pc0, gp0: 0, text_addr: T, text, bss: (0, 0), stack: (0, 0) }
    }

    /// An LZ stream: a byte `c` below 0x80 takes the next `c + 1` bytes as they are; otherwise
    /// `(c & 0x7F) + 3` bytes from a 16-bit little-endian distance back. Greedy, test-only.
    fn lz(data: &[u8]) -> Vec<u8> {
        let (mut out, mut lit, mut i) = (Vec::new(), Vec::new(), 0);
        let flush = |out: &mut Vec<u8>, lit: &mut Vec<u8>| {
            lit.chunks(128).for_each(|c| {
                out.push(c.len() as u8 - 1);
                out.extend(c);
            });
            lit.clear();
        };
        while i < data.len() {
            let (mut best, mut off) = (0, 0);
            for j in i.saturating_sub(0xFFFF)..i {
                let n = (0..130.min(data.len() - i)).take_while(|&k| data[j + k] == data[i + k]).count();
                if n > best {
                    (best, off) = (n, i - j);
                }
            }
            if best >= 3 {
                flush(&mut out, &mut lit);
                out.push(0x80 | (best - 3) as u8);
                out.extend((off as u16).to_le_bytes());
                i += best;
            } else {
                lit.push(data[i]);
                i += 1;
            }
        }
        flush(&mut out, &mut lit);
        out
    }

    /// The unpacker of [`lz`]: `src..end` to `dst`, FlushCache, `jr entry`. Position independent.
    fn unlz(src: u32, end: u32, dst: u32, entry: u32) -> Vec<u32> {
        let mut w = Vec::new();
        w.extend(li(A0, src));
        w.extend(li(A1, end));
        w.extend(li(A2, dst));
        w.extend(li(A3, entry));
        w.extend([
            i(0x24, A0, T0, 0), // 8 loop: lbu t0,0(a0)
            i(0x09, A0, A0, 1),
            i(0x0C, T0, T1, 0x80),
            i(0x05, T1, ZERO, 10), // bnez t1, match
            i(0x0C, T0, T2, 0x7F),
            i(0x09, T2, T2, 1),
            i(0x24, A0, T3, 0), // 14 lit: lbu t3,0(a0)
            i(0x09, A0, A0, 1),
            i(0x28, A2, T3, 0),
            i(0x09, T2, T2, 0xFFFF),
            i(0x05, T2, ZERO, 0xFFFB), // bnez t2, lit
            i(0x09, A2, A2, 1),
            i(0x04, ZERO, ZERO, 14), // b next
            NOP,
            i(0x09, T2, T2, 3), // 22 match
            i(0x24, A0, T3, 0),
            i(0x24, A0, T4, 1),
            i(0x09, A0, A0, 2),
            T4 << 16 | T4 << 11 | 8 << 6, // sll t4,t4,8
            r(T3, T4, T3, 0x25),
            r(A2, T3, T3, 0x23), // subu t3,a2,t3
            i(0x24, T3, T4, 0), // 29 cp: lbu t4,0(t3)
            i(0x09, T3, T3, 1),
            i(0x28, A2, T4, 0),
            i(0x09, T2, T2, 0xFFFF),
            i(0x05, T2, ZERO, 0xFFFB), // bnez t2, cp
            i(0x09, A2, A2, 1),
            r(A0, A1, T0, 0x2B), // 35 next: sltu t0,a0,a1
            i(0x05, T0, ZERO, 0xFFE3), // bnez t0, loop
            NOP,
        ]);
        w.extend(bios(FLUSH_CACHE));
        w.extend([r(A3, 0, 0, 8), NOP]);
        w
    }

    /// The unpacked program: 32 functions at `at`, each calling the next with its number, the
    /// last calling the BIOS's InitHeap (the run ends there).
    fn program(at: u32) -> Vec<u32> {
        let mut w = Vec::new();
        for k in 0..32u32 {
            let next = at + 4 * (w.len() as u32 + 8);
            let jal = 3 << 26 | (next >> 2 & 0x03FF_FFFF);
            w.extend([i(0x09, SPR, SPR, 0xFFE0), i(0x2B, SPR, RA, 0x1C), jal, i(0x09, ZERO, A0, k)]);
            w.extend([i(0x23, SPR, RA, 0x1C), NOP, r(RA, 0, 0, 8), i(0x09, SPR, SPR, 0x20)]);
        }
        w.extend(bios(0x39));
        w.extend([r(RA, 0, 0, 8), NOP]);
        w
    }

    /// A packed executable: stub at `T`, the stream after it; the stub at `stub` (moved there by
    /// `prefix` when it is not `T`).
    fn packed(prefix: &dyn Fn(u32, u32) -> Vec<u32>, plain: &[u8]) -> (PsxExe, u32) {
        let stream = lz(plain);
        let pre = prefix(0, 0).len() as u32;
        let src = T + 4 * (pre + unlz(0, 0, 0, 0).len() as u32);
        let stub = unlz(src, src + stream.len() as u32, DST, DST);
        let at = T + 4 * pre;
        let mut w = prefix(at, stub.len() as u32);
        let to = if w.is_empty() { at } else { MOVED };
        w.extend(stub);
        let mut text = bytes(&w);
        text.extend(&stream);
        text.resize(text.len().next_multiple_of(4), 0);
        (exe(text, T), to)
    }

    const MOVED: u32 = 0x801E_0000;

    /// Copies `n` words from `from` to [`MOVED`] and jumps there.
    fn mover(from: u32, n: u32) -> Vec<u32> {
        let mut w = Vec::new();
        w.extend(li(T0, from));
        w.extend(li(T1, MOVED));
        w.push(i(0x09, ZERO, T2, n));
        w.extend([
            i(0x23, T0, T3, 0), // cl: lw t3,0(t0)
            i(0x09, T0, T0, 4),
            i(0x09, T2, T2, 0xFFFF),
            i(0x2B, T1, T3, 0),
            i(0x05, T2, ZERO, 0xFFFB), // bnez t2, cl
            i(0x09, T1, T1, 4),
        ]);
        w.extend(li(T4, MOVED));
        w.extend([r(T4, 0, 0, 8), NOP]);
        w
    }

    fn word(ram: &[u8], a: u32) -> u32 {
        let o = (a & 0x1FFF_FFFF) as usize;
        u32::from_le_bytes(ram[o..o + 4].try_into().unwrap())
    }

    #[test]
    fn an_lz_packed_executable_is_unpacked_and_entered() {
        let plain = program(DST);
        let (x, _) = packed(&|_, _| Vec::new(), &bytes(&plain));
        assert!(lz(&bytes(&plain)).len() < plain.len() * 4 / 2, "the stream compresses");
        let u = unpacked(&x).expect("unpacked");
        assert!((0..plain.len() as u32).all(|k| word(&u.ram, DST + 4 * k) == plain[k as usize]));
        let d = DST & 0x1FFF_FFFF;
        assert!(u.spans.iter().any(|&(s, e)| s <= d && d + 4 * plain.len() as u32 <= e), "{:x?}", u.spans);
        assert_eq!(u.entries[0], DST);
        assert!(u.entries.iter().all(|&e| (DST..DST + 4 * plain.len() as u32).contains(&e)), "{:x?}", u.entries);
    }

    /// A stub that first moves itself to high RAM: both the moved stub and the code it unpacks
    /// are entered.
    #[test]
    fn a_stub_that_moves_itself_is_followed() {
        let plain = program(DST);
        let (x, to) = packed(&mover, &bytes(&plain));
        assert_eq!(to, MOVED);
        let u = unpacked(&x).expect("unpacked");
        assert!((0..plain.len() as u32).all(|k| word(&u.ram, DST + 4 * k) == plain[k as usize]));
        assert!(u.entries.contains(&MOVED) && u.entries.contains(&DST), "{:x?}", u.entries);
    }

    /// An ordinary start-up writes (it clears its BSS) but runs nothing it wrote; an unpacker
    /// whose stream address comes from a register nothing set never agrees with itself.
    #[test]
    fn a_plain_start_or_an_unknown_stream_unpacks_nothing() {
        let mut w = Vec::new();
        w.extend(li(T0, 0x8008_0000));
        w.extend(li(T1, 0x8008_4000));
        w.extend([i(0x2B, T0, ZERO, 0), i(0x09, T0, T0, 4), r(T0, T1, T2, 0x2B), i(0x05, T2, ZERO, 0xFFFC), NOP]);
        w.extend(bios(0x39));
        assert_eq!(unpacked(&exe(bytes(&w), T)), None);
        let plain = program(DST);
        let (mut x, _) = packed(&|_, _| Vec::new(), &bytes(&plain));
        x.text[..8].copy_from_slice(&[0; 8]); // the stream's address: a0 as the caller left it
        assert_eq!(unpacked(&x), None);
    }
}
