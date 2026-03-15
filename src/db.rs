use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

/// Wrapper around a SQLite connection for session-scoped state persistence.
///
/// The `Mutex` ensures `Db` is `Send + Sync`, allowing it to be shared via
/// `Arc<Db>` across threads (e.g., wave task execution).
pub struct Db {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

/// Row data for beginning a transcript entry.
pub struct TranscriptRow<'a> {
    pub phase: &'a str,
    pub role: &'a str,
    pub round: u32,
    pub prompt: &'a str,
}

const SCHEMA_V1: &str = "\
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS loop_status (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS documents (
    key TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS transcript_entries (
    id INTEGER PRIMARY KEY,
    phase TEXT,
    role TEXT,
    round INTEGER,
    prompt TEXT,
    output TEXT,
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at TEXT
);
CREATE TABLE IF NOT EXISTS findings (
    key TEXT PRIMARY KEY,
    data TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS task_status (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS task_metrics (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS log_entries (
    id INTEGER PRIMARY KEY,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
";

impl Db {
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("db mutex should not be poisoned")
    }

    /// Open or create a database at the given path.
    /// Sets WAL journal mode and runs schema migrations.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;

        let needs_init = {
            let exists: bool = conn.query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |row| row.get(0),
            )?;
            !exists
        };

        if needs_init {
            conn.execute_batch(SCHEMA_V1)?;
            conn.execute("INSERT INTO schema_version (version) VALUES (?1)", params![1])?;
        }

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Open a read-only connection (for TUI monitoring).
    #[allow(dead_code)]
    pub fn open_readonly(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA query_only=true;")?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    // -- Documents (generic key-value) ----------------------------------------

    pub fn read_document(&self, key: &str) -> String {
        let conn = self.conn();
        conn.query_row(
            "SELECT content FROM documents WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or_default()
    }

    pub fn write_document(&self, key: &str, content: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO documents (key, content, updated_at) VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
            params![key, content],
        )?;
        Ok(())
    }

    // -- Loop status (single-row) ---------------------------------------------

    pub fn read_status(&self) -> Option<String> {
        let conn = self.conn();
        conn.query_row("SELECT data FROM loop_status WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
    }

    pub fn write_status(&self, json: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO loop_status (id, data) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![json],
        )?;
        Ok(())
    }

    // -- Transcript entries (two-phase) ---------------------------------------

    pub fn begin_transcript(&self, entry: &TranscriptRow<'_>) -> Result<i64, rusqlite::Error> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO transcript_entries (phase, role, round, prompt) VALUES (?1, ?2, ?3, ?4)",
            params![entry.phase, entry.role, entry.round, entry.prompt],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn complete_transcript(&self, id: i64, output: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn();
        conn.execute(
            "UPDATE transcript_entries SET output = ?1, completed_at = datetime('now') WHERE id = ?2",
            params![output, id],
        )?;
        Ok(())
    }

    pub fn rotate_transcript(&self, max_entries: usize) -> Result<(), rusqlite::Error> {
        let conn = self.conn();
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM transcript_entries",
            [],
            |row| row.get(0),
        )?;
        if count > max_entries as i64 {
            let to_delete = count - (max_entries as i64 / 2);
            conn.execute(
                "DELETE FROM transcript_entries WHERE id IN (SELECT id FROM transcript_entries ORDER BY id ASC LIMIT ?1)",
                params![to_delete],
            )?;
        }
        Ok(())
    }

    // -- Findings -------------------------------------------------------------

    pub fn read_findings(&self, key: &str) -> String {
        let conn = self.conn();
        conn.query_row(
            "SELECT data FROM findings WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or_default()
    }

    pub fn write_findings(&self, key: &str, json: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO findings (key, data, updated_at) VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            params![key, json],
        )?;
        Ok(())
    }

    // -- Task status (single-row) ---------------------------------------------

    pub fn read_task_status(&self) -> String {
        let conn = self.conn();
        conn.query_row("SELECT data FROM task_status WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
        .unwrap_or_default()
    }

    pub fn write_task_status(&self, json: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO task_status (id, data) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![json],
        )?;
        Ok(())
    }

    // -- Task metrics (single-row) --------------------------------------------

    pub fn read_task_metrics(&self) -> String {
        let conn = self.conn();
        conn.query_row("SELECT data FROM task_metrics WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
        .unwrap_or_default()
    }

    pub fn write_task_metrics(&self, json: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO task_metrics (id, data) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![json],
        )?;
        Ok(())
    }

    // -- Log entries ----------------------------------------------------------

    pub fn append_log(&self, message: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO log_entries (message) VALUES (?1)",
            params![message],
        )?;
        Ok(())
    }

    /// Read recent log entries (most recent first, up to `limit`).
    #[allow(dead_code)]
    pub fn read_recent_logs(&self, limit: usize) -> Vec<String> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT message FROM log_entries ORDER BY id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map(params![limit as i64], |row| row.get::<_, String>(0))
            .ok();
        match rows {
            Some(iter) => iter.filter_map(|r| r.ok()).collect(),
            None => Vec::new(),
        }
    }

    // -- Transactions ---------------------------------------------------------

    /// Execute a closure within a SQLite transaction.
    pub fn transaction<F, R>(&self, f: F) -> Result<R, rusqlite::Error>
    where
        F: FnOnce(&Db) -> Result<R, rusqlite::Error>,
    {
        // We must hold the lock for the entire transaction to prevent
        // interleaved operations from other threads.
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        // Drop the lock temporarily so the closure can re-acquire it
        drop(conn);

        match f(self) {
            Ok(result) => {
                self.conn().execute_batch("COMMIT")?;
                Ok(result)
            }
            Err(err) => {
                let _ = self.conn().execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent_loop_db_test_{name}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("test.db")
    }

    #[test]
    fn open_creates_schema_and_sets_wal() {
        let path = temp_db_path("open");
        let db = Db::open(&path).expect("open should succeed");
        let conn = db.conn();
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        let version: i32 = conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 1);
        drop(conn);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn document_round_trip() {
        let path = temp_db_path("doc_rt");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.read_document("missing"), "");
        db.write_document("key1", "value1").unwrap();
        assert_eq!(db.read_document("key1"), "value1");
        db.write_document("key1", "updated").unwrap();
        assert_eq!(db.read_document("key1"), "updated");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn status_round_trip() {
        let path = temp_db_path("status_rt");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.read_status(), None);
        db.write_status(r#"{"status":"PENDING"}"#).unwrap();
        assert_eq!(db.read_status().unwrap(), r#"{"status":"PENDING"}"#);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn transcript_two_phase() {
        let path = temp_db_path("transcript");
        let db = Db::open(&path).unwrap();
        let id = db.begin_transcript(&TranscriptRow {
            phase: "implement",
            role: "implementer",
            round: 1,
            prompt: "do something",
        }).unwrap();
        assert!(id > 0);
        db.complete_transcript(id, "done").unwrap();
        let output: String = db.conn().query_row(
            "SELECT output FROM transcript_entries WHERE id = ?1",
            params![id],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(output, "done");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn findings_round_trip() {
        let path = temp_db_path("findings");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.read_findings("f.json"), "");
        db.write_findings("f.json", r#"{"round":1}"#).unwrap();
        assert_eq!(db.read_findings("f.json"), r#"{"round":1}"#);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn task_status_and_metrics_round_trip() {
        let path = temp_db_path("task_sm");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.read_task_status(), "");
        db.write_task_status(r#"{"tasks":[]}"#).unwrap();
        assert_eq!(db.read_task_status(), r#"{"tasks":[]}"#);
        assert_eq!(db.read_task_metrics(), "");
        db.write_task_metrics(r#"{"tasks":[]}"#).unwrap();
        assert_eq!(db.read_task_metrics(), r#"{"tasks":[]}"#);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn append_log_preserves_ordering() {
        let path = temp_db_path("log_order");
        let db = Db::open(&path).unwrap();
        db.append_log("first").unwrap();
        db.append_log("second").unwrap();
        db.append_log("third").unwrap();
        let recent = db.read_recent_logs(2);
        assert_eq!(recent, vec!["third", "second"]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn transaction_commits_on_success() {
        let path = temp_db_path("tx_ok");
        let db = Db::open(&path).unwrap();
        db.transaction(|db| {
            db.write_document("a", "1")?;
            db.write_document("b", "2")?;
            Ok(())
        }).unwrap();
        assert_eq!(db.read_document("a"), "1");
        assert_eq!(db.read_document("b"), "2");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let path = temp_db_path("tx_err");
        let db = Db::open(&path).unwrap();
        db.write_document("x", "original").unwrap();
        let result: Result<(), _> = db.transaction(|db| {
            db.write_document("x", "changed")?;
            Err(rusqlite::Error::QueryReturnedNoRows) // simulate error
        });
        assert!(result.is_err());
        assert_eq!(db.read_document("x"), "original");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rotate_transcript_trims_old_entries() {
        let path = temp_db_path("rotate");
        let db = Db::open(&path).unwrap();
        for i in 0..10 {
            db.begin_transcript(&TranscriptRow {
                phase: "test",
                role: "impl",
                round: i,
                prompt: &format!("prompt {i}"),
            }).unwrap();
        }
        db.rotate_transcript(6).unwrap();
        let count: i64 = db.conn().query_row("SELECT count(*) FROM transcript_entries", [], |r| r.get(0)).unwrap();
        assert!(count <= 6, "expected <= 6 entries after rotation, got {count}");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn wal_allows_concurrent_read() {
        let path = temp_db_path("wal_concurrent");
        let writer = Db::open(&path).unwrap();
        writer.write_document("key", "value").unwrap();
        let reader = Db::open_readonly(&path).unwrap();
        assert_eq!(reader.read_document("key"), "value");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
