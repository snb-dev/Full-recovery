//! What the app is working on: the opened drive, the files found on it, and
//! the background jobs (scans, recovery, imaging). Independent of the window
//! toolkit so it can be tested on its own.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use fullrecovery_core::folder::{self, FindKind, FolderFind, FolderObserver};
use fullrecovery_core::fs::{self as rfs, FileData, FsEntry};
use fullrecovery_core::partition::{self, Partition};
use fullrecovery_core::{
    devices, image, recover, scan, Category, Confidence, FileSource, FoundFile, ScanObserver,
    ScanOptions, Source,
};
use serde::Serialize;
use serde_json::{json, Value};

/// Sends an event to the window: name and JSON payload.
pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// How often progress events are sent at most.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// Largest file the preview will load.
pub const MAX_PREVIEW: u64 = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// Found in a file system: original name and folder are known.
    Files,
    /// Found by the deep scan: only the content is known.
    Deep,
    /// Found among the files of a folder (phones): trash, thumbnails,
    /// hidden folders and media inside other files.
    Folder,
}

/// A recoverable file, as shown in the result list.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: u32,
    pub name: String,
    /// Folder shown in the folder tree, e.g. `/Photos/2024`.
    pub folder: String,
    pub ext: String,
    pub size: u64,
    pub modified: Option<i64>,
    pub deleted: bool,
    pub category: Category,
    pub confidence: Confidence,
    pub method: Method,
    /// Why the file can't be recovered, if it can't.
    pub problem: Option<&'static str>,
    /// The deep scan found this file's content intact where the file system
    /// says it is.
    pub verified: bool,
    /// For folder scans: where the file was found.
    pub found_as: Option<FindKind>,
}

#[derive(Clone)]
enum Origin {
    Fs {
        volume: usize,
        entry: FsEntry,
    },
    Carved(FoundFile),
    /// A whole file of a scanned folder, recovered under `name`.
    File(PathBuf),
    /// Part of a file of a scanned folder.
    Embedded {
        path: PathBuf,
        found: FoundFile,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub path: String,
    pub size: u64,
    pub partitions: Vec<Partition>,
    /// The source is a folder read file by file, not a drive.
    pub folder: bool,
}

#[derive(Default)]
pub struct Session {
    source: Option<(PathBuf, SharedSource)>,
    /// Set instead of `source` when a folder is scanned.
    folder: Option<PathBuf>,
    items: Vec<Item>,
    origins: Vec<Origin>,
    /// Folder names of the volumes read by the quick scan.
    volumes: Vec<String>,
    cancel: Arc<AtomicBool>,
    busy: bool,
}

pub type Shared = Arc<Mutex<Session>>;

fn lock(shared: &Shared) -> MutexGuard<'_, Session> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Turns an I/O error into a message for people.
pub fn describe(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => {
            "Full Recovery needs administrator rights to read this drive. Close it and start it \
             again as administrator (on Linux and macOS: with sudo)."
                .into()
        }
        io::ErrorKind::NotFound => "The drive or file was not found. Was it unplugged?".into(),
        _ => e.to_string(),
    }
}

/// Opens a drive or disk image and reads its partition table. Forgets any
/// previous results.
pub fn open(shared: &Shared, path: &str) -> Result<SourceInfo, String> {
    if lock(shared).busy {
        return Err("Please wait for the current task to finish.".into());
    }
    let src = FileSource::open(Path::new(path)).map_err(|e| describe(&e))?;
    open_source(shared, path, Arc::new(src))
}

/// Opens a USB stick or card reader that Android handed to the app: `fd` is
/// the open USB device with `interface` claimed (see MainActivity.kt).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn open_usb(shared: &Shared, usb: UsbDevice) -> Result<SourceInfo, String> {
    use fullrecovery_core::usb::{usbfs::Usbfs, MassStorage};

    if lock(shared).busy {
        return Err("Please wait for the current task to finish.".into());
    }
    // SAFETY: the app's Android side keeps the connection, and so `fd`, open.
    let transport =
        unsafe { Usbfs::from_raw_fd(usb.fd, usb.interface, usb.endpoint_in, usb.endpoint_out) }
            .map_err(|e| describe(&e))?;
    let device = MassStorage::open(transport).map_err(|e| {
        format!("Could not read the USB drive: {e}. Unplug it, plug it in again and retry.")
    })?;
    let path = format!("usb:{}", usb.name);
    open_source(shared, &path, Arc::new(device))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn open_usb(_shared: &Shared, _usb: UsbDevice) -> Result<SourceInfo, String> {
    Err("Reading USB devices directly is only needed and supported on Android.".into())
}

