//! `fullrecovery` command-line tool.

mod files;
mod safety;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use fullrecovery_core::{
    carve, image, recover, scan, Category, Confidence, FileSource, FoundFile, ScanObserver,
    ScanOptions, Source,
};
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "fullrecovery",
    version,
    about = "Free, open-source data recovery"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the file types that can be recovered.
    Types,
    /// Deep-scan a drive or disk image and list the files that can be recovered.
    Scan {
        #[command(flatten)]
        scan: ScanArgs,
        /// Print every file found, not just the summary.
        #[arg(long)]
        list: bool,
        /// Save the full list of found files as JSON.
        #[arg(long, value_name = "FILE")]
        report: Option<PathBuf>,
    },
    /// Deep-scan a drive or disk image and save every file found.
    Recover {
        #[command(flatten)]
        scan: ScanArgs,
        /// Folder to save recovered files in. Must be on a different drive.
        #[arg(short, long, value_name = "DIR")]
        out: PathBuf,
    },
    /// Show the partitions and file systems on a drive or disk image.
    Partitions {
        /// Drive or disk image (read-only).
        source: PathBuf,
        /// Search the whole drive for lost or deleted partitions (slow).
        #[arg(long)]
        search: bool,
        /// Size of the source in bytes, if it cannot be detected.
        #[arg(long, value_parser = parse_size)]
        size: Option<u64>,
    },
    /// List files, including deleted ones, with their original names and folders.
    Files {
        #[command(flatten)]
        volume: files::VolumeArgs,
        /// Only show deleted files.
        #[arg(long)]
        deleted: bool,
    },
    /// Recover deleted files with their original names, folders and dates.
    Undelete {
        #[command(flatten)]
        volume: files::VolumeArgs,
        /// Folder to save recovered files in. Must be on a different drive.
        #[arg(short, long, value_name = "DIR")]
        out: PathBuf,
        /// Also copy files that are not deleted.
        #[arg(long)]
        all: bool,
        /// Only recover files with at least this confidence: low, medium or high.
        #[arg(long, default_value = "low")]
        min_confidence: Confidence,
        /// Only recover files whose path contains this text (not case-sensitive).
        #[arg(long, value_name = "TEXT")]
        path: Option<String>,
    },
    /// Copy a drive byte-by-byte to an image file, skipping unreadable sectors.
    Image {
        /// Drive or partition to copy (e.g. /dev/sdb, \\.\PhysicalDrive1).
        source: PathBuf,
        /// Image file to create.
        dest: PathBuf,
        /// Size of the source in bytes, if it cannot be detected.
        #[arg(long, value_parser = parse_size)]
        size: Option<u64>,
    },
}

#[derive(Args)]
struct ScanArgs {
    /// Drive, partition or disk image to scan (read-only).
    source: PathBuf,
    /// Only look for these types (ids or extensions, e.g. jpg,mp4,docx).
    #[arg(short, long, value_delimiter = ',')]
    types: Option<Vec<String>>,
    /// Only report files with at least this confidence: low, medium or high.
    #[arg(long, default_value = "low")]
    min_confidence: Confidence,
    /// Look for files every N bytes. 512 suits file systems; 1 is thorough but slow.
    #[arg(long, default_value = "512", value_parser = parse_size)]
    align: u64,
    /// Start scanning at this byte offset (suffixes K, M, G, T allowed).
    #[arg(long, default_value = "0", value_parser = parse_size)]
    start: u64,
    /// Stop scanning at this byte offset.
    #[arg(long, value_parser = parse_size)]
    end: Option<u64>,
    /// Size of the source in bytes, if it cannot be detected.
    #[arg(long, value_parser = parse_size)]
    size: Option<u64>,
    /// Also search inside files that were already recovered (finds embedded files).
    #[arg(long)]
    no_skip: bool,
}

