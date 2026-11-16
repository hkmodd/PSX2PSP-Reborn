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
/// libpress's `DecDCTvlc(bs, buf)`: an MDEC bitstream decoded to the MDEC's run-length codes, in
/// the runtime's C (`psp/psxrt/vlc.c`). Arguments: the address of its `size` global (its saved
/// registers 4 bytes on, the output's end 0x28 on), and of its first table (the second 64 KB on).
pub const DEC_DCT_VLC: u32 = 3;
/// The loop of libetc's `v_wait(count, timeout)`, under `VSync(n)`, hooked at its head: passes
/// that count the timeout down at 16(sp) and read `Vcount`, which only the vblank interrupt moves.
/// Arguments: the address of `Vcount`, and the loop's back-edge (the translation sends the PS1
/// cycles of one pass there instead, [`crate::emit`]'s `loop_cost`, so the runtime charges each
/// pass what the translated loop would).
pub const VSYNC_WAIT: u32 = 4;

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

/// `DecDCTvlc`, PsyQ libpress (Sheep Dog 'n' Wolf: 0x80111870), the whole routine. Its globals
/// are `lui`/`addiu` pairs: `size` (0, 1), the first table (2, 3), the second (4, 5), the saved
/// registers (8, 9 and 199, 200), the output's end (46, 47 and 187, 188).
const DCT_VLC: [(u32, u32); 212] = [
    (0x3C08_0000, I), (0x2508_0000, I), (0x3C06_0000, I), (0x24C6_0000, I),
    (0x3C07_0000, I), (0x24E7_0000, I), (0x1480_000F, F), (0x8D09_0000, F),
    (0x3C08_0000, I), (0x2508_0000, I), (0x8D04_0000, F), (0x8D05_0004, F),
    (0x8D02_0008, F), (0x8D03_000C, F), (0x8D0C_0010, F), (0x8D0D_0014, F),
    (0x8D0F_0018, F), (0x8D18_001C, F), (0x8D19_0020, F), (0x0129_4820, F),
    (0x1000_0064, F), (0x00A9_7020, F), (0x0000_6820, F), (0x0000_7820, F),
    (0x0000_C020, F), (0x0000_C820, F), (0x0129_4820, F), (0x00A9_7020, F),
    (0x8C89_0000, F), (0x948C_0004, F), (0x948A_0006, F), (0x9482_0008, F),
    (0x9483_000A, F), (0x214A_FFFD, F), (0x0540_0002, F), (0x000C_6280, F),
    (0x200D_0001, F), (0x2084_000C, F), (0x0002_1400, F), (0x0043_1025, F),
    (0x0000_1825, F), (0xACA9_0000, F), (0x3129_FFFF, F), (0x0009_4880, F),
    (0x2529_0004, F), (0x0125_4820, F), (0x3C08_0000, I), (0x2508_0000, I),
    (0xAD09_0000, F), (0x20A5_0002, F), (0x11A0_0035, F), (0x0002_4582, F),
    (0x3901_03FF, F), (0x1020_0085, F), (0x20A5_0002, F), (0x21A1_FFFD, F),
    (0x0420_0002, F), (0x20C1_FC00, F), (0x2021_FC00, F), (0x0002_4602, F),
    (0x0008_4080, F), (0x0101_4020, F), (0x9509_0000, F), (0x950A_0002, F),
    (0x0000_4024, F), (0x1140_000A, F), (0x0122_1004, F), (0x2001_0020, F),
    (0x002A_0822, F), (0x0022_4006, F), (0x0440_0004, F), (0x0142_1004, F),
    (0x200B_FFFF, F), (0x002B_5806, F), (0x010B_4022, F), (0x006A_1820, F),
    (0x0069_1820, F), (0x3061_0010, F), (0x1020_0005, F), (0x3063_000F, F),
    (0x9489_0000, F), (0x2084_0002, F), (0x0069_4804, F), (0x0049_1025, F),
    (0x21A1_FFFE, F), (0x1C20_0008, F), (0x0328_4820, F), (0x1020_0004, F),
    (0x0308_4820, F), (0x01E8_4820, F), (0x1000_0004, F), (0x01E8_7820, F),
    (0x1000_0002, F), (0x0308_C020, F), (0x0328_C820, F), (0x0009_4880, F),
    (0x3129_03FF, F), (0x0189_4825, F), (0x21AD_0001, F), (0x21A1_FFF9, F),
    (0x1420_0011, F), (0xA4A9_0000, F), (0x1000_000F, F), (0x21AD_FFFA, F),
    (0x3901_01FF, F), (0x1020_0051, F), (0x20A5_0002, F), (0x0002_1280, F),
    (0x2063_000A, F), (0x3061_0010, F), (0x1020_0005, F), (0x3063_000F, F),
    (0x9489_0000, F), (0x2084_0002, F), (0x0069_4804, F), (0x0049_1025, F),
    (0x0188_4025, F), (0xA4A8_0000, F), (0x00AE_0823, F), (0x0421_004F, F),
    (0x20A5_0002, F), (0x0002_44C2, F), (0x0008_40C0, F), (0x0106_4020, F),
    (0x8D09_0000, F), (0x0000_0000, F), (0x1520_0011, F), (0x3121_00FF, F),
    (0x0002_1200, F), (0x2063_0008, F), (0x3061_0010, F), (0x1020_0005, F),
    (0x3063_000F, F), (0x9488_0000, F), (0x2084_0002, F), (0x0068_4004, F),
    (0x0048_1025, F), (0x0002_45C2, F), (0x0008_4080, F), (0x0107_4020, F),
    (0x8D09_0000, F), (0x0000_5820, F), (0x1000_0002, F), (0x3121_00FF, F),
    (0x8D0B_0004, F), (0x0022_1004, F), (0x0061_1820, F), (0x3061_0010, F),
    (0x1020_0005, F), (0x3063_000F, F), (0x9488_0000, F), (0x2084_0002, F),
    (0x0068_4004, F), (0x0048_1025, F), (0x0009_4C02, F), (0x3921_7C1F, F),
    (0x1020_0015, F), (0x3921_FE00, F), (0x1020_FF93, F), (0xA4A9_0000, F),
    (0x1160_FFD8, F), (0x20A5_0002, F), (0x316A_FFFF, F), (0x3941_7C1F, F),
    (0x1020_000D, F), (0x3941_FE00, F), (0x1020_FF8B, F), (0xA4AA_0000, F),
    (0x000B_5402, F), (0x1140_FFCF, F), (0x20A5_0002, F), (0x3941_7C1F, F),
    (0x1020_0005, F), (0x3941_FE00, F), (0x1020_FF83, F), (0xA4AA_0000, F),
    (0x1000_FFC8, F), (0x20A5_0002, F), (0x0002_4402, F), (0xA4A8_0000, F),
    (0x20A5_0002, F), (0x9488_0000, F), (0x2084_0002, F), (0x0002_1400, F),
    (0x0068_4004, F), (0x1000_FFBF, F), (0x0048_1025, F), (0x3C08_0000, I),
    (0x2508_0000, I), (0x8D09_0000, F), (0x3408_FE00, F), (0x00A9_0823, F),
    (0x0421_0004, F), (0x0000_0000, F), (0xA4A8_0000, F), (0x1000_FFFB, F),
    (0x20A5_0002, F), (0x03E0_0008, F), (0x0000_1020, F), (0x3C08_0000, I),
    (0x2508_0000, I), (0xAD04_0000, F), (0xAD05_0004, F), (0xAD02_0008, F),
    (0xAD03_000C, F), (0xAD0C_0010, F), (0xAD0D_0014, F), (0xAD0F_0018, F),
    (0xAD18_001C, F), (0xAD19_0020, F), (0x03E0_0008, F), (0x2002_0001, F),
];

