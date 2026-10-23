//! Commands that work with partitions and file systems: `partitions`,
//! `files` and `undelete`.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Args;
use fullrecovery_core::fs::{self as rfs, FsEntry, Volume};
use fullrecovery_core::partition::{self, Partition};
use fullrecovery_core::{recover, Confidence, Source};
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
            match recover::save_fs_entry(&src, e, &dir) {
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
