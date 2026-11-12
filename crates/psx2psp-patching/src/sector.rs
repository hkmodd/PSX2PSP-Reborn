//! CD-ROM sector integrity: EDC (CRC-32/0xD8018001) and Reed-Solomon P/Q ECC (ECMA-130 Annex A).
//! Lookup tables are generated at compile time.

pub const SECTOR: usize = 2352;
pub const SYNC: [u8; 12] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

const EDC_LUT: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut e = i as u32;
        let mut k = 0;
        while k < 8 {
            e = (e >> 1) ^ if e & 1 != 0 { 0xD801_8001 } else { 0 };
            k += 1;
        }
        t[i] = e;
        i += 1;
    }
    t
};

/// GF(2^8) (poly 0x11D): `F[i] = i·α`, `B[i ^ F[i]] = i`  — i.e. B divides by (α+1).
const ECC_LUT: ([u8; 256], [u8; 256]) = {
    let mut f = [0u8; 256];
    let mut b = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let j = (i << 1) ^ if i & 0x80 != 0 { 0x11D } else { 0 };
        f[i] = j as u8;
        b[i ^ j] = i as u8;
        i += 1;
    }
    (f, b)
};

pub fn edc(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |e, &b| (e >> 8) ^ EDC_LUT[((e ^ b as u32) & 0xFF) as usize])
}

fn ecc_block(src: &[u8], major_count: usize, minor_count: usize, major_mult: usize, minor_inc: usize, dest: &mut [u8]) {
    let (f, b) = (&ECC_LUT.0, &ECC_LUT.1);
    let size = major_count * minor_count;
    for major in 0..major_count {
        let mut index = (major >> 1) * major_mult + (major & 1);
        let (mut a, mut x) = (0u8, 0u8);
        for _ in 0..minor_count {
            let t = src[index];
            index += minor_inc;
            if index >= size {
                index -= size;
            }
            a ^= t;
            x ^= t;
            a = f[a as usize];
        }
        a = b[(f[a as usize] ^ x) as usize];
        dest[major] = a;
        dest[major + major_count] = a ^ x;
    }
}

