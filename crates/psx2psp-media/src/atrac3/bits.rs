//! MSB-first bit writer (ATRAC3 packs every field big-endian, bit by bit).

#[derive(Debug, Default)]
pub struct BitWriter {
    buf: Vec<u8>,
    len: usize,
}

impl BitWriter {
    pub fn with_capacity(bytes: usize) -> Self {
        Self { buf: Vec::with_capacity(bytes), len: 0 }
    }

    /// Appends the low `n` bits of `v`, most significant first.
    pub fn put(&mut self, v: u32, n: u32) {
        debug_assert!(n <= 32 && (n == 32 || v >> n == 0), "{v:#x} does not fit {n} bits");
        for i in (0..n).rev() {
            if self.len.is_multiple_of(8) {
                self.buf.push(0);
            }
            if (v >> i) & 1 != 0 {
                *self.buf.last_mut().expect("pushed above") |= 0x80 >> (self.len % 8);
            }
            self.len += 1;
        }
    }

    /// Appends `v` as an `n`-bit two's-complement field.
    pub fn put_signed(&mut self, v: i32, n: u32) {
        debug_assert!((-(1 << (n - 1))..1 << (n - 1)).contains(&v), "{v} does not fit {n} signed bits");
        self.put(v as u32 & ((1u32 << n) - 1), n);
    }

    #[cfg(test)]
    pub fn bits(&self) -> usize {
        self.len
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_msb_first_across_bytes() {
        let mut w = BitWriter::default();
        w.put(0x28, 6);
        w.put(3, 2);
        w.put(0b101, 3);
        w.put_signed(-1, 4);
        w.put(0, 1);
        assert_eq!(w.bits(), 16);
        assert_eq!(w.into_bytes(), [0xA3, 0b1011_1110]);
    }
}
