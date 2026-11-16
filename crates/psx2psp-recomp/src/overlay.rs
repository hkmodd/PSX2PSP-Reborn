//! Overlays: code a game loads from a plain disc file (no PS-X EXE header) to an address of its
//! choosing (`docs/recomp/S24_plan.md`). The file says nowhere where it goes, but its code does:
//! every `jal` into the overlay itself and every pointer to one of its functions (a handler
//! table, a C++ vtable) names an absolute address that is, in the file, the start of a function.
//! [`locate_all`] finds the load address that lines those up with the function prologues of the
//! file; data files line nothing up and are left alone.

use std::collections::{BTreeSet, HashMap};

use crate::exe::PsxExe;

/// Lowest load address considered: below it are the BIOS and kernel areas.
const LOW: u32 = 0x8001_0000;
/// End of the PS1 RAM in KSEG0.
const HIGH: u32 = 0x8020_0000;
/// Votes a file needs to fix its load address on its own.
const STRONG: u32 = 8;
/// Votes a file needs to take one of the load addresses the strong files of its disc fixed.
const WEAK: u32 = 3;
/// Votes a file needs to take a slot no address beats (only ties): a small overlay whose
/// functions the executable reaches through its own tables, such as MediEvil's CR.BIN (2 votes
/// at the slot its 25 sibling overlays fixed, 2 at unrelated addresses).
const TIED: u32 = 2;

fn is_prologue(w: u32) -> bool {
    matches!(w >> 16, 0x27BD | 0x23BD) && w & 0x8000 != 0
}

/// Load-address votes of one file: each distinct `jal` target or word-aligned RAM pointer `t`
/// and each prologue offset `o` (`addiu sp,sp,-n`, or the `addi` form) vote for `t - o`. At the
/// true address every call or pointer to a function of the file votes once; elsewhere votes
/// are coincidences.
fn tally(bytes: &[u8]) -> Option<HashMap<u32, u32>> {
    let len = (bytes.len() & !3) as u32;
    if len < 0x800 || len > HIGH - LOW {
        return None;
    }
    let words: Vec<u32> = bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    let prologues: Vec<u32> = (0..words.len()).filter(|&i| is_prologue(words[i])).map(|i| 4 * i as u32).collect();
    let targets: BTreeSet<u32> = words
        .iter()
        .map(|&w| if w >> 26 == 3 { 0x8000_0000 | (w & 0x03FF_FFFF) << 2 } else { w })
        .filter(|&t| t & 3 == 0 && (LOW..HIGH).contains(&t))
        .collect();
    if prologues.len() < WEAK as usize {
        return None;
    }
    let mut votes: HashMap<u32, u32> = HashMap::new();
    for &t in &targets {
        for &o in &prologues {
            let b = t.wrapping_sub(o);
            if b >= LOW && b <= HIGH - len {
                *votes.entry(b).or_default() += 1;
            }
        }
    }
    Some(votes)
}

/// The best address of `votes` (lowest on a tie) and the best count among the others.
fn best(votes: &HashMap<u32, u32>, among: impl Fn(u32) -> bool) -> Option<((u32, u32), u32)> {
    let mut top: Option<(u32, u32)> = None;
    let mut second = 0;
    for (&b, &v) in votes.iter().filter(|&(&b, _)| among(b)) {
        match top {
            Some((tb, tv)) if v < tv || (v == tv && b > tb) => second = second.max(v),
            Some((_, tv)) => {
                second = second.max(tv);
                top = Some((b, v));
            }
            None => top = Some((b, v)),
        }
    }
    top.map(|t| (t, second))
}

/// The load address of each of `files` taken as an overlay, with its votes; `None` for a file
/// that does not look like code for one address.
///
/// A file fixes its address alone with [`STRONG`] votes and three times the runner-up's. The
/// addresses so fixed are the disc's overlay slots; a file with fewer votes (a small overlay
/// that only calls into the executable, say) takes the slot it gives most votes, with at least
/// [`WEAK`] and more than any other address, or [`TIED`] with no address above and no other
/// slot as high. A wrong slot costs nothing at run time: the runtime
/// takes a translation only when the RAM there holds exactly the file.
pub fn locate_all(files: &[&[u8]]) -> Vec<Option<(u32, u32)>> {
    let tallies: Vec<Option<HashMap<u32, u32>>> = files.iter().map(|f| tally(f)).collect();
    let strong: Vec<Option<(u32, u32)>> = tallies
        .iter()
        .map(|t| {
            let ((b, v), second) = best(t.as_ref()?, |_| true)?;
            (v >= STRONG && v >= 3 * second).then_some((b, v))
        })
        .collect();
    let slots: BTreeSet<u32> = strong.iter().flatten().map(|&(b, _)| b).collect();
    tallies
        .iter()
        .zip(strong)
        .map(|(t, s)| {
            s.or_else(|| {
                let t = t.as_ref()?;
                let ((b, v), slot2) = best(t, |b| slots.contains(&b))?;
                let other = best(t, |b| !slots.contains(&b)).map_or(0, |((_, v), _)| v);
                (v >= WEAK && v > other || v >= TIED && v >= other && v > slot2).then_some((b, v))
            })
        })
        .collect()
}

/// The overlay `bytes` as an executable at `base` for discovery and translation: its entry is
/// its first prologue (discovery also starts from every other prologue and every pointer to its
/// text, so the choice only names one function).
pub fn as_exe(bytes: &[u8], base: u32) -> Option<PsxExe> {
    let text = bytes[..bytes.len() & !3].to_vec();
    let pc0 = text.chunks_exact(4).position(|c| is_prologue(u32::from_le_bytes(c.try_into().unwrap())))?;
    Some(PsxExe { pc0: base + 4 * pc0 as u32, gp0: 0, text_addr: base, text, bss: (0, 0), stack: (0, 0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROLOGUE: u32 = 0x27BD_FFE0;
    const B: u32 = 0x8001_0000;

    /// 512 words: prologues at the word indices `pro`, then `targets` as data words from 400.
    fn file(pro: &[usize], targets: &[u32]) -> Vec<u8> {
        let mut w = vec![0u32; 512];
        pro.iter().for_each(|&i| w[i] = PROLOGUE);
        targets.iter().enumerate().for_each(|(k, &t)| w[400 + k] = t);
        w.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn a_small_overlay_takes_a_fixed_slot_on_a_tie() {
        // Strong: prologues at 2^k - 1 (distinct differences), each pointed at once: 9 votes at B.
        let pro: Vec<usize> = (0..9).map(|k| (1 << k) - 1).collect();
        let strong = file(&pro, &pro.iter().map(|&i| B + 4 * i as u32).collect::<Vec<_>>());
        // Small: 2 votes at B (its functions at 10 and 50 pointed), 2 at X (X and X + 40).
        let x = 0x8010_0000;
        let small = file(&[0, 10, 50], &[B + 40, B + 200, x, x + 40]);
        let at = locate_all(&[&strong, &small]);
        assert_eq!(at[0], Some((B, 9)));
        assert_eq!(at[1], Some((B, 2)));
        // Without a slot fixed by another file the small one stays unplaced.
        assert_eq!(locate_all(&[&small]), vec![None]);
    }
}
