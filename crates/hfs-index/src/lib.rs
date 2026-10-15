//! Persistent index and queries.
//!
//! The index is **derived data, never a source of truth.** Everything in it can be rebuilt
//! from the volume. That property drives the design: corruption means rebuild rather than
//! repair, and the schema is versioned because once anyone holds an index file it cannot be
//! broken.
//!
//! An index is also a map of somebody's machine. It lives outside the scanned volume and is
//! never committed, but it is still sensitive — treat the file as private.
//!
//! No platform code lives here, so the index is testable anywhere.

use hfs_core::FileProvenance;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

/// Schema version. Bump only with a migration, never in place.
///
/// Version 3 adds the last-write timestamp that refreshes compare for change detection.
/// Version 2 added per-entry stream occupancy and provenance. Version 1 indexes migrate
/// forward through each step on open; anything else is rejected with instructions to
/// rebuild, because the index is derived data and a rebuild is always safe.
pub const SCHEMA_VERSION: u32 = 3;

const SCHEMA: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Directories carry their full path, because answering "which directory is
-- biggest" needs the path and reconstructing it per query would be slow.
-- `subtree_allocated` is the total beneath the directory, not just its own
-- index blocks. `subtree_stream` is the named-stream total beneath it, rolled
-- up the same way, so a directory's size includes storage a listing never shows.
CREATE TABLE dirs (
    id                BLOB PRIMARY KEY,
    parent            BLOB,
    name              TEXT NOT NULL,
    path              TEXT NOT NULL,
    depth             INTEGER NOT NULL,
    index_bytes       INTEGER NOT NULL DEFAULT 0,
    stream_bytes      INTEGER NOT NULL DEFAULT 0,
    stream_count      INTEGER NOT NULL DEFAULT 0,
    provenance        TEXT NOT NULL DEFAULT 'unknown',
    subtree_logical   INTEGER NOT NULL DEFAULT 0,
    subtree_allocated INTEGER NOT NULL DEFAULT 0,
    subtree_stream    INTEGER NOT NULL DEFAULT 0,
    subtree_files     INTEGER NOT NULL DEFAULT 0
);

-- No primary key on `id`: two hardlinks share one file ID and are two rows.
-- That is deliberate. It is what lets a query count how many names point at
-- the same data.
--
-- `stream_bytes` is the named-stream allocation measured on the same handle as
-- `allocated`, and `provenance` says which queries produced the row. A zero in
-- `stream_bytes` means "measured none" only when provenance says the streams
-- were enumerated; otherwise it means unknown.
CREATE TABLE files (
    id           BLOB NOT NULL,
    parent       BLOB NOT NULL,
    name         TEXT NOT NULL,
    ext          TEXT NOT NULL,
    logical      INTEGER NOT NULL,
    allocated    INTEGER NOT NULL,
    links        INTEGER NOT NULL,
    stream_bytes INTEGER NOT NULL DEFAULT 0,
    stream_count INTEGER NOT NULL DEFAULT 0,
    provenance   TEXT NOT NULL DEFAULT 'unknown',
    modified     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX files_allocated ON files(allocated DESC);
CREATE INDEX files_ext       ON files(ext);
CREATE INDEX files_parent    ON files(parent);
CREATE INDEX files_id        ON files(id);
CREATE INDEX dirs_subtree    ON dirs(subtree_allocated DESC);
CREATE INDEX dirs_path       ON dirs(path);
"#;

/// Bring a version-2 index forward: the last-write timestamp that refreshes compare.
///
/// Backfilled to 0, which is 1601-01-01 in Windows ticks and therefore never a real
/// modification time — an unambiguous "unknown". A refresh treats unknown stored
/// timestamps as changed, so the first refresh after migration re-verifies everything
/// rather than assuming nothing moved.
fn migrate_v2_to_v3(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("ALTER TABLE files ADD COLUMN modified INTEGER NOT NULL DEFAULT 0;")
}
///
/// Backfilling to `unknown` rather than `full` is the honest choice. The v1 rows were
/// measured, but this build cannot say which queries produced them, and upgrading an
/// unrecorded past into a measured one would be exactly the kind of silent optimism the
/// provenance field exists to prevent.
fn migrate_v1_to_v2(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "ALTER TABLE files ADD COLUMN stream_bytes INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE files ADD COLUMN stream_count INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE files ADD COLUMN provenance TEXT NOT NULL DEFAULT 'unknown';
         ALTER TABLE dirs ADD COLUMN stream_bytes INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE dirs ADD COLUMN stream_count INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE dirs ADD COLUMN provenance TEXT NOT NULL DEFAULT 'unknown';
         ALTER TABLE dirs ADD COLUMN subtree_stream INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX IF NOT EXISTS dirs_path ON dirs(path);",
    )
}

/// An open index.
pub struct Index {
    conn: Connection,
    /// Directory path, so path handling stays in one place.
    path: std::path::PathBuf,
}

/// Facts about the volume a scan was taken from, plus what the scan itself observed.
///
/// The second half is what makes `explain` possible without re-walking the volume: how much
/// was measured, how much was unreadable, and under what privilege. Every figure here was
/// true at scan time; the live volume may have changed since, which is why suggestions built
/// on an index must re-verify before acting.
#[derive(Debug, Clone, Default)]
pub struct ScanMetadata {
    pub root: String,
    pub filesystem: String,
    pub volume_id: [u8; 16],
    pub cluster_bytes: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Under what privilege the scan ran. A standard-user scan leaves system directories
    /// unread, and anyone reading the index deserves to know that before trusting its totals.
    pub elevated: bool,
    /// What happened when the backup privilege was requested: `enabled`, `not-held`, or the
    /// refusal described in words. Free text rather than an enum because the platform layer
    /// owns the outcome type and this crate must not.
    pub backup_privilege: String,
    /// Walk totals, stored so the index can report what it holds and what it missed.
    pub files: u64,
    pub directories: u64,
    pub reparse_points: u64,
    pub stream_bytes: u64,
    pub stream_files: u64,
    pub stream_count: u64,
    pub stream_unknown_files: u64,
    pub denied_directories: u64,
    pub denied_files: u64,
    pub denied_file_bytes: u64,
    pub hardlink_groups: u64,
    pub hardlink_extra_names: u64,
    /// Bytes lost to rounding files up to whole clusters, and bytes in directory index
    /// blocks. Stored so a dashboard can reconcile from the index alone, without re-walking.
    pub slack_bytes: u64,
    pub directory_bytes: u64,
}

/// One directory, as recorded.
#[derive(Debug, Clone)]
pub struct DirRow {
    pub id: [u8; 16],
    pub parent: Option<[u8; 16]>,
    pub name: String,
    pub path: String,
    pub depth: u32,
    /// Bytes the directory itself occupies: its index blocks.
    pub index_bytes: u64,
    /// Named-stream allocation on the directory itself, measured on the same handle.
    pub stream_bytes: u64,
    pub stream_count: u32,
    pub provenance: FileProvenance,
}

/// One file, as recorded.
#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: [u8; 16],
    pub parent: [u8; 16],
    pub name: String,
    pub ext: String,
    pub logical: u64,
    pub allocated: u64,
    pub links: u32,
    /// Named-stream allocation, measured on allocation size like the main data.
    pub stream_bytes: u64,
    pub stream_count: u32,
    pub provenance: FileProvenance,
    /// Last write time in Windows ticks. 0 means unknown — never a real timestamp — and a
    /// refresh treats unknown as changed, re-verifying rather than assuming.
    pub modified: i64,
}

