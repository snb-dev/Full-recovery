# Full Recovery

Free and open-source data recovery for **Windows, macOS, Linux and Android**.
Get back deleted photos, videos, music, documents and more from hard disks,
USB drives, SD cards and memory cards, even after formatting.

No payments, no "preview only" limits, no accounts.

> **Status:** early development. The recovery engine and a command-line tool
> work today. The desktop and Android apps are next (see [Roadmap](#roadmap)).

## What works now

- **Deep scan (file carving):** finds files by their content, so it works even
  when the file system is formatted, damaged or missing.
- **Verified results:** every file is checked against its format's structure
  (and checksums where the format has them) and gets a **recovery confidence**:
  - `high`: the file's whole structure was verified
  - `medium`: looks right but could not be fully verified
  - `low`: the file is damaged or fragmented; the readable part was saved
- **Drive imaging for failing drives:** copy a dying drive once, skipping bad
  sectors, then recover from the copy without stressing the drive again.
- **Read-only:** the drive being recovered is never written to. On Linux the
  tool refuses to save recovered files onto the same disk.

### Supported file types

| Type     | Formats                                                    |
|----------|------------------------------------------------------------|
| Photo    | JPG, PNG, GIF, BMP, WebP, HEIC (iPhone), AVIF, CR3 (Canon) |
| Video    | MP4, MOV, 3GP, AVI, MKV, WebM, OGV                         |
| Audio    | WAV, M4A, OGG, Opus                                        |
| Document | PDF, DOCX, XLSX, PPTX, ODT, ODS, ODP, ODG, EPUB            |
| Archive  | ZIP, 7Z, APK, JAR                                          |
| Database | SQLite (contacts, messages, app data)                      |

Run `fullrecovery types` for the current list.

## Install

Pre-built downloads will come with the first release. For now, build from
source with [Rust](https://rustup.rs):

```sh
git clone https://github.com/snb-dev/Full-recovery
cd Full-recovery
cargo build --release
# The program is at target/release/fullrecovery
```

## How to use

> **Most important rule:** stop using the drive you want to recover from, and
> always save recovered files to a **different** drive. New data overwrites
> deleted files.

```sh
# See what can be recovered (nothing is saved)
fullrecovery scan /dev/sdb

# Recover everything to a folder on another drive
fullrecovery recover /dev/sdb --out /media/backup/recovered

# Only photos and videos, only fully verified files
fullrecovery recover /dev/sdb --out ./recovered --types jpg,heic,mp4,mov --min-confidence high

# Failing drive? Copy it first, then recover from the copy
fullrecovery image /dev/sdb sdb.img
fullrecovery recover sdb.img --out ./recovered
```

Drive names:

- **Linux:** `/dev/sdb`, `/dev/mmcblk0` (SD card), `/dev/nvme0n1`. Run with `sudo`.
- **macOS:** `/dev/rdisk2`. Find it with `diskutil list`. Run with `sudo`.
- **Windows:** `\\.\PhysicalDrive1` or `\\.\E:`. Run as Administrator.

Disk image files (`.img`, `.dd`, `.raw`) work on every system.

Found files are saved in folders by type (`photo/`, `video/`, ...), and a
`report.json` lists every file with its location, size and confidence.

## What no recovery tool can do

These limits apply to every recovery program, paid ones included. We tell you
the truth instead of selling false hope:

- **SSDs and TRIM:** when a file is deleted on most SSDs (and on many phones),
  the drive erases the data itself soon after. Once that happens, nothing can
  bring it back. Act quickly and stop using the drive.
- **Modern Android phones:** since Android 10, internal storage is encrypted
  file by file. Deleting a file destroys its key, so its data can't be read.
  On phones, we can recover from SD cards, thumbnails and caches, app trash
  folders and backups. Apps that claim more are usually restoring backups.
- **Overwritten data:** if new files were saved over the old ones, the old
  data is gone.
- **Encrypted drives** (BitLocker, FileVault, LUKS) need their key or password.

## Roadmap

1. **Core engine** ✅: deep scan, 30+ formats, confidence scores, drive imaging
2. **File systems:** recover deleted files *with their original names and
   folders* from NTFS, FAT32, exFAT, then ext4, APFS and HFS+; find lost
   partitions
3. **Desktop app** (Windows, macOS, Linux): drive list, scan, preview photos,
   videos and documents before recovering, filters and search
4. **Android app:** SD card and storage scan, thumbnail and cache recovery,
   deeper scan on rooted phones
5. **Modern extras:** repair broken photos and videos, duplicate detection,
   drive health (SMART) warnings, "protection mode" that keeps deleted files
   safe for easy recovery, more formats (camera RAW, MP3, Office 97-2003, ...)

## Project layout

```
crates/core   recovery engine (library, used by every app)
crates/cli    `fullrecovery` command-line tool
```

The engine is written in Rust, which is fast and memory-safe, and the same
code builds for desktop and Android. The apps will use Flutter for one shared
interface on all platforms.

## Contributing

Contributions are welcome! Before sending a change, run:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

To add a new file type, add a carver in `crates/core/src/carve/`, register it
in `carve/mod.rs`, and add a test file to `crates/core/src/fixtures.rs`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
