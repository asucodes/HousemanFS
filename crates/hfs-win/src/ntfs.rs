//! NTFS volume metadata, read through documented control codes.
//!
//! A directory walk cannot see the filesystem's own bookkeeping. On NTFS that is not a small
//! omission: the master file table alone is roughly a kilobyte per file, so a volume with a
//! million files carries about a gigabyte of it, and the USN change journal can be configured
//! to hundreds of megabytes more. Neither is visible as a file, and both occupy real space.
//!
//! These queries need a handle to the *volume*, which requires administrator rights. That is
//! why the tool reports its privilege level alongside its numbers: without elevation this
//! module returns nothing and the residual absorbs the difference.
//!
//! The control codes below are documented in `winioctl.h` and are computed from
//! `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, n, METHOD_BUFFERED, FILE_ANY_ACCESS)`. They are written
//! out rather than imported so that the encoding is visible and checkable.

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileStandardInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::util::{last_error, to_wide};

/// `FSCTL_GET_NTFS_VOLUME_DATA` — retrieves NTFS volume geometry and metadata sizes.
const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;

/// `FSCTL_QUERY_USN_JOURNAL` — retrieves the size and state of the change journal.
const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;

/// Mirrors `NTFS_VOLUME_DATA_BUFFER`.
///
/// `#[repr(C)]` with the fields in declaration order, matching the layout in `winioctl.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct NtfsVolumeDataBuffer {
    volume_serial_number: i64,
    number_sectors: i64,
    total_clusters: i64,
    free_clusters: i64,
    total_reserved: i64,
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
    bytes_per_file_record_segment: u32,
    clusters_per_file_record_segment: u32,
    mft_valid_data_length: i64,
    mft_start_lcn: i64,
    mft2_start_lcn: i64,
    mft_zone_start: i64,
    mft_zone_end: i64,
}

/// Mirrors `USN_JOURNAL_DATA_V0`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct UsnJournalDataV0 {
    usn_journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

/// The parts of NTFS bookkeeping that occupy space but are not files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtfsMetadata {
    /// Bytes the master file table currently occupies.
    ///
    /// This is where a file's record lives, and for small files it is where the file's *data*
    /// lives too — a resident file occupies no clusters at all. So this figure is not pure
    /// overhead: it is storage that a directory walk reports as zero.
    pub mft_bytes: u64,
    /// Bytes per file record, for context on the figure above.
    pub bytes_per_record: u32,
    /// Bytes reserved for the master file table's growth zone.
    ///
    /// Reserved, not occupied. Reported separately because conflating the two would overstate
    /// what is actually in use.
    pub mft_zone_bytes: u64,
    /// Maximum size the change journal may grow to.
    pub usn_journal_max_bytes: u64,
    /// Bytes per cluster, as the filesystem reports it rather than as we assumed.
    pub bytes_per_cluster: u32,
}

/// Query NTFS metadata for the volume containing `mount`.
///
/// `mount` is a path on the volume, e.g. `C:\`. Returns `None` when the volume is not NTFS, or
/// when the query is not permitted — which is the normal outcome without elevation.
pub fn ntfs_metadata(mount: &str) -> io::Result<Option<NtfsMetadata>> {
    let volume_path = volume_device_path(mount);
    let wide = to_wide(&volume_path);

    // A volume handle is what these control codes require. Opening one needs administrator
    // rights; without them this fails and the caller falls back to reporting the shortfall.
    //
    // SAFETY: `wide` is NUL-terminated and outlives the call; remaining arguments are constants
    // or documented-acceptable nulls.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            // Read access is required, not optional. A volume handle opened with no access
            // satisfies `CreateFileW` but the resulting handle is refused by `DeviceIoControl`,
            // so the query fails while appearing to have opened successfully.
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }

    let result = query_metadata(handle);

    // SAFETY: `handle` is valid and is closed exactly once.
    unsafe { CloseHandle(handle) };

    result
}

fn query_metadata(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<Option<NtfsMetadata>> {
    let mut volume: NtfsVolumeDataBuffer = Default::default();
    let mut returned: u32 = 0;

    // SAFETY: `handle` is a valid volume handle; the output buffer matches the control code's
    // documented output structure and its true size is passed.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            &mut volume as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<NtfsVolumeDataBuffer>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };

    if ok == 0 {
        // Not an NTFS volume, or not permitted. Both are ordinary outcomes rather than errors,
        // so the caller is told "no metadata available" instead of being given a failure.
        return Ok(None);
    }

    let mut journal: UsnJournalDataV0 = Default::default();
    let mut journal_returned: u32 = 0;

    // SAFETY: as above.
    let journal_ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut journal as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<UsnJournalDataV0>() as u32,
            &mut journal_returned,
            std::ptr::null_mut(),
        )
    };

    // A volume may legitimately have no change journal — it is not created at startup, and an
    // administrator can delete it. Absence is reported as zero rather than as an error.
    let journal_bytes = if journal_ok != 0 {
        journal.maximum_size
    } else {
        0
    };

    let zone_clusters = volume
        .mft_zone_end
        .saturating_sub(volume.mft_zone_start)
        .max(0) as u64;

    Ok(Some(NtfsMetadata {
        mft_bytes: volume.mft_valid_data_length.max(0) as u64,
        bytes_per_record: volume.bytes_per_file_record_segment,
        mft_zone_bytes: zone_clusters.saturating_mul(volume.bytes_per_cluster as u64),
        usn_journal_max_bytes: journal_bytes,
        bytes_per_cluster: volume.bytes_per_cluster,
    }))
}