/// A file together with enough of its location to display.
#[derive(Debug, Clone)]
pub struct FileHit {
    pub path: String,
    pub logical: u64,
    pub allocated: u64,
    pub links: u32,
    pub stream_bytes: u64,
    pub stream_count: u32,
    pub provenance: FileProvenance,
    pub modified: i64,
}

/// A directory subtree together with enough to display.
#[derive(Debug, Clone)]
pub struct DirHit {
    pub path: String,
    pub allocated: u64,
    pub streams: u64,
    pub files: u64,
}

/// A structured question over the index: the typed intermediate representation.
///
/// Queries cross the API boundary in this shape, never as SQL strings — the index owns all
/// SQL, so a caller (a hand-written parser today, a local model tomorrow) can express only
/// what this struct can hold. That bound is deliberate: it is what will make a future
/// natural-language frontend unable to ask for anything the index was not built to answer.
#[derive(Debug, Clone, Default)]
pub struct AskQuery {
    /// Minimum occupied size, inclusive.
    pub min_allocated: Option<u64>,
    /// Maximum occupied size, inclusive.
    pub max_allocated: Option<u64>,
    /// Extensions to include, lowercased without dots. Empty means all.
    pub exts: Vec<String>,
    /// Name substring, matched literally.
    pub name: Option<String>,
    /// Files last written before this timestamp (Windows ticks).
    pub older_than: Option<i64>,
    /// Files last written at or after this timestamp (Windows ticks).
    pub newer_than: Option<i64>,
    /// Only files with more than one name.
    pub shared_only: bool,
    /// Only files carrying named streams.
    pub streams_only: bool,
    pub limit: u32,
}

impl AskQuery {
    /// Whether the query narrows anything at all. An empty query is "largest files", which
    /// is a fine question and needs no apology — but the caller should know.
    pub fn is_empty(&self) -> bool {
        self.min_allocated.is_none()
            && self.max_allocated.is_none()
            && self.exts.is_empty()
            && self.name.is_none()
            && self.older_than.is_none()
            && self.newer_than.is_none()
            && !self.shared_only
            && !self.streams_only
    }
}

/// Aggregate totals for the indexed volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub files: u64,
    pub directories: u64,
    pub logical: u64,
    pub allocated: u64,
    pub stream_bytes: u64,
}

/// What a refresh changed, as counted while applying it.
///
/// `moved` means the same file identity at a new location — a rename reads this way too,
/// because at the filesystem level that is what a rename is. `stale` counts rows that could
/// be neither confirmed nor refuted: their parent was not scanned this refresh, so they
/// stay, and the count says how many such rows the index carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshReport {
    pub added_dirs: u64,
    pub removed_dirs: u64,
    pub stale_dirs: u64,
    pub added_files: u64,
    pub removed_files: u64,
    pub modified_files: u64,
    pub moved_files: u64,
    pub stale_files: u64,
}

/// One entry's measurements and where they came from: the unit `explain` reports.
#[derive(Debug, Clone)]
pub struct ExplainRecord {
    /// Either `"file"` or `"directory"`.
    pub kind: &'static str,
    pub path: String,
    pub logical: u64,
    pub allocated: u64,
    pub links: u32,
    pub stream_bytes: u64,
    pub stream_count: u32,
    pub provenance: FileProvenance,
    /// Last write time in Windows ticks, 0 when unknown. Reported as an age, never as a
    /// judgment — staleness is a model factor, not an index fact.
    pub modified: i64,
}

impl Index {
    /// Open an index, creating it if absent.
    ///
    /// A file with an unrecognised schema is rejected rather than migrated in place: the index
    /// is derived data, so the correct response to a version it does not understand is to
    /// rebuild it, not to guess.
    pub fn open(path: impl Into<std::path::PathBuf>) -> rusqlite::Result<Self> {
        let path = path.into();
        let conn = Connection::open(&path)?;

        // Ask whether the table exists before querying it. Querying a missing table is an
        // error, not an empty result, and on a fresh file there is nothing to query yet.
        let has_meta = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)?;

