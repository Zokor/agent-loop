use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Lock-file on-disk format
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LockFileContent {
    pub pid: u32,
    pub started_at: String,
    pub mode: String,
    pub max_parallel: u32,
}

// ---------------------------------------------------------------------------
// WaveRunLock
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[allow(dead_code)]
pub struct WaveRunLock {
    pub pid: u32,
    pub started_at: String,
    pub mode: String,
    pub max_parallel: u32,
    pub lock_path: PathBuf,
    /// Guards against deleting a lock file acquired by a *different* process
    /// in the window between an explicit `release()` call and `Drop`.
    released: AtomicBool,
}

impl WaveRunLock {
    /// Acquire an exclusive wave-run lock.
    ///
    /// * If the lock file already exists and is held by a **live** process that
    ///   is not stale, an error is returned.
    /// * If the lock file exists but the owning PID is dead **or** the file is
    ///   older than `stale_seconds`, the lock is reclaimed with a warning
    ///   printed to stderr.
    /// * Otherwise a new lock file is written.
    pub fn acquire(
        lock_path: PathBuf,
        mode: &str,
        max_parallel: u32,
        stale_seconds: u64,
    ) -> Result<Self, String> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create lock directory: {e}"))?;
        }

        let pid = std::process::id();
        let started_at = now_iso8601();

        let content = LockFileContent {
            pid,
            started_at: started_at.clone(),
            mode: mode.to_string(),
            max_parallel,
        };

        let json = serde_json::to_string(&content)
            .map_err(|e| format!("Failed to serialize lock file: {e}"))?;

        // Attempt atomic creation via O_CREAT | O_EXCL to avoid TOCTOU.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                file.write_all(json.as_bytes())
                    .map_err(|e| format!("Failed to write lock file: {e}"))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Lock file exists — check if reclaimable.
                let raw = fs::read_to_string(&lock_path)
                    .map_err(|e| format!("Failed to read lock file: {e}"))?;
                let existing: LockFileContent = serde_json::from_str(&raw)
                    .map_err(|e| format!("Failed to parse lock file: {e}"))?;

                let stale = is_lock_stale(&lock_path, stale_seconds);
                let alive = is_pid_alive(existing.pid);

                if alive && !stale {
                    return Err(format!(
                        "Wave run already in progress (PID {}, started {}). \
                         Use 'agent-loop reset --wave-lock' to force.",
                        existing.pid, existing.started_at
                    ));
                }

                // Reclaim the stale / dead-owner lock.
                if !alive {
                    eprintln!(
                        "Warning: reclaiming wave lock from dead process (PID {})",
                        existing.pid
                    );
                } else {
                    eprintln!(
                        "Warning: reclaiming stale wave lock (older than {}s) from PID {}",
                        stale_seconds, existing.pid
                    );
                }

                // Overwrite with our lock content.
                fs::write(&lock_path, json)
                    .map_err(|e| format!("Failed to write lock file: {e}"))?;
            }
            Err(e) => {
                return Err(format!("Failed to create lock file: {e}"));
            }
        }

        Ok(Self {
            pid,
            started_at,
            mode: mode.to_string(),
            max_parallel,
            lock_path,
            released: AtomicBool::new(false),
        })
    }

    /// Release the lock by deleting the lock file.
    ///
    /// Idempotent: the first call deletes the lock file; subsequent calls
    /// (including the implicit one from `Drop`) are no-ops, preventing the
    /// race where another process re-acquires the lock between an explicit
    /// `release()` and `Drop`.
    pub fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            // Already released — do nothing.
            return;
        }
        let _ = fs::remove_file(&self.lock_path);
    }

    /// Returns `true` when the lock file's modification time is older than
    /// `stale_seconds` from *now*.
    #[cfg(test)]
    fn is_stale(&self, stale_seconds: u64) -> bool {
        is_lock_stale(&self.lock_path, stale_seconds)
    }
}

impl Drop for WaveRunLock {
    /// RAII release: ensures the lock file is removed even when the holder
    /// exits via an early `?` return or panic. The `released` flag makes this
    /// a true no-op after an explicit `release()`, preventing deletion of a
    /// lock file that was re-acquired by another process.
    fn drop(&mut self) {
        self.release();
    }
}

// ---------------------------------------------------------------------------
// PID liveness check
// ---------------------------------------------------------------------------

