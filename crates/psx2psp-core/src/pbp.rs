//! EBOOT.PBP container: a 40-byte header of 8 monotonically increasing offsets followed by
//! the section payloads. Section *i* spans `offsets[i]..offsets[i+1]` (DATA.PSAR runs to EOF).

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use crate::error::{Error, Result};

pub const PBP_MAGIC: [u8; 4] = *b"\0PBP";
pub const PBP_VERSION: u32 = 0x0001_0000;
pub const PBP_HEADER_LEN: u32 = 0x28;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    ParamSfo,
    Icon0,
    Icon1,
    Pic0,
    Pic1,
    Snd0,
    DataPsp,
    DataPsar,
}

impl Section {
    pub const ALL: [Section; 8] = [
        Self::ParamSfo,
        Self::Icon0,
        Self::Icon1,
        Self::Pic0,
        Self::Pic1,
        Self::Snd0,
        Self::DataPsp,
        Self::DataPsar,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::ParamSfo => "PARAM.SFO",
            Self::Icon0 => "ICON0.PNG",
            Self::Icon1 => "ICON1.PMF",
            Self::Pic0 => "PIC0.PNG",
            Self::Pic1 => "PIC1.PNG",
            Self::Snd0 => "SND0.AT3",
            Self::DataPsp => "DATA.PSP",
            Self::DataPsar => "DATA.PSAR",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PbpHeader {
    pub version: u32,
    pub offsets: [u32; 8],
}

impl PbpHeader {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < PBP_HEADER_LEN as usize || b[..4] != PBP_MAGIC {
            return Err(Error::format("PBP", "bad magic (not an EBOOT.PBP)"));
        }
        let rd = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let mut offsets = [0u32; 8];
        for (i, o) in offsets.iter_mut().enumerate() {
            *o = rd(8 + i * 4);
        }
        if offsets[0] < PBP_HEADER_LEN || offsets.windows(2).any(|w| w[1] < w[0]) {
            return Err(Error::format("PBP", format!("offsets not monotonic: {offsets:x?}")));
        }
        Ok(Self { version: rd(4), offsets })
    }

    pub fn to_bytes(&self) -> [u8; PBP_HEADER_LEN as usize] {
        let mut b = [0u8; PBP_HEADER_LEN as usize];
        b[..4].copy_from_slice(&PBP_MAGIC);
        b[4..8].copy_from_slice(&self.version.to_le_bytes());
        for (i, o) in self.offsets.iter().enumerate() {
            b[8 + i * 4..12 + i * 4].copy_from_slice(&o.to_le_bytes());
        }
        b
    }
}

/// Random-access view over an existing PBP.
pub struct PbpFile<R> {
    inner: R,
    header: PbpHeader,
    len: u64,
}

impl<R: Read + Seek> PbpFile<R> {
    pub fn open(mut inner: R) -> Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        let mut h = [0u8; PBP_HEADER_LEN as usize];
        inner.read_exact(&mut h).map_err(|_| Error::format("PBP", "file shorter than header"))?;
        let header = PbpHeader::parse(&h)?;
        if header.offsets[7] as u64 > len {
            return Err(Error::format("PBP", "DATA.PSAR offset beyond end of file"));
        }
        Ok(Self { inner, header, len })
    }

    pub fn header(&self) -> &PbpHeader {
        &self.header
    }

    pub fn range(&self, s: Section) -> Range<u64> {
        let i = s.index();
        let start = self.header.offsets[i] as u64;
        let end = self.header.offsets.get(i + 1).map_or(self.len, |&e| e as u64);
        start..end
    }

    /// Reads a whole section (refuses >64 MiB to protect against hostile headers; use
    /// [`Self::section_reader`] for DATA.PSAR).
    pub fn read(&mut self, s: Section) -> Result<Vec<u8>> {
        let r = self.range(s);
        let n = r.end - r.start;
        if n > 64 << 20 {
            return Err(Error::format("PBP", format!("{} too large to buffer ({n} bytes)", s.file_name())));
        }
        let mut v = vec![0u8; n as usize];
        self.inner.seek(SeekFrom::Start(r.start))?;
        self.inner.read_exact(&mut v)?;
        Ok(v)
    }

