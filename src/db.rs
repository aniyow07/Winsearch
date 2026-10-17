//! Persistent SQLite index: schema, migrations, recovery and write operations.
//!
//! The database is the single source of truth for search. One writer
//! connection (owned by the indexer thread) mutates it; search and preview
//! code use separate read-only connections, which WAL mode lets run
//! concurrently with writes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::model::FileRecord;
use crate::util;

/// Bump when the schema changes; `migrate` upgrades older databases and
/// rebuilds ones it cannot upgrade.
pub const SCHEMA_VERSION: i32 = 1;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS roots(
    id                 INTEGER PRIMARY KEY,
    path               TEXT NOT NULL UNIQUE COLLATE NOCASE,
    volume_serial      INTEGER,
    fs_name            TEXT,
    scan_gen           INTEGER NOT NULL DEFAULT 0,
    state              TEXT NOT NULL DEFAULT 'new',
    last_complete_scan INTEGER,
    error_count        INTEGER NOT NULL DEFAULT 0,
    usn_journal_id     INTEGER,
    usn_next           INTEGER,
    last_error         TEXT
);
CREATE TABLE IF NOT EXISTS files(
    id         INTEGER PRIMARY KEY,
    root_id    INTEGER NOT NULL,
    path       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    name       TEXT NOT NULL COLLATE NOCASE,
    parent_id  INTEGER NOT NULL,
    ext        TEXT NOT NULL,
    is_dir     INTEGER NOT NULL,
    size       INTEGER NOT NULL,
    created    INTEGER,
    modified   INTEGER,
    attrs      INTEGER NOT NULL DEFAULT 0,
    file_ref   INTEGER,
    seen_gen   INTEGER NOT NULL,
    listed_gen INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS files_parent   ON files(parent_id);
CREATE INDEX IF NOT EXISTS files_ext      ON files(ext);
CREATE INDEX IF NOT EXISTS files_root_gen ON files(root_id, seen_gen);
CREATE INDEX IF NOT EXISTS files_ref      ON files(file_ref) WHERE file_ref IS NOT NULL;

-- Trigram full-text index over file names for substring search
-- (see docs/BENCHMARKS.md for why this was chosen over LIKE scans).
CREATE VIRTUAL TABLE IF NOT EXISTS names_fts USING fts5(
    name, content='files', content_rowid='id', tokenize='trigram'
);
"#;

const TRIGGERS_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS files_name ON files(name);
CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
    INSERT INTO names_fts(rowid, name) VALUES (new.id, new.name);
END;
CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
    INSERT INTO names_fts(names_fts, rowid, name) VALUES ('delete', old.id, old.name);
END;
CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE OF name ON files
WHEN old.name IS NOT new.name BEGIN
    INSERT INTO names_fts(names_fts, rowid, name) VALUES ('delete', old.id, old.name);
    INSERT INTO names_fts(rowid, name) VALUES (new.id, new.name);
END;
"#;

#[derive(Clone, Debug)]
pub struct RootRow {
    pub id: i64,
    pub path: String,
    pub scan_gen: i64,
    pub state: String,
    pub last_complete_scan: Option<i64>,
    pub error_count: i64,
    pub volume_serial: Option<u32>,
    pub usn_journal_id: Option<u64>,
    pub usn_next: Option<i64>,
    pub last_error: Option<String>,
}

pub struct Db {
    pub conn: Connection,
    path: PathBuf,
}

/// What `Db::open` had to do to give back a usable database.
#[derive(Debug, Default, Clone)]
pub struct OpenReport {
    /// Set when an unreadable/corrupt file was moved aside and a fresh
    /// index was created. Holds the backup location.
    pub recovered_from: Option<PathBuf>,
}

fn is_corruption(err: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode::*;
    matches!(
        err.sqlite_error_code(),
        Some(DatabaseCorrupt) | Some(NotADatabase)
    )
}

impl Db {
    /// Opens (creating if needed) the writable index. A corrupt or
    /// unreadable database is renamed to `*.corrupt-<time>` and replaced by
    /// an empty one so the application always starts.
    pub fn open(path: &Path) -> Result<(Db, OpenReport)> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating index directory {}", dir.display()))?;
        }
        match Self::try_open(path) {
            Ok(db) => Ok((db, OpenReport::default())),
            Err(e) => {
                let corrupt = e
                    .downcast_ref::<rusqlite::Error>()
                    .map(is_corruption)
                    .unwrap_or(false)
                    || e.to_string().contains("newer than supported");
                if !corrupt {
                    return Err(e);
                }
                tracing::error!("index database unusable ({e:#}); moving it aside and rebuilding");
                let backup = Self::move_aside(path)?;
                let db = Self::try_open(path)?;
                Ok((db, OpenReport { recovered_from: Some(backup) }))
            }
        }
    }

    fn move_aside(path: &Path) -> Result<PathBuf> {
        let backup = path.with_extension(format!("corrupt-{}", util::now_unix()));
        std::fs::rename(path, &backup)
            .with_context(|| format!("moving corrupt index {} aside", path.display()))?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{}", path.display(), suffix));
            let _ = std::fs::remove_file(side);
        }
        Ok(backup)
    }

    fn try_open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-32768;
             PRAGMA foreign_keys=OFF;",
        )?;
        // Touch the schema so a non-database file fails here, not later.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))?;
        let mut db = Db { conn, path: path.to_path_buf() };
        db.migrate()?;
        Ok(db)
    }

    /// Read-only connection for search/preview threads.
    pub fn open_reader(path: &Path) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.execute_batch("PRAGMA cache_size=-16384; PRAGMA temp_store=MEMORY;")?;
        Ok(conn)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i32 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "index schema version {version} is newer than supported ({SCHEMA_VERSION})"
            );
        }
        if version < SCHEMA_VERSION {
            let tx = self.conn.transaction()?;
            if version != 0 {
                // No in-place upgrade path yet for older layouts: drop and rebuild.
                tx.execute_batch(
                    "DROP TABLE IF EXISTS names_fts; DROP TABLE IF EXISTS files;
                     DROP TABLE IF EXISTS roots; DROP TABLE IF EXISTS meta;",
                )?;
            }
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            tracing::info!("index schema migrated from v{version} to v{SCHEMA_VERSION}");
        }
        // Indexes added after v1 shipped; cheap no-ops once they exist.
        self.conn.execute_batch("CREATE INDEX IF NOT EXISTS files_modified ON files(modified);")?;
        if !fts_stale(&self.conn) {
            self.conn.execute_batch(TRIGGERS_SQL)?;
        }
        Ok(())
    }

    /// Runs SQLite's quick_check. Returns Ok(None) when healthy.
    pub fn quick_check(&self) -> Result<Option<String>> {
        let res: String = self.conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        Ok(if res == "ok" { None } else { Some(res) })
    }

    /// Deletes every indexed entry and resets scan state (used by "Rebuild index").
    pub fn clear_all(&mut self) -> Result<()> {
        self.suspend_fts()?;
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM files;
             UPDATE roots SET state='new', scan_gen=0, last_complete_scan=NULL, error_count=0,
                              usn_journal_id=NULL, usn_next=NULL, last_error=NULL;",
        )?;
        tx.commit()?;
        self.rebuild_fts()?;
        let _ = self.conn.execute_batch("VACUUM;");
        Ok(())
    }

    // ---- trigram index maintenance ----------------------------------------

    /// True while the trigram index is known to be out of date (a bulk load
    /// suspended it, or the process stopped before rebuilding it). Search
    /// falls back to LIKE scans meanwhile.
    pub fn fts_stale(&self) -> Result<bool> {
        Ok(fts_stale(&self.conn))
    }

    /// Stops per-row trigram maintenance for a bulk load. The flag is
    /// persisted first so a crash leaves a state that is detected and
    /// repaired on the next run.
    pub fn suspend_fts(&self) -> Result<()> {
        self.conn.execute_batch(
            "INSERT INTO meta(key,value) VALUES('fts_stale','1')
                ON CONFLICT(key) DO UPDATE SET value='1';
             DROP TRIGGER IF EXISTS files_ai;
             DROP TRIGGER IF EXISTS files_ad;
             DROP TRIGGER IF EXISTS files_au;
             DROP INDEX IF EXISTS files_name;",
        )?;
        Ok(())
    }

    /// Rebuilds the trigram index and the name index from the files table and
    /// re-arms the triggers.
    pub fn rebuild_fts(&self) -> Result<()> {
        let t = std::time::Instant::now();
        self.conn.execute_batch("INSERT INTO names_fts(names_fts) VALUES('rebuild');")?;
        self.conn.execute_batch(TRIGGERS_SQL)?;
        self.conn.execute("UPDATE meta SET value='0' WHERE key='fts_stale'", [])?;
        tracing::info!("rebuilt name index in {:.1}s", t.elapsed().as_secs_f64());
        Ok(())
    }

    pub fn root_row_count(&self, root_id: i64) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM (SELECT 1 FROM files WHERE root_id=?1 LIMIT 1)",
            params![root_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    // ---- roots ---------------------------------------------------------

    pub fn ensure_root(&self, path: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO roots(path) VALUES (?1) ON CONFLICT(path) DO NOTHING",
            params![path],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM roots WHERE path=?1", params![path], |r| r.get(0))?)
    }

    pub fn roots(&self) -> Result<Vec<RootRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, path, scan_gen, state, last_complete_scan, error_count,
                    volume_serial, usn_journal_id, usn_next, last_error
             FROM roots ORDER BY path",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(RootRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    scan_gen: r.get(2)?,
                    state: r.get(3)?,
                    last_complete_scan: r.get(4)?,
                    error_count: r.get(5)?,
                    volume_serial: r.get::<_, Option<i64>>(6)?.map(|v| v as u32),
                    usn_journal_id: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                    usn_next: r.get(8)?,
                    last_error: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn root(&self, path: &str) -> Result<Option<RootRow>> {
        Ok(self.roots()?.into_iter().find(|r| r.path.eq_ignore_ascii_case(path)))
    }

    /// Removes a root and everything indexed under it.
    pub fn remove_root(&mut self, root_id: i64) -> Result<u64> {
        self.suspend_fts()?;
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM files WHERE root_id=?1", params![root_id])?;
        tx.execute("DELETE FROM roots WHERE id=?1", params![root_id])?;
        tx.commit()?;
        self.rebuild_fts()?;
        Ok(n as u64)
    }

    /// Starts a scan of `root_id`. A scan that was interrupted keeps its
    /// generation so it can resume; otherwise a new generation is started.
    /// Returns (generation, resuming).
    pub fn begin_scan(&self, root_id: i64, force_new: bool) -> Result<(i64, bool)> {
        let (gen_, state): (i64, String) = self.conn.query_row(
            "SELECT scan_gen, state FROM roots WHERE id=?1",
            params![root_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if state == "scanning" && gen_ > 0 && !force_new {
            return Ok((gen_, true));
        }
        let new_gen = gen_ + 1;
        self.conn.execute(
            "UPDATE roots SET scan_gen=?2, state='scanning', error_count=0, last_error=NULL WHERE id=?1",
            params![root_id, new_gen],
        )?;
        Ok((new_gen, false))
    }

    /// Completes a scan: anything under the root not seen in this generation
    /// no longer exists and is removed. Returns the number of removed rows.
    pub fn finish_scan(&self, root_id: i64, gen_: i64, errors: u64, last_error: Option<&str>) -> Result<u64> {
        let removed = self.conn.execute(
            "DELETE FROM files WHERE root_id=?1 AND seen_gen<?2",
            params![root_id, gen_],
        )?;
        self.conn.execute(
            "UPDATE roots SET state='complete', last_complete_scan=?2, error_count=?3, last_error=?4 WHERE id=?1",
            params![root_id, util::now_unix(), errors as i64, last_error],
        )?;
        Ok(removed as u64)
    }

    pub fn set_root_state(&self, root_id: i64, state: &str, last_error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE roots SET state=?2, last_error=?3 WHERE id=?1",
            params![root_id, state, last_error],
        )?;
        Ok(())
    }

    pub fn set_root_volume(&self, root_id: i64, serial: Option<u32>, fs_name: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE roots SET volume_serial=?2, fs_name=?3 WHERE id=?1",
            params![root_id, serial.map(|s| s as i64), fs_name],
        )?;
        Ok(())
    }

    pub fn set_root_usn(&self, root_id: i64, journal_id: Option<u64>, next: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE roots SET usn_journal_id=?2, usn_next=?3 WHERE id=?1",
            params![root_id, journal_id.map(|j| j as i64), next],
        )?;
        Ok(())
    }

    /// Paths of directories already fully listed in generation `gen_` plus
    /// their child directories, used to resume an interrupted scan.
    pub fn resume_state(&self, root_id: i64, gen_: i64) -> Result<crate::scanner::ResumeState> {
        let mut state = crate::scanner::ResumeState::default();
        let mut st = self.conn.prepare(
            "SELECT path FROM files WHERE root_id=?1 AND is_dir=1 AND listed_gen=?2",
        )?;
        for p in st.query_map(params![root_id, gen_], |r| r.get::<_, String>(0))? {
            state.listed.insert(p?.to_lowercase());
        }
        let mut st = self.conn.prepare(
            "SELECT p.path, c.path, c.attrs FROM files c JOIN files p ON p.id = c.parent_id
             WHERE c.root_id=?1 AND c.is_dir=1 AND c.seen_gen=?2",
        )?;
        for row in st.query_map(params![root_id, gen_], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })? {
            let (parent, path, attrs) = row?;
            if attrs as u32 & crate::model::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue; // never descended into links
            }
            state.children.entry(parent.to_lowercase()).or_default().push(path);
        }
        Ok(state)
    }

    // ---- entries -------------------------------------------------------

    /// Inserts or updates records in one transaction, stamping them with
    /// `gen_`. Directories in `listed` are marked as fully enumerated.
    pub fn upsert_batch(
        &mut self,
        root_id: i64,
        gen_: i64,
        records: &[FileRecord],
        listed: &[String],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut up = tx.prepare_cached(UPSERT_SQL)?;
            let mut find = tx.prepare_cached("SELECT id FROM files WHERE path=?1")?;
            // Records arrive grouped by directory, so one lookup per run.
            let mut cur_parent: Option<(&str, i64)> = None;
            for r in records {
                let pid = match cur_parent {
                    Some((p, id)) if p == r.parent => id,
                    _ => {
                        let id = find
                            .query_row(params![r.parent], |row| row.get(0))
                            .optional()?
                            .unwrap_or(ROOT_PARENT);
                        cur_parent = Some((&r.parent, id));
                        id
                    }
                };
                up.execute(params![
                    root_id,
                    r.path,
                    r.name,
                    pid,
                    r.ext,
                    r.is_dir as i64,
                    r.size as i64,
                    r.created,
                    r.modified,
                    r.attrs as i64,
                    r.file_ref.map(|f| f as i64),
                    gen_
                ])?;
            }
            let mut mark = tx.prepare_cached("UPDATE files SET listed_gen=?2 WHERE path=?1")?;
            for d in listed {
                mark.execute(params![d, gen_])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Inserts/updates a single record outside a scan (incremental updates).
    pub fn upsert_one(&self, root_id: i64, r: &FileRecord) -> Result<()> {
        let gen_: i64 = self
            .conn
            .query_row("SELECT scan_gen FROM roots WHERE id=?1", params![root_id], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        let pid: i64 = self
            .conn
            .query_row("SELECT id FROM files WHERE path=?1", params![r.parent], |row| row.get(0))
            .optional()?
            .unwrap_or(ROOT_PARENT);
        let mut up = self.conn.prepare_cached(UPSERT_SQL)?;
        up.execute(params![
            root_id,
            r.path,
            r.name,
            pid,
            r.ext,
            r.is_dir as i64,
            r.size as i64,
            r.created,
            r.modified,
            r.attrs as i64,
            r.file_ref.map(|f| f as i64),
            gen_
        ])?;
        Ok(())
    }

    /// Removes `path` and, if it is a directory, everything below it.
    pub fn remove_tree(&self, path: &str) -> Result<u64> {
        let mut n = self.conn.execute("DELETE FROM files WHERE path=?1", params![path])?;
        let prefix = format!("{}\\%", util::escape_like(path.trim_end_matches('\\')));
        n += self
            .conn
            .execute("DELETE FROM files WHERE path LIKE ?1 ESCAPE '^'", params![prefix])?;
        Ok(n as u64)
    }

    /// Re-roots everything below directory `from` to `to` after a rename/move.
    /// The entry for `to` itself is refreshed separately by the caller.
    pub fn rename_tree(&self, from: &str, to: &str, root_id: i64) -> Result<u64> {
        let from = from.trim_end_matches('\\');
        let to = to.trim_end_matches('\\');
        // Anything previously indexed at the destination is stale now. Paths
        // compare without case, so a case-only rename must keep the source.
        if !from.eq_ignore_ascii_case(to) {
            self.remove_tree(to)?;
        }
        // Keep the directory's row id: its children reference it.
        let new_name = util::split_path(to).map(|(_, n)| n).unwrap_or(to);
        self.conn.execute(
            "UPDATE files SET path=?2, name=?3, root_id=?4 WHERE path=?1",
            params![from, to, new_name, root_id],
        )?;
        let prefix = format!("{}\\%", util::escape_like(from));
        let cut = from.chars().count() as i64 + 1; // 1-based substr start of the '\'
        let n = self.conn.execute(
            "UPDATE files SET path = ?2 || substr(path, ?3), root_id = ?4 WHERE path LIKE ?1 ESCAPE '^'",
            params![prefix, to, cut, root_id],
        )?;
        Ok(n as u64)
    }

    pub fn get(&self, path: &str) -> Result<Option<FileRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path,name,ext,is_dir,size,created,modified,attrs,file_ref
                 FROM files WHERE path=?1",
                params![path],
                row_to_record,
            )
            .optional()?)
    }

    /// Indexed children of a directory (or of a root when `dir` is a root).
    pub fn children(&self, dir: &str) -> Result<Vec<FileRecord>> {
        let mut st = self.conn.prepare_cached(
            "SELECT c.path,c.name,c.ext,c.is_dir,c.size,c.created,c.modified,c.attrs,c.file_ref
             FROM files c WHERE c.parent_id = (SELECT id FROM files WHERE path=?1)",
        )?;
        let mut rows = st
            .query_map(params![dir], row_to_record)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.is_empty() && self.root(dir)?.is_some() {
            let mut st = self.conn.prepare_cached(
                "SELECT path,name,ext,is_dir,size,created,modified,attrs,file_ref
                 FROM files WHERE parent_id=?1 AND root_id=(SELECT id FROM roots WHERE path=?2)",
            )?;
            rows = st
                .query_map(params![ROOT_PARENT, dir], row_to_record)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(rows)
    }

    pub fn path_for_ref(&self, root_id: i64, file_ref: u64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path FROM files WHERE file_ref=?2 AND root_id=?1",
                params![root_id, file_ref as i64],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Bigger page cache while a scan writes many rows; released afterwards.
    pub fn set_bulk_mode(&self, on: bool) {
        let sql = if on {
            "PRAGMA cache_size=-196608;"
        } else {
            "PRAGMA cache_size=-32768; PRAGMA shrink_memory;"
        };
        let _ = self.conn.execute_batch(sql);
    }

    pub fn checkpoint(&self) {
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }

    pub fn optimize(&self) {
        let _ = self
            .conn
            .execute_batch("INSERT INTO names_fts(names_fts) VALUES('optimize'); PRAGMA optimize;");
    }
}

/// `parent_id` of entries directly inside an indexed root (or whose parent
/// directory is not in the index).
pub const ROOT_PARENT: i64 = 0;

const UPSERT_SQL: &str = "INSERT INTO files(root_id,path,name,parent_id,ext,is_dir,size,created,modified,attrs,file_ref,seen_gen)
     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
     ON CONFLICT(path) DO UPDATE SET
        root_id=excluded.root_id, name=excluded.name, parent_id=excluded.parent_id,
        ext=excluded.ext, is_dir=excluded.is_dir, size=excluded.size,
        created=excluded.created, modified=excluded.modified, attrs=excluded.attrs,
        file_ref=coalesce(excluded.file_ref, files.file_ref), seen_gen=excluded.seen_gen";

/// Columns: path,name,ext,is_dir,size,created,modified,attrs,file_ref
fn row_to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    let path: String = r.get(0)?;
    let parent = util::split_path(&path).map(|(p, _)| p.to_string()).unwrap_or_default();
    Ok(FileRecord {
        name: r.get(1)?,
        parent,
        ext: r.get(2)?,
        is_dir: r.get::<_, i64>(3)? != 0,
        size: r.get::<_, i64>(4)? as u64,
        created: r.get(5)?,
        modified: r.get(6)?,
        attrs: r.get::<_, i64>(7)? as u32,
        file_ref: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        path,
    })
}

/// Reads the persisted "trigram index out of date" flag on any connection.
pub fn fts_stale(conn: &Connection) -> bool {
    conn.query_row("SELECT value FROM meta WHERE key='fts_stale'", [], |r| r.get::<_, String>(0))
        .map(|v| v == "1")
        .unwrap_or(false)
}