impl ScanArgs {
    fn options(&self) -> ScanOptions {
        ScanOptions {
            alignment: self.align,
            start: self.start,
            end: self.end,
            types: self.types.clone(),
            skip_found: !self.no_skip,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Types => {
            list_types();
            Ok(())
        }
        Command::Scan { scan, list, report } => run_scan(&scan, None, list, report.as_deref()),
        Command::Recover { scan, out } => {
            safety::check_output_location(&scan.source, &out)?;
            fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
            run_scan(&scan, Some(&out), false, Some(&out.join("report.json")))
        }
        Command::Partitions {
            source,
            search,
            size,
        } => files::run_partitions(&source, size, search),
        Command::Files { volume, deleted } => files::run_files(&volume, deleted),
        Command::Undelete {
            volume,
            out,
            all,
            min_confidence,
            path,
        } => files::run_undelete(&volume, &out, all, min_confidence, path.as_deref()),
        Command::Image { source, dest, size } => run_image(&source, &dest, size),
    }
}

fn list_types() {
    for c in carve::all() {
        println!(
            "{:<8} {:<48} {}",
            c.id(),
            c.description(),
            c.extensions().join(", ")
        );
    }
}

pub(crate) fn open_source(path: &Path, size: Option<u64>) -> Result<FileSource> {
    let src = FileSource::open(path).with_context(|| {
        format!(
            "cannot open {} (raw drives usually need administrator/root rights)",
            path.display()
        )
    })?;
    let src = match size {
        Some(size) => src.with_len(size),
        None => src,
    };
    if src.is_empty() {
        bail!(
            "{} is empty or its size is unknown; pass --size",
            path.display()
        );
    }
    Ok(src)
}

pub(crate) fn progress_bar(total: u64) -> ProgressBar {
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner} [{elapsed_precise}] {wide_bar} {bytes}/{total_bytes} ({bytes_per_sec}, {eta}) {msg}",
        )
        .unwrap(),
    );
    bar.enable_steady_tick(Duration::from_millis(200));
    bar
}

struct CliObserver<'a> {
    bar: ProgressBar,
    src: &'a dyn Source,
    out: Option<&'a Path>,
    list: bool,
    min_confidence: Confidence,
    kept: u64,
    failed: u64,
}

impl ScanObserver for CliObserver<'_> {
    fn found(&mut self, f: &FoundFile) -> ControlFlow<()> {
        if f.confidence < self.min_confidence {
            return ControlFlow::Continue(());
        }
        self.kept += 1;
        if self.list {
            self.bar.println(format!(
                "{:>14}  {:>10}  {:<5} {:?}",
                f.offset,
                HumanBytes(f.len).to_string(),
                f.extension,
                f.confidence
            ));
        }
        if let Some(dir) = self.out {
            // Save files as soon as they are found, so an interrupted scan keeps them.
            if let Err(e) = recover::recover_file(self.src, f, dir) {
                self.failed += 1;
                self.bar
                    .println(format!("could not save file at offset {}: {e}", f.offset));
            }
        }
        self.bar.set_message(format!("{} files found", self.kept));
        ControlFlow::Continue(())
    }

    fn progress(&mut self, done: u64, _total: u64) -> ControlFlow<()> {
        self.bar.set_position(done);
        ControlFlow::Continue(())
    }

    fn read_error(&mut self, offset: u64, len: u64, error: &std::io::Error) {
        self.bar.println(format!(
            "unreadable: {len} bytes at offset {offset}: {error}"
        ));
    }
}

#[derive(Serialize)]
struct Report<'a> {
    source: &'a Path,
    bytes_scanned: u64,
    unreadable_bytes: u64,
    cancelled: bool,
    files: Vec<&'a FoundFile>,
}

