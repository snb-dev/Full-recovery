use std::io;

use memchr::memchr;

use super::{Carved, Carver, Confidence};
use crate::reader::{eof, Reader};

pub struct Jpeg;

#[derive(Default)]
struct State {
    frame: bool,
    scan: bool,
    /// End of the last complete segment, used for partial recovery.
    good: u64,
}

impl Carver for Jpeg {
    fn id(&self) -> &'static str {
        "jpg"
    }

    fn description(&self) -> &'static str {
        "JPEG image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["jpg"]
    }

    fn max_len(&self) -> u64 {
        128 << 20
    }

    fn first_bytes(&self) -> &'static [u8] {
        &[0xFF]
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.len() >= 4
            && head[..3] == [0xFF, 0xD8, 0xFF]
            && matches!(head[3], 0xC0..=0xCF | 0xDB | 0xDD | 0xE0..=0xEF | 0xFE)
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        let mut st = State::default();
        match walk(r, &mut st)? {
            Some(c) => Ok(Some(c)),
            // The structure broke after image data started: the file is damaged
            // or fragmented, so keep the part that decodes.
            None if st.scan => Ok(Some(Carved::new(st.good, Confidence::Low, "jpg"))),
            None => Ok(None),
        }
    }
}

fn walk(r: &mut Reader<'_>, st: &mut State) -> io::Result<Option<Carved>> {
    r.skip(2)?;
    loop {
        st.good = r.offset();
        if r.u8()? != 0xFF {
            return Ok(None);
        }
        let mut marker = r.u8()?;
        while marker == 0xFF {
            marker = r.u8()?;
        }
        match marker {
            0xD9 => {
                let conf = if st.frame && st.scan {
                    Confidence::High
                } else {
                    Confidence::Medium
                };
                return Ok(Some(Carved::new(r.offset(), conf, "jpg")));
            }
            0x01 | 0xD0..=0xD7 => {}
            0x00 | 0x02..=0xBF | 0xD8 => return Ok(None),
            _ => {
                let len = r.u16_be()?;
                if len < 2 {
                    return Ok(None);
                }
                if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
                    st.frame = true;
                }
                r.skip(u64::from(len) - 2)?;
                if marker == 0xDA {
                    st.scan = true;
                    skip_entropy_data(r)?;
                }
            }
        }
    }
}

/// Moves past compressed image data to the next marker.
fn skip_entropy_data(r: &mut Reader<'_>) -> io::Result<()> {
    loop {
        let s = r.peek(64 * 1024)?;
        if s.is_empty() {
            return Err(eof());
        }
        let Some(i) = memchr(0xFF, s) else {
            let n = s.len() as u64;
            r.skip(n)?;
            continue;
        };
        r.skip(i as u64)?;
        let pair = r.peek(2)?;
        if pair.len() < 2 {
            return Err(eof());
        }
        match pair[1] {
            // Stuffed byte or restart marker: still image data.
            0x00 | 0xD0..=0xD7 => r.skip(2)?,
            0xFF => r.skip(1)?,
            _ => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn complete_jpeg() {
        let data = fixtures::jpeg();
        let mut r = Reader::new(&data, 0, u64::MAX);
        let c = Jpeg.carve(&mut r).unwrap().unwrap();
        assert_eq!(c, Carved::new(data.len() as u64, Confidence::High, "jpg"));
    }

    #[test]
    fn broken_jpeg_is_partial() {
        let mut data = fixtures::jpeg();
        let cut = data.len() - 2;
        data.truncate(cut);
        // Garbage where the end marker should be.
        data.extend_from_slice(&[0xFF, 0x05, 1, 2, 3]);
        let mut r = Reader::new(&data, 0, u64::MAX);
        let c = Jpeg.carve(&mut r).unwrap().unwrap();
        assert_eq!(c.confidence, Confidence::Low);
        assert_eq!(c.len, cut as u64);
    }
}