/// Writes P (0x81C, 172 B) and Q (0x8C8, 104 B) parity. Mode 2 computes with the 4 header bytes zeroed.
fn ecc_generate(s: &mut [u8; SECTOR], zero_address: bool) {
    let saved: [u8; 4] = s[12..16].try_into().unwrap();
    if zero_address {
        s[12..16].fill(0);
    }
    let mut p = [0u8; 172];
    ecc_block(&s[0x0C..], 86, 24, 2, 86, &mut p);
    s[0x81C..0x81C + 172].copy_from_slice(&p);
    let mut q = [0u8; 104];
    ecc_block(&s[0x0C..], 52, 43, 86, 88, &mut q);
    s[0x8C8..0x8C8 + 104].copy_from_slice(&q);
    if zero_address {
        s[12..16].copy_from_slice(&saved);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectorMode {
    /// Not a data sector (CD-DA, or no sync pattern).
    Audio,
    Mode1,
    Mode2Form1,
    Mode2Form2,
}

pub fn mode(s: &[u8; SECTOR]) -> SectorMode {
    if s[..12] != SYNC {
        return SectorMode::Audio;
    }
    match s[15] {
        1 => SectorMode::Mode1,
        // XA subheader byte 2 (submode) bit 5 = Form 2.
        2 if s[18] & 0x20 != 0 => SectorMode::Mode2Form2,
        2 => SectorMode::Mode2Form1,
        _ => SectorMode::Audio,
    }
}

/// Byte range of the user data area inside a raw sector.
pub fn user_data_range(m: SectorMode) -> Option<std::ops::Range<usize>> {
    match m {
        SectorMode::Mode1 => Some(16..16 + 2048),
        SectorMode::Mode2Form1 => Some(24..24 + 2048),
        SectorMode::Mode2Form2 => Some(24..24 + 2324),
        SectorMode::Audio => None,
    }
}

/// Recomputes EDC/ECC in place so the sector stays valid after its payload was modified.
pub fn regenerate(s: &mut [u8; SECTOR]) {
    match mode(s) {
        SectorMode::Mode1 => {
            let e = edc(&s[..0x810]);
            s[0x810..0x814].copy_from_slice(&e.to_le_bytes());
            s[0x814..0x81C].fill(0);
            ecc_generate(s, false);
        }
        SectorMode::Mode2Form1 => {
            let e = edc(&s[0x10..0x818]);
            s[0x818..0x81C].copy_from_slice(&e.to_le_bytes());
            ecc_generate(s, true);
        }
        SectorMode::Mode2Form2 => {
            // Form 2 EDC is optional: an all-zero field means "not present" and must stay so.
            if s[0x92C..0x930] != [0; 4] {
                let e = edc(&s[0x10..0x92C]);
                s[0x92C..0x930].copy_from_slice(&e.to_le_bytes());
            }
        }
        SectorMode::Audio => {}
    }
}

/// True when the stored EDC matches the payload (ECC is implied valid when EDC is, for our purposes).
pub fn verify_edc(s: &[u8; SECTOR]) -> bool {
    let (range, at) = match mode(s) {
        SectorMode::Mode1 => (0..0x810, 0x810),
        SectorMode::Mode2Form1 => (0x10..0x818, 0x818),
        SectorMode::Mode2Form2 if s[0x92C..0x930] != [0; 4] => (0x10..0x92C, 0x92C),
        _ => return true,
    };
    edc(&s[range]).to_le_bytes() == s[at..at + 4]
}

pub fn to_bcd(v: u8) -> u8 {
    (v / 10) << 4 | (v % 10)
}

/// Builds a Mode 2 Form 1 sector at `lba` from 2048 bytes of user data (sync, header, subheader, EDC, ECC).
pub fn build_mode2_form1(lba: u32, user: &[u8; 2048]) -> [u8; SECTOR] {
    let mut s = [0u8; SECTOR];
    s[..12].copy_from_slice(&SYNC);
    let a = lba + 150;
    s[12] = to_bcd((a / 75 / 60) as u8);
    s[13] = to_bcd((a / 75 % 60) as u8);
    s[14] = to_bcd((a % 75) as u8);
    s[15] = 2;
    s[16..24].copy_from_slice(&[0, 0, 0x08, 0, 0, 0, 0x08, 0]); // data submode, duplicated
    s[24..24 + 2048].copy_from_slice(user);
    regenerate(&mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent check of the RS code: every P column / Q diagonal must have zero syndromes
    /// S0 = Σ bᵢ and S1 = Σ bᵢ·α^(n-1-i). Computed by straightforward GF multiplication,
    /// not by the generator's table walk.
    fn gf_mul2(x: u8) -> u8 {
        (x << 1) ^ if x & 0x80 != 0 { 0x1D } else { 0 }
    }

    fn syndromes_ok(s: &[u8; SECTOR], zero_address: bool) -> bool {
        let mut buf = *s;
        if zero_address {
            buf[12..16].fill(0);
        }
        let d = &buf[0x0C..];
        // P: 43 columns × 2 interleaves = 86 codewords of 24 data + 2 parity, stride 86 (in words of 2 bytes).
        for major in 0..86 {
            let word = major >> 1;
            let lsb = major & 1;
            let mut syms: Vec<u8> = (0..24).map(|minor| d[(minor * 43 + word) * 2 + lsb]).collect();
            syms.push(d[(24 * 43 + word) * 2 + lsb]);
            syms.push(d[(25 * 43 + word) * 2 + lsb]);
            let s0 = syms.iter().fold(0, |a, &b| a ^ b);
            let s1 = syms.iter().fold(0u8, |a, &b| gf_mul2(a) ^ b);
            if s0 != 0 || s1 != 0 {
                return false;
            }
        }
        // Q: 26 diagonals × 2 = 52 codewords of 43 data + 2 parity.
        for major in 0..52 {
            let row = major >> 1;
            let lsb = major & 1;
            let mut syms: Vec<u8> = (0..43).map(|minor| {
                let word = (row * 43 + minor * 44) % (26 * 43);
                d[word * 2 + lsb]
            }).collect();
            syms.push(d[(26 * 43 + row) * 2 + lsb]);
            syms.push(d[(26 * 43 + 26 + row) * 2 + lsb]);
            let s0 = syms.iter().fold(0, |a, &b| a ^ b);
            let s1 = syms.iter().fold(0u8, |a, &b| gf_mul2(a) ^ b);
            if s0 != 0 || s1 != 0 {
                return false;
            }
        }
        true
    }

    fn noise(seed: u32) -> [u8; 2048] {
        let mut x = seed.wrapping_mul(2654435761).max(1);
        std::array::from_fn(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
    }

    #[test]
    fn edc_known_value() {
        // CRC with poly 0xD8018001 reflected, init 0: empty input → 0; single 0x01 → table[1].
        assert_eq!(edc(&[]), 0);
        assert_eq!(edc(&[1]), EDC_LUT[1]);
        assert_eq!(EDC_LUT[0x80], 0xD801_8001);
    }

    #[test]
    fn built_sector_has_valid_edc_and_rs_syndromes() {
        for seed in 1..20 {
            let s = build_mode2_form1(seed * 977, &noise(seed));
            assert_eq!(mode(&s), SectorMode::Mode2Form1);
            assert!(verify_edc(&s));
            assert!(syndromes_ok(&s, true), "seed {seed}");
        }
    }

    #[test]
    fn mode1_regenerate_is_rs_valid() {
        let mut s = build_mode2_form1(1234, &noise(7));
        s[15] = 1;
        regenerate(&mut s);
        assert!(verify_edc(&s));
        assert!(syndromes_ok(&s, false));
    }

    #[test]
    fn corruption_detected_and_repaired() {
        let mut s = build_mode2_form1(16, &noise(3));
        s[100] ^= 0x55;
        assert!(!verify_edc(&s));
        regenerate(&mut s);
        assert!(verify_edc(&s) && syndromes_ok(&s, true));
    }

    #[test]
    fn header_msf_is_bcd() {
        let s = build_mode2_form1(0, &[0; 2048]);
        assert_eq!(&s[12..16], &[0x00, 0x02, 0x00, 0x02]);
    }
}
