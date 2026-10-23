//! Small but structurally valid files of every supported type, for tests.

use crate::carve::ogg_crc;

/// Every fixture with the extension it should be recovered as.
pub fn all() -> Vec<(Vec<u8>, &'static str)> {
    vec![
        (jpeg(), "jpg"),
        (png(), "png"),
        (gif(), "gif"),
        (bmp(), "bmp"),
        (wav(), "wav"),
        (mp4(), "mp4"),
        (heic(), "heic"),
        (webm(), "webm"),
        (opus(), "opus"),
        (pdf(), "pdf"),
        (
            zip(&[
                ("[Content_Types].xml", b"<Types/>"),
                ("word/document.xml", b"<w:document/>"),
            ]),
            "docx",
        ),
        (sevenz(), "7z"),
        (sqlite(), "db"),
    ]
}

pub fn jpeg() -> Vec<u8> {
    let mut d = vec![0xFF, 0xD8];
    d.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
    d.extend_from_slice(b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    d.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
    d.extend((1..=64).map(|i| i as u8));
    d.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 8, 0, 16, 0, 16, 1, 1, 0x11, 0]);
    d.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 0x3F, 0]);
    // Compressed data with a stuffed 0xFF byte and a restart marker.
    d.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56, 0xFF, 0xD0, 0x78, 0x9A]);
    d.extend_from_slice(&[0xFF, 0xD9]);
    d
}

fn png_chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut c = (data.len() as u32).to_be_bytes().to_vec();
    c.extend_from_slice(ty);
    c.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(ty);
    crc.update(data);
    c.extend_from_slice(&crc.finalize().to_be_bytes());
    c
}

pub fn png() -> Vec<u8> {
    let mut d = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    d.extend(png_chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]));
    d.extend(png_chunk(
        b"IDAT",
        &[0x78, 0x9C, 0x63, 0x60, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01],
    ));
    d.extend(png_chunk(b"IEND", &[]));
    d
}

pub fn gif() -> Vec<u8> {
    let mut d = b"GIF89a".to_vec();
    d.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]);
    d.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
    d.extend_from_slice(&[0x21, 0xF9, 4, 0, 0, 0, 0, 0]);
    d.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]);
    d.extend_from_slice(&[2, 2, 0x4C, 0x01, 0]);
    d.push(0x3B);
    d
}

pub fn bmp() -> Vec<u8> {
    // 2x2 pixels, 24 bits: rows are padded to 8 bytes.
    let mut d = b"BM".to_vec();
    d.extend_from_slice(&70u32.to_le_bytes());
    d.extend_from_slice(&[0; 4]);
    d.extend_from_slice(&54u32.to_le_bytes());
    d.extend_from_slice(&40u32.to_le_bytes());
    d.extend_from_slice(&2i32.to_le_bytes());
    d.extend_from_slice(&2i32.to_le_bytes());
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&24u16.to_le_bytes());
    d.extend_from_slice(&[0; 24]);
    d.extend((0..16).map(|i| i as u8 * 9));
    d
}

pub fn wav() -> Vec<u8> {
    let mut body = b"WAVEfmt ".to_vec();
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&[1, 0, 1, 0, 0x40, 0x1F, 0, 0, 0x80, 0x3E, 0, 0, 2, 0, 16, 0]);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&8u32.to_le_bytes());
    body.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let mut d = b"RIFF".to_vec();
    d.extend_from_slice(&(body.len() as u32).to_le_bytes());
    d.extend(body);
    d
}

pub fn bmff_box(ty: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(ty);
    b.extend_from_slice(payload);
    b
}

pub fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
    let mut payload = major.to_vec();
    payload.extend_from_slice(&[0; 4]);
    for b in compatible {
        payload.extend_from_slice(*b);
    }
    bmff_box(b"ftyp", &payload)
}

pub fn mp4() -> Vec<u8> {
    let mut d = ftyp(b"isom", &[b"isom", b"mp41"]);
    d.extend(bmff_box(b"moov", &bmff_box(b"mvhd", &[0; 100])));
    d.extend(bmff_box(b"mdat", &[0x5A; 300]));
    d
}

pub fn heic() -> Vec<u8> {
    let mut d = ftyp(b"heic", &[b"mif1", b"heic"]);
    d.extend(bmff_box(b"meta", &[0; 40]));
    d.extend(bmff_box(b"mdat", &[0x33; 200]));
    d
}

