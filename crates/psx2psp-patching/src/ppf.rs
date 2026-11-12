//! PlayStation Patch Format (PPF 1.0 / 2.0 / 3.0) — the de-facto distribution format for
//! PS1 fixes (LibCrypt removal, PAL→NTSC "NTSC-fix", translations).
//!
//! Records are applied *in stream* to each image block before compression; the user's BIN is
//! never modified.

use psx2psp_core::ImagePatch;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PpfError {
    #[error("not a PPF file (bad magic)")]
    Magic,
    #[error("PPF truncated at byte {0}")]
    Truncated(usize),
    #[error("PPF image type {0} unsupported (only BIN)")]
    ImageType(u8),
    #[error("PPF uses undo data and was made for a different image")]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PpfVersion {
    V1,
    V2,
    V3,
}

#[derive(Debug, Clone)]
struct Record {
    offset: u64,
    data: Vec<u8>,
    seq: usize,
}

#[derive(Debug, Clone)]
pub struct PpfPatch {
    pub version: PpfVersion,
    pub description: String,
    /// 1024-byte block that must match the image at `validation_offset` (PPF2/3 optional).
    pub validation: Option<(u64, Vec<u8>)>,
    /// Sorted by offset (stable): enables O(log n) lookup per block.
    records: Vec<Record>,
}

const DIZ_BEGIN: &[u8] = b"@BEGIN_FILE_ID.DIZ";

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

impl PpfPatch {
    pub fn parse(buf: &[u8]) -> Result<Self, PpfError> {
        let version = match buf.get(..5) {
            Some(b"PPF10") => PpfVersion::V1,
            Some(b"PPF20") => PpfVersion::V2,
            Some(b"PPF30") => PpfVersion::V3,
            _ => return Err(PpfError::Magic),
        };
        let need = |n: usize| if buf.len() < n { Err(PpfError::Truncated(buf.len())) } else { Ok(()) };
        need(56)?;
        let description = String::from_utf8_lossy(&buf[6..56]).trim_end_matches(['\0', ' ']).to_string();

        let (mut pos, validation, undo) = match version {
            PpfVersion::V1 => (56, None, false),
            PpfVersion::V2 => {
                need(1084)?;
                (1084, Some((0x9320u64, buf[60..1084].to_vec())), false)
            }
            PpfVersion::V3 => {
                need(60)?;
                let image_type = buf[56];
                if image_type > 1 {
                    return Err(PpfError::ImageType(image_type));
                }
                let blockcheck = buf[57] != 0;
                let undo = buf[58] != 0;
                if blockcheck {
                    need(1084)?;
                    let at = if image_type == 0 { 0x9320 } else { 0x80A0 };
                    (1084, Some((at, buf[60..1084].to_vec())), undo)
                } else {
                    (60, None, undo)
                }
            }
        };

        let end = match version {
            PpfVersion::V1 => buf.len(),
            _ => rfind(&buf[pos..], DIZ_BEGIN).map_or(buf.len(), |p| pos + p),
        };
        let off_len = if version == PpfVersion::V3 { 8 } else { 4 };
        let mut records = Vec::new();
        while pos < end {
            let hdr = buf.get(pos..pos + off_len + 1).ok_or(PpfError::Truncated(pos))?;
            let offset = if off_len == 8 {
                u64::from_le_bytes(hdr[..8].try_into().unwrap())
            } else {
                u32::from_le_bytes(hdr[..4].try_into().unwrap()) as u64
            };
            let len = hdr[off_len] as usize;
            pos += off_len + 1;
            let data = buf.get(pos..pos + len).ok_or(PpfError::Truncated(pos))?.to_vec();
            pos += len;
            if undo {
                pos += len;
            }
            let seq = records.len();
            records.push(Record { offset, data, seq });
        }
        records.sort_by_key(|r| r.offset);
        Ok(Self { version, description, validation, records })
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    /// Checks the PPF2/3 validation block against the image bytes at its offset.
    pub fn validates(&self, image_bytes_at_offset: &[u8]) -> bool {
        self.validation.as_ref().is_none_or(|(_, v)| image_bytes_at_offset.get(..v.len()) == Some(v))
    }
}

impl ImagePatch for PpfPatch {
    fn apply(&self, offset: u64, block: &mut [u8]) {
        let end = offset + block.len() as u64;
        // Records are ≤255 bytes long: anything starting before `offset - 255` cannot reach the block.
        let first = self.records.partition_point(|r| r.offset + 255 < offset);
        let mut hits: Vec<&Record> = self.records[first..]
            .iter()
            .take_while(|r| r.offset < end)
            .filter(|r| r.offset + r.data.len() as u64 > offset)
            .collect();
        hits.sort_by_key(|r| r.seq); // overlapping records: later in file wins, as in ApplyPPF
        for r in hits {
            let s = r.offset.max(offset);
            let e = (r.offset + r.data.len() as u64).min(end);
            let src = &r.data[(s - r.offset) as usize..(e - r.offset) as usize];
            block[(s - offset) as usize..(e - offset) as usize].copy_from_slice(src);
        }
    }

