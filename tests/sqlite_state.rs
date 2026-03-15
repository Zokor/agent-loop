//! Integration tests for SQLite-backed state persistence.

mod support {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn temp_project_root(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "{prefix}_{}_{}_{}", std::process::id(), nanos, seq
        ));
        fs::create_dir_all(&root).expect("create temp root");
        root
    }
}

use std::fs;

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

fn open_test_db(prefix: &str) -> (std::path::PathBuf, rusqlite::Connection) {
    let root = support::temp_project_root(prefix);
    let db_path = root.join("test.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    conn.execute_batch(SCHEMA_V1).unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (1)", []).unwrap();
    (root, conn)
}

#[test]
fn document_round_trip_through_db() {
    let (root, conn) = open_test_db("sqlite_doc_rt");

    conn.execute(
        "INSERT INTO documents (key, content) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET content = excluded.content",
        rusqlite::params!["task.md", "Build feature X"],
    ).unwrap();

    let content: String = conn.query_row(
        "SELECT content FROM documents WHERE key = ?1",
        rusqlite::params!["task.md"],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(content, "Build feature X");

    conn.execute(
        "INSERT INTO documents (key, content) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET content = excluded.content",
        rusqlite::params!["task.md", "Updated task"],
    ).unwrap();
    let updated: String = conn.query_row(
        "SELECT content FROM documents WHERE key = ?1",
        rusqlite::params!["task.md"],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(updated, "Updated task");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn status_merge_patch_through_db() {
    let (root, conn) = open_test_db("sqlite_status_merge");

    let status_json = r#"{"status":"PENDING","round":0,"implementer":"claude","reviewer":"codex","mode":"dual-agent","lastRunTask":"test","timestamp":"2026-01-01T00:00:00.000Z"}"#;
    conn.execute(
        "INSERT INTO loop_status (id, data) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        rusqlite::params![status_json],
    ).unwrap();

    let read: String = conn.query_row("SELECT data FROM loop_status WHERE id = 1", [], |r| r.get(0)).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&read).unwrap();
    assert_eq!(parsed["status"], "PENDING");
    assert_eq!(parsed["round"], 0);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn transcript_two_phase_write() {
    let (root, conn) = open_test_db("sqlite_transcript");

    conn.execute(
        "INSERT INTO transcript_entries (phase, role, round, prompt) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params!["implement", "implementer", 1, "write code"],
    ).unwrap();
    let row_id = conn.last_insert_rowid();
    assert!(row_id > 0);

    let output: Option<String> = conn.query_row(
        "SELECT output FROM transcript_entries WHERE id = ?1",
        rusqlite::params![row_id],
        |r| r.get(0),
    ).unwrap();
    assert!(output.is_none());

    conn.execute(
        "UPDATE transcript_entries SET output = ?1, completed_at = datetime('now') WHERE id = ?2",
        rusqlite::params!["code written", row_id],
    ).unwrap();

    let completed_output: String = conn.query_row(
        "SELECT output FROM transcript_entries WHERE id = ?1",
        rusqlite::params![row_id],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(completed_output, "code written");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn log_entries_preserve_insertion_order() {
    let (root, conn) = open_test_db("sqlite_log_order");

    for msg in ["first", "second", "third"] {
        conn.execute("INSERT INTO log_entries (message) VALUES (?1)", rusqlite::params![msg]).unwrap();
    }

    let mut stmt = conn.prepare("SELECT message FROM log_entries ORDER BY id ASC").unwrap();
    let msgs: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().filter_map(|r| r.ok()).collect();
    assert_eq!(msgs, vec!["first", "second", "third"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn transaction_atomicity() {
    let (root, conn) = open_test_db("sqlite_tx_atom");

    conn.execute("INSERT INTO documents (key, content) VALUES ('x', 'original')", []).unwrap();

    conn.execute_batch("BEGIN").unwrap();
    conn.execute(
        "INSERT INTO documents (key, content) VALUES ('x', 'changed') ON CONFLICT(key) DO UPDATE SET content = excluded.content",
        [],
    ).unwrap();
    conn.execute_batch("ROLLBACK").unwrap();

    let content: String = conn.query_row(
        "SELECT content FROM documents WHERE key = 'x'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(content, "original");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn wal_concurrent_read_during_write() {
    let (root, writer) = open_test_db("sqlite_wal_concurrent");
    let db_path = root.join("test.db");

    writer.execute("INSERT INTO documents (key, content) VALUES ('key', 'value1')", []).unwrap();

    let reader = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ).unwrap();

    let content: String = reader.query_row(
        "SELECT content FROM documents WHERE key = 'key'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(content, "value1");

    writer.execute(
        "INSERT INTO documents (key, content) VALUES ('key', 'value2') ON CONFLICT(key) DO UPDATE SET content = excluded.content",
        [],
    ).unwrap();

    let updated: String = reader.query_row(
        "SELECT content FROM documents WHERE key = 'key'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(updated, "value2");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn findings_round_trip_through_db() {
    let (root, conn) = open_test_db("sqlite_findings");

    let findings_json = r#"{"round":1,"findings":[{"id":"F-001","severity":"high","summary":"Missing error handling"}]}"#;
    conn.execute(
        "INSERT INTO findings (key, data) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET data = excluded.data",
        rusqlite::params!["findings.json", findings_json],
    ).unwrap();

    let read: String = conn.query_row(
        "SELECT data FROM findings WHERE key = ?1",
        rusqlite::params!["findings.json"],
        |r| r.get(0),
    ).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&read).unwrap();
    assert_eq!(parsed["round"], 1);
    assert_eq!(parsed["findings"][0]["id"], "F-001");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn task_status_and_metrics_round_trip() {
    let (root, conn) = open_test_db("sqlite_task_sm");

    let status_json = r#"{"tasks":[{"title":"Task 1","status":"done","retries":0}]}"#;
    conn.execute(
        "INSERT INTO task_status (id, data) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        rusqlite::params![status_json],
    ).unwrap();
    let read: String = conn.query_row("SELECT data FROM task_status WHERE id = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(read, status_json);

    let metrics_json = r#"{"tasks":[{"title":"Task 1","duration_ms":1234}]}"#;
    conn.execute(
        "INSERT INTO task_metrics (id, data) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        rusqlite::params![metrics_json],
    ).unwrap();
    let read_m: String = conn.query_row("SELECT data FROM task_metrics WHERE id = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(read_m, metrics_json);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn transcript_rotation_trims_oldest_entries() {
    let (root, conn) = open_test_db("sqlite_transcript_rot");

    for i in 0..20 {
        conn.execute(
            "INSERT INTO transcript_entries (phase, role, round, prompt) VALUES ('test', 'impl', ?1, 'prompt')",
            rusqlite::params![i],
        ).unwrap();
    }

    let count: i64 = conn.query_row("SELECT count(*) FROM transcript_entries", [], |r| r.get(0)).unwrap();
    if count > 10 {
        let to_delete = count - 5;
        conn.execute(
            "DELETE FROM transcript_entries WHERE id IN (SELECT id FROM transcript_entries ORDER BY id ASC LIMIT ?1)",
            rusqlite::params![to_delete],
        ).unwrap();
    }

    let remaining: i64 = conn.query_row("SELECT count(*) FROM transcript_entries", [], |r| r.get(0)).unwrap();
    assert!(remaining <= 10, "expected <= 10 after rotation, got {remaining}");

    let _ = fs::remove_dir_all(&root);
}
