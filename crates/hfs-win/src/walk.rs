//! Recursive directory walk.
//!
//! Produces the raw material for accounting: for every file, how many bytes it reports, how
//! many bytes it actually occupies, and how many names point at it.
//!
//! Four deliberate choices, each a correctness decision rather than a style one:
//!
//! 1. **Reparse points are never followed.** A junction can point at an ancestor, at another
//!    volume, or at a path outside the requested scope. Following them causes infinite loops,
//!    double counting, and — in a tool that later acts — operations outside the intended
//!    target. They are recorded and skipped.
//! 2. **The traversal is iterative, not recursive.** A deep enough tree will overflow the stack
//!    in a recursive implementation, and "deep enough" is attacker-controlled.
//! 3. **Hardlink groups are counted once, by file ID.** Several names for one set of data do
//!    not each occupy space. Summing per-file allocation would overcount, and reporting that
//!    overcount as reclaimable would be wrong in the optimistic direction — the failure this
//!    project exists to avoid. Identity comes from the file ID, never the path.
//! 4. **Unreadable paths are recorded, not swallowed.** The accounting can only be as complete
//!    as the scan, and the user is entitled to know what was missed.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileIdInfo,
    FileStandardInfo, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, GetFileInformationByHandleEx, OPEN_EXISTING, WIN32_FIND_DATAW,
};

use crate::util::{from_wide, last_error, to_wide};

/// How many unreadable directory paths to retain for reporting.
///
/// Enough to reveal the pattern — usually one or two system directories account for all of them
/// — without turning the report into a wall of paths on a badly permissioned volume.
const DENIED_SAMPLE_LIMIT: usize = 12;

/// What a directory entry is, from the scanner's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    /// A reparse point: junction, symlink, mount point, or a dedup/placeholder tag. Recorded,
    /// never followed.
    Reparse,
}

/// One file discovered during a walk.
#[derive(Debug, Clone)]
pub struct ScannedEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    /// Bytes the file reports.
    pub logical: u64,
    /// Bytes the file actually occupies.
    pub allocated: u64,
    /// How many names point at this file.
    pub links: u32,
}

/// Totals gathered during a walk.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WalkSummary {
    pub files: u64,
    pub directories: u64,
    pub reparse_points: u64,

    /// Directories that could not be opened, so their contents are entirely unknown.
    ///
    /// These are the dangerous ones. An unreadable *file* is a known quantity of unknown size;
    /// an unreadable *directory* hides everything beneath it, which can be arbitrarily large.
    /// Distinguishing the two is what turns "1,769 unreadable paths" into a diagnosis.
    pub denied_directories: u64,

    /// A sample of those directories, by path.
    ///
    /// Naming them is the difference between a number and something a user can act on. A count
    /// tells you the accounting is incomplete; the paths tell you *where* it is incomplete, and
    /// usually point straight at the cause. Capped, because a badly permissioned volume could
    /// have thousands and listing them all would bury the answer.
    pub denied_directory_samples: Vec<String>,
    /// Files that could not be opened.
    pub denied_files: u64,
    /// Reported size of those files: a lower bound on what they occupy, since the directory
    /// entry still tells us their logical size even when we cannot open them.
    pub denied_file_bytes: u64,

    /// Summed logical size, counting each hardlink group once.
    pub logical_bytes: u64,
    /// Physical bytes occupied by file data, counting each hardlink group once.
    pub allocated_bytes: u64,
    /// Bytes lost to rounding files up to whole clusters.
    pub slack_bytes: u64,

    /// Physical bytes occupied by directories themselves.
    ///
    /// A directory is not free. Its index blocks live outside the master file table and hold
    /// the names it contains, and a walk that lists names never sees them. Reported separately
    /// from file data because it is a different kind of occupancy: nothing can be reclaimed by
    /// removing it, but it is still part of where the space went.
    pub directory_bytes: u64,

    /// Files that have more than one name.
    pub hardlinked_files: u64,
    /// Distinct sets of data referenced by those files — how many times their storage was
    /// counted, which is once per group rather than once per name.
    pub hardlink_groups: u64,
    /// Additional names beyond the first, which contribute no storage.
    pub hardlink_extra_names: u64,

    /// Files whose identity could not be read, so a hardlink group involving them may have
    /// been counted more than once. Recorded so the totals can be qualified honestly rather
    /// than presented as exact.
    pub unknown_identity_files: u64,
}

