//! End-to-end: synthetic disc → EBOOT.PBP → parse → extract → byte-identical image.

use std::io::{Cursor, Seek, Write};
use std::path::Path;

use psx2psp_core::disc::RAW_SECTOR;
use psx2psp_core::pbp::{PbpSections, write_pbp};
use psx2psp_core::psar::{self, BLOCK, ISO_DATA};
use psx2psp_core::{DiscImage, DiscSpec, Donor, ImagePatch, PbpFile, PsarJob, Section, Sfo};

const S: usize = RAW_SECTOR as usize;

fn raw_sector(lba: u32, user: &[u8]) -> Vec<u8> {
    let mut s = vec![0u8; S];
    s[..12].copy_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
    s[15] = 2;
    s[24..24 + user.len()].copy_from_slice(user);
    // Pseudo-random filler so compression is non-trivial but deterministic.
    let mut x = lba.wrapping_mul(0x9E37_79B9) | 1;
    for b in &mut s[24 + user.len()..24 + 2048] {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *b = if x.is_multiple_of(4) { x as u8 } else { 0 };
    }
    s
}

/// 300-sector disc with an ISO9660 PVD (LBA 16), root dir (LBA 18) and SYSTEM.CNF (LBA 19).
fn make_disc(dir: &Path, name: &str, id_exe: &str, sectors: u32) -> std::path::PathBuf {
    let mut img = Vec::with_capacity(sectors as usize * S);
    for lba in 0..sectors {
        let user: Vec<u8> = match lba {
            16 => {
                let mut p = vec![0u8; 2048];
                p[0] = 1;
                p[1..6].copy_from_slice(b"CD001");
                p[40..48].copy_from_slice(b"TESTVOL ");
                let root = &mut p[156..190];
                root[0] = 34;
                root[2..6].copy_from_slice(&18u32.to_le_bytes());
                root[10..14].copy_from_slice(&2048u32.to_le_bytes());
                p
            }
            18 => {
                let name = b"SYSTEM.CNF;1";
                let mut d = vec![0u8; 64];
                d[0] = (33 + name.len() + 1) as u8;
                d[2..6].copy_from_slice(&19u32.to_le_bytes());
                let cnf = format!("BOOT = cdrom:\\{id_exe};1\r\n");
                d[10..14].copy_from_slice(&(cnf.len() as u32).to_le_bytes());
                d[32] = name.len() as u8;
                d[33..33 + name.len()].copy_from_slice(name);
                d
            }
            19 => format!("BOOT = cdrom:\\{id_exe};1\r\n").into_bytes(),
            _ => Vec::new(),
        };
        img.extend(raw_sector(lba, &user));
    }
    let bin = dir.join(format!("{name}.bin"));
    std::fs::write(&bin, &img).unwrap();
    let cue = dir.join(format!("{name}.cue"));
    let audio_at = sectors - 50;
    let (m, s, f) = (audio_at / 75 / 60, audio_at / 75 % 60, audio_at % 75);
    std::fs::write(
        &cue,
        format!("FILE \"{name}.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 {m:02}:{s:02}:{f:02}\n"),
    )
    .unwrap();
    cue
}

fn fake_donor() -> Donor {
    let mut pgd = b"\0PGD\x01\0\0\0\x01\0\0\0".to_vec();
    pgd.resize(0x1370, 0xEE);
    Donor { data_psp: b"~PSP-fake-loader".to_vec(), boot_png: b"\x89PNG-boot".to_vec(), pgd, title_consts: None }
}

fn build(discs: &[(&DiscImage, &str)], patches: &[&dyn ImagePatch], donor: &Donor) -> Vec<u8> {
    let job = PsarJob {
        discs: discs
            .iter()
            .enumerate()
            .map(|(i, (img, id))| DiscSpec {
                image: img,
                disc_id: id.to_string(),
                title: format!("Test Game (Disc {})", i + 1),
                patches: patches.to_vec(),
            })
            .collect(),
        title: "Test Game".into(),
        level: 9,
        boot_png: &donor.boot_png,
        donor,
    };
    let sections = PbpSections {
        param_sfo: Sfo::pops("Test Game", discs[0].1).to_bytes(),
        icon0: Some(b"icon0".to_vec()),
        data_psp: donor.data_psp.clone(),
        ..Default::default()
    };
    let mut out = Cursor::new(Vec::new());
    write_pbp(&mut out, &sections, 0x10000, |w, base| {
        psar::write_psar(w, base, &job, &()).map(|_| ())
    })
    .unwrap();
    out.into_inner()
}