        // Read as text and parse rather than reading an integer. SQLite columns are dynamically
        // typed, so a value written by one build can land as TEXT and another as INTEGER, and
        // reading it back as a fixed type fails on whichever it is not. Casting in SQL makes the
        // read work against both.
        let existing: Option<u32> = if has_meta {
            conn.query_row(
                "SELECT CAST(value AS TEXT) FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|raw| raw.parse().ok())
        } else {
            None
        };

        match existing {
            None => conn.execute_batch(SCHEMA)?,
            Some(v) if v == SCHEMA_VERSION => {}
            Some(2) => migrate_v2_to_v3(&conn)?,
            Some(1) => {
                migrate_v1_to_v2(&conn)?;
                migrate_v2_to_v3(&conn)?;
            }
            Some(v) => {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(1),
                    Some(format!(
                        "index schema is version {v} but this build understands {SCHEMA_VERSION}; \
                         delete the index and rebuild it"
                    )),
                ));
            }
        }

        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION as i64],
        )?;

        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Record the scan in `meta`, replacing the previous record.
    fn store_meta(tx: &rusqlite::Transaction, meta: &ScanMetadata) -> rusqlite::Result<()> {
        for (key, value) in [
            ("root", meta.root.clone()),
            ("filesystem", meta.filesystem.clone()),
            ("cluster_bytes", meta.cluster_bytes.to_string()),
            ("total_bytes", meta.total_bytes.to_string()),
            ("free_bytes", meta.free_bytes.to_string()),
            ("elevated", meta.elevated.to_string()),
            ("backup_privilege", meta.backup_privilege.clone()),
            ("scan_files", meta.files.to_string()),
            ("scan_directories", meta.directories.to_string()),
            ("scan_reparse_points", meta.reparse_points.to_string()),
            ("scan_stream_bytes", meta.stream_bytes.to_string()),
            ("scan_stream_files", meta.stream_files.to_string()),
            ("scan_stream_count", meta.stream_count.to_string()),
            (
                "scan_stream_unknown_files",
                meta.stream_unknown_files.to_string(),
            ),
            (
                "scan_denied_directories",
                meta.denied_directories.to_string(),
            ),
            ("scan_denied_files", meta.denied_files.to_string()),
            ("scan_denied_file_bytes", meta.denied_file_bytes.to_string()),
            ("scan_hardlink_groups", meta.hardlink_groups.to_string()),
            (
                "scan_hardlink_extra_names",
                meta.hardlink_extra_names.to_string(),
            ),
            ("scan_slack_bytes", meta.slack_bytes.to_string()),
            ("scan_directory_bytes", meta.directory_bytes.to_string()),
        ] {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('volume_id', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![meta.volume_id.to_vec()],
        )?;
        Ok(())
    }

    /// Replace the contents of the index with one scan.
    ///
    /// Stored in a single transaction so a crash mid-write leaves the previous scan intact
    /// rather than a half-populated index that looks complete.
    pub fn write_scan<'a>(
        &mut self,
        meta: &ScanMetadata,
        dirs: impl Iterator<Item = &'a DirRow>,
        files: impl Iterator<Item = &'a FileRow>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;

        tx.execute_batch("DELETE FROM files; DELETE FROM dirs;")?;
        Self::store_meta(&tx, meta)?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO dirs (id, parent, name, path, depth, index_bytes,
                                   stream_bytes, stream_count, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for dir in dirs {
                stmt.execute(params![
                    dir.id.to_vec(),
                    dir.parent.map(|p| p.to_vec()),
                    dir.name,
                    dir.path,
                    dir.depth,
                    dir.index_bytes as i64,
                    dir.stream_bytes as i64,
                    dir.stream_count,
                    dir.provenance.as_str(),
                ])?;
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (id, parent, name, ext, logical, allocated, links,
                                    stream_bytes, stream_count, provenance, modified)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for file in files {
                stmt.execute(params![
                    file.id.to_vec(),
                    file.parent.to_vec(),
                    file.name,
                    file.ext,
                    file.logical as i64,
                    file.allocated as i64,
                    file.links,
                    file.stream_bytes as i64,
                    file.stream_count,
                    file.provenance.as_str(),
                    file.modified,
                ])?;
            }
        }

        tx.commit()?;
        self.propagate_subtree_totals()?;
        Ok(())
    }

    /// Fold a fresh scan into the index, touching only what changed.
    ///
    /// A rebuild replaces millions of rows to record thousands of changes; a refresh diffs
    /// the new scan against the stored one inside one transaction and reports the delta.
    /// Matching is by location first (`parent`, `name`), then by file identity, so a rename
    /// reads as a move rather than a delete plus an add.
    ///
    /// Deletion is conservative on purpose. A row disappears only when its parent directory
    /// was scanned this refresh and the entry is absent — proof of absence, not absence of
    /// proof. Rows under directories that were not scanned (denied, or gone along with
    /// their parent) are left untouched and counted as stale, because deleting an entry
    /// that still exists would be worse than keeping one that does not. The report says
    /// which happened.
    pub fn refresh_scan<'a>(
        &mut self,
        meta: &ScanMetadata,
        dirs: impl Iterator<Item = &'a DirRow>,
        files: impl Iterator<Item = &'a FileRow>,
    ) -> rusqlite::Result<RefreshReport> {
        use std::collections::{HashMap, HashSet};

        let mut report = RefreshReport::default();

        // A stored directory: parent id, path, and the measured fields a refresh compares.
        type StoredDir = (Option<Vec<u8>>, String, i64, i64, u32, String);
        // A stored file keyed by (parent, name): identity plus every compared field.
        type StoredFile = (Vec<u8>, i64, i64, u32, i64, u32, String, i64);

        let mut old_dirs: HashMap<Vec<u8>, StoredDir> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT id, parent, path, index_bytes, stream_bytes, stream_count, provenance
                 FROM dirs",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                old_dirs.insert(
                    row.get(0)?,
                    (
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ),
                );
            }
        }
        let old_dir_by_path: HashMap<&str, &Vec<u8>> = old_dirs
            .iter()
            .map(|(id, dir)| (dir.1.as_str(), id))
            .collect();

        // Existing files: (parent, name) -> full row.
        let mut old_files: HashMap<(Vec<u8>, String), StoredFile> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT parent, name, id, logical, allocated, links,
                        stream_bytes, stream_count, provenance, modified
                 FROM files",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let parent: Vec<u8> = row.get(0)?;
                let name: String = row.get(1)?;
                old_files.insert(
                    (parent, name),
                    (
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ),
                );
            }
        }

        let tx = self.conn.transaction()?;

        // Directories first, so file parentage always resolves against the new tree.
        let mut new_dir_ids: HashSet<Vec<u8>> = HashSet::new();
        let mut new_dir_paths: HashSet<&str> = HashSet::new();
        // Directories that were listed but not entered. Their subtrees are unscanned, so
        // nothing beneath them may be deleted — and "scanned" below always excludes them.
        let mut new_denied_ids: HashSet<Vec<u8>> = HashSet::new();
        {
            let mut insert = tx.prepare(
                "INSERT INTO dirs (id, parent, name, path, depth, index_bytes,
                                   stream_bytes, stream_count, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            let mut update = tx.prepare(
                "UPDATE dirs SET parent = ?2, name = ?3, depth = ?4, index_bytes = ?5,
                                 stream_bytes = ?6, stream_count = ?7, provenance = ?8
                 WHERE id = ?1",
            )?;
            let mut reidentify = tx.prepare("UPDATE dirs SET id = ?2 WHERE id = ?1")?;
            let mut reparent_files =
                tx.prepare("UPDATE files SET parent = ?2 WHERE parent = ?1")?;
            let mut delete = tx.prepare("DELETE FROM dirs WHERE id = ?1")?;

            for dir in dirs {
                let id = dir.id.to_vec();
                new_dir_ids.insert(id.clone());
                new_dir_paths.insert(dir.path.as_str());
                if dir.provenance == FileProvenance::Denied {
                    new_denied_ids.insert(id.clone());
                }

                if let Some(old) = old_dirs.get(&id) {
                    if old.2 != dir.index_bytes as i64
                        || old.3 != dir.stream_bytes as i64
                        || old.4 != dir.stream_count
                        || old.5 != dir.provenance.as_str()
                    {
                        update.execute(params![
                            id,
                            dir.parent.map(|p| p.to_vec()),
                            dir.name,
                            dir.depth,
                            dir.index_bytes as i64,
                            dir.stream_bytes as i64,
                            dir.stream_count,
                            dir.provenance.as_str(),
                        ])?;
                    }
                } else if let Some(old_id) = old_dir_by_path.get(dir.path.as_str()) {
                    // Same path, new identity: the directory was replaced, not moved.
                    // Children follow the new identity so they are not orphaned.
                    reidentify.execute(params![old_id, id])?;
                    reparent_files.execute(params![id, old_id])?;
                    update.execute(params![
                        id,
                        dir.parent.map(|p| p.to_vec()),
                        dir.name,
                        dir.depth,
                        dir.index_bytes as i64,
                        dir.stream_bytes as i64,
                        dir.stream_count,
                        dir.provenance.as_str(),
                    ])?;
                } else {
                    insert.execute(params![
                        id,
                        dir.parent.map(|p| p.to_vec()),
                        dir.name,
                        dir.path,
                        dir.depth,
                        dir.index_bytes as i64,
                        dir.stream_bytes as i64,
                        dir.stream_count,
                        dir.provenance.as_str(),
                    ])?;
                    report.added_dirs += 1;
                }
            }

            for (id, (parent, path, ..)) in &old_dirs {
                if new_dir_paths.contains(path.as_str()) {
                    continue;
                }
                // Gone from the scan. Removed only if the parent was scanned — proof the
                // directory is truly absent rather than merely unreachable this time. A
                // denied parent was listed but not entered, so its children prove nothing.
                let parent_scanned = match parent {
                    None => false,
                    Some(parent_id) => old_dirs.get(parent_id).is_some_and(|parent_dir| {
                        new_dir_paths.contains(parent_dir.1.as_str())
                            && !new_denied_ids.contains(parent_id)
                    }),
                };
                if parent_scanned {
                    delete.execute(params![id])?;
                    report.removed_dirs += 1;
                } else {
                    report.stale_dirs += 1;
                }
            }
        }

        // Files, keyed by location, with identity as the fallback for moves.
        {
            let mut insert = tx.prepare(
                "INSERT INTO files (id, parent, name, ext, logical, allocated, links,
                                    stream_bytes, stream_count, provenance, modified)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            let mut update = tx.prepare(
                "UPDATE files SET id = ?3, logical = ?4, allocated = ?5, links = ?6,
                                  stream_bytes = ?7, stream_count = ?8, provenance = ?9,
                                  modified = ?10
                 WHERE parent = ?1 AND name = ?2",
            )?;
            let mut relocate = tx.prepare(
                "UPDATE files SET parent = ?3, name = ?4, logical = ?5, allocated = ?6,
                                  links = ?7, stream_bytes = ?8, stream_count = ?9,
                                  provenance = ?10, modified = ?11
                 WHERE parent = ?1 AND name = ?2",
            )?;
            let mut delete = tx.prepare("DELETE FROM files WHERE parent = ?1 AND name = ?2")?;

            // Identity -> current locations, for recognising moves. Owned, so the
            // file loop below can consume locations while consulting it.
            let mut by_id: HashMap<Vec<u8>, Vec<(Vec<u8>, String)>> = HashMap::new();
            for (key, row) in &old_files {
                by_id.entry(row.0.clone()).or_default().push(key.clone());
            }

            for file in files {
                let key = (file.parent.to_vec(), file.name.clone());
                let fresh = (
                    file.id.to_vec(),
                    file.logical as i64,
                    file.allocated as i64,
                    file.links,
                    file.stream_bytes as i64,
                    file.stream_count,
                    file.provenance.as_str().to_string(),
                    file.modified,
                );

                match old_files.remove(&key) {
                    Some(old) if old == fresh => {}
                    Some(_) => {
                        update.execute(params![
                            key.0, key.1, fresh.0, fresh.1, fresh.2, fresh.3, fresh.4, fresh.5,
                            fresh.6, fresh.7
                        ])?;
                        report.modified_files += 1;
                    }
                    None => {
                        // Not at this location. Same identity elsewhere means moved or
                        // renamed; otherwise it is new. (A hardlink group shares one id
                        // across several keys; the first match consumes one location, and
                        // any genuinely new name still reads as added.)
                        let relocated = by_id
                            .get_mut(&fresh.0)
                            .and_then(|keys| keys.pop())
                            .filter(|old_key| old_files.remove(old_key).is_some());
                        match relocated {
                            Some((old_parent, old_name)) => {
                                relocate.execute(params![
                                    old_parent, old_name, key.0, key.1, fresh.1, fresh.2, fresh.3,
                                    fresh.4, fresh.5, fresh.6, fresh.7
                                ])?;
                                report.moved_files += 1;
                            }
                            None => {
                                insert.execute(params![
                                    fresh.0, key.0, key.1, file.ext, fresh.1, fresh.2, fresh.3,
                                    fresh.4, fresh.5, fresh.6, fresh.7
                                ])?;
                                report.added_files += 1;
                            }
                        }
                    }
                }
            }

            // Whatever remains was not seen. Removed only under a scanned parent; anything
            // else stays, and the report counts it as stale rather than pretending. In
            // particular, everything under a denied directory stays: the parent was listed
            // but never entered, so absence proves nothing.
            for ((parent, name), _) in old_files {
                if new_dir_ids.contains(&parent) && !new_denied_ids.contains(&parent) {
                    delete.execute(params![parent, name])?;
                    report.removed_files += 1;
                } else {
                    report.stale_files += 1;
                }
            }
        }

        Self::store_meta(&tx, meta)?;
        tx.commit()?;
        self.propagate_subtree_totals()?;
        Ok(report)
    }

    /// Roll each directory's own contents up through its ancestors.
    ///
    /// A directory's "size" as a user means everything beneath it, not its own index blocks.
    /// Computing that at query time would need a recursive walk per query, so it is done once
    /// at write time instead.
    fn propagate_subtree_totals(&mut self) -> rusqlite::Result<()> {
        // Direct contributions: each directory's own files and its own index blocks.
        // Stream bytes ride along in the fourth slot, rolled up by the same mechanism, so a
        // directory's size includes the named-stream storage beneath it.
        //
        // Hardlink groups are counted once per directory: the inner query collapses the
        // group's names to one row before summing, so two names for one dataset do not count
        // twice. Without this the index would commit the classic double-count the walk takes
        // care to avoid. (The same group under *two* directories appears under both — subtree
        // figures overlap by nature — but the volume-level summary below counts it once.)
        let mut direct: std::collections::HashMap<Vec<u8>, (i64, i64, i64, i64)> =
            std::collections::HashMap::new();

        {
            let mut stmt = self.conn.prepare(
                "SELECT parent, SUM(logical_once), SUM(allocated_once), SUM(names),
                        SUM(stream_once)
                 FROM (SELECT parent, MAX(logical) AS logical_once,
                              MAX(allocated) AS allocated_once, COUNT(*) AS names,
                              MAX(stream_bytes) AS stream_once
                       FROM files GROUP BY parent, id)
                 GROUP BY parent",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let parent: Vec<u8> = row.get(0)?;
                direct.insert(
                    parent,
                    (
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    ),
                );
            }
        }

        // Parents, so contributions can be pushed upward from deepest first.
        let mut parents: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>> =
            std::collections::HashMap::new();
        let mut depths: Vec<(Vec<u8>, u32)> = Vec::new();
        {
            let mut stmt = self.conn.prepare("SELECT id, parent, depth FROM dirs")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: Vec<u8> = row.get(0)?;
                let parent: Option<Vec<u8>> = row.get(1)?;
                let depth: u32 = row.get(2)?;
                parents.insert(id.clone(), parent);
                depths.push((id, depth));
            }
        }

        // Deepest first, so a child's total is complete before its parent consumes it.
        depths.sort_by_key(|a| std::cmp::Reverse(a.1));

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE dirs SET subtree_logical = ?2, subtree_allocated = ?3,
                 subtree_files = ?4, subtree_stream = ?5 WHERE id = ?1",
            )?;
            for (id, _depth) in depths {
                // Everything attributed to this directory: its own files, plus whatever its
                // children pushed up. Children are processed first because the list runs
                // deepest first, so by the time a directory is reached its total is complete.
                let total = direct.remove(&id).unwrap_or((0, 0, 0, 0));

                stmt.execute(params![id.clone(), total.0, total.1, total.2, total.3])?;

                if let Some(Some(parent)) = parents.get(&id) {
                    let entry = direct.entry(parent.clone()).or_insert((0, 0, 0, 0));
                    entry.0 += total.0;
                    entry.1 += total.1;
                    entry.2 += total.2;
                    entry.3 += total.3;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A fact recorded about the scan, such as the volume root or filesystem name.
    ///
    /// Read as text and parsed by the caller, because the value's SQLite type depends on what
    /// wrote it rather than on what it means.
    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT CAST(value AS TEXT) FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn summary(&self) -> rusqlite::Result<Summary> {
        // Names are counted; bytes are counted per dataset. Two names for one hardlinked file
        // are two rows but one set of bytes, and summing the rows would overcount — the same
        // rule the walk applies, applied here so the index agrees with the scan.
        let files = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))?
            as u64;
        let directories = self
            .conn
            .query_row("SELECT COUNT(*) FROM dirs", [], |r| r.get::<_, i64>(0))?
            as u64;
        let (logical, allocated, stream_bytes) = self.conn.query_row(
            "SELECT COALESCE(SUM(logical_once), 0), COALESCE(SUM(allocated_once), 0),
                    COALESCE(SUM(stream_once), 0)
             FROM (SELECT MAX(logical) AS logical_once, MAX(allocated) AS allocated_once,
                          MAX(stream_bytes) AS stream_once
                   FROM files GROUP BY id)",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                ))
            },
        )?;

        Ok(Summary {
            files,
            directories,
            logical,
            allocated,
            stream_bytes,
        })
    }

    /// The scan record: volume facts plus what the scan observed and under what privilege.
    ///
    /// Missing keys read as zero (or empty text) rather than failing. An index written before
    /// a key existed, or one whose write was interrupted, is still queryable; the absent
    /// facts simply report as unknown, which is what they are.
    pub fn scan_metadata(&self) -> ScanMetadata {
        let number = |key: &str| self.meta(key).and_then(|v| v.parse().ok()).unwrap_or(0);
        let flag = |key: &str| self.meta(key).is_some_and(|v| v == "true");
        ScanMetadata {
            root: self.meta("root").unwrap_or_default(),
            filesystem: self.meta("filesystem").unwrap_or_default(),
            volume_id: self
                .conn
                .query_row(
                    "SELECT value FROM meta WHERE key = 'volume_id'",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .unwrap_or([0u8; 16]),
            cluster_bytes: number("cluster_bytes") as u32,
            total_bytes: number("total_bytes"),
            free_bytes: number("free_bytes"),
            elevated: flag("elevated"),
            backup_privilege: self.meta("backup_privilege").unwrap_or_default(),
            files: number("scan_files"),
            directories: number("scan_directories"),
            reparse_points: number("scan_reparse_points"),
            stream_bytes: number("scan_stream_bytes"),
            stream_files: number("scan_stream_files"),
            stream_count: number("scan_stream_count"),
            stream_unknown_files: number("scan_stream_unknown_files"),
            denied_directories: number("scan_denied_directories"),
            denied_files: number("scan_denied_files"),
            denied_file_bytes: number("scan_denied_file_bytes"),
            hardlink_groups: number("scan_hardlink_groups"),
            hardlink_extra_names: number("scan_hardlink_extra_names"),
            slack_bytes: number("scan_slack_bytes"),
            directory_bytes: number("scan_directory_bytes"),
        }
    }

    /// The measurement behind one path: its figures and where each came from.
    ///
    /// Matching is case-insensitive and accepts either separator, because the caller's
    /// spelling of a path and the walk's spelling may differ in exactly those ways. A path
    /// that is not in the index is `None`, which is different from an entry with unknown
    /// provenance — the first was never scanned, the second was scanned but not fully
    /// measured.
    pub fn explain_path(&self, raw: &str) -> rusqlite::Result<Option<ExplainRecord>> {
        let wanted = normalize_path(raw);

        if let Some(dir) = self
            .conn
            .query_row(
                "SELECT path, index_bytes, stream_bytes, stream_count, provenance
                 FROM dirs WHERE path = ?1 COLLATE NOCASE",
                params![wanted],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? as u64,
                        row.get::<_, i64>(2)? as u64,
                        row.get::<_, u32>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?
        {
            return Ok(Some(ExplainRecord {
                kind: "directory",
                path: dir.0,
                // A directory's own occupancy is its index blocks plus its own streams. The
                // subtree it contains is shown by `dirs`, not folded in here, so the two are
                // never confused.
                logical: 0,
                allocated: dir.1.saturating_add(dir.2),
                links: 0,
                stream_bytes: dir.2,
                stream_count: dir.3,
                provenance: FileProvenance::parse(&dir.4),
                // Directories carry no stored timestamp; an age shown here would be invented.
                modified: 0,
            }));
        }

        let Some((parent, name)) = split_parent(&wanted) else {
            return Ok(None);
        };
        self.conn
            .query_row(
                "SELECT d.path, f.name, f.logical, f.allocated, f.links,
                        f.stream_bytes, f.stream_count, f.provenance, f.modified
                 FROM files f JOIN dirs d ON f.parent = d.id
                 WHERE d.path = ?1 COLLATE NOCASE AND f.name = ?2 COLLATE NOCASE
                 LIMIT 1",
                params![parent, name],
                |row| {
                    let dir: String = row.get(0)?;
                    let name: String = row.get(1)?;
                    Ok(ExplainRecord {
                        kind: "file",
                        path: join_path(&dir, &name),
                        logical: row.get::<_, i64>(2)? as u64,
                        allocated: row.get::<_, i64>(3)? as u64,
                        links: row.get::<_, u32>(4)?,
                        stream_bytes: row.get::<_, i64>(5)? as u64,
                        stream_count: row.get::<_, u32>(6)?,
                        provenance: FileProvenance::parse(&row.get::<_, String>(7)?),
                        modified: row.get::<_, i64>(8)?,
                    })
                },
            )
            .optional()
    }

    /// The largest files by occupied size.
    pub fn largest_files(&self, limit: u32) -> rusqlite::Result<Vec<FileHit>> {
        let mut stmt = self.conn.prepare(&format!(
            "{FILE_COLUMNS} ORDER BY f.allocated DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map(params![limit], file_hit)?;
        rows.collect()
    }

    /// The largest directory subtrees by occupied size, streams included.
    pub fn largest_dirs(&self, limit: u32) -> rusqlite::Result<Vec<DirHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, subtree_allocated, subtree_stream, subtree_files
             FROM dirs ORDER BY subtree_allocated DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(DirHit {
                path: row.get::<_, String>(0)?,
                allocated: row.get::<_, i64>(1)? as u64,
                streams: row.get::<_, i64>(2)? as u64,
                files: row.get::<_, i64>(3)? as u64,
            })
        })?;
        rows.collect()
    }

    /// Files whose name contains `term`, largest first.
    ///
    /// The term is matched literally: SQL wildcards in it are escaped, so searching for
    /// `100%` does not become a pattern. A search box that reinterprets its input is a small
    /// dishonesty, and this tool has a policy about those.
    pub fn search_name(&self, term: &str, limit: u32) -> rusqlite::Result<Vec<FileHit>> {
        let pattern = format!("%{}%", escape_like(term));
        let mut stmt = self.conn.prepare(&format!(
            "{FILE_COLUMNS} WHERE f.name LIKE ?1 ESCAPE '\\'
             ORDER BY f.allocated DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![pattern, limit], file_hit)?;
        rows.collect()
    }

    /// Answer a structured question: every clause becomes one parameterized predicate.
    ///
    /// The SQL is assembled, never interpolated: values travel as parameters, so a hostile
    /// or malformed term cannot escape its clause. Unknown timestamps never match an age
    /// predicate — "older than a year" must not return files whose age was never measured.
    pub fn ask(&self, query: &AskQuery) -> rusqlite::Result<Vec<FileHit>> {
        let mut sql = FILE_COLUMNS.to_string();
        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<Value> = Vec::new();

        if let Some(min) = query.min_allocated {
            clauses.push("f.allocated >= ?".to_string());
            values.push(Value::Integer(min as i64));
        }
        if let Some(max) = query.max_allocated {
            clauses.push("f.allocated <= ?".to_string());
            values.push(Value::Integer(max as i64));
        }
        if !query.exts.is_empty() {
            let holes = vec!["?"; query.exts.len()].join(",");
            clauses.push(format!("f.ext IN ({holes})"));
            for ext in &query.exts {
                values.push(Value::Text(ext.clone()));
            }
        }
        if let Some(name) = &query.name {
            clauses.push("f.name LIKE ? ESCAPE '\\'".to_string());
            values.push(Value::Text(format!("%{}%", escape_like(name))));
        }
        if let Some(older) = query.older_than {
            clauses.push("f.modified > 0 AND f.modified < ?".to_string());
            values.push(Value::Integer(older));
        }
        if let Some(newer) = query.newer_than {
            clauses.push("f.modified >= ?".to_string());
            values.push(Value::Integer(newer));
        }
        if query.shared_only {
            clauses.push("f.links > 1".to_string());
        }
        if query.streams_only {
            clauses.push("f.stream_count > 0".to_string());
        }

        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY f.allocated DESC LIMIT ?");
        values.push(Value::Integer(i64::from(query.limit)));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), file_hit)?;
        rows.collect()
    }

    /// Total occupied by files with one extension.
    ///
    /// Hardlink groups contribute once, like everywhere else bytes are summed: the inner
    /// query collapses each group's names to one row, while the file count still counts
    /// names, matching the summary's convention.
    pub fn by_extension(&self, limit: u32) -> rusqlite::Result<Vec<(String, u64, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ext, SUM(allocated_once), SUM(names)
             FROM (SELECT ext, MAX(allocated) AS allocated_once, COUNT(*) AS names
                   FROM files GROUP BY ext, id)
             GROUP BY ext ORDER BY SUM(allocated_once) DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))
        })?;
        rows.collect()
    }

    /// The index as versioned JSON: the extension surface.
    ///
    /// Anything may build on this output in any language, without running code inside a
    /// process that can read the whole disk. The contract is versioned
    /// (`contract_version`) separately from the storage schema (`schema_version`): the
    /// storage may change how it keeps facts, but this shape changes only deliberately.
    /// This is a snapshot — the largest lists are capped — not a full dump; it answers
    /// "what does the index say", not "give me every row".
    ///
    /// Built by hand rather than derived: the contract shape is a compatibility promise,
    /// and spelling each field keeps additions deliberate instead of automatic.
    pub fn export_snapshot(&self, limit: u32) -> rusqlite::Result<serde_json::Value> {
        let scan = self.scan_metadata();
        let summary = self.summary()?;
        let files = self.largest_files(limit)?;
        let dirs = self.largest_dirs(limit)?;
        let ext = self.by_extension(limit)?;

        Ok(serde_json::json!({
            "contract": "housemanfs-export",
            "contract_version": 1,
            "schema_version": SCHEMA_VERSION,
            "meta": {
                "root": scan.root,
                "filesystem": scan.filesystem,
                "cluster_bytes": scan.cluster_bytes,
                "total_bytes": scan.total_bytes,
                "free_bytes": scan.free_bytes,
            },
            "scan": {
                "elevated": scan.elevated,
                "backup_privilege": scan.backup_privilege,
                "files": scan.files,
                "directories": scan.directories,
                "reparse_points": scan.reparse_points,
                "stream_bytes": scan.stream_bytes,
                "stream_files": scan.stream_files,
                "stream_count": scan.stream_count,
                "stream_unknown_files": scan.stream_unknown_files,
                "denied_directories": scan.denied_directories,
                "denied_files": scan.denied_files,
                "denied_file_bytes": scan.denied_file_bytes,
                "hardlink_groups": scan.hardlink_groups,
                "hardlink_extra_names": scan.hardlink_extra_names,
            },
            "summary": {
                "files": summary.files,
                "directories": summary.directories,
                "logical": summary.logical,
                "allocated": summary.allocated,
                "stream_bytes": summary.stream_bytes,
            },
            "largest_files": files.iter().map(|hit| serde_json::json!({
                "path": hit.path,
                "logical": hit.logical,
                "allocated": hit.allocated,
                "links": hit.links,
                "stream_bytes": hit.stream_bytes,
                "stream_count": hit.stream_count,
                "provenance": hit.provenance.as_str(),
            })).collect::<Vec<_>>(),
            "largest_dirs": dirs.iter().map(|hit| serde_json::json!({
                "path": hit.path,
                "allocated": hit.allocated,
                "streams": hit.streams,
                "files": hit.files,
            })).collect::<Vec<_>>(),
            "by_extension": ext.iter().map(|(name, allocated, count)| serde_json::json!({
                "ext": name,
                "allocated": allocated,
                "files": count,
            })).collect::<Vec<_>>(),
        }))
    }
}

