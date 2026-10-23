//! FAT12, FAT16 and FAT32: USB drives, SD cards, cameras.

use std::collections::HashSet;
use std::io;

use super::{
    dos_datetime, push_extent, truncate_extents, FileData, FsEntry, FsKind, Volume, VolumeInfo,
};
use crate::carve::{le16, le32, Confidence};
use crate::source::{read_fully, Source};

const ATTR_VOLUME: u8 = 0x08;
const ATTR_DIR: u8 = 0x10;
const ATTR_LFN: u8 = 0x0F;
const DELETED: u8 = 0xE5;
/// Largest directory FAT allows (65536 entries of 32 bytes).
const MAX_DIR: u64 = 2 << 20;

struct Layout {
    kind: FsKind,
    /// Bytes of the FAT that describe clusters.
    fat_len: u64,
    cluster: u64,
    fat_offset: u64,
    root_offset: u64,
    root_len: u64,
    data_offset: u64,
    clusters: u32,
    root_cluster: u32,
    size: u64,
    media: u8,
}

fn layout(b: &[u8]) -> Option<Layout> {
    let jump = (b[0] == 0xEB && b[2] == 0x90) || b[0] == 0xE9;
    let bps = u64::from(le16(b, 11));
    let spc = u64::from(b[13]);
    let reserved = u64::from(le16(b, 14));
    let fats = u64::from(b[16]);
    let root_entries = u64::from(le16(b, 17));
    let media = b[21];
    let fat16_size = u64::from(le16(b, 22));
    if !jump
        || !matches!(bps, 512 | 1024 | 2048 | 4096)
        || !spc.is_power_of_two()
        || spc > 128
        || reserved == 0
        || !(1..=2).contains(&fats)
        || !(media == 0xF0 || media >= 0xF8)
    {
        return None;
    }
    let total = match le16(b, 19) {
        0 => u64::from(le32(b, 32)),
        n => u64::from(n),
    };
    let fat_size = match fat16_size {
        0 => u64::from(le32(b, 36)),
        n => n,
    };
    let root_sectors = (root_entries * 32).div_ceil(bps);
    let data_start = reserved + fats * fat_size + root_sectors;
    if fat_size == 0 || data_start >= total {
        return None;
    }
    let clusters = (total - data_start) / spc;
    let kind = match clusters {
        0..4085 => FsKind::Fat12,
        4085..65525 => FsKind::Fat16,
        _ => FsKind::Fat32,
    };
    let fat32 = kind == FsKind::Fat32;
    if fat32 != (root_entries == 0) || (fat32 && fat16_size != 0) || clusters > 0x0FFF_FFF5 {
        return None;
    }
    let fat_needed = match kind {
        FsKind::Fat12 => (clusters + 2) * 3 / 2,
        FsKind::Fat16 => (clusters + 2) * 2,
        _ => (clusters + 2) * 4,
    };
    if fat_size * bps < fat_needed {
        return None;
    }
    Some(Layout {
        fat_len: fat_needed,
        kind,
        cluster: spc * bps,
        fat_offset: reserved * bps,
        root_offset: (reserved + fats * fat_size) * bps,
        root_len: root_sectors * bps,
        data_offset: data_start * bps,
        clusters: clusters as u32,
        root_cluster: if fat32 { le32(b, 44) } else { 0 },
        size: total * bps,
        media,
    })
}

pub(super) fn probe(src: &dyn Source, offset: u64, boot: &[u8]) -> io::Result<Option<VolumeInfo>> {
    let Some(l) = layout(boot) else {
        return Ok(None);
    };
    // The FAT must fit in the source; a damaged boot sector can claim a huge one.
    if offset.saturating_add(l.fat_offset + l.fat_len) > src.len() {
        return Ok(None);
    }
    // The first FAT entry repeats the media byte, padded with 0xFF.
    let mut first = [0u8; 2];
    read_fully(src, offset + l.fat_offset, &mut first)?;
    if first != [l.media, 0xFF] {
        return Ok(None);
    }
    Ok(Some(VolumeInfo {
        kind: l.kind,
        offset,
        size: l.size,
    }))
}

