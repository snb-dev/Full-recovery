//! Copies found files out of the source.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::fs::{self as rfs, FileData, FsEntry};
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

/// Saves a file found in a file system under `dir`, keeping its folders,
/// name and modification time. Returns where it was saved.
pub fn save_fs_entry(src: &dyn Source, entry: &FsEntry, dir: &Path) -> io::Result<PathBuf> {
    if let FileData::Unsupported(why) = entry.data {
        return Err(io::Error::new(io::ErrorKind::Unsupported, why));
    }
    let mut path = dir.to_path_buf();
    for part in entry.path.split('/').filter(|p| !p.is_empty()) {
        path.push(safe_name(part));
    }
    let path = unique_path(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut w = BufWriter::new(File::create(&path)?);
    rfs::extract(src, entry, &mut w)?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    if let Some(t) = entry.modified.and_then(|t| u64::try_from(t).ok()) {
        // Keeping the original date is a nicety; ignore failures.
        let _ = file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(t));
    }
    Ok(path)
}

/// Makes a file name safe to create on Windows, macOS and Linux.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']);
    if cleaned.is_empty() {
        return "_".into();
    }
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{cleaned}")
    } else {
        cleaned.to_owned()
    }
}

/// Adds " (1)", " (2)", ... before the extension if `path` already exists.
pub fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|i| path.with_file_name(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("unbounded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("photo.jpg"), "photo.jpg");
        assert_eq!(safe_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_name("con.txt"), "_con.txt");
        assert_eq!(safe_name("COM1"), "_COM1");
        assert_eq!(safe_name("..."), "_");
        assert_eq!(safe_name("ünïcødé файл.txt"), "ünïcødé файл.txt");
    }
}
