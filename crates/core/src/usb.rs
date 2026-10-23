//! Reads USB sticks and card readers directly over USB, without the
//! operating system's storage driver. This is how the Android app reads a
//! memory card in a USB card reader without root: Android hands the app the
//! USB device, and this module speaks the USB mass storage protocol
//! ("Bulk-Only Transport") with SCSI commands to read its blocks.

use std::io;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::source::Source;

/// Moves bytes to and from the device's two bulk endpoints.
///
/// A stalled endpoint must be reported as `io::ErrorKind::BrokenPipe`
/// (what the kernel reports as `EPIPE`).
pub trait BulkTransport: Send {
    fn bulk_out(&mut self, data: &[u8]) -> io::Result<()>;
    /// Reads up to `buf.len()` bytes; a short read ends the data phase.
    fn bulk_in(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Clears a stall on the IN (`true`) or OUT endpoint.
    fn clear_halt(&mut self, in_endpoint: bool) -> io::Result<()>;
    /// Bulk-Only Mass Storage Reset (class request to the interface).
    fn reset(&mut self) -> io::Result<()>;
}

const CBW_SIGNATURE: &[u8; 4] = b"USBC";
const CSW_SIGNATURE: &[u8; 4] = b"USBS";
/// Bytes asked for in one bulk transfer; some kernels refuse more.
const TRANSFER: usize = 16 << 10;
/// Bytes read with one SCSI READ command.
const READ_CHUNK: u64 = 256 << 10;
/// Pause between checks while a card reader gets ready.
const READY_WAIT: Duration = Duration::from_millis(if cfg!(test) { 1 } else { 300 });

fn stalled(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::BrokenPipe
}

/// A USB mass storage device (stick, card reader, external disk).
pub struct MassStorage<T> {
    inner: Mutex<Inner<T>>,
    block_size: u64,
    blocks: u64,
    /// Vendor and product as the device reports them, e.g. "Generic STORAGE DEVICE".
    pub name: String,
}

struct Inner<T> {
    transport: T,
    tag: u32,
}

/// The device reported an error for a command.
fn scsi_error(op: u8, sense: Option<[u8; 3]>) -> io::Error {
    let what = match sense {
        Some([0x02, 0x3A, _]) => "no memory card in the reader".to_owned(),
        Some([0x02, ..]) => "the device is not ready".to_owned(),
        Some([0x03, ..]) => "unreadable sector".to_owned(),
        Some([key, asc, ascq]) => {
            format!("command {op:#04x} failed (sense {key:#x}/{asc:#04x}/{ascq:#04x})")
        }
        None => format!("command {op:#04x} failed"),
    };
    io::Error::other(what)
}

impl<T: BulkTransport> Inner<T> {
    /// Sends one SCSI command and reads its data into `data`.
    /// Returns the number of bytes received.
    fn command(&mut self, cdb: &[u8], data: &mut [u8]) -> io::Result<usize> {
        match self.transfer(cdb, data) {
            Ok(Some(n)) => Ok(n),
            Ok(None) => {
                let sense = if cdb[0] == 0x03 { None } else { self.sense() };
                Err(scsi_error(cdb[0], sense))
            }
            Err(e) => {
                // Get the device back to a known state for the next command.
                let _ = self.reset_recovery();
                Err(e)
            }
        }
    }

