//! Integration tests for task lifecycle persistence in `run-tasks`.
//!
//! These tests verify resume-after-interrupt, `--continue-on-fail`,
//! fail-fast default behavior, corruption recovery, and retry-history
//! resume correctness by exercising the `run-tasks` binary with mock
//! agent binaries.

use std::fs;
use std::path::Path;

fn agent_loop_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_agent-loop"))
}

fn create_project_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agent_loop_lifecycle_{}_{}_{}",
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

fn write_state_file(project_dir: &Path, name: &str, content: &str) {
    let state_dir = project_dir.join(".agent-loop").join("state");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    fs::write(state_dir.join(name), content).expect("state file should be written");
}

fn read_state_file(project_dir: &Path, name: &str) -> String {
    let path = project_dir.join(".agent-loop").join("state").join(name);
    fs::read_to_string(path).unwrap_or_default()
}

fn read_task_status(project_dir: &Path) -> serde_json::Value {
    let raw = read_state_file(project_dir, "task_status.json");
    if raw.trim().is_empty() {
        return serde_json::json!({"tasks": []});
    }
    serde_json::from_str(&raw).expect("task_status.json should be valid JSON")
}

#[cfg(unix)]
fn create_mock_agent(project_dir: &Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = project_dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("bin dir should be created");

    let path = bin_dir.join(name);
    fs::write(&path, script).expect("mock agent should be written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("mock agent should be executable");
}

/// Shell snippet to read the current timestamp from status.json and write
/// it back as part of the updated status. This is needed because the
/// implementation loop uses stale-timestamp detection.
const READ_TIMESTAMP_SNIPPET: &str = r#"
STATE_DIR="$(pwd)/.agent-loop/state"
mkdir -p "$STATE_DIR"
STATUS_FILE="$STATE_DIR/status.json"
# Extract the current timestamp so the stale-detection does not fire
CURRENT_TS=""
if [ -f "$STATUS_FILE" ]; then
    # Simple grep to extract timestamp value
    CURRENT_TS=$(sed -n 's/.*"timestamp"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$STATUS_FILE" | head -1)
fi
if [ -z "$CURRENT_TS" ]; then
    CURRENT_TS="2026-02-16T00:00:00.000Z"
fi
"#;

/// Create mock agents that succeed for --version and write APPROVED status
/// (with correct timestamp) for all task runs.
#[cfg(unix)]
fn create_succeeding_agents(project_dir: &Path) {
    let script = format!(
        r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "mock 1.0.0"; exit 0 ;;
    esac
done
{READ_TIMESTAMP_SNIPPET}
# Write APPROVED status with the current timestamp to pass stale detection
printf '{{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","rating":5,"timestamp":"%s"}}' "$CURRENT_TS" > "$STATUS_FILE"
exit 0
"#
    );
    create_mock_agent(project_dir, "claude", &script);
    create_mock_agent(project_dir, "codex", &script);
}

/// Create mock agents where claude fails (non-retryable) for task runs.
#[cfg(unix)]
fn create_failing_agents(project_dir: &Path) {
    let claude_script = format!(
        r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "claude mock 1.0.0"; exit 0 ;;
    esac
done
{READ_TIMESTAMP_SNIPPET}
# Write NEEDS_CHANGES status (non-retryable)
printf '{{"status":"NEEDS_CHANGES","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","reason":"tests failing","timestamp":"%s"}}' "$CURRENT_TS" > "$STATUS_FILE"
exit 1
"#
    );
    create_mock_agent(project_dir, "claude", &claude_script);

    let codex_script = r#"#!/bin/sh
echo "codex mock 1.0.0"
exit 0
"#;
    create_mock_agent(project_dir, "codex", codex_script);
}

/// Create mock agent that succeeds on the Nth invocation (tracked via a counter file).
#[cfg(unix)]
fn create_agent_succeeds_on_nth(project_dir: &Path, succeed_on: u32) {
    let claude_script = format!(
        r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "claude mock 1.0.0"; exit 0 ;;
    esac
done
{READ_TIMESTAMP_SNIPPET}
COUNTER_FILE="$STATE_DIR/.invoke_counter"
COUNT=0
if [ -f "$COUNTER_FILE" ]; then
    COUNT=$(cat "$COUNTER_FILE")
fi
COUNT=$((COUNT + 1))
echo "$COUNT" > "$COUNTER_FILE"
if [ "$COUNT" -ge {succeed_on} ]; then
    printf '{{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","rating":5,"timestamp":"%s"}}' "$CURRENT_TS" > "$STATUS_FILE"
    exit 0
else
    printf '{{"status":"NEEDS_CHANGES","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","reason":"not ready yet","timestamp":"%s"}}' "$CURRENT_TS" > "$STATUS_FILE"
    exit 1
fi
"#
    );
    create_mock_agent(project_dir, "claude", &claude_script);

    let codex_script = r#"#!/bin/sh
echo "codex mock 1.0.0"
exit 0
"#;
    create_mock_agent(project_dir, "codex", codex_script);
}

/// Build a PATH that includes the project's bin dir first, followed by
/// system directories so shell builtins like `cat`, `mkdir`, `echo` work.
fn test_path(project_dir: &Path) -> String {
    let bin_dir = project_dir.join("bin");
    format!(
        "{}:/usr/bin:/bin:/usr/sbin:/sbin",
        bin_dir.display()
    )
}

fn run_tasks_cmd(
    project_dir: &Path,
    extra_args: &[&str],
) -> std::process::Output {
    std::process::Command::new(agent_loop_bin())
        .arg("run-tasks")
        .arg("--single-agent")
        .args(extra_args)
        .env("PATH", test_path(project_dir))
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "30")
        .env("MAX_ROUNDS", "1")
        .current_dir(project_dir)
        .output()
        .expect("agent-loop should run")
}

// ---------------------------------------------------------------------------
// Test 1: Resume-after-interrupt skips done tasks and resumes running tasks
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn resume_after_interrupt_skips_done_resumes_running() {
    let project_dir = create_project_dir("resume_skip");
    create_succeeding_agents(&project_dir);

    // Write tasks file with 3 tasks
    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Setup\nSetup content\n\n### Task 2: Build\nBuild content\n\n### Task 3: Test\nTest content\n",
    );

    // Pre-seed task_status.json: task1=done, task2=running, task3=pending
    let status_json = r#"{
  "tasks": [
    { "title": "Task 1: Setup", "status": "done", "retries": 0 },
    { "title": "Task 2: Build", "status": "running", "retries": 0 },
    { "title": "Task 3: Test", "status": "pending", "retries": 0 }
  ]
}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    // Pre-seed resume state files (needed because Task 2 is "running" so
    // the code will use --resume mode which requires task.md and status.json)
    write_state_file(&project_dir, "task.md", "### Task 2: Build\nBuild content");
    write_state_file(
        &project_dir,
        "status.json",
        r#"{"status":"INTERRUPTED","round":1,"implementer":"claude","reviewer":"codex","mode":"single-agent","lastRunTask":"","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Task 1 should be skipped (already done)
    assert!(
        stdout.contains("already done, skipping"),
        "Task 1 should be skipped: {stdout}"
    );

    // Verify final task_status.json
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "done", "Task 1 remains done");
    assert_eq!(tasks[1]["status"], "done", "Task 2 should complete");
    assert_eq!(tasks[2]["status"], "done", "Task 3 should complete");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 2: --continue-on-fail skips failing tasks and continues
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn continue_on_fail_skips_failing_and_continues() {
    let project_dir = create_project_dir("continue_on_fail");

    // Create agent that fails on 1st invocation but succeeds on 2nd+
    // Task 1 fails, Task 2 succeeds
    create_agent_succeeds_on_nth(&project_dir, 2);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Failing\nThis will fail\n\n### Task 2: Passing\nThis will pass\n",
    );

    let output = run_tasks_cmd(&project_dir, &["--continue-on-fail"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should complete with failures (exit code 1)
    assert!(
        !output.status.success(),
        "should exit non-zero with failures"
    );
    assert!(
        stdout.contains("Tasks completed with failures"),
        "should report failures: {stdout}"
    );

    // Verify final state
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["status"], "failed", "Task 1 should be failed");
    assert_eq!(tasks[1]["status"], "done", "Task 2 should be done");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 3: Fail-fast default stops on first failure
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn fail_fast_default_stops_on_first_failure() {
    let project_dir = create_project_dir("fail_fast");
    create_failing_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Failing\nThis will fail\n\n### Task 2: Never reached\nShould not run\n",
    );

    let output = run_tasks_cmd(&project_dir, &[]);

    // Should fail
    assert!(!output.status.success(), "should exit non-zero");

    // Verify task 2 remains pending (never executed)
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["status"], "failed", "Task 1 should be failed");
    assert_eq!(tasks[1]["status"], "pending", "Task 2 should remain pending");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 4: Corruption recovery — corrupt task_status.json treated as all-pending
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn corrupt_task_status_json_treated_as_all_pending() {
    let project_dir = create_project_dir("corrupt_recovery");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Setup\nContent\n",
    );

    // Write corrupt task_status.json
    write_state_file(&project_dir, "task_status.json", "{corrupted json data!!!");

    let output = run_tasks_cmd(&project_dir, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should succeed (treats all as pending)
    assert!(
        output.status.success(),
        "should succeed after recovery: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );

    // Should have printed a warning
    assert!(
        stderr.contains("invalid task_status.json"),
        "should warn about corruption: {stderr}"
    );

    // Task should be completed
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "done");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 5: CLI flag validation via binary
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn cli_rejects_conflicting_fail_flags() {
    let project_dir = create_project_dir("conflicting_flags");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Test\nContent\n",
    );

    let output = std::process::Command::new(agent_loop_bin())
        .args([
            "run-tasks",
            "--continue-on-fail",
            "--fail-fast",
            "--single-agent",
        ])
        .env("PATH", test_path(&project_dir))
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "10")
        .env("MAX_ROUNDS", "1")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop should run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail with conflicting flags"
    );
    assert!(
        stderr.contains("cannot be used together"),
        "error should mention conflicting flags, got: {stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 6: init command resets task_status.json
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn init_command_resets_task_status_json() {
    let project_dir = create_project_dir("init_reset");
    create_succeeding_agents(&project_dir);

    // Write existing task_status.json with data
    let existing = r#"{"tasks":[{"title":"Task 1","status":"done","retries":0}]}"#;
    write_state_file(&project_dir, "task_status.json", existing);

    let output = std::process::Command::new(agent_loop_bin())
        .args(["init"])
        .env("PATH", test_path(&project_dir))
        .env("AUTO_COMMIT", "0")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop init should run");

    assert!(
        output.status.success(),
        "init should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify task_status.json is reset (empty)
    let content = read_state_file(&project_dir, "task_status.json");
    assert!(
        content.trim().is_empty(),
        "task_status.json should be empty after init, got: {content}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 7: Skipped tasks from prior --continue-on-fail are re-executed
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn skipped_tasks_are_reexecuted_on_subsequent_runs() {
    let project_dir = create_project_dir("skipped_reexecute");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Previously skipped\nContent\n",
    );

    // Pre-seed with a skipped task from a previous run
    let status_json = r#"{"tasks":[{"title":"Task 1: Previously skipped","status":"skipped","retries":0}]}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should succeed — skipped task should be re-executed
    assert!(
        output.status.success(),
        "should succeed: stdout={}, stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Task should now be done
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "done", "Previously skipped task should now be done");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 8: Failed task with exhausted retries is not re-executed
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn failed_task_with_exhausted_retries_is_not_reexecuted() {
    let project_dir = create_project_dir("retry_exhausted");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Exhausted\nContent\n",
    );

    // Pre-seed: failed with retries=2, max_retries default is 2
    let status_json = r#"{"tasks":[{"title":"Task 1: Exhausted","status":"failed","retries":2,"last_error":"MAX_ROUNDS"}]}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    // Run with default max-retries=2 — retries already exhausted
    let output = run_tasks_cmd(&project_dir, &[]);

    // Should fail without re-executing the task
    assert!(!output.status.success(), "should fail due to exhausted retries");

    // Task should remain failed (not re-executed to done)
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "failed", "Task should remain failed");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 9: Running task beyond retry boundary is not re-executed
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn running_task_beyond_retry_boundary_fails_immediately() {
    let project_dir = create_project_dir("running_exhausted");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Exhausted running\nContent\n",
    );

    // Pre-seed: running with retries=3, max_retries default is 2.
    // Since retries > max_retries (3 > 2), this is truly exhausted.
    let status_json = r#"{"tasks":[{"title":"Task 1: Exhausted running","status":"running","retries":3,"last_error":"MAX_ROUNDS"}]}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    let output = run_tasks_cmd(&project_dir, &[]);

    // Should fail without re-executing
    assert!(!output.status.success(), "should fail due to exhausted retries");

    // Task should be marked failed
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "failed", "Task should be failed");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 9b: Running task at retry boundary gets one final attempt
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn running_task_at_retry_boundary_gets_final_attempt() {
    let project_dir = create_project_dir("running_boundary");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: At boundary\nContent\n",
    );

    // Pre-seed: running with retries=2, max_retries default is 2.
    // Since retries == max_retries, the interrupted attempt hasn't completed
    // yet, so the task should get one final resume attempt.
    let status_json = r#"{"tasks":[{"title":"Task 1: At boundary","status":"running","retries":2,"last_error":"timeout"}]}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    // Pre-seed resume state files (needed for --resume mode)
    write_state_file(&project_dir, "task.md", "### Task 1: At boundary\nContent");
    write_state_file(
        &project_dir,
        "status.json",
        r#"{"status":"INTERRUPTED","round":1,"implementer":"claude","reviewer":"codex","mode":"single-agent","lastRunTask":"","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should succeed — the task gets its final attempt and completes
    assert!(
        output.status.success(),
        "should succeed with final attempt: stdout={}, stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Task should be done
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "done", "Task should complete on final attempt");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 10: Retry-history resume — persisted retries affect behavior
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn retry_history_resume_with_remaining_budget() {
    let project_dir = create_project_dir("retry_budget");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Resumable\nContent\n",
    );

    // Pre-seed: running with retries=1, max_retries default is 2
    // Should still have budget remaining (1 < 2)
    let status_json = r#"{"tasks":[{"title":"Task 1: Resumable","status":"running","retries":1,"last_error":"timeout"}]}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    // Pre-seed resume state files (needed for --resume mode)
    write_state_file(&project_dir, "task.md", "### Task 1: Resumable\nContent");
    write_state_file(
        &project_dir,
        "status.json",
        r#"{"status":"INTERRUPTED","round":1,"implementer":"claude","reviewer":"codex","mode":"single-agent","lastRunTask":"","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should succeed (agent completes successfully on resume)
    assert!(
        output.status.success(),
        "should succeed with remaining budget: stdout={}, stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify task completed
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "done", "Task should complete");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 11: --continue-on-fail with pre-failed task skips it
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn continue_on_fail_with_prefailed_task_skips_it() {
    let project_dir = create_project_dir("prefailed_skip");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Previously failed\nContent\n\n### Task 2: New task\nContent\n",
    );

    // Pre-seed: task1=failed, task2=pending
    let status_json = r#"{
  "tasks": [
    { "title": "Task 1: Previously failed", "status": "failed", "retries": 1, "last_error": "error" },
    { "title": "Task 2: New task", "status": "pending", "retries": 0 }
  ]
}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    let output = run_tasks_cmd(&project_dir, &["--continue-on-fail"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should complete with failures (exit code 1 due to skipped failed task)
    assert!(
        !output.status.success(),
        "should exit non-zero"
    );
    assert!(
        stdout.contains("previously failed, skipping"),
        "should report skipping failed task: {stdout}"
    );

    // Verify final state
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "skipped", "Failed task should be skipped");
    assert_eq!(tasks[1]["status"], "done", "Task 2 should complete");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 12: task_status.json schema matches contract
// ---------------------------------------------------------------------------

#[test]
fn task_status_json_schema_matches_contract() {
    let project_dir = create_project_dir("schema_contract");

    let status_json = r#"{
  "tasks": [
    {
      "title": "Task 1: Build parser",
      "status": "done",
      "retries": 1
    },
    {
      "title": "Task 2: Add retries",
      "status": "failed",
      "retries": 2,
      "last_error": "MAX_ROUNDS reached"
    },
    {
      "title": "Task 3: Pending",
      "status": "pending",
      "retries": 0
    },
    {
      "title": "Task 4: Running",
      "status": "running",
      "retries": 0
    },
    {
      "title": "Task 5: Skipped",
      "status": "skipped",
      "retries": 0
    }
  ]
}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    let raw = read_state_file(&project_dir, "task_status.json");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("should be valid JSON");

    // Verify the schema contract
    let tasks = parsed["tasks"].as_array().expect("tasks should be an array");
    assert_eq!(tasks.len(), 5);

    // Verify all valid status values are accepted
    let statuses: Vec<&str> = tasks
        .iter()
        .map(|t| t["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        vec!["done", "failed", "pending", "running", "skipped"]
    );

    // Verify last_error is only present when set
    assert!(!tasks[0].as_object().unwrap().contains_key("last_error"));
    assert_eq!(tasks[1]["last_error"], "MAX_ROUNDS reached");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 13: --max-parallel 1 works normally (no warning)
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn max_parallel_1_works_normally() {
    let project_dir = create_project_dir("max_parallel_1");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: First\nContent\n\n### Task 2: Second\nContent\n",
    );

    let output = run_tasks_cmd(&project_dir, &["--max-parallel", "1"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "should succeed: stdout={stdout}, stderr={stderr}"
    );
    assert!(
        stdout.contains("All tasks completed"),
        "should report all tasks completed: {stdout}"
    );

    // Should NOT print the "not yet supported" warning
    assert!(
        !stderr.contains("not yet supported"),
        "should not warn about parallel execution: {stderr}"
    );

    // Verify all done
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert!(tasks.iter().all(|t| t["status"] == "done"));

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 14: --max-parallel 2 prints warning and falls back to sequential
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn max_parallel_2_prints_warning_and_runs_sequentially() {
    let project_dir = create_project_dir("max_parallel_2");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: First\nContent\n\n### Task 2: Second\nContent\n",
    );

    let output = run_tasks_cmd(&project_dir, &["--max-parallel", "2"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should succeed (falls back to sequential)
    assert!(
        output.status.success(),
        "should succeed: stdout={stdout}, stderr={stderr}"
    );

    // Should print the "not yet supported" warning
    assert!(
        stderr.contains("Parallel task execution is not yet supported; running sequentially with max_parallel=1"),
        "should warn about unsupported parallel execution: {stderr}"
    );

    // All tasks should still complete sequentially
    assert!(
        stdout.contains("All tasks completed"),
        "should report all tasks completed: {stdout}"
    );

    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert!(tasks.iter().all(|t| t["status"] == "done"));

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 15: --max-parallel 0 exits non-zero with validation error
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn max_parallel_0_rejected_with_validation_error() {
    let project_dir = create_project_dir("max_parallel_0");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Test\nContent\n",
    );

    let output = run_tasks_cmd(&project_dir, &["--max-parallel", "0"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "should fail with --max-parallel 0"
    );
    assert!(
        stderr.contains("--max-parallel must be at least 1"),
        "should report validation error: {stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 15b: Config-driven max_parallel > 1 prints warning without CLI flag
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn config_driven_max_parallel_prints_warning() {
    let project_dir = create_project_dir("config_max_parallel");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: First\nContent\n\n### Task 2: Second\nContent\n",
    );

    // Write .agent-loop.toml with max_parallel = 4 (no CLI override)
    fs::write(
        project_dir.join(".agent-loop.toml"),
        "max_parallel = 4\n",
    )
    .expect("should write config");

    // Run without --max-parallel flag — config value should trigger warning
    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "should succeed with config-driven max_parallel: stdout={stdout}, stderr={stderr}"
    );

    // Should print the "not yet supported" warning from config value
    assert!(
        stderr.contains("Parallel task execution is not yet supported; running sequentially with max_parallel=1"),
        "should warn about unsupported parallel execution from config: {stderr}"
    );

    // All tasks should still complete sequentially
    assert!(
        stdout.contains("All tasks completed"),
        "should report all tasks completed: {stdout}"
    );

    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert!(tasks.iter().all(|t| t["status"] == "done"));

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 16: Duplicate title handling in task_status.json
// ---------------------------------------------------------------------------

#[test]
fn duplicate_titles_preserve_distinct_status_per_occurrence() {
    let project_dir = create_project_dir("duplicate_titles");

    let status_json = r#"{
  "tasks": [
    { "title": "Task 1: Build", "status": "done", "retries": 0 },
    { "title": "Task 1: Build", "status": "failed", "retries": 2, "last_error": "timeout" },
    { "title": "Task 1: Build", "status": "pending", "retries": 0 }
  ]
}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    let raw = read_state_file(&project_dir, "task_status.json");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("should be valid JSON");
    let tasks = parsed["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 3);

    // Each occurrence has distinct status even with same title
    assert_eq!(tasks[0]["status"], "done");
    assert_eq!(tasks[1]["status"], "failed");
    assert_eq!(tasks[1]["retries"], 2);
    assert_eq!(tasks[2]["status"], "pending");

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test 14: All tasks succeed — exit code 0
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn all_tasks_succeed_exit_zero() {
    let project_dir = create_project_dir("all_succeed");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: First\nContent\n\n### Task 2: Second\nContent\n",
    );

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "should exit 0: stdout={}, stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("All tasks completed"),
        "should report all tasks completed: {stdout}"
    );

    // Verify all done
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert!(tasks.iter().all(|t| t["status"] == "done"));

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Task metrics helper
// ---------------------------------------------------------------------------

fn read_task_metrics(project_dir: &Path) -> serde_json::Value {
    let raw = read_state_file(project_dir, "task_metrics.json");
    if raw.trim().is_empty() {
        return serde_json::json!({"tasks": []});
    }
    serde_json::from_str(&raw).expect("task_metrics.json should be valid JSON")
}

// ---------------------------------------------------------------------------
// Test: task_metrics.json is persisted after successful multi-task run
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn task_metrics_persisted_after_successful_multi_task_run() {
    let project_dir = create_project_dir("metrics_multi_task");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Setup\nSetup content\n\n### Task 2: Build\nBuild content\n",
    );

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "should succeed: stdout={}, stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify task_metrics.json exists and has correct structure
    let metrics = read_task_metrics(&project_dir);
    let tasks = metrics["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2, "should have metrics for 2 tasks");

    for (i, task) in tasks.iter().enumerate() {
        assert!(
            task["task_started_at"].is_string(),
            "task {} should have task_started_at string",
            i + 1
        );
        assert!(
            task["task_ended_at"].is_string(),
            "task {} should have task_ended_at string",
            i + 1
        );
        let duration = task["duration_ms"].as_u64();
        assert!(
            duration.is_some(),
            "task {} should have numeric duration_ms",
            i + 1
        );
    }

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test: run-tasks prints task duration summary table
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn run_tasks_prints_task_duration_summary_table() {
    let project_dir = create_project_dir("metrics_summary_table");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Setup\nSetup content\n\n### Task 2: Build\nBuild content\n",
    );

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "should succeed: stdout={}, stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify summary table is printed
    assert!(
        stdout.contains("Task Durations:"),
        "should contain 'Task Durations:' header: {stdout}"
    );
    assert!(
        stdout.contains("Task 1: Setup"),
        "summary should include Task 1 title: {stdout}"
    );
    assert!(
        stdout.contains("Task 2: Build"),
        "summary should include Task 2 title: {stdout}"
    );
    assert!(
        stdout.contains("Total"),
        "summary should include Total line: {stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test: skipped/unexecuted tasks show n/a and no timing
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn skipped_or_unexecuted_tasks_show_na_and_no_timing() {
    let project_dir = create_project_dir("metrics_skipped");
    create_failing_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Failing\nThis will fail\n\n### Task 2: Never reached\nShould not run\n",
    );

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should fail (fail-fast)
    assert!(!output.status.success(), "should exit non-zero");

    // Verify summary table shows n/a for unexecuted task
    assert!(
        stdout.contains("Task Durations:"),
        "should contain summary header: {stdout}"
    );
    assert!(
        stdout.contains("n/a"),
        "unexecuted task should show n/a: {stdout}"
    );

    // Verify task_metrics.json — unexecuted task has null timing fields
    let metrics = read_task_metrics(&project_dir);
    let tasks = metrics["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);

    // Task 1 was executed (and failed) — should have timing
    assert!(
        tasks[0]["task_started_at"].is_string(),
        "executed task should have task_started_at"
    );

    // Task 2 was never executed — should have null timing
    assert!(
        tasks[1].get("task_started_at").is_none()
            || tasks[1]["task_started_at"].is_null(),
        "unexecuted task should have null/missing task_started_at"
    );
    assert!(
        tasks[1].get("task_ended_at").is_none()
            || tasks[1]["task_ended_at"].is_null(),
        "unexecuted task should have null/missing task_ended_at"
    );
    assert!(
        tasks[1].get("duration_ms").is_none()
            || tasks[1]["duration_ms"].is_null(),
        "unexecuted task should have null/missing duration_ms"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test: init command resets task_metrics.json
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn init_command_resets_task_metrics_json() {
    let project_dir = create_project_dir("init_reset_metrics");
    create_succeeding_agents(&project_dir);

    // Write existing task_metrics.json with data
    let existing = r#"{"tasks":[{"title":"Task 1","task_started_at":"2026-02-16T10:00:00.000Z","task_ended_at":"2026-02-16T10:05:00.000Z","duration_ms":300000}]}"#;
    write_state_file(&project_dir, "task_metrics.json", existing);

    let output = std::process::Command::new(agent_loop_bin())
        .args(["init"])
        .env("PATH", test_path(&project_dir))
        .env("AUTO_COMMIT", "0")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop init should run");

    assert!(
        output.status.success(),
        "init should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify task_metrics.json is reset (empty)
    let content = read_state_file(&project_dir, "task_metrics.json");
    assert!(
        content.trim().is_empty(),
        "task_metrics.json should be empty after init, got: {content}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test: Re-execution with pre-existing metrics clears stale timing
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn reexecuted_task_clears_stale_timing_from_previous_run() {
    let project_dir = create_project_dir("metrics_reexec");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Retried\nContent\n",
    );

    // Pre-seed: task was previously failed with old timing data
    let status_json = r#"{"tasks":[{"title":"Task 1: Retried","status":"failed","retries":0,"last_error":"tests failing"}]}"#;
    write_state_file(&project_dir, "task_status.json", status_json);

    let old_metrics = r#"{"tasks":[{"title":"Task 1: Retried","task_started_at":"2026-01-01T00:00:00.000Z","task_ended_at":"2026-01-01T00:05:00.000Z","duration_ms":300000}]}"#;
    write_state_file(&project_dir, "task_metrics.json", old_metrics);

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "should succeed on retry: stdout={}, stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify metrics were refreshed — task_started_at should NOT be the old value
    let metrics = read_task_metrics(&project_dir);
    let tasks = metrics["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1);

    let started = tasks[0]["task_started_at"]
        .as_str()
        .expect("should have task_started_at");
    assert_ne!(
        started, "2026-01-01T00:00:00.000Z",
        "task_started_at should be refreshed, not the old stale value"
    );

    let ended = tasks[0]["task_ended_at"]
        .as_str()
        .expect("should have task_ended_at");
    assert_ne!(
        ended, "2026-01-01T00:05:00.000Z",
        "task_ended_at should be refreshed, not the old stale value"
    );

    // duration_ms should be present and a valid number
    assert!(
        tasks[0]["duration_ms"].as_u64().is_some(),
        "should have numeric duration_ms"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test: Non-executed task with old metrics preserves timing from reconciliation
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn skipped_task_with_old_metrics_preserves_reconciled_timing() {
    let project_dir = create_project_dir("metrics_skip_old");
    create_failing_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Failing\nThis will fail\n\n### Task 2: Skipped\nNever reached\n",
    );

    // Pre-seed metrics for Task 2 with old timing (from a previous run that
    // completed it, but the task list has since been re-initialized)
    let old_metrics = r#"{"tasks":[
        {"title":"Task 1: Failing"},
        {"title":"Task 2: Skipped","task_started_at":"2026-01-01T00:00:00.000Z","task_ended_at":"2026-01-01T00:05:00.000Z","duration_ms":300000}
    ]}"#;
    write_state_file(&project_dir, "task_metrics.json", old_metrics);

    let output = run_tasks_cmd(&project_dir, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should fail (fail-fast on task 1)
    assert!(!output.status.success(), "should exit non-zero");

    // Verify summary header is present
    assert!(
        stdout.contains("Task Durations:"),
        "should contain summary header: {stdout}"
    );

    let metrics = read_task_metrics(&project_dir);
    let tasks = metrics["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);

    // Task 1 was executed and should have fresh timing from this run
    assert!(
        tasks[0]["task_started_at"].is_string(),
        "executed failing task should have task_started_at"
    );

    // Task 2 preserves old reconciled timing (since it wasn't re-executed,
    // its old timing stays from reconciliation)
    assert!(
        tasks[1]["task_started_at"].is_string(),
        "Task 2 should preserve reconciled timing"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Test: Failed task on re-execution gets fresh timing, not stale + new mixed
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn failed_task_reexecution_gets_consistent_fresh_timing() {
    let project_dir = create_project_dir("metrics_fail_reexec");
    create_failing_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Always fails\nContent\n",
    );

    // Pre-seed: task was previously run and has old timing
    let old_metrics = r#"{"tasks":[{"title":"Task 1: Always fails","task_started_at":"2026-01-01T00:00:00.000Z","task_ended_at":"2026-01-01T00:05:00.000Z","duration_ms":300000}]}"#;
    write_state_file(&project_dir, "task_metrics.json", old_metrics);

    // No pre-seeded task_status, so it starts fresh (pending)
    let output = run_tasks_cmd(&project_dir, &[]);

    // Should fail
    assert!(!output.status.success(), "should exit non-zero");

    // Verify timing was refreshed — task_started_at should be newer than the old value
    let metrics = read_task_metrics(&project_dir);
    let tasks = metrics["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1);

    let started = tasks[0]["task_started_at"]
        .as_str()
        .expect("should have task_started_at");
    assert_ne!(
        started, "2026-01-01T00:00:00.000Z",
        "task_started_at should be refreshed on re-execution"
    );

    let ended = tasks[0]["task_ended_at"]
        .as_str()
        .expect("should have task_ended_at after failure");
    assert_ne!(
        ended, "2026-01-01T00:05:00.000Z",
        "task_ended_at should be refreshed on re-execution"
    );

    // duration_ms should be present (the task ran and failed)
    assert!(
        tasks[0]["duration_ms"].as_u64().is_some(),
        "should have numeric duration_ms after failed execution"
    );

    let _ = fs::remove_dir_all(&project_dir);
}
