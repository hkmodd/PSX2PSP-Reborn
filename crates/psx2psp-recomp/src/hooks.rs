//! Recipes: library code the runtime runs itself (HLE) instead of its translation.
//!
//! A recipe is found by its exact code, with the immediates that change from one link to the
//! next masked, so every executable that links the library gets it, wherever it lies. Its hook
//! replaces one guest instruction by a host call the runtime serves: [`HOOK`] in the call's `pc`
//! word, the hook's id in its instruction word, then two argument words. The runtime does what
//! the guest code would, with the same device reads and the same results, and continues where
//! the guest would (through the dispatch table), so only the host's spinning goes.
//!
//! The ids are shared with `psp/psxrt/native.c`.

use std::collections::BTreeMap;

use crate::discover::Discovery;
use crate::exe::PsxExe;

/// Flag in a host call's `pc` word: a hook (beside [`crate::emit::RESUME`]).
pub const HOOK: u32 = 2;

/// libpad's `_padChkRC2wait()`: has the wait on root counter 2 that `_padSetRC2wait(limit)`
/// started expired? Arguments: the addresses of its `start` and `limit` globals.
pub const PAD_CHK_RC2_WAIT: u32 = 1;
/// The loop in libpad's `_padSioRW2` that waits for the pad's /ACK (I_STAT bit 7) or a root
/// counter 2 timeout, on its registers. Arguments: where it goes on an /ACK, and on a timeout.
pub const PAD_SIO_ACK_WAIT: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hook {
    pub id: u32,
    pub args: [u32; 2],
}

/// Word kinds in a pattern.
const F: u32 = 0xFFFF_FFFF; // exact
const I: u32 = 0xFFFF_0000; // immediate masked (`lui` of a global, `lw`/`sw` offset)
const J: u32 = 0xFC00_0000; // `j` target masked

/// `_padChkRC2wait`, PsyQ libpad (Sheep Dog 'n' Wolf: 0x8009C6BC).
const CHK_RC2_WAIT: [(u32, u32); 40] = [
    (0x3C02_1F80, F), (0x3442_1120, F), (0x9443_0000, F), (0x3C02_0000, I),
    (0x8C42_0000, I), (0x3064_FFFF, F), (0x0082_102A, F), (0x1040_000A, F),
    (0x3C03_1F80, F), (0x3463_1128, F), (0x9462_0000, F), (0x0000_0000, F),
    (0x1040_0004, F), (0x3C02_0001, F), (0x9462_0000, F), (0x0800_0000, J),
    (0x0082_2021, F), (0x0082_2021, F), (0x3C02_1F80, F), (0x3442_1124, F),
    (0x9442_0000, F), (0x0000_0000, F), (0x3042_0200, F), (0x1440_0008, F),
    (0x0000_0000, F), (0x3C02_0000, I), (0x8C42_0000, I), (0x3C03_0000, I),
    (0x8C63_0000, I), (0x0082_1023, F), (0x0800_0000, J), (0x0002_10C3, F),
    (0x3C02_0000, I), (0x8C42_0000, I), (0x3C03_0000, I), (0x8C63_0000, I),
    (0x0082_1023, F), (0x0043_102A, F), (0x03E0_0008, F), (0x3842_0001, F),
];

/// The /ACK wait loop of `_padSioRW2` (Sheep Dog 'n' Wolf: 0x8009BB1C), from its head to the
/// back-edge's delay slot. Registers: a3 T2 mode, t1 T2 count, a2 T2 target, t2 0x10000,
/// t0 I_STAT, a0 start, a1 limit.
const SIO_ACK_WAIT: [(u32, u32); 34] = [
    (0x94E2_0000, F), (0x9522_0000, F), (0x0000_0000, F), (0x3043_FFFF, F),
    (0x0064_102A, F), (0x1040_0009, F), (0x0000_0000, F), (0x94C2_0000, F),
    (0x0000_0000, F), (0x1040_0004, F), (0x0000_0000, F), (0x94C2_0000, F),
    (0x0800_0000, J), (0x0043_1821, F), (0x006A_1821, F), (0x94E2_0000, F),
    (0x0000_0000, F), (0x3042_0200, F), (0x1040_0006, F), (0x0064_1023, F),
    (0x0045_102A, F), (0x1440_0007, F), (0x2402_FFFE, F), (0x0800_0000, J),
    (0x0000_0000, F), (0x0002_10C3, F), (0x0045_102A, F), (0x1040_0039, F),
    (0x2402_FFFE, F), (0x8D02_0000, F), (0x0000_0000, F), (0x3042_0080, F),
    (0x1040_FFDF, F), (0x0000_0000, F),
];

fn matches(text: &[u32], at: usize, pat: &[(u32, u32)]) -> bool {
    text.get(at..at + pat.len())
        .is_some_and(|w| w.iter().zip(pat).all(|(&w, &(v, m))| w & m == v))
}

/// The address a `j` at `pc` goes to.
fn j_target(pc: u32, w: u32) -> u32 {
    (pc.wrapping_add(4) & 0xF000_0000) | ((w & 0x03FF_FFFF) << 2)
}