/// A USB mass storage device opened by the Android side of the app.
#[derive(Debug, serde::Deserialize)]
pub struct UsbDevice {
    pub name: String,
    pub fd: i32,
    pub interface: u16,
    pub endpoint_in: u8,
    pub endpoint_out: u8,
}

fn open_source(shared: &Shared, path: &str, src: SharedSource) -> Result<SourceInfo, String> {
    let mut s = lock(shared);
    if s.busy {
        return Err("Please wait for the current task to finish.".into());
    }
    if src.is_empty() {
        return Err("This drive is empty or its size can't be read.".into());
    }
    let partitions = partition::read_table(&*src).map_err(|e| describe(&e))?;
    let info = SourceInfo {
        path: path.into(),
        size: src.len(),
        partitions,
        folder: false,
    };
    *s = Session {
        source: Some((PathBuf::from(path), src)),
        ..Session::default()
    };
    Ok(info)
}

/// Opens a folder (phone storage, an SD card's files) for a folder scan.
pub fn open_folder(shared: &Shared, path: &str) -> Result<SourceInfo, String> {
    let mut s = lock(shared);
    if s.busy {
        return Err("Please wait for the current task to finish.".into());
    }
    std::fs::read_dir(path).map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => {
            "Full Recovery is not allowed to read this storage yet. Please allow access to all \
             files when asked."
                .to_owned()
        }
        _ => describe(&e),
    })?;
    *s = Session {
        folder: Some(PathBuf::from(path)),
        ..Session::default()
    };
    Ok(SourceInfo {
        path: path.into(),
        size: 0,
        partitions: Vec::new(),
        folder: true,
    })
}

/// Marks the session busy and returns what a background job needs: the
/// open drive (`None` for a folder) and the cancel flag.
type SharedSource = Arc<dyn Source>;

fn begin_any(shared: &Shared) -> Result<(Option<SharedSource>, Arc<AtomicBool>), String> {
    let mut s = lock(shared);
    if s.busy {
        return Err("Please wait for the current task to finish.".into());
    }
    if s.source.is_none() && s.folder.is_none() {
        return Err("Choose a drive first.".into());
    }
    let src = s.source.as_ref().map(|(_, src)| Arc::clone(src));
    s.busy = true;
    s.cancel = Arc::new(AtomicBool::new(false));
    Ok((src, Arc::clone(&s.cancel)))
}

/// Like [`begin_any`], for jobs that need a drive.
fn begin(shared: &Shared) -> Result<(SharedSource, Arc<AtomicBool>), String> {
    {
        let s = lock(shared);
        if s.source.is_none() && s.folder.is_some() {
            return Err("This works on drives only, not on folders.".into());
        }
    }
    let (src, cancel) = begin_any(shared)?;
    Ok((src.expect("checked above"), cancel))
}

fn finish(shared: &Shared) {
    lock(shared).busy = false;
}

pub fn cancel(shared: &Shared) {
    lock(shared).cancel.store(true, Ordering::Relaxed);
}

fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some(("", name)) => ("/".into(), name.into()),
        Some((folder, name)) => (folder.into(), name.into()),
        None => ("/".into(), path.into()),
    }
}

fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

struct Throttle(Instant);

impl Throttle {
    fn new() -> Self {
        Self(Instant::now() - PROGRESS_EVERY)
    }

    fn ready(&mut self) -> bool {
        if self.0.elapsed() < PROGRESS_EVERY {
            return false;
        }
        self.0 = Instant::now();
        true
    }
}

