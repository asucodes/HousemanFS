//! Building and querying the index.
//!
//! A scan takes minutes; a finder has to answer in milliseconds. The index is what separates
//! the two, and this module is the bridge between the platform layer and it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use hfs_core::FileProvenance;
use hfs_index::{DirRow, FileRow, Index, RefreshReport, ScanMetadata, extension_of};
use hfs_win::{
    BackupPrivilege, EntryKind, WalkOptions, WalkSummary, is_elevated, path_facts, volume_info,
    walk_with,
};

use crate::human;

/// Where the index lives unless told otherwise.
pub const DEFAULT_INDEX: &str = "housemanfs.db";

/// Scan `path` and store the result, replacing whatever was there.
pub fn build(path: &str, db: &str, out: &mut String) -> Result<(), String> {
    let _ = writeln!(out, "index       {db}");
    let scan = collect(path, out)?;

    let mut index = Index::open(db).map_err(|e| e.to_string())?;
    index
        .write_scan(&scan.meta, scan.dirs.iter(), scan.files.iter())
        .map_err(|e| e.to_string())?;

    print_stored(out, &scan);
    Ok(())
}

/// Re-scan and fold the result into the existing index, touching only what changed.
///
/// Refuses to run against an index built from a different volume: volume identity survives
/// remounting, so a mismatch means the index is not this path's past, and merging them
/// would corrupt both histories. Rebuild instead. The check runs before the walk, because a
/// refused refresh must not cost a full scan.
pub fn refresh(db: &str, path: &str, out: &mut String) -> Result<(), String> {
    let mut index = open(db)?;
    let stored = index.scan_metadata();
    let root = if path.is_empty() {
        stored.root.clone()
    } else {
        path.to_string()
    };
    if root.is_empty() {
        return Err("no path to refresh: pass one explicitly".to_string());
    }

    let live = volume_info(&root).map_err(|e| e.to_string())?;
    if *live.volume_id.as_bytes() != stored.volume_id && stored.volume_id != [0u8; 16] {
        return Err(format!(
            "index covers volume {} but {root} is a different volume; rebuild with `index`",
            stored.root
        ));
    }

    let scan = collect(&root, out)?;
    let report = index
        .refresh_scan(&scan.meta, scan.dirs.iter(), scan.files.iter())
        .map_err(|e| e.to_string())?;

    print_refresh(out, &report);
    Ok(())
}

/// One walk, converted to index rows and scan facts.
struct CollectedScan {
    dirs: Vec<DirRow>,
    files: Vec<FileRow>,
    meta: ScanMetadata,
    summary: WalkSummary,
}

/// One walk, converted to index rows and scan facts.
///
/// Shared by full builds and refreshes: the walk is the expensive part, and both commands
/// must agree on how entries become rows or their results would silently diverge.
fn collect(path: &str, out: &mut String) -> Result<CollectedScan, String> {
    // Same as a scan: ask for the backup privilege, so protected files are measured rather
    // than estimated from what the directory entry claims.
    let backup = hfs_win::enable_backup_privilege();

    let volume = volume_info(path).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "scanning    {path}");
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
                // A denied directory was listed but not entered: its row marks the subtree
                // as unscanned, so a refresh keeps the rows beneath it instead of reading
                // the absence as deletion.
                let provenance = if entry.denied {
                    FileProvenance::Denied
                } else if !entry.streams_known {
                    FileProvenance::NoStreams
                } else if entry.id.is_none() {
                    FileProvenance::NoIdentity
                } else {
                    FileProvenance::Full
                };
                dirs.push(DirRow {
                    id: dir_ids[&path],
                    parent: dir_ids.get(&parent_key).copied(),
                    name,
                    path: path.clone(),
                    depth: depth_of(&path),
                    index_bytes: entry.allocated,
                    stream_bytes: entry.stream_bytes,
                    stream_count: entry.stream_count,
                    provenance,
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
                    // Unknown timestamps store as 0, which a later refresh reads as
                    // "changed" — re-verifying is safe, assuming stasis is not.
                    modified: entry.modified.unwrap_or(0),
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

    Ok(CollectedScan {
        dirs,
        files,
        meta,
        summary: result.summary,
    })
}

/// What a full build stored, in the words the build reports.
fn print_stored(out: &mut String, scan: &CollectedScan) {
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "stored      {} directories, {} files",
        scan.dirs.len(),
        scan.files.len()
    );
    let _ = writeln!(
        out,
        "occupying   {} of file data",
        human(scan.summary.allocated_bytes)
    );
    if scan.summary.stream_count > 0 {
        let _ = writeln!(
            out,
            "            {} in {} named {}",
            human(scan.summary.stream_bytes),
            scan.summary.stream_count,
            plural(scan.summary.stream_count, "stream", "streams"),
        );
    }
    if scan.summary.denied_directories > 0 {
        let _ = writeln!(
            out,
            "note        {} directories were unreadable and are absent",
            scan.summary.denied_directories
        );
    }
}