/// Controls what a walk collects.
#[derive(Debug, Clone, Copy, Default)]
pub struct WalkOptions {
    /// Retain one record per entry. Off by default: a whole-volume scan would otherwise hold
    /// millions of records in memory for no benefit.
    pub collect_entries: bool,
}

/// The result of a walk.
#[derive(Debug, Clone)]
pub struct Walk {
    pub summary: WalkSummary,
    pub entries: Vec<ScannedEntry>,
}

/// Walk `root` and everything beneath it, without following reparse points.
pub fn walk(root: &str) -> io::Result<Walk> {
    walk_with(root, WalkOptions::default())
}

/// Walk with explicit options.
pub fn walk_with(root: &str, options: WalkOptions) -> io::Result<Walk> {
    let mut summary = WalkSummary::default();
    let mut entries = Vec::new();
    let mut pending: Vec<PathBuf> = vec![PathBuf::from(root)];

    // Identity of every hardlink group already counted. Bounded by the number of hardlinked
    // files rather than by the number of files, so it stays small on typical volumes.
    let mut counted_groups: HashSet<[u8; 16]> = HashSet::new();

    while let Some(dir) = pending.pop() {
        match read_directory(
            &dir,
            &options,
            &mut summary,
            &mut entries,
            &mut counted_groups,
        ) {
            Ok(mut children) => pending.append(&mut children),
            // An unreadable directory is recorded and skipped. Aborting the whole scan because
            // one directory is protected would make the tool useless on a real system, and
            // silently ignoring it would make the numbers quietly wrong.
            Err(_) => {
                summary.denied_directories += 1;
                if summary.denied_directory_samples.len() < DENIED_SAMPLE_LIMIT {
                    summary
                        .denied_directory_samples
                        .push(dir.display().to_string());
                }
            }
        }
    }

    Ok(Walk { summary, entries })
}

/// Read one directory. Returns the subdirectories that should be descended into.
fn read_directory(
    dir: &Path,
    options: &WalkOptions,
    summary: &mut WalkSummary,
    entries: &mut Vec<ScannedEntry>,
    counted_groups: &mut HashSet<[u8; 16]>,
) -> io::Result<Vec<PathBuf>> {
    let pattern = format!("{}\\*", dir.display());
    let wide = to_wide(&pattern);

    let mut find_data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };

    // SAFETY: `wide` is NUL-terminated and outlives the call; `find_data` is a valid, aligned,
    // initialised output buffer; the search filter is null, which the API permits.
    let handle = unsafe {
        FindFirstFileExW(
            wide.as_ptr(),
            FindExInfoBasic,
            &mut find_data as *mut _ as *mut core::ffi::c_void,
            FindExSearchNameMatch,
            std::ptr::null(),
            0,
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }

    let mut children = Vec::new();

    loop {
        let name = from_wide(&find_data.cFileName);
        let attributes = find_data.dwFileAttributes;

        // "." and ".." are present in every listing and would cause an immediate loop.
        if name != "." && name != ".." {
            let path = dir.join(&name);
            let logical =
                ((find_data.nFileSizeHigh as u64) << 32) | (find_data.nFileSizeLow as u64);

            if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                summary.reparse_points += 1;
                if options.collect_entries {
                    entries.push(ScannedEntry {
                        path,
                        kind: EntryKind::Reparse,
                        logical,
                        allocated: 0,
                        links: 0,
                    });
                }
            } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                summary.directories += 1;

                // Ask the directory what it occupies. Its index blocks are allocated outside
                // the master file table and hold the names it contains, so a walk that only
                // lists names never sees them. A failure here is not fatal: the directory is
                // still traversed, it simply contributes no measured overhead.
                let allocated = query_file(&path).map(|f| f.allocated).unwrap_or(0);
                summary.directory_bytes += allocated;

                if options.collect_entries {
                    entries.push(ScannedEntry {
                        path: path.clone(),
                        kind: EntryKind::Directory,
                        logical: 0,
                        allocated,
                        links: 0,
                    });
                }
                children.push(path);
            } else {
                record_file(&path, logical, options, summary, entries, counted_groups);
            }
        }

        // SAFETY: `handle` came from FindFirstFileExW and has not been closed.
        let more = unsafe { FindNextFileW(handle, &mut find_data) };
        if more == 0 {
            break;
        }
    }

    // SAFETY: `handle` is valid and closed exactly once.
    unsafe { FindClose(handle) };
    Ok(children)
}