/// Quick scan: reads the file systems (all partitions, plus lost ones if
/// `search_lost`) and lists their files, including deleted ones.
pub fn start_quick_scan(shared: &Shared, search_lost: bool, emit: Emit) -> Result<(), String> {
    let (src, cancel) = begin_any(shared)?;
    let folder = {
        let mut s = lock(shared);
        s.items.clear();
        s.origins.clear();
        s.volumes.clear();
        s.folder.clone()
    };
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let result = match (&src, &folder) {
            (Some(src), _) => quick_scan(&shared, &**src, search_lost, &cancel, &emit),
            (None, Some(root)) => folder_scan(&shared, root, &cancel, &emit),
            (None, None) => Ok(()),
        };
        finish(&shared);
        let found = lock(&shared).items.len();
        emit(
            "scan-done",
            json!({
                "found": found,
                "cancelled": cancel.load(Ordering::Relaxed),
                "error": result.err().map(|e| describe(&e)),
            }),
        );
    });
    Ok(())
}

fn quick_scan(
    shared: &Shared,
    src: &dyn Source,
    search_lost: bool,
    cancel: &AtomicBool,
    emit: &Emit,
) -> io::Result<()> {
    let progress = |phase: &str, done: u64, total: u64| {
        let found = lock(shared).items.len();
        emit(
            "scan-progress",
            json!({ "phase": phase, "done": done, "total": total, "found": found }),
        );
    };
    progress("Reading partitions", 0, 1);
    let mut parts = partition::read_table(src)?;
    if search_lost {
        let mut throttle = Throttle::new();
        let lost = partition::search(src, &mut |done, total| {
            if throttle.ready() {
                progress("Searching for lost partitions", done, total);
            }
            if cancel.load(Ordering::Relaxed) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        let first = parts.len() as u32 + 1;
        for (i, mut p) in lost.into_iter().enumerate() {
            if !parts.iter().any(|q| q.offset == p.offset) {
                p.index = first + i as u32;
                parts.push(p);
            }
        }
    }
    let readable: Vec<&Partition> = parts.iter().filter(|p| p.file_system.is_some()).collect();
    let multiple = readable.len() > 1;
    for (n, p) in readable.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let fs_name = p.file_system.map_or("", |f| f.name());
        progress(
            &format!("Reading partition {} ({fs_name})", p.index),
            n as u64,
            readable.len() as u64,
        );
        let Some(volume) = rfs::open_boot(src, p.offset, p.boot_offset)? else {
            continue;
        };
        let label = volume.label.clone().filter(|l| !l.is_empty());
        let volume_name = match (&label, multiple) {
            (_, false) => String::new(),
            (Some(l), true) => format!("Partition {} - {l}", p.index),
            (None, true) => format!("Partition {} - {fs_name}", p.index),
        };
        let mut s = lock(shared);
        let volume_index = s.volumes.len();
        s.volumes.push(volume_name.clone());
        for entry in volume.entries.into_iter().filter(|e| !e.is_dir) {
            let (folder, name) = split_path(&entry.path);
            let folder = if volume_name.is_empty() {
                folder
            } else if folder == "/" {
                format!("/{volume_name}")
            } else {
                format!("/{volume_name}{folder}")
            };
            let ext = extension(&name);
            let problem = match entry.data {
                FileData::Unsupported(why) => Some(why),
                _ => None,
            };
            let item = Item {
                id: s.items.len() as u32,
                category: Category::from_extension(&ext),
                ext,
                name,
                folder,
                size: entry.size,
                modified: entry.modified,
                deleted: entry.deleted,
                confidence: entry.confidence,
                method: Method::Files,
                problem,
                verified: false,
                found_as: None,
            };
            s.items.push(item);
            s.origins.push(Origin::Fs {
                volume: volume_index,
                entry,
            });
        }
    }
    progress("Done", 1, 1);
    Ok(())
}

fn folder_scan(shared: &Shared, root: &Path, cancel: &AtomicBool, emit: &Emit) -> io::Result<()> {
    let mut observer = FolderItems {
        shared,
        cancel,
        emit,
        throttle: Throttle::new(),
    };
    folder::scan_folder(root, &mut observer)
}

struct FolderItems<'a> {
    shared: &'a Shared,
    cancel: &'a AtomicBool,
    emit: &'a Emit,
    throttle: Throttle,
}

