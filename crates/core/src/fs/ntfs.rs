//! NTFS: Windows system and data drives, many external hard disks.
//!
//! Every file has a record in the Master File Table (MFT). Deleting a file
//! only marks its record as unused, so names, dates and data locations of
//! deleted files usually survive until the record is reused.

use std::collections::HashMap;
use std::io;

use super::{
    filetime, push_extent, truncate_extents, utf16_le, Extent, FileData, FsEntry, FsKind, Volume,
    VolumeInfo,
};
use crate::carve::{le16, le32, le64, Confidence};
use crate::source::{read_fully, Source};

const ROOT: u64 = 5;
const BITMAP_RECORD: u64 = 6;
const VOLUME_RECORD: u64 = 3;
/// Records below this number belong to NTFS itself.
const FIRST_USER_RECORD: u64 = 24;

const STANDARD_INFORMATION: u32 = 0x10;
const FILE_NAME: u32 = 0x30;
const VOLUME_NAME: u32 = 0x60;
const DATA: u32 = 0x80;
const END: u32 = 0xFFFF_FFFF;

const FLAG_COMPRESSED: u16 = 0x00FF;
const FLAG_ENCRYPTED: u16 = 0x4000;
const DOS_NAMESPACE: u8 = 2;
const REF_MASK: u64 = 0xFFFF_FFFF_FFFF;

struct Layout {
    cluster: u64,
    mft_offset: u64,
    record_size: usize,
    size: u64,
}

fn layout(b: &[u8]) -> Option<Layout> {
    if &b[3..11] != b"NTFS    " || b[510..512] != [0x55, 0xAA] {
        return None;
    }
    let bps = u64::from(le16(b, 11));
    let spc = match b[13] {
        n @ 1..=0x80 => u64::from(n),
        n => 1u64.checked_shl(256 - u32::from(n))?,
    };
    if !matches!(bps, 512 | 1024 | 2048 | 4096) || !spc.is_power_of_two() {
        return None;
    }
    let cluster = bps * spc;
    let record_size = match b[64] as i8 {
        n @ 1.. => cluster * n as u64,
        n @ -31..=-1 => 1u64 << -n,
        _ => return None,
    };
    if !(256..=65536).contains(&record_size) {
        return None;
    }
    Some(Layout {
        cluster,
        mft_offset: le64(b, 48).checked_mul(cluster)?,
        record_size: record_size as usize,
        size: le64(b, 40).saturating_add(1).saturating_mul(bps),
    })
}

pub(super) fn probe(src: &dyn Source, offset: u64, boot: &[u8]) -> io::Result<Option<VolumeInfo>> {
    let Some(l) = layout(boot) else {
        return Ok(None);
    };
    let mut sig = [0u8; 4];
    read_fully(src, offset.saturating_add(l.mft_offset), &mut sig)?;
    Ok((&sig == b"FILE").then_some(VolumeInfo {
        kind: FsKind::Ntfs,
        offset,
        size: l.size,
    }))
}

/// Undoes the "update sequence" protection: the last two bytes of every
/// 512-byte block were moved into a table at the start of the record.
fn fixup(rec: &mut [u8]) -> bool {
    let at = usize::from(le16(rec, 4));
    let count = usize::from(le16(rec, 6));
    if count < 2 || at + count * 2 > rec.len() || (count - 1) * 512 > rec.len() {
        return false;
    }
    let check = [rec[at], rec[at + 1]];
    for i in 1..count {
        let end = i * 512 - 2;
        if rec[end..end + 2] != check {
            return false;
        }
        rec[end] = rec[at + i * 2];
        rec[end + 1] = rec[at + i * 2 + 1];
    }
    true
}

struct Attr<'a> {
    ty: u32,
    raw: &'a [u8],
}

impl<'a> Attr<'a> {
    fn non_resident(&self) -> bool {
        self.raw[8] != 0
    }

    fn named(&self) -> bool {
        self.raw[9] != 0
    }

    fn flags(&self) -> u16 {
        le16(self.raw, 12)
    }

    fn value(&self) -> Option<&'a [u8]> {
        let len = le32(self.raw, 16) as usize;
        let at = usize::from(le16(self.raw, 20));
        self.raw.get(at..at.checked_add(len)?)
    }
}

fn attributes(rec: &[u8]) -> impl Iterator<Item = Attr<'_>> {
    let used = (le32(rec, 24) as usize).min(rec.len());
    let mut at = usize::from(le16(rec, 20));
    std::iter::from_fn(move || {
        if at + 16 > used {
            return None;
        }
        let ty = le32(rec, at);
        let len = le32(rec, at + 4) as usize;
        if ty == END || len < 16 || at + len > used {
            return None;
        }
        let attr = Attr {
            ty,
            raw: &rec[at..at + len],
        };
        at += len;
        Some(attr)
    })
}