fn run_scan(args: &ScanArgs, out: Option<&Path>, list: bool, report: Option<&Path>) -> Result<()> {
    let src = open_source(&args.source, args.size)?;
    let opts = args.options();
    if let Some(types) = &opts.types {
        if scan::select_carvers(Some(types)).is_empty() {
            bail!("no known file type in {types:?}; see `fullrecovery types`");
        }
    }
    let total = opts.end.unwrap_or(src.len()).min(src.len()) - opts.start.min(src.len());
    let mut obs = CliObserver {
        bar: progress_bar(total),
        src: &src,
        out,
        list,
        min_confidence: args.min_confidence,
        kept: 0,
        failed: 0,
    };
    let summary = scan(&src, &opts, &mut obs)?;
    obs.bar.finish_and_clear();

    let files: Vec<&FoundFile> = summary
        .files
        .iter()
        .filter(|f| f.confidence >= args.min_confidence)
        .collect();
    print_summary(&files);
    println!(
        "Scanned {} of {}.",
        HumanBytes(summary.bytes_scanned),
        HumanBytes(total)
    );
    if summary.unreadable_bytes > 0 {
        println!(
            "Warning: {} could not be read. The drive may be failing: copy it with \
             `fullrecovery image` and scan the copy.",
            HumanBytes(summary.unreadable_bytes)
        );
    }
    if let Some(dir) = out {
        println!(
            "Saved {} files to {}.",
            obs.kept - obs.failed,
            dir.display()
        );
        if obs.failed > 0 {
            println!("{} files could not be saved.", obs.failed);
        }
    }
    if let Some(path) = report {
        let report = Report {
            source: &args.source,
            bytes_scanned: summary.bytes_scanned,
            unreadable_bytes: summary.unreadable_bytes,
            cancelled: summary.cancelled,
            files,
        };
        let mut w = BufWriter::new(
            File::create(path).with_context(|| format!("creating {}", path.display()))?,
        );
        serde_json::to_writer_pretty(&mut w, &report)?;
        w.flush()?;
        println!("Report written to {}.", path.display());
    }
    Ok(())
}

fn print_summary(files: &[&FoundFile]) {
    if files.is_empty() {
        println!("No files found.");
        return;
    }
    // category -> (count, bytes, [low, medium, high])
    let mut rows: BTreeMap<Category, (u64, u64, [u64; 3])> = BTreeMap::new();
    for f in files {
        let row = rows.entry(f.category).or_default();
        row.0 += 1;
        row.1 += f.len;
        row.2[f.confidence as usize] += 1;
    }
    println!(
        "{:<10} {:>7} {:>11}   {:>6} {:>6} {:>6}",
        "Type", "Files", "Size", "High", "Medium", "Low"
    );
    for (cat, (count, bytes, conf)) in rows {
        println!(
            "{:<10} {:>7} {:>11}   {:>6} {:>6} {:>6}",
            cat.name(),
            count,
            HumanBytes(bytes).to_string(),
            conf[2],
            conf[1],
            conf[0]
        );
    }
}

fn run_image(source: &Path, dest: &Path, size: Option<u64>) -> Result<()> {
    let src = open_source(source, size)?;
    safety::check_output_location(source, dest)?;
    let mut out = BufWriter::with_capacity(
        4 << 20,
        File::create_new(dest).with_context(|| format!("creating {}", dest.display()))?,
    );
    let bar = progress_bar(src.len());
    let report = image::create_image(&src, &mut out, &mut |done, _| {
        bar.set_position(done);
        ControlFlow::Continue(())
    })?;
    bar.finish_and_clear();
    println!(
        "Copied {} to {}.",
        HumanBytes(report.bytes_copied),
        dest.display()
    );
    if !report.bad_ranges.is_empty() {
        let bad: u64 = report.bad_ranges.iter().map(|r| r.1).sum();
        let map = dest.with_extension("badsectors.txt");
        let mut w = BufWriter::new(File::create(&map)?);
        for (offset, len) in &report.bad_ranges {
            writeln!(w, "{offset} {len}")?;
        }
        w.flush()?;
        println!(
            "{} could not be read and were filled with zeros; ranges listed in {}.",
            HumanBytes(bad),
            map.display()
        );
    }
    Ok(())
}

/// Parses a byte count with an optional K/M/G/T suffix (powers of 1024).
pub(crate) fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (digits, shift) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let shift = match c.to_ascii_uppercase() {
                'K' => 10,
                'M' => 20,
                'G' => 30,
                'T' => 40,
                _ => return Err(format!("unknown size suffix in `{s}`")),
            };
            (&s[..i], shift)
        }
        _ => (s, 0),
    };
    let n: u64 = digits.parse().map_err(|_| format!("invalid size `{s}`"))?;
    n.checked_shl(shift)
        .filter(|v| v >> shift == n)
        .ok_or_else(|| format!("size `{s}` is too large"))
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512"), Ok(512));
        assert_eq!(parse_size("4k"), Ok(4096));
        assert_eq!(parse_size("2G"), Ok(2 << 30));
        assert!(parse_size("1X").is_err());
        assert!(parse_size("99999999999T").is_err());
    }
}
