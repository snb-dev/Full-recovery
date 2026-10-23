//! File system recovery: lists live and deleted files with their original
//! names, folders and dates, and where their data is stored.

mod exfat;
mod fat;
mod ntfs;

use std::io::{self, Write};

use serde::Serialize;

use crate::carve::Confidence;
use crate::source::{read_fully, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum FsKind {
    #[serde(rename = "FAT12")]
    Fat12,
    #[serde(rename = "FAT16")]
    Fat16,
    #[serde(rename = "FAT32")]
    Fat32,
    #[serde(rename = "exFAT")]
    ExFat,
    #[serde(rename = "NTFS")]
    Ntfs,
}

impl FsKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fat12 => "FAT12",
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
            Self::ExFat => "exFAT",
            Self::Ntfs => "NTFS",
        }
    }
}

/// A file system found at some offset of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct VolumeInfo {
    pub kind: FsKind,
    pub offset: u64,
    pub size: u64,
}

/// A piece of a file's data. `offset` is absolute within the source; `None`
/// means a sparse range that reads as zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Extent {
    pub offset: Option<u64>,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileData {
    Extents(Vec<Extent>),
    /// Small files stored inside the file system's own records (NTFS).
    Resident(Vec<u8>),
    /// The data exists but can't be read yet; the reason is given.
    Unsupported(&'static str),
}

#[derive(Debug, Clone, Serialize)]
pub struct FsEntry {
    /// Full path from the volume root, e.g. `/Photos/Holiday/IMG_0001.JPG`.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub deleted: bool,
    /// Last modification time, in seconds since 1970 (local time for FAT/exFAT).
    pub modified: Option<i64>,
    pub created: Option<i64>,
    /// How likely the recovered content is to be intact.
    pub confidence: Confidence,
    #[serde(skip)]
    pub data: FileData,
}

impl FsEntry {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Volume {
    #[serde(flatten)]
    pub info: VolumeInfo,
    pub label: Option<String>,
    pub entries: Vec<FsEntry>,
}

/// Identifies the file system whose boot sector is at `offset`, checking that
/// its main structures are where the boot sector says they are.
pub fn probe(src: &dyn Source, offset: u64) -> io::Result<Option<VolumeInfo>> {
    probe_boot(src, offset, offset)
}

/// Like [`probe`], but reads the boot sector from `boot_at`: a backup copy
/// to use when the volume's first sector is damaged.
pub fn probe_boot(src: &dyn Source, offset: u64, boot_at: u64) -> io::Result<Option<VolumeInfo>> {
    let Some(boot) = read_boot(src, boot_at)? else {
        return Ok(None);
    };
    if let Some(v) = ntfs::probe(src, offset, &boot)? {
        return Ok(Some(v));
    }
    if let Some(v) = exfat::probe(src, offset, &boot)? {
        return Ok(Some(v));
    }
    fat::probe(src, offset, &boot)
}

fn read_boot(src: &dyn Source, at: u64) -> io::Result<Option<[u8; 512]>> {
    let mut boot = [0u8; 512];
    Ok((read_fully(src, at, &mut boot)? == boot.len()).then_some(boot))
}

/// Reads the whole directory tree, including deleted files, of the file
/// system at `offset`.
pub fn open(src: &dyn Source, offset: u64) -> io::Result<Option<Volume>> {
    open_boot(src, offset, offset)
}

/// Like [`open`], but reads the boot sector from `boot_at` (see [`probe_boot`]).
pub fn open_boot(src: &dyn Source, offset: u64, boot_at: u64) -> io::Result<Option<Volume>> {
    let Some(info) = probe_boot(src, offset, boot_at)? else {
        return Ok(None);
    };
    let boot = read_boot(src, boot_at)?.expect("probed");
    let volume = match info.kind {
        FsKind::Ntfs => ntfs::read(src, info, &boot)?,
        FsKind::ExFat => exfat::read(src, info, &boot)?,
        _ => fat::read(src, info, &boot)?,
    };
    Ok(Some(volume))
}

/// Writes the content of `entry` to `out`.
pub fn extract(src: &dyn Source, entry: &FsEntry, out: &mut dyn Write) -> io::Result<()> {
    match &entry.data {
        FileData::Resident(data) => out.write_all(data),
        FileData::Unsupported(why) => Err(io::Error::new(io::ErrorKind::Unsupported, *why)),
        FileData::Extents(extents) => {
            let mut buf = vec![0u8; 1 << 20];
            for e in extents {
                let mut done = 0;
                while done < e.len {
                    let n = buf.len().min((e.len - done) as usize);
                    match e.offset {
                        Some(at) => {
                            if read_fully(src, at + done, &mut buf[..n])? < n {
                                return Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "file data extends past the end of the source",
                                ));
                            }
                        }
                        None => buf[..n].fill(0),
                    }
                    out.write_all(&buf[..n])?;
                    done += n as u64;
                }
            }
            Ok(())
        }
    }
}

