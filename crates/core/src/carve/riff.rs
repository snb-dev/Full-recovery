//! RIFF containers: WAV audio, AVI video and WebP images.

use std::io;

use super::{Carved, Carver, Confidence};
use crate::reader::Reader;

pub struct Riff;

impl Carver for Riff {
    fn id(&self) -> &'static str {
        "riff"
    }

    fn description(&self) -> &'static str {
        "RIFF media (WAV, AVI, WebP)"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["wav", "avi", "webp"]
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn first_bytes(&self) -> &'static [u8] {
        b"R"
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.len() >= 12
            && head.starts_with(b"RIFF")
            && matches!(&head[8..12], b"WAVE" | b"AVI " | b"WEBP")
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        let mut total = 0u64;
        let mut ext = "";
        let mut clean = true;
        loop {
            r.seek(total);
            let id: [u8; 4] = r.bytes()?;
            let size = u64::from(r.u32_le()?);
            let form: [u8; 4] = r.bytes()?;
            if total == 0 {
                ext = match &form {
                    b"WAVE" => "wav",
                    b"AVI " => "avi",
                    _ => "webp",
                };
            } else if !(&id == b"RIFF" && &form == b"AVIX") {
                break;
            }
            let end = total + 8 + size;
            if size < 4 || end > r.limit() {
                if total == 0 {
                    return Ok(None);
                }
                break;
            }
            let mut p = total + 12;
            while p + 8 <= end {
                r.seek(p);
                let chunk_id: [u8; 4] = r.bytes()?;
                if !chunk_id.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
                    clean = false;
                    break;
                }
                let chunk_size = u64::from(r.u32_le()?);
                p += 8 + chunk_size + (chunk_size & 1);
            }
            if p != end && p != end + 1 {
                clean = false;
            }
            total = end;
            // Large AVI files continue in extra "AVIX" RIFF chunks.
            if ext != "avi" || total + 12 > r.limit() {
                break;
            }
        }
        let conf = if clean {
            Confidence::High
        } else {
            Confidence::Medium
        };
        Ok(Some(Carved::new(total, conf, ext)))
    }
}