/// Check whether a process with the given PID is still alive.
///
/// On Unix this sends signal 0 via `libc::kill`; on other platforms it
/// conservatively returns `false`.
#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    // SAFETY: `kill(pid, 0)` does not send a signal; it only checks whether
    // the process exists and the caller has permission to signal it.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_pid_alive(_pid: u32) -> bool {
    false
}

// ---------------------------------------------------------------------------
// WaveProgressEvent
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum WaveProgressEvent {
    RunStart {
        timestamp: String,
        max_parallel: u32,
        total_tasks: usize,
        total_waves: usize,
    },
    WaveStart {
        timestamp: String,
        wave_index: usize,
        task_count: usize,
    },
    TaskStart {
        timestamp: String,
        wave_index: usize,
        task_index: usize,
        title: String,
    },
    TaskEnd {
        timestamp: String,
        wave_index: usize,
        task_index: usize,
        title: String,
        success: bool,
    },
    WaveEnd {
        timestamp: String,
        wave_index: usize,
        passed: usize,
        failed: usize,
    },
    RunInterrupted {
        timestamp: String,
        reason: String,
    },
    RunEnd {
        timestamp: String,
        total_passed: usize,
        total_failed: usize,
        total_skipped: usize,
    },
}

// ---------------------------------------------------------------------------
// Journal helpers
// ---------------------------------------------------------------------------

/// Append a single `WaveProgressEvent` as one JSON line to the journal file.
pub fn append_journal_event(
    journal_path: &Path,
    event: &WaveProgressEvent,
) -> std::io::Result<()> {
    let line = serde_json::to_string(event)?;

    if let Some(parent) = journal_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path)?;

    writeln!(file, "{}", line)?;
    Ok(())
}

