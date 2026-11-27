use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{Row, SCHEMA_VERSION};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  session_id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,
  parent_session_id TEXT, run_id TEXT,
  cwd TEXT NOT NULL, project_root TEXT,
  title TEXT, preview TEXT, labels TEXT NOT NULL DEFAULT '[]', archived_at_ms INTEGER,
  model TEXT, provider TEXT, git_branch TEXT,
  created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL, seq INTEGER NOT NULL UNIQUE,
  message_count INTEGER NOT NULL DEFAULT 0,
  tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0,
  format_version INTEGER NOT NULL,
  indexed_len INTEGER NOT NULL, indexed_mtime INTEGER NOT NULL,
  indexed_dev INTEGER NOT NULL, indexed_ino INTEGER NOT NULL,
  indexed_lock_dev INTEGER NOT NULL, indexed_lock_ino INTEGER NOT NULL, indexed_lock_len INTEGER NOT NULL,
  verified_len INTEGER, verified_mtime INTEGER,
  last_input TEXT
);
CREATE INDEX IF NOT EXISTS sessions_list ON sessions(kind, archived_at_ms, seq DESC);
CREATE INDEX IF NOT EXISTS sessions_project ON sessions(project_root, seq DESC);
CREATE INDEX IF NOT EXISTS sessions_cwd ON sessions(cwd, seq DESC);
CREATE INDEX IF NOT EXISTS sessions_parent ON sessions(parent_session_id);
CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v);
INSERT OR IGNORE INTO kv (k, v) VALUES ('seq', 0);
";

pub(super) const COLS: &str = "session_id, path, kind, parent_session_id, run_id, cwd, project_root, title, preview, labels, archived_at_ms, \
    model, provider, git_branch, created_at_ms, updated_at_ms, seq, message_count, tokens_in, tokens_out, \
    format_version, indexed_len, indexed_mtime, verified_len, verified_mtime, last_input, indexed_dev, indexed_ino, \
    indexed_lock_dev, indexed_lock_ino, indexed_lock_len";

/// How the database was opened in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// a new database (first run, or the file was missing)
    Created,
    /// the old file was unusable (reason) and was set aside
    Rebuilt(String),
}

pub(super) fn row_of(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    let labels: String = r.get(9)?;
    Ok(Row {
        session_id: r.get(0)?,
        path: r.get(1)?,
        kind: r.get(2)?,
        parent_session_id: r.get(3)?,
        run_id: r.get(4)?,
        cwd: r.get(5)?,
        project_root: r.get(6)?,
        title: r.get(7)?,
        preview: r.get(8)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        archived_at_ms: r.get(10)?,
        model: r.get(11)?,
        provider: r.get(12)?,
        git_branch: r.get(13)?,
        created_at_ms: r.get(14)?,
        updated_at_ms: r.get(15)?,
        seq: r.get(16)?,
        message_count: r.get(17)?,
        tokens_in: r.get(18)?,
        tokens_out: r.get(19)?,
        format_version: r.get(20)?,
        indexed_len: r.get(21)?,
        indexed_mtime: r.get(22)?,
        verified_len: r.get(23)?,
        verified_mtime: r.get(24)?,
        last_input: r.get(25)?,
        indexed_dev: r.get(26)?,
        indexed_ino: r.get(27)?,
        indexed_lock_dev: r.get(28)?,
        indexed_lock_ino: r.get(29)?,
        indexed_lock_len: r.get(30)?,
    })
}

pub(super) fn write_row(c: &Connection, r: &Row) -> rusqlite::Result<()> {
    c.execute(
        &format!(
            "INSERT OR REPLACE INTO sessions ({COLS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, \
              ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31)"
        ),
        params![
            r.session_id,
            r.path,
            r.kind,
            r.parent_session_id,
            r.run_id,
            r.cwd,
            r.project_root,
            r.title,
            r.preview,
            serde_json::to_string(&r.labels).unwrap_or_else(|_| "[]".into()),
            r.archived_at_ms,
            r.model,
            r.provider,
            r.git_branch,
            r.created_at_ms,
            r.updated_at_ms,
            r.seq,
            r.message_count,
            r.tokens_in,
            r.tokens_out,
            r.format_version,
            r.indexed_len,
            r.indexed_mtime,
            r.verified_len,
            r.verified_mtime,
            r.last_input,
            r.indexed_dev,
            r.indexed_ino,
            r.indexed_lock_dev,
            r.indexed_lock_ino,
            r.indexed_lock_len,
        ],
    )?;
    Ok(())
}

