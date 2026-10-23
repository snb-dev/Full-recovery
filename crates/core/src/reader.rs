//! Buffered cursor used by carvers to parse a candidate file.

use std::io;

use memchr::memmem;

use crate::source::{read_fully, Source};

const BUF_SIZE: usize = 64 * 1024;

pub(crate) fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "end of candidate data")
}

/// Cursor over a [`Source`] starting at a candidate file's first byte.
///
/// All offsets are relative to that first byte. Reading past the carver's
/// maximum length or the end of the source fails with `UnexpectedEof`.
pub struct Reader<'a> {
    src: &'a dyn Source,
    start: u64,
    limit: u64,
    pos: u64,
    buf: Vec<u8>,
    buf_start: u64,
}

impl<'a> Reader<'a> {
    pub fn new(src: &'a dyn Source, start: u64, max_len: u64) -> Self {
        let limit = src.len().saturating_sub(start).min(max_len);
        Self {
            src,
            start,
            limit,
            pos: 0,
            buf: Vec::new(),
            buf_start: 0,
        }
    }

    /// Current offset from the start of the candidate.
    pub fn offset(&self) -> u64 {
        self.pos
    }

    /// Largest offset that can be read up to.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.pos)
    }

    pub fn seek(&mut self, offset: u64) {
        self.pos = offset;
    }

    pub fn skip(&mut self, n: u64) -> io::Result<()> {
        self.pos = self.pos.saturating_add(n);
        if self.pos > self.limit {
            return Err(eof());
        }
        Ok(())
    }

    /// Returns up to `n` bytes at the cursor without consuming them. The slice
    /// is shorter than `n` only at the end of the readable range.
    pub fn peek(&mut self, n: usize) -> io::Result<&[u8]> {
        let buf_end = self.buf_start + self.buf.len() as u64;
        if self.pos < self.buf_start || self.pos.saturating_add(n as u64) > buf_end {
            self.load(n)?;
        }
        let from = (self.pos - self.buf_start) as usize;
        let to = from.saturating_add(n).min(self.buf.len());
        Ok(&self.buf[from.min(to)..to])
    }

    fn load(&mut self, n: usize) -> io::Result<()> {
        let want = (n.max(BUF_SIZE) as u64).min(self.remaining()) as usize;
        self.buf.resize(want, 0);
        let got = read_fully(self.src, self.start + self.pos, &mut self.buf)?;
        self.buf.truncate(got);
        self.buf_start = self.pos;
        Ok(())
    }

    pub fn bytes<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let s = self.peek(N)?;
        if s.len() < N {
            return Err(eof());
        }
        let mut out = [0u8; N];
        out.copy_from_slice(s);
        self.pos += N as u64;
        Ok(out)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes::<1>()?[0])
    }

    pub fn u16_be(&mut self) -> io::Result<u16> {
        self.bytes().map(u16::from_be_bytes)
    }

    pub fn u32_be(&mut self) -> io::Result<u32> {
        self.bytes().map(u32::from_be_bytes)
    }

    pub fn u32_le(&mut self) -> io::Result<u32> {
        self.bytes().map(u32::from_le_bytes)
    }

    /// Feeds the next `n` bytes to `f` piece by piece and moves past them.
    pub fn consume(&mut self, mut n: u64, mut f: impl FnMut(&[u8])) -> io::Result<()> {
        while n > 0 {
            let s = self.peek(n.min(BUF_SIZE as u64) as usize)?;
            if s.is_empty() {
                return Err(eof());
            }
            let k = s.len();
            f(s);
            self.pos += k as u64;
            n -= k as u64;
        }
        Ok(())
    }

    /// Moves to the next occurrence of `needle` and returns its offset, or
    /// returns `None` (with the cursor at the limit) if there is none.
    pub fn find(&mut self, needle: &[u8]) -> io::Result<Option<u64>> {
        let finder = memmem::Finder::new(needle);
        loop {
            let s = self.peek(BUF_SIZE)?;
            if let Some(i) = finder.find(s) {
                self.pos += i as u64;
                return Ok(Some(self.pos));
            }
            if s.len() < BUF_SIZE || s.len() < needle.len() {
                self.pos = self.limit;
                return Ok(None);
            }
            self.pos += (s.len() + 1 - needle.len()) as u64;
        }
    }

    /// Reads up to `n` bytes at `offset` without moving the cursor.
    pub fn read_at(&self, offset: u64, n: usize) -> io::Result<Vec<u8>> {
        let avail = self.limit.saturating_sub(offset).min(n as u64) as usize;
        let mut out = vec![0u8; avail];
        let got = read_fully(self.src, self.start + offset, &mut out)?;
        out.truncate(got);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_across_buffer_boundary() {
        let mut data = vec![0u8; BUF_SIZE * 3];
        let at = BUF_SIZE * 2 - 2;
        data[at..at + 5].copy_from_slice(b"%%EOF");
        let mut r = Reader::new(&data, 0, u64::MAX);
        assert_eq!(r.find(b"%%EOF").unwrap(), Some(at as u64));
        r.skip(5).unwrap();
        assert_eq!(r.find(b"%%EOF").unwrap(), None);
        assert_eq!(r.offset(), data.len() as u64);
    }

    #[test]
    fn reads_stop_at_limit() {
        let data: Vec<u8> = (0..100).collect();
        let mut r = Reader::new(&data, 10, 4);
        assert_eq!(r.u32_be().unwrap(), u32::from_be_bytes([10, 11, 12, 13]));
        assert_eq!(r.u8().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }
}
