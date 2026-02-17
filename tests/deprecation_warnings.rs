//! Integration tests: deprecation warnings for legacy `run` flags and `run-tasks` subcommand.
//!
//! These tests invoke the compiled binary and verify that stderr contains the
//! expected deprecation messages. The warnings are emitted during dispatch
//! (before any agent invocation), so no mock agents are needed — the process
//! will exit with a non-zero code for other reasons (missing state, etc.),
//! but the deprecation warning will already be on stderr.

use std::process::Command;
use std::path::PathBuf;

fn agent_loop_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-loop"))
}

// ---------------------------------------------------------------------------
// run --planning-only emits deprecation warning
// ---------------------------------------------------------------------------

#[test]
fn run_planning_only_emits_deprecation_warning() {
    let output = Command::new(agent_loop_bin())
        .args(["run", "--planning-only", "some task"])
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("Warning: 'run --planning-only' is deprecated. Use 'plan' instead."),
        "stderr should contain --planning-only deprecation warning, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// run --resume emits deprecation warning
// ---------------------------------------------------------------------------

#[test]
fn run_resume_emits_deprecation_warning() {
    let output = Command::new(agent_loop_bin())
        .args(["run", "--resume"])
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("Warning: 'run --resume' is deprecated. Use 'resume' instead."),
        "stderr should contain --resume deprecation warning, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// run --planning-only --resume emits deprecation warning
// ---------------------------------------------------------------------------

#[test]
fn run_planning_only_resume_emits_deprecation_warning() {
    let output = Command::new(agent_loop_bin())
        .args(["run", "--planning-only", "--resume"])
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);

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
    let tmp = std::env::temp_dir().join(format!(
        "agent_loop_run_tasks_depr_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).expect("temp dir should be created");

    // Write a minimal tasks file so the binary gets past arg parsing
    let tasks_file = tmp.join("tasks.md");
    std::fs::write(&tasks_file, "# No task headings\nJust text.\n")
        .expect("tasks file should be written");

    let output = Command::new(agent_loop_bin())
        .args(["run-tasks", "--file", &tasks_file.to_string_lossy()])
        .current_dir(&tmp)
        .output()
        .expect("agent-loop should execute");

    let _ = std::fs::remove_dir_all(&tmp);

    let stderr = String::from_utf8_lossy(&output.stderr);

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
    let tmp = std::env::temp_dir().join(format!(
        "agent_loop_run_tasks_both_depr_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).expect("temp dir should be created");

    let tasks_file = tmp.join("tasks.md");
    std::fs::write(&tasks_file, "# No task headings\nJust text.\n")
        .expect("tasks file should be written");

    let output = Command::new(agent_loop_bin())
        .args(["run-tasks", "--tasks-file", &tasks_file.to_string_lossy()])
        .current_dir(&tmp)
        .output()
        .expect("agent-loop should execute");

    let _ = std::fs::remove_dir_all(&tmp);

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("Warning: 'run-tasks' is deprecated. Use 'tasks' instead."),
        "stderr should contain run-tasks subcommand deprecation warning, got: {stderr}"
    );
    assert!(
        stderr.contains("Warning: --tasks-file is deprecated. Use --file instead."),
        "stderr should contain --tasks-file flag deprecation warning, got: {stderr}"
    );
}
