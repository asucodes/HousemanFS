//! Building and querying the index.
//!
//! A scan takes minutes; a finder has to answer in milliseconds. The index is what separates
//! the two, and this module is the bridge between the platform layer and it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use hfs_core::FileProvenance;
use hfs_index::{DirRow, FileRow, Index, ScanMetadata, extension_of};
use hfs_win::{
    BackupPrivilege, EntryKind, WalkOptions, is_elevated, path_facts, volume_info, walk_with,
};

use crate::human;

/// Where the index lives unless told otherwise.
pub const DEFAULT_INDEX: &str = "housemanfs.db";

/// Scan `path` and store the result, replacing whatever was there.
pub fn build(path: &str, db: &str, out: &mut String) -> Result<(), String> {
    // Same as a scan: ask for the backup privilege, so protected files are measured rather
    // than estimated from what the directory entry claims.
    let backup = hfs_win::enable_backup_privilege();

    let volume = volume_info(path).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "scanning    {path}");
    let _ = writeln!(out, "index       {db}");
    if !backup.is_active() {
        let _ = writeln!(
            out,
            "privilege   no backup privilege — protected files are sized by report, not allocation"
        );
    }

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

    // The walk reports everything beneath the root but never the root itself. Without a
    // root row, entries directly on it would point at a parent that does not exist, and
    // every query joining files to directories would silently drop them — which is exactly
    // what happened to single-directory indexes before this row existed.
    let root_spelling = path.replace('/', "\\");
    let root_facts = path_facts(path).ok();
    let root_id = root_facts
        .as_ref()
        .and_then(|f| f.id)
        .unwrap_or_else(|| derive_id(&root_spelling));
    dir_ids.insert(root_spelling.clone(), root_id);
    dirs.push(DirRow {
        id: root_id,
        parent: None,
        name: file_name(Path::new(&root_spelling)),
        path: root_spelling.clone(),
        depth: 0,
        index_bytes: root_facts.as_ref().map(|f| f.allocated).unwrap_or(0),
        stream_bytes: 0,
        stream_count: 0,
        provenance: if root_facts.as_ref().is_some_and(|f| f.id.is_some()) {
            // The root is a directory, and directory streams are not enumerable (see the
            // walk), so even a fully-queried root is `NoStreams`, never `Full`.
            FileProvenance::NoStreams
        } else if root_facts.is_some() {
            FileProvenance::NoIdentity
        } else {
            FileProvenance::Denied
        },
    });

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
                    stream_bytes: entry.stream_bytes,
                    stream_count: entry.stream_count,
                    provenance: if !entry.streams_known {
                        FileProvenance::NoStreams
                    } else if entry.id.is_none() {
                        FileProvenance::NoIdentity
                    } else {
                        FileProvenance::Full
                    },
                });
            }
            EntryKind::File => {
                let parent = match dir_ids.get(&parent_key).copied() {
                    Some(id) => id,
                    // The containing directory's identity was unavailable. Derive one from the
                    // path so the file still belongs to its parent rather than being dropped.
                    None => derive_id(&parent_key),
                };
                // Denied first: an unreadable file also lacks an identity and enumerated
                // streams, but "could not be opened" is the truth and the other two codes
                // would merely describe its symptoms.
                let provenance = if entry.denied {
                    FileProvenance::Denied
                } else if entry.id.is_none() {
                    FileProvenance::NoIdentity
                } else if !entry.streams_known {
                    FileProvenance::NoStreams
                } else {
                    FileProvenance::Full
                };
                files.push(FileRow {
                    id: entry.id.unwrap_or_else(|| derive_id(&path)),
                    parent,
                    ext: extension_of(&name),
                    name,
                    logical: entry.logical,
                    allocated: entry.allocated,
                    links: entry.links,
                    stream_bytes: entry.stream_bytes,
                    stream_count: entry.stream_count,
                    provenance,
                });
            }
        }
    }

    let meta = ScanMetadata {
        // Stored in the walk's spelling, so the recorded root matches the paths beneath it
        // and `explain` can resolve the root itself.
        root: root_spelling,
        filesystem: volume.filesystem.clone(),
        volume_id: *volume.volume_id.as_bytes(),
        cluster_bytes: volume.cluster_bytes,
        total_bytes: volume.total_bytes,
        free_bytes: volume.free_bytes,
        elevated: is_elevated(),
        backup_privilege: match backup {
            BackupPrivilege::Enabled => "enabled".to_string(),
            BackupPrivilege::NotHeld => "not-held".to_string(),
            BackupPrivilege::Failed(code) => format!("failed(os error {code})"),
        },
        files: result.summary.files,
        directories: result.summary.directories,
        reparse_points: result.summary.reparse_points,
        stream_bytes: result.summary.stream_bytes,
        stream_files: result.summary.stream_files,
        stream_count: result.summary.stream_count,
        stream_unknown_files: result.summary.stream_unknown_files,
        denied_directories: result.summary.denied_directories,
        denied_files: result.summary.denied_files,
        denied_file_bytes: result.summary.denied_file_bytes,
        hardlink_groups: result.summary.hardlink_groups,
        hardlink_extra_names: result.summary.hardlink_extra_names,
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
    if result.summary.stream_count > 0 {
        let _ = writeln!(
            out,
            "            {} in {} named {}",
            human(result.summary.stream_bytes),
            result.summary.stream_count,
            plural(result.summary.stream_count, "stream", "streams"),
        );
    }
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
        write_stream_note(out, hit.stream_bytes, hit.stream_count);
    }
    Ok(())
}

