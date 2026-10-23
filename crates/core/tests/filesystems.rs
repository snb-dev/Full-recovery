//! Recovery from real FAT12/16/32, exFAT and NTFS images made by each file
//! system's own tools (see `images/make-images.sh`).

use std::io::Read;
use std::ops::ControlFlow;

use flate2::read::GzDecoder;
use fullrecovery_core::fs::{self, FsEntry, FsKind, Volume};
use fullrecovery_core::partition::{self, Scheme};
use fullrecovery_core::Confidence;

fn image(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/images/{name}.img.gz", env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    GzDecoder::new(std::fs::File::open(&path).unwrap())
        .read_to_end(&mut out)
        .unwrap();
    out
}

/// Same content as `make_file` in make-images.sh.
fn content(name: &str, size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while out.len() < size {
        out.extend_from_slice(format!("{name}:{i}\n").as_bytes());
        i += 1;
    }
    out.truncate(size);
    out
}

/// 2024-03-15 10:30:00 UTC, the time given to every test file.
const MODIFIED: i64 = 1_710_498_600;

/// (path, size, deleted) of every file on the test images.
const FILES: &[(&str, u64, bool)] = &[
    ("/readme.txt", 1000, false),
    ("/big.bin", 300_000, false),
    ("/empty.txt", 0, false),
    ("/tiny.txt", 50, true),
    ("/ünïcødé файл.txt", 2000, true),
    ("/Photos/holiday photo 2024.jpg", 20000, false),
    ("/Photos/deleted picture.jpg", 15000, true),
    ("/Documents/notes.txt", 300, false),
    ("/Documents/report final.docx", 8000, true),
    ("/Old Stuff/letter.txt", 5000, true),
    ("/Old Stuff/data.bin", 12000, true),
];

/// FAT loses the first letter of deleted short (8.3) names.
fn fat_path(path: &str, deleted: bool) -> String {
    let (dir, name) = path.rsplit_once('/').unwrap();
    let short = name.len() <= 12 && name.is_ascii() && !name.contains(' ');
    if deleted && short {
        format!("{dir}/_{}", &name[1..])
    } else {
        path.to_owned()
    }
}

fn find<'a>(v: &'a Volume, path: &str) -> &'a FsEntry {
    v.entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| {
            let paths: Vec<_> = v.entries.iter().map(|e| &e.path).collect();
            panic!("{path} not found in {paths:?}")
        })
}

fn check_volume(data: &Vec<u8>, v: &Volume, kind: FsKind) {
    assert_eq!(v.info.kind, kind);
    assert_eq!(v.label.as_deref(), Some("TESTVOL"));
    let fat = matches!(kind, FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32);
    for &(path, size, deleted) in FILES {
        let path = if fat {
            fat_path(path, deleted)
        } else {
            path.to_owned()
        };
        let e = find(v, &path);
        assert!(!e.is_dir, "{path}");
        assert_eq!((e.size, e.deleted), (size, deleted), "{path}");
        assert_eq!(e.modified, Some(MODIFIED), "{path}");
        let expected_confidence = if deleted && fat {
            // FAT erases where a deleted file's data was; it is assumed contiguous.
            Confidence::Medium
        } else {
            Confidence::High
        };
        assert_eq!(e.confidence, expected_confidence, "{path}");

        let mut out = Vec::new();
        fs::extract(data, e, &mut out).unwrap();
        let name = path.rsplit('/').next().unwrap();
        let original = FILES
            .iter()
            .map(|f| f.0.rsplit('/').next().unwrap())
            .find(|n| {
                *n == name
                    || name
                        .strip_prefix('_')
                        .is_some_and(|rest| n.get(1..) == Some(rest))
            })
            .unwrap();
        assert!(
            out == content(original, size as usize),
            "{path}: content differs"
        );
    }
    for (dir, deleted) in [
        ("/Photos", false),
        ("/Documents", false),
        ("/Old Stuff", true),
    ] {
        let e = find(v, dir);
        assert!(e.is_dir && e.deleted == deleted, "{dir}");
    }
    let files = v.entries.iter().filter(|e| !e.is_dir).count();
    assert_eq!(files, FILES.len(), "unexpected entries: {:?}", v.entries);
}

fn check_image(name: &str, kind: FsKind) {
    let data = image(name);
    let parts = partition::read_table(&data).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(
        (parts[0].scheme, parts[0].file_system),
        (Scheme::None, Some(kind))
    );
    let v = fs::open(&data, 0).unwrap().unwrap();
    assert_eq!(v.info.size, data.len() as u64);
    check_volume(&data, &v, kind);
}

#[test]
fn fat12() {
    check_image("fat12", FsKind::Fat12);
}

#[test]
fn fat16() {
    check_image("fat16", FsKind::Fat16);
}

#[test]
fn fat32() {
    check_image("fat32", FsKind::Fat32);
}

#[test]
fn exfat() {
    check_image("exfat", FsKind::ExFat);
}

#[test]
fn ntfs() {
    check_image("ntfs", FsKind::Ntfs);
}

const MIB: usize = 1 << 20;

/// A disk with a FAT16 partition at 1 MiB and an NTFS partition after it.
fn disk() -> (Vec<u8>, [(usize, usize); 2]) {
    let (fat, ntfs) = (image("fat16"), image("ntfs"));
    let fat_at = MIB;
    let ntfs_at = fat_at + fat.len();
    let mut disk = vec![0u8; ntfs_at + ntfs.len() + MIB];
    disk[fat_at..ntfs_at].copy_from_slice(&fat);
    disk[ntfs_at..ntfs_at + ntfs.len()].copy_from_slice(&ntfs);
    (disk, [(fat_at, fat.len()), (ntfs_at, ntfs.len())])
}

