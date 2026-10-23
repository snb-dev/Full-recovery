//! Byte-by-byte copy of a drive, for recovering from failing hardware.
//!
//! Every read stresses a dying drive, so the safest approach is to copy it
//! once and run all scans on the copy. Unreadable sectors are filled with
//! zeros and listed in the report.

use std::io::{self, Write};
use std::ops::ControlFlow;

use serde::Serialize;

use crate::source::{read_fully, Source};

const BLOCK: usize = 1 << 20;
const SECTOR: usize = 512;

#[derive(Debug, Default, Serialize)]
pub struct ImageReport {
    pub bytes_copied: u64,
    /// `(offset, length)` of every unreadable range, in order.
    pub bad_ranges: Vec<(u64, u64)>,
    pub cancelled: bool,
}

/// Copies all of `src` to `out`. `progress` gets `(done, total)` after each
/// block and can stop the copy.
pub fn create_image(
    src: &dyn Source,
    out: &mut dyn Write,
    progress: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> io::Result<ImageReport> {
    let total = src.len();
    let mut report = ImageReport::default();
    let mut buf = vec![0u8; BLOCK];
    let mut pos = 0u64;
    while pos < total {
        let n = BLOCK.min((total - pos) as usize);
        let block = &mut buf[..n];
        if read_fully(src, pos, block).is_err() {
            // Retry sector by sector to save everything that is still readable.
            for (i, sector) in block.chunks_mut(SECTOR).enumerate() {
                let at = pos + (i * SECTOR) as u64;
                if read_fully(src, at, sector).is_err() {
                    sector.fill(0);
                    add_bad_range(&mut report.bad_ranges, at, sector.len() as u64);
                }
            }
        }
        out.write_all(block)?;
        pos += n as u64;
        report.bytes_copied = pos;
        if progress(pos, total).is_break() {
            report.cancelled = true;
            break;
        }
    }
    out.flush()?;
    Ok(report)
}

fn add_bad_range(ranges: &mut Vec<(u64, u64)>, offset: u64, len: u64) {
    match ranges.last_mut() {
        Some((start, l)) if *start + *l == offset => *l += len,
        _ => ranges.push((offset, len)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flaky(Vec<u8>);

    impl Source for Flaky {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            // Sectors 3 and 4 of the second block cannot be read.
            let bad = (BLOCK + 3 * SECTOR) as u64..(BLOCK + 5 * SECTOR) as u64;
            if offset < bad.end && offset + buf.len() as u64 > bad.start {
                return Err(io::Error::other("bad sector"));
            }
            self.0.read_at(offset, buf)
        }
    }

    #[test]
    fn copies_around_bad_sectors() {
        let data: Vec<u8> = (0..BLOCK * 2 + 100).map(|i| (i % 251) as u8 + 1).collect();
        let mut out = Vec::new();
        let report = create_image(&Flaky(data.clone()), &mut out, &mut |_, _| {
            ControlFlow::Continue(())
        })
        .unwrap();
        let bad_start = BLOCK + 3 * SECTOR;
        assert_eq!(report.bad_ranges, [(bad_start as u64, 2 * SECTOR as u64)]);
        assert_eq!(out.len(), data.len());
        assert!(out[bad_start..bad_start + 2 * SECTOR]
            .iter()
            .all(|b| *b == 0));
        assert_eq!(out[..bad_start], data[..bad_start]);
        assert_eq!(
            out[bad_start + 2 * SECTOR..],
            data[bad_start + 2 * SECTOR..]
        );
    }
}
