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

use rusqlite::{Connection, OptionalExtension, params};

/// Schema version. Bump only with a migration, never in place.
pub const SCHEMA_VERSION: u32 = 1;

const SCHEMA: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Directories carry their full path, because answering "which directory is
-- biggest" needs the path and reconstructing it per query would be slow.
-- `subtree_allocated` is the total beneath the directory, not just its own
-- index blocks.
CREATE TABLE dirs (
    id                BLOB PRIMARY KEY,
    parent            BLOB,
    name              TEXT NOT NULL,
    path              TEXT NOT NULL,
    depth             INTEGER NOT NULL,
    index_bytes       INTEGER NOT NULL DEFAULT 0,
    subtree_logical   INTEGER NOT NULL DEFAULT 0,
    subtree_allocated INTEGER NOT NULL DEFAULT 0,
    subtree_files     INTEGER NOT NULL DEFAULT 0
);

-- No primary key on `id`: two hardlinks share one file ID and are two rows.
-- That is deliberate. It is what lets a query count how many names point at
-- the same data.
CREATE TABLE files (
    id        BLOB NOT NULL,
    parent    BLOB NOT NULL,
    name      TEXT NOT NULL,
    ext       TEXT NOT NULL,
    logical   INTEGER NOT NULL,
    allocated INTEGER NOT NULL,
    links     INTEGER NOT NULL
);

CREATE INDEX files_allocated ON files(allocated DESC);
CREATE INDEX files_ext       ON files(ext);
CREATE INDEX files_parent    ON files(parent);
CREATE INDEX files_id        ON files(id);
CREATE INDEX dirs_subtree    ON dirs(subtree_allocated DESC);
"#;

/// An open index.
pub struct Index {
    conn: Connection,
    /// Directory path, so path handling stays in one place.
    path: std::path::PathBuf,
}

/// Facts about the volume a scan was taken from.
#[derive(Debug, Clone)]
pub struct ScanMetadata {
    pub root: String,
    pub filesystem: String,
    pub volume_id: [u8; 16],
    pub cluster_bytes: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
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
}

/// A file together with enough of its location to display.
#[derive(Debug, Clone)]
pub struct FileHit {
    pub path: String,
    pub logical: u64,
    pub allocated: u64,
    pub links: u32,
}