#[test]
fn single_disc_roundtrip_is_bit_exact() {
    let dir = tempfile::tempdir().unwrap();
    let cue = make_disc(dir.path(), "game", "SLUS_005.94", 300);
    let disc = DiscImage::open(&cue).unwrap();
    assert_eq!(disc.probe().disc_id.as_deref(), Some("SLUS00594"));
    assert_eq!(disc.probe().volume_id.as_deref(), Some("TESTVOL"));

    let donor = fake_donor();
    let pbp_bytes = build(&[(&disc, "SLUS00594")], &[], &donor);
    let mut pbp = PbpFile::open(Cursor::new(pbp_bytes)).unwrap();

    let sfo = Sfo::parse(&pbp.read(Section::ParamSfo).unwrap()).unwrap();
    assert_eq!(sfo.get_str("DISC_ID"), Some("SLUS00594"));
    let psar_at = pbp.range(Section::DataPsar).start;
    assert_eq!(psar_at % 0x10000, 0);

    let discs = psar::read_discs(pbp.inner_mut(), psar_at).unwrap();
    assert_eq!(discs.len(), 1);
    let d = &discs[0];
    assert_eq!(d.disc_id, "SLUS00594");
    assert_eq!(d.title, "Test Game (Disc 1)");
    assert_eq!(d.index.len(), (300 * S).div_ceil(BLOCK));
    let tracks = d.tracks();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[1].start, 250);
    assert_eq!(d.image_size(), 300 * RAW_SECTOR);

    let mut extracted = Vec::new();
    psar::extract_disc(pbp.inner_mut(), psar_at, d, &mut extracted, &()).unwrap();
    let original = std::fs::read(dir.path().join("game.bin")).unwrap();
    assert!(extracted == original, "extracted image differs from source");

    // The donor round-trips through our own output (STARTDAT/p1 consistency).
    pbp.inner_mut().rewind().unwrap();
    let again = Donor::from_pbp(&mut pbp).unwrap();
    assert_eq!(again.boot_png, donor.boot_png);
    assert_eq!(again.pgd, donor.pgd);
    assert!(again.data_psp.starts_with(&donor.data_psp));

    // Header invariants.
    let raw = pbp.into_inner().into_inner();
    let h = &raw[psar_at as usize..];
    let p1 = u32::from_le_bytes(h[0x0C..0x10].try_into().unwrap()) as usize;
    let p2 = u32::from_le_bytes(h[0x1220..0x1224].try_into().unwrap()) as usize;
    assert_eq!(&h[p1..p1 + 8], b"STARTDAT");
    assert_eq!(&h[p2..p2 + 4], b"\0PGD");
    assert_eq!(p1 % 16, 0);
    assert_eq!(&h[0x400..0x40B], b"_SLUS_00594");
    assert_eq!(u32::from_le_bytes(h[0xBFC..0xC00].try_into().unwrap()), ISO_DATA as u32);
    assert_eq!(&h[0x800..0x80A], &[0x41, 0, 0xA0, 0, 0, 0, 0, 0x01, 0x20, 0]);
}

struct Xor;
impl ImagePatch for Xor {
    fn apply(&self, offset: u64, block: &mut [u8]) {
        // Flip byte at absolute offset 100_000 if this block contains it.
        let at = 100_000u64;
        if (offset..offset + block.len() as u64).contains(&at) {
            block[(at - offset) as usize] ^= 0xFF;
        }
    }
    fn describe(&self) -> String {
        "xor".into()
    }
}

#[test]
fn multi_disc_roundtrip_with_patch() {
    let dir = tempfile::tempdir().unwrap();
    let d1 = DiscImage::open(make_disc(dir.path(), "d1", "SCES_021.05", 200)).unwrap();
    let d2 = DiscImage::open(make_disc(dir.path(), "d2", "SCES_121.05", 333)).unwrap();
    let donor = fake_donor();
    let bytes = build(&[(&d1, "SCES02105"), (&d2, "SCES12105")], &[&Xor], &donor);
    let mut pbp = PbpFile::open(Cursor::new(bytes)).unwrap();
    let psar_at = pbp.range(Section::DataPsar).start;
    let discs = psar::read_discs(pbp.inner_mut(), psar_at).unwrap();
    assert_eq!(discs.len(), 2);
    assert_eq!(discs[0].offset, 0x8000);
    assert_eq!(discs[1].offset % 0x8000, 0);
    assert_eq!(discs[1].disc_id, "SCES12105");

    for (d, name) in discs.iter().zip(["d1", "d2"]) {
        let mut out = Vec::new();
        psar::extract_disc(pbp.inner_mut(), psar_at, d, &mut out, &()).unwrap();
        let mut orig = std::fs::read(dir.path().join(format!("{name}.bin"))).unwrap();
        orig[100_000] ^= 0xFF;
        assert!(out == orig, "{name} mismatch");
    }
    pbp.inner_mut().rewind().unwrap();
    let back = Donor::from_pbp(&mut pbp).unwrap();
    assert_eq!(back.pgd, donor.pgd);
    assert!(back.title_consts.is_some());
}

#[test]
fn cancellation_is_honoured() {
    struct Stop;
    impl psx2psp_core::Monitor for Stop {
        fn cancelled(&self) -> bool {
            true
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let disc = DiscImage::open(make_disc(dir.path(), "c", "SLUS_000.01", 100)).unwrap();
    let donor = fake_donor();
    let job = PsarJob {
        discs: vec![DiscSpec { image: &disc, disc_id: "SLUS00001".into(), title: "t".into(), patches: vec![] }],
        title: "t".into(),
        level: 1,
        boot_png: &donor.boot_png,
        donor: &donor,
    };
    let mut c = Cursor::new(Vec::new());
    c.write_all(b"x").unwrap();
    assert!(matches!(psar::write_psar(&mut c, 1, &job, &Stop), Err(psx2psp_core::Error::Cancelled)));
}
