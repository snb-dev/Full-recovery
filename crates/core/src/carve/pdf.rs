use std::io;

use memchr::memmem;

use super::{Carved, Carver, Confidence};
use crate::reader::Reader;

pub struct Pdf;

impl Carver for Pdf {
    fn id(&self) -> &'static str {
        "pdf"
    }

    fn description(&self) -> &'static str {
        "PDF document"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["pdf"]
    }

    fn max_len(&self) -> u64 {
        512 << 20
    }

    fn first_bytes(&self) -> &'static [u8] {
        b"%"
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.len() >= 8
            && head.starts_with(b"%PDF-")
            && head[5].is_ascii_digit()
            && head[6] == b'.'
            && head[7].is_ascii_digit()
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        loop {
            let Some(at) = r.find(b"%%EOF")? else {
                return Ok(None);
            };
            r.skip(5)?;
            let eol = r.peek(2)?;
            let eol_len = if eol.starts_with(b"\r\n") {
                2
            } else if matches!(eol.first(), Some(b'\n' | b'\r')) {
                1
            } else {
                0
            };
            r.skip(eol_len)?;
            // Edited and "linearized" PDFs have more sections after an %%EOF.
            if continues(r.peek(64)?) {
                continue;
            }
            let tail_start = at.saturating_sub(1024);
            let tail = r.read_at(tail_start, (at - tail_start) as usize)?;
            let conf = if memmem::find(&tail, b"startxref").is_some() {
                Confidence::High
            } else {
                Confidence::Medium
            };
            return Ok(Some(Carved::new(r.offset(), conf, "pdf")));
        }
    }
}

/// Whether the bytes after an `%%EOF` start another section of the same PDF.
fn continues(s: &[u8]) -> bool {
    let s = s.trim_ascii_start();
    s.starts_with(b"xref") || is_object_header(s)
}

/// Matches `<number> <number> obj`.
fn is_object_header(s: &[u8]) -> bool {
    let digits = |s: &[u8]| s.iter().take_while(|b| b.is_ascii_digit()).count();
    let a = digits(s);
    if a == 0 || s.get(a) != Some(&b' ') {
        return false;
    }
    let s = &s[a + 1..];
    let b = digits(s);
    b > 0 && s[b..].starts_with(b" obj")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_headers() {
        assert!(is_object_header(b"12 0 obj\n<<"));
        assert!(!is_object_header(b"12 obj"));
        assert!(!is_object_header(b"x 0 obj"));
        assert!(continues(b"\n\nxref\n0 1"));
        assert!(!continues(b"\0\0\0\0"));
    }
}
