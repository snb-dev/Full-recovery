use std::io;

use super::{Carved, Carver, Confidence};
use crate::reader::Reader;

pub struct Gif;

impl Carver for Gif {
    fn id(&self) -> &'static str {
        "gif"
    }

    fn description(&self) -> &'static str {
        "GIF image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["gif"]
    }

    fn max_len(&self) -> u64 {
        64 << 20
    }

    fn first_bytes(&self) -> &'static [u8] {
        b"G"
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.len() >= 13
            && (head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"))
            && head[6..8] != [0, 0]
            && head[8..10] != [0, 0]
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        r.skip(10)?;
        let flags = r.u8()?;
        r.skip(2)?;
        skip_color_table(r, flags)?;
        let mut images = 0;
        loop {
            let block_start = r.offset();
            match r.u8()? {
                0x3B => {
                    let conf = if images > 0 {
                        Confidence::High
                    } else {
                        Confidence::Medium
                    };
                    return Ok(Some(Carved::new(r.offset(), conf, "gif")));
                }
                0x21 => {
                    r.skip(1)?;
                    skip_sub_blocks(r)?;
                }
                0x2C => {
                    r.skip(8)?;
                    let flags = r.u8()?;
                    skip_color_table(r, flags)?;
                    if !(1..=12).contains(&r.u8()?) {
                        return Ok(None);
                    }
                    skip_sub_blocks(r)?;
                    images += 1;
                }
                _ => {
                    return Ok(
                        (images > 0).then(|| Carved::new(block_start, Confidence::Low, "gif"))
                    )
                }
            }
        }
    }
}

fn skip_color_table(r: &mut Reader<'_>, flags: u8) -> io::Result<()> {
    if flags & 0x80 != 0 {
        r.skip(3u64 << ((flags & 7) + 1))?;
    }
    Ok(())
}

fn skip_sub_blocks(r: &mut Reader<'_>) -> io::Result<()> {
    loop {
        match r.u8()? {
            0 => return Ok(()),
            n => r.skip(n.into())?,
        }
    }
}
