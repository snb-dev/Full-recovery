//! Core engine of Full Recovery.
//!
//! The engine never writes to the device it reads from. It works on any
//! [`Source`] (a disk image, a partition or a raw device) and offers:
//!
//! * [`devices`]: lists the drives of the computer;
//! * [`folder`]: finds lost media among the files of a folder (phones);
//! * [`fs`]: reads FAT, exFAT and NTFS file systems, including deleted files
//!   with their original names and folders;
//! * [`partition`]: reads MBR/GPT partition tables and searches for lost
//!   partitions;
//! * [`scan`]: signature-based deep scan ("file carving") that finds files by
//!   their content, even after formatting or when the file system is gone;
//! * [`recover`]: copying found files out to a safe location;
//! * [`image`]: making a byte-by-byte copy of a failing drive, skipping
//!   unreadable sectors, so recovery can work on the copy instead.

pub mod carve;
pub mod devices;
pub mod folder;
pub mod fs;
pub mod image;
pub mod partition;
pub mod reader;
pub mod recover;
pub mod scan;
pub mod source;

#[cfg(test)]
mod fixtures;

pub use carve::{Carved, Carver, Category, Confidence};
pub use scan::{scan, FoundFile, ScanObserver, ScanOptions, ScanSummary};
pub use source::{FileSource, Source};
