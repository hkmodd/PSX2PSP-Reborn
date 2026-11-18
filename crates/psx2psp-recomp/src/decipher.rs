//! Code a game deciphers in place before it runs it, deciphered without running the game.
//!
//! Some games keep code ciphered in their own text and decipher it with a loop of constants just
//! before they jump in: Spyro 2 (SCES-02104) hides its LibCrypt check at 8006A0B8 in the
//! executable (196 words, XOR with a key that steps `key = 0x16D16348 - key`, deciphered at
//! 8006A3C8, then `jr 8006A0F8`), and 20 words at 80082738 in a level overlay the same way. The
//! disc text there is noise; the code that runs is what the loop leaves.
//!
//! Each loop of the text that loads and stores in it is run on the R3000A interpreter twice, in
//! lockstep, from the straight code before its head, with every register at a different random
//! value in each run and the text as the only memory. A loop whose path and stores are the same
//! in both runs reads nothing but its constants and the text: what it leaves is what the game's
//! own run leaves. The run goes on past the loop's exit to the jump that follows (the entry into
//! what it deciphered). Deciphered code that holds such a loop in turn is deciphered again.
//!
//! The result is a guess the runtime checks: its code is translated as floating units, which the
//! runtime places only where RAM holds their words.

use crate::exe::PsxExe;
use crate::insn::{Insn, Isa, Kind, decode};
use crate::interp::{Bus, Cpu, Size, Trap};

/// Longest loop looked at: words from its head to its branch.
const LOOP_MAX: u32 = 64;
/// Most straight words before a loop's head run with it: where it sets its constants.
const LEAD_MAX: u32 = 32;
/// Most words after a loop's branch run: to the jump into what it deciphered.
const TAIL_MAX: u32 = 16;
/// Most steps of one run.
const STEPS_MAX: u32 = 1 << 20;
/// Rounds of deciphering: code a round deciphers is looked at in the next.
const ROUNDS: usize = 4;
/// Fewest words a loop changes to count.
const MIN_WORDS: usize = 4;

/// What [`deciphered`] finds in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deciphered {
    /// The text as the loops leave it.
    pub text: Vec<u8>,
    /// The words they changed: [start, end) addresses, in order, apart.
    pub spans: Vec<(u32, u32)>,
    /// Where the code after a loop jumps into what it changed.
    pub entries: Vec<u32>,
}

/// The text as the only memory; the step's store (compared between the two runs) and the bytes
/// it wrote over, at their offset.
struct Text<'a> {
    at: u32,
    bytes: &'a mut [u8],
    stored: Option<(u32, u32, u32)>,
    over: Option<(usize, Vec<u8>)>,
}

impl Text<'_> {
    fn offset(&self, addr: u32, size: Size) -> Result<usize, Trap> {
        let o = (addr & 0x1FFF_FFFF).wrapping_sub(self.at & 0x1FFF_FFFF) as usize;
        if o.checked_add(size as usize).is_some_and(|e| e <= self.bytes.len()) { Ok(o) } else { Err(Trap::Bus { addr }) }
    }
}

impl Bus for Text<'_> {
    fn load(&mut self, addr: u32, size: Size) -> Result<u32, Trap> {
        let o = self.offset(addr, size)?;
        Ok(self.bytes[o..o + size as usize].iter().rev().fold(0, |v, &b| v << 8 | u32::from(b)))
    }

    fn store(&mut self, addr: u32, size: Size, value: u32) -> Result<(), Trap> {
        let o = self.offset(addr, size)?;
        self.over = Some((o, self.bytes[o..o + size as usize].to_vec()));
        self.bytes[o..o + size as usize].copy_from_slice(&value.to_le_bytes()[..size as usize]);
        self.stored = Some((addr, size as u32, value));
        Ok(())
    }
}

/// A run's registers: none zero but `zero`, different for each `seed`.
pub(crate) fn cpu(pc: u32, seed: u64) -> Cpu {
    let mut s = seed;
    let mut next = || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as u32 | 1
    };
    let mut c = Cpu::new(Isa::R3000A, pc);
    for r in 1..32 {
        c.gpr[r] = next();
    }
    (c.hi, c.lo) = (next(), next());
    c
}

