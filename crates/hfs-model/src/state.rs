//! What the model is told about one file.
//!
//! State assembly is the equivalent of a prompt, and it is versioned rather than a string:
//! exactly these facts, in exactly this shape. Anything the model may consider must arrive
//! through here, so that what it saw is as inspectable as what it said. File names stay in
//! the state — they reveal a great deal, which is precisely why the model runs locally and
//! this crate never sends them anywhere.

use hfs_core::{FileProvenance, extension_of};

/// The facts about one file that a decision may consider.
///
/// Assembled from the index, never from the live volume: the model reasons over the same
/// measured snapshot the user can inspect with `explain`. Every field is a measurement or
/// a direct derivation of one; there is no score, rank or judgment here.
#[derive(Debug, Clone)]
pub struct FileState {
    /// File name, not path. The name carries signal (extensions, naming patterns); the full
    /// path carries identity, which the model does not need and should not memorise.
    pub name: String,
    pub ext: String,
    /// Name of the containing directory (`target`, `node_modules`), not its path. Enough
    /// context to recognise regenerable build output without handing the model identities
    /// to memorise.
    pub directory: String,
    /// Bytes the file reports.
    pub logical: u64,
    /// Bytes the file actually occupies.
    pub allocated: u64,
    /// Named-stream allocation.
    pub stream_bytes: u64,
    /// How many named streams were found.
    pub stream_count: u32,
    /// How many names point at this file.
    pub links: u32,
    /// Last write time in Windows ticks, 0 when unknown.
    pub modified: i64,
    /// Which queries produced the figures. A state built on unmeasured figures must be
    /// recognisable as such downstream, so factors derived from it can be down-weighted or
    /// dropped rather than trusted equally.
    pub provenance: FileProvenance,
}

impl FileState {
    /// Assemble state from measured parts. The extension is derived from the name by the
    /// same rule the index uses, so the two can never disagree about what a file is.
    ///
    /// The signature is long because the state is explicit: every fact the model may
    /// consider is a parameter, and nothing arrives by a side channel.
    pub fn assemble(
        name: &str,
        logical: u64,
        allocated: u64,
        stream_bytes: u64,
        stream_count: u32,
        links: u32,
        provenance: FileProvenance,
    ) -> Self {
        Self {
            name: name.to_string(),
            ext: extension_of(name),
            directory: String::new(),
            logical,
            allocated,
            stream_bytes,
            stream_count,
            links,
            modified: 0,
            provenance,
        }
    }

    /// Add the containing directory's name and the last-write time. Builder-style so the
    /// six-argument assembly stays stable while callers attach what they know.
    pub fn with_location(mut self, directory: &str, modified: i64) -> Self {
        self.directory = directory.to_string();
        self.modified = modified;
        self
    }

    /// Age in Windows ticks at `now`, or `None` when the timestamp was never measured.
    /// Unknown stays unknown all the way down: no rule may treat it as old or as new.
    pub fn age_at(&self, now: i64) -> Option<i64> {
        if self.modified > 0 && self.modified <= now {
            Some(now - self.modified)
        } else {
            None
        }
    }

    /// Whether the file reports more than it occupies: sparse, compressed, or both. A fact
    /// about measurement, not a judgment — such files are routinely the most misleading
    /// entries in a size ranking, which is why the model is told explicitly.
    pub fn is_sparse_like(&self) -> bool {
        self.allocated < self.logical
    }

    /// Whether every figure in this state is a measurement. Below this, factors are built
    /// on reports and unknowns, and the combination rules must treat them accordingly.
    pub fn fully_measured(&self) -> bool {
        self.provenance == FileProvenance::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembly_derives_the_extension_by_the_shared_rule() {
        let state = FileState::assemble("archive.tar.gz", 100, 4096, 0, 0, 1, FileProvenance::Full);
        assert_eq!(state.ext, "gz");
        assert!(state.fully_measured());
        assert!(!state.is_sparse_like());
    }

    #[test]
    fn sparse_files_are_flagged_as_fact() {
        let state = FileState::assemble(
            "disk.vhdx",
            100_000_000_000,
            2_000_000_000,
            0,
            0,
            1,
            FileProvenance::Full,
        );
        assert!(state.is_sparse_like());
    }

    #[test]
    fn unmeasured_state_is_recognisable() {
        let state = FileState::assemble("pagefile.sys", 1, 1, 0, 0, 1, FileProvenance::Denied);
        assert!(!state.fully_measured());
    }

    #[test]
    fn age_is_none_when_unmeasured() {
        let now = 134_000_000_000_000_000i64;
        let unknown = FileState::assemble("x.bin", 1, 1, 0, 0, 1, FileProvenance::Full);
        assert_eq!(unknown.age_at(now), None);
        let known = FileState::assemble("x.bin", 1, 1, 0, 0, 1, FileProvenance::Full)
            .with_location("data", now - 10);
        assert_eq!(known.age_at(now), Some(10));
        // A timestamp from the future is a broken clock, not a negative age.
        let future = FileState::assemble("x.bin", 1, 1, 0, 0, 1, FileProvenance::Full)
            .with_location("data", now + 10);
        assert_eq!(future.age_at(now), None);
    }
}
