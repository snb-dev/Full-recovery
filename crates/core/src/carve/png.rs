use std::io;

use super::{Carved, Carver, Confidence};
use crate::reader::Reader;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

pub struct Png;

impl Carver for Png {
    fn id(&self) -> &'static str {
        "png"
    }

    fn description(&self) -> &'static str {
        "PNG image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["png"]
    }

    fn max_len(&self) -> u64 {
        256 << 20
    }

    fn first_bytes(&self) -> &'static [u8] {
        &[0x89]
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.len() >= 16 && head[..8] == SIGNATURE && &head[12..16] == b"IHDR"
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        r.skip(8)?;
        let mut crc_ok = true;
        let mut image_data = false;
        loop {
            let chunk_start = r.offset();
            let len = r.u32_be()?;
            let ty: [u8; 4] = r.bytes()?;
            if len > 0x7FFF_FFFF || !ty.iter().all(u8::is_ascii_alphabetic) {
                return Ok(image_data.then(|| Carved::new(chunk_start, Confidence::Low, "png")));
            }
            let mut crc = crc32fast::Hasher::new();
            crc.update(&ty);
            r.consume(len.into(), |s| crc.update(s))?;
            if r.u32_be()? != crc.finalize() {
                crc_ok = false;
            }
            match &ty {
                b"IDAT" => image_data = true,
                b"IEND" => {
                    let conf = if crc_ok && image_data {
                        Confidence::High
                    } else {
                        Confidence::Medium
                    };
                    return Ok(Some(Carved::new(r.offset(), conf, "png")));
                }
                _ => {}
            }
        }
    }
}
