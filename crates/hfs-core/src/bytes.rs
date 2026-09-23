//! Storage accounting for individual files.
//!
//! The type here that matters is [`Ownership`]. It exists because the most common way for a
//! disk tool to be wrong is to assume that every file owns its own bytes.
//!
//! Several mechanisms break that assumption:
//!
//! - **Hard links** give one set of data several names. Deleting one name frees nothing; the
//!   data survives until the last name goes.
//! - **Deduplication and block cloning** make unrelated files share the same extents. Again,
//!   deleting one frees nothing, because the other still references the data.
//! - **Sparse files** report a large logical size while occupying far less.
//! - **Compression** makes allocated size smaller than logical size.
//!
//! A tool that ignores these reports reclaimable space that does not exist. Since the error is
//! always in the optimistic direction, it is the exact failure this project exists to avoid.
//!
//! The remedy is a type that refuses to answer when the answer is not determinable:
//! [`Ownership::reclaimable_bytes`] returns `None` rather than a plausible guess.

/// How a file's bytes relate to the bytes of every other file on the volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// This file exclusively owns its extents. Deleting it frees its allocated size.
    Unique,

    /// The extents are reachable from somewhere else as well.
    ///
    /// `sharers` is the number of names or files that reference the same data. Deleting one of
    /// them frees nothing, because the others still hold a reference. There is no safe way to
    /// convert this into a reclaimable figure without knowing *which* holder will be deleted
    /// last, which we cannot know in advance.
    Shared { sharers: u32 },

    /// Ownership could not be determined.
    ///
    /// This is not a fallback to be treated as `Unique`. It is a distinct, meaningful state
    /// that must be surfaced to the user rather than rounded off into a number.
    Unknown,
}

impl Ownership {
    /// How many bytes deleting this file would free, if that can be determined.
    ///
    /// Returns `None` when the answer depends on state we do not have. Callers must treat
    /// `None` as "unknown", never as zero and never as `size.allocated`.
    pub fn reclaimable_bytes(&self, size: ExtentBytes) -> Option<u64> {
        match self {
            Self::Unique => Some(size.allocated),
            // Deleting one holder of shared data frees nothing that can be attributed to this
            // deletion in particular.
            Self::Shared { .. } | Self::Unknown => None,
        }
    }
}

/// A file's size, measured two ways.
///
/// `logical` is the size the file reports: how many bytes you would read. `allocated` is what
/// it actually occupies, which is what matters for reclaiming space and for telling the user
/// where their disk went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentBytes {
    /// Bytes readable from the file.
    pub logical: u64,
    /// Bytes actually occupied on the volume.
    pub allocated: u64,
}

impl ExtentBytes {
    pub const fn new(logical: u64, allocated: u64) -> Self {
        Self { logical, allocated }
    }

    /// Bytes wasted rounding the file up to whole allocation units.
    ///
    /// Saturating, because a compressed file occupies less than its logical size and there is
    /// then no slack at all.
    pub const fn slack(&self) -> u64 {
        self.allocated.saturating_sub(self.logical)
    }

    /// True when the file occupies less than it reports, which means it is compressed or
    /// sparse. Such a file's logical size must never be used for a reclaim estimate.
    pub const fn is_compressed_or_sparse(&self) -> bool {
        self.allocated < self.logical
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    #[test]
    fn slack_is_the_gap_between_logical_and_allocated() {
        let size = ExtentBytes::new(5_000, 8_192);
        assert_eq!(size.slack(), 3_192);
    }

    #[test]
    fn compressed_files_have_no_slack() {
        // A compressed file occupies less than it reports, so slack must not go negative.
        let size = ExtentBytes::new(10 * GB, 3 * GB);
        assert_eq!(size.slack(), 0);
        assert!(size.is_compressed_or_sparse());
    }

    #[test]
    fn a_sparse_file_is_not_its_logical_size() {
        // A 100 GB sparse VM image occupying 2 GB must never be ranked by logical size.
        let size = ExtentBytes::new(100 * GB, 2 * GB);
        assert!(size.is_compressed_or_sparse());
    }

    #[test]
    fn only_uniquely_owned_bytes_are_reclaimable() {
        let size = ExtentBytes::new(4 * GB, 4 * GB);

        assert_eq!(Ownership::Unique.reclaimable_bytes(size), Some(4 * GB));
        assert_eq!(
            Ownership::Shared { sharers: 2 }.reclaimable_bytes(size),
            None,
            "deleting one hardlink frees nothing"
        );
        assert_eq!(Ownership::Unknown.reclaimable_bytes(size), None);
    }

    /// The central correctness property of the whole project.
    ///
    /// Three files that look like 30 GB of duplicates, but which share one copy of the data,
    /// are worth 10 GB reclaimable at most. A naive tool reports 30. This test is the reason
    /// [`Ownership`] exists.
    #[test]
    fn shared_extents_are_never_counted_as_reclaimable() {
        let size = ExtentBytes::new(10 * GB, 10 * GB);
        let files = [
            Ownership::Unique,
            Ownership::Shared { sharers: 3 },
            Ownership::Shared { sharers: 3 },
        ];

        let naive: u64 = files.iter().map(|_| size.allocated).sum();
        let honest: u64 = files
            .iter()
            .filter_map(|ownership| ownership.reclaimable_bytes(size))
            .sum();

        assert_eq!(naive, 30 * GB);
        assert_eq!(honest, 10 * GB);
    }

    #[test]
    fn unknown_is_not_treated_as_zero_or_as_allocated() {
        let size = ExtentBytes::new(2 * GB, 2 * GB);
        let result = Ownership::Unknown.reclaimable_bytes(size);

        assert_ne!(
            result,
            Some(0),
            "unknown is not the same as freeing nothing"
        );
        assert_ne!(result, Some(2 * GB), "unknown is not the same as unique");
        assert_eq!(result, None);
    }
}
