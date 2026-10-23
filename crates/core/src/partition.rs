//! Partition tables (MBR and GPT) and searching for lost partitions.

use std::io;
use std::ops::ControlFlow;

use serde::Serialize;

use crate::carve::{le16, le32, le64};
use crate::fs::{self, utf16_le, FsKind, VolumeInfo};
use crate::source::{read_fully, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    Mbr,
    Gpt,
    /// The whole device holds a single file system (common on USB sticks).
    None,
    /// Found by [`search`] rather than in a partition table.
    Found,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Partition {
    /// 1-based number, in table order.
    pub index: u32,
    pub scheme: Scheme,
    pub offset: u64,
    pub size: u64,
    /// Where the file system's boot sector was read: `offset`, or a backup
    /// copy if the first sector is damaged. Pass it to [`fs::open_boot`].
    pub boot_offset: u64,
    /// Partition type, e.g. "Microsoft basic data".
    pub kind: String,
    /// GPT partition name.
    pub name: Option<String>,
    pub file_system: Option<FsKind>,
}

/// Reads the partition table. Partitions whose file system is recognised
/// have `file_system` set.
pub fn read_table(src: &dyn Source) -> io::Result<Vec<Partition>> {
    let mut parts = match read_gpt(src)? {
        Some(parts) => parts,
        None => read_mbr(src)?,
    };
    if parts.is_empty() {
        if let Some(v) = fs::probe(src, 0)? {
            parts.push(Partition {
                index: 1,
                scheme: Scheme::None,
                offset: 0,
                size: v.size,
                boot_offset: 0,
                kind: "whole device".into(),
                name: None,
                file_system: Some(v.kind),
            });
        }
        return Ok(parts);
    }
    for p in &mut parts {
        p.file_system = fs::probe(src, p.offset)?.map(|v| v.kind);
        // NTFS keeps a copy of its boot sector in the partition's last sector.
        let backup = (p.offset + p.size).saturating_sub(512);
        if p.file_system.is_none() && backup > p.offset {
            if let Some(v) = fs::probe_boot(src, p.offset, backup)? {
                p.file_system = Some(v.kind);
                p.boot_offset = backup;
            }
        }
    }
    Ok(parts)
}

fn read_gpt(src: &dyn Source) -> io::Result<Option<Vec<Partition>>> {
    for sector in [512u64, 4096] {
        let disk_sectors = src.len() / sector;
        // The backup header at the end of the disk helps if the main one is damaged.
        for lba in [1, disk_sectors.saturating_sub(1)] {
            if let Some(parts) = read_gpt_at(src, sector, lba)? {
                return Ok(Some(parts));
            }
        }
    }
    Ok(None)
}

fn read_gpt_at(src: &dyn Source, sector: u64, lba: u64) -> io::Result<Option<Vec<Partition>>> {
    let mut h = vec![0u8; sector as usize];
    if lba == 0 || read_fully(src, lba * sector, &mut h)? < h.len() || &h[..8] != b"EFI PART" {
        return Ok(None);
    }
    let header_size = le32(&h, 12) as usize;
    if !(92..=h.len()).contains(&header_size) {
        return Ok(None);
    }
    let mut check = h[..header_size].to_vec();
    check[16..20].fill(0);
    if crc32fast::hash(&check) != le32(&h, 16) {
        return Ok(None);
    }
    let entries_lba = le64(&h, 72);
    let count = le32(&h, 80).min(1024) as usize;
    let entry_size = le32(&h, 84) as usize;
    if entry_size < 128 {
        return Ok(None);
    }
    let mut table = vec![0u8; count * entry_size];
    read_fully(src, entries_lba.saturating_mul(sector), &mut table)?;
    let mut parts = Vec::new();
    for e in table.chunks_exact(entry_size) {
        let type_guid = &e[..16];
        let (first, last) = (le64(e, 32), le64(e, 40));
        if type_guid.iter().all(|&b| b == 0) || last < first {
            continue;
        }
        let name = utf16_le(&e[56..128]);
        parts.push(Partition {
            index: parts.len() as u32 + 1,
            scheme: Scheme::Gpt,
            offset: first * sector,
            size: (last - first + 1) * sector,
            boot_offset: first * sector,
            kind: gpt_type_name(type_guid),
            name: (!name.is_empty()).then_some(name),
            file_system: None,
        });
    }
    Ok(Some(parts))
}

fn guid_string(g: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{}",
        le32(g, 0),
        le16(g, 4),
        le16(g, 6),
        g[8],
        g[9],
        g[10..16]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>()
    )
}

fn gpt_type_name(g: &[u8]) -> String {
    let guid = guid_string(g);
    let name = match guid.as_str() {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI system",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        _ => return guid,
    };
    name.into()
}

fn mbr_type_name(t: u8) -> String {
    let name = match t {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x0B | 0x0C => "FAT32",
        0x07 => "NTFS/exFAT",
        0x27 => "Windows recovery",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0xAF => "Apple HFS+",
        0xEF => "EFI system",
        _ => return format!("type 0x{t:02X}"),
    };
    name.into()
}

