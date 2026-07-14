//! Minimal MSB-first bit reader used by all codec header parsers.

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// Read `n` bits (n <= 64), MSB first. Returns None on end of data.
    pub fn read(&mut self, n: u32) -> Option<u64> {
        debug_assert!(n <= 64);
        if self.pos + n as usize > self.data.len() * 8 {
            return None;
        }
        let mut v: u64 = 0;
        for _ in 0..n {
            let byte = self.data[self.pos >> 3];
            let bit = (byte >> (7 - (self.pos & 7))) & 1;
            v = (v << 1) | bit as u64;
            self.pos += 1;
        }
        Some(v)
    }

    pub fn read_u32(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        self.read(n).map(|v| v as u32)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }
}

/// Find the first occurrence of `pat` in `hay`.
pub fn find_pattern(hay: &[u8], pat: &[u8]) -> Option<usize> {
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    hay.windows(pat.len()).position(|w| w == pat)
}

pub fn contains_pattern(hay: &[u8], pat: &[u8]) -> bool {
    find_pattern(hay, pat).is_some()
}

#[cfg(test)]
pub mod tests_support {
    /// MSB-first bit writer for constructing synthetic headers in tests.
    pub struct BitWriter {
        bits: Vec<bool>,
    }

    impl BitWriter {
        pub fn new() -> Self {
            BitWriter { bits: Vec::new() }
        }

        pub fn put(&mut self, n: u32, v: u64) {
            for i in (0..n).rev() {
                self.bits.push((v >> i) & 1 == 1);
            }
        }

        pub fn finish(&self) -> Vec<u8> {
            let mut out = vec![0u8; self.bits.len().div_ceil(8)];
            for (i, b) in self.bits.iter().enumerate() {
                if *b {
                    out[i >> 3] |= 1 << (7 - (i & 7));
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitreader_reads_msb_first() {
        let data = [0b1010_1100, 0b0101_0011];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(3), Some(0b101));
        assert_eq!(r.read(5), Some(0b01100));
        assert_eq!(r.read(8), Some(0b0101_0011));
        assert_eq!(r.read(1), None);
    }

    #[test]
    fn pattern_search() {
        assert_eq!(find_pattern(b"abcdef", b"cd"), Some(2));
        assert_eq!(find_pattern(b"abcdef", b"xy"), None);
    }
}