/// The global a `lui r,hi` / `lw r,lo(r)` pair addresses.
fn global(hi: u32, lo: u32) -> u32 {
    (hi << 16).wrapping_add(lo as i16 as i32 as u32)
}

/// The hooks for `exe`, by guest address: recipes whose code is there exactly, at an instruction
/// the discovery reached that is not a delay slot.
pub fn find(exe: &PsxExe, d: &Discovery) -> BTreeMap<u32, Hook> {
    let text: Vec<u32> = exe
        .text
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let pc_of = |i: usize| exe.text_addr.wrapping_add(4 * i as u32);
    let mut hooks = BTreeMap::new();
    for i in 0..text.len() {
        let pc = pc_of(i);
        if !d.code.contains(&pc) || d.delay_slots.contains_key(&pc) {
            continue;
        }
        if matches(&text, i, &CHK_RC2_WAIT) {
            let w = &text[i..i + CHK_RC2_WAIT.len()];
            let (start, limit) = (global(w[3] & 0xFFFF, w[4] & 0xFFFF), global(w[27] & 0xFFFF, w[28] & 0xFFFF));
            let same = [(25, 26), (32, 33)].iter().all(|&(h, l)| global(w[h] & 0xFFFF, w[l] & 0xFFFF) == start)
                && global(w[34] & 0xFFFF, w[35] & 0xFFFF) == limit;
            let jumps = j_target(pc_of(i + 15), w[15]) == pc + 0x48 && j_target(pc_of(i + 30), w[30]) == pc + 0x94;
            if same && jumps {
                hooks.insert(pc, Hook { id: PAD_CHK_RC2_WAIT, args: [start, limit] });
            }
        } else if matches(&text, i, &SIO_ACK_WAIT) {
            let w = &text[i..i + SIO_ACK_WAIT.len()];
            // The timeout `j` and the expired `beqz` (pc + 0x6C + 4 + 0x39 * 4) go to one place.
            let timeout = pc + 0x154;
            let jumps = j_target(pc_of(i + 12), w[12]) == pc + 0x3C && j_target(pc_of(i + 23), w[23]) == timeout;
            if jumps && d.code.contains(&(pc + 0x88)) && d.code.contains(&timeout) {
                hooks.insert(pc, Hook { id: PAD_SIO_ACK_WAIT, args: [pc + 0x88, timeout] });
            }
        }
    }
    hooks
}

#[cfg(test)]
mod tests {
    use super::*;

    const JR_RA: u32 = 0x03E0_0008;

    /// The pattern's words with the masked fields as Sheep Dog 'n' Wolf's LEV00 has them.
    fn real(pat: &[(u32, u32)], fill: &[(usize, u32)]) -> Vec<u32> {
        let mut w: Vec<u32> = pat.iter().map(|&(v, _)| v).collect();
        for &(i, v) in fill {
            w[i] = v;
        }
        w
    }

    fn exe(addr: u32, words: &[u32]) -> (PsxExe, Discovery) {
        let x = PsxExe {
            pc0: addr,
            gp0: 0,
            text_addr: addr,
            text: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            bss: (0, 0),
            stack: (0, 0),
        };
        let d = Discovery::run(&x);
        (x, d)
    }

    #[test]
    fn finds_pad_chk_rc2_wait_with_its_globals() {
        let at = 0x8009_C6BC;
        let mut w = real(
            &CHK_RC2_WAIT,
            &[(3, 0x3C02_800C), (4, 0x8C42_CAC4), (15, 0x0802_71C1), (25, 0x3C02_800C), (26, 0x8C42_CAC4),
              (27, 0x3C03_800C), (28, 0x8C63_CAC8), (30, 0x0802_71D4), (32, 0x3C02_800C), (33, 0x8C42_CAC4),
              (34, 0x3C03_800C), (35, 0x8C63_CAC8)],
        );
        let (x, d) = exe(at, &w);
        let h = find(&x, &d);
        assert_eq!(h.get(&at), Some(&Hook { id: PAD_CHK_RC2_WAIT, args: [0x800B_CAC4, 0x800B_CAC8] }));
        assert_eq!(h.len(), 1);
        // Another limit global in one of its reads: not the recipe.
        w[35] = 0x8C63_CACC;
        let (x, d) = exe(at, &w);
        assert!(find(&x, &d).is_empty());
    }

    #[test]
    fn finds_the_sio_ack_wait_loop_and_its_exits() {
        let at = 0x8009_BB1C;
        let mut w = real(&SIO_ACK_WAIT, &[(12, 0x0802_6ED6), (23, 0x0802_6F1C)]);
        w.extend([JR_RA, 0]); // the /ACK exit, 0x8009BBA4
        w.resize((0x154 / 4) as usize, 0);
        w.extend([JR_RA, 0]); // the timeout exit, 0x8009BC70
        let (x, d) = exe(at, &w);
        let h = find(&x, &d);
        assert_eq!(h.get(&at), Some(&Hook { id: PAD_SIO_ACK_WAIT, args: [0x8009_BBA4, 0x8009_BC70] }));
        assert_eq!(h.len(), 1);
    }
}
