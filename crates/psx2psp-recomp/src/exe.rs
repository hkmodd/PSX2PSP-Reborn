//! PS-X EXE: the PS1 executable format.
//!
//! A 0x800-byte header (magic `PS-X EXE`, entry point, `gp`, load address and size of the text,
//! BSS, initial stack) followed by the text, loaded as one block at its address.

use thiserror::Error;

const HEADER: usize = 0x800;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExeError {
    #[error("not a PS-X EXE (bad magic)")]
    Magic,
    #[error("PS-X EXE truncated: text needs {need} bytes, file has {have}")]
    Truncated { need: usize, have: usize },
    #[error("PS-X EXE text at {0:#010x} is not word-aligned or does not fit in 32 bits")]
    Layout(u32),
    #[error("PS-X EXE entry point {0:#010x} is outside its text")]
    Entry(u32),
}

/// A parsed PS-X EXE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsxExe {
    /// Entry point.
    pub pc0: u32,
    /// Initial `gp`.
    pub gp0: u32,
    /// Load address of the text.
    pub text_addr: u32,
    /// The text (code and initialised data), as loaded in RAM.
    pub text: Vec<u8>,
    /// BSS to clear before starting (address, size).
    pub bss: (u32, u32),
    /// Initial stack (base address, size); both 0 when the loader keeps its own.
    pub stack: (u32, u32),
}

impl PsxExe {
    pub fn parse(bytes: &[u8]) -> Result<Self, ExeError> {
        if bytes.len() < HEADER || &bytes[..8] != b"PS-X EXE" {
            return Err(ExeError::Magic);
        }
        let w = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let (pc0, gp0, text_addr, size) = (w(0x10), w(0x14), w(0x18), w(0x1C) as usize);
        let need = HEADER + size;
        if bytes.len() < need {
            return Err(ExeError::Truncated { need, have: bytes.len() });
        }
        if text_addr & 3 != 0 || size % 4 != 0 || text_addr.checked_add(size as u32).is_none() {
            return Err(ExeError::Layout(text_addr));
        }
        let exe = PsxExe {
            pc0,
            gp0,
            text_addr,
            text: bytes[HEADER..need].to_vec(),
            bss: (w(0x28), w(0x2C)),
            stack: (w(0x30), w(0x34)),
        };
        if !exe.contains(pc0) {
            return Err(ExeError::Entry(pc0));
        }
        Ok(exe)
    }

    /// End of the text (exclusive).
    pub fn text_end(&self) -> u32 {
        self.text_addr + self.text.len() as u32
    }

    /// True if `addr` is a word inside the text.
    pub fn contains(&self, addr: u32) -> bool {
        addr & 3 == 0 && (self.text_addr..self.text_end()).contains(&addr)
    }

    /// The word at `addr`, if inside the text.
    pub fn word(&self, addr: u32) -> Option<u32> {
        if !self.contains(addr) {
            return None;
        }
        let o = (addr - self.text_addr) as usize;
        Some(u32::from_le_bytes(self.text[o..o + 4].try_into().unwrap()))
    }
}
