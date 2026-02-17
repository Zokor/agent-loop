//! Integration test: `--tasks-file` deprecation warning is emitted to stderr.

use std::fs;

fn agent_loop_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_agent-loop"))
}

fn create_project_dir(name: &str) -> std::path::PathBuf {
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
    dir
}

// ---------------------------------------------------------------------------
// Test: --tasks-file emits deprecation warning to stderr
// ---------------------------------------------------------------------------

#[test]
fn tasks_file_flag_emits_deprecation_warning() {
    let project_dir = create_project_dir("deprecation_warning");

    // Create a tasks file with no valid task headings so parse fails early
    // (no agent binaries needed).
    let tasks_file = project_dir.join("empty_tasks.md");
    fs::write(&tasks_file, "# No task headings here\nJust some text.\n")
        .expect("tasks file should be written");

    let output = std::process::Command::new(agent_loop_bin())
        .args([
            "tasks",
            "--tasks-file",
            tasks_file.to_str().unwrap(),
            "--single-agent",
        ])
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "10")
        .env("MAX_ROUNDS", "1")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop should run");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // The command will fail because there are no task headings, but the
    // deprecation warning should have been emitted before parse failure.
    assert!(
        stderr.contains("Warning: --tasks-file is deprecated. Use --file instead."),
        "stderr should contain deprecation warning, got: {stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test: --file does NOT emit deprecation warning
// ---------------------------------------------------------------------------

#[test]
fn file_flag_does_not_emit_deprecation_warning() {
    let project_dir = create_project_dir("no_deprecation");

    let tasks_file = project_dir.join("empty_tasks.md");
    fs::write(&tasks_file, "# No task headings here\nJust some text.\n")
        .expect("tasks file should be written");

    let output = std::process::Command::new(agent_loop_bin())
        .args([
            "tasks",
            "--file",
            tasks_file.to_str().unwrap(),
            "--single-agent",
        ])
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "10")
        .env("MAX_ROUNDS", "1")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop should run");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("--tasks-file is deprecated"),
        "stderr should NOT contain deprecation warning when using --file, got: {stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test: --file and --tasks-file together is rejected
// ---------------------------------------------------------------------------

#[test]
fn file_and_tasks_file_together_rejected() {
    let project_dir = create_project_dir("both_flags");

    let tasks_file = project_dir.join("tasks.md");
    fs::write(&tasks_file, "### Task 1: Test\nContent\n")
        .expect("tasks file should be written");

    let output = std::process::Command::new(agent_loop_bin())
        .args([
            "tasks",
            "--file",
            tasks_file.to_str().unwrap(),
            "--tasks-file",
            tasks_file.to_str().unwrap(),
            "--single-agent",
        ])
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "10")
        .env("MAX_ROUNDS", "1")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop should run");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "should fail with both flags"
    );
    assert!(
        stderr.contains("cannot be used together"),
        "error should mention conflicting flags, got: {stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}