impl FolderObserver for FolderItems<'_> {
    fn found(&mut self, f: FolderFind) -> ControlFlow<()> {
        let mut s = lock(self.shared);
        let id = s.items.len() as u32;
        let ext = extension(&f.name);
        s.items.push(Item {
            id,
            name: f.name,
            folder: f.folder,
            ext,
            size: f.size,
            modified: f.modified,
            deleted: f.kind == FindKind::Trashed,
            category: f.category,
            confidence: f.confidence,
            method: Method::Folder,
            problem: None,
            verified: false,
            found_as: Some(f.kind),
        });
        s.origins.push(match f.embedded {
            Some(found) => Origin::Embedded {
                path: f.path,
                found,
            },
            None => Origin::File(f.path),
        });
        ControlFlow::Continue(())
    }

    fn progress(&mut self, folder: &Path, files_seen: u64) -> ControlFlow<()> {
        if self.throttle.ready() {
            let found = lock(self.shared).items.len();
            (self.emit)(
                "scan-progress",
                json!({
                    "phase": format!("Looking in {}", folder.display()),
                    "done": 0,
                    "total": 0,
                    "files_seen": files_seen,
                    "found": found,
                }),
            );
        }
        if self.cancel.load(Ordering::Relaxed) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// Deep scan: finds files by their content anywhere on the drive. Files the
/// quick scan already listed are not added again; instead, their content is
/// checked.
pub fn start_deep_scan(shared: &Shared, emit: Emit) -> Result<(), String> {
    let (src, cancel) = begin(shared)?;
    let known = known_starts(&lock(shared));
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let mut observer = DeepObserver {
            shared: &shared,
            cancel: &cancel,
            emit: &emit,
            throttle: Throttle::new(),
            known,
            added: 0,
        };
        let result = scan(&*src, &ScanOptions::default(), &mut observer);
        finish(&shared);
        let found = lock(&shared).items.len();
        emit(
            "scan-done",
            json!({
                "found": found,
                "cancelled": cancel.load(Ordering::Relaxed),
                "error": result.err().map(|e| describe(&e)),
            }),
        );
    });
    Ok(())
}

/// A file the quick scan found, by where its data starts on the drive.
struct Known {
    id: u32,
    size: u64,
    /// Stored in one piece, so a carved file at the same place is the same file.
    contiguous: bool,
}

fn known_starts(s: &Session) -> HashMap<u64, Known> {
    let mut known = HashMap::new();
    for (id, origin) in s.origins.iter().enumerate() {
        let Origin::Fs { entry, .. } = origin else {
            continue;
        };
        let FileData::Extents(extents) = &entry.data else {
            continue;
        };
        if let Some(start) = extents.first().and_then(|e| e.offset) {
            known.insert(
                start,
                Known {
                    id: id as u32,
                    size: entry.size,
                    contiguous: extents.len() == 1,
                },
            );
        }
    }
    known
}

struct DeepObserver<'a> {
    shared: &'a Shared,
    cancel: &'a AtomicBool,
    emit: &'a Emit,
    throttle: Throttle,
    known: HashMap<u64, Known>,
    added: u32,
}

