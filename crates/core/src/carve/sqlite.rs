//! SQLite databases (contacts, SMS, browser history, app data).

use std::io;

use super::{be16, be32, Carved, Carver, Confidence};
use crate::reader::Reader;

pub struct Sqlite;

impl Carver for Sqlite {
    fn id(&self) -> &'static str {
        "sqlite"
    }

    fn description(&self) -> &'static str {
        "SQLite database"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["db"]
    }

    fn max_len(&self) -> u64 {
        16 << 30
    }

    fn first_bytes(&self) -> &'static [u8] {
        b"S"
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.starts_with(b"SQLite format 3\0")
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        let h: [u8; 100] = r.bytes()?;
        let page_size = match be16(&h, 16) {
            1 => 65536,
            n if n >= 512 && n.is_power_of_two() => u64::from(n),
            _ => return Ok(None),
        };
        // The stored page count is only valid if the two change counters agree.
        let pages = u64::from(be32(&h, 28));
        if h[21..24] != [64, 32, 32] || pages == 0 || h[24..28] != h[92..96] {
            return Ok(None);
        }
        let len = page_size * pages;
        if len > r.limit() || !matches!(r.u8()?, 0x05 | 0x0D) {
            return Ok(None);
        }
        Ok(Some(Carved::new(len, Confidence::High, "db")))
    }
}
