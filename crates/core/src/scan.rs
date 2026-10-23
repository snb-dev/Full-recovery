//! Deep scan: finds files by their content, without needing a file system.

use std::io;
use std::ops::ControlFlow;

use serde::Serialize;

use crate::carve::{self, Carver, Category, Confidence, HEAD_LEN};
use crate::reader::Reader;
use crate::source::{read_fully, Source};

/// Bytes read from the source at a time.
const CHUNK: u64 = 4 << 20;
/// Granularity used to work around unreadable areas.
const RETRY_BLOCK: usize = 64 << 10;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Files are only looked for at multiples of this many bytes from `start`.
    /// File systems store files at sector boundaries, so 512 finds nearly
    /// everything; 1 also finds files embedded inside other files but is slower.
    pub alignment: u64,
    pub start: u64,
    pub end: Option<u64>,
    /// Carver ids or extensions to look for (`None` means all).
    pub types: Option<Vec<String>>,
    /// Jump past fully verified files instead of searching inside them.
    pub skip_found: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            alignment: 512,
            start: 0,
            end: None,
            types: None,
            skip_found: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FoundFile {
    /// Sequence number within the scan.
    pub id: u64,
    pub offset: u64,
    pub len: u64,
    pub extension: &'static str,
    pub carver: &'static str,
    pub category: Category,
    pub confidence: Confidence,
}

/// Receives scan events. Returning `ControlFlow::Break` stops the scan.
pub trait ScanObserver {
    fn found(&mut self, _file: &FoundFile) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    /// `done` and `total` are bytes relative to the scanned range.
    fn progress(&mut self, _done: u64, _total: u64) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn read_error(&mut self, _offset: u64, _len: u64, _error: &io::Error) {}
}

impl ScanObserver for () {}

#[derive(Debug, Default, Serialize)]
pub struct ScanSummary {
    pub files: Vec<FoundFile>,
    pub bytes_scanned: u64,
    pub unreadable_bytes: u64,
    pub cancelled: bool,
}

/// Returns the carvers selected by `types` (all of them for `None`).
pub fn select_carvers(types: Option<&[String]>) -> Vec<&'static dyn Carver> {
    carve::all()
        .iter()
        .copied()
        .filter(|c| {
            types.is_none_or(|t| {
                t.iter()
                    .any(|t| t == c.id() || c.extensions().contains(&t.as_str()))
            })
        })
        .collect()
}

fn wanted(types: Option<&[String]>, carver: &dyn Carver, ext: &str) -> bool {
    types.is_none_or(|t| t.iter().any(|t| t == carver.id() || t == ext))
}

/// Scans `src` for files. The source is only ever read.
pub fn scan(
    src: &dyn Source,
    opts: &ScanOptions,
    obs: &mut dyn ScanObserver,
) -> io::Result<ScanSummary> {
    let types = opts.types.as_deref();
    let mut dispatch: Vec<Vec<&dyn Carver>> = vec![Vec::new(); 256];
    for c in select_carvers(types) {
        for &b in c.first_bytes() {
            dispatch[usize::from(b)].push(c);
        }
    }

    let align = opts.alignment.max(1);
    let src_len = src.len();
    let end = opts.end.unwrap_or(src_len).min(src_len);
    let start = opts.start.min(end);
    let total = end - start;
    let step = (CHUNK / align).max(1) * align;
    let align_up = |x: u64| start + (x - start).div_ceil(align) * align;

    let mut summary = ScanSummary::default();
    let mut buf = vec![0u8; step as usize + HEAD_LEN];
    let mut pos = start;
    while pos < end {
        let main_len = step.min(end - pos);
        // Read a little past the chunk so headers at its end can be checked.
        let want = (main_len + HEAD_LEN as u64).min(src_len - pos) as usize;
        let got = read_chunk(src, pos, &mut buf[..want], obs, &mut summary);
        let data = &buf[..got];

        let mut next = pos + main_len;
        let mut i = 0u64;
        while i < main_len && (i as usize) < data.len() {
            let at = pos + i;
            let off = i as usize;
            let head = &data[off..(off + HEAD_LEN).min(data.len())];
            let mut jump = None;
            for &c in &dispatch[usize::from(head[0])] {
                if !c.matches(head) {
                    continue;
                }
                let mut r = Reader::new(src, at, c.max_len());
                // Read errors and truncation inside a candidate just mean "no file here".
                let Ok(Some(carved)) = c.carve(&mut r) else {
                    continue;
                };
                if carved.len == 0 {
                    continue;
                }
                if wanted(types, c, carved.extension) {
                    let file = FoundFile {
                        id: summary.files.len() as u64,
                        offset: at,
                        len: carved.len,
                        extension: carved.extension,
                        carver: c.id(),
                        category: Category::from_extension(carved.extension),
                        confidence: carved.confidence,
                    };
                    let flow = obs.found(&file);
                    summary.files.push(file);
                    if flow.is_break() {
                        summary.cancelled = true;
                        summary.bytes_scanned = at - start;
                        return Ok(summary);
                    }
                }
                if opts.skip_found && carved.confidence == Confidence::High {
                    jump = Some(align_up(at + carved.len));
                }
                break;
            }
            match jump {
                Some(j) if j >= pos + main_len => {
                    next = j;
                    break;
                }
                Some(j) => i = j - pos,
                None => i += align,
            }
        }

        pos = next;
        summary.bytes_scanned = pos.min(end) - start;
        if obs.progress(summary.bytes_scanned, total).is_break() {
            summary.cancelled = true;
            break;
        }
    }
    Ok(summary)
}