/// The largest directory subtrees.
pub fn dirs(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.largest_dirs(limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "largest directories");
    let _ = writeln!(out);
    for hit in rows {
        let _ = writeln!(
            out,
            "{:>12}  {:>7} {}  {}",
            human(hit.allocated),
            hit.files,
            noun(hit.files),
            truncate(&hit.path, 80)
        );
        if hit.streams > 0 {
            let _ = writeln!(
                out,
                "             including {} in named streams",
                human(hit.streams)
            );
        }
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
        write_stream_note(out, hit.stream_bytes, hit.stream_count);
    }
    Ok(())
}

/// The measurement behind one indexed path.
///
/// This is the provenance promise made concrete: for any figure the index reports, show
/// which measurement produced it — and say plainly which parts were never measured.
pub fn explain(db: &str, path: &str, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let scan = index.scan_metadata();

    let Some(record) = index.explain_path(path).map_err(|e| e.to_string())? else {
        let _ = writeln!(out, "{path} is not in the index");
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "the index covers {} and nothing else; a path outside it was never scanned.",
            if scan.root.is_empty() {
                "nothing".to_string()
            } else {
                scan.root.clone()
            }
        );
        return Ok(());
    };

    let _ = writeln!(out, "{}", record.path);
    let _ = writeln!(out, "kind        {}", record.kind);
    if record.kind == "file" {
        let _ = writeln!(out, "reported    {}", human(record.logical));
    }
    let _ = writeln!(
        out,
        "occupies    {} ({})",
        human(record.allocated),
        if record.provenance.allocation_measured() {
            "measured"
        } else {
            "reported, not measured"
        }
    );
    if record.links > 1 {
        let _ = writeln!(
            out,
            "identity    {} names share this data; counted once",
            record.links
        );
    }
    match (record.stream_count, record.provenance) {
        (0, FileProvenance::Full | FileProvenance::NoIdentity) => {
            let _ = writeln!(out, "streams     none — measured");
        }
        (0, _) => {
            let _ = writeln!(out, "streams     unknown — never enumerated for this entry");
        }
        (n, _) => {
            let _ = writeln!(
                out,
                "streams     {} across {} {} — measured on allocation",
                human(record.stream_bytes),
                n,
                plural(u64::from(n), "stream", "streams"),
            );
        }
    }
    let _ = writeln!(out, "measured by {}", provenance_text(record.provenance));
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "indexed from {} ({}), scanned as {} with backup privilege {}; {} {} unreadable there.",
        scan.root,
        scan.filesystem,
        if scan.elevated {
            "elevated"
        } else {
            "standard user"
        },
        if scan.backup_privilege.is_empty() {
            "unknown"
        } else {
            &scan.backup_privilege
        },
        scan.denied_directories.saturating_add(scan.denied_files),
        plural(
            scan.denied_directories.saturating_add(scan.denied_files),
            "path was",
            "paths were"
        ),
    );
    Ok(())
}

