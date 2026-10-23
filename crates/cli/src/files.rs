//! Commands that work with partitions and file systems: `partitions`,
//! `files` and `undelete`.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use clap::Args;
use fullrecovery_core::fs::{self as rfs, FileData, FsEntry, Volume};
use fullrecovery_core::partition::{self, Partition};
use fullrecovery_core::{Confidence, Source};
use indicatif::HumanBytes;
use serde::Serialize;

use crate::{open_source, parse_size, progress_bar, safety};

#[derive(Args)]
pub struct VolumeArgs {
    /// Drive, partition or disk image (read-only).
    source: PathBuf,
    /// Only use this partition (number from `fullrecovery partitions`).
    #[arg(short, long)]
    partition: Option<u32>,
    /// Use the file system starting at this byte offset (from `partitions --search`).
    #[arg(long, value_parser = parse_size, conflicts_with = "partition")]
    offset: Option<u64>,
    /// Also search the whole drive for lost or deleted partitions (slow).
    #[arg(long, conflicts_with = "offset")]
    search: bool,
    /// Size of the source in bytes, if it cannot be detected.
    #[arg(long, value_parser = parse_size)]
    size: Option<u64>,
}

fn search(src: &dyn Source) -> Result<Vec<Partition>> {
    println!("Searching the whole drive for file systems...");
    let bar = progress_bar(src.len());
    let found = partition::search(src, &mut |done, _| {
        bar.set_position(done);
        ControlFlow::Continue(())
    })?;
    bar.finish_and_clear();
    Ok(found)
}

pub fn run_partitions(source: &Path, size: Option<u64>, search: bool) -> Result<()> {
    let src = open_source(source, size)?;
    let table = partition::read_table(&src)?;
    if table.is_empty() {
        println!("No partition table or known file system found.");
    } else {
        print_partitions(&table);
    }
    if search {
        println!();
        let found = self::search(&src)?;
        let lost: Vec<Partition> = found
            .into_iter()
            .filter(|f| !table.iter().any(|p| p.offset == f.offset))
            .collect();
        if lost.is_empty() {
            println!("No lost partitions found.");
        } else {
            println!(
                "Found {} file system(s) not in the partition table:",
                lost.len()
            );
            print_partitions(&lost);
            println!(
                "\nTo see their files: `fullrecovery files {} --search`.",
                source.display()
            );
        }
    } else if !table.is_empty() {
        println!("\nPartition missing? Try `--search` to look for lost partitions.");
    }
    Ok(())
}

fn print_partitions(parts: &[Partition]) {
    println!(
        "{:>3}  {:>14}  {:>10}  {:<7} {:<24} Name",
        "#", "Offset", "Size", "FS", "Type"
    );
    for p in parts {
        println!(
            "{:>3}  {:>14}  {:>10}  {:<7} {:<24} {}",
            p.index,
            p.offset,
            HumanBytes(p.size).to_string(),
            p.file_system.map_or("?", |f| f.name()),
            p.kind,
            p.name.as_deref().unwrap_or("")
        );
    }
}

/// Opens the selected volumes. Each comes with a folder name for its files.
fn open_volumes(args: &VolumeArgs, src: &dyn Source) -> Result<Vec<(String, Volume)>> {
    if let Some(offset) = args.offset {
        let Some(v) = rfs::open(src, offset)? else {
            bail!("no known file system at offset {offset}");
        };
        return Ok(vec![(format!("offset-{offset}"), v)]);
    }
    let mut table = partition::read_table(src)?;
    if args.search {
        let lost: Vec<Partition> = search(src)?
            .into_iter()
            .filter(|f| !table.iter().any(|p| p.offset == f.offset))
            .collect();
        let first = table.len() as u32 + 1;
        table.extend(lost.into_iter().enumerate().map(|(i, mut p)| {
            p.index = first + i as u32;
            p
        }));
    }
    let mut volumes = Vec::new();
    for p in &table {
        if args.partition.is_some_and(|n| n != p.index) {
            continue;
        }
        if p.file_system.is_none() {
            if args.partition.is_some() {
                bail!(
                    "partition {} has no supported file system (FAT, exFAT, NTFS)",
                    p.index
                );
            }
            continue;
        }
        if let Some(v) = rfs::open_boot(src, p.offset, p.boot_offset)? {
            volumes.push((format!("partition{}-{}", p.index, v.info.kind.name()), v));
        }
    }
    if volumes.is_empty() {
        match args.partition {
            Some(n) => bail!("there is no partition {n}; see `fullrecovery partitions`"),
            None => bail!(
                "no FAT, exFAT or NTFS file system found. Try `fullrecovery partitions --search` \
                 for lost partitions, or `fullrecovery recover` for a deep scan."
            ),
        }
    }
    Ok(volumes)
}

