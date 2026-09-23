//! Volume-level facts, read through documented Windows APIs.
//!
//! These are the numbers the accounting is reconciled against: capacity and free space come
//! from the filesystem itself, and cluster size determines how much space a file wastes
//! rounding up to a whole allocation unit.

use std::io;

use hfs_core::VolumeId;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetVolumeInformationW,
};

use crate::util::{from_wide, last_error, to_wide};

/// What Windows reports about a mounted volume.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// The path this was queried with, e.g. `C:\`.
    pub root: String,
    /// Filesystem name as reported, e.g. `NTFS`.
    pub filesystem: String,
    /// Volume serial number.
    pub serial: u32,
    /// Capacity in bytes.
    pub total_bytes: u64,
    /// Free bytes on the volume.
    pub free_bytes: u64,
    /// Free bytes available to the calling user, which differs from [`Self::free_bytes`] when
    /// quotas are in force. Reported separately rather than conflated, because using the wrong
    /// one silently misstates free space on a quota-managed machine.
    pub available_bytes: u64,
    /// Bytes per allocation unit. Not assumed to be 4096: ReFS and some NTFS volumes use 64K,
    /// and slack is computed from this.
    pub cluster_bytes: u32,
    /// Stable identifier derived from observed properties rather than from a drive letter.
    pub volume_id: VolumeId,
}

/// Query a volume by any path on it.
pub fn volume_info(root: &str) -> io::Result<VolumeInfo> {
    let wide = to_wide(root);

    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;

    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call, and the three
    // out-parameters are valid, aligned, initialised locals.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };
    if ok == 0 {
        return Err(last_error());
    }

    let mut fs_name = [0u16; 64];
    let mut serial: u32 = 0;
    let mut fs_flags: u32 = 0;

    // SAFETY: as above. The name buffer is passed with its true element count, and the null
    // pointers are for optional outputs the API documents as acceptable to omit.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            &mut fs_flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }

    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;

    // SAFETY: as above; out-parameters are valid locals.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }

    let cluster_bytes = sectors_per_cluster.saturating_mul(bytes_per_sector);
    let filesystem = from_wide(&fs_name);

    Ok(VolumeInfo {
        root: root.to_string(),
        volume_id: derive_volume_id(serial, cluster_bytes, total),
        filesystem,
        serial,
        total_bytes: total,
        free_bytes: free,
        available_bytes: available,
        cluster_bytes,
    })
}

/// Build a stable 16-byte identifier from properties that survive a remount.
///
/// Deliberately not derived from the drive letter, which is reassigned, and deliberately not
/// a cryptographic hash yet: this only needs to be stable and collision-resistant across the
/// handful of volumes on one machine. If the index ever becomes portable between machines this
/// should be revisited.
fn derive_volume_id(serial: u32, cluster_bytes: u32, total_bytes: u64) -> VolumeId {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&serial.to_le_bytes());
    bytes[4..8].copy_from_slice(&cluster_bytes.to_le_bytes());
    bytes[8..16].copy_from_slice(&total_bytes.to_le_bytes());
    VolumeId::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_id_is_deterministic_and_property_derived() {
        let a = derive_volume_id(0x1234_5678, 4096, 500_000_000_000);
        let b = derive_volume_id(0x1234_5678, 4096, 500_000_000_000);
        assert_eq!(a, b);
    }

    #[test]
    fn different_properties_produce_different_ids() {
        let a = derive_volume_id(1, 4096, 100);
        let b = derive_volume_id(2, 4096, 100);
        let c = derive_volume_id(1, 65536, 100);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn wide_conversion_round_trips_and_terminates() {
        let wide = to_wide("C:\\");
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(from_wide(&wide), "C:\\");
    }
}
