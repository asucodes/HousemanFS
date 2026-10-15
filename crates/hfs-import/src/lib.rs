//! Walk-to-index conversion, shared by every frontend.
//!
//! A scan takes minutes; the conversion from entries to index rows is where per-entry
//! meaning is assigned — provenance codes, derived identities, the synthesised root row.
//! Two frontends (CLI, desktop) must agree on every one of those decisions, or the same
//! volume would index differently depending on which button started the scan. So the
//! conversion lives here, once, and frontends only walk, store and report.
//!
//! What stays frontend-side: progress display, cancellation buttons, privilege messages,
//! and choosing between a full build and a refresh. What lives here: everything that
//! decides what a row means.
//!
//! No operating-system calls beyond what `hfs-win` already made during the walk, and no
//! output. This crate converts; it does not print.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;

use hfs_core::FileProvenance;
use hfs_index::{DirRow, FileRow, ScanMetadata, extension_of};
use hfs_win::{
    BackupPrivilege, EntryKind, WalkControl, WalkOptions, WalkSummary, enable_backup_privilege,
    is_elevated, path_facts, volume_info, walk_with_control,
};

/// One walk, converted to index rows and scan facts.
pub struct ConvertedScan {
    pub dirs: Vec<DirRow>,
    pub files: Vec<FileRow>,
    pub meta: ScanMetadata,
    pub summary: WalkSummary,
    pub backup: BackupPrivilege,
}

/// Walk `path` and convert the entries to index rows.
///
/// The backup privilege is requested first, so protected files are measured rather than
/// estimated from what the directory entry claims. Progress and cancellation flow through
/// `control`; a cancelled walk still converts, because partial results marked as partial
/// beat no results — but storing them is the caller's decision, and the `cancelled` flag
/// on the walk travels inside `summary` only insofar as the caller checks it.
pub fn scan_path(path: &str, control: &mut WalkControl) -> Result<ConvertedScan, String> {
    let backup = enable_backup_privilege();
    let volume = volume_info(path).map_err(|e| e.to_string())?;

    let result = walk_with_control(
        path,
        WalkOptions {
            collect_entries: true,
        },
        control,
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

    Ok(ConvertedScan {
        dirs,
        files,
        meta,
        summary: result.summary,
        backup,
    })
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
