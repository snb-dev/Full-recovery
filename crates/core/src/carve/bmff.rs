//! ISO base media files: MP4, MOV, M4A, 3GP, HEIC, AVIF and Canon CR3.

use std::io;

use super::{be32, be64, Carved, Carver, Confidence};
use crate::reader::Reader;

/// Box types that can appear at the top level of a file.
const TOP_LEVEL: &[&[u8; 4]] = &[
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pdin", b"moof",
    b"mfra", b"styp", b"sidx", b"ssix", b"prft", b"emsg", b"pnot",
];

pub struct IsoMedia;

impl Carver for IsoMedia {
    fn id(&self) -> &'static str {
        "mp4"
    }

    fn description(&self) -> &'static str {
        "ISO media (MP4, MOV, M4A, 3GP, HEIC, AVIF, CR3)"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mp4", "mov", "m4a", "3gp", "heic", "avif", "cr3"]
    }

    fn max_len(&self) -> u64 {
        64 << 30
    }

    fn first_bytes(&self) -> &'static [u8] {
        &[0]
    }

    fn matches(&self, head: &[u8]) -> bool {
        if head.len() < 12 || &head[4..8] != b"ftyp" {
            return false;
        }
        let size = be32(head, 0);
        (16..=4096).contains(&size)
            && size % 4 == 0
            && head[8..12]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b' ')
    }

    fn carve(&self, r: &mut Reader<'_>) -> io::Result<Option<Carved>> {
        let ftyp_size = u64::from(r.u32_be()?);
        let ftyp = r.read_at(0, ftyp_size.min(256) as usize)?;
        let ext = classify(&ftyp);
        let still_image = matches!(ext, "heic" | "avif");

        let (mut moov, mut mdat, mut meta) = (false, false, false);
        r.seek(0);
        loop {
            let at = r.offset();
            let h = r.peek(16)?;
            if h.len() < 8 {
                break;
            }
            let ty: [u8; 4] = h[4..8].try_into().unwrap();
            if !TOP_LEVEL.contains(&&ty) {
                break;
            }
            let size = match be32(h, 0) {
                // A size of 1 means a 64-bit size follows.
                1 if h.len() == 16 && be64(h, 8) >= 16 => be64(h, 8),
                // 0 ("to the end of the file") and other sizes can't be followed.
                s if s >= 8 => u64::from(s),
                _ => break,
            };
            if at + size > r.limit() {
                break;
            }
            match &ty {
                b"moov" => moov = true,
                b"mdat" => mdat = true,
                b"meta" => meta = true,
                _ => {}
            }
            r.seek(at + size);
        }

        let len = r.offset();
        if len <= ftyp_size {
            return Ok(None);
        }
        let complete = mdat && if still_image { meta || moov } else { moov };
        let conf = if complete {
            Confidence::High
        } else {
            Confidence::Low
        };
        Ok(Some(Carved::new(len, conf, ext)))
    }
}

/// Picks the extension from the major and compatible brands of a `ftyp` box.
fn classify(ftyp: &[u8]) -> &'static str {
    let major = &ftyp[8..12];
    let brands: Vec<&[u8]> = std::iter::once(major)
        .chain(ftyp.get(16..).unwrap_or_default().chunks_exact(4))
        .collect();
    let has = |names: &[&[u8; 4]]| {
        brands
            .iter()
            .any(|b| names.iter().any(|n| *b == n.as_slice()))
    };
    if has(&[b"avif", b"avis"]) {
        return "avif";
    }
    if has(&[
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"mif1", b"msf1",
    ]) {
        return "heic";
    }
    match major {
        b"qt  " => "mov",
        b"M4A " | b"M4B " | b"M4P " => "m4a",
        b"crx " => "cr3",
        m if m.starts_with(b"3g") => "3gp",
        _ => "mp4",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn brands() {
        assert_eq!(
            classify(&fixtures::ftyp(b"isom", &[b"isom", b"mp41"])),
            "mp4"
        );
        assert_eq!(classify(&fixtures::ftyp(b"qt  ", &[b"qt  "])), "mov");
        assert_eq!(
            classify(&fixtures::ftyp(b"heic", &[b"mif1", b"heic"])),
            "heic"
        );
        assert_eq!(
            classify(&fixtures::ftyp(b"avif", &[b"mif1", b"miaf"])),
            "avif"
        );
        assert_eq!(classify(&fixtures::ftyp(b"3gp4", &[b"isom"])), "3gp");
    }

    #[test]
    fn video_without_moov_is_low() {
        let mut data = fixtures::ftyp(b"isom", &[b"isom"]);
        data.extend_from_slice(&fixtures::bmff_box(b"mdat", &[7; 100]));
        let mut r = Reader::new(&data, 0, u64::MAX);
        let c = IsoMedia.carve(&mut r).unwrap().unwrap();
        assert_eq!(c, Carved::new(data.len() as u64, Confidence::Low, "mp4"));
    }
}