/// Runs `text` (at `at`) from `start` while the pc stays in [start, the branch at `branch` and
/// TAIL_MAX words past it). The text as left and where the run went next, or the pc where the
/// two runs failed or parted. Past the loop's exit, a failure or a parting ends the run there:
/// what the loop left stands, and the code there (deciphered code the run fell into) is where
/// it went.
fn run(text: &[u8], at: u32, start: u32, branch: u32) -> Result<(Vec<u8>, u32), u32> {
    let end = branch + 8 + 4 * TAIL_MAX;
    let (mut ta, mut tb) = (text.to_vec(), text.to_vec());
    let (mut a, mut b) = (cpu(start, 1), cpu(start, 2));
    let mut out = false;
    for _ in 0..STEPS_MAX {
        let pc = a.pc;
        let slot = a.next_pc != pc.wrapping_add(4);
        out |= !slot && pc == branch + 8;
        if !slot && !(start..end).contains(&pc) {
            return Ok((ta, pc));
        }
        let mut ma = Text { at, bytes: &mut ta, stored: None, over: None };
        if a.step(&mut ma).is_err() {
            return if out { Ok((ta, pc)) } else { Err(pc) };
        }
        let (sa, over) = (ma.stored, ma.over);
        let mut mb = Text { at, bytes: &mut tb, stored: None, over: None };
        if b.step(&mut mb).is_err() || (a.pc, a.next_pc, sa) != (b.pc, b.next_pc, mb.stored) {
            if !out {
                return Err(pc);
            }
            if let Some((o, old)) = over {
                ta[o..o + old.len()].copy_from_slice(&old);
            }
            return Ok((ta, pc));
        }
    }
    Err(a.pc)
}

fn kind(w: u32) -> Kind {
    decode(Insn(w), Isa::R3000A)
}

/// Words that may set a loop's constants: no jump, nothing that traps by its kind.
fn straight(k: Kind) -> bool {
    !k.has_delay_slot() && !matches!(k, Kind::Syscall | Kind::Break | Kind::Invalid | Kind::Rfe | Kind::Mtc0 | Kind::Gte | Kind::Mtc2 | Kind::Ctc2 | Kind::Lwc2 | Kind::Swc2)
}

/// The loops of `text` (at `at`) whose branch is in [lo, hi): (head, branch). A loop is a
/// conditional branch back at most LOOP_MAX words, with a load and a store between its head and
/// its delay slot and no other jump.
fn loops(text: &[u8], at: u32, lo: u32, hi: u32) -> Vec<(u32, u32)> {
    let word = |a: u32| a.checked_sub(at).and_then(|o| text.get(o as usize..o as usize + 4)).map(|w| u32::from_le_bytes(w.try_into().unwrap()));
    let mut out = Vec::new();
    for b in (lo.max(at)..hi.min(at + text.len() as u32 & !3)).step_by(4) {
        let w = word(b).unwrap_or(0);
        if !matches!(kind(w), Kind::Beq | Kind::Bne | Kind::Blez | Kind::Bgtz | Kind::Bltz | Kind::Bgez) {
            continue;
        }
        let head = Insn(w).branch_target(b);
        if head > b || b - head > 4 * LOOP_MAX || head < at {
            continue;
        }
        let body: Vec<Kind> = (head..=b + 4).step_by(4).filter_map(word).map(kind).collect();
        let jumps = body.iter().filter(|k| k.has_delay_slot()).count();
        let loads = body.iter().any(|k| matches!(k, Kind::Lw | Kind::Lh | Kind::Lhu | Kind::Lb | Kind::Lbu));
        let stores = body.iter().any(|k| matches!(k, Kind::Sw | Kind::Sh | Kind::Sb));
        if jumps == 1 && loads && stores {
            out.push((head, b));
        }
    }
    out
}

/// Deciphers the loop (head, branch) of `text` (at `at`): the text as it leaves it and where the
/// code after it went, or None. The run starts as far back as the words before the head are
/// straight, and again after any word a start fails at before the head.
fn decipher_loop(text: &[u8], at: u32, head: u32, branch: u32) -> Option<(Vec<u8>, u32)> {
    let word = |a: u32| a.checked_sub(at).and_then(|o| text.get(o as usize..o as usize + 4)).map(|w| u32::from_le_bytes(w.try_into().unwrap()));
    let mut start = head;
    while head - start < 4 * LEAD_MAX && start > at && word(start - 4).is_some_and(|w| straight(kind(w))) && !word(start.wrapping_sub(8)).is_some_and(|w| kind(w).has_delay_slot()) {
        start -= 4;
    }
    loop {
        match run(text, at, start, branch) {
            Ok(r) => return Some(r),
            Err(pc) if pc >= start && pc < head => start = pc + 4,
            Err(_) => return None,
        }
    }
}

