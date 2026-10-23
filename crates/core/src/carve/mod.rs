//! File-type carvers. Each one recognises a file's header and works out where
//! the file ends by walking its internal structure.

mod bmff;
mod bmp;
mod gif;
mod jpeg;
mod mkv;
mod ogg;
mod pdf;
mod png;
mod riff;
mod sevenz;
mod sqlite;
mod zip;

#[cfg(test)]
pub(crate) use ogg::crc as ogg_crc;

use std::io;
use std::str::FromStr;

use serde::Serialize;

use crate::reader::Reader;

/// Number of bytes from a candidate's start passed to [`Carver::matches`].
pub const HEAD_LEN: usize = 32;

/// How likely a found file is to open correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// The structure breaks part-way (damaged or fragmented file); only the
    /// readable first part was recovered.
    Low,
    /// The structure looks right but could not be fully verified.
    Medium,
    /// The whole structure was verified, including checksums where the
    /// format has them.
    High,
}

impl FromStr for Confidence {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(format!(
                "unknown confidence `{s}` (use low, medium or high)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Photo,
    Video,
    Audio,
    Document,
    Archive,
    Database,
    Other,
}

impl Category {
    /// Category of a file extension (case does not matter).
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "heif" | "avif" | "tif"
            | "tiff" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf" | "svg"
            | "ico" | "psd" => Self::Photo,
            "mp4" | "m4v" | "mov" | "3gp" | "avi" | "mkv" | "webm" | "ogv" | "wmv" | "flv"
            | "mpg" | "mpeg" | "mts" | "m2ts" | "ts" | "vob" => Self::Video,
            "wav" | "mp3" | "m4a" | "aac" | "flac" | "ogg" | "opus" | "oga" | "wma" | "amr"
            | "aiff" | "mid" => Self::Audio,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
            | "odg" | "epub" | "txt" | "rtf" | "csv" | "md" | "pages" | "numbers" | "key"
            | "html" | "htm" | "xml" | "json" => Self::Document,
            "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "jar" | "apk" | "iso" => {
                Self::Archive
            }
            "db" | "sqlite" | "sqlite3" | "mdb" | "accdb" => Self::Database,
            _ => Self::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::Database => "database",
            Self::Other => "other",
        }
    }
}

/// A file found by a carver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Carved {
    pub len: u64,
    pub confidence: Confidence,
    pub extension: &'static str,
}

impl Carved {
    pub fn new(len: u64, confidence: Confidence, extension: &'static str) -> Self {
        Self {
            len,
            confidence,
            extension,
        }
    }
}

pub trait Carver: Send + Sync {
    /// Short identifier used to select the carver, e.g. `"jpg"` or `"zip"`.
    fn id(&self) -> &'static str;

    fn description(&self) -> &'static str;

    /// Every extension this carver can produce.
    fn extensions(&self) -> &'static [&'static str];

    /// Largest file the carver will look for.
    fn max_len(&self) -> u64;

    /// Every possible first byte of a matching file, used for fast dispatch.
    fn first_bytes(&self) -> &'static [u8];

    /// Cheap check of the first [`HEAD_LEN`] bytes (fewer at the end of the source).
    fn matches(&self, head: &[u8]) -> bool;

    /// Works out the file's length. `Ok(None)` means it is not a valid file.
    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>>;
}

static ALL: &[&dyn Carver] = &[
    &jpeg::Jpeg,
    &png::Png,
    &gif::Gif,
    &bmp::Bmp,
    &bmff::IsoMedia,
    &riff::Riff,
    &mkv::Matroska,
    &ogg::Ogg,
    &pdf::Pdf,
    &zip::Zip,
    &sevenz::SevenZip,
    &sqlite::Sqlite,
];

/// All built-in carvers.
pub fn all() -> &'static [&'static dyn Carver] {
    ALL
}

pub(crate) fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

pub(crate) fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

pub(crate) fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

pub(crate) fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(b[at..at + 2].try_into().unwrap())
}

pub(crate) fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

pub(crate) fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_extension_has_a_category() {
        for c in all() {
            for ext in c.extensions() {
                assert_ne!(Category::from_extension(ext), Category::Other, "{ext}");
            }
        }
    }

    #[test]
    fn carver_ids_are_unique() {
        let mut ids: Vec<_> = all().iter().map(|c| c.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all().len());
    }
}