/// Decodes a run list into `(cluster, count)` pairs; `None` clusters are sparse.
fn runs(raw: &[u8]) -> Option<Vec<(Option<u64>, u64)>> {
    let mut out = Vec::new();
    let mut lcn: i64 = 0;
    let mut p = 0;
    while let Some(&header) = raw.get(p) {
        if header == 0 {
            break;
        }
        let (len_size, off_size) = (usize::from(header & 0x0F), usize::from(header >> 4));
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return None;
        }
        let field = raw.get(p + 1..p + 1 + len_size + off_size)?;
        let mut len_bytes = [0u8; 8];
        len_bytes[..len_size].copy_from_slice(&field[..len_size]);
        let len = u64::from_le_bytes(len_bytes);
        if off_size == 0 {
            out.push((None, len));
        } else {
            let off = &field[len_size..];
            let fill = if off[off_size - 1] & 0x80 != 0 {
                0xFF
            } else {
                0
            };
            let mut off_bytes = [fill; 8];
            off_bytes[..off_size].copy_from_slice(off);
            lcn = lcn.checked_add(i64::from_le_bytes(off_bytes))?;
            out.push((Some(u64::try_from(lcn).ok()?), len));
        }
        p += 1 + len_size + off_size;
    }
    Some(out)
}

/// One record's piece of a file's (unnamed) data stream.
enum DataPiece {
    Resident(Vec<u8>),
    NonResident {
        start_vcn: u64,
        runs: Vec<(Option<u64>, u64)>,
        size: u64,
        flags: u16,
    },
}

fn data_piece(a: &Attr<'_>) -> Option<DataPiece> {
    if !a.non_resident() {
        return Some(DataPiece::Resident(a.value()?.to_vec()));
    }
    if a.raw.len() < 64 {
        return None;
    }
    let runs_at = usize::from(le16(a.raw, 32));
    Some(DataPiece::NonResident {
        start_vcn: le64(a.raw, 16),
        runs: runs(a.raw.get(runs_at..)?)?,
        size: le64(a.raw, 48),
        flags: a.flags(),
    })
}

#[derive(Default)]
struct Record {
    seq: u16,
    in_use: bool,
    is_dir: bool,
    /// Name and parent reference (record number + sequence).
    name: Option<(String, u64)>,
    dos_name: bool,
    modified: Option<i64>,
    created: Option<i64>,
    data: Vec<DataPiece>,
}

fn parse_record(rec: &[u8], records: &mut HashMap<u64, Record>, number: u64) -> Option<String> {
    let flags = le16(rec, 22);
    let base = le64(rec, 32) & REF_MASK;
    let mut label = None;
    let mut r = Record {
        seq: le16(rec, 16),
        in_use: flags & 1 != 0,
        is_dir: flags & 2 != 0,
        ..Default::default()
    };
    for a in attributes(rec) {
        match a.ty {
            STANDARD_INFORMATION => {
                if let Some(v) = a.value().filter(|v| v.len() >= 16) {
                    r.created = filetime(le64(v, 0));
                    r.modified = filetime(le64(v, 8));
                }
            }
            FILE_NAME => {
                let Some(v) = a.value().filter(|v| v.len() >= 66) else {
                    continue;
                };
                let len = usize::from(v[64]);
                let dos = v[65] == DOS_NAMESPACE;
                if r.name.is_none() || (r.dos_name && !dos) {
                    if let Some(name) = v.get(66..66 + len * 2) {
                        r.name = Some((utf16_le(name), le64(v, 0)));
                        r.dos_name = dos;
                        if r.modified.is_none() {
                            r.created = filetime(le64(v, 8));
                            r.modified = filetime(le64(v, 16));
                        }
                    }
                }
            }
            DATA if !a.named() => r.data.extend(data_piece(&a)),
            VOLUME_NAME if number == VOLUME_RECORD => label = a.value().map(utf16_le),
            _ => {}
        }
    }
    if base != 0 {
        // An extension record holding more of a large or fragmented file's data.
        if let Some(owner) = records.get_mut(&base) {
            owner.data.append(&mut r.data);
        } else {
            records.entry(base).or_default().data.append(&mut r.data);
        }
        return label;
    }
    match records.get_mut(&number) {
        // An extension record was seen first: keep the data it contributed.
        Some(existing) => {
            r.data.append(&mut existing.data);
            *existing = r;
        }
        None => {
            records.insert(number, r);
        }
    }
    label
}