    fn describe(&self) -> String {
        format!("PPF {:?} — {} ({} records)", self.version, self.description, self.records.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ppf3(records: &[(u64, &[u8])]) -> Vec<u8> {
        let mut b = b"PPF30\x02".to_vec();
        let mut desc = b"test patch".to_vec();
        desc.resize(50, b' ');
        b.extend(desc);
        b.extend([0, 0, 0, 0]); // BIN, no blockcheck, no undo, dummy
        for (o, d) in records {
            b.extend(o.to_le_bytes());
            b.push(d.len() as u8);
            b.extend(*d);
        }
        b.extend(b"@BEGIN_FILE_ID.DIZhello@END_FILE_ID.DIZ\x05\x00");
        b
    }

    #[test]
    fn ppf3_applies_across_block_boundaries() {
        let p = PpfPatch::parse(&ppf3(&[(10, b"ABCD"), (2350, b"XYZW"), (5000, b"Q")])).unwrap();
        assert_eq!(p.version, PpfVersion::V3);
        assert_eq!(p.description, "test patch");
        assert_eq!(p.record_count(), 3);

        let mut whole = vec![0u8; 6000];
        p.apply(0, &mut whole);

        // Same result when applied in two 2352-aligned halves (streaming path).
        let mut a = vec![0u8; 2352];
        let mut b = vec![0u8; 6000 - 2352];
        p.apply(0, &mut a);
        p.apply(2352, &mut b);
        a.extend(b);
        assert_eq!(a, whole);
        assert_eq!(&whole[10..14], b"ABCD");
        assert_eq!(&whole[2350..2354], b"XYZW");
        assert_eq!(whole[5000], b'Q');
    }

    #[test]
    fn later_record_wins_on_overlap() {
        let p = PpfPatch::parse(&ppf3(&[(4, b"BBBB"), (2, b"AAAA")])).unwrap();
        let mut blk = vec![0u8; 16];
        p.apply(0, &mut blk);
        assert_eq!(&blk[..9], b"\0\0AAAABB\0");
    }

    #[test]
    fn ppf1_and_truncation() {
        let mut b = b"PPF10\x00".to_vec();
        b.extend([b' '; 50]);
        b.extend(7u32.to_le_bytes());
        b.push(2);
        b.extend(b"hi");
        let p = PpfPatch::parse(&b).unwrap();
        let mut blk = [0u8; 16];
        p.apply(0, &mut blk);
        assert_eq!(&blk[7..9], b"hi");
        b.pop();
        assert!(matches!(PpfPatch::parse(&b), Err(PpfError::Truncated(_))));
        assert_eq!(PpfPatch::parse(b"nope").unwrap_err(), PpfError::Magic);
    }
}
