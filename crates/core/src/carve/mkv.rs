//! Matroska (MKV) and WebM video.

use std::io;

use super::{Carved, Carver, Confidence};
use crate::reader::Reader;

const EBML: u32 = 0x1A45_DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;

/// Elements that may start a segment's body.
const SEGMENT_CHILDREN: &[u32] = &[
    0x114D_9B74, // SeekHead
    0x1549_A966, // Info
    0x1654_AE6B, // Tracks
    0x1F43_B675, // Cluster
    0x1C53_BB6B, // Cues
    0x1254_C367, // Tags
    0x1043_A770, // Chapters
    0x1941_A469, // Attachments
    0xEC,        // Void
];

pub struct Matroska;

impl Carver for Matroska {
    fn id(&self) -> &'static str {
        "mkv"
    }

    fn description(&self) -> &'static str {
        "Matroska / WebM video"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mkv", "webm"]
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn first_bytes(&self) -> &'static [u8] {
        &[0x1A]
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.starts_with(&EBML.to_be_bytes())
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        read_id(r)?;
        let Some(Some(header_len)) = read_size(r)? else {
            return Ok(None);
        };
        if header_len > 4096 {
            return Ok(None);
        }
        let header_end = r.offset() + header_len;
        let mut doc_type = Vec::new();
        while r.offset() < header_end {
            let (Some(id), Some(Some(size))) = (read_id(r)?, read_size(r)?) else {
                return Ok(None);
            };
            if id == DOC_TYPE {
                doc_type = r.read_at(r.offset(), size.min(32) as usize)?;
            }
            r.skip(size)?;
        }
        let ext = match doc_type.as_slice() {
            b"matroska" => "mkv",
            b"webm" => "webm",
            _ => return Ok(None),
        };

        r.seek(header_end);
        if read_id(r)? != Some(SEGMENT) {
            return Ok(None);
        }
        // Live recordings may leave the segment size unknown; those can't be carved.
        let Some(Some(segment_len)) = read_size(r)? else {
            return Ok(None);
        };
        let body = r.offset();
        let len = body + segment_len;
        if len > r.limit() {
            return Ok(None);
        }
        let conf = match read_id(r)? {
            Some(id) if SEGMENT_CHILDREN.contains(&id) => Confidence::High,
            _ => Confidence::Medium,
        };
        Ok(Some(Carved::new(len, conf, ext)))
    }
}

/// Length of a variable-size integer, from its first byte.
fn vint_len(first: u8) -> Option<u32> {
    let n = first.leading_zeros() + 1;
    (n <= 8).then_some(n)
}

/// Reads an element ID (marker bits kept). `None` if invalid.
fn read_id(r: &mut Reader<'_>) -> io::Result<Option<u32>> {
    let first = r.u8()?;
    let Some(n) = vint_len(first).filter(|n| *n <= 4) else {
        return Ok(None);
    };
    let mut id = u32::from(first);
    for _ in 1..n {
        id = id << 8 | u32::from(r.u8()?);
    }
    Ok(Some(id))
}

/// Reads an element size. `None` if invalid, `Some(None)` if "unknown size".
fn read_size(r: &mut Reader<'_>) -> io::Result<Option<Option<u64>>> {
    let first = r.u8()?;
    let Some(n) = vint_len(first) else {
        return Ok(None);
    };
    let mask = ((1u16 << (8 - n)) - 1) as u8;
    let mut value = u64::from(first & mask);
    let mut all_ones = first & mask == mask;
    for _ in 1..n {
        let b = r.u8()?;
        all_ones &= b == 0xFF;
        value = value << 8 | u64::from(b);
    }
    Ok(Some((!all_ones).then_some(value)))
}
