//! Finds lost media among the *files* of a folder tree, for when the drive
//! itself can't be read raw (phones without root, network or cloud folders):
//!
//! * deleted files waiting in a trash (Android keeps them for 30 days as
//!   `.trashed-<expiry>-<name>`);
//! * thumbnails, which often outlive the photo they were made from;
//! * photos and videos in hidden folders that galleries don't show;
//! * photos and videos stored inside other files, such as thumbnail
//!   databases (`.thumbdata`) and app caches, found with the deep scan.

use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::carve::{Category, Confidence};
use crate::scan::{scan, FoundFile, ScanObserver, ScanOptions};
use crate::source::FileSource;

/// Folders that can't be read or never hold user media.
const SKIPPED: &[&str] = &["Android/data", "Android/obb", "proc", "sys", "dev"];
/// Largest file searched for embedded media.
const MAX_CONTAINER: u64 = 512 << 20;
const MAX_DEPTH: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FindKind {
    /// Deleted, waiting in a trash folder.
    Trashed,
    /// A thumbnail: a small copy of a photo or video.
    Thumbnail,
    /// Media in a hidden folder.
    Hidden,
    /// Media found inside another file.
    Embedded,
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderFind {
    /// File holding the data.
    pub path: PathBuf,
    /// Name to recover the file as.
    pub name: String,
    /// Folder relative to the scanned root, like `/DCIM/Camera`.
    pub folder: String,
    pub kind: FindKind,
    pub size: u64,
    pub modified: Option<i64>,
    pub category: Category,
    pub confidence: Confidence,
    /// Where the file is inside `path`, for embedded media.
    #[serde(skip)]
    pub embedded: Option<FoundFile>,
}

/// Receives finds and progress. Returning `Break` stops the scan.
pub trait FolderObserver {
    fn found(&mut self, find: FolderFind) -> ControlFlow<()>;

    /// Called for every folder entered, with the number of files seen so far.
    fn progress(&mut self, _folder: &Path, _files_seen: u64) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
}

/// Searches `root` and everything below it.
pub fn scan_folder(root: &Path, obs: &mut dyn FolderObserver) -> io::Result<()> {
    let mut walker = Walker { root, obs, seen: 0 };
    // Only the root must be readable; unreadable subfolders are skipped.
    fs::read_dir(root)?;
    let _ = walker.walk(root, 0);
    Ok(())
}

struct Walker<'a> {
    root: &'a Path,
    obs: &'a mut dyn FolderObserver,
    seen: u64,
}

fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn is_media(category: Category) -> bool {
    matches!(
        category,
        Category::Photo | Category::Video | Category::Audio
    )
}

/// `.trashed-1735689600-IMG_1.jpg` → `IMG_1.jpg`.
fn trashed_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(".trashed-")?;
    let (expiry, original) = rest.split_once('-')?;
    (!expiry.is_empty() && expiry.bytes().all(|b| b.is_ascii_digit()) && !original.is_empty())
        .then_some(original)
}

impl Walker<'_> {
    fn relative(&self, dir: &Path) -> String {
        let rel = dir.strip_prefix(self.root).unwrap_or(dir);
        let mut out = String::new();
        for part in rel.components() {
            out.push('/');
            out.push_str(&part.as_os_str().to_string_lossy());
        }
        if out.is_empty() {
            out.push('/');
        }
        out
    }

    fn walk(&mut self, dir: &Path, depth: usize) -> ControlFlow<()> {
        let folder = self.relative(dir);
        if depth > MAX_DEPTH || SKIPPED.iter().any(|s| folder.trim_start_matches('/') == *s) {
            return ControlFlow::Continue(());
        }
        self.obs.progress(dir, self.seen)?;
        let Ok(entries) = fs::read_dir(dir) else {
            return ControlFlow::Continue(());
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            // Symbolic links are not followed: they can loop or leave the tree.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                self.walk(&path, depth + 1)?;
            } else if kind.is_file() {
                self.seen += 1;
                self.file(&path, &folder)?;
            }
        }
        ControlFlow::Continue(())
    }

    fn file(&mut self, path: &Path, folder: &str) -> ControlFlow<()> {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let Ok(meta) = fs::metadata(path) else {
            return ControlFlow::Continue(());
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        let lower_folder = folder.to_ascii_lowercase();
        let hidden = folder.split('/').any(|p| p.starts_with('.'));
        let thumbs = lower_folder.contains("thumb");
        let cache = lower_folder
            .split('/')
            .any(|p| matches!(p, "cache" | ".cache" | "caches"));

        let find = |name: &str, kind, confidence| {
            let category = Category::from_extension(&extension(name));
            FolderFind {
                path: path.to_path_buf(),
                name: name.to_owned(),
                folder: folder.to_owned(),
                kind,
                size: meta.len(),
                modified,
                category,
                confidence,
                embedded: None,
            }
        };

        if let Some(original) = trashed_name(&name) {
            return self
                .obs
                .found(find(original, FindKind::Trashed, Confidence::High));
        }
        let category = Category::from_extension(&extension(&name));
        if is_media(category) {
            if thumbs {
                return self
                    .obs
                    .found(find(&name, FindKind::Thumbnail, Confidence::High));
            }
            if hidden || name.starts_with('.') {
                return self
                    .obs
                    .found(find(&name, FindKind::Hidden, Confidence::High));
            }
            // Visible media is not lost.
            return ControlFlow::Continue(());
        }
        let container =
            name.starts_with(".thumbdata") || thumbs || cache || extension(&name) == "db";
        if container && meta.len() >= 1024 && meta.len() <= MAX_CONTAINER {
            return self.embedded(path, &name, folder, modified);
        }
        ControlFlow::Continue(())
    }

    /// Searches inside a file for photos and videos.
    fn embedded(
        &mut self,
        path: &Path,
        name: &str,
        folder: &str,
        modified: Option<i64>,
    ) -> ControlFlow<()> {
        let Ok(src) = FileSource::open(path) else {
            return ControlFlow::Continue(());
        };
        let mut finds = Vec::new();
        let mut collect = Collect(&mut finds);
        let opts = ScanOptions {
            // Media inside other files is not aligned to sectors.
            alignment: 1,
            types: Some(
                ["jpg", "png", "gif", "webp", "bmp", "mp4", "heic"]
                    .map(String::from)
                    .to_vec(),
            ),
            ..ScanOptions::default()
        };
        if scan(&src, &opts, &mut collect).is_err() {
            return ControlFlow::Continue(());
        }
        let stem = name.trim_start_matches('.');
        for (n, f) in finds.into_iter().enumerate() {
            // A file that is just one picture was found by name already.
            if f.offset == 0 && f.len == meta_len(path) {
                continue;
            }
            self.obs.found(FolderFind {
                path: path.to_path_buf(),
                name: format!("{stem}_{:03}.{}", n + 1, f.extension),
                folder: format!("{}/{name}", folder.trim_end_matches('/')),
                kind: FindKind::Embedded,
                size: f.len,
                modified,
                category: f.category,
                confidence: f.confidence,
                embedded: Some(f),
            })?;
        }
        ControlFlow::Continue(())
    }
}

