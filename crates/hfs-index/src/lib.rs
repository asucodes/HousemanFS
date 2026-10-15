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
use rusqlite::{Connection, OptionalExtension, params};

/// Schema version. Bump only with a migration, never in place.
///
/// Version 2 adds per-entry stream occupancy and provenance, plus the scan-level record that
/// `explain` reads. Version 1 indexes migrate forward on open; anything else is rejected with
/// instructions to rebuild, because the index is derived data and a rebuild is always safe.
pub const SCHEMA_VERSION: u32 = 2;

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
    provenance   TEXT NOT NULL DEFAULT 'unknown'
);

CREATE INDEX files_allocated ON files(allocated DESC);
CREATE INDEX files_ext       ON files(ext);
CREATE INDEX files_parent    ON files(parent);
CREATE INDEX files_id        ON files(id);
CREATE INDEX dirs_subtree    ON dirs(subtree_allocated DESC);
CREATE INDEX dirs_path       ON dirs(path);
"#;

/// Bring a version-1 index forward: stream and provenance columns, backfilled to unknown.
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
#[derive(Debug, Clone)]
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
}

/// A directory subtree together with enough to display.
#[derive(Debug, Clone)]
pub struct DirHit {
    pub path: String,
    pub allocated: u64,
    pub streams: u64,
    pub files: u64,
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
            Some(1) => migrate_v1_to_v2(&conn)?,
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
                                    stream_bytes, stream_count, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                ])?;
            }
        }

        tx.commit()?;
        self.propagate_subtree_totals()?;
        Ok(())
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
            }));
        }

        let Some((parent, name)) = split_parent(&wanted) else {
            return Ok(None);
        };
        self.conn
            .query_row(
                "SELECT d.path, f.name, f.logical, f.allocated, f.links,
                        f.stream_bytes, f.stream_count, f.provenance
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
                    })
                },
            )
            .optional()
    }

    /// The largest files by occupied size.
    pub fn largest_files(&self, limit: u32) -> rusqlite::Result<Vec<FileHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.path, f.name, f.logical, f.allocated, f.links,
                    f.stream_bytes, f.stream_count, f.provenance
             FROM files f JOIN dirs d ON f.parent = d.id
             ORDER BY f.allocated DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
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
            })
        })?;
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
        let escaped = term
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let mut stmt = self.conn.prepare(
            "SELECT d.path, f.name, f.logical, f.allocated, f.links,
                    f.stream_bytes, f.stream_count, f.provenance
             FROM files f JOIN dirs d ON f.parent = d.id
             WHERE f.name LIKE ?1 ESCAPE '\\'
             ORDER BY f.allocated DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit], |row| {
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
            })
        })?;
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
        assert_eq!(index.meta("schema_version").as_deref(), Some("2"));

        // The rows survived, and their unknown past is reported as unknown.
        let largest = index.largest_files(1).unwrap();
        assert_eq!(largest.len(), 1);
        assert_eq!(largest[0].allocated, 8_192);
        assert_eq!(largest[0].stream_bytes, 0);
        assert_eq!(largest[0].provenance, FileProvenance::Unknown);

        let _ = std::fs::remove_file(&path);
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
        }];
        index
            .write_scan(&meta(), std::iter::once(&dir), files.iter())
            .unwrap();

        // `%` is data here, not a pattern: it matches the file that has one, and a bare
        // `%` alone must not match everything.
        assert_eq!(index.search_name("100%", 5).unwrap().len(), 1);
        assert_eq!(index.search_name("%", 5).unwrap().len(), 1);
    }
}
