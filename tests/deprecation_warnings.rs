//! Integration tests: deprecation warnings for legacy `run` flags and `run-tasks` subcommand.
//!
//! These tests invoke the compiled binary and verify that stderr contains the
//! expected deprecation messages. Each test runs inside a fresh temp directory
//! so no existing `.agent-loop/state` can interfere, and uses arguments that
//! cause the process to fail immediately after dispatch (non-zero exit), which
//! guarantees no real agent workflows are invoked.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn agent_loop_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-loop"))
}

/// RAII temp directory that is removed on drop (panic-safe cleanup).
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agent_loop_depr_{}_{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// run --planning-only emits deprecation warning
// ---------------------------------------------------------------------------
// Omit the TASK argument so `resolve_task_for_plan` fails with "Task is
// required" immediately after dispatch. The warning is printed during
// dispatch, before the handler runs.

#[test]
fn run_planning_only_emits_deprecation_warning() {
    let tmp = TempDir::new("planning_only");

    let output = Command::new(agent_loop_bin())
        .args(["run", "--planning-only"])
        .current_dir(tmp.path())
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "command should fail (no task provided)"
    );
    assert!(
        stderr.contains("Warning: 'run --planning-only' is deprecated. Use 'plan' instead."),
        "stderr should contain --planning-only deprecation warning, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// run --resume emits deprecation warning
// ---------------------------------------------------------------------------
// Run in an empty temp dir so `ensure_resume_state_dir_exists` fails
// immediately with "Cannot resume: .agent-loop/state does not exist."

#[test]
fn run_resume_emits_deprecation_warning() {
    let tmp = TempDir::new("resume");

    let output = Command::new(agent_loop_bin())
        .args(["run", "--resume"])
        .current_dir(tmp.path())
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "command should fail (no state dir in temp dir)"
    );
    assert!(
        stderr.contains("Warning: 'run --resume' is deprecated. Use 'resume' instead."),
        "stderr should contain --resume deprecation warning, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// run --planning-only --resume emits deprecation warning
// ---------------------------------------------------------------------------
// Same isolation: empty temp dir → resume fails immediately.

#[test]
fn run_planning_only_resume_emits_deprecation_warning() {
    let tmp = TempDir::new("planning_only_resume");

    let output = Command::new(agent_loop_bin())
        .args(["run", "--planning-only", "--resume"])
        .current_dir(tmp.path())
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "command should fail (no state dir in temp dir)"
    );
    assert!(
        stderr.contains("Warning: 'run --planning-only --resume' is deprecated. Use 'resume' instead."),
        "stderr should contain combined deprecation warning, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// run-tasks emits deprecation warning
// ---------------------------------------------------------------------------

#[test]
fn run_tasks_subcommand_emits_deprecation_warning() {
    let tmp = TempDir::new("run_tasks");
    let tasks_file = tmp.path().join("tasks.md");
    fs::write(&tasks_file, "# No task headings\nJust text.\n")
        .expect("tasks file should be written");

    let output = Command::new(agent_loop_bin())
        .args(["run-tasks", "--file", &tasks_file.to_string_lossy()])
        .current_dir(tmp.path())
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "command should fail (no valid task headings)"
    );
    assert!(
        stderr.contains("Warning: 'run-tasks' is deprecated. Use 'tasks' instead."),
        "stderr should contain run-tasks deprecation warning, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// run-tasks --tasks-file emits both deprecation warnings
// ---------------------------------------------------------------------------

#[test]
fn run_tasks_with_tasks_file_flag_emits_both_deprecation_warnings() {
    let tmp = TempDir::new("run_tasks_both");
    let tasks_file = tmp.path().join("tasks.md");
    fs::write(&tasks_file, "# No task headings\nJust text.\n")
        .expect("tasks file should be written");

    let output = Command::new(agent_loop_bin())
        .args(["run-tasks", "--tasks-file", &tasks_file.to_string_lossy()])
        .current_dir(tmp.path())
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "command should fail (no valid task headings)"
    );
    assert!(
        stderr.contains("Warning: 'run-tasks' is deprecated. Use 'tasks' instead."),
        "stderr should contain run-tasks subcommand deprecation warning, got: {stderr}"
    );
    assert!(
        stderr.contains("Warning: --tasks-file is deprecated. Use --file instead."),
        "stderr should contain --tasks-file flag deprecation warning, got: {stderr}"
    );
}
