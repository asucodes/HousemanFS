//! Building and querying the index.
//!
//! A scan takes minutes; a finder has to answer in milliseconds. The index is what separates
//! the two, and this module is the bridge between the platform layer and it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use hfs_index::{DirRow, FileRow, Index, ScanMetadata, extension_of};
use hfs_win::{EntryKind, WalkOptions, volume_info, walk_with};

use crate::human;

/// Where the index lives unless told otherwise.
pub const DEFAULT_INDEX: &str = "housemanfs.db";

/// Scan `path` and store the result, replacing whatever was there.
pub fn build(path: &str, db: &str, out: &mut String) -> Result<(), String> {
    let volume = volume_info(path).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "scanning    {path}");
    let _ = writeln!(out, "index       {db}");

    let result = walk_with(
        path,
        WalkOptions {
            collect_entries: true,
        },
    )
    .map_err(|e| e.to_string())?;

    // Every directory gets an identity. Where the filesystem gave us one it is used; where it
    // did not, a value is derived from the path so that a directory and its children still
    // agree on who their parent is. Without that, files under a directory whose identity query
    // failed would be orphaned and silently missing from every query.
    let mut dir_ids: HashMap<String, [u8; 16]> = HashMap::new();
    for entry in &result.entries {
        if entry.kind == EntryKind::Directory {
            let key = entry.path.display().to_string();
            let id = entry.id.unwrap_or_else(|| derive_id(&key));
            dir_ids.insert(key, id);
        }
    }

    let mut dirs: Vec<DirRow> = Vec::new();
    let mut files: Vec<FileRow> = Vec::new();

    for entry in &result.entries {
        let path = entry.path.display().to_string();
        let name = file_name(&entry.path);
        let parent_key = entry
            .path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        match entry.kind {
            EntryKind::Reparse => {
                // Recorded during the walk, not stored: it points somewhere else, and counting
                // its target would double-count.
            }
            EntryKind::Directory => {
                dirs.push(DirRow {
                    id: dir_ids[&path],
                    parent: dir_ids.get(&parent_key).copied(),
                    name,
                    path: path.clone(),
                    depth: depth_of(&path),
                    index_bytes: entry.allocated,
                });
            }
            EntryKind::File => {
                let parent = match dir_ids.get(&parent_key).copied() {
                    Some(id) => id,
                    // The containing directory's identity was unavailable. Derive one from the
                    // path so the file still belongs to its parent rather than being dropped.
                    None => derive_id(&parent_key),
                };
                files.push(FileRow {
                    id: entry.id.unwrap_or_else(|| derive_id(&path)),
                    parent,
                    ext: extension_of(&name),
                    name,
                    logical: entry.logical,
                    allocated: entry.allocated,
                    links: entry.links,
                });
            }
        }
    }

    let meta = ScanMetadata {
        root: path.to_string(),
        filesystem: volume.filesystem.clone(),
        volume_id: *volume.volume_id.as_bytes(),
        cluster_bytes: volume.cluster_bytes,
        total_bytes: volume.total_bytes,
        free_bytes: volume.free_bytes,
    };

    let mut index = Index::open(db).map_err(|e| e.to_string())?;
    index
        .write_scan(&meta, dirs.iter(), files.iter())
        .map_err(|e| e.to_string())?;

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "stored      {} directories, {} files",
        dirs.len(),
        files.len()
    );
    let _ = writeln!(
        out,
        "occupying   {} of file data",
        human(result.summary.allocated_bytes)
    );
    if result.summary.denied_directories > 0 {
        let _ = writeln!(
            out,
            "note        {} directories were unreadable and are absent",
            result.summary.denied_directories
        );
    }

    Ok(())
}

/// The largest files by occupied size.
pub fn top(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.largest_files(limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "largest files");
    let _ = writeln!(out);
    for hit in rows {
        let _ = writeln!(
            out,
            "{:>12}  {}",
            human(hit.allocated),
            truncate(&hit.path, 100)
        );
        if hit.links > 1 {
            let _ = writeln!(out, "             {} names share this data", hit.links);
        }
    }
    Ok(())
}

/// The largest directory subtrees.
pub fn dirs(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.largest_dirs(limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "largest directories");
    let _ = writeln!(out);
    for (path, allocated, file_count) in rows {
        let _ = writeln!(
            out,
            "{:>12}  {:>7} {}  {}",
            human(allocated),
            file_count,
            noun(file_count),
            truncate(&path, 80)
        );
    }
    Ok(())
}

/// Files whose name contains a term.
pub fn search(db: &str, term: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.search_name(term, limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "matching {term:?}");
    let _ = writeln!(out);
    if rows.is_empty() {
        let _ = writeln!(out, "nothing found");
        return Ok(());
    }
    for hit in rows {
        let _ = writeln!(
            out,
            "{:>12}  {}",
            human(hit.allocated),
            truncate(&hit.path, 100)
        );
    }
    Ok(())
}

/// Totals by extension.
pub fn extensions(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.by_extension(limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "occupied by extension");
    let _ = writeln!(out);
    for (ext, allocated, count) in rows {
        let label = if ext.is_empty() {
            "(none)".to_string()
        } else {
            ext
        };
        let _ = writeln!(
            out,
            "{:>12}  {:>7} {}  {}",
            human(allocated),
            count,
            noun(count),
            label
        );
    }
    Ok(())
}

fn open(db: &str) -> Result<Index, String> {
    if !Path::new(db).exists() {
        return Err(format!(
            "no index at {db}. Build one with: housemanfs index <path>"
        ));
    }
    Index::open(db).map_err(|e| e.to_string())
}

/// "file" or "files", so a single result does not read as "1 files".
fn noun(count: u64) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

/// The final component of a path.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Depth of a Windows path, measured in separators.
///
/// Used only to order directories so subtree totals can be rolled up from the deepest first.
fn depth_of(path: &str) -> u32 {
    path.trim_end_matches(['\\', '/'])
        .matches(['\\', '/'])
        .count() as u32
}

/// A stable 16-byte value derived from text, used only when the filesystem did not supply an
/// identity. Two different seeds so the result is wider than one 64-bit hash.
fn derive_id(text: &str) -> [u8; 16] {
    fn fnv(bytes: &[u8], seed: u64) -> u64 {
        let mut h = seed;
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }
    let a = fnv(text.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let b = fnv(text.as_bytes(), 0x9e37_79b9_7f4a_7c15);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes());
    out
}

/// Shorten a path for display, keeping the beginning and the end.
///
/// Deep paths are common and the middle is rarely the interesting part.
fn truncate(path: &str, limit: usize) -> String {
    if path.chars().count() <= limit {
        return path.to_string();
    }
    let keep = limit - 3;
    let head = keep / 2;
    let tail = keep - head;
    let start: String = path.chars().take(head).collect();
    let end: String = path.chars().rev().take(tail).collect::<String>();
    let end: String = end.chars().rev().collect();
    format!("{start}...{end}")
}