pub(super) fn read(src: &dyn Source, info: VolumeInfo, boot: &[u8]) -> io::Result<Volume> {
    let l = layout(boot).expect("probed");
    let n = l.clusters as usize + 2;
    let raw_len = match l.kind {
        FsKind::Fat12 => n * 3 / 2 + 1,
        FsKind::Fat16 => n * 2,
        _ => n * 4,
    };
    let mut raw = vec![0u8; raw_len + 1];
    read_fully(src, info.offset + l.fat_offset, &mut raw)?;
    let table = (0..n)
        .map(|i| match l.kind {
            FsKind::Fat12 => {
                let v = le16(&raw, i * 3 / 2);
                u32::from(if i % 2 == 1 { v >> 4 } else { v & 0xFFF })
            }
            FsKind::Fat16 => u32::from(le16(&raw, i * 2)),
            _ => le32(&raw, i * 4) & 0x0FFF_FFFF,
        })
        .collect();

    let mut fat = Fat {
        src,
        base: info.offset,
        l,
        table,
        entries: Vec::new(),
        visited: HashSet::new(),
        label: None,
    };
    let root = match fat.l.kind {
        FsKind::Fat32 => Dir::Chain(fat.l.root_cluster),
        _ => Dir::FixedRoot,
    };
    fat.walk(root, "", false)?;
    Ok(Volume {
        info,
        label: fat.label,
        entries: fat.entries,
    })
}

enum Dir {
    /// FAT12/16 root directory: a fixed area before the data clusters.
    FixedRoot,
    Chain(u32),
    /// A deleted directory: its cluster chain is gone, so read on from its
    /// first cluster as long as the clusters are free.
    Deleted(u32),
}

struct Fat<'a> {
    src: &'a dyn Source,
    base: u64,
    l: Layout,
    table: Vec<u32>,
    entries: Vec<FsEntry>,
    visited: HashSet<u32>,
    label: Option<String>,
}