    /// Streaming reader positioned at the section start, limited to its length.
    pub fn section_reader(&mut self, s: Section) -> Result<io::Take<&mut R>> {
        let r = self.range(s);
        self.inner.seek(SeekFrom::Start(r.start))?;
        Ok((&mut self.inner).take(r.end - r.start))
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    pub fn inner_mut(&mut self) -> &mut R {
        &mut self.inner
    }
}

/// Everything except DATA.PSAR, which is streamed by the caller.
#[derive(Debug, Clone, Default)]
pub struct PbpSections {
    pub param_sfo: Vec<u8>,
    pub icon0: Option<Vec<u8>>,
    pub icon1: Option<Vec<u8>>,
    pub pic0: Option<Vec<u8>>,
    pub pic1: Option<Vec<u8>>,
    pub snd0: Option<Vec<u8>>,
    pub data_psp: Vec<u8>,
}

/// Writes a PBP. DATA.PSAR starts at an offset aligned to `psar_align` (power of two, ≥1)
/// and is produced by `write_psar`, which receives the writer positioned at that offset and
/// must use offsets relative to it.
pub fn write_pbp<W: Write + Seek>(
    w: &mut W,
    sections: &PbpSections,
    psar_align: u64,
    write_psar: impl FnOnce(&mut W, u64) -> Result<()>,
) -> Result<()> {
    debug_assert!(psar_align.is_power_of_two());
    let parts: [&[u8]; 7] = [
        &sections.param_sfo,
        sections.icon0.as_deref().unwrap_or_default(),
        sections.icon1.as_deref().unwrap_or_default(),
        sections.pic0.as_deref().unwrap_or_default(),
        sections.pic1.as_deref().unwrap_or_default(),
        sections.snd0.as_deref().unwrap_or_default(),
        &sections.data_psp,
    ];
    let start = w.stream_position()?;
    let mut header = PbpHeader { version: PBP_VERSION, offsets: [0; 8] };
    let mut off = PBP_HEADER_LEN as u64;
    for (i, p) in parts.iter().enumerate() {
        header.offsets[i] = u32::try_from(off).map_err(|_| Error::format("PBP", "sections exceed 4 GiB"))?;
        off += p.len() as u64;
    }
    let psar = off.next_multiple_of(psar_align);
    header.offsets[7] = u32::try_from(psar).map_err(|_| Error::format("PBP", "sections exceed 4 GiB"))?;

    w.write_all(&header.to_bytes())?;
    for p in parts {
        w.write_all(p)?;
    }
    io::copy(&mut io::repeat(0).take(psar - off), w)?;
    write_psar(w, start + psar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn write_then_read_every_section() {
        let s = PbpSections {
            param_sfo: b"SFO!".to_vec(),
            icon0: Some(vec![1; 10]),
            icon1: None,
            pic0: Some(vec![2; 3]),
            pic1: Some(vec![3; 7]),
            snd0: None,
            data_psp: vec![4; 33],
        };
        let mut c = Cursor::new(Vec::new());
        write_pbp(&mut c, &s, 0x10000, |w, base| {
            assert_eq!(base % 0x10000, 0);
            w.write_all(b"PSAR")?;
            Ok(())
        })
        .unwrap();
        let mut p = PbpFile::open(Cursor::new(c.into_inner())).unwrap();
        assert_eq!(p.header().version, PBP_VERSION);
        assert_eq!(p.read(Section::ParamSfo).unwrap(), b"SFO!");
        assert_eq!(p.read(Section::Icon0).unwrap(), vec![1; 10]);
        assert!(p.read(Section::Icon1).unwrap().is_empty());
        assert_eq!(p.read(Section::Pic1).unwrap(), vec![3; 7]);
        // DATA.PSP's range runs up to the aligned PSAR offset: payload followed by zero padding.
        let psp = p.read(Section::DataPsp).unwrap();
        assert_eq!(&psp[..33], &[4; 33]);
        assert!(psp[33..].iter().all(|&b| b == 0));
        assert_eq!(p.read(Section::DataPsar).unwrap(), b"PSAR");
        assert_eq!(p.range(Section::DataPsar).start, 0x10000);
    }

    #[test]
    fn rejects_garbage() {
        assert!(PbpFile::open(Cursor::new(vec![0u8; 10])).is_err());
        let mut h = PbpHeader { version: 1, offsets: [0x28, 0x30, 0x20, 0, 0, 0, 0, 0] }.to_bytes().to_vec();
        h.resize(0x100, 0);
        assert!(PbpFile::open(Cursor::new(h)).is_err());
    }
}
