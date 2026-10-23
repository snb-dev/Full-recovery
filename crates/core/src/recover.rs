//! Copies found files out of the source.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::scan::FoundFile;
use crate::source::{read_fully, Source};

/// Where [`recover_file`] saves `file` inside `dir`: `<dir>/<category>/f<id>_<offset>.<ext>`.
pub fn output_path(dir: &Path, file: &FoundFile) -> PathBuf {
    dir.join(file.category.name()).join(format!(
        "f{:06}_{:x}.{}",
        file.id, file.offset, file.extension
    ))
}

/// Writes the bytes of `file` to `out`.
pub fn extract(src: &dyn Source, file: &FoundFile, out: &mut dyn Write) -> io::Result<()> {
    let mut buf = vec![0u8; (1 << 20).min(file.len) as usize];
    let mut done = 0;
    while done < file.len {
        let n = buf.len().min((file.len - done) as usize);
        let got = read_fully(src, file.offset + done, &mut buf[..n])?;
        if got < n {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file extends past the end of the source",
            ));
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// Saves `file` under `dir` (see [`output_path`]) and returns its path.
pub fn recover_file(src: &dyn Source, file: &FoundFile, dir: &Path) -> io::Result<PathBuf> {
    let path = output_path(dir, file);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = BufWriter::new(File::create(&path)?);
    extract(src, file, &mut out)?;
    out.flush()?;
    Ok(path)
}
