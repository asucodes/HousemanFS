//! Platform-neutral data model for HousemanFS.
//!
//! This crate holds the types that describe a scanned volume without touching
//! any operating-system API: file identity, storage accounting, and the
//! evidence attached to every claim the tool makes.
//!
//! Two rules govern what belongs here:
//!
//! 1. **No platform code.** Anything that calls into an operating system
//!    belongs in a platform crate. Keeping this crate pure is what makes the
//!    data model testable anywhere, and it is the seam that would let a
//!    second platform be added later without redesigning the model.
//! 2. **No decisions, only facts.** Types here describe what was observed and
//!    how it was measured. Judgment about what to do with those facts lives
//!    elsewhere, so that it can be inspected and tested separately.
//!
//! The accounting types encode the project's central claim: a file does not
//! necessarily own its bytes, and a reclaim estimate that assumes otherwise is
//! wrong in the optimistic direction. See [`bytes::Ownership`].

#![forbid(unsafe_code)]

pub mod accounting;
pub mod bytes;
pub mod identity;

pub use accounting::{reconcile, Accounting, Denied, Reconciliation};
pub use bytes::{ExtentBytes, Ownership};
pub use identity::{FileId, FileKey, VolumeId};