impl ScanObserver for DeepObserver<'_> {
    fn found(&mut self, f: &FoundFile) -> ControlFlow<()> {
        let mut s = lock(self.shared);
        if let Some(k) = self.known.get(&f.offset) {
            // Already listed with its real name. If the content checks out,
            // the file is certainly intact.
            if k.contiguous && k.size == f.len {
                if let Some(item) = s.items.get_mut(k.id as usize) {
                    item.verified = true;
                    item.confidence = item.confidence.max(f.confidence);
                }
            }
            return ControlFlow::Continue(());
        }
        let id = s.items.len() as u32;
        self.added += 1;
        let category = f.category;
        let mut folder = category.name().to_owned();
        folder[..1].make_ascii_uppercase();
        s.items.push(Item {
            id,
            name: format!("{}_{:06}.{}", category.name(), self.added, f.extension),
            folder: format!("/Deep scan/{folder}s"),
            ext: f.extension.into(),
            size: f.len,
            modified: None,
            // The content alone doesn't tell whether the file was deleted.
            deleted: false,
            category,
            confidence: f.confidence,
            method: Method::Deep,
            problem: None,
            verified: false,
            found_as: None,
        });
        s.origins.push(Origin::Carved(f.clone()));
        ControlFlow::Continue(())
    }

    fn progress(&mut self, done: u64, total: u64) -> ControlFlow<()> {
        if self.throttle.ready() {
            let found = lock(self.shared).items.len();
            (self.emit)(
                "scan-progress",
                json!({ "phase": "Deep scan", "done": done, "total": total, "found": found }),
            );
        }
        if self.cancel.load(Ordering::Relaxed) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// Items found so far, starting at `from` (to fetch new ones during a scan).
pub fn items(shared: &Shared, from: usize) -> Vec<Item> {
    let s = lock(shared);
    s.items.get(from..).unwrap_or_default().to_vec()
}

/// Writes up to `max` bytes, then stops the copy.
struct Limited {
    data: Vec<u8>,
    max: usize,
}

impl Write for Limited {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let room = self.max - self.data.len();
        self.data.extend_from_slice(&buf[..buf.len().min(room)]);
        if self.data.len() == self.max {
            return Err(io::Error::other("limit reached"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads up to `max` bytes of item `id`.
/// Writes the content of a found file.
fn write_origin(src: Option<&dyn Source>, origin: &Origin, out: &mut dyn Write) -> io::Result<()> {
    let no_drive = || io::Error::new(io::ErrorKind::NotFound, "no drive is open");
    match origin {
        Origin::Fs { entry, .. } => rfs::extract(src.ok_or_else(no_drive)?, entry, out),
        Origin::Carved(f) => recover::extract(src.ok_or_else(no_drive)?, f, out),
        Origin::File(path) => io::copy(&mut File::open(path)?, out).map(|_| ()),
        Origin::Embedded { path, found } => recover::extract(&FileSource::open(path)?, found, out),
    }
}

pub fn read_item(shared: &Shared, id: u32, max: u64) -> Result<(Item, Vec<u8>), String> {
    let (item, origin, src) = {
        let s = lock(shared);
        let (item, origin) = s
            .items
            .get(id as usize)
            .cloned()
            .zip(s.origins.get(id as usize).cloned())
            .ok_or("No such file.")?;
        (
            item,
            origin,
            s.source.as_ref().map(|(_, src)| Arc::clone(src)),
        )
    };
    let max = max.min(item.size) as usize;
    let mut out = Limited {
        data: Vec::with_capacity(max),
        max,
    };
    if max > 0 {
        let result = write_origin(src.as_deref().map(|s| s as &dyn Source), &origin, &mut out);
        if let Err(e) = result {
            if out.data.len() < max {
                return Err(describe(&e));
            }
        }
    }
    Ok((item, out.data))
}

pub fn mime_type(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "mp4" | "m4v" | "3gp" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// A text or hex preview of a file.
#[derive(Debug, Serialize)]
pub struct TextPreview {
    /// "text" or "hex".
    pub kind: &'static str,
    pub content: String,
    pub truncated: bool,
}

const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "csv", "md", "log", "json", "xml", "html", "htm", "ini", "cfg", "conf", "yaml", "yml",
    "srt", "js", "css", "py", "rs", "c", "h", "java", "sh", "bat",
];

pub fn text_preview(shared: &Shared, id: u32) -> Result<TextPreview, String> {
    let (item, data) = read_item(shared, id, 256 << 10)?;
    let truncated = (data.len() as u64) < item.size;
    if TEXT_EXTENSIONS.contains(&item.ext.as_str()) {
        if let Ok(text) = std::str::from_utf8(&data) {
            return Ok(TextPreview {
                kind: "text",
                content: text.into(),
                truncated,
            });
        }
        // Cut in the middle of a character, or another encoding.
        return Ok(TextPreview {
            kind: "text",
            content: String::from_utf8_lossy(&data).into_owned(),
            truncated,
        });
    }
    let shown = &data[..data.len().min(4096)];
    let mut content = String::new();
    // 8 bytes per row fit the narrow preview pane.
    for (i, row) in shown.chunks(8).enumerate() {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = row
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    char::from(b)
                } else {
                    '.'
                }
            })
            .collect();
        content.push_str(&format!("{:06x}  {:<24} {ascii}\n", i * 8, hex.join(" ")));
    }
    Ok(TextPreview {
        kind: "hex",
        content,
        truncated: (shown.len() as u64) < item.size,
    })
}

/// Recovers `ids` into `out` in the background.
pub fn start_recover(
    shared: &Shared,
    ids: Vec<u32>,
    out: PathBuf,
    emit: Emit,
) -> Result<(), String> {
    {
        let s = lock(shared);
        if let Some((source_path, _)) = &s.source {
            if devices::same_disk(source_path, &out).unwrap_or(false) {
                return Err(
                    "This folder is on the drive you are recovering from. Saving there \
                            could overwrite the files you want back. Please choose a folder on \
                            another drive (for example a USB stick or external disk)."
                        .into(),
                );
            }
        }
    }
    let (src, cancel) = begin_any(shared)?;
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let src = src.as_deref().map(|s| s as &dyn Source);
        let total = ids.len();
        let (mut saved, mut failed) = (0u32, 0u32);
        let mut errors = Vec::new();
        let mut throttle = Throttle::new();
        for (n, id) in ids.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if throttle.ready() {
                emit("recover-progress", json!({ "done": n, "total": total }));
            }
            // Copy what is needed so the session stays usable while saving.
            let job = {
                let s = lock(&shared);
                s.items
                    .get(*id as usize)
                    .cloned()
                    .zip(s.origins.get(*id as usize).cloned())
                    .map(|(item, origin)| {
                        let volume = match &origin {
                            Origin::Fs { volume, .. } => s.volumes.get(*volume).cloned(),
                            _ => None,
                        };
                        (item, origin, volume)
                    })
            };
            let result = match job {
                Some((item, origin, volume)) => save(src, volume.as_deref(), &item, &origin, &out)
                    .map_err(|e| format!("{}: {}", item.name, describe(&e))),
                None => Err(format!("file {id} not found")),
            };
            match result {
                Ok(()) => saved += 1,
                Err(e) => {
                    failed += 1;
                    if errors.len() < 50 {
                        errors.push(e);
                    }
                }
            }
        }
        finish(&shared);
        emit(
            "recover-done",
            json!({
                "saved": saved,
                "failed": failed,
                "errors": errors,
                "cancelled": cancel.load(Ordering::Relaxed),
                "folder": out,
            }),
        );
    });
    Ok(())
}