pub(super) fn by_path(c: &Connection, path: &str) -> rusqlite::Result<Option<Row>> {
    c.query_row(&format!("SELECT {COLS} FROM sessions WHERE path = ?1"), [path], row_of).optional()
}

/// The next `seq` (inside the caller's write transaction).
pub(super) fn next_seq(c: &Connection) -> rusqlite::Result<i64> {
    c.query_row("UPDATE kv SET v = v + 1 WHERE k = 'seq' RETURNING v", [], |r| r.get(0))
}

pub(super) fn kv_get(c: &Connection, k: &str) -> rusqlite::Result<Option<String>> {
    c.query_row("SELECT CAST(v AS TEXT) FROM kv WHERE k = ?1", [k], |r| r.get(0)).optional()
}

pub(super) fn kv_set(c: &Connection, k: &str, v: impl rusqlite::ToSql) -> rusqlite::Result<()> {
    c.execute("INSERT OR REPLACE INTO kv (k, v) VALUES (?1, ?2)", params![k, v])?;
    Ok(())
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Why opening failed: an unusable file (set it aside and rebuild) or anything else (busy,
/// permissions, I/O: leave the file alone and work without the index for now).
pub(super) enum OpenError {
    Bad(String),
    Unsupported(String),
    Other(anyhow::Error),
}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ffi::ErrorCode;
        match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => Self::Bad(e.to_string()),
            _ => Self::Other(e.into()),
        }
    }
}

/// Open (creating the schema when the file is new). Returns the connection, whether the
/// schema was just created, and the journal mode in effect.
pub(super) fn connect(db: &Path) -> std::result::Result<(Connection, bool, String), OpenError> {
    if let Some(parent) = db.parent() {
        mare_shared::private::create_dir_all(parent).map_err(|e| OpenError::Other(e.into()))?;
    }
    // Pre-create `index.sqlite` with private permissions (0600 on Unix, protected DACL on Windows)
    // so SQLite creates `-wal` / `-shm` sidecars with matching private permissions too.
    match mare_shared::private::create_new(db) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(OpenError::Other(e.into())),
    }
    let mut conn = Connection::open(db)?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    let journal = if mode.eq_ignore_ascii_case("wal") {
        "wal".to_string()
    } else {
        conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get::<_, String>(0))?.to_lowercase()
    };
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let mut fresh = false;
    if version == 0 {
        // another process may be creating it at the same moment: one of us does, under the lock
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let v: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v == 0 {
            let has_tables: i64 = tx.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            if has_tables > 0 {
                return Err(OpenError::Unsupported("schema version 0 with existing tables".into()));
            }
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            fresh = true;
        } else if v != SCHEMA_VERSION {
            return Err(OpenError::Unsupported(format!("schema version {v}, expected {SCHEMA_VERSION}")));
        }
        tx.commit()?;
    } else if version != SCHEMA_VERSION {
        return Err(OpenError::Unsupported(format!("schema version {version}, expected {SCHEMA_VERSION}")));
    } else {
        let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(OpenError::Bad(format!("quick_check: {check}")));
        }
    }
    Ok((conn, fresh, journal))
}

/// Remove derived index files only while holding the exclusive index lock.
pub(super) fn remove_index(db: &Path) -> Result<()> {
    for ext in ["-wal", "-shm", ""] {
        let mut name = db.as_os_str().to_owned();
        name.push(ext);
        let path = PathBuf::from(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing session index {}", path.display())),
        }
    }
    Ok(())
}

/// Move an unusable database out of the way (`index.sqlite.bad-<unix ts>`).
pub(super) fn set_aside(db: &Path) -> Result<PathBuf> {
    let ts = now_ms() / 1000;
    let mut bad = db.as_os_str().to_owned();
    bad.push(format!(".bad-{ts}"));
    let bad = PathBuf::from(bad);
    std::fs::rename(db, &bad).with_context(|| format!("setting aside {}", db.display()))?;
    for ext in ["-wal", "-shm"] {
        let mut p = db.as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(p);
    }
    Ok(bad)
}