/// The code `exe`'s loops decipher in its text, or None when no loop with only constants changes
/// MIN_WORDS words of it.
pub fn deciphered(exe: &PsxExe) -> Option<Deciphered> {
    let at = exe.text_addr;
    let mut text = exe.text[..exe.text.len() & !3].to_vec();
    let end = at + text.len() as u32;
    let mut look = vec![(at, end)];
    let mut entries = Vec::new();
    for _ in 0..ROUNDS {
        let mut changed = Vec::new();
        for &(lo, hi) in &look {
            for (head, branch) in loops(&text, at, lo, hi) {
                let Some((t, next)) = decipher_loop(&text, at, head, branch) else { continue };
                let diff: Vec<u32> = (0..text.len() / 4).filter(|&j| t[4 * j..4 * j + 4] != text[4 * j..4 * j + 4]).map(|j| at + 4 * j as u32).collect();
                if diff.len() < MIN_WORDS {
                    continue;
                }
                let next = next & 0x1FFF_FFFF | at & !0x1FFF_FFFF;
                if diff.binary_search(&next).is_ok() || diff.first().is_some_and(|&f| f <= next && next <= *diff.last().unwrap()) {
                    entries.push(next);
                }
                changed.extend(diff);
                text = t;
            }
        }
        if changed.is_empty() {
            break;
        }
        changed.sort_unstable();
        changed.dedup();
        look = spans(&changed);
    }
    let all: Vec<u32> = (0..text.len() / 4).filter(|&j| text[4 * j..4 * j + 4] != exe.text[4 * j..4 * j + 4]).map(|j| at + 4 * j as u32).collect();
    if all.is_empty() {
        return None;
    }
    entries.sort_unstable();
    entries.dedup();
    Some(Deciphered { text, spans: spans(&all), entries })
}