    /// One Bulk-Only Transport exchange: command, data, status.
    /// `Ok(None)` means the device reported that the command failed.
    fn transfer(&mut self, cdb: &[u8], data: &mut [u8]) -> io::Result<Option<usize>> {
        self.tag = self.tag.wrapping_add(1);
        let mut cbw = [0u8; 31];
        cbw[..4].copy_from_slice(CBW_SIGNATURE);
        cbw[4..8].copy_from_slice(&self.tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        cbw[12] = if data.is_empty() { 0 } else { 0x80 };
        cbw[14] = cdb.len() as u8;
        cbw[15..15 + cdb.len()].copy_from_slice(cdb);
        self.transport.bulk_out(&cbw)?;

        let mut got = 0;
        while got < data.len() {
            let want = (data.len() - got).min(TRANSFER);
            match self.transport.bulk_in(&mut data[got..got + want]) {
                Ok(n) => {
                    got += n;
                    if n < want {
                        break;
                    }
                }
                // The device has less data than asked for (e.g. an error).
                Err(e) if stalled(&e) => {
                    self.transport.clear_halt(true)?;
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        let mut csw = [0u8; 13];
        let n = match self.transport.bulk_in(&mut csw) {
            Err(e) if stalled(&e) => {
                self.transport.clear_halt(true)?;
                self.transport.bulk_in(&mut csw)?
            }
            other => other?,
        };
        if n != 13 || &csw[..4] != CSW_SIGNATURE || csw[4..8] != self.tag.to_le_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the USB device answered out of turn",
            ));
        }
        match csw[12] {
            0 => Ok(Some(got)),
            1 => Ok(None),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the USB device reported a phase error",
            )),
        }
    }

    /// Asks why the last command failed: (sense key, ASC, ASCQ).
    fn sense(&mut self) -> Option<[u8; 3]> {
        let mut data = [0u8; 18];
        let n = self.transfer(&[0x03, 0, 0, 0, 18, 0], &mut data).ok()??;
        (n >= 14).then(|| [data[2] & 0x0F, data[12], data[13]])
    }

    fn reset_recovery(&mut self) -> io::Result<()> {
        self.transport.reset()?;
        self.transport.clear_halt(true)?;
        self.transport.clear_halt(false)
    }
}

impl<T: BulkTransport> MassStorage<T> {
    /// Identifies the device and reads its size. Waits a little for card
    /// readers that need time after the card is inserted.
    pub fn open(transport: T) -> io::Result<Self> {
        let mut inner = Inner { transport, tag: 0 };
        let mut inquiry = [0u8; 36];
        let n = inner.command(&[0x12, 0, 0, 0, 36, 0], &mut inquiry)?;
        let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_owned();
        let name = if n >= 32 {
            format!("{} {}", text(&inquiry[8..16]), text(&inquiry[16..32]))
                .trim()
                .to_owned()
        } else {
            String::new()
        };

        let mut ready = Err(io::Error::other("the device is not ready"));
        for attempt in 0..10 {
            ready = inner.command(&[0, 0, 0, 0, 0, 0], &mut []).map(|_| ());
            if ready.is_ok() {
                break;
            }
            if attempt < 9 {
                thread::sleep(READY_WAIT);
            }
        }
        ready?;

        let mut cap = [0u8; 8];
        inner.command(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &mut cap)?;
        let mut last = u64::from(u32::from_be_bytes(cap[..4].try_into().unwrap()));
        let mut block_size = u64::from(u32::from_be_bytes(cap[4..].try_into().unwrap()));
        if last == 0xFFFF_FFFF {
            // Over 2 TB: READ CAPACITY (16).
            let mut cap16 = [0u8; 32];
            let mut cdb = [0u8; 16];
            cdb[0] = 0x9E;
            cdb[1] = 0x10;
            cdb[13] = 32;
            inner.command(&cdb, &mut cap16)?;
            last = u64::from_be_bytes(cap16[..8].try_into().unwrap());
            block_size = u64::from(u32::from_be_bytes(cap16[8..12].try_into().unwrap()));
        }
        if !(512..=65536).contains(&block_size) || !block_size.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported block size {block_size}"),
            ));
        }
        Ok(Self {
            inner: Mutex::new(inner),
            block_size,
            blocks: last + 1,
            name,
        })
    }

    /// Reads whole blocks starting at block `lba`.
    fn read_blocks(&self, lba: u64, buf: &mut [u8]) -> io::Result<usize> {
        let count = (buf.len() as u64 / self.block_size) as u32;
        let cdb: Vec<u8> = if lba + u64::from(count) <= 0xFFFF_FFFF && count <= 0xFFFF {
            let mut c = vec![0x28, 0];
            c.extend_from_slice(&(lba as u32).to_be_bytes());
            c.push(0);
            c.extend_from_slice(&(count as u16).to_be_bytes());
            c.push(0);
            c
        } else {
            let mut c = vec![0x88, 0];
            c.extend_from_slice(&lba.to_be_bytes());
            c.extend_from_slice(&count.to_be_bytes());
            c.extend_from_slice(&[0, 0]);
            c
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.command(&cdb, buf)
    }
}

