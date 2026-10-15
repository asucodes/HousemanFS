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

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_STREAM_INFO, FileIdInfo,
    FileStandardInfo, FileStreamInfo, FindClose, FindExInfoBasic, FindExSearchNameMatch,
    FindFirstFileExW, FindNextFileW, GetFileInformationByHandleEx, OPEN_EXISTING,
    WIN32_FIND_DATAW,
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

/// One entry discovered during a walk.
#[derive(Debug, Clone)]
pub struct ScannedEntry {
    pub path: PathBuf,
    pub kind: EntryKind,

    /// Stable identity of the underlying data, when the query succeeded.
    ///
    /// Two names for one file share this value, which is how a hardlink group is recognised
    /// and why the index can count how many names point at the same bytes. `None` means the
    /// query was refused, which is different from "this entry has no identity" — a caller needs
    /// to tell those apart rather than treating both as unknown.
    pub id: Option<[u8; 16]>,
    /// Identity of the containing directory, so the index can rebuild the tree.
    pub parent: Option<[u8; 16]>,

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

    /// Physical bytes occupied by named alternate data streams, counting each hardlink group
    /// once.
    ///
    /// The allocation reported for a file is the allocation of its unnamed `::$DATA` stream.
    /// A named stream is separate storage with its own extents: it is invisible to directory
    /// enumeration, invisible to the standard allocation query, and a common source of
    /// apparently missing space. Measured per stream on allocation size, because a sparse
    /// stream's clusters — not its logical length — are what occupy the volume.
    pub stream_bytes: u64,
    /// Files carrying at least one named stream.
    pub stream_files: u64,
    /// Total named streams found, across files and directories.
    pub stream_count: u64,
    /// Files that opened but whose streams could not be enumerated, so their stream
    /// occupancy is unknown rather than zero. Zero is a measurement; this is the absence of
    /// one, and the two must never be conflated.
    pub stream_unknown_files: u64,

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
    // The root's own identity becomes the parent of everything directly beneath it.
    let root_id = query_file(Path::new(root)).ok().and_then(|f| f.id);
    let mut pending: Vec<(PathBuf, Option<[u8; 16]>)> = vec![(PathBuf::from(root), root_id)];

    // Identity of every hardlink group already counted. Bounded by the number of hardlinked
    // files rather than by the number of files, so it stays small on typical volumes.
    let mut counted_groups: HashSet<[u8; 16]> = HashSet::new();

