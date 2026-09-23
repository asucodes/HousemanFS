//! File and volume identity.
//!
//! The rule that governs this module: **identity is the file ID, never the path.**
//!
//! Paths are not stable. A rename or a move is among the most common operations on a
//! filesystem, and any index keyed on paths loses every derived fact about a file the moment
//! it moves — including any content hash, because the hash would have to be recomputed.
//! Keying on the file ID makes a move free.
//!
//! Note that platform-level "reference numbers" (such as NTFS MFT record numbers) are *not*
//! used here. They are recycled after deletion and their layout shifts as records move between
//! resident and non-resident storage, so they are not durable identity.

use std::fmt;

/// A volume, identified in a way that survives remounting.
///
/// Derived from observed properties rather than from a mount point or drive letter, because
/// drive letters are reassigned and mount points are not portable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VolumeId([u8; 16]);

impl VolumeId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A file within a volume.
///
/// The bytes are platform-defined and opaque here: this crate must not interpret them, because
/// what a file ID means differs between filesystems and between platforms. It only needs to be
/// stable for the lifetime of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId([u8; 16]);

impl FileId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A fully-qualified reference to one file. This is the primary key of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub volume: VolumeId,
    pub file: FileId,
}

impl FileKey {
    pub const fn new(volume: VolumeId, file: FileId) -> Self {
        Self { volume, file }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_keyed_on_id_not_path() {
        let volume = VolumeId::from_bytes([1; 16]);
        let file = FileId::from_bytes([2; 16]);
        let key = FileKey::new(volume, file);

        // A move changes the name and the parent, neither of which is part of identity.
        assert_eq!(key, FileKey::new(volume, file));
        assert_ne!(key.file, FileId::from_bytes([3; 16]));
    }

    #[test]
    fn same_file_id_on_different_volumes_is_a_different_key() {
        let file = FileId::from_bytes([7; 16]);
        let a = FileKey::new(VolumeId::from_bytes([1; 16]), file);
        let b = FileKey::new(VolumeId::from_bytes([2; 16]), file);

        assert_ne!(a, b);
    }
}
