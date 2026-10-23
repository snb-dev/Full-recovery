//! Guards against the most common way to destroy recoverable data: writing
//! recovered files onto the drive that is being recovered.

use std::path::Path;

use anyhow::Result;

/// Fails if `out` is on the same physical disk as the device `source`.
#[cfg(target_os = "linux")]
pub fn check_output_location(source: &Path, out: &Path) -> Result<()> {
    use std::fs;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let meta = fs::metadata(source)?;
    if !meta.file_type().is_block_device() {
        // Disk images are copies; writing next to them is safe.
        return Ok(());
    }
    let Some(existing) = out.ancestors().find(|p| p.exists()) else {
        return Ok(());
    };
    let out_dev = fs::metadata(existing)?.dev();
    if disk_of(meta.rdev()) == disk_of(out_dev) {
        anyhow::bail!(
            "{} is on the drive being recovered ({}). Writing there would overwrite the \
             files you want back. Choose a folder on a different drive.",
            out.display(),
            source.display()
        );
    }
    Ok(())
}

/// Name of the whole disk holding block device `dev` (e.g. `sda` for `sda1`).
#[cfg(target_os = "linux")]
fn disk_of(dev: u64) -> String {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    let id = format!("{major}:{minor}");
    let Ok(path) = std::fs::canonicalize(format!("/sys/dev/block/{id}")) else {
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

#[cfg(not(target_os = "linux"))]
pub fn check_output_location(_source: &Path, _out: &Path) -> Result<()> {
    eprintln!("Reminder: save recovered files to a different drive than the one being recovered.");
    Ok(())
}