/// Join a directory path and a name without doubling the separator.
///
/// A volume root is stored as `C:\`, which already ends in a separator, so a naive join
/// produces `C:\\file`. Cosmetic in a path but it breaks comparisons and looks wrong on screen.
fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// Escape a literal for a LIKE pattern. SQL wildcards in the term are data, not pattern —
/// a search box that reinterprets its input is a small dishonesty, and this tool has a
/// policy about those.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// One file row from a joined files+dirs query. Every file listing shares these columns in
/// this order, so they share one mapper rather than three copies that could drift.
fn file_hit(row: &rusqlite::Row) -> rusqlite::Result<FileHit> {
    let dir: String = row.get(0)?;
    let name: String = row.get(1)?;
    Ok(FileHit {
        path: join_path(&dir, &name),
        logical: row.get::<_, i64>(2)? as u64,
        allocated: row.get::<_, i64>(3)? as u64,
        links: row.get::<_, u32>(4)?,
        stream_bytes: row.get::<_, i64>(5)? as u64,
        stream_count: row.get::<_, u32>(6)?,
        provenance: FileProvenance::parse(&row.get::<_, String>(7)?),
        modified: row.get::<_, i64>(8)?,
    })
}

/// The shared SELECT list behind every file listing, matching [`file_hit`].
const FILE_COLUMNS: &str = "SELECT d.path, f.name, f.logical, f.allocated, f.links,
        f.stream_bytes, f.stream_count, f.provenance, f.modified
    FROM files f JOIN dirs d ON f.parent = d.id";