    while let Some((dir, id)) = pending.pop() {
        match read_directory(
            &dir,
            id,
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
    parent: Option<[u8; 16]>,
    options: &WalkOptions,
    summary: &mut WalkSummary,
    entries: &mut Vec<ScannedEntry>,
    counted_groups: &mut HashSet<[u8; 16]>,
) -> io::Result<Vec<(PathBuf, Option<[u8; 16]>)>> {
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
                        id: None,
                        parent,
                        logical,
                        allocated: 0,
                        links: 0,
                    });
                }
            } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                summary.directories += 1;

                // Ask the directory what it occupies and who it is. Its index blocks are
                // allocated outside the master file table and hold the names it contains, so a
                // walk that only lists names never sees them. Its identity is needed because
                // it becomes the parent of everything beneath it. A failure is not fatal: the
                // directory is still traversed, it simply contributes no measured overhead and
                // its children are recorded with no parent.
                let queried = query_file(&path).ok();
                let allocated = queried.as_ref().map(|f| f.allocated).unwrap_or(0);
                let id = queried.as_ref().and_then(|f| f.id);
                summary.directory_bytes += allocated;

                // A directory can carry named streams too. Rare, but the whole point of this
                // field is that it would otherwise be invisible: the handle is already open,
                // so the enumeration is free.
                if let Some(facts) = &queried {
                    if let Some(streams) = &facts.streams {
                        summary.stream_bytes += streams.bytes;
                        summary.stream_count += streams.count as u64;
                    }
                }

                if options.collect_entries {
                    entries.push(ScannedEntry {
                        path: path.clone(),
                        kind: EntryKind::Directory,
                        id,
                        parent,
                        logical: 0,
                        allocated,
                        links: 0,
                    });
                }
                children.push((path, id));
            } else {
                record_file(
                    &path,
                    logical,
                    parent,
                    options,
                    summary,
                    entries,
                    counted_groups,
                );
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
    parent: Option<[u8; 16]>,
    options: &WalkOptions,
    summary: &mut WalkSummary,
    entries: &mut Vec<ScannedEntry>,
    counted_groups: &mut HashSet<[u8; 16]>,
) {
    summary.files += 1;

    let (allocated, links, id, streams) = match query_file(path) {
        Ok(facts) => (
            facts.allocated,
            facts.links,
            facts.id,
            facts.streams,
        ),
        Err(_) => {
            // Still counted, using the size the directory entry reported, so a locked or
            // protected file does not silently vanish from the totals. Its true allocation is
            // unknown, which is recorded rather than estimated. Its streams are equally
            // unknown, but the file is already recorded as unreadable; counting it twice in
            // two unknowns would misstate one problem as two.
            summary.denied_files += 1;
            summary.denied_file_bytes += logical;
            (logical, 1, None, None)
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

        match streams {
            Some(facts) => {
                summary.stream_bytes += facts.bytes;
                summary.stream_count += facts.count as u64;
                if facts.count > 0 {
                    summary.stream_files += 1;
                }
            }
            None => {
                // The file opened, so its main allocation is measured, but its streams could
                // not be enumerated. Recorded so that a systematic failure to enumerate —
                // rather than an absence of streams — is visible in the report.
                summary.stream_unknown_files += 1;
            }
        }
    }

    if options.collect_entries {
        entries.push(ScannedEntry {
            path: path.to_path_buf(),
            kind: EntryKind::File,
            id,
            parent,
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
    /// The file's named streams, or `None` when they could not be enumerated. `None` is not
    /// zero: zero named streams is a measurement, and a failed query is the absence of one.
    streams: Option<StreamFacts>,
}

/// The named streams one file carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamFacts {
    /// Allocated bytes across every named stream.
    bytes: u64,
    /// How many named streams there are.
    count: u32,
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

    // Identity and streams are separate queries; either failing is not fatal. The file is
    // still counted — it simply cannot participate in hardlink grouping, or its stream
    // occupancy is recorded as unknown rather than zero.
    // SAFETY: `handle` is valid and open; each buffer matches the requested information class
    // and its true size is passed.
    let id_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut id_info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };

    // SAFETY: `handle` is still valid and open; the buffer is grown by the callee as needed.
    let streams = query_streams(handle);

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
        streams,
    })
}

/// Starting buffer for a stream enumeration, in bytes. Room for dozens of entries, which
/// covers every real file; larger answers grow the buffer instead of being truncated.
const STREAM_BUFFER_BYTES: usize = 4096;

/// Ceiling for that growth. A file with more named streams than this is pathological, and a
/// bounded loop means a hostile file cannot make a scan allocate without limit.
const STREAM_BUFFER_LIMIT: usize = 1024 * 1024;