impl<T: BulkTransport> Source for MassStorage<T> {
    fn len(&self) -> u64 {
        self.blocks * self.block_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.len();
        if offset >= len || buf.is_empty() {
            return Ok(0);
        }
        let bs = self.block_size;
        let want = (buf.len() as u64).min(len - offset).min(READ_CHUNK);
        let first = offset / bs;
        let end = (offset + want).div_ceil(bs);
        let skip = (offset - first * bs) as usize;
        if skip == 0 && want % bs == 0 {
            // Aligned: read straight into the caller's buffer.
            let n = self.read_blocks(first, &mut buf[..want as usize])?;
            return Ok(n);
        }
        let mut tmp = vec![0u8; ((end - first) * bs) as usize];
        let n = self.read_blocks(first, &mut tmp)?;
        if n <= skip {
            return Ok(0);
        }
        let copy = (n - skip).min(want as usize);
        buf[..copy].copy_from_slice(&tmp[skip..skip + copy]);
        Ok(copy)
    }
}

/// The USB connection on Linux and Android, through the kernel's usbfs.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod usbfs {
    use std::io;
    use std::mem::size_of;
    use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

    use super::BulkTransport;

    #[repr(C)]
    struct BulkTransfer {
        ep: libc::c_uint,
        len: libc::c_uint,
        timeout: libc::c_uint,
        data: *mut libc::c_void,
    }

    #[repr(C)]
    struct ControlTransfer {
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: u32,
        data: *mut libc::c_void,
    }

    /// Linux `_IOC(dir, 'U', nr, size)`.
    const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
        (dir << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | nr
    }

    const READ_WRITE: u64 = 3;
    const READ: u64 = 2;
    const USBDEVFS_CONTROL: u64 = ioc(READ_WRITE, 0, size_of::<ControlTransfer>());
    const USBDEVFS_BULK: u64 = ioc(READ_WRITE, 2, size_of::<BulkTransfer>());
    const USBDEVFS_CLEAR_HALT: u64 = ioc(READ, 21, size_of::<libc::c_uint>());
    const TIMEOUT_MS: u32 = 20_000;

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn ioctl_numbers_match_linux_headers() {
        // Values of USBDEVFS_CONTROL, USBDEVFS_BULK and USBDEVFS_CLEAR_HALT
        // from <linux/usbdevice_fs.h> on 64-bit systems.
        assert_eq!(USBDEVFS_CONTROL, 0xC018_5500);
        assert_eq!(USBDEVFS_BULK, 0xC018_5502);
        assert_eq!(USBDEVFS_CLEAR_HALT, 0x8004_5515);
    }

    /// A claimed mass storage interface of an open USB device.
    pub struct Usbfs {
        fd: OwnedFd,
        interface: u16,
        endpoint_in: u8,
        endpoint_out: u8,
    }

    impl Usbfs {
        /// Uses a copy of `fd`, an open usbfs device (on Android:
        /// `UsbDeviceConnection.getFileDescriptor()`) whose interface
        /// `interface` is already claimed.
        ///
        /// # Safety
        /// `fd` must be an open file descriptor for the duration of the call.
        pub unsafe fn from_raw_fd(
            fd: RawFd,
            interface: u16,
            endpoint_in: u8,
            endpoint_out: u8,
        ) -> io::Result<Self> {
            let fd = BorrowedFd::borrow_raw(fd).try_clone_to_owned()?;
            Ok(Self {
                fd,
                interface,
                endpoint_in,
                endpoint_out,
            })
        }

        fn bulk(&self, endpoint: u8, data: *mut u8, len: usize) -> io::Result<usize> {
            let mut t = BulkTransfer {
                ep: endpoint.into(),
                len: len as libc::c_uint,
                timeout: TIMEOUT_MS,
                data: data.cast(),
            };
            // SAFETY: `t` points at a live buffer of `len` bytes.
            let n = unsafe { libc::ioctl(self.fd.as_raw_fd(), USBDEVFS_BULK as _, &mut t) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(n as usize)
        }
    }

    impl BulkTransport for Usbfs {
        fn bulk_out(&mut self, data: &[u8]) -> io::Result<()> {
            let mut done = 0;
            while done < data.len() {
                let rest = &data[done..];
                // usbfs only reads from the buffer for OUT transfers.
                let n = self.bulk(self.endpoint_out, rest.as_ptr().cast_mut(), rest.len())?;
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::WriteZero));
                }
                done += n;
            }
            Ok(())
        }

        fn bulk_in(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.bulk(self.endpoint_in, buf.as_mut_ptr(), buf.len())
        }

        fn clear_halt(&mut self, in_endpoint: bool) -> io::Result<()> {
            let mut ep: libc::c_uint = if in_endpoint {
                self.endpoint_in
            } else {
                self.endpoint_out
            }
            .into();
            // SAFETY: the kernel reads one unsigned int.
            let r = unsafe { libc::ioctl(self.fd.as_raw_fd(), USBDEVFS_CLEAR_HALT as _, &mut ep) };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn reset(&mut self) -> io::Result<()> {
            let mut c = ControlTransfer {
                request_type: 0x21, // class request to the interface
                request: 0xFF,      // Bulk-Only Mass Storage Reset
                value: 0,
                index: self.interface,
                length: 0,
                timeout: TIMEOUT_MS,
                data: std::ptr::null_mut(),
            };
            // SAFETY: no data stage.
            let r = unsafe { libc::ioctl(self.fd.as_raw_fd(), USBDEVFS_CONTROL as _, &mut c) };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

/// A simulated USB card reader, for tests: answers Bulk-Only Transport
/// commands from a disk image in memory.
#[cfg(test)]
pub(crate) mod simulated {
    use std::collections::VecDeque;
    use std::io;
    use std::ops::Range;

    use super::BulkTransport;

    pub struct CardReader {
        pub disk: Vec<u8>,
        pub block_size: usize,
        /// TEST UNIT READY fails this many times first (card settling).
        pub not_ready: u32,
        /// Reading these blocks fails like a bad sector.
        pub bad_blocks: Range<u64>,
        /// Data and status waiting to be read from the IN endpoint.
        pending: VecDeque<Vec<u8>>,
        stalled_in: bool,
        sense: [u8; 3],
        pub commands: Vec<u8>,
    }

    impl CardReader {
        pub fn new(disk: Vec<u8>, block_size: usize) -> Self {
            Self {
                disk,
                block_size,
                not_ready: 0,
                bad_blocks: 0..0,
                pending: VecDeque::new(),
                stalled_in: false,
                sense: [0; 3],
                commands: Vec::new(),
            }
        }

        fn status(&mut self, tag: &[u8], residue: u32, status: u8) {
            let mut csw = b"USBS".to_vec();
            csw.extend_from_slice(tag);
            csw.extend_from_slice(&residue.to_le_bytes());
            csw.push(status);
            self.pending.push_back(csw);
        }

        fn fail(&mut self, tag: &[u8], len: u32, sense: [u8; 3]) {
            self.sense = sense;
            if len > 0 {
                // A device with no data for a read stalls the IN endpoint.
                self.stalled_in = true;
            }
            self.status(tag, len, 1);
        }

        fn execute(&mut self, cbw: &[u8]) {
            assert_eq!(&cbw[..4], b"USBC");
            let tag = cbw[4..8].to_vec();
            let len = u32::from_le_bytes(cbw[8..12].try_into().unwrap());
            let cdb = &cbw[15..15 + usize::from(cbw[14])];
            self.commands.push(cdb[0]);
            let blocks = (self.disk.len() / self.block_size) as u64;
            let reply = match cdb[0] {
                0x00 if self.not_ready > 0 => {
                    self.not_ready -= 1;
                    return self.fail(&tag, len, [0x02, 0x04, 0x01]);
                }
                0x00 => Vec::new(),
                0x03 => {
                    let mut s = vec![0u8; 18];
                    s[0] = 0x70;
                    s[2] = self.sense[0];
                    s[7] = 10;
                    s[12] = self.sense[1];
                    s[13] = self.sense[2];
                    s
                }
                0x12 => {
                    let mut d = vec![0u8; 36];
                    d[8..16].copy_from_slice(b"Generic ");
                    d[16..32].copy_from_slice(b"Card Reader     ");
                    d
                }
                0x25 => {
                    let last = (blocks - 1).min(0xFFFF_FFFF) as u32;
                    let mut d = last.to_be_bytes().to_vec();
                    d.extend_from_slice(&(self.block_size as u32).to_be_bytes());
                    d
                }
                0x28 | 0x88 => {
                    let (lba, count) = if cdb[0] == 0x28 {
                        (
                            u64::from(u32::from_be_bytes(cdb[2..6].try_into().unwrap())),
                            u64::from(u16::from_be_bytes(cdb[7..9].try_into().unwrap())),
                        )
                    } else {
                        (
                            u64::from_be_bytes(cdb[2..10].try_into().unwrap()),
                            u64::from(u32::from_be_bytes(cdb[10..14].try_into().unwrap())),
                        )
                    };
                    let bad = lba < self.bad_blocks.end && lba + count > self.bad_blocks.start;
                    if lba + count > blocks || bad {
                        return self.fail(&tag, len, [0x03, 0x11, 0x00]);
                    }
                    let bs = self.block_size as u64;
                    self.disk[(lba * bs) as usize..((lba + count) * bs) as usize].to_vec()
                }
                _ => return self.fail(&tag, len, [0x05, 0x20, 0x00]),
            };
            self.sense = [0; 3];
            let sent = reply.len().min(len as usize);
            if sent > 0 {
                self.pending.push_back(reply[..sent].to_vec());
            }
            self.status(&tag, len - sent as u32, 0);
        }
    }

    impl BulkTransport for CardReader {
        fn bulk_out(&mut self, data: &[u8]) -> io::Result<()> {
            assert_eq!(data.len(), 31, "only commands are sent");
            self.execute(data);
            Ok(())
        }

        fn bulk_in(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.stalled_in {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            let Some(front) = self.pending.front_mut() else {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            };
            let n = buf.len().min(front.len());
            buf[..n].copy_from_slice(&front[..n]);
            front.drain(..n);
            if front.is_empty() {
                self.pending.pop_front();
            }
            Ok(n)
        }

        fn clear_halt(&mut self, in_endpoint: bool) -> io::Result<()> {
            if in_endpoint {
                self.stalled_in = false;
            }
            Ok(())
        }

        fn reset(&mut self) -> io::Result<()> {
            self.pending.clear();
            self.stalled_in = false;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::simulated::CardReader;
    use super::*;
    use crate::source::read_fully;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn reads_like_a_file() {
        let disk = pattern(3 << 20);
        let mut reader = CardReader::new(disk.clone(), 512);
        reader.not_ready = 3;
        let dev = MassStorage::open(reader).unwrap();
        assert_eq!(dev.name, "Generic Card Reader");
        assert_eq!(dev.len(), disk.len() as u64);
        // Unaligned, across chunk and transfer boundaries.
        for (offset, len) in [
            (0, 512),
            (100, 1000),
            (1 << 20, 700_000),
            ((3 << 20) - 10, 10),
        ] {
            let mut buf = vec![0u8; len];
            let n = read_fully(&dev, offset as u64, &mut buf).unwrap();
            assert_eq!(&buf[..n], &disk[offset..offset + n], "at {offset}");
            assert_eq!(n, len.min(disk.len() - offset));
        }
    }

    #[test]
    fn bad_sectors_fail_only_their_reads() {
        let disk = pattern(1 << 20);
        let mut reader = CardReader::new(disk.clone(), 512);
        reader.bad_blocks = 100..102;
        let dev = MassStorage::open(reader).unwrap();
        let mut buf = vec![0u8; 512];
        let err = dev.read_at(100 * 512, &mut buf).unwrap_err();
        assert!(err.to_string().contains("unreadable sector"), "{err}");
        // The device keeps working after the error.
        assert_eq!(dev.read_at(200 * 512, &mut buf).unwrap(), 512);
        assert_eq!(buf, disk[200 * 512..201 * 512]);
    }

    #[test]
    fn deleted_files_through_a_card_reader() {
        let gz = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/images/fat32.img.gz");
        let mut disk = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(gz).unwrap())
            .read_to_end(&mut disk)
            .unwrap();
        let dev = MassStorage::open(CardReader::new(disk.clone(), 512)).unwrap();
        let from_usb = crate::fs::open(&dev, 0).unwrap().unwrap();
        let from_image = crate::fs::open(&disk, 0).unwrap().unwrap();
        assert_eq!(from_usb.entries.len(), from_image.entries.len());
        for (a, b) in from_usb.entries.iter().zip(&from_image.entries) {
            assert_eq!(a.path, b.path);
            let (mut x, mut y) = (Vec::new(), Vec::new());
            crate::fs::extract(&dev, a, &mut x).unwrap();
            crate::fs::extract(&disk, b, &mut y).unwrap();
            assert!(x == y, "{} differs", a.path);
        }
        assert!(from_usb.entries.iter().any(|e| e.deleted));
    }

    /// A card reader that misbehaves at random: corrupted, short or missing
    /// replies, stalls and errors.
    struct Flaky {
        reader: CardReader,
        seed: u64,
    }

    impl Flaky {
        fn roll(&mut self, one_in: u64) -> bool {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            self.seed % one_in == 0
        }
    }

    impl BulkTransport for Flaky {
        fn bulk_out(&mut self, data: &[u8]) -> io::Result<()> {
            if self.roll(30) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            self.reader.bulk_out(data)
        }

        fn bulk_in(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.roll(40) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            if self.roll(40) {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            let mut n = self.reader.bulk_in(buf)?;
            if self.roll(25) && n > 0 {
                n = (self.seed as usize) % n;
            }
            if self.roll(20) && n > 0 {
                let at = (self.seed as usize) % n;
                buf[at] ^= 0xA5;
            }
            Ok(n)
        }

        fn clear_halt(&mut self, in_endpoint: bool) -> io::Result<()> {
            self.reader.clear_halt(in_endpoint)
        }

        fn reset(&mut self) -> io::Result<()> {
            self.reader.reset()
        }
    }

    #[test]
    fn misbehaving_devices_never_crash() {
        let disk = pattern(1 << 20);
        let mut opened = 0;
        for seed in 1..400u64 {
            let flaky = Flaky {
                reader: CardReader::new(disk.clone(), 512),
                seed: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            };
            let Ok(dev) = MassStorage::open(flaky) else {
                continue;
            };
            opened += 1;
            let mut buf = vec![0u8; 70_000];
            for offset in [0u64, 12_345, 500_000, 1_000_000] {
                // Errors are fine; wrong data is caught by the file
                // systems' and carvers' checks; crashes and hangs are not.
                let _ = read_fully(&dev, offset, &mut buf);
            }
        }
        assert!(opened > 0);
    }

    #[test]
    fn large_blocks_and_missing_card() {
        let disk = pattern(1 << 20);
        let dev = MassStorage::open(CardReader::new(disk.clone(), 4096)).unwrap();
        let mut buf = vec![0u8; 100];
        assert_eq!(read_fully(&dev, 5000, &mut buf).unwrap(), 100);
        assert_eq!(buf, disk[5000..5100]);

        let mut empty = CardReader::new(disk, 512);
        empty.not_ready = u32::MAX;
        let err = MassStorage::open(empty).err().unwrap();
        assert!(err.to_string().contains("not ready"), "{err}");
    }
}