fn save(
    src: Option<&dyn Source>,
    volume: Option<&str>,
    item: &Item,
    origin: &Origin,
    out: &Path,
) -> io::Result<()> {
    if let (Origin::Fs { entry, .. }, Some(src)) = (origin, src) {
        let mut dir = out.to_path_buf();
        if let Some(name) = volume.filter(|n| !n.is_empty()) {
            dir.push(recover::safe_name(name));
        }
        return recover::save_fs_entry(src, entry, &dir).map(|_| ());
    }
    // Everything else is saved under the folder shown in the list.
    let mut path = out.to_path_buf();
    for part in item.folder.split('/').filter(|p| !p.is_empty()) {
        path.push(recover::safe_name(part));
    }
    let path = recover::unique_path(path.join(recover::safe_name(&item.name)));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut w = BufWriter::new(File::create(&path)?);
    write_origin(src, origin, &mut w)?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    if let Some(t) = item.modified.and_then(|t| u64::try_from(t).ok()) {
        let _ = file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(t));
    }
    Ok(())
}

/// Where recovered files go when there is no folder picker (Android).
pub fn default_output_dir() -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let stamp = rfs::format_time(now).replace(':', "-");
    PathBuf::from("/storage/emulated/0/Download/Full Recovery").join(stamp)
}