fn volume_title(name: &str, v: &Volume) -> String {
    format!(
        "{name}: {} {}{}",
        v.info.kind.name(),
        HumanBytes(v.info.size),
        v.label
            .as_ref()
            .map(|l| format!(" \"{l}\""))
            .unwrap_or_default()
    )
}

pub fn run_files(args: &VolumeArgs, only_deleted: bool) -> Result<()> {
    let src = open_source(&args.source, args.size)?;
    for (name, v) in open_volumes(args, &src)? {
        println!("{}", volume_title(&name, &v));
        let mut deleted = 0;
        for e in v.entries.iter().filter(|e| !only_deleted || e.deleted) {
            deleted += u32::from(e.deleted && !e.is_dir);
            println!(
                "  {:<7} {:>10}  {:<16}  {:<6}  {}{}",
                if e.deleted { "deleted" } else { "" },
                if e.is_dir {
                    String::new()
                } else {
                    HumanBytes(e.size).to_string()
                },
                e.modified.map(rfs::format_time).unwrap_or_default(),
                if e.deleted {
                    confidence_name(e.confidence)
                } else {
                    ""
                },
                e.path,
                if e.is_dir { "/" } else { "" }
            );
        }
        println!("  {deleted} deleted files\n");
    }
    Ok(())
}

fn confidence_name(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

#[derive(Serialize)]
struct Recovered<'a> {
    #[serde(flatten)]
    entry: &'a FsEntry,
    saved_as: PathBuf,
}

pub fn run_undelete(
    args: &VolumeArgs,
    out: &Path,
    all: bool,
    min_confidence: Confidence,
    path_filter: Option<&str>,
) -> Result<()> {
    safety::check_output_location(&args.source, out)?;
    let src = open_source(&args.source, args.size)?;
    let volumes = open_volumes(args, &src)?;
    let filter = path_filter.map(str::to_lowercase);
    let mut report = Vec::new();
    let (mut saved, mut failed) = (0u64, 0u64);
    for (name, v) in &volumes {
        println!("{}", volume_title(name, v));
        let dir = if volumes.len() == 1 {
            out.to_path_buf()
        } else {
            out.join(name)
        };
        for e in &v.entries {
            let wanted = !e.is_dir
                && (all || e.deleted)
                && e.confidence >= min_confidence
                && filter
                    .as_ref()
                    .is_none_or(|f| e.path.to_lowercase().contains(f));
            if !wanted {
                continue;
            }
            match save(&src, e, &dir) {
                Ok(path) => {
                    saved += 1;
                    report.push(Recovered {
                        entry: e,
                        saved_as: path,
                    });
                }
                Err(err) => {
                    failed += 1;
                    println!("  could not recover {}: {err:#}", e.path);
                }
            }
        }
    }
    fs::create_dir_all(out)?;
    let report_path = out.join("undelete-report.json");
    let mut w = BufWriter::new(File::create(&report_path)?);
    serde_json::to_writer_pretty(&mut w, &report)?;
    w.flush()?;
    println!("Recovered {saved} files to {}.", out.display());
    if failed > 0 {
        println!("{failed} files could not be recovered.");
    }
    println!("Details are in {}.", report_path.display());
    Ok(())
}

/// Saves one file under `dir`, keeping its folders, name and modification time.
fn save(src: &dyn Source, e: &FsEntry, dir: &Path) -> Result<PathBuf> {
    if let FileData::Unsupported(why) = e.data {
        bail!("{why}");
    }
    let mut path = dir.to_path_buf();
    for part in e.path.split('/').filter(|p| !p.is_empty()) {
        path.push(safe_name(part));
    }
    let path = unique(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = BufWriter::new(file);
    rfs::extract(src, e, &mut w)?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    if let Some(t) = e.modified.and_then(|t| u64::try_from(t).ok()) {
        // Keeping the original date is a nicety; ignore failures.
        let _ = file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(t));
    }
    Ok(path)
}

/// Makes a file name safe to create on Windows, macOS and Linux.
fn safe_name(name: &str) -> String {
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
fn unique(path: PathBuf) -> PathBuf {
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