/// Record a file, attributing its storage to the volume exactly once.
fn record_file(
    path: &Path,
    logical: u64,
    options: &WalkOptions,
    summary: &mut WalkSummary,
    entries: &mut Vec<ScannedEntry>,
    counted_groups: &mut HashSet<[u8; 16]>,
) {
    summary.files += 1;

    let (allocated, links, id) = match query_file(path) {
        Ok(facts) => (facts.allocated, facts.links, facts.id),
        Err(_) => {
            // Still counted, using the size the directory entry reported, so a locked or
            // protected file does not silently vanish from the totals. Its true allocation is
            // unknown, which is recorded rather than estimated.
            summary.denied_files += 1;
            summary.denied_file_bytes += logical;
            (logical, 1, None)
        }
    };

    let mut counted = true;

    if links > 1 {
        summary.hardlinked_files += 1;
        match id {
            Some(id) => {
                if counted_groups.insert(id) {
                    summary.hardlink_groups += 1;
                } else {
                    // Another name for data already counted. It occupies no additional space.
                    summary.hardlink_extra_names += 1;
                    counted = false;
                }
            }
            None => {
                // Identity unavailable, so we cannot tell whether this is a new group. Count
                // it and record the uncertainty rather than presenting the total as exact.
                summary.unknown_identity_files += 1;
            }
        }
    }

    if counted {
        summary.logical_bytes += logical;
        summary.allocated_bytes += allocated;
        summary.slack_bytes += allocated.saturating_sub(logical);
    }

    if options.collect_entries {
        entries.push(ScannedEntry {
            path: path.to_path_buf(),
            kind: EntryKind::File,
            logical,
            allocated,
            links,
        });
    }
}

/// What the filesystem reports about one file.
struct FileFacts {
    allocated: u64,
    links: u32,
    /// Stable identity of the underlying data, used to detect that two names refer to the same
    /// bytes.
    id: Option<[u8; 16]>,
}

/// Ask the filesystem for a file's allocation, link count and identity.
///
/// Opened with `FILE_READ_ATTRIBUTES` only, never with read or write access, and always with
/// full sharing so that inspecting a file can never lock another process out.
/// `FILE_FLAG_OPEN_REPARSE_POINT` ensures the link itself is measured rather than its target.
fn query_file(path: &Path) -> io::Result<FileFacts> {
    let wide = to_wide(&path.display().to_string());

    // SAFETY: `wide` is NUL-terminated and outlives the call; the remaining arguments are
    // constants or documented-acceptable nulls.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }

    let mut standard: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let mut id_info: FILE_ID_INFO = unsafe { std::mem::zeroed() };

    // SAFETY: `handle` is valid and open; each buffer matches the requested information class
    // and its true size is passed.
    let standard_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            &mut standard as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };

    // Identity is a separate query. A failure here is not fatal — the file is still counted, it
    // simply cannot participate in hardlink grouping.
    // SAFETY: as above.
    let id_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut id_info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };

    // SAFETY: `handle` is valid and is closed exactly once, on every path below.
    unsafe { CloseHandle(handle) };

    if standard_ok == 0 {
        return Err(last_error());
    }

    Ok(FileFacts {
        allocated: standard.AllocationSize.max(0) as u64,
        links: standard.NumberOfLinks,
        id: if id_ok != 0 {
            Some(id_info.FileId.Identifier)
        } else {
            None
        },
    })
}