/// Aggregate totals for the indexed volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub files: u64,
    pub directories: u64,
    pub logical: u64,
    pub allocated: u64,
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

        let existing: Option<u32> = if has_meta {
            conn.query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|v: i64| v as u32)
        } else {
            None
        };

        match existing {
            None => conn.execute_batch(SCHEMA)?,
            Some(v) if v == SCHEMA_VERSION => {}
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
            params![SCHEMA_VERSION],
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
                "INSERT INTO dirs (id, parent, name, path, depth, index_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for dir in dirs {
                stmt.execute(params![
                    dir.id.to_vec(),
                    dir.parent.map(|p| p.to_vec()),
                    dir.name,
                    dir.path,
                    dir.depth,
                    dir.index_bytes as i64,
                ])?;
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (id, parent, name, ext, logical, allocated, links)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
        let mut direct: std::collections::HashMap<Vec<u8>, (i64, i64, i64)> =
            std::collections::HashMap::new();

        {
            let mut stmt = self.conn.prepare(
                "SELECT parent, SUM(logical), SUM(allocated), COUNT(*) FROM files GROUP BY parent",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let parent: Vec<u8> = row.get(0)?;
                direct.insert(parent, (row.get(1)?, row.get(2)?, row.get::<_, i64>(3)?));
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
                 subtree_files = ?4 WHERE id = ?1",
            )?;
            for (id, _depth) in depths {
                // Everything attributed to this directory: its own files, plus whatever its
                // children pushed up. Children are processed first because the list runs
                // deepest first, so by the time a directory is reached its total is complete.
                let total = direct.remove(&id).unwrap_or((0, 0, 0));

                stmt.execute(params![id.clone(), total.0, total.1, total.2])?;

                if let Some(Some(parent)) = parents.get(&id) {
                    let entry = direct.entry(parent.clone()).or_insert((0, 0, 0));
                    entry.0 += total.0;
                    entry.1 += total.1;
                    entry.2 += total.2;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn summary(&self) -> rusqlite::Result<Summary> {
        let files = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))?
            as u64;
        let directories = self
            .conn
            .query_row("SELECT COUNT(*) FROM dirs", [], |r| r.get::<_, i64>(0))?
            as u64;
        let logical = self
            .conn
            .query_row("SELECT COALESCE(SUM(logical), 0) FROM files", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0) as u64;
        let allocated = self
            .conn
            .query_row("SELECT COALESCE(SUM(allocated), 0) FROM files", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0) as u64;

        Ok(Summary {
            files,
            directories,
            logical,
            allocated,
        })
    }

    /// The largest files by occupied size.
    pub fn largest_files(&self, limit: u32) -> rusqlite::Result<Vec<FileHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.path, f.name, f.logical, f.allocated, f.links
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
            })
        })?;
        rows.collect()
    }

    /// The largest directory subtrees by occupied size.
    pub fn largest_dirs(&self, limit: u32) -> rusqlite::Result<Vec<(String, u64, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, subtree_allocated, subtree_files
             FROM dirs ORDER BY subtree_allocated DESC LIMIT ?1",
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

    /// Files whose name contains `term`, largest first.
    pub fn search_name(&self, term: &str, limit: u32) -> rusqlite::Result<Vec<FileHit>> {
        let pattern = format!("%{term}%");
        let mut stmt = self.conn.prepare(
            "SELECT d.path, f.name, f.logical, f.allocated, f.links
             FROM files f JOIN dirs d ON f.parent = d.id
             WHERE f.name LIKE ?1
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
            })
        })?;
        rows.collect()
    }

    /// Total occupied by files with one extension.
    pub fn by_extension(&self, limit: u32) -> rusqlite::Result<Vec<(String, u64, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ext, SUM(allocated), COUNT(*)
             FROM files GROUP BY ext ORDER BY SUM(allocated) DESC LIMIT ?1",
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

/// Split a filename into its extension, lowercased, without the dot.
///
/// Returns an empty string for names with no extension. Leading dots are not treated as
/// extensions, so `.gitignore` has none — which is what a user would expect.
pub fn extension_of(name: &str) -> String {
    let stem = name.rsplit(['\\', '/']).next().unwrap_or(name);
    match stem.rsplit_once('.') {
        Some((prefix, ext)) if !prefix.is_empty() && !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Index {
        Index::open(":memory:").expect("in-memory index opens")
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
        let dir = DirRow {
            id: root,
            parent: None,
            name: "C:".to_string(),
            path: r"C:\".to_string(),
            depth: 0,
            index_bytes: 4_096,
        };
        let files = [
            FileRow {
                id: [2u8; 16],
                parent: root,
                name: "big.bin".to_string(),
                ext: "bin".to_string(),
                logical: 100,
                allocated: 8_192,
                links: 1,
            },
            FileRow {
                id: [3u8; 16],
                parent: root,
                name: "small.txt".to_string(),
                ext: "txt".to_string(),
                logical: 10,
                allocated: 4_096,
                links: 1,
            },
        ];

        let meta = ScanMetadata {
            root: r"C:\".to_string(),
            filesystem: "NTFS".to_string(),
            volume_id: [9u8; 16],
            cluster_bytes: 4_096,
            total_bytes: 1_000_000,
            free_bytes: 500_000,
        };

        index
            .write_scan(&meta, std::iter::once(&dir), files.iter())
            .unwrap();

        let summary = index.summary().unwrap();
        assert_eq!(summary.files, 2);
        assert_eq!(summary.allocated, 12_288);

        let largest = index.largest_files(1).unwrap();
        assert_eq!(largest[0].path, r"C:\big.bin");
        assert_eq!(largest[0].allocated, 8_192);

        let found = index.search_name("small", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, r"C:\small.txt");
    }

    #[test]
    fn hardlinks_are_separate_rows_sharing_one_id() {
        let mut index = empty();
        let root = [1u8; 16];
        let dir = DirRow {
            id: root,
            parent: None,
            name: "C:".to_string(),
            path: r"C:\".to_string(),
            depth: 0,
            index_bytes: 0,
        };
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
            },
            FileRow {
                id: [7u8; 16],
                parent: root,
                name: "b.dat".to_string(),
                ext: "dat".to_string(),
                logical: 50,
                allocated: 4_096,
                links: 2,
            },
        ];
        let meta = ScanMetadata {
            root: r"C:\".to_string(),
            filesystem: "NTFS".to_string(),
            volume_id: [9u8; 16],
            cluster_bytes: 4_096,
            total_bytes: 1_000,
            free_bytes: 500,
        };
        index
            .write_scan(&meta, std::iter::once(&dir), files.iter())
            .unwrap();

        assert_eq!(index.summary().unwrap().files, 2, "both names exist");
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
            DirRow {
                id: root,
                parent: None,
                name: "C:".to_string(),
                path: r"C:\".to_string(),
                depth: 0,
                index_bytes: 0,
            },
            DirRow {
                id: child,
                parent: Some(root),
                name: "data".to_string(),
                path: r"C:\data".to_string(),
                depth: 1,
                index_bytes: 0,
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
        }];
        let meta = ScanMetadata {
            root: r"C:\".to_string(),
            filesystem: "NTFS".to_string(),
            volume_id: [9u8; 16],
            cluster_bytes: 4_096,
            total_bytes: 1_000,
            free_bytes: 500,
        };
        index.write_scan(&meta, dirs.iter(), files.iter()).unwrap();

        let largest = index.largest_dirs(2).unwrap();
        // The parent owns the child's total too.
        let root_total = largest.iter().find(|(p, _, _)| p == r"C:\").unwrap().1;
        assert_eq!(root_total, 4_096);
    }
}