pub fn webm() -> Vec<u8> {
    let mut d = vec![0x1A, 0x45, 0xDF, 0xA3, 0x8B];
    d.extend_from_slice(&[0x42, 0x86, 0x81, 0x01]);
    d.extend_from_slice(&[0x42, 0x82, 0x84]);
    d.extend_from_slice(b"webm");
    // Segment with an 8-byte size holding an Info element.
    d.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0, 0, 0, 0, 0, 0, 8]);
    d.extend_from_slice(&[0x15, 0x49, 0xA9, 0x66, 0x83, 1, 2, 3]);
    d
}

fn ogg_page(flags: u8, sequence: u32, body: &[u8]) -> Vec<u8> {
    assert!(body.len() < 255);
    let mut p = b"OggS".to_vec();
    p.extend_from_slice(&[0, flags]);
    p.extend_from_slice(&[0; 8]);
    p.extend_from_slice(&0x1234u32.to_le_bytes());
    p.extend_from_slice(&sequence.to_le_bytes());
    p.extend_from_slice(&[0; 4]);
    p.extend_from_slice(&[1, body.len() as u8]);
    p.extend_from_slice(body);
    let crc = ogg_crc(&[&p]);
    p[22..26].copy_from_slice(&crc.to_le_bytes());
    p
}

pub fn opus() -> Vec<u8> {
    let mut d = ogg_page(0x02, 0, b"OpusHead\x01\x02\x38\x01\x80\xBB\0\0\0\0\0");
    d.extend(ogg_page(0x00, 1, b"OpusTags\0\0\0\0\0\0\0\0"));
    d.extend(ogg_page(0x04, 2, &[0xFC, 0xFF, 0xFE]));
    d
}

pub fn pdf() -> Vec<u8> {
    let mut d = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
    d.extend_from_slice(b"xref\n0 2\ntrailer\n<< /Root 1 0 R >>\nstartxref\n9\n%%EOF\n");
    // An incremental update appended by an editor.
    d.extend_from_slice(b"2 0 obj\n<< >>\nendobj\nxref\n2 1\nstartxref\n120\n%%EOF\n");
    d
}

/// A ZIP archive with the given entries stored uncompressed.
pub fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut d = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = d.len() as u32;
        let crc = crc32fast::hash(data);
        let size = data.len() as u32;
        let name_len = name.len() as u16;

        d.extend_from_slice(b"PK\x03\x04");
        d.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        d.extend_from_slice(&crc.to_le_bytes());
        d.extend_from_slice(&size.to_le_bytes());
        d.extend_from_slice(&size.to_le_bytes());
        d.extend_from_slice(&name_len.to_le_bytes());
        d.extend_from_slice(&[0, 0]);
        d.extend_from_slice(name.as_bytes());
        d.extend_from_slice(data);

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&name_len.to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = d.len() as u32;
    let count = entries.len() as u16;
    d.extend_from_slice(&central);
    d.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    d.extend_from_slice(&count.to_le_bytes());
    d.extend_from_slice(&count.to_le_bytes());
    d.extend_from_slice(&(central.len() as u32).to_le_bytes());
    d.extend_from_slice(&cd_offset.to_le_bytes());
    d.extend_from_slice(&[0, 0]);
    d
}

pub fn sevenz() -> Vec<u8> {
    let packed = [0xA5u8; 16];
    let next_header = [0x01, 0x04, 0x06, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00];
    let mut start_header = Vec::new();
    start_header.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    start_header.extend_from_slice(&(next_header.len() as u64).to_le_bytes());
    start_header.extend_from_slice(&crc32fast::hash(&next_header).to_le_bytes());
    let mut d = vec![b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, 0, 4];
    d.extend_from_slice(&crc32fast::hash(&start_header).to_le_bytes());
    d.extend(start_header);
    d.extend_from_slice(&packed);
    d.extend_from_slice(&next_header);
    d
}

pub fn sqlite() -> Vec<u8> {
    let mut d = vec![0u8; 1024];
    d[..16].copy_from_slice(b"SQLite format 3\0");
    d[16..18].copy_from_slice(&512u16.to_be_bytes());
    d[18] = 1;
    d[19] = 1;
    d[21..24].copy_from_slice(&[64, 32, 32]);
    d[24..28].copy_from_slice(&7u32.to_be_bytes());
    d[28..32].copy_from_slice(&2u32.to_be_bytes());
    d[92..96].copy_from_slice(&7u32.to_be_bytes());
    d[96..100].copy_from_slice(&3_045_000u32.to_be_bytes());
    d[100] = 0x0D;
    d[512] = 0x0D;
    d
}