/// Enumerate a file's named streams on an already-open handle.
///
/// The unnamed `::$DATA` stream is skipped: its allocation is what `FileStandardInfo` already
/// reported, and counting it again would overstate the volume. Every other stream is measured
/// on allocation size, not logical size, because a sparse stream's clusters are what occupy
/// the volume — the same rule the main allocation follows.
///
/// Returns `None` when the enumeration could not be performed, which the caller records as
/// unknown. A successful enumeration that finds nothing is `Some` with a zero count, and the
/// difference is the whole point of the shape.
fn query_streams(handle: HANDLE) -> Option<StreamFacts> {
    // The entries are read through a pointer to a struct containing `i64`, so the backing
    // store must be 8-byte aligned. A `u64` buffer guarantees that; a `u8` buffer would not.
    let mut buffer = vec![0u64; STREAM_BUFFER_BYTES / std::mem::size_of::<u64>()];

    loop {
        // SAFETY: `handle` is valid and open; the buffer is 8-byte aligned and its true size
        // is passed. The API fills only what it fills.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStreamInfo,
                buffer.as_mut_ptr() as *mut core::ffi::c_void,
                (buffer.len() * std::mem::size_of::<u64>()) as u32,
            )
        };

        if ok != 0 {
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    buffer.as_ptr() as *const u8,
                    buffer.len() * std::mem::size_of::<u64>(),
                )
            };
            return parse_streams(bytes);
        }

        // A truncated answer is reported with `ERROR_MORE_DATA`; the API does not say how
        // much room it wanted, so the buffer doubles instead. Anything else is a failure to
        // query, which is recorded as unknown.
        if last_error().raw_os_error() == Some(ERROR_MORE_DATA as i32)
            && buffer.len() * std::mem::size_of::<u64>() < STREAM_BUFFER_LIMIT
        {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        return None;
    }
}