fn is_extended(t: u8) -> bool {
    matches!(t, 0x05 | 0x0F | 0x85)
}

fn read_mbr(src: &dyn Source) -> io::Result<Vec<Partition>> {
    let mut mbr = [0u8; 512];
    if read_fully(src, 0, &mut mbr)? < 512 || mbr[510..] != [0x55, 0xAA] {
        return Ok(Vec::new());
    }
    // A boot sector of a file system also ends in 55 AA but is not an MBR.
    if fs::probe(src, 0)?.is_some() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    for e in mbr[446..510].chunks_exact(16) {
        let (status, ty) = (e[0], e[4]);
        let (start, count) = (u64::from(le32(e, 8)), u64::from(le32(e, 12)));
        if ty == 0 || count == 0 || start == 0 || (status != 0 && status != 0x80) {
            continue;
        }
        if is_extended(ty) {
            read_logical(src, start, &mut parts)?;
        } else {
            push_mbr(&mut parts, ty, start, count);
        }
    }
    Ok(parts)
}

/// Follows the chain of extended boot records holding logical partitions.
fn read_logical(src: &dyn Source, ext_start: u64, parts: &mut Vec<Partition>) -> io::Result<()> {
    let mut ebr_lba = ext_start;
    for _ in 0..128 {
        let mut ebr = [0u8; 512];
        if read_fully(src, ebr_lba * 512, &mut ebr)? < 512 || ebr[510..] != [0x55, 0xAA] {
            break;
        }
        let (logical, next) = (&ebr[446..462], &ebr[462..478]);
        if logical[4] != 0 && le32(logical, 12) != 0 {
            let start = ebr_lba + u64::from(le32(logical, 8));
            push_mbr(parts, logical[4], start, u64::from(le32(logical, 12)));
        }
        if !is_extended(next[4]) || le32(next, 8) == 0 {
            break;
        }
        ebr_lba = ext_start + u64::from(le32(next, 8));
    }
    Ok(())
}

fn push_mbr(parts: &mut Vec<Partition>, ty: u8, start: u64, count: u64) {
    if ty == 0xEE {
        // Protective entry of a GPT disk whose GPT could not be read.
        return;
    }
    parts.push(Partition {
        index: parts.len() as u32 + 1,
        scheme: Scheme::Mbr,
        offset: start * 512,
        size: count * 512,
        boot_offset: start * 512,
        kind: mbr_type_name(ty),
        name: None,
        file_system: None,
    });
}

/// Searches the whole source for file systems, including ones whose partition
/// was deleted or whose partition table is damaged. `progress` gets
/// `(done, total)` and can stop the search.
pub fn search(
    src: &dyn Source,
    progress: &mut dyn FnMut(u64, u64) -> ControlFlow<()>,
) -> io::Result<Vec<Partition>> {
    const CHUNK: usize = 4 << 20;
    let total = src.len();
    // (volume, where its boot sector was found)
    let mut found: Vec<(VolumeInfo, u64)> = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    let mut pos = 0u64;
    while pos < total {
        let got = read_fully(src, pos, &mut buf).unwrap_or(0);
        for (i, sector) in buf[..got].chunks_exact(512).enumerate() {
            if sector[510..] != [0x55, 0xAA] {
                continue;
            }
            let at = pos + (i * 512) as u64;
            for candidate in volume_starts(sector, at) {
                if found.iter().any(|(v, _)| v.offset == candidate) {
                    continue;
                }
                if let Some(v) = fs::probe_boot(src, candidate, at)? {
                    found.push((v, at));
                    break;
                }
            }
        }
        pos += CHUNK as u64;
        if progress(pos.min(total), total).is_break() {
            break;
        }
    }
    found.sort_by_key(|(v, _)| v.offset);
    Ok(found
        .into_iter()
        .enumerate()
        .map(|(i, (v, boot_offset))| Partition {
            index: i as u32 + 1,
            scheme: Scheme::Found,
            offset: v.offset,
            size: v.size,
            boot_offset,
            kind: "found by search".into(),
            name: None,
            file_system: Some(v.kind),
        })
        .collect())
}

/// Where a volume could start if `sector` (at `at`) is its boot sector or one
/// of its backup copies.
fn volume_starts(sector: &[u8], at: u64) -> Vec<u64> {
    let bps = u64::from(le16(sector, 11));
    let mut starts = vec![at];
    if &sector[3..11] == b"NTFS    " {
        // The backup is the volume's last sector.
        let backup_of = le64(sector, 40)
            .checked_mul(bps)
            .and_then(|n| at.checked_sub(n));
        starts.extend(backup_of);
    } else if &sector[3..11] == b"EXFAT   " {
        // Backup boot region starts at sector 12.
        starts.extend(at.checked_sub(12 << sector[108].min(12)));
    } else if &sector[82..90] == b"FAT32   " {
        let backup_sector = u64::from(le16(sector, 50));
        starts.extend(
            at.checked_sub(backup_sector * bps)
                .filter(|_| backup_sector != 0),
        );
    }
    starts
}
