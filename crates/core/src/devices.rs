//! Lists the drives that can be recovered from, and checks where recovered
//! files may safely be written.

use std::io;
use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// A whole physical disk, card or stick (all its partitions).
    Disk,
    /// One drive letter (Windows).
    Volume,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    /// What to open with [`crate::FileSource::open`].
    pub path: String,
    /// Human-readable name, e.g. the disk model or "USB drive (E:)".
    pub name: String,
    /// Size in bytes; 0 if it could not be read (usually: needs admin rights).
    pub size: u64,
    pub removable: bool,
    pub kind: DeviceKind,
}

/// Lists the disks of this computer. Reading them usually needs
/// administrator (root) rights; listing them does not.
pub fn list() -> Vec<Device> {
    imp::list()
}

/// Whether `out` (an existing or planned folder) is on the same physical disk
/// as the device `source`. Writing recovered files there would overwrite the
/// files being recovered. Only detected on Linux; always `false` elsewhere.
pub fn same_disk(source: &Path, out: &Path) -> io::Result<bool> {
    imp::same_disk(source, out)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::path::Path;

    use super::{Device, DeviceKind};

    fn read(path: impl AsRef<Path>) -> Option<String> {
        fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
    }

    pub fn list() -> Vec<Device> {
        let Ok(dir) = fs::read_dir("/sys/block") else {
            return Vec::new();
        };
        let mut out: Vec<Device> = dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if ["ram", "zram", "sr", "fd"]
                    .iter()
                    .any(|p| name.starts_with(p))
                {
                    return None;
                }
                let sys = e.path();
                let size = read(sys.join("size"))?.parse::<u64>().ok()? * 512;
                if size == 0 {
                    return None;
                }
                // Virtual disks report a numeric vendor id; it means nothing to people.
                let vendor = read(sys.join("device/vendor"))
                    .filter(|v| !v.starts_with("0x"))
                    .unwrap_or_default();
                let model = read(sys.join("device/model")).unwrap_or_default();
                let label = format!("{vendor} {model}").trim().to_owned();
                Some(Device {
                    path: format!("/dev/{name}"),
                    name: if label.is_empty() {
                        format!("Disk {name}")
                    } else {
                        label
                    },
                    size,
                    removable: read(sys.join("removable")).as_deref() == Some("1"),
                    kind: DeviceKind::Disk,
                })
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    pub fn same_disk(source: &Path, out: &Path) -> io::Result<bool> {
        let meta = fs::metadata(source)?;
        if !meta.file_type().is_block_device() {
            // Disk images are copies; writing next to them is safe.
            return Ok(false);
        }
        let Some(existing) = out.ancestors().find(|p| p.exists()) else {
            return Ok(false);
        };
        let out_dev = fs::metadata(existing)?.dev();
        Ok(disk_of(meta.rdev()) == disk_of(out_dev))
    }

    /// Name of the whole disk holding block device `dev` (e.g. `sda` for `sda1`).
    fn disk_of(dev: u64) -> String {
        let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
        let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
        let id = format!("{major}:{minor}");
        let Ok(path) = fs::canonicalize(format!("/sys/dev/block/{id}")) else {
            return id;
        };
        let disk = if path.join("partition").exists() {
            path.parent().unwrap_or(&path)
        } else {
            &path
        };
        disk.file_name()
            .map_or(id, |n| n.to_string_lossy().into_owned())
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::mem::size_of;
    use std::path::Path;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        RemovableMedia, DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::{Device, DeviceKind};

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn list() -> Vec<Device> {
        let mut out = volumes();
        out.extend(physical_drives());
        out
    }

    fn volumes() -> Vec<Device> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26u8 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = char::from(b'A' + i);
            let root = wide(&format!("{letter}:\\"));
            let ty = unsafe { GetDriveTypeW(root.as_ptr()) };
            if ty != DRIVE_REMOVABLE && ty != DRIVE_FIXED {
                continue;
            }
            let mut label = [0u16; 261];
            let ok = unsafe {
                GetVolumeInformationW(
                    root.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    0,
                )
            };
            let label = if ok != 0 {
                crate::fs::utf16_string(&label)
            } else {
                String::new()
            };
            let mut total = 0u64;
            unsafe { GetDiskFreeSpaceExW(root.as_ptr(), null_mut(), &mut total, null_mut()) };
            let kind = if ty == DRIVE_REMOVABLE {
                "USB drive"
            } else {
                "Local disk"
            };
            let name = if label.is_empty() {
                format!("{kind} ({letter}:)")
            } else {
                format!("{label} ({letter}:)")
            };
            out.push(Device {
                path: format!(r"\\.\{letter}:"),
                name,
                size: total,
                removable: ty == DRIVE_REMOVABLE,
                kind: DeviceKind::Volume,
            });
        }
        out
    }

    fn physical_drives() -> Vec<Device> {
        let mut out = Vec::new();
        for n in 0..32 {
            let path = format!(r"\\.\PhysicalDrive{n}");
            let name = wide(&path);
            // No access rights are needed to ask for the geometry.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut geometry: DISK_GEOMETRY_EX = unsafe { std::mem::zeroed() };
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                    null(),
                    0,
                    (&mut geometry as *mut DISK_GEOMETRY_EX).cast(),
                    size_of::<DISK_GEOMETRY_EX>() as u32,
                    &mut returned,
                    null_mut(),
                )
            };
            unsafe { CloseHandle(handle) };
            if ok == 0 {
                continue;
            }
            out.push(Device {
                path,
                name: format!("Disk {n}"),
                size: geometry.DiskSize as u64,
                removable: geometry.Geometry.MediaType == RemovableMedia,
                kind: DeviceKind::Disk,
            });
        }
        out
    }

    pub fn same_disk(_source: &Path, _out: &Path) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::fs;
    use std::io;
    use std::path::Path;

    use super::{Device, DeviceKind};

    pub fn list() -> Vec<Device> {
        let Ok(dir) = fs::read_dir("/dev") else {
            return Vec::new();
        };
        let mut out: Vec<Device> = dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let number = name.strip_prefix("disk")?;
                if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                // The raw device is much faster to read.
                let path = format!("/dev/r{name}");
                let size = crate::FileSource::open(Path::new(&path))
                    .map(|s| crate::Source::len(&s))
                    .unwrap_or(0);
                Some(Device {
                    path,
                    name: format!("Disk {number}"),
                    size,
                    removable: false,
                    kind: DeviceKind::Disk,
                })
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    pub fn same_disk(_source: &Path, _out: &Path) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use std::io;
    use std::path::Path;

    use super::Device;

    pub fn list() -> Vec<Device> {
        Vec::new()
    }

    pub fn same_disk(_source: &Path, _out: &Path) -> io::Result<bool> {
        Ok(false)
    }
}