/// Normalise a caller-supplied path to the walk's spelling: backslash separators, no
/// trailing separator except on a volume root.
fn normalize_path(raw: &str) -> String {
    let mut out = raw.replace('/', "\\");
    while out.len() > 3 && (out.ends_with('\\')) {
        out.pop();
    }
    out
}

/// Split a normalised path into its parent directory and final name.
fn split_parent(path: &str) -> Option<(String, String)> {
    let trimmed = path.trim_end_matches('\\');
    let (parent, name) = trimmed.rsplit_once('\\')?;
    if name.is_empty() {
        return None;
    }
    // A file directly on a volume root has `C:` as its parent; the stored dir path is
    // `C:\`, so the separator is reattached rather than dropped.
    let parent = if parent.len() == 2 && parent.as_bytes()[1] == b':' {
        format!("{parent}\\")
    } else {
        parent.to_string()
    };
    Some((parent, name.to_string()))
}

/// Split a filename into its extension, lowercased, without the dot.
///
/// Lives in `hfs-core` because it is pure string handling with no persistence in it; the
/// decision-model crate needs it too, and depending on the index for one function would drag
/// SQLite into the model's build graph. Re-exported here so existing callers do not move.
pub use hfs_core::extension_of;

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Index {
        Index::open(":memory:").expect("in-memory index opens")
    }

    fn meta() -> ScanMetadata {
        ScanMetadata {
            root: r"C:\".to_string(),
            filesystem: "NTFS".to_string(),
            volume_id: [9u8; 16],
            cluster_bytes: 4_096,
            total_bytes: 1_000_000,
            free_bytes: 500_000,
            elevated: true,
            backup_privilege: "enabled".to_string(),
            files: 0,
            directories: 0,
            reparse_points: 0,
            stream_bytes: 0,
            stream_files: 0,
            stream_count: 0,
            stream_unknown_files: 0,
            denied_directories: 0,
            denied_files: 0,
            denied_file_bytes: 0,
            hardlink_groups: 0,
            hardlink_extra_names: 0,
            slack_bytes: 0,
            directory_bytes: 0,
        }
    }

    fn root_dir() -> DirRow {
        DirRow {
            id: [1u8; 16],
            parent: None,
            name: "C:".to_string(),
            path: r"C:\".to_string(),
            depth: 0,
            index_bytes: 4_096,
            stream_bytes: 0,
            stream_count: 0,
            provenance: FileProvenance::Full,
        }
    }

    #[test]
    fn extension_extraction() {
        assert_eq!(extension_of("report.pdf"), "pdf");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of(".gitignore"), "");
        assert_eq!(extension_of("Makefile"), "");
        assert_eq!(extension_of("Report.PDF"), "pdf");
    }

    #[test]
    fn a_scan_can_be_stored_and_queried() {
        let mut index = empty();

        let root = [1u8; 16];
        let dir = root_dir();
        let files = [
            FileRow {
                id: [2u8; 16],
                parent: root,
                name: "big.bin".to_string(),
                ext: "bin".to_string(),
                logical: 100,
                allocated: 8_192,
                links: 1,
                stream_bytes: 4_096,
                stream_count: 1,
                provenance: FileProvenance::Full,
                modified: 0,
            },
            FileRow {
                id: [3u8; 16],
                parent: root,
                name: "small.txt".to_string(),
                ext: "txt".to_string(),
                logical: 10,
                allocated: 4_096,
                links: 1,
                stream_bytes: 0,
                stream_count: 0,
                provenance: FileProvenance::Full,
                modified: 0,
            },
        ];

        let meta = meta();

        index
            .write_scan(&meta, std::iter::once(&dir), files.iter())
            .unwrap();

        let summary = index.summary().unwrap();
        assert_eq!(summary.files, 2);
        assert_eq!(summary.allocated, 12_288);
        assert_eq!(summary.stream_bytes, 4_096);

        let largest = index.largest_files(1).unwrap();
        assert_eq!(largest[0].path, r"C:\big.bin");
        assert_eq!(largest[0].allocated, 8_192);
        assert_eq!(largest[0].stream_bytes, 4_096);

        let found = index.search_name("small", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, r"C:\small.txt");

        // The scan record survives the round trip.
        let stored = index.scan_metadata();
        assert_eq!(stored.root, r"C:\");
        assert!(stored.elevated);
        assert_eq!(stored.backup_privilege, "enabled");
    }

    #[test]
    fn explain_reports_a_file_and_its_provenance() {
        let mut index = empty();
        let root = [1u8; 16];
        let dir = root_dir();
        let files = [FileRow {
            id: [2u8; 16],
            parent: root,
            name: "marked.txt".to_string(),
            ext: "txt".to_string(),
            logical: 10,
            allocated: 4_096,
            links: 1,
            stream_bytes: 4_096,
            stream_count: 1,
            provenance: FileProvenance::Full,
            modified: 0,
        }];
        index
            .write_scan(&meta(), std::iter::once(&dir), files.iter())
            .unwrap();

        // Either separator works, and so does a trailing one on a directory.
        let record = index
            .explain_path(r"C:/marked.txt")
            .unwrap()
            .expect("the file is indexed");
        assert_eq!(record.kind, "file");
        assert_eq!(record.stream_bytes, 4_096);
        assert_eq!(record.provenance, FileProvenance::Full);

        let dir_record = index
            .explain_path(r"C:\/")
            .unwrap()
            .expect("the root is indexed");
        assert_eq!(dir_record.kind, "directory");

        assert!(index.explain_path(r"C:\absent.txt").unwrap().is_none());
    }

    #[test]
    fn hardlinks_are_separate_rows_sharing_one_id() {
        let mut index = empty();
        let root = [1u8; 16];
        let dir = root_dir();
        // Two names, one file: same id, two rows.
        let files = [
            FileRow {
                id: [7u8; 16],
                parent: root,
                name: "a.dat".to_string(),
                ext: "dat".to_string(),
                logical: 50,
                allocated: 4_096,
                links: 2,
                stream_bytes: 0,
                stream_count: 0,
                provenance: FileProvenance::Full,
                modified: 0,
            },
            FileRow {
                id: [7u8; 16],
                parent: root,
                name: "b.dat".to_string(),
                ext: "dat".to_string(),
                logical: 50,
                allocated: 4_096,
                links: 2,
                stream_bytes: 0,
                stream_count: 0,
                provenance: FileProvenance::Full,
                modified: 0,
            },
        ];
        index
            .write_scan(&meta(), std::iter::once(&dir), files.iter())
            .unwrap();

        assert_eq!(index.summary().unwrap().files, 2, "both names exist");
        let summary = index.summary().unwrap();
        assert_eq!(
            summary.allocated, 4_096,
            "one dataset, counted once despite two names"
        );
        let by_id: i64 = index
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE id = ?1",
                params![[7u8; 16].to_vec()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(by_id, 2, "one id, two names");
    }

    #[test]
    fn subtree_totals_roll_up_through_parents() {
        let mut index = empty();

        let root = [1u8; 16];
        let child = [2u8; 16];
        let dirs = [
            root_dir(),
            DirRow {
                id: child,
                parent: Some(root),
                name: "data".to_string(),
                path: r"C:\data".to_string(),
                depth: 1,
                index_bytes: 0,
                stream_bytes: 0,
                stream_count: 0,
                provenance: FileProvenance::Full,
            },
        ];
        let files = [FileRow {
            id: [3u8; 16],
            parent: child,
            name: "file.bin".to_string(),
            ext: "bin".to_string(),
            logical: 90,
            allocated: 4_096,
            links: 1,
            stream_bytes: 4_096,
            stream_count: 1,
            provenance: FileProvenance::Full,
            modified: 0,
        }];
        index
            .write_scan(&meta(), dirs.iter(), files.iter())
            .unwrap();

        let largest = index.largest_dirs(2).unwrap();
        // The parent owns the child's total too, streams included.
        let root_hit = largest.iter().find(|hit| hit.path == r"C:\").unwrap();
        assert_eq!(root_hit.allocated, 4_096);
        assert_eq!(root_hit.streams, 4_096);
    }

    #[test]
    fn a_version_1_index_migrates_forward_without_losing_rows() {
        // A v1 database, built the way the old schema did: no stream or provenance
        // columns, version stamped 1. Committed here as SQL rather than as a binary,
        // because a binary would be opaque and unreviewable.
        let old_schema = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE dirs (
    id BLOB PRIMARY KEY, parent BLOB, name TEXT NOT NULL, path TEXT NOT NULL,
    depth INTEGER NOT NULL, index_bytes INTEGER NOT NULL DEFAULT 0,
    subtree_logical INTEGER NOT NULL DEFAULT 0,
    subtree_allocated INTEGER NOT NULL DEFAULT 0,
    subtree_files INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE files (
    id BLOB NOT NULL, parent BLOB NOT NULL, name TEXT NOT NULL, ext TEXT NOT NULL,
    logical INTEGER NOT NULL, allocated INTEGER NOT NULL, links INTEGER NOT NULL
);"#;
        let path = std::env::temp_dir().join(format!("hfs-migrate-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("v1 database opens");
            conn.execute_batch(old_schema).unwrap();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
                [],
            )
            .unwrap();
            let root = [1u8; 16];
            conn.execute(
                "INSERT INTO dirs (id, parent, name, path, depth, index_bytes)
                 VALUES (?1, NULL, 'C:', 'C:\\', 0, 4096)",
                params![root.to_vec()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO files (id, parent, name, ext, logical, allocated, links)
                 VALUES (?1, ?2, 'old.bin', 'bin', 100, 8192, 1)",
                params![[2u8; 16].to_vec(), root.to_vec()],
            )
            .unwrap();
        }

        let index = Index::open(&path).expect("a v1 index migrates on open");
        assert_eq!(index.meta("schema_version").as_deref(), Some("3"));

        // The rows survived, and their unknown past is reported as unknown.
        let largest = index.largest_files(1).unwrap();
        assert_eq!(largest.len(), 1);
        assert_eq!(largest[0].allocated, 8_192);
        assert_eq!(largest[0].stream_bytes, 0);
        assert_eq!(largest[0].provenance, FileProvenance::Unknown);
        assert_eq!(largest[0].modified, 0, "no timestamp was ever recorded");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ask_answers_structured_questions() {
        use super::AskQuery;

        const DAY: i64 = 86_400 * 10_000_000;
        const NOW: i64 = 134_000_000_000_000_000;

        let mut index = empty();
        let root = [1u8; 16];
        let dir = root_dir();
        let files = [
            FileRow {
                id: [2u8; 16],
                parent: root,
                name: "old.iso".to_string(),
                ext: "iso".to_string(),
                logical: 1_000,
                allocated: 8_192,
                links: 1,
                stream_bytes: 0,
                stream_count: 0,
                provenance: FileProvenance::Full,
                modified: NOW - 400 * DAY,
            },
            FileRow {
                id: [3u8; 16],
                parent: root,
                name: "fresh.iso".to_string(),
                ext: "iso".to_string(),
                logical: 1_000,
                allocated: 16_384,
                links: 2,
                stream_bytes: 0,
                stream_count: 0,
                provenance: FileProvenance::Full,
                modified: NOW - 2 * DAY,
            },
            FileRow {
                id: [4u8; 16],
                parent: root,
                name: "notes.txt".to_string(),
                ext: "txt".to_string(),
                logical: 10,
                allocated: 4_096,
                links: 1,
                stream_bytes: 0,
                stream_count: 0,
                provenance: FileProvenance::Full,
                // Never measured: must not match any age predicate.
                modified: 0,
            },
        ];
        index
            .write_scan(&meta(), std::iter::once(&dir), files.iter())
            .unwrap();

        // Size plus extension.
        let hits = index
            .ask(&AskQuery {
                min_allocated: Some(8_000),
                exts: vec!["iso".to_string()],
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 2);

        // Age: the unmeasured file is excluded, not guessed about.
        let hits = index
            .ask(&AskQuery {
                older_than: Some(NOW - 365 * DAY),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("old.iso"));

        // Shared names only.
        let hits = index
            .ask(&AskQuery {
                shared_only: true,
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("fresh.iso"));

        // Everything combined, largest first.
        let hits = index
            .ask(&AskQuery {
                name: Some("iso".to_string()),
                newer_than: Some(NOW - 30 * DAY),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("fresh.iso"));
    }

    #[test]
    fn search_treats_wildcards_as_literal_text() {
        let mut index = empty();
        let root = [1u8; 16];
        let dir = root_dir();
        let files = [FileRow {
            id: [2u8; 16],
            parent: root,
            name: "100%.txt".to_string(),
            ext: "txt".to_string(),
            logical: 10,
            allocated: 4_096,
            links: 1,
            stream_bytes: 0,
            stream_count: 0,
            provenance: FileProvenance::Full,
            modified: 0,
        }];
        index
            .write_scan(&meta(), std::iter::once(&dir), files.iter())
            .unwrap();

        // `%` is data here, not a pattern: it matches the file that has one, and a bare
        // `%` alone must not match everything.
        assert_eq!(index.search_name("100%", 5).unwrap().len(), 1);
        assert_eq!(index.search_name("%", 5).unwrap().len(), 1);
    }

    fn file_row(
        id: [u8; 16],
        parent: [u8; 16],
        name: &str,
        allocated: u64,
        modified: i64,
    ) -> FileRow {
        FileRow {
            id,
            parent,
            name: name.to_string(),
            ext: extension_of(name),
            logical: allocated,
            allocated,
            links: 1,
            stream_bytes: 0,
            stream_count: 0,
            provenance: FileProvenance::Full,
            modified,
        }
    }

    #[test]
    fn refresh_updates_only_what_changed() {
        let mut index = empty();
        let root = [1u8; 16];
        let dir = root_dir();
        let before = [
            file_row([2u8; 16], root, "steady.txt", 4_096, 100),
            file_row([3u8; 16], root, "growing.txt", 4_096, 100),
        ];
        index
            .write_scan(&meta(), std::iter::once(&dir), before.iter())
            .unwrap();

        // One file untouched, one grown: only the grown row is rewritten.
        let after = [
            file_row([2u8; 16], root, "steady.txt", 4_096, 100),
            file_row([3u8; 16], root, "growing.txt", 8_192, 200),
        ];
        let report = index
            .refresh_scan(&meta(), std::iter::once(&dir), after.iter())
            .unwrap();

        assert_eq!(report.added_files, 0);
        assert_eq!(report.removed_files, 0);
        assert_eq!(report.modified_files, 1);
        assert_eq!(index.summary().unwrap().allocated, 12_288);
        let grown = index.search_name("growing", 1).unwrap();
        assert_eq!(grown[0].modified, 200);
    }

    #[test]
    fn refresh_counts_adds_removes_and_moves() {
        let mut index = empty();
        let root = [1u8; 16];
        let child = [2u8; 16];
        let dirs = [
            root_dir(),
            DirRow {
                id: child,
                parent: Some(root),
                name: "data".to_string(),
                path: r"C:\data".to_string(),
                depth: 1,
                index_bytes: 0,
                stream_bytes: 0,
                stream_count: 0,
                provenance: FileProvenance::Full,
            },
        ];
        let before = [
            file_row([3u8; 16], root, "doomed.txt", 4_096, 100),
            file_row([4u8; 16], root, "traveller.txt", 4_096, 100),
        ];
        index
            .write_scan(&meta(), dirs.iter(), before.iter())
            .unwrap();

        // doomed.txt is gone (its parent was scanned, so the absence is proof);
        // traveller.txt moved into data/ keeping its identity; newcomer.txt appeared.
        let after = [
            file_row([4u8; 16], child, "traveller.txt", 4_096, 100),
            file_row([5u8; 16], root, "newcomer.txt", 4_096, 300),
        ];
        let report = index
            .refresh_scan(&meta(), dirs.iter(), after.iter())
            .unwrap();

        assert_eq!(report.added_files, 1);
        assert_eq!(report.removed_files, 1);
        assert_eq!(report.moved_files, 1);
        assert_eq!(report.modified_files, 0);
        assert!(index.search_name("doomed", 5).unwrap().is_empty());
        let moved = index.search_name("traveller", 1).unwrap();
        assert_eq!(moved[0].path, r"C:\data\traveller.txt");
    }

    #[test]
    fn refresh_keeps_rows_under_unscanned_parents() {
        let mut index = empty();
        let root = [1u8; 16];
        let child = [2u8; 16];
        let scanned_vault = DirRow {
            id: child,
            parent: Some(root),
            name: "vault".to_string(),
            path: r"C:\vault".to_string(),
            depth: 1,
            index_bytes: 0,
            stream_bytes: 0,
            stream_count: 0,
            provenance: FileProvenance::Full,
        };
        // The vault is listed but not entered this refresh: its row is present but denied,
        // and its contents are absent. Deleting secret.txt would confuse "could not look"
        // with "looked and found nothing", so it stays and is counted as stale.
        let denied_vault = DirRow {
            provenance: FileProvenance::Denied,
            ..scanned_vault.clone()
        };
        let before = [file_row([3u8; 16], child, "secret.txt", 4_096, 100)];
        index
            .write_scan(&meta(), [root_dir(), scanned_vault].iter(), before.iter())
            .unwrap();

        let report = index
            .refresh_scan(
                &meta(),
                [root_dir(), denied_vault].iter(),
                std::iter::empty(),
            )
            .unwrap();

        assert_eq!(report.removed_files, 0);
        assert_eq!(report.removed_dirs, 0);
        assert_eq!(report.stale_files, 1);
        assert_eq!(index.search_name("secret", 5).unwrap().len(), 1);
    }
}
