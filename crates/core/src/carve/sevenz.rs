use std::io;

use super::{le32, le64, Carved, Carver, Confidence};
use crate::reader::Reader;

const SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];

pub struct SevenZip;

impl Carver for SevenZip {
    fn id(&self) -> &'static str {
        "7z"
    }

    fn description(&self) -> &'static str {
        "7-Zip archive"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["7z"]
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn first_bytes(&self) -> &'static [u8] {
        b"7"
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.len() >= 32 && head.starts_with(&SIGNATURE) && head[6] == 0
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        let h: [u8; 32] = r.bytes()?;
        if crc32fast::hash(&h[12..32]) != le32(&h, 8) {
            return Ok(None);
        }
        let (next_offset, next_size) = (le64(&h, 12), le64(&h, 20));
        let Some(len) = next_offset
            .checked_add(next_size)
            .and_then(|n| n.checked_add(32))
            .filter(|len| next_size > 0 && *len <= r.limit())
        else {
            return Ok(None);
        };
        let mut conf = Confidence::Medium;
        if next_size <= 16 << 20 {
            r.seek(32 + next_offset);
            let mut crc = crc32fast::Hasher::new();
            r.consume(next_size, |s| crc.update(s))?;
            if crc.finalize() == le32(&h, 28) {
                conf = Confidence::High;
            }
        }
        Ok(Some(Carved::new(len, conf, "7z")))
    }
}