#[test]
fn mbr_partitions() {
    let (mut disk, parts) = disk();
    for (i, ((at, len), ty)) in parts.iter().zip([0x06u8, 0x07]).enumerate() {
        let e = &mut disk[446 + i * 16..462 + i * 16];
        e[4] = ty;
        e[8..12].copy_from_slice(&((at / 512) as u32).to_le_bytes());
        e[12..16].copy_from_slice(&((len / 512) as u32).to_le_bytes());
    }
    disk[510..512].copy_from_slice(&[0x55, 0xAA]);

    let found = partition::read_table(&disk).unwrap();
    let summary: Vec<_> = found
        .iter()
        .map(|p| (p.scheme, p.offset, p.file_system))
        .collect();
    assert_eq!(
        summary,
        [
            (Scheme::Mbr, parts[0].0 as u64, Some(FsKind::Fat16)),
            (Scheme::Mbr, parts[1].0 as u64, Some(FsKind::Ntfs)),
        ]
    );
    let v = fs::open(&disk, found[1].offset).unwrap().unwrap();
    check_volume(&disk, &v, FsKind::Ntfs);
}

fn write_gpt_header(disk: &mut [u8], lba: u64, entries_lba: u64, entries: &[u8]) {
    let mut h = vec![0u8; 92];
    h[..8].copy_from_slice(b"EFI PART");
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    h[12..16].copy_from_slice(&92u32.to_le_bytes());
    h[24..32].copy_from_slice(&lba.to_le_bytes());
    h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    h[80..84].copy_from_slice(&128u32.to_le_bytes());
    h[84..88].copy_from_slice(&128u32.to_le_bytes());
    h[88..92].copy_from_slice(&crc32fast::hash(entries).to_le_bytes());
    let crc = crc32fast::hash(&h);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    let at = lba as usize * 512;
    disk[at..at + 92].copy_from_slice(&h);
    let e = entries_lba as usize * 512;
    disk[e..e + entries.len()].copy_from_slice(entries);
}

#[test]
fn gpt_partitions_and_backup_header() {
    let (mut disk, parts) = disk();
    // "Microsoft basic data" type GUID, as stored on disk.
    let basic_data = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];
    let mut entries = vec![0u8; 128 * 128];
    for (i, (at, len)) in parts.iter().enumerate() {
        let e = &mut entries[i * 128..(i + 1) * 128];
        e[..16].copy_from_slice(&basic_data);
        e[16] = i as u8 + 1;
        e[32..40].copy_from_slice(&((at / 512) as u64).to_le_bytes());
        e[40..48].copy_from_slice(&(((at + len) / 512 - 1) as u64).to_le_bytes());
        for (j, u) in "Data".encode_utf16().enumerate() {
            e[56 + j * 2..58 + j * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    let last = (disk.len() / 512 - 1) as u64;
    write_gpt_header(&mut disk, 1, 2, &entries);
    write_gpt_header(&mut disk, last, last - 32, &entries);

    let check = |disk: &Vec<u8>| {
        let found = partition::read_table(disk).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.scheme == Scheme::Gpt
            && p.kind == "Microsoft basic data"
            && p.name.as_deref() == Some("Data")));
        assert_eq!(found[0].file_system, Some(FsKind::Fat16));
        assert_eq!(found[1].file_system, Some(FsKind::Ntfs));
    };
    check(&disk);
    // Damage the main header: the backup at the end of the disk is used.
    disk[512..1024].fill(0);
    check(&disk);
}

#[test]
fn search_finds_lost_partitions() {
    let (mut disk, parts) = disk();
    // No partition table, and the NTFS boot sector is wiped: only its backup
    // in the partition's last sector is left.
    disk[parts[1].0..parts[1].0 + 512].fill(0);
    assert!(partition::read_table(&disk).unwrap().is_empty());

    let found = partition::search(&disk, &mut |_, _| ControlFlow::Continue(())).unwrap();
    let summary: Vec<_> = found.iter().map(|p| (p.offset, p.file_system)).collect();
    assert_eq!(
        summary,
        [
            (parts[0].0 as u64, Some(FsKind::Fat16)),
            (parts[1].0 as u64, Some(FsKind::Ntfs)),
        ]
    );
    let ntfs = &found[1];
    assert_ne!(ntfs.boot_offset, ntfs.offset);
    let v = fs::open_boot(&disk, ntfs.offset, ntfs.boot_offset)
        .unwrap()
        .unwrap();
    check_volume(&disk, &v, FsKind::Ntfs);
}

/// Damaged disks are the normal case for recovery: reading one must never
/// crash, whatever its metadata contains.
#[test]
fn damaged_metadata_does_not_crash() {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for name in ["fat12", "fat32", "exfat", "ntfs"] {
        let original = image(name);
        for round in 0..60 {
            let mut data = original.clone();
            // Mostly keep the boot sector so the volume is still recognised;
            // damage the start of the volume (where metadata lives) and random spots.
            for i in 0..64 {
                let range = if i % 2 == 0 { 256 << 10 } else { data.len() };
                let at = 512 + (next() as usize) % (range - 512);
                data[at] = next() as u8;
            }
            if round % 3 == 0 {
                // Damage boot sector fields too (sizes, offsets, counts).
                let at = 11 + (next() as usize) % 100;
                data[at] = next() as u8;
            }
            if round % 10 == 0 {
                let at = 512 + (next() as usize) % (data.len() - 4096);
                data[at..at + 4096].fill(0xFF);
            }
            let _ = partition::read_table(&data);
            if let Ok(Some(v)) = fs::open(&data, 0) {
                for e in &v.entries {
                    let _ = fs::extract(&data, e, &mut std::io::sink());
                }
            }
        }
    }
}
