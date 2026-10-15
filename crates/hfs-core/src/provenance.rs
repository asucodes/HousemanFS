//! Provenance of a single indexed entry.
//!
//! Every number the tool reports must be traceable to the measurement behind it. For a whole
//! volume that trace is the reconciliation; for one file or directory it is this: which of
//! the platform queries produced its figures, and which did not.
//!
//! The distinction that matters is between a measured zero and an unknown. A file with no
//! named streams and a file whose streams could not be enumerated both occupy zero *known*
//! stream bytes, but only the first was measured. Collapsing the two is exactly the optimistic
//! lie the project exists to prevent, so the provenance travels with the numbers into the
//! index and back out through `explain`.

/// How one indexed entry's figures were produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileProvenance {
    /// Allocation, identity and streams were all measured.
    Full,
    /// Measured, but identity was unavailable, so hardlink grouping may have overcounted.
    NoIdentity,
    /// Allocation and identity measured; streams could not be enumerated.
    NoStreams,
    /// The entry could not be opened. Its size is the directory entry's report, not a
    /// measurement, and its allocation is unknown.
    Denied,
    /// The provenance was not recorded, which is the case for every entry written by an
    /// index older than schema version 2. Unknown is reported as unknown, never upgraded
    /// into a measurement after the fact.
    #[default]
    Unknown,
}

impl FileProvenance {
    /// The code stored in the index. Short and stable: index rows outlive the build that
    /// wrote them, so this string is a compatibility surface.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::NoIdentity => "no-id",
            Self::NoStreams => "no-streams",
            Self::Denied => "denied",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a stored code. Anything unrecognised is `Unknown` rather than an error: a newer
    /// build's code must never break an older build's read, and the index is derived data, so
    /// the safe response to a code that cannot be understood is to report less, not to fail.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "full" => Self::Full,
            "no-id" => Self::NoIdentity,
            "no-streams" => Self::NoStreams,
            "denied" => Self::Denied,
            _ => Self::Unknown,
        }
    }

    /// Whether the entry's allocation figure is a measurement.
    pub const fn allocation_measured(self) -> bool {
        !matches!(self, Self::Denied | Self::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for provenance in [
            FileProvenance::Full,
            FileProvenance::NoIdentity,
            FileProvenance::NoStreams,
            FileProvenance::Denied,
            FileProvenance::Unknown,
        ] {
            assert_eq!(FileProvenance::parse(provenance.as_str()), provenance);
        }
    }

    #[test]
    fn unrecognised_codes_become_unknown() {
        assert_eq!(FileProvenance::parse("thorough"), FileProvenance::Unknown);
        assert_eq!(FileProvenance::parse(""), FileProvenance::Unknown);
    }

    #[test]
    fn only_measured_entries_claim_measured_allocation() {
        assert!(FileProvenance::Full.allocation_measured());
        assert!(FileProvenance::NoIdentity.allocation_measured());
        assert!(FileProvenance::NoStreams.allocation_measured());
        assert!(!FileProvenance::Denied.allocation_measured());
        assert!(!FileProvenance::Unknown.allocation_measured());
    }
}
