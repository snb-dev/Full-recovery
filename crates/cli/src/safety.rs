//! Guards against the most common way to destroy recoverable data: writing
//! recovered files onto the drive that is being recovered.

use std::path::Path;

use anyhow::{bail, Result};
use fullrecovery_core::devices;

/// Fails if `out` is on the same physical disk as `source`.
pub fn check_output_location(source: &Path, out: &Path) -> Result<()> {
    if devices::same_disk(source, out)? {
        bail!(
            "{} is on the drive being recovered ({}). Writing there would overwrite the \
             files you want back. Choose a folder on a different drive.",
            out.display(),
            source.display()
        );
    }
    if !cfg!(target_os = "linux") {
        eprintln!(
            "Reminder: save recovered files to a different drive than the one being recovered."
        );
    }
    Ok(())
}
