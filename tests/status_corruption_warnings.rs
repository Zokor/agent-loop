//! Integration tests: `agent-loop status` corruption warnings and stale-run hints.

use std::fs;
use std::path::Path;
use std::process::Command;

fn agent_loop_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_agent-loop"))
}

fn setup_project(name: &str) -> std::path::PathBuf {
    let project_dir = std::env::temp_dir().join(format!(
        "agent_loop_status_corruption_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let state_dir = project_dir.join(".agent-loop").join("state");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    project_dir
}

fn write_status_json(project_dir: &Path, content: &str) {
    let status_path = project_dir
        .join(".agent-loop")
        .join("state")
        .join("status.json");
    fs::write(status_path, content).expect("status.json should be written");
}

fn run_status(project_dir: &Path) -> (String, String, i32) {
    let output = Command::new(agent_loop_bin())
        .arg("status")
        .current_dir(project_dir)
        .output()
        .expect("agent-loop status should run");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

#[test]
fn status_with_corrupt_json_shows_warnings_and_recovery_hint() {
    let project_dir = setup_project("corrupt_json");
    write_status_json(&project_dir, "{not valid json!!!");

    let (stdout, stderr, code) = run_status(&project_dir);

    assert_eq!(code, 0, "status command should exit 0 even with corrupt data");

    // stderr should contain the warning from read_status_with_warnings
    assert!(
        stderr.contains("\u{26a0} status.json:"),
        "stderr should contain warning prefix, got:\n{stderr}"
    );
    assert!(
        stderr.contains("invalid JSON"),
        "stderr should mention invalid JSON, got:\n{stderr}"
    );

    // stdout should contain warning section and recovery hint
    assert!(
        stdout.contains("Warnings:"),
        "stdout should contain Warnings section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("agent-loop init"),
        "stdout should contain init recovery hint, got:\n{stdout}"
    );
    assert!(
        stdout.contains("corrupted"),
        "stdout should mention corruption, got:\n{stdout}"
    );

    // Also gets ERROR status hint
    assert!(
        stdout.contains("--resume"),
        "stdout should contain --resume hint for ERROR status, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_with_missing_fields_shows_per_field_warnings() {
    let project_dir = setup_project("missing_fields");
    // Valid JSON but missing required fields
    write_status_json(&project_dir, r#"{"lastRunTask": "some task"}"#);

    let (stdout, stderr, _code) = run_status(&project_dir);

    // stderr should have warnings for missing required fields
    assert!(
        stderr.contains("status.json:"),
        "stderr should contain status.json warnings, got:\n{stderr}"
    );

    // stdout should list warnings
    assert!(
        stdout.contains("Warnings:"),
        "stdout should contain Warnings section, got:\n{stdout}"
    );

    // Should contain recovery hint
    assert!(
        stdout.contains("agent-loop init"),
        "stdout should contain init hint, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_with_max_rounds_shows_resume_hint() {
    let project_dir = setup_project("max_rounds");
    write_status_json(
        &project_dir,
        r#"{
            "status": "MAX_ROUNDS",
            "round": 5,
            "implementer": "claude",
            "reviewer": "codex",
            "mode": "dual-agent",
            "lastRunTask": "build feature",
            "timestamp": "2026-02-14T00:00:00.000Z"
        }"#,
    );

    let (stdout, _stderr, code) = run_status(&project_dir);

    assert_eq!(code, 0);
    assert!(
        stdout.contains("MAX_ROUNDS"),
        "stdout should show MAX_ROUNDS status, got:\n{stdout}"
    );
    assert!(
        stdout.contains("--resume"),
        "stdout should contain --resume hint, got:\n{stdout}"
    );
    assert!(
        stdout.contains("agent-loop init"),
        "stdout should contain init hint, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_with_interrupted_shows_resume_hint() {
    let project_dir = setup_project("interrupted");
    write_status_json(
        &project_dir,
        r#"{
            "status": "INTERRUPTED",
            "round": 2,
            "implementer": "claude",
            "reviewer": "codex",
            "mode": "dual-agent",
            "lastRunTask": "fix bug",
            "reason": "Interrupted by signal",
            "timestamp": "2026-02-14T00:00:00.000Z"
        }"#,
    );

    let (stdout, _stderr, code) = run_status(&project_dir);

    assert_eq!(code, 0);
    assert!(
        stdout.contains("INTERRUPTED"),
        "stdout should show INTERRUPTED, got:\n{stdout}"
    );
    assert!(
        stdout.contains("--resume"),
        "stdout should contain --resume hint, got:\n{stdout}"
    );
    assert!(
        stdout.contains("agent-loop init"),
        "stdout should contain init hint, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_with_error_shows_resume_hint() {
    let project_dir = setup_project("error_status");
    write_status_json(
        &project_dir,
        r#"{
            "status": "ERROR",
            "round": 1,
            "implementer": "claude",
            "reviewer": "codex",
            "mode": "dual-agent",
            "lastRunTask": "task",
            "reason": "agent timed out",
            "timestamp": "2026-02-14T00:00:00.000Z"
        }"#,
    );

    let (stdout, _stderr, code) = run_status(&project_dir);

    assert_eq!(code, 0);
    assert!(
        stdout.contains("ERROR"),
        "stdout should show ERROR, got:\n{stdout}"
    );
    assert!(
        stdout.contains("--resume"),
        "stdout should contain --resume hint, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_with_valid_active_status_shows_no_warnings_or_hints() {
    let project_dir = setup_project("valid_active");
    write_status_json(
        &project_dir,
        r#"{
            "status": "IMPLEMENTING",
            "round": 2,
            "implementer": "claude",
            "reviewer": "codex",
            "mode": "dual-agent",
            "lastRunTask": "build feature",
            "timestamp": "2026-02-14T00:00:00.000Z"
        }"#,
    );

    let (stdout, stderr, code) = run_status(&project_dir);

    assert_eq!(code, 0);
    assert!(
        !stdout.contains("Warnings:"),
        "valid status should not show warnings, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("--resume"),
        "active status should not show resume hint, got:\n{stdout}"
    );
    assert!(
        !stderr.contains("\u{26a0}"),
        "valid status should produce no stderr warnings, got:\n{stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_with_stale_timestamp_reason_on_non_terminal_status_shows_resume_hint() {
    let project_dir = setup_project("stale_reason_non_terminal");
    write_status_json(
        &project_dir,
        r#"{
            "status": "NEEDS_CHANGES",
            "round": 3,
            "implementer": "claude",
            "reviewer": "codex",
            "mode": "dual-agent",
            "lastRunTask": "implement auth",
            "reason": "stale timestamp detected during recovery",
            "timestamp": "2026-02-14T00:00:00.000Z"
        }"#,
    );

    let (stdout, _stderr, code) = run_status(&project_dir);

    assert_eq!(code, 0);
    assert!(
        stdout.contains("NEEDS_CHANGES"),
        "stdout should show NEEDS_CHANGES status, got:\n{stdout}"
    );
    assert!(
        stdout.contains("--resume"),
        "stdout should contain --resume hint for stale reason, got:\n{stdout}"
    );
    assert!(
        stdout.contains("agent-loop init"),
        "stdout should contain init hint for stale reason, got:\n{stdout}"
    );
    assert!(
        stdout.contains("stale"),
        "stdout should mention stale in hint, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_with_timestamp_in_reason_on_non_terminal_status_shows_resume_hint() {
    let project_dir = setup_project("timestamp_reason_non_terminal");
    write_status_json(
        &project_dir,
        r#"{
            "status": "DISPUTED",
            "round": 2,
            "implementer": "claude",
            "reviewer": "codex",
            "mode": "dual-agent",
            "lastRunTask": "fix bug",
            "reason": "timestamp mismatch — external modification detected",
            "timestamp": "2026-02-14T00:00:00.000Z"
        }"#,
    );

    let (stdout, _stderr, code) = run_status(&project_dir);

    assert_eq!(code, 0);
    assert!(
        stdout.contains("DISPUTED"),
        "stdout should show DISPUTED status, got:\n{stdout}"
    );
    assert!(
        stdout.contains("--resume"),
        "stdout should contain --resume hint for timestamp reason, got:\n{stdout}"
    );
    assert!(
        stdout.contains("agent-loop init"),
        "stdout should contain init hint for timestamp reason, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_with_non_stale_reason_on_non_terminal_status_shows_no_hints() {
    let project_dir = setup_project("normal_reason_non_terminal");
    write_status_json(
        &project_dir,
        r#"{
            "status": "NEEDS_CHANGES",
            "round": 2,
            "implementer": "claude",
            "reviewer": "codex",
            "mode": "dual-agent",
            "lastRunTask": "fix bug",
            "reason": "missing test coverage",
            "timestamp": "2026-02-14T00:00:00.000Z"
        }"#,
    );

    let (stdout, _stderr, code) = run_status(&project_dir);

    assert_eq!(code, 0);
    assert!(
        !stdout.contains("--resume"),
        "non-stale non-terminal status should not show resume hint, got:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}