pub(super) fn read(src: &dyn Source, info: VolumeInfo, boot: &[u8]) -> io::Result<Volume> {
    let l = layout(boot).expect("probed");
    let base = info.offset;
    let mut mft0 = vec![0u8; l.record_size];
    read_fully(src, base + l.mft_offset, &mut mft0)?;
    let mut records = HashMap::new();
    let mft_extents = if fixup(&mut mft0) {
        parse_record(&mft0, &mut records, 0);
        let mft = records.remove(&0).unwrap_or_default();
        data_extents(&l, base, &mft.data).map(|(e, _)| e)
    } else {
        None
    };
    // If the MFT's own record is damaged, read on from its start until records stop.
    let guessed = mft_extents.is_none();
    let mft_extents = mft_extents.unwrap_or_else(|| {
        vec![Extent {
            offset: Some(base + l.mft_offset),
            len: info.size.saturating_sub(l.mft_offset),
        }]
    });

    let mut label = None;
    let mut number = 0u64;
    let mut buf = Vec::new();
    'extents: for e in &mft_extents {
        let Some(start) = e.offset else {
            number += e.len / l.record_size as u64;
            continue;
        };
        let mut done = 0;
        while done < e.len {
            let n = (e.len - done).min(4 << 20) as usize / l.record_size * l.record_size;
            if n == 0 {
                break;
            }
            buf.resize(n, 0);
            let got = read_fully(src, start + done, &mut buf)?;
            for rec in buf[..got].chunks_exact_mut(l.record_size) {
                if &rec[..4] == b"FILE" && fixup(rec) {
                    if let Some(name) = parse_record(rec, &mut records, number) {
                        label = Some(name);
                    }
                } else if guessed && rec.iter().all(|&b| b == 0) {
                    break 'extents;
                }
                number += 1;
            }
            done += n as u64;
        }
    }

    let bitmap = match records.get(&BITMAP_RECORD) {
        Some(r) => match data_extents(&l, base, &r.data) {
            // Without the bitmap, deleted files are just rated less certain.
            Some((extents, size)) => {
                read_extents(src, &extents, size.min(512 << 20)).unwrap_or_default()
            }
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    let ntfs = Ntfs {
        l,
        base,
        records,
        bitmap,
    };
    Ok(Volume {
        info,
        label,
        entries: ntfs.entries(),
    })
}

fn read_extents(src: &dyn Source, extents: &[Extent], max: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut w = io::Cursor::new(&mut out);
    let mut limited: Vec<Extent> = extents.to_vec();
    truncate_extents(&mut limited, max);
    let entry = FsEntry {
        path: String::new(),
        is_dir: false,
        size: max,
        deleted: false,
        modified: None,
        created: None,
        confidence: Confidence::High,
        data: FileData::Extents(limited),
    };
    super::extract(src, &entry, &mut w)?;
    Ok(out)
}

/// Turns data pieces into extents of the whole stream, plus its size.
/// `None` if the stream is resident, compressed, encrypted or incomplete.
fn data_extents(l: &Layout, base: u64, pieces: &[DataPiece]) -> Option<(Vec<Extent>, u64)> {
    let mut parts: Vec<_> = pieces
        .iter()
        .filter_map(|p| match p {
            DataPiece::NonResident {
                start_vcn,
                runs,
                size,
                flags,
            } => Some((*start_vcn, runs, *size, *flags)),
            DataPiece::Resident(_) => None,
        })
        .collect();
    parts.sort_by_key(|p| p.0);
    let &(first_vcn, _, size, flags) = parts.first()?;
    if first_vcn != 0 || flags & (FLAG_COMPRESSED | FLAG_ENCRYPTED) != 0 {
        return None;
    }
    let mut extents = Vec::new();
    let mut vcn = 0;
    for (start_vcn, runs, _, _) in parts {
        if start_vcn != vcn {
            return None;
        }
        for &(lcn, count) in runs {
            let offset = lcn.map(|c| base + c * l.cluster);
            push_extent(&mut extents, offset, count * l.cluster);
            vcn += count;
        }
    }
    truncate_extents(&mut extents, size);
    Some((extents, size))
}

struct Ntfs {
    l: Layout,
    base: u64,
    records: HashMap<u64, Record>,
    bitmap: Vec<u8>,
}

impl Ntfs {
    fn cluster_in_use(&self, offset: u64) -> bool {
        let lcn = (offset - self.base) / self.l.cluster;
        self.bitmap
            .get((lcn / 8) as usize)
            .is_none_or(|b| b & (1 << (lcn % 8)) != 0)
    }

    /// Full path of a record, or a path under `/_orphaned` if a parent is gone.
    fn path(&self, number: u64) -> Option<String> {
        let mut parts = Vec::new();
        let mut current = self.records.get(&number)?;
        for _ in 0..256 {
            let (name, parent_ref) = current.name.as_ref()?;
            parts.push(name.as_str());
            let parent = parent_ref & REF_MASK;
            if parent == ROOT {
                break;
            }
            let seq = (parent_ref >> 48) as u16;
            match self.records.get(&parent) {
                // Deleting a record bumps its sequence number.
                Some(p)
                    if p.is_dir
                        && (p.seq == seq || (!p.in_use && p.seq == seq.wrapping_add(1))) =>
                {
                    current = p;
                }
                _ => {
                    parts.push("_orphaned");
                    break;
                }
            }
        }
        parts.reverse();
        Some(format!("/{}", parts.join("/")))
    }

    fn entries(&self) -> Vec<FsEntry> {
        let mut numbers: Vec<u64> = self.records.keys().copied().collect();
        numbers.sort_unstable();
        let mut out = Vec::new();
        for n in numbers {
            let r = &self.records[&n];
            if n < FIRST_USER_RECORD {
                continue;
            }
            let Some(path) = self.path(n) else {
                continue;
            };
            if path.starts_with("/$Extend/") || path == "/$Extend" {
                continue;
            }
            let deleted = !r.in_use || self.deleted_ancestor(n);
            let (data, size, confidence) = if r.is_dir {
                let conf = if deleted {
                    Confidence::Medium
                } else {
                    Confidence::High
                };
                (FileData::Extents(Vec::new()), 0, conf)
            } else {
                self.file_data(r, deleted)
            };
            out.push(FsEntry {
                path,
                is_dir: r.is_dir,
                size,
                deleted,
                modified: r.modified,
                created: r.created,
                confidence,
                data,
            });
        }
        out
    }

    fn deleted_ancestor(&self, number: u64) -> bool {
        let mut current = number;
        for _ in 0..256 {
            let Some((_, parent_ref)) = self.records.get(&current).and_then(|r| r.name.as_ref())
            else {
                return false;
            };
            let parent = parent_ref & REF_MASK;
            if parent == ROOT {
                return false;
            }
            match self.records.get(&parent) {
                Some(p) if !p.in_use => return true,
                Some(_) => current = parent,
                None => return false,
            }
        }
        false
    }

    fn file_data(&self, r: &Record, deleted: bool) -> (FileData, u64, Confidence) {
        if let Some(DataPiece::Resident(bytes)) = r.data.first() {
            return (
                FileData::Resident(bytes.clone()),
                bytes.len() as u64,
                Confidence::High,
            );
        }
        let flags = r.data.iter().find_map(|p| match p {
            DataPiece::NonResident {
                start_vcn: 0,
                flags,
                size,
                ..
            } => Some((*flags, *size)),
            _ => None,
        });
        match flags {
            None if r.data.is_empty() => (FileData::Extents(Vec::new()), 0, Confidence::High),
            Some((f, size)) if f & FLAG_ENCRYPTED != 0 => (
                FileData::Unsupported("the file is encrypted (EFS)"),
                size,
                Confidence::Low,
            ),
            Some((f, size)) if f & FLAG_COMPRESSED != 0 => (
                FileData::Unsupported("NTFS-compressed files are not supported yet"),
                size,
                Confidence::Low,
            ),
            _ => match data_extents(&self.l, self.base, &r.data) {
                Some((extents, size)) => {
                    let overwritten = deleted
                        && extents.iter().any(|e| {
                            e.offset.is_some_and(|start| {
                                (start..start + e.len)
                                    .step_by(self.l.cluster as usize)
                                    .any(|o| self.cluster_in_use(o))
                            })
                        });
                    let conf = if overwritten {
                        Confidence::Low
                    } else {
                        Confidence::High
                    };
                    (FileData::Extents(extents), size, conf)
                }
                None => (
                    FileData::Unsupported("the file's data map is incomplete"),
                    0,
                    Confidence::Low,
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_lists() {
        // 0x18 clusters at 0x5634, then sparse 0x10, then 0x08 clusters 0x10 back.
        let raw = [0x21, 0x18, 0x34, 0x56, 0x01, 0x10, 0x11, 0x08, 0xF0, 0x00];
        assert_eq!(
            runs(&raw),
            Some(vec![
                (Some(0x5634), 0x18),
                (None, 0x10),
                (Some(0x5624), 0x08)
            ])
        );
    }

    #[test]
    fn fixups() {
        let mut rec = vec![0u8; 1024];
        rec[4..6].copy_from_slice(&48u16.to_le_bytes());
        rec[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec[48..54].copy_from_slice(&[0xAB, 0xCD, 1, 2, 3, 4]);
        rec[510..512].copy_from_slice(&[0xAB, 0xCD]);
        rec[1022..1024].copy_from_slice(&[0xAB, 0xCD]);
        assert!(fixup(&mut rec));
        assert_eq!(
            (&rec[510..512], &rec[1022..1024]),
            (&[1, 2][..], &[3, 4][..])
        );
        rec[510] = 0;
        assert!(!fixup(&mut rec));
    }
}