/// `v_wait`, PsyQ libetc (Sheep Dog 'n' Wolf: 0x80092078, MediEvil: 0x800A3F90), the whole
/// routine: `Vcount` read at 3, 4 and 28, 29; the loop's head at 10, its timeout path at 18 (a
/// message and two calls, then `j` to the exit at 34), its back-edge at 32.
const V_WAIT: [(u32, u32); 38] = [
    (0x27BD_FFE0, F), (0x0005_2BC0, F), (0xAFA5_0010, F), (0x3C02_0000, I),
    (0x8C42_0000, I), (0x0000_0000, F), (0x0044_102A, F), (0x1040_001A, F),
    (0xAFBF_0018, F), (0x2403_FFFF, F), (0x8FA2_0010, F), (0x0000_0000, F),
    (0x2442_FFFF, F), (0xAFA2_0010, F), (0x8FA2_0010, F), (0x0000_0000, F),
    (0x1443_000B, F), (0x0000_0000, F), (0x3C04_0000, I), (0x0C00_0000, J),
    (0x2484_0000, I), (0x0C00_0000, J), (0x0000_2021, F), (0x2404_0003, F),
    (0x0C00_0000, J), (0x0000_2821, F), (0x0800_0000, J), (0x0000_0000, F),
    (0x3C02_0000, I), (0x8C42_0000, I), (0x0000_0000, F), (0x0044_102A, F),
    (0x1440_FFE9, F), (0x0000_0000, F), (0x8FBF_0018, F), (0x27BD_0020, F),
    (0x03E0_0008, F), (0x0000_0000, F),
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
        } else if matches(&text, i, &DCT_VLC) {
            let w = &text[i..i + DCT_VLC.len()];
            let g = |h: usize| global(w[h] & 0xFFFF, w[h + 1] & 0xFFFF);
            // The runtime finds the other globals from these two, as this link lays them out.
            let (size, tab1) = (g(0), g(2));
            let laid_out = g(4) == tab1.wrapping_add(0x1_0000)
                && [8, 199].iter().all(|&h| g(h) == size.wrapping_add(4))
                && [46, 187].iter().all(|&h| g(h) == size.wrapping_add(0x28));
            if laid_out {
                hooks.insert(pc, Hook { id: DEC_DCT_VLC, args: [size, tab1] });
            }
        } else if matches(&text, i, &V_WAIT) {
            let w = &text[i..i + V_WAIT.len()];
            let vcount = global(w[3] & 0xFFFF, w[4] & 0xFFFF);
            let (head, timeout, exit) = (pc + 0x28, pc + 0x48, pc + 0x88);
            let reached = [head, timeout, exit].iter().all(|a| d.code.contains(a));
            if global(w[28] & 0xFFFF, w[29] & 0xFFFF) == vcount && j_target(pc_of(i + 26), w[26]) == exit && reached {
                hooks.insert(head, Hook { id: VSYNC_WAIT, args: [vcount, pc + 0x80] });
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

    #[test]
    fn finds_dec_dct_vlc_with_its_globals() {
        let at = 0x8011_1870;
        let mut w = real(
            &DCT_VLC,
            &[(0, 0x3C08_8012), (1, 0x2508_ACE0), (2, 0x3C06_8012), (3, 0x24C6_AD10), (4, 0x3C07_8013),
              (5, 0x24E7_AD10), (8, 0x3C08_8012), (9, 0x2508_ACE4), (46, 0x3C08_8012), (47, 0x2508_AD08),
              (187, 0x3C08_8012), (188, 0x2508_AD08), (199, 0x3C08_8012), (200, 0x2508_ACE4)],
        );
        let (x, d) = exe(at, &w);
        let h = find(&x, &d);
        assert_eq!(h.get(&at), Some(&Hook { id: DEC_DCT_VLC, args: [0x8011_ACE0, 0x8011_AD10] }));
        assert_eq!(h.len(), 1);
        // The saved registers somewhere else in one place: not the layout the runtime knows.
        w[200] = 0x2508_ACF4;
        let (x, d) = exe(at, &w);
        assert!(find(&x, &d).is_empty());
    }

    #[test]
    fn finds_v_wait_at_its_loop_head() {
        let at = 0x800A_3F90; // MediEvil's words
        let mut w = real(
            &V_WAIT,
            &[(3, 0x3C02_800F), (4, 0x8C42_CB98), (18, 0x3C04_800C), (19, 0x0C02_E6E8), (20, 0x2484_6E60),
              (21, 0x0C02_A497), (24, 0x0C02_E700), (26, 0x0802_9006), (28, 0x3C02_800F), (29, 0x8C42_CB98)],
        );
        let (x, d) = exe(at, &w);
        let h = find(&x, &d);
        assert_eq!(h.get(&0x800A_3FB8), Some(&Hook { id: VSYNC_WAIT, args: [0x800E_CB98, 0x800A_4010] }));
        assert_eq!(h.len(), 1);
        // The loop reading another global than the first test: not the recipe.
        w[29] = 0x8C42_CB9C;
        let (x, d) = exe(at, &w);
        assert!(find(&x, &d).is_empty());
    }
}