/// What a refresh changed. Only nonzero lines print: a quiet refresh that found nothing is
/// the common case, and a wall of zeros would bury the days when something moved.
fn print_refresh(out: &mut String, report: &RefreshReport) {
    let _ = writeln!(out);
    let _ = writeln!(out, "refresh     complete");
    let files = report.added_files + report.removed_files + report.modified_files;
    if files == 0 && report.moved_files == 0 {
        let _ = writeln!(out, "            nothing changed");
    }
    if report.added_files > 0 {
        let _ = writeln!(out, "added       {} files", report.added_files);
    }
    if report.removed_files > 0 {
        let _ = writeln!(out, "removed     {} files", report.removed_files);
    }
    if report.modified_files > 0 {
        let _ = writeln!(out, "modified    {} files", report.modified_files);
    }
    if report.moved_files > 0 {
        let _ = writeln!(
            out,
            "moved       {} files (same data, new location)",
            report.moved_files
        );
    }
    if report.added_dirs + report.removed_dirs > 0 {
        let _ = writeln!(
            out,
            "directories +{} -{}",
            report.added_dirs, report.removed_dirs
        );
    }
    if report.stale_files + report.stale_dirs > 0 {
        let _ = writeln!(
            out,
            "note        {} files and {} directories were not rescanned and are kept as-is",
            report.stale_files, report.stale_dirs
        );
    }
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
    if record.kind == "file" {
        // An age, not a verdict. Whether old means "abandoned" is a model factor with a
        // confidence attached, not something the index declares.
        let _ = writeln!(out, "modified    {}", format_age(record.modified));
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

/// A Windows timestamp as an age. 0 means unknown and is shown as unknown — "a long time
/// ago" would be a guess wearing a measurement's clothes.
fn format_age(modified: i64) -> String {
    if modified <= 0 {
        return "unknown".to_string();
    }
    let now = windows_ticks_now();
    if modified > now {
        return "in the future — the clock moved, not the file".to_string();
    }
    const TICK: i64 = 10_000_000;
    let seconds = (now - modified) / TICK;
    const MINUTE: i64 = 60;
    const HOUR: i64 = 3_600;
    const DAY: i64 = 86_400;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    let (value, unit) = if seconds < MINUTE {
        (seconds, "second")
    } else if seconds < HOUR {
        (seconds / MINUTE, "minute")
    } else if seconds < DAY {
        (seconds / HOUR, "hour")
    } else if seconds < MONTH {
        (seconds / DAY, "day")
    } else if seconds < YEAR {
        (seconds / MONTH, "month")
    } else {
        (seconds / YEAR, "year")
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

/// Current time in Windows ticks, without depending on a datetime crate for one call.
///
/// Full precision matters: truncating to whole seconds would leave the clock up to a second
/// behind a file written "now", and every fresh file would read as modified in the future.
fn windows_ticks_now() -> i64 {
    const UNIX_TO_WINDOWS_TICKS: u128 = 11_644_473_600 * 10_000_000;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100 + UNIX_TO_WINDOWS_TICKS) as i64)
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_as_ages() {
        assert_eq!(format_age(0), "unknown");
        assert_eq!(format_age(-5), "unknown");
        let now = windows_ticks_now();
        // A file written this instant is seconds old, never from the future.
        assert!(format_age(now - 30 * 10_000_000).starts_with("30 second"));
        assert!(format_age(now - 3 * 86_400 * 10_000_000).starts_with("3 day"));
        assert!(format_age(now - 400 * 86_400 * 10_000_000).starts_with("1 year"));
    }

    #[test]
    fn the_clock_keeps_sub_second_precision() {
        // Two consecutive reads must not go backwards, and the clock must resolve below
        // one second — truncation once made every fresh file read as future-dated.
        let first = windows_ticks_now();
        let second = windows_ticks_now();
        assert!(second >= first);
        assert!(second % 10_000_000 != 0 || first % 10_000_000 != 0);
    }
}