impl Fat<'_> {
    fn valid(&self, c: u32) -> bool {
        c >= 2 && c < self.l.clusters + 2
    }

    fn next(&self, c: u32) -> u32 {
        self.table.get(c as usize).copied().unwrap_or(0)
    }

    fn end_of_chain(&self, v: u32) -> bool {
        v >= match self.l.kind {
            FsKind::Fat12 => 0xFF8,
            FsKind::Fat16 => 0xFFF8,
            _ => 0x0FFF_FFF8,
        }
    }

    fn chain(&self, start: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = start;
        while self.valid(c) && out.len() <= self.l.clusters as usize {
            out.push(c);
            let next = self.next(c);
            if self.end_of_chain(next) {
                break;
            }
            c = next;
        }
        out
    }

    fn cluster_offset(&self, c: u32) -> u64 {
        self.base + self.l.data_offset + u64::from(c - 2) * self.l.cluster
    }

    fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len as usize];
        let n = read_fully(self.src, offset, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn dir_bytes(&self, dir: &Dir) -> io::Result<Vec<u8>> {
        match *dir {
            Dir::FixedRoot => self.read(self.base + self.l.root_offset, self.l.root_len),
            Dir::Chain(start) => {
                let mut out = Vec::new();
                for c in self.chain(start) {
                    if out.len() as u64 >= MAX_DIR {
                        break;
                    }
                    out.extend(self.read(self.cluster_offset(c), self.l.cluster)?);
                }
                Ok(out)
            }
            Dir::Deleted(start) => {
                let mut out = Vec::new();
                let mut c = start;
                while self.valid(c) && self.next(c) == 0 && (out.len() as u64) < MAX_DIR {
                    let data = self.read(self.cluster_offset(c), self.l.cluster)?;
                    let ended = data.chunks_exact(32).any(|e| e[0] == 0);
                    out.extend(data);
                    if ended {
                        break;
                    }
                    c += 1;
                }
                Ok(out)
            }
        }
    }

    fn walk(&mut self, dir: Dir, path: &str, parent_deleted: bool) -> io::Result<()> {
        let data = self.dir_bytes(&dir)?;
        if let Dir::Deleted(start) = dir {
            // Only trust a deleted directory whose "." entry still points to itself.
            let dot = data.get(..32);
            if !dot.is_some_and(|e| &e[..11] == b".          " && self.start_cluster(e) == start) {
                return Ok(());
            }
        }
        let mut lfn = LongName::default();
        for e in data.chunks_exact(32) {
            if e[0] == 0 {
                break;
            }
            let attr = e[11];
            if attr == ATTR_LFN {
                lfn.push(e);
                continue;
            }
            let long_name = lfn.take(e);
            let deleted = e[0] == DELETED;
            if e[0] == b'.' {
                continue;
            }
            if attr & ATTR_VOLUME != 0 {
                if path.is_empty() && !deleted && self.label.is_none() {
                    self.label = Some(latin1(&e[..11]).trim_end().to_owned());
                }
                continue;
            }
            let name = long_name.unwrap_or_else(|| short_name(e));
            if name.is_empty() || name.contains(['/', '\0']) {
                continue;
            }
            let full = format!("{path}/{name}");
            let cluster = self.start_cluster(e);
            let size = u64::from(le32(e, 28));
            let gone = deleted || parent_deleted;
            let mut entry = FsEntry {
                path: full.clone(),
                is_dir: attr & ATTR_DIR != 0,
                size,
                deleted: gone,
                modified: dos_datetime(le16(e, 24), le16(e, 22)),
                created: dos_datetime(le16(e, 16), le16(e, 14)),
                confidence: Confidence::High,
                data: FileData::Extents(Vec::new()),
            };
            if entry.is_dir {
                entry.size = 0;
                if gone {
                    entry.confidence = Confidence::Medium;
                }
                self.entries.push(entry);
                if self.valid(cluster) && self.visited.insert(cluster) {
                    let sub = if deleted {
                        Dir::Deleted(cluster)
                    } else {
                        Dir::Chain(cluster)
                    };
                    self.walk(sub, &full, gone)?;
                }
            } else {
                (entry.data, entry.confidence) = self.file_data(cluster, size, deleted);
                self.entries.push(entry);
            }
        }
        Ok(())
    }

    fn start_cluster(&self, e: &[u8]) -> u32 {
        let high = if self.l.kind == FsKind::Fat32 {
            u32::from(le16(e, 20)) << 16
        } else {
            0
        };
        high | u32::from(le16(e, 26))
    }

    fn file_data(&self, start: u32, size: u64, deleted: bool) -> (FileData, Confidence) {
        if size == 0 {
            return (FileData::Extents(Vec::new()), Confidence::High);
        }
        if !self.valid(start) {
            return (
                FileData::Unsupported("the file's data location is lost"),
                Confidence::Low,
            );
        }
        let needed = size.div_ceil(self.l.cluster) as usize;
        let (clusters, confidence) = if deleted {
            // Deleting erases the cluster chain. Most files are stored in one
            // piece, so read on from the first cluster.
            let clusters: Vec<u32> = (start..)
                .take(needed)
                .take_while(|&c| self.valid(c))
                .collect();
            let free = clusters.iter().all(|&c| self.next(c) == 0);
            let conf = if clusters.len() == needed && free {
                Confidence::Medium
            } else {
                Confidence::Low
            };
            (clusters, conf)
        } else {
            let mut chain = self.chain(start);
            let conf = if chain.len() >= needed {
                Confidence::High
            } else {
                Confidence::Low
            };
            chain.truncate(needed);
            (chain, conf)
        };
        let mut extents = Vec::new();
        for c in clusters {
            push_extent(&mut extents, Some(self.cluster_offset(c)), self.l.cluster);
        }
        truncate_extents(&mut extents, size);
        (FileData::Extents(extents), confidence)
    }
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// The 8.3 name of a directory entry. The first letter of a deleted entry's
/// name is lost and shown as `_`.
fn short_name(e: &[u8]) -> String {
    let mut base = e[..8].to_vec();
    match base[0] {
        DELETED => base[0] = b'_',
        0x05 => base[0] = DELETED,
        _ => {}
    }
    let mut base = latin1(&base).trim_end().to_owned();
    let mut ext = latin1(&e[8..11]).trim_end().to_owned();
    if e[12] & 0x08 != 0 {
        base = base.to_lowercase();
    }
    if e[12] & 0x10 != 0 {
        ext = ext.to_lowercase();
    }
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

/// Collects the long file name entries that precede a short entry.
#[derive(Default)]
struct LongName {
    parts: Vec<[u16; 13]>,
    checksum: Option<u8>,
}

impl LongName {
    fn push(&mut self, e: &[u8]) {
        let starts_new = e[0] != DELETED && e[0] & 0x40 != 0;
        if starts_new || self.checksum.is_some_and(|c| c != e[13]) {
            self.parts.clear();
        }
        self.checksum = Some(e[13]);
        let mut units = [0u16; 13];
        let offsets = (1..11)
            .step_by(2)
            .chain((14..26).step_by(2))
            .chain((28..32).step_by(2));
        for (u, o) in units.iter_mut().zip(offsets) {
            *u = le16(e, o);
        }
        self.parts.push(units);
    }

    /// Returns the long name if it belongs to short entry `e`.
    fn take(&mut self, e: &[u8]) -> Option<String> {
        let parts = std::mem::take(&mut self.parts);
        let checksum = self.checksum.take()?;
        let rest = &e[1..11];
        let belongs = if e[0] == DELETED {
            // The first letter was overwritten; any letter that fits will do.
            (0x20..=0xFF).any(|first| short_checksum(first, rest) == checksum)
        } else {
            short_checksum(e[0], rest) == checksum
        };
        if parts.is_empty() || !belongs {
            return None;
        }
        // Parts are stored last first.
        let units: Vec<u16> = parts
            .iter()
            .rev()
            .flatten()
            .copied()
            .take_while(|&u| u != 0 && u != 0xFFFF)
            .collect();
        (!units.is_empty()).then(|| String::from_utf16_lossy(&units))
    }
}

fn short_checksum(first: u8, rest: &[u8]) -> u8 {
    std::iter::once(&first)
        .chain(rest)
        .fold(0u8, |sum, &b| sum.rotate_right(1).wrapping_add(b))
}
