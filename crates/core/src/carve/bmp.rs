use std::io;

use super::{le16, le32, Carved, Carver, Confidence};
use crate::reader::Reader;

pub struct Bmp;

impl Carver for Bmp {
    fn id(&self) -> &'static str {
        "bmp"
    }

    fn description(&self) -> &'static str {
        "BMP image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["bmp"]
    }

    fn max_len(&self) -> u64 {
        512 << 20
    }

    fn first_bytes(&self) -> &'static [u8] {
        b"B"
    }

    fn matches(&self, head: &[u8]) -> bool {
        if head.len() < 30 || !head.starts_with(b"BM") || head[6..10] != [0; 4] {
            return false;
        }
        let size = le32(head, 2);
        let data_offset = le32(head, 10);
        let dib = le32(head, 14);
        matches!(dib, 12 | 40 | 52 | 56 | 64 | 108 | 124)
            && data_offset >= 14 + dib
            && size > data_offset
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        let h: [u8; 34] = r.bytes()?;
        let size = u64::from(le32(&h, 2));
        if size > r.limit() {
            return Ok(None);
        }
        let data_offset = u64::from(le32(&h, 10));
        let mut conf = Confidence::Medium;
        if le32(&h, 14) >= 40 && le32(&h, 30) == 0 {
            // Uncompressed: the size can be checked against the dimensions.
            let width = u64::from((le32(&h, 18) as i32).unsigned_abs());
            let height = u64::from((le32(&h, 22) as i32).unsigned_abs());
            let bpp = u64::from(le16(&h, 28));
            if matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) {
                let expected = data_offset + (width * bpp).div_ceil(32) * 4 * height;
                if size == expected || size == expected + 2 {
                    conf = Confidence::High;
                }
            }
        }
        Ok(Some(Carved::new(size, conf, "bmp")))
    }
}
