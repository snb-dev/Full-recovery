//! Ogg containers: Vorbis, Opus, FLAC-in-Ogg and Theora.

use std::io;

use super::{le32, Carved, Carver, Confidence};
use crate::reader::Reader;

const BEGIN_OF_STREAM: u8 = 0x02;
const END_OF_STREAM: u8 = 0x04;

pub struct Ogg;

impl Carver for Ogg {
    fn id(&self) -> &'static str {
        "ogg"
    }

    fn description(&self) -> &'static str {
        "Ogg media (Vorbis, Opus, Theora)"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ogg", "opus", "oga", "ogv"]
    }

    fn max_len(&self) -> u64 {
        4 << 30
    }

    fn first_bytes(&self) -> &'static [u8] {
        b"O"
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.len() >= 27
            && head.starts_with(b"OggS")
            && head[4] == 0
            && head[5] & BEGIN_OF_STREAM != 0
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        let mut ext = None;
        let mut ended = false;
        let mut clean = true;
        loop {
            let at = r.offset();
            let h = r.peek(27)?;
            if h.len() < 27 || !h.starts_with(b"OggS") || h[4] != 0 {
                break;
            }
            let mut header: [u8; 27] = h.try_into().unwrap();
            r.skip(27)?;
            let segments = r.read_at(r.offset(), header[26].into())?;
            let body_len: usize = segments.iter().map(|&b| usize::from(b)).sum();
            let body = r.read_at(r.offset() + segments.len() as u64, body_len)?;
            if segments.len() < header[26].into() || body.len() < body_len {
                r.seek(at);
                break;
            }
            let stored = le32(&header, 22);
            header[22..26].fill(0);
            if crc(&[&header, &segments, &body]) != stored {
                if ext.is_none() {
                    return Ok(None);
                }
                // A damaged or foreign page: stop before it.
                clean = false;
                r.seek(at);
                break;
            }
            r.skip((segments.len() + body_len) as u64)?;
            ext.get_or_insert_with(|| classify(&body));
            if header[5] & END_OF_STREAM != 0 {
                ended = true;
            }
        }
        let Some(ext) = ext else {
            return Ok(None);
        };
        let conf = match (clean, ended) {
            (true, true) => Confidence::High,
            (true, false) => Confidence::Medium,
            (false, _) => Confidence::Low,
        };
        Ok(Some(Carved::new(r.offset(), conf, ext)))
    }
}

fn classify(first_packet: &[u8]) -> &'static str {
    if first_packet.starts_with(b"OpusHead") {
        "opus"
    } else if first_packet.starts_with(b"\x80theora") {
        "ogv"
    } else if first_packet.starts_with(b"\x7fFLAC") {
        "oga"
    } else {
        "ogg"
    }
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04C1_1DB7
            } else {
                r << 1
            };
            j += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// Ogg page checksum (CRC-32, polynomial 0x04C11DB7, not reflected).
pub(crate) fn crc(parts: &[&[u8]]) -> u32 {
    let mut c = 0u32;
    for part in parts {
        for &b in *part {
            c = (c << 8) ^ CRC_TABLE[usize::from((c >> 24) as u8 ^ b)];
        }
    }
    c
}