/// Turn a mount path such as `C:\` into the device path the volume handle needs, `\\.\C:`.
///
/// The drive-letter form is used rather than the volume GUID form because it is what the
/// caller already has. A volume with no drive letter would need the GUID path, which is not
/// yet supported — and that limitation should be surfaced rather than guessed at.
fn volume_device_path(mount: &str) -> String {
    let trimmed = mount.trim_end_matches(['\\', '/']);
    let bytes = trimmed.as_bytes();

    if bytes.len() >= 2 && bytes[1] == b':' {
        let letter = trimmed[..2].to_string();
        return format!(r"\\.\{letter}");
    }

    // Fall back to the path as given. The open will fail for anything that is not a volume,
    // which is the correct outcome rather than a silent wrong answer.
    trimmed.to_string()
}

/// One of NTFS's own system files.
#[derive(Debug, Clone)]
pub struct SystemFile {
    pub name: String,
    /// Bytes actually occupied, or `None` if the file could not be measured.
    pub allocated: Option<u64>,
}

/// The NTFS system files, by their names in the root directory.
///
/// These are ordinary entries in the root directory index, but NTFS marks them as metadata and
/// **excludes them from directory enumeration**. A walk therefore never sees them, and their
/// space is invisible to it. `$MFT` is deliberately omitted here because it is measured through
/// `FSCTL_GET_NTFS_VOLUME_DATA` instead, and counting it twice would overstate the total.
const SYSTEM_FILES: [&str; 11] = [
    "$MFTMirr",
    "$LogFile",
    "$Bitmap",
    "$Boot",
    "$BadClus",
    "$Secure",
    "$UpCase",
    r"$Extend\$ObjId",
    r"$Extend\$Quota",
    r"$Extend\$Reparse",
    r"$Extend\$UsnJrnl",
];

/// Measure the NTFS system files that a directory walk cannot see.
///
/// Requires administrator rights, like every other volume-level query. Files that cannot be
/// opened are returned with `None` rather than being omitted, so the caller can report what
/// could not be measured instead of silently under-reporting.
pub fn system_files(mount: &str) -> Vec<SystemFile> {
    let root = mount.trim_end_matches(['\\', '/']);

    SYSTEM_FILES
        .iter()
        .map(|name| {
            let path = format!(r"{root}\{name}");
            SystemFile {
                name: name.to_string(),
                allocated: allocated_size(&path).ok(),
            }
        })
        .collect()
}

/// The allocation of `$MFT` itself.
///
/// Preferred over the value in `FSCTL_GET_NTFS_VOLUME_DATA`, which reports the *valid data
/// length*: the portion of the table holding live records. The table is allocated in larger
/// chunks than that, so the valid length understates what the table actually occupies.
pub fn mft_allocation(mount: &str) -> Option<u64> {
    let root = mount.trim_end_matches(['\\', '/']);
    allocated_size(&format!(r"{root}\$MFT")).ok()
}

/// Ask the filesystem how many bytes a path occupies, without reading any of it.
fn allocated_size(path: &str) -> io::Result<u64> {
    let wide = to_wide(path);

    // SAFETY: `wide` is NUL-terminated and outlives the call; remaining arguments are constants
    // or documented-acceptable nulls. `FILE_FLAG_BACKUP_SEMANTICS` is required to open entries
    // that are directories or metadata rather than ordinary files.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }

    let mut standard: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };

    // SAFETY: `handle` is valid and open; the buffer matches the requested information class
    // and its true size is passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            &mut standard as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };

    // SAFETY: `handle` is valid and closed exactly once, on every path below.
    unsafe { CloseHandle(handle) };

    if ok == 0 {
        return Err(last_error());
    }

    Ok(standard.AllocationSize.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_path_from_drive_letter() {
        assert_eq!(volume_device_path(r"C:\"), r"\\.\C:");
        assert_eq!(volume_device_path("D:"), r"\\.\D:");
        assert_eq!(volume_device_path("e:/"), r"\\.\e:");
    }
}