/// Parse a `FILE_STREAMS_INFO` buffer into the named streams it describes.
fn parse_streams(buffer: &[u8]) -> Option<StreamFacts> {
    // Fixed part of FILE_STREAM_INFO: two u32 counts and two i64 sizes, before the name.
    // Spelled as an offset rather than computed from the struct's size, which includes tail
    // padding that the entries do not carry.
    const FIXED: usize = std::mem::offset_of!(FILE_STREAM_INFO, StreamName);

    let mut bytes = 0u64;
    let mut count = 0u32;
    let mut offset = 0usize;

    loop {
        if offset + FIXED > buffer.len() {
            // Either the end of the list or a truncated buffer; both mean enumeration is
            // untrustworthy, and a partial count must not look like a full one.
            return None;
        }

        // SAFETY: `buffer` is 8-byte aligned (see the caller), `offset` advances by
        // `NextEntryOffset`, which the API guarantees to be 8-byte aligned, and the fixed
        // part fits within the buffer.
        let entry =
            unsafe { &*(buffer.as_ptr().add(offset) as *const FILE_STREAM_INFO) };

        let name_end = offset + FIXED + entry.StreamNameLength as usize;
        let name = if entry.StreamNameLength == 0 {
            String::new()
        } else if name_end <= buffer.len() {
            let wide = unsafe {
                core::slice::from_raw_parts(
                    buffer.as_ptr().add(offset + FIXED) as *const u16,
                    entry.StreamNameLength as usize / 2,
                )
            };
            // Lossy for the same reason as everywhere else: a stream name is not guaranteed
            // to be valid UTF-16, and a scan must not stop for one.
            String::from_utf16_lossy(wide)
        } else {
            return None;
        };

        if !name.is_empty() && name != "::$DATA" {
            bytes += entry.StreamAllocationSize.max(0) as u64;
            count += 1;
        }

        if entry.NextEntryOffset == 0 {
            return Some(StreamFacts { bytes, count });
        }
        offset += entry.NextEntryOffset as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::volume_info;
    use std::io::Write as _;

    /// Serialise one `FILE_STREAM_INFO` entry into `buf` at byte `at`, the way the API packs
    /// them: fixed part, name, with `next` carrying the reader to the next entry.
    fn write_entry(buf: &mut [u8], at: usize, next: u32, name: &str, alloc: i64) {
        let name_bytes: Vec<u8> = name
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();

        buf[at..at + 4].copy_from_slice(&next.to_le_bytes());
        buf[at + 4..at + 8].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&alloc.to_le_bytes());
        buf[at + 16..at + 24].copy_from_slice(&alloc.to_le_bytes());
        buf[at + 24..at + 24 + name_bytes.len()].copy_from_slice(&name_bytes);
    }

    /// A mutable byte view of an 8-byte-aligned buffer, because `parse_streams` reads entries
    /// through a pointer to a struct containing `i64`.
    fn bytes_mut(backing: &mut [u64]) -> &mut [u8] {
        // SAFETY: the view stays within the allocation and the alignment is guaranteed by
        // the `u64` element type.
        unsafe {
            core::slice::from_raw_parts_mut(
                backing.as_mut_ptr() as *mut u8,
                backing.len() * std::mem::size_of::<u64>(),
            )
        }
    }

    /// The immutable counterpart.
    fn bytes(backing: &[u64]) -> &[u8] {
        // SAFETY: as above, read-only.
        unsafe {
            core::slice::from_raw_parts(
                backing.as_ptr() as *const u8,
                backing.len() * std::mem::size_of::<u64>(),
            )
        }
    }

    #[test]
    fn parse_skips_the_unnamed_stream_and_sums_the_named_ones() {
        let mut backing = vec![0u64; 64];
        {
            let raw = bytes_mut(&mut backing);
            write_entry(raw, 0, 48, "::$DATA", 4096);
            write_entry(raw, 48, 0, ":zone.identifier", 8192);
        }

        let facts = parse_streams(bytes(&backing)).expect("a well-formed buffer parses");
        assert_eq!(facts.count, 1, "the unnamed stream is not a named stream");
        assert_eq!(facts.bytes, 8192, "only the named stream's allocation");
    }

    #[test]
    fn parse_reports_a_file_without_named_streams_as_zero() {
        let mut backing = vec![0u64; 64];
        write_entry(bytes_mut(&mut backing), 0, 0, "::$DATA", 4096);

        let facts = parse_streams(bytes(&backing)).expect("a well-formed buffer parses");
        assert_eq!(facts.count, 0);
        assert_eq!(facts.bytes, 0);
    }

    #[test]
    fn parse_treats_a_truncated_buffer_as_unknown() {
        let mut backing = vec![0u64; 64];
        {
            let raw = bytes_mut(&mut backing);
            write_entry(raw, 0, 48, "::$DATA", 4096);
            write_entry(raw, 48, 0, ":zone.identifier", 8192);
        }

        // Cut inside the second entry's name. A partial count must not look like a full one,
        // so a truncated buffer is the absence of a measurement, not a smaller number.
        let facts = parse_streams(&bytes(&backing)[..40]);
        assert!(facts.is_none(), "a truncated buffer must not parse");
    }

    #[test]
    fn a_named_stream_is_measured_on_a_real_file() {
        let dir = std::env::temp_dir();
        let name = format!("hfs-streams-test-{}", std::process::id());
        let file = dir.join(&name);
        let stream_path = dir.join(format!("{name}:probe"));

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&stream_path);

        // Main data large enough to be non-resident, so its allocation is measurable in
        // clusters rather than hidden inside the master file table record.
        std::fs::write(&file, &[0u8; 10_000]).expect("scratch file");

        // A named stream is created by opening a path that names one. On a volume without
        // alternate streams — anything non-NTFS — there is nothing to test here.
        let made = std::fs::File::create(&stream_path);
        let Ok(mut stream) = made else {
            let _ = std::fs::remove_file(&file);
            return;
        };
        stream.write_all(&[0u8; 5_000]).expect("stream data");
        drop(stream);

        let facts = query_file(&file).expect("the scratch file opens");
        let streams = facts
            .streams
            .expect("stream enumeration on a file this process just wrote");

        // The stream's allocation is its logical size rounded up to whole clusters, the same
        // rule the main data follows. A logical-size figure would understate it; a size not
        // cluster-aligned would mean the measurement is not an allocation at all.
        let cluster = volume_info(&dir.display().to_string())
            .map(|v| v.cluster_bytes as u64)
            .unwrap_or(4096);
        let expected = (5_000 + cluster - 1) / cluster * cluster;

        assert_eq!(streams.count, 1, "the probe stream is the only named one");
        assert_eq!(streams.bytes, expected, "allocation, rounded to clusters");

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&stream_path);
    }
}
