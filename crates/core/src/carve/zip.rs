//! ZIP and formats built on it: DOCX, XLSX, PPTX, ODF, EPUB, APK and JAR.

use std::io;

use super::{le16, le32, le64, Carved, Carver, Confidence};
use crate::reader::Reader;

const LOCAL_HEADER: &[u8; 4] = b"PK\x03\x04";
const CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";
const END_OF_CENTRAL_DIR: &[u8; 4] = b"PK\x05\x06";
const ZIP64_LOCATOR: &[u8; 4] = b"PK\x06\x07";
const ZIP64_END: &[u8; 4] = b"PK\x06\x06";

pub struct Zip;

impl Carver for Zip {
    fn id(&self) -> &'static str {
        "zip"
    }

    fn description(&self) -> &'static str {
        "ZIP archives and Office/ODF/EPUB/APK files"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[
            "zip", "docx", "xlsx", "pptx", "odt", "ods", "odp", "odg", "epub", "apk", "jar",
        ]
    }

    fn max_len(&self) -> u64 {
        2 << 30
    }

    fn first_bytes(&self) -> &'static [u8] {
        b"P"
    }

    fn matches(&self, head: &[u8]) -> bool {
        head.len() >= 30
            && head.starts_with(LOCAL_HEADER)
            && le16(head, 4) <= 63
            && (1..1024).contains(&le16(head, 26))
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        loop {
            let Some(at) = r.find(END_OF_CENTRAL_DIR)? else {
                return Ok(None);
            };
            r.skip(4)?;
            let eocd = r.read_at(at, 22)?;
            if eocd.len() < 22 {
                return Ok(None);
            }
            let mut cd_size = u64::from(le32(&eocd, 12));
            let mut cd_offset = u64::from(le32(&eocd, 16));
            if cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
                match zip64_directory(r, at)? {
                    Some(dir) => (cd_offset, cd_size) = dir,
                    None => continue,
                }
            } else if cd_offset + cd_size != at {
                // The end record of a different (e.g. nested) archive.
                continue;
            }
            let len = at + 22 + u64::from(le16(&eocd, 20));
            let cd = r.read_at(cd_offset, cd_size.min(4 << 20) as usize)?;
            if len > r.limit() || !cd.starts_with(CENTRAL_HEADER) {
                continue;
            }
            let ext = classify(r, &cd)?;
            return Ok(Some(Carved::new(len, Confidence::High, ext)));
        }
    }
}

/// Finds the central directory of a ZIP64 archive whose end record is at `eocd`.
fn zip64_directory(r: &Reader<'_>, eocd: u64) -> io::Result<Option<(u64, u64)>> {
    let Some(locator_at) = eocd.checked_sub(20) else {
        return Ok(None);
    };
    let locator = r.read_at(locator_at, 20)?;
    if locator.len() < 20 || !locator.starts_with(ZIP64_LOCATOR) {
        return Ok(None);
    }
    let record_at = le64(&locator, 8);
    let record = r.read_at(record_at, 56)?;
    if record.len() < 56 || !record.starts_with(ZIP64_END) {
        return Ok(None);
    }
    let (size, offset) = (le64(&record, 40), le64(&record, 48));
    Ok((offset.checked_add(size) == Some(record_at)).then_some((offset, size)))
}

fn central_names(cd: &[u8]) -> Vec<&[u8]> {
    let mut names = Vec::new();
    let mut p = 0;
    while cd.get(p..p + 4) == Some(CENTRAL_HEADER) && p + 46 <= cd.len() {
        let name_len = usize::from(le16(cd, p + 28));
        let extra_len = usize::from(le16(cd, p + 30));
        let comment_len = usize::from(le16(cd, p + 32));
        let Some(name) = cd.get(p + 46..p + 46 + name_len) else {
            break;
        };
        names.push(name);
        p += 46 + name_len + extra_len + comment_len;
    }
    names
}

fn classify(r: &Reader<'_>, cd: &[u8]) -> io::Result<&'static str> {
    let names = central_names(cd);
    let has = |n: &str| names.contains(&n.as_bytes());

    if has("AndroidManifest.xml")
        && names
            .iter()
            .any(|n| n.starts_with(b"classes") && n.ends_with(b".dex"))
    {
        return Ok("apk");
    }
    if has("word/document.xml") {
        return Ok("docx");
    }
    if has("xl/workbook.xml") {
        return Ok("xlsx");
    }
    if has("ppt/presentation.xml") {
        return Ok("pptx");
    }
    if has("mimetype") {
        // ODF and EPUB store their type, uncompressed, as the first entry.
        let local = r.read_at(0, 30)?;
        let name_len = u64::from(le16(&local, 26));
        if r.read_at(30, name_len as usize)? == b"mimetype" {
            let data_at = 30 + name_len + u64::from(le16(&local, 28));
            let mime = r.read_at(data_at, le32(&local, 18).min(128) as usize)?;
            let ext = match mime.as_slice() {
                b"application/epub+zip" => Some("epub"),
                b"application/vnd.oasis.opendocument.text" => Some("odt"),
                b"application/vnd.oasis.opendocument.spreadsheet" => Some("ods"),
                b"application/vnd.oasis.opendocument.presentation" => Some("odp"),
                b"application/vnd.oasis.opendocument.graphics" => Some("odg"),
                _ => None,
            };
            if let Some(ext) = ext {
                return Ok(ext);
            }
        }
    }
    if has("META-INF/MANIFEST.MF") {
        return Ok("jar");
    }
    Ok("zip")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn carve(data: &Vec<u8>) -> Option<Carved> {
        Zip.carve(&mut Reader::new(data, 0, u64::MAX)).unwrap()
    }

    #[test]
    fn office_and_odf() {
        let docx = fixtures::zip(&[
            ("[Content_Types].xml", b"<x/>"),
            ("word/document.xml", b"<w/>"),
        ]);
        assert_eq!(
            carve(&docx),
            Some(Carved::new(docx.len() as u64, Confidence::High, "docx"))
        );

        let odt = fixtures::zip(&[
            ("mimetype", b"application/vnd.oasis.opendocument.text"),
            ("content.xml", b"<x/>"),
        ]);
        assert_eq!(carve(&odt).unwrap().extension, "odt");

        let apk = fixtures::zip(&[("AndroidManifest.xml", b"x"), ("classes.dex", b"dex")]);
        assert_eq!(carve(&apk).unwrap().extension, "apk");
    }

    #[test]
    fn skips_nested_archive_end() {
        let inner = fixtures::zip(&[("a.txt", b"hello")]);
        let outer = fixtures::zip(&[("inner.zip", &inner), ("b.txt", b"world")]);
        assert_eq!(
            carve(&outer),
            Some(Carved::new(outer.len() as u64, Confidence::High, "zip"))
        );
    }
}
