//! Read-only access to the data being recovered.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::path::Path;

/// Random-access, read-only view of a disk image, partition or raw device.
pub trait Source: Send + Sync {
    /// Total size in bytes.
    fn len(&self) -> u64;

    /// Reads up to `buf.len()` bytes at `offset`. Returns 0 at the end.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Reads until `buf` is full or the source ends. Returns the bytes read.
pub fn read_fully(src: &dyn Source, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match src.read_at(offset + done as u64, &mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// A file, disk image or raw device opened read-only.
pub struct FileSource {
    file: File,
    len: u64,
    /// Raw Windows devices only accept sector-aligned reads.
    align: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut len = file.metadata()?.len();
        if len == 0 {
            // Block devices report a size of 0 in their metadata.
            len = file.seek(SeekFrom::End(0)).unwrap_or(0);
        }
        if len == 0 {
            len = device_len(&file).unwrap_or(0);
        }
        let align = if cfg!(windows) && path.to_string_lossy().starts_with(r"\\.\") {
            4096
        } else {
            1
        };
        Ok(Self { file, len, align })
    }

    /// Overrides the detected size, for devices whose size cannot be queried.
    pub fn with_len(mut self, len: u64) -> Self {
        self.len = len;
        self
    }

    fn read_raw(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(buf, offset)
        }
    }
}

/// Asks the operating system for the size of a raw device.
#[cfg(windows)]
fn device_len(file: &File) -> Option<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let mut info = GET_LENGTH_INFORMATION { Length: 0 };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            IOCTL_DISK_GET_LENGTH_INFO,
            std::ptr::null(),
            0,
            (&mut info as *mut GET_LENGTH_INFORMATION).cast(),
            std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(info.Length as u64)
}

#[cfg(target_os = "macos")]
fn device_len(file: &File) -> Option<u64> {
    use std::os::fd::AsRawFd;
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;

    let (mut size, mut count) = (0u32, 0u64);
    let fd = file.as_raw_fd();
    let ok = unsafe {
        libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut size) == 0
            && libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut count) == 0
    };
    ok.then_some(u64::from(size) * count)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn device_len(_file: &File) -> Option<u64> {
    None
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let max = (self.len - offset).min(buf.len() as u64) as usize;
        let buf = &mut buf[..max];
        let a = self.align;
        if a <= 1 || (offset % a == 0 && max as u64 % a == 0) {
            return self.read_raw(offset, buf);
        }
        let start = offset / a * a;
        let end = (offset + max as u64).div_ceil(a) * a;
        let mut tmp = vec![0u8; (end - start) as usize];
        let n = self.read_raw(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        if n <= skip {
            return Ok(0);
        }
        let k = (n - skip).min(max);
        buf[..k].copy_from_slice(&tmp[skip..skip + k]);
        Ok(k)
    }
}

impl Source for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Some(rest) = usize::try_from(offset).ok().and_then(|o| self.get(o..)) else {
            return Ok(0);
        };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }
}