fn meta_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

struct Collect<'a>(&'a mut Vec<FoundFile>);

impl ScanObserver for Collect<'_> {
    fn found(&mut self, file: &FoundFile) -> ControlFlow<()> {
        self.0.push(file.clone());
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    struct All(Vec<FolderFind>);

    impl FolderObserver for All {
        fn found(&mut self, find: FolderFind) -> ControlFlow<()> {
            self.0.push(find);
            ControlFlow::Continue(())
        }
    }

    fn write(root: &Path, rel: &str, data: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    #[test]
    fn finds_trash_thumbnails_hidden_and_embedded_media() {
        let root = std::env::temp_dir().join(format!("fullrecovery-folder-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (jpeg, png) = (fixtures::jpeg(), fixtures::png());
        write(&root, "DCIM/Camera/visible.jpg", &jpeg);
        write(
            &root,
            "DCIM/Camera/.trashed-1735689600-IMG_20240101_1200.jpg",
            &jpeg,
        );
        write(&root, "DCIM/.thumbnails/1234.jpg", &jpeg);
        write(&root, "Pictures/.private/secret.png", &png);
        write(&root, "Android/data/com.app/files/x.jpg", &jpeg);
        write(&root, "Documents/notes.txt", b"hello");
        // A thumbnail database: pictures one after another, not aligned.
        let mut thumbdata = vec![7u8; 1000];
        thumbdata.extend_from_slice(&jpeg);
        thumbdata.extend_from_slice(&[0u8; 333]);
        thumbdata.extend_from_slice(&png);
        thumbdata.extend_from_slice(&[9u8; 500]);
        write(
            &root,
            "DCIM/.thumbnails/.thumbdata4--1967290299",
            &thumbdata,
        );

        let mut all = All(Vec::new());
        scan_folder(&root, &mut all).unwrap();
        let mut got: Vec<_> = all
            .0
            .iter()
            .map(|f| (f.kind, f.folder.as_str(), f.name.as_str(), f.size))
            .collect();
        got.sort_by(|a, b| (a.1, a.2).cmp(&(b.1, b.2)));
        let (j, p) = (jpeg.len() as u64, png.len() as u64);
        assert_eq!(
            got,
            [
                (FindKind::Thumbnail, "/DCIM/.thumbnails", "1234.jpg", j),
                (
                    FindKind::Embedded,
                    "/DCIM/.thumbnails/.thumbdata4--1967290299",
                    "thumbdata4--1967290299_001.jpg",
                    j
                ),
                (
                    FindKind::Embedded,
                    "/DCIM/.thumbnails/.thumbdata4--1967290299",
                    "thumbdata4--1967290299_002.png",
                    p
                ),
                (
                    FindKind::Trashed,
                    "/DCIM/Camera",
                    "IMG_20240101_1200.jpg",
                    j
                ),
                (FindKind::Hidden, "/Pictures/.private", "secret.png", p),
            ]
        );
        let embedded = all.0.iter().find(|f| f.name.ends_with("_001.jpg")).unwrap();
        assert_eq!(embedded.embedded.as_ref().unwrap().offset, 1000);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn trashed_names() {
        assert_eq!(
            trashed_name(".trashed-1735689600-IMG_1.jpg"),
            Some("IMG_1.jpg")
        );
        assert_eq!(trashed_name(".trashed-abc-IMG_1.jpg"), None);
        assert_eq!(trashed_name(".trashed-1735689600-"), None);
        assert_eq!(trashed_name("IMG_1.jpg"), None);
    }
}