/// Fills `buf` from `pos`. Unreadable areas (bad sectors) are reported and
/// zero-filled so the rest of the chunk can still be scanned.
fn read_chunk(
    src: &dyn Source,
    pos: u64,
    buf: &mut [u8],
    obs: &mut dyn ScanObserver,
    summary: &mut ScanSummary,
) -> usize {
    if let Ok(n) = read_fully(src, pos, buf) {
        return n;
    }
    let mut done = 0;
    while done < buf.len() {
        let n = RETRY_BLOCK.min(buf.len() - done);
        let at = pos + done as u64;
        match read_fully(src, at, &mut buf[done..done + n]) {
            Ok(k) if k < n => return done + k,
            Ok(_) => {}
            Err(e) => {
                buf[done..done + n].fill(0);
                obs.read_error(at, n as u64, &e);
                summary.unreadable_bytes += n as u64;
            }
        }
        done += n;
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::recover;

    /// Deterministic pseudo-random filler that stands in for unrelated data.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    /// Offset, content and expected extension of a file placed on a test disk.
    type Placed = (u64, Vec<u8>, &'static str);

    /// Builds a disk image with every fixture at a sector boundary, separated
    /// by noise, with one file straddling the scanner's chunk boundary.
    fn disk() -> (Vec<u8>, Vec<Placed>) {
        let mut disk = noise(6 << 20, 0x9E37_79B9_7F4A_7C15);
        let mut placed = Vec::new();
        let mut at = 4096u64;
        for (data, ext) in fixtures::all() {
            if placed.len() == 3 {
                at = CHUNK - 512;
            }
            disk[at as usize..at as usize + data.len()].copy_from_slice(&data);
            placed.push((at, data.clone(), ext));
            at = (at + data.len() as u64).div_ceil(512) * 512 + 8192;
        }
        assert!(at < disk.len() as u64);
        (disk, placed)
    }

    #[test]
    fn finds_every_fixture_exactly() {
        let (disk, placed) = disk();
        let summary = scan(&disk, &ScanOptions::default(), &mut ()).unwrap();
        for (at, data, ext) in &placed {
            let f = summary
                .files
                .iter()
                .find(|f| f.offset == *at)
                .unwrap_or_else(|| panic!("{ext} at {at} not found: {:?}", summary.files));
            assert_eq!(
                (f.extension, f.len, f.confidence),
                (*ext, data.len() as u64, Confidence::High)
            );
            let mut out = Vec::new();
            recover::extract(&disk, f, &mut out).unwrap();
            assert_eq!(&out, data, "{ext} content differs");
        }
        assert_eq!(
            summary.files.len(),
            placed.len(),
            "unexpected extra files: {:?}",
            summary.files
        );
        assert_eq!(summary.bytes_scanned, disk.len() as u64);
    }

    #[test]
    fn type_filter() {
        let (disk, _) = disk();
        let opts = ScanOptions {
            types: Some(vec!["docx".into(), "png".into()]),
            ..Default::default()
        };
        let summary = scan(&disk, &opts, &mut ()).unwrap();
        let mut exts: Vec<_> = summary.files.iter().map(|f| f.extension).collect();
        exts.sort_unstable();
        assert_eq!(exts, ["docx", "png"]);
    }

    #[test]
    fn stops_when_observer_breaks() {
        struct StopAfterFirst;
        impl ScanObserver for StopAfterFirst {
            fn found(&mut self, _: &FoundFile) -> ControlFlow<()> {
                ControlFlow::Break(())
            }
        }
        let (disk, _) = disk();
        let summary = scan(&disk, &ScanOptions::default(), &mut StopAfterFirst).unwrap();
        assert!(summary.cancelled);
        assert_eq!(summary.files.len(), 1);
    }

    #[test]
    fn unreadable_areas_are_skipped() {
        struct BadSectors(Vec<u8>);
        impl Source for BadSectors {
            fn len(&self) -> u64 {
                self.0.len() as u64
            }
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
                // The first 64 KiB is unreadable.
                if offset < 65536 {
                    return Err(io::Error::other("bad sector"));
                }
                self.0.read_at(offset, buf)
            }
        }
        let png = fixtures::png();
        let mut data = vec![0u8; 1 << 20];
        data[1 << 19..(1 << 19) + png.len()].copy_from_slice(&png);
        let summary = scan(&BadSectors(data), &ScanOptions::default(), &mut ()).unwrap();
        assert_eq!(summary.unreadable_bytes, 65536);
        assert_eq!(summary.files.len(), 1);
        assert_eq!(summary.files[0].offset, 1 << 19);
    }
}
