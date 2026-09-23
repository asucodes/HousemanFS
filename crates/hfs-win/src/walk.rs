//! Recursive directory walk.
//!
//! Produces the raw material for accounting: for every file, how many bytes it reports, how
//! many bytes it actually occupies, and how many names point at it.
//!
//! Three deliberate choices, each of which is a correctness decision rather than a style one:
//!
//! 1. **Reparse points are never followed.** A junction can point at an ancestor, at another
//!    volume, or at a path outside the requested scope. Following them causes infinite loops,
//!    double counting, and — in a tool that later acts — operations outside the intended
//!    target. They are recorded and skipped.
//! 2. **The traversal is iterative, not recursive.** A deep enough tree will overflow the
//!    stack in a recursive implementation, and "deep enough" is attacker-controlled.
//! 3. **Hardlinked files are not summed.** A file with more than one name does not own its
//!    bytes exclusively, so adding its allocation to the total would overcount. Those files
//!    are counted and their allocation reported separately, as an explicit upper bound.

use std::io;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileStandardInfo,
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    GetFileInformationByHandleEx, OPEN_EXISTING, WIN32_FIND_DATAW,
};

use crate::util::{from_wide, last_error, to_wide};

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
    /// Bytes the file actually occupies. Zero for directories in this slice.
    pub allocated: u64,
    /// How many names point at this file. Greater than one means it does not exclusively own
    /// its bytes.
    pub links: u32,
}

/// Totals gathered during a walk.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WalkSummary {
    pub files: u64,
    pub directories: u64,
    pub reparse_points: u64,
    /// Paths that could not be read. Recorded rather than swallowed: the accounting can only
    /// be as complete as the scan, and the user is entitled to know what was missed.
    pub denied: u64,

    /// Summed logical size of singly-linked files.
    pub logical_bytes: u64,
    /// Summed allocated size of singly-linked files.
    pub allocated_bytes: u64,
    /// Slack: bytes lost to rounding files up to whole clusters.
    pub slack_bytes: u64,

    /// Count of files with more than one name.
    pub shared_files: u64,
    /// Their summed allocation. An **upper bound**, not a reclaimable figure: two links to the
    /// same data both report the same allocation, and deleting one frees nothing.
    pub shared_allocated_upper_bound: u64,
}

/// The result of a walk.
#[derive(Debug, Clone)]
pub struct Walk {
    pub summary: WalkSummary,
    pub entries: Vec<ScannedEntry>,
}

/// Walk `root` and everything beneath it, without following reparse points.
pub fn walk(root: &str) -> io::Result<Walk> {
    let mut summary = WalkSummary::default();
    let mut entries = Vec::new();
    let mut pending: Vec<PathBuf> = vec![PathBuf::from(root)];

    while let Some(dir) = pending.pop() {
        match read_directory(&dir, &mut summary, &mut entries) {
            Ok(mut children) => pending.append(&mut children),
            // An unreadable directory is recorded and skipped. Aborting the whole scan because
            // one directory is protected would make the tool useless on a real system, and
            // silently ignoring it would make the numbers quietly wrong.
            Err(_) => summary.denied += 1,
        }
    }

    Ok(Walk { summary, entries })
}

/// Read one directory. Returns the subdirectories that should be descended into.
fn read_directory(
    dir: &Path,
    summary: &mut WalkSummary,
    entries: &mut Vec<ScannedEntry>,
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
                entries.push(ScannedEntry {
                    path,
                    kind: EntryKind::Reparse,
                    logical,
                    allocated: 0,
                    links: 0,
                });
            } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                summary.directories += 1;
                entries.push(ScannedEntry {
                    path: path.clone(),
                    kind: EntryKind::Directory,
                    logical: 0,
                    allocated: 0,
                    links: 0,
                });
                children.push(path);
            } else {
                record_file(&path, logical, summary, entries);
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

/// Record a file, querying the filesystem for what it actually occupies.
///
/// If the file cannot be opened — it is locked, or we lack permission — it is still counted,
/// using its reported size as a fallback, and the shortfall is visible in the summary rather
/// than hidden.
fn record_file(
    path: &Path,
    logical: u64,
    summary: &mut WalkSummary,
    entries: &mut Vec<ScannedEntry>,
) {
    summary.files += 1;

    let (allocated, links) = match file_standard_info(path) {
        Ok((allocated, _end_of_file, links)) => (allocated, links),
        Err(_) => {
            summary.denied += 1;
            (logical, 1)
        }
    };

    if links > 1 {
        summary.shared_files += 1;
        summary.shared_allocated_upper_bound += allocated;
    } else {
        summary.logical_bytes += logical;
        summary.allocated_bytes += allocated;
        summary.slack_bytes += allocated.saturating_sub(logical);
    }

    entries.push(ScannedEntry {
        path: path.to_path_buf(),
        kind: EntryKind::File,
        logical,
        allocated,
        links,
    });
}

/// Ask the filesystem for a file's allocation and link count.
///
/// Opened with `FILE_READ_ATTRIBUTES` only, never with read or write access, and always with
/// full sharing so that opening a file for inspection can never lock another process out.
/// `FILE_FLAG_OPEN_REPARSE_POINT` ensures we measure the link itself rather than whatever it
/// points at.
fn file_standard_info(path: &Path) -> io::Result<(u64, u64, u32)> {
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

    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };

    // SAFETY: `handle` is valid and open; the buffer matches the requested information class
    // and its true size is passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };

    // SAFETY: `handle` is valid and is closed exactly once, on both paths below.
    unsafe { CloseHandle(handle) };

    if ok == 0 {
        return Err(last_error());
    }

    Ok((
        info.AllocationSize.max(0) as u64,
        info.EndOfFile.max(0) as u64,
        info.NumberOfLinks,
    ))
}
