//! Integration tests: `--tasks-file` deprecation warning, flag conflict rejection,
//! and validation error integration.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
            "agent_loop_deprecation_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("project dir should be created");
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

/// Run `agent-loop tasks` with the given extra args inside `project_dir`.
fn run_tasks_command(project_dir: &Path, extra_args: &[&str]) -> Output {
    Command::new(agent_loop_bin())
        .arg("tasks")
        .args(extra_args)
        .arg("--single-agent")
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "10")
        .env("MAX_ROUNDS", "1")
        .current_dir(project_dir)
        .output()
        .expect("agent-loop should execute")
}

/// Write a minimal tasks file with no valid headings so parsing fails early
/// without requiring agent binaries.
fn write_headingless_tasks_file(dir: &Path) -> PathBuf {
    let file = dir.join("empty_tasks.md");
    fs::write(&file, "# No task headings here\nJust some text.\n")
        .expect("tasks file should be written");
    file
}

// ---------------------------------------------------------------------------
// Test: --tasks-file emits deprecation warning to stderr
// ---------------------------------------------------------------------------

#[test]
fn tasks_file_flag_emits_deprecation_warning() {
    let tmp = TempDir::new("deprecation_warning");
    let tasks_file = write_headingless_tasks_file(tmp.path());

    let output = run_tasks_command(
        tmp.path(),
        &["--tasks-file", &tasks_file.to_string_lossy()],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);

    // The command will fail because there are no task headings, but the
    // deprecation warning should have been emitted before parse failure.
    assert!(
        stderr.contains("Warning: --tasks-file is deprecated. Use --file instead."),
        "stderr should contain deprecation warning, got: {stderr}"
    );
    // Also assert the expected failure mode: missing task headings parse error.
    assert!(
        stderr.contains("No tasks found"),
        "stderr should contain parse error about missing tasks, got: {stderr}"
    );
    assert!(
        !output.status.success(),
        "command should fail due to missing task headings"
    );
}

// ---------------------------------------------------------------------------
// Test: --file does NOT emit deprecation warning
// ---------------------------------------------------------------------------

#[test]
fn file_flag_does_not_emit_deprecation_warning() {
    let tmp = TempDir::new("no_deprecation");
    let tasks_file = write_headingless_tasks_file(tmp.path());

    let output = run_tasks_command(
        tmp.path(),
        &["--file", &tasks_file.to_string_lossy()],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("--tasks-file is deprecated"),
        "stderr should NOT contain deprecation warning when using --file, got: {stderr}"
    );
    // Should still fail with the same parse error.
    assert!(
        stderr.contains("No tasks found"),
        "stderr should contain parse error about missing tasks, got: {stderr}"
    );
    assert!(
        !output.status.success(),
        "command should fail due to missing task headings"
    );
}

// ---------------------------------------------------------------------------
// Test: --file and --tasks-file together is rejected
// ---------------------------------------------------------------------------

#[test]
fn file_and_tasks_file_together_rejected() {
    let tmp = TempDir::new("both_flags");
    let tasks_file = tmp.path().join("tasks.md");
    fs::write(&tasks_file, "### Task 1: Test\nContent\n")
        .expect("tasks file should be written");

    let file_arg = tasks_file.to_string_lossy().to_string();
    let output = run_tasks_command(
        tmp.path(),
        &["--file", &file_arg, "--tasks-file", &file_arg],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "should fail when both --file and --tasks-file are provided"
    );
    assert!(
        stderr.contains("cannot be used together"),
        "error should mention conflicting flags, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test: --round-step 0 is rejected
// ---------------------------------------------------------------------------

#[test]
fn round_step_zero_rejected() {
    let tmp = TempDir::new("round_step_zero");
    let tasks_file = tmp.path().join("tasks.md");
    fs::write(&tasks_file, "### Task 1: Test\nContent\n")
        .expect("tasks file should be written");

    let output = run_tasks_command(
        tmp.path(),
        &["--file", &tasks_file.to_string_lossy(), "--round-step", "0"],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "should fail with --round-step 0"
    );
    assert!(
        stderr.contains("--round-step"),
        "error should mention --round-step, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test: --continue-on-fail + --fail-fast is rejected
// ---------------------------------------------------------------------------

#[test]
fn continue_on_fail_with_fail_fast_rejected() {
    let tmp = TempDir::new("conflict_flags");
    let tasks_file = tmp.path().join("tasks.md");
    fs::write(&tasks_file, "### Task 1: Test\nContent\n")
        .expect("tasks file should be written");

    let output = run_tasks_command(
        tmp.path(),
        &[
            "--file",
            &tasks_file.to_string_lossy(),
            "--continue-on-fail",
            "--fail-fast",
        ],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "should fail with --continue-on-fail and --fail-fast together"
    );
    assert!(
        stderr.contains("cannot be used together"),
        "error should mention conflicting flags, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test: --max-parallel 0 is rejected
// ---------------------------------------------------------------------------

#[test]
fn max_parallel_zero_rejected() {
    let tmp = TempDir::new("max_parallel_zero");
    let tasks_file = tmp.path().join("tasks.md");
    fs::write(&tasks_file, "### Task 1: Test\nContent\n")
        .expect("tasks file should be written");

    let output = run_tasks_command(
        tmp.path(),
        &[
            "--file",
            &tasks_file.to_string_lossy(),
            "--max-parallel",
            "0",
        ],
    );

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "should fail with --max-parallel 0"
    );
    assert!(
        stderr.contains("--max-parallel"),
        "error should mention --max-parallel, got: {stderr}"
    );
}