/// Read the journal file and return the last `max_events` entries.
///
/// Invalid lines are silently skipped.  If the file does not exist an empty
/// `Vec` is returned.
pub fn read_recent_events(
    journal_path: &Path,
    max_events: usize,
) -> Vec<WaveProgressEvent> {
    let file = match fs::File::open(journal_path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = std::io::BufReader::new(file);
    let all_events: Vec<WaveProgressEvent> = reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<WaveProgressEvent>(&line).ok())
        .collect();

    let skip = all_events.len().saturating_sub(max_events);
    all_events.into_iter().skip(skip).collect()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn now_iso8601() -> String {
    // Use SystemTime to produce a simple ISO-8601 UTC timestamp without
    // pulling in the `chrono` crate.
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();

    // Break epoch seconds into date/time components (UTC).
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Convert days since epoch to a calendar date (Gregorian).
    let (year, month, day) = days_to_ymd(days);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since 1970-01-01 to (year, month, day).
fn days_to_ymd(days_since_epoch: u64) -> (u64, u64, u64) {
    // Algorithm adapted from Howard Hinnant's `civil_from_days`.
    let z = days_since_epoch as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // day of era [0, 146096]
    let yoe =
        (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // year of era [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

fn is_lock_stale(lock_path: &Path, stale_seconds: u64) -> bool {
    let metadata = match fs::metadata(lock_path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let modified = match metadata.modified() {
        Ok(t) => t,
        Err(_) => return false,
    };
    match modified.elapsed() {
        Ok(elapsed) => elapsed.as_secs() > stale_seconds,
        Err(_) => false,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper: create a temporary directory that is cleaned up when dropped.
    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    // -- Lock acquire / release cycle ---------------------------------------

    #[test]
    fn lock_acquire_release_cycle() {
        let dir = tmp_dir();
        let lock_path = dir.path().join("wave.lock");

        let lock = WaveRunLock::acquire(lock_path.clone(), "wave", 4, 300)
            .expect("acquire should succeed");

        assert!(lock_path.exists(), "lock file should exist after acquire");
        assert_eq!(lock.pid, std::process::id());
        assert_eq!(lock.mode, "wave");
        assert_eq!(lock.max_parallel, 4);

        // A second acquire by the same (alive) PID should fail.
        let err = WaveRunLock::acquire(lock_path.clone(), "wave", 4, 300);
        assert!(err.is_err(), "second acquire should fail while lock is held");
        assert!(
            err.unwrap_err().contains("Wave run already in progress"),
            "error should mention the holding PID"
        );

        lock.release();
        assert!(!lock_path.exists(), "lock file should be removed after release");

        // After release a new acquire should succeed.
        let lock2 = WaveRunLock::acquire(lock_path.clone(), "wave", 2, 300)
            .expect("acquire after release should succeed");
        lock2.release();
    }

    // -- Stale lock detection (dead PID) ------------------------------------

    #[test]
    fn stale_lock_with_dead_pid() {
        let dir = tmp_dir();
        let lock_path = dir.path().join("wave.lock");

        // Write a lock file attributed to a PID that (almost certainly) does
        // not exist.  PID 4_000_000 is above the typical kernel limit.
        let fake = LockFileContent {
            pid: 4_000_000,
            started_at: "2020-01-01T00:00:00Z".to_string(),
            mode: "wave".to_string(),
            max_parallel: 2,
        };
        fs::write(&lock_path, serde_json::to_string(&fake).unwrap()).unwrap();

        // Acquire should succeed because the owning PID is dead.
        let lock = WaveRunLock::acquire(lock_path.clone(), "wave", 4, 300)
            .expect("should reclaim lock from dead PID");

        assert_eq!(lock.pid, std::process::id());
        lock.release();
    }

    // -- Journal append produces valid JSONL --------------------------------

    #[test]
    fn journal_append_valid_jsonl() {
        let dir = tmp_dir();
        let journal = dir.path().join("journal.jsonl");

        let events = vec![
            WaveProgressEvent::RunStart {
                timestamp: "2026-02-21T10:00:00Z".to_string(),
                max_parallel: 4,
                total_tasks: 8,
                total_waves: 3,
            },
            WaveProgressEvent::TaskStart {
                timestamp: "2026-02-21T10:00:01Z".to_string(),
                wave_index: 0,
                task_index: 0,
                title: "lint".to_string(),
            },
            WaveProgressEvent::TaskEnd {
                timestamp: "2026-02-21T10:00:05Z".to_string(),
                wave_index: 0,
                task_index: 0,
                title: "lint".to_string(),
                success: true,
            },
        ];

        for ev in &events {
            append_journal_event(&journal, ev).expect("append should succeed");
        }

        // Every line must be valid JSON.
        let raw = fs::read_to_string(&journal).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let parsed: serde_json::Value =
                serde_json::from_str(line).expect("each line should be valid JSON");
            assert!(parsed.is_object());
        }
    }

    // -- read_recent_events returns correct count ---------------------------

    #[test]
    fn read_recent_events_returns_correct_count() {
        let dir = tmp_dir();
        let journal = dir.path().join("journal.jsonl");

        // Write 5 events.
        for i in 0..5 {
            let ev = WaveProgressEvent::WaveStart {
                timestamp: format!("2026-02-21T10:00:{:02}Z", i),
                wave_index: i,
                task_count: 2,
            };
            append_journal_event(&journal, &ev).unwrap();
        }

        let recent = read_recent_events(&journal, 3);
        assert_eq!(recent.len(), 3, "should return exactly max_events entries");

        // The returned events should be the *last* three (wave_index 2, 3, 4).
        for (offset, ev) in recent.iter().enumerate() {
            match ev {
                WaveProgressEvent::WaveStart { wave_index, .. } => {
                    assert_eq!(*wave_index, offset + 2);
                }
                _ => panic!("unexpected event variant"),
            }
        }

        // Asking for more than available returns all.
        let all = read_recent_events(&journal, 100);
        assert_eq!(all.len(), 5);

        // Non-existent file returns empty vec.
        let missing = dir.path().join("nope.jsonl");
        assert!(read_recent_events(&missing, 10).is_empty());
    }

    // -- is_pid_alive for current process -----------------------------------

    #[test]
    fn is_pid_alive_current_process() {
        let pid = std::process::id();
        assert!(
            is_pid_alive(pid),
            "current process PID should be reported as alive"
        );
    }

    // -- is_stale helper ----------------------------------------------------

    #[test]
    fn is_stale_reports_correctly() {
        let dir = tmp_dir();
        let lock_path = dir.path().join("wave.lock");

        let lock = WaveRunLock::acquire(lock_path.clone(), "wave", 4, 300)
            .expect("acquire should succeed");

        // A freshly-created lock with a 300-second threshold should not be stale.
        assert!(!lock.is_stale(300));

        // Sleep briefly so the file's mtime is at least 1 second in the past,
        // then assert staleness with a 0-second threshold.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(lock.is_stale(0));

        lock.release();
    }
}
