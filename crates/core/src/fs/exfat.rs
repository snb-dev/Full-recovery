//! exFAT: large SD cards, USB drives, cameras and phones.

use std::collections::HashSet;
use std::io;

use super::{
    dos_datetime, push_extent, truncate_extents, utf16_le, FileData, FsEntry, FsKind, Volume,
    VolumeInfo,
};
use crate::carve::{le16, le32, le64, Confidence};
use crate::source::{read_fully, Source};

const IN_USE: u8 = 0x80;
const FILE: u8 = 0x05;
const STREAM: u8 = 0x40;
const NAME: u8 = 0x41;
const BITMAP: u8 = 0x81;
const LABEL: u8 = 0x83;
const ATTR_DIR: u16 = 0x10;
const NO_FAT_CHAIN: u8 = 0x02;
const MAX_DIR: u64 = 256 << 20;

struct Layout {
    cluster: u64,
    fat_offset: u64,
    heap_offset: u64,
    clusters: u32,
    root: u32,
    size: u64,
}

fn layout(b: &[u8]) -> Option<Layout> {
    if &b[3..11] != b"EXFAT   " || b[510..512] != [0x55, 0xAA] {
        return None;
    }
    let (bps_shift, spc_shift) = (u32::from(b[108]), u32::from(b[109]));
    if !(9..=12).contains(&bps_shift) || bps_shift + spc_shift > 25 {
        return None;
    }
    let bps = 1u64 << bps_shift;
    let l = Layout {
        cluster: bps << spc_shift,
        fat_offset: u64::from(le32(b, 80)) * bps,
        heap_offset: u64::from(le32(b, 88)) * bps,
        clusters: le32(b, 92),
        root: le32(b, 96),
        size: le64(b, 72).saturating_mul(bps),
    };
    let fat_len = u64::from(le32(b, 84)) * bps;
    let heap_len = u64::from(l.clusters) * l.cluster;
    let valid = l.fat_offset > 0
        && l.heap_offset >= l.fat_offset + fat_len
        && fat_len >= (u64::from(l.clusters) + 2) * 4
        && l.heap_offset + heap_len <= l.size
        && l.root >= 2
        && l.root < l.clusters + 2;
    valid.then_some(l)
}

pub(super) fn probe(src: &dyn Source, offset: u64, boot: &[u8]) -> io::Result<Option<VolumeInfo>> {
    let Some(l) = layout(boot) else {
        return Ok(None);
    };
    // The FAT must fit in the source; a damaged boot sector can claim a huge one.
    let fat_end = l.fat_offset + (u64::from(l.clusters) + 2) * 4;
    if offset.saturating_add(fat_end) > src.len() {
        return Ok(None);
    }
    let mut first = [0u8; 8];
    read_fully(src, offset + l.fat_offset, &mut first)?;
    if first != [0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF] {
        return Ok(None);
    }
    Ok(Some(VolumeInfo {
        kind: FsKind::ExFat,
        offset,
        size: l.size,
    }))
}

pub(super) fn read(src: &dyn Source, info: VolumeInfo, boot: &[u8]) -> io::Result<Volume> {
    let l = layout(boot).expect("probed");
    let mut raw = vec![0u8; (l.clusters as usize + 2) * 4];
    read_fully(src, info.offset + l.fat_offset, &mut raw)?;
    let fat = raw.chunks_exact(4).map(|c| le32(c, 0)).collect();
    let mut ex = ExFat {
        src,
        base: info.offset,
        l,
        fat,
        bitmap: Vec::new(),
        entries: Vec::new(),
        visited: HashSet::new(),
        label: None,
    };

    let root = ex.read_clusters(&ex.chain(ex.l.root), MAX_DIR)?;
    for e in root.chunks_exact(32).take_while(|e| e[0] != 0) {
        match e[0] {
            BITMAP if ex.bitmap.is_empty() => {
                let clusters = ex.chain(le32(e, 20));
                ex.bitmap = ex.read_clusters(&clusters, le64(e, 24))?;
            }
            LABEL if ex.label.is_none() => {
                let chars = usize::from(e[1].min(11));
                ex.label = Some(utf16_le(&e[2..2 + chars * 2]));
            }
            _ => {}
        }
    }
    ex.visited.insert(ex.l.root);
    ex.walk(&root, "", false)?;
    Ok(Volume {
        info,
        label: ex.label,
        entries: ex.entries,
    })
}

/// A file entry set: a file entry, a stream extension and name entries.
struct EntrySet {
    name: String,
    is_dir: bool,
    deleted: bool,
    modified: Option<i64>,
    created: Option<i64>,
    first_cluster: u32,
    size: u64,
    contiguous: bool,
}

struct ExFat<'a> {
    src: &'a dyn Source,
    base: u64,
    l: Layout,
    fat: Vec<u32>,
    bitmap: Vec<u8>,
    entries: Vec<FsEntry>,
    visited: HashSet<u32>,
    label: Option<String>,
}