/// Appends a range to `extents`, merging it with the last one if adjacent.
pub(crate) fn push_extent(extents: &mut Vec<Extent>, offset: Option<u64>, len: u64) {
    if let Some(last) = extents.last_mut() {
        let adjacent = match (last.offset, offset) {
            (Some(a), Some(b)) => a + last.len == b,
            (None, None) => true,
            _ => false,
        };
        if adjacent {
            last.len += len;
            return;
        }
    }
    extents.push(Extent { offset, len });
}

/// Shortens `extents` so they cover exactly `size` bytes (or less if short).
pub(crate) fn truncate_extents(extents: &mut Vec<Extent>, size: u64) {
    let mut left = size;
    extents.retain_mut(|e| {
        e.len = e.len.min(left);
        left -= e.len;
        e.len > 0
    });
}

pub(crate) fn utf16_string(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

pub(crate) fn utf16_le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    utf16_string(&units)
}

/// Days since 1970-01-01 of a calendar date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Converts an MS-DOS date and time (FAT, exFAT) to seconds since 1970.
pub(crate) fn dos_datetime(date: u16, time: u16) -> Option<i64> {
    let (day, month) = (i64::from(date & 0x1F), i64::from((date >> 5) & 0x0F));
    let year = 1980 + i64::from(date >> 9);
    let (sec, min, hour) = (
        i64::from(time & 0x1F) * 2,
        i64::from((time >> 5) & 0x3F),
        i64::from(time >> 11),
    );
    if day == 0 || !(1..=12).contains(&month) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Converts a Windows FILETIME (100 ns units since 1601) to seconds since 1970.
pub(crate) fn filetime(ft: u64) -> Option<i64> {
    (ft != 0).then(|| (ft / 10_000_000) as i64 - 11_644_473_600)
}

/// Formats seconds since 1970 as `YYYY-MM-DD HH:MM`.
pub fn format_time(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Inverse of days_from_civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        // 2024-02-29 13:45:30
        let date = ((2024 - 1980) << 9) | (2 << 5) | 29;
        let time = (13 << 11) | (45 << 5) | 15;
        let t = dos_datetime(date, time).unwrap();
        assert_eq!(t, 1_709_214_330);
        assert_eq!(format_time(t), "2024-02-29 13:45");
        assert_eq!(filetime(133_538_879_300_000_000), Some(1_709_414_330));
        assert_eq!(dos_datetime(0, 0), None);
    }

    #[test]
    fn extents() {
        let mut e = Vec::new();
        push_extent(&mut e, Some(100), 10);
        push_extent(&mut e, Some(110), 10);
        push_extent(&mut e, None, 5);
        push_extent(&mut e, Some(500), 10);
        assert_eq!(e.len(), 3);
        truncate_extents(&mut e, 22);
        assert_eq!(
            e,
            [
                Extent {
                    offset: Some(100),
                    len: 20
                },
                Extent {
                    offset: None,
                    len: 2
                }
            ]
        );
    }
}