/// Copies the open drive to an image file in the background.
pub fn start_image(shared: &Shared, dest: PathBuf, emit: Emit) -> Result<(), String> {
    let (src, cancel) = begin(shared)?;
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let result = (|| -> io::Result<image::ImageReport> {
            let mut out = BufWriter::with_capacity(4 << 20, File::create_new(&dest)?);
            let mut throttle = Throttle::new();
            image::create_image(&*src, &mut out, &mut |done, total| {
                if throttle.ready() {
                    emit("image-progress", json!({ "done": done, "total": total }));
                }
                if cancel.load(Ordering::Relaxed) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
        })();
        finish(&shared);
        match result {
            Ok(report) => emit(
                "image-done",
                json!({
                    "path": dest,
                    "bytes": report.bytes_copied,
                    "bad_bytes": report.bad_ranges.iter().map(|r| r.1).sum::<u64>(),
                    "cancelled": report.cancelled,
                }),
            ),
            Err(e) => emit("image-done", json!({ "error": describe(&e) })),
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::mpsc;

    use super::*;

    fn test_image(dir: &Path) -> PathBuf {
        let gz = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../crates/core/tests/images/ntfs.img.gz"
        );
        let mut data = Vec::new();
        flate2::read::GzDecoder::new(File::open(gz).unwrap())
            .read_to_end(&mut data)
            .unwrap();
        let path = dir.join("disk.img");
        std::fs::write(&path, data).unwrap();
        path
    }

    fn events() -> (Emit, mpsc::Receiver<(String, Value)>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let emit: Emit = Arc::new(move |name, payload| {
            let _ = tx.lock().unwrap().send((name.to_owned(), payload));
        });
        (emit, rx)
    }

    fn wait_for(rx: &mpsc::Receiver<(String, Value)>, name: &str) -> Value {
        loop {
            let (event, payload) = rx.recv_timeout(Duration::from_secs(30)).unwrap();
            if event == name {
                return payload;
            }
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fullrecovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn quick_scan_preview_and_recover() {
        let dir = temp_dir("session");
        let shared: Shared = Arc::default();
        let info = open(&shared, test_image(&dir).to_str().unwrap()).unwrap();
        assert_eq!(info.partitions.len(), 1);

        let (emit, rx) = events();
        start_quick_scan(&shared, false, emit.clone()).unwrap();
        let done = wait_for(&rx, "scan-done");
        assert_eq!(done["error"], Value::Null);
        let all = items(&shared, 0);
        assert_eq!(all.len(), 11);
        let photo = all
            .iter()
            .find(|i| i.name == "deleted picture.jpg")
            .unwrap();
        assert_eq!(
            (
                photo.folder.as_str(),
                photo.deleted,
                photo.category,
                photo.confidence
            ),
            ("/Photos", true, Category::Photo, Confidence::High)
        );

        let text = all.iter().find(|i| i.name == "readme.txt").unwrap();
        let preview = text_preview(&shared, text.id).unwrap();
        assert_eq!(preview.kind, "text");
        assert!(preview.content.starts_with("readme.txt:0\n"));
        assert_eq!(text_preview(&shared, photo.id).unwrap().kind, "hex");

        let out = dir.join("recovered");
        let ids = all.iter().filter(|i| i.deleted).map(|i| i.id).collect();
        start_recover(&shared, ids, out.clone(), emit).unwrap();
        let done = wait_for(&rx, "recover-done");
        assert_eq!(
            (done["saved"].as_u64(), done["failed"].as_u64()),
            (Some(6), Some(0))
        );
        let saved = std::fs::read(out.join("Photos/deleted picture.jpg")).unwrap();
        assert_eq!(saved.len(), 15000);
        assert!(saved.starts_with(b"deleted picture.jpg:0\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deep_scan_finds_content() {
        let dir = temp_dir("deep");
        let path = dir.join("disk.img");
        let mut disk = vec![0u8; 1 << 20];
        let pdf = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\nstartxref\n0\n%%EOF\n";
        disk[4096..4096 + pdf.len()].copy_from_slice(pdf);
        std::fs::write(&path, &disk).unwrap();
        let shared: Shared = Arc::default();
        open(&shared, path.to_str().unwrap()).unwrap();
        let (emit, rx) = events();
        start_deep_scan(&shared, emit).unwrap();
        wait_for(&rx, "scan-done");
        let all = items(&shared, 0);
        assert_eq!(all.len(), 1);
        assert_eq!(
            (all[0].name.as_str(), all[0].folder.as_str(), all[0].method),
            ("document_000001.pdf", "/Deep scan/Documents", Method::Deep)
        );
        let (_, data) = read_item(&shared, 0, MAX_PREVIEW).unwrap();
        assert_eq!(data, pdf);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deep_scan_after_quick_scan_adds_only_new_files() {
        let dir = temp_dir("dedupe");
        let gz = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../crates/core/tests/images/fat16.img.gz"
        );
        let mut data = Vec::new();
        flate2::read::GzDecoder::new(File::open(gz).unwrap())
            .read_to_end(&mut data)
            .unwrap();
        // A JPEG in free space that no directory entry points to any more.
        let jpeg = [
            &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10][..],
            b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00",
            &[0xFF, 0xC0, 0x00, 0x0B, 8, 0, 1, 0, 1, 1, 1, 0x11, 0],
            &[
                0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 0x3F, 0, 0x12, 0x34, 0xFF, 0xD9,
            ],
        ]
        .concat();
        let at = data.len() - 8192;
        data[at..at + jpeg.len()].copy_from_slice(&jpeg);
        let path = dir.join("disk.img");
        std::fs::write(&path, &data).unwrap();

        let shared: Shared = Arc::default();
        open(&shared, path.to_str().unwrap()).unwrap();
        let (emit, rx) = events();
        start_quick_scan(&shared, false, emit.clone()).unwrap();
        wait_for(&rx, "scan-done");
        assert_eq!(items(&shared, 0).len(), 11);
        start_deep_scan(&shared, emit).unwrap();
        wait_for(&rx, "scan-done");
        let all = items(&shared, 0);
        let new: Vec<_> = all.iter().filter(|i| i.method == Method::Deep).collect();
        assert_eq!(new.len(), 1, "{new:?}");
        assert_eq!(
            (new[0].name.as_str(), new[0].size),
            ("photo_000001.jpg", jpeg.len() as u64)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tiny but complete JPEG.
    fn jpeg() -> Vec<u8> {
        [
            &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10][..],
            b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00",
            &[0xFF, 0xC0, 0x00, 0x0B, 8, 0, 1, 0, 1, 1, 1, 0x11, 0],
            &[
                0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 0x3F, 0, 0x12, 0x34, 0xFF, 0xD9,
            ],
        ]
        .concat()
    }

    #[test]
    fn folder_scan_finds_trash_and_cached_photos() {
        let dir = temp_dir("folder");
        let phone = dir.join("phone");
        let camera = phone.join("DCIM/Camera");
        std::fs::create_dir_all(&camera).unwrap();
        std::fs::write(camera.join("visible.jpg"), jpeg()).unwrap();
        std::fs::write(camera.join(".trashed-1735689600-IMG_1.jpg"), jpeg()).unwrap();
        let mut cache = vec![1u8; 2777];
        cache.extend(jpeg());
        cache.extend([2u8; 100]);
        std::fs::create_dir_all(phone.join("DCIM/.thumbnails")).unwrap();
        std::fs::write(phone.join("DCIM/.thumbnails/.thumbdata4-1"), &cache).unwrap();

        let shared: Shared = Arc::default();
        let info = open_folder(&shared, phone.to_str().unwrap()).unwrap();
        assert!(info.folder);
        assert!(start_deep_scan(&shared, Arc::new(|_, _| {})).is_err());
        let (emit, rx) = events();
        start_quick_scan(&shared, false, emit.clone()).unwrap();
        wait_for(&rx, "scan-done");
        let all = items(&shared, 0);
        let mut names: Vec<_> = all
            .iter()
            .map(|i| (i.name.as_str(), i.deleted, i.found_as))
            .collect();
        names.sort_by_key(|n| n.0);
        assert_eq!(
            names,
            [
                ("IMG_1.jpg", true, Some(FindKind::Trashed)),
                ("thumbdata4-1_001.jpg", false, Some(FindKind::Embedded)),
            ]
        );
        for item in &all {
            assert_eq!(read_item(&shared, item.id, MAX_PREVIEW).unwrap().1, jpeg());
        }

        let out = dir.join("out");
        start_recover(
            &shared,
            all.iter().map(|i| i.id).collect(),
            out.clone(),
            emit,
        )
        .unwrap();
        let done = wait_for(&rx, "recover-done");
        assert_eq!(done["saved"].as_u64(), Some(2));
        assert_eq!(
            std::fs::read(out.join("DCIM/Camera/IMG_1.jpg")).unwrap(),
            jpeg()
        );
        assert_eq!(
            std::fs::read(out.join("DCIM/.thumbnails/.thumbdata4-1/thumbdata4-1_001.jpg")).unwrap(),
            jpeg()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn busy_session_refuses_second_job() {
        let shared: Shared = Arc::default();
        assert!(start_deep_scan(&shared, Arc::new(|_, _| {})).is_err());
        lock(&shared).busy = true;
        assert!(open(&shared, "/nonexistent").is_err());
    }
}