/// The index as versioned JSON: the extension surface.
///
/// Anything may build on this output in any language, without running code inside a process
/// that can read the whole disk. The contract is versioned (`contract_version`) separately
/// from the storage schema (`schema_version`): the storage may change how it keeps facts,
/// but this shape changes only deliberately. This is a snapshot — the largest lists are
/// capped by `--limit` — not a full dump; it answers "what does the index say", not "give
/// me every row".
pub fn export(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let scan = index.scan_metadata();
    let summary = index.summary().map_err(|e| e.to_string())?;
    let files = index.largest_files(limit).map_err(|e| e.to_string())?;
    let dirs = index.largest_dirs(limit).map_err(|e| e.to_string())?;
    let ext = index.by_extension(limit).map_err(|e| e.to_string())?;

    // Built by hand rather than derived: the contract shape is a compatibility promise, and
    // spelling each field keeps additions deliberate instead of automatic.
    let document = serde_json::json!({
        "contract": "housemanfs-export",
        "contract_version": 1,
        "schema_version": hfs_index::SCHEMA_VERSION,
        "meta": {
            "root": scan.root,
            "filesystem": scan.filesystem,
            "cluster_bytes": scan.cluster_bytes,
            "total_bytes": scan.total_bytes,
            "free_bytes": scan.free_bytes,
        },
        "scan": {
            "elevated": scan.elevated,
            "backup_privilege": scan.backup_privilege,
            "files": scan.files,
            "directories": scan.directories,
            "reparse_points": scan.reparse_points,
            "stream_bytes": scan.stream_bytes,
            "stream_files": scan.stream_files,
            "stream_count": scan.stream_count,
            "stream_unknown_files": scan.stream_unknown_files,
            "denied_directories": scan.denied_directories,
            "denied_files": scan.denied_files,
            "denied_file_bytes": scan.denied_file_bytes,
            "hardlink_groups": scan.hardlink_groups,
            "hardlink_extra_names": scan.hardlink_extra_names,
        },
        "summary": {
            "files": summary.files,
            "directories": summary.directories,
            "logical": summary.logical,
            "allocated": summary.allocated,
            "stream_bytes": summary.stream_bytes,
        },
        "largest_files": files.iter().map(|hit| serde_json::json!({
            "path": hit.path,
            "logical": hit.logical,
            "allocated": hit.allocated,
            "links": hit.links,
            "stream_bytes": hit.stream_bytes,
            "stream_count": hit.stream_count,
            "provenance": hit.provenance.as_str(),
        })).collect::<Vec<_>>(),
        "largest_dirs": dirs.iter().map(|hit| serde_json::json!({
            "path": hit.path,
            "allocated": hit.allocated,
            "streams": hit.streams,
            "files": hit.files,
        })).collect::<Vec<_>>(),
        "by_extension": ext.iter().map(|(name, allocated, count)| serde_json::json!({
            "ext": name,
            "allocated": allocated,
            "files": count,
        })).collect::<Vec<_>>(),
    });

    out.push_str(&serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?);
    out.push('\n');
    Ok(())
}

/// What a provenance code means, in the words `explain` shows.
fn provenance_text(provenance: FileProvenance) -> &'static str {
    match provenance {
        FileProvenance::Full => "allocation, identity and streams",
        FileProvenance::NoIdentity => {
            "allocation and streams; identity unavailable, so hardlink grouping may overcount"
        }
        FileProvenance::NoStreams => {
            "allocation and identity; streams never enumerated, so the stream figure is unknown"
        }
        FileProvenance::Denied => {
            "nothing — the entry could not be opened, and its size is the directory's report"
        }
        FileProvenance::Unknown => {
            "an unknown combination — indexed before provenance was recorded; treat as approximate"
        }
    }
}

/// One line about named streams, printed only when there is something to say.
fn write_stream_note(out: &mut String, bytes: u64, count: u32) {
    if count > 0 {
        let _ = writeln!(
            out,
            "             plus {} in {} named {}",
            human(bytes),
            count,
            plural(u64::from(count), "stream", "streams"),
        );
    }
}

/// Pick the singular or plural form. A report that says "1 streams" is a small
/// carelessness, and carelessness compounds in a tool that asks to be trusted.
pub(crate) fn plural(count: u64, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
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
