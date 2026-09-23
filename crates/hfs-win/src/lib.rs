//! Windows platform layer.
//!
//! This is the only crate permitted to talk to the operating system, and the only one
//! permitted to contain `unsafe`. Everything it learns is translated into the
//! platform-neutral types in `hfs-core` before it leaves this crate, so that the data model
//! stays testable anywhere and a second platform can be added later without redesigning it.
//!
//! Two rules apply to every function here:
//!
//! 1. **Read-only.** Nothing in this crate opens a file for write, creates, deletes or renames
//!    anything. The first release cannot modify a scanned volume, and that is enforced here
//!    rather than promised in documentation.
//! 2. **Documented APIs only.** No undocumented control codes, no raw on-disk structure
//!    parsing. Undocumented behaviour breaks silently across Windows releases, which is not
//!    acceptable in a tool people point at their only copy of something.

mod util;

pub mod elevation;
pub mod volume;
pub mod walk;

pub use elevation::is_elevated;
pub use volume::{VolumeInfo, volume_info};
pub use walk::{EntryKind, ScannedEntry, Walk, WalkOptions, WalkSummary, walk, walk_with};