impl ExFat<'_> {
    fn valid(&self, c: u32) -> bool {
        c >= 2 && c < self.l.clusters + 2
    }

    fn in_use(&self, c: u32) -> bool {
        let bit = (c - 2) as usize;
        self.bitmap
            .get(bit / 8)
            .is_none_or(|byte| byte & (1 << (bit % 8)) != 0)
    }

    fn chain(&self, start: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = start;
        while self.valid(c) && out.len() <= self.l.clusters as usize {
            out.push(c);
            c = self.fat.get(c as usize).copied().unwrap_or(0);
        }
        out
    }

    fn cluster_offset(&self, c: u32) -> u64 {
        self.base + self.l.heap_offset + u64::from(c - 2) * self.l.cluster
    }

    fn read_clusters(&self, clusters: &[u32], max: u64) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        for &c in clusters {
            if out.len() as u64 >= max {
                break;
            }
            let start = out.len();
            out.resize(start + self.l.cluster as usize, 0);
            let n = read_fully(self.src, self.cluster_offset(c), &mut out[start..])?;
            out.truncate(start + n);
        }
        out.truncate(max.min(out.len() as u64) as usize);
        Ok(out)
    }

    /// Finds the clusters of a file or directory and how sure we are of them.
    fn locate(&self, set: &EntrySet) -> (Vec<u32>, Confidence) {
        let needed = set.size.div_ceil(self.l.cluster) as usize;
        let contiguous = || -> Vec<u32> {
            (set.first_cluster..)
                .take(needed)
                .take_while(|&c| self.valid(c))
                .collect()
        };
        let (clusters, exact) = if set.contiguous {
            (contiguous(), true)
        } else {
            let mut chain = self.chain(set.first_cluster);
            if chain.len() >= needed {
                chain.truncate(needed);
                (chain, true)
            } else if set.deleted {
                // The chain was erased: assume the file was stored in one piece.
                (contiguous(), false)
            } else {
                (chain, false)
            }
        };
        let confidence = if clusters.len() < needed
            || (set.deleted && clusters.iter().any(|&c| self.in_use(c)))
        {
            Confidence::Low
        } else if exact {
            Confidence::High
        } else {
            Confidence::Medium
        };
        (clusters, confidence)
    }

    fn walk(&mut self, data: &[u8], path: &str, parent_deleted: bool) -> io::Result<()> {
        let count = data.len() / 32;
        let mut i = 0;
        while i < count {
            let e = &data[i * 32..i * 32 + 32];
            if e[0] == 0 {
                break;
            }
            let secondary = usize::from(e[1]);
            if e[0] & !IN_USE == FILE && (2..=18).contains(&secondary) && i + secondary < count {
                let raw = &data[i * 32..(i + secondary + 1) * 32];
                if let Some(set) = parse_set(raw) {
                    self.add(set, path, parent_deleted)?;
                    i += secondary + 1;
                    continue;
                }
            }
            i += 1;
        }
        Ok(())
    }

    fn add(&mut self, set: EntrySet, path: &str, parent_deleted: bool) -> io::Result<()> {
        let full = format!("{path}/{}", set.name);
        let deleted = set.deleted || parent_deleted;
        let (clusters, confidence) = if set.size == 0 {
            (Vec::new(), Confidence::High)
        } else {
            self.locate(&set)
        };
        let mut entry = FsEntry {
            path: full.clone(),
            is_dir: set.is_dir,
            size: if set.is_dir { 0 } else { set.size },
            deleted,
            modified: set.modified,
            created: set.created,
            confidence,
            data: FileData::Extents(Vec::new()),
        };
        if set.is_dir {
            self.entries.push(entry);
            if confidence > Confidence::Low
                && clusters.first().is_some_and(|&c| self.visited.insert(c))
            {
                let data = self.read_clusters(&clusters, set.size.min(MAX_DIR))?;
                self.walk(&data, &full, deleted)?;
            }
            return Ok(());
        }
        let mut extents = Vec::new();
        for c in clusters {
            push_extent(&mut extents, Some(self.cluster_offset(c)), self.l.cluster);
        }
        truncate_extents(&mut extents, set.size);
        entry.data = FileData::Extents(extents);
        self.entries.push(entry);
        Ok(())
    }
}

fn parse_set(raw: &[u8]) -> Option<EntrySet> {
    let in_use = raw[0] & IN_USE;
    // Every entry of a set must agree on whether the file is deleted.
    let stream = &raw[32..64];
    if stream[0] != STREAM | in_use {
        return None;
    }
    if raw[64..].chunks_exact(32).any(|e| e[0] != NAME | in_use) {
        return None;
    }
    // Deleting only clears the in-use bits, so the checksum can be verified
    // after setting them again.
    let mut sum: u16 = 0;
    for (i, &b) in raw.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        let b = if i % 32 == 0 { b | IN_USE } else { b };
        sum = sum.rotate_right(1).wrapping_add(u16::from(b));
    }
    if sum != le16(raw, 2) {
        return None;
    }
    let name_len = usize::from(stream[3]);
    let units: Vec<u8> = raw[64..]
        .chunks_exact(32)
        .flat_map(|e| e[2..32].iter().copied())
        .take(name_len * 2)
        .collect();
    let name = utf16_le(&units);
    if name.is_empty() || name.contains(['/', '\0']) {
        return None;
    }
    let time = |at: usize| {
        let t = le32(raw, at);
        dos_datetime((t >> 16) as u16, t as u16)
    };
    Some(EntrySet {
        name,
        is_dir: le16(raw, 4) & ATTR_DIR != 0,
        deleted: in_use == 0,
        modified: time(12),
        created: time(8),
        first_cluster: le32(stream, 20),
        size: le64(stream, 24),
        contiguous: stream[1] & NO_FAT_CHAIN != 0,
    })
}