/// Runs of consecutive word addresses, as [start, end).
fn spans(words: &[u32]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &a in words {
        match out.last_mut() {
            Some(s) if s.1 == a => s.1 = a + 4,
            _ => out.push((a, a + 4)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOP: u32 = 0;
    const T: u32 = 0x8001_0000;

    fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
        op << 26 | rs << 21 | rt << 16 | (imm & 0xFFFF)
    }
    fn r(rs: u32, rt: u32, rd: u32, f: u32) -> u32 {
        rs << 21 | rt << 16 | rd << 11 | f
    }
    fn exe(words: &[u32]) -> PsxExe {
        PsxExe { pc0: T, gp0: 0, text_addr: T, text: words.iter().flat_map(|w| w.to_le_bytes()).collect(), bss: (0, 0), stack: (0, 0) }
    }
    fn word(x: &[u8], k: usize) -> u32 {
        u32::from_le_bytes(x[4 * k..4 * k + 4].try_into().unwrap())
    }

    /// Spyro 2's scheme: `n` words from `at` XORed with a key that steps `key = c - key`, then a
    /// jump to `entry`. Registers as the game's executable has them.
    fn decryptor(at: u32, n: u32, key: u32, c: u32, entry: u32) -> Vec<u32> {
        let (zero, v0, v1, t0, t2, t6, t7) = (0, 2, 3, 8, 10, 14, 15);
        let hi = |a: u32| (a >> 16) + (a >> 15 & 1);
        vec![
            i(0x0F, 0, t0, hi(entry)),
            i(0x09, t0, t0, entry),
            i(0x0F, 0, v0, key >> 16),
            i(0x0D, v0, v0, key),
            i(0x0F, 0, v1, c >> 16),
            i(0x0D, v1, v1, c),
            i(0x0F, 0, t6, hi(at)),
            i(0x09, t6, t6, at),
            i(0x08, zero, t7, n),
            i(0x23, t6, t2, 0), // loop: lw t2,0(t6)
            i(0x08, t7, t7, 0xFFFF),
            r(t2, v0, t2, 0x26),
            i(0x2B, t6, t2, 0),
            i(0x08, t6, t6, 4),
            i(0x05, t7, zero, 0xFFFA), // bnez t7, loop
            r(v1, v0, v0, 0x22),       // sub v0, v1, v0
            r(t0, 0, 0, 8),            // jr t0
            r(0, 0, t6, 0x20),         // add t6, zero, zero
        ]
    }

    fn cipher(plain: &[u32], mut key: u32, c: u32) -> Vec<u32> {
        plain
            .iter()
            .map(|&w| {
                let x = w ^ key;
                key = c.wrapping_sub(key);
                x
            })
            .collect()
    }

    /// The executable's words of Spyro 2 at 8006A0F8 as they run, from the trace of the sim.
    const PLAIN: [u32; 8] = [0xAFBF_0000, 0x3404_0001, 0x0000_2825, 0x0C01_7BC0, 0x0000_3025, 0x8FBF_0000, 0x3C04_8007, 0x2484_BC9C];

    #[test]
    fn a_rolling_xor_is_deciphered_and_its_entry_found() {
        let (key, c) = (0x6D24_339B, 0x16D1_6348);
        let mut w = vec![JR_RA, NOP];
        let at = T + 4 * w.len() as u32;
        w.extend(cipher(&PLAIN, key, c));
        w.extend([JR_RA, NOP]);
        w.extend(decryptor(at, PLAIN.len() as u32, key, c, at + 8));
        let d = deciphered(&exe(&w)).expect("deciphered");
        assert_eq!(d.spans, [(at, at + 4 * PLAIN.len() as u32)]);
        assert_eq!(d.entries, [at + 8]);
        let o = (at - T) as usize / 4;
        assert!((0..PLAIN.len()).all(|k| word(&d.text, o + k) == PLAIN[k]));
    }

    /// The disc words of Spyro 2's executable at 8006A0F8 and the key there: the loop at
    /// 8006A3C8 starts at 8006A0B8 with 0x6D24339B, 16 steps before.
    #[test]
    fn spyro_2s_words_come_out_as_they_ran() {
        let (mut key, c) = (0x6D24_339Bu32, 0x16D1_6348u32);
        for _ in 0..16 {
            key = c.wrapping_sub(key);
        }
        let disc = [0xC29B_339B, 0x9DA9_2FAC, 0x6D24_1BBE, 0xA5AC_546D, 0x6D24_03BE, 0x2612_2FAD, 0x5120_B39C, 0x8D29_9331];
        assert_eq!(cipher(&disc, key, c), PLAIN);
    }

    const JR_RA: u32 = 0x03E0_0008;

    /// A loop over a register the code before it does not set, a copy to RAM outside the text,
    /// and a loop that changes too few words are not deciphering.
    #[test]
    fn loops_of_unknowns_or_outside_the_text_are_left() {
        let (a0, a1, t0, t1) = (4, 5, 8, 9);
        let body = |base: u32| {
            vec![
                i(0x23, base, t0, 0), // loop: lw t0,0(base)
                i(0x08, t1, t1, 0xFFFF),
                r(t0, t0, t0, 0x26),
                i(0x2B, base, t0, 0),
                i(0x09, base, base, 4),
                i(0x05, t1, 0, 0xFFFA),
                NOP,
                JR_RA,
                NOP,
            ]
        };
        // From an argument: a0 and t1 come from the caller.
        let mut w = vec![JR_RA, NOP];
        w.extend(body(a0));
        assert_eq!(deciphered(&exe(&w)), None);
        // To RAM: a constant address outside the text.
        let mut w = vec![JR_RA, NOP, i(0x0F, 0, a1, 0x8019), i(0x08, 0, t1, 8)];
        w.extend(body(a1));
        assert_eq!(deciphered(&exe(&w)), None);
        // Three words.
        let mut w = vec![JR_RA, NOP, 0x1111_1111, 0x2222_2222, 0x3333_3333, JR_RA, NOP];
        w.extend(decryptor(T + 8, 3, 0x1234_5678, 0x0101_0101, T + 8));
        assert_eq!(deciphered(&exe(&w)), None);
    }

    /// Code deciphered right after the loop runs on from it: the run stops where it cannot
    /// follow (`sw ra,0(sp)` with an unknown `sp`), and that is the entry.
    #[test]
    fn code_the_loop_falls_into_is_entered_there() {
        let (key, c) = (0x5555_AAAA, 0x0123_4567);
        let mut w = vec![JR_RA, NOP];
        let at = T + 4 * (w.len() + 16) as u32;
        w.extend(&decryptor(at, PLAIN.len() as u32, key, c, 0)[..16]);
        w.extend(cipher(&PLAIN, key, c));
        let d = deciphered(&exe(&w)).expect("deciphered");
        assert_eq!(d.spans, [(at, at + 4 * PLAIN.len() as u32)]);
        assert_eq!(d.entries, [at]);
    }

    /// Code deciphered by code that is itself deciphered: two rounds.
    #[test]
    fn a_deciphered_decryptor_deciphers_in_turn() {
        let (k1, c1, k2, c2) = (0x0BAD_F00D, 0x1357_9BDF, 0x7654_3210, 0x0F0F_0F0F);
        let mut w = vec![JR_RA, NOP];
        let inner_at = T + 4 * w.len() as u32;
        w.extend(cipher(&PLAIN, k2, c2));
        w.extend([JR_RA, NOP]);
        let outer_at = T + 4 * w.len() as u32;
        let inner = decryptor(inner_at, PLAIN.len() as u32, k2, c2, inner_at);
        w.extend(cipher(&inner, k1, c1));
        w.extend([JR_RA, NOP]);
        w.extend(decryptor(outer_at, inner.len() as u32, k1, c1, outer_at));
        let d = deciphered(&exe(&w)).expect("deciphered");
        let (o, p) = ((inner_at - T) as usize / 4, (outer_at - T) as usize / 4);
        assert!((0..PLAIN.len()).all(|k| word(&d.text, o + k) == PLAIN[k]), "the inner code");
        assert!((0..inner.len()).all(|k| word(&d.text, p + k) == inner[k]), "the inner decryptor");
        assert_eq!(d.entries, [inner_at, outer_at]);
    }
}
