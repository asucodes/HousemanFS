//! Volume-level accounting and reconciliation.
//!
//! The output of a scan is not a file listing. It is an explanation of where the bytes went,
//! together with an honest account of the bytes that could not be explained.
//!
//! That second half is what makes the difference. A tool that reports a 40 GB unexplained gap
//! as though the volume were simply 40 GB of files is not trustworthy. One that says *"11 GB
//! unaccounted: 6 GB estimated system metadata, 5 GB across 1,284 paths we were denied access
//! to"* is telling the truth, and the truth is checkable.
//!
//! The residual is therefore a first-class output, not an internal diagnostic.

/// Paths the scanner could not read.
///
/// This is recorded because it bounds how much the accounting can possibly be trusted. If a
/// thousand paths were unreadable, some unknown number of bytes is missing from `file_bytes`,
/// and any claim about reclaimable space is correspondingly weaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied {
    pub paths: u64,
}

impl Denied {
    pub const fn new(paths: u64) -> Self {
        Self { paths }
    }

    pub const fn any(&self) -> bool {
        self.paths > 0
    }
}

/// The byte-level explanation of one volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accounting {
    /// Capacity reported by the filesystem.
    pub total: u64,
    /// Free space reported by the filesystem.
    pub free: u64,

    /// Physical bytes occupied by file data.
    ///
    /// **Shared extents must be counted once, not once per holder.** A hardlink group or a
    /// deduplicated set contributes its data a single time. Getting this wrong is the classic
    /// double-count that makes every disk tool overreport.
    pub file_bytes: u64,

    /// Bytes occupied by alternate data streams, which directory enumeration does not reveal
    /// and which are a common source of apparently missing space.
    pub stream_bytes: u64,

    /// Bytes occupied by directory metadata.
    pub directory_bytes: u64,

    /// Filesystem metadata whose size is known: reserved areas, journals, bitmaps.
    ///
    /// `None` means not measurable on this filesystem, which is different from zero and must
    /// be reported differently.
    pub known_metadata: Option<u64>,

    pub denied: Denied,
}

impl Accounting {
    pub const fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Bytes we can point at and explain.
    pub const fn attributed(&self) -> u64 {
        self.file_bytes
            .saturating_add(self.stream_bytes)
            .saturating_add(self.directory_bytes)
            .saturating_add(match self.known_metadata {
                Some(bytes) => bytes,
                None => 0,
            })
    }

    /// Bytes in use that we cannot account for.
    pub const fn residual(&self) -> u64 {
        self.used().saturating_sub(self.attributed())
    }

    /// The residual as a fraction of capacity. This is what gets compared against a tolerance.
    pub fn residual_ratio(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.residual() as f64 / self.total as f64
    }
}

/// Whether the accounting explains the volume well enough to be trusted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reconciliation {
    /// The unexplained remainder is within tolerance.
    Explained { residual: u64, ratio: f64 },

    /// Too much is unexplained. The scan must not present its figures as complete.
    Unexplained {
        residual: u64,
        ratio: f64,
        /// How much of the gap may be down to paths we could not read. This is the first
        /// thing worth investigating, and it is not quantifiable from the index alone.
        unreadable_paths: u64,
    },
}

/// Compare the accounting against a tolerance, expressed as a fraction of capacity.
///
/// `Unexplained` is a finding to surface to the user, not an internal error to swallow.
pub fn reconcile(accounting: &Accounting, tolerance: f64) -> Reconciliation {
    let residual = accounting.residual();
    let ratio = accounting.residual_ratio();

    if ratio <= tolerance {
        Reconciliation::Explained { residual, ratio }
    } else {
        Reconciliation::Unexplained {
            residual,
            ratio,
            unreadable_paths: accounting.denied.paths,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    fn sample() -> Accounting {
        Accounting {
            total: 500 * GB,
            free: 88 * GB,
            file_bytes: 380 * GB,
            stream_bytes: 4 * GB,
            directory_bytes: 2 * GB,
            known_metadata: Some(15 * GB),
            denied: Denied::new(0),
        }
    }

    #[test]
    fn used_is_capacity_minus_free() {
        assert_eq!(sample().used(), 412 * GB);
    }

    #[test]
    fn attributed_is_the_sum_of_the_parts() {
        assert_eq!(sample().attributed(), 401 * GB);
    }

    #[test]
    fn residual_is_used_minus_attributed() {
        // 412 used, 401 attributed, so 11 GB is unexplained.
        assert_eq!(sample().residual(), 11 * GB);
    }

    #[test]
    fn residual_ratio_is_measured_against_capacity() {
        let accounting = sample();
        assert!((accounting.residual_ratio() - 0.022).abs() < 0.001);
    }

    #[test]
    fn over_attribution_does_not_underflow() {
        // If we somehow attribute more than is used, the residual must be zero rather than
        // wrapping around into a nonsense figure.
        let mut accounting = sample();
        accounting.file_bytes = 900 * GB;

        assert_eq!(accounting.residual(), 0);
    }

    #[test]
    fn empty_volume_has_zero_ratio_rather_than_a_division_error() {
        let accounting = Accounting {
            total: 0,
            free: 0,
            file_bytes: 0,
            stream_bytes: 0,
            directory_bytes: 0,
            known_metadata: None,
            denied: Denied::new(0),
        };

        assert_eq!(accounting.residual_ratio(), 0.0);
    }

    #[test]
    fn unmeasurable_metadata_is_not_the_same_as_zero_metadata() {
        let mut accounting = sample();
        accounting.known_metadata = None;

        assert_eq!(accounting.attributed(), 386 * GB);
        assert_eq!(accounting.residual(), 26 * GB);
    }

    #[test]
    fn within_tolerance_is_explained() {
        let accounting = sample();
        assert!(matches!(
            reconcile(&accounting, 0.05),
            Reconciliation::Explained { .. }
        ));
    }

    #[test]
    fn beyond_tolerance_is_unexplained_and_reports_the_cause() {
        let mut accounting = sample();
        accounting.denied = Denied::new(1_284);

        match reconcile(&accounting, 0.005) {
            Reconciliation::Unexplained {
                residual,
                unreadable_paths,
                ..
            } => {
                assert_eq!(residual, 11 * GB);
                assert_eq!(unreadable_paths, 1_284);
            }
            other => panic!("expected Unexplained, got {other:?}"),
        }
    }
}
