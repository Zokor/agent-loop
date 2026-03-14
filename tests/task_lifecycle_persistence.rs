//! Integration tests for task lifecycle persistence in `implement`.

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

fn write_session_state_file(project_dir: &Path, session: &str, name: &str, content: &str) {
    let state_dir = project_dir
        .join(".agent-loop")
        .join("state")
        .join(session);
    fs::create_dir_all(&state_dir).expect("session state dir should be created");
    fs::write(state_dir.join(name), content).expect("session state file should be written");
}

fn read_state_file(project_dir: &Path, name: &str) -> String {
    let path = project_dir.join(".agent-loop").join("state").join(name);
    fs::read_to_string(path).unwrap_or_default()
}

fn read_session_state_file(project_dir: &Path, session: &str, name: &str) -> String {
    let path = project_dir
        .join(".agent-loop")
        .join("state")
        .join(session)
        .join(name);
    fs::read_to_string(path).unwrap_or_default()
}

fn read_task_status(project_dir: &Path) -> serde_json::Value {
    let raw = read_state_file(project_dir, "task_status.json");
    if raw.trim().is_empty() {
        return serde_json::json!({"tasks": []});
    }
    serde_json::from_str(&raw).expect("task_status.json should be valid JSON")
}

fn read_task_metrics(project_dir: &Path) -> serde_json::Value {
    let raw = read_state_file(project_dir, "task_metrics.json");
    if raw.trim().is_empty() {
        return serde_json::json!({"tasks": []});
    }
    serde_json::from_str(&raw).expect("task_metrics.json should be valid JSON")
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

const READ_TIMESTAMP_SNIPPET: &str = r#"
STATE_DIR="$(pwd)/.agent-loop/state"
mkdir -p "$STATE_DIR"
STATUS_FILE="$STATE_DIR/status.json"
FINDINGS_FILE="$STATE_DIR/findings.json"
ALL_ARGS="$*"
CURRENT_TS=$(printf '%s' "$ALL_ARGS" | sed -n 's/.*"timestamp"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | tail -1)
if [ -z "$CURRENT_TS" ]; then
    CURRENT_TS="2026-02-16T00:00:00.000Z"
fi
"#;

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
printf '{{"round":1,"findings":[]}}' > "$FINDINGS_FILE"
printf '{{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","rating":5,"timestamp":"%s"}}' "$CURRENT_TS" > "$STATUS_FILE"
exit 0
"#
    );
    create_mock_agent(project_dir, "claude", &script);
    create_mock_agent(project_dir, "codex", &script);
}

#[cfg(unix)]
fn create_prompt_path_succeeding_agents(project_dir: &Path) {
    let script = r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "mock 1.0.0"; exit 0 ;;
    esac
done
ALL_ARGS="$*"
TASK_FILE_PATH=$(printf '%s' "$ALL_ARGS" | grep -o '/[^"[:space:]]*\.agent-loop/state[^"[:space:]]*/task\.md' | head -n1)
if [ -n "$TASK_FILE_PATH" ]; then
    STATE_DIR=$(dirname "$TASK_FILE_PATH")
else
    STATE_DIR="$(pwd)/.agent-loop/state"
fi
mkdir -p "$STATE_DIR"
STATUS_FILE="$STATE_DIR/status.json"
FINDINGS_FILE="$STATE_DIR/findings.json"
CURRENT_TS=$(printf '%s' "$ALL_ARGS" | sed -n 's/.*"timestamp"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | tail -1)
if [ -z "$CURRENT_TS" ]; then
    CURRENT_TS="2026-02-16T00:00:00.000Z"
fi
printf '{"round":1,"findings":[]}' > "$FINDINGS_FILE"
printf '{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","rating":5,"timestamp":"%s"}' "$CURRENT_TS" > "$STATUS_FILE"
exit 0
"#;
    create_mock_agent(project_dir, "claude", script);
    create_mock_agent(project_dir, "codex", script);
}

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
    printf '{{"round":1,"findings":[]}}' > "$FINDINGS_FILE"
    printf '{{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","rating":5,"timestamp":"%s"}}' "$CURRENT_TS" > "$STATUS_FILE"
    exit 0
fi
printf '{{"round":1,"findings":[{{"id":"F-001","severity":"MEDIUM","summary":"not ready","file_refs":[]}}]}}' > "$FINDINGS_FILE"
printf '{{"status":"NEEDS_CHANGES","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","reason":"not ready","timestamp":"%s"}}' "$CURRENT_TS" > "$STATUS_FILE"
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

#[cfg(unix)]
fn create_resume_sensitive_agents(project_dir: &Path) {
    let script = format!(
        r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "mock 1.0.0"; exit 0 ;;
    esac
done
{READ_TIMESTAMP_SNIPPET}
TASK_FILE="$STATE_DIR/task.md"
if [ -f "$TASK_FILE" ] && grep -q "Task 2: No resume leakage" "$TASK_FILE"; then
    if printf '%s' "$ALL_ARGS" | grep -q -- "--resume"; then
        : > "$STATE_DIR/resume_leak_detected"
    fi
fi
printf '{{"round":1,"findings":[]}}' > "$FINDINGS_FILE"
printf '{{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","rating":5,"timestamp":"%s"}}' "$CURRENT_TS" > "$STATUS_FILE"
exit 0
"#
    );
    create_mock_agent(project_dir, "claude", &script);
    create_mock_agent(project_dir, "codex", &script);
}

fn test_path(project_dir: &Path) -> String {
    let bin_dir = project_dir.join("bin");
    format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin_dir.display())
}

fn run_implement_cmd(project_dir: &Path, extra_args: &[&str]) -> std::process::Output {
    std::process::Command::new(agent_loop_bin())
        .arg("implement")
        .arg("--single-agent")
        .args(extra_args)
        .env("PATH", test_path(project_dir))
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "30")
        .env("REVIEW_MAX_ROUNDS", "1")
        .current_dir(project_dir)
        .output()
        .expect("agent-loop should run")
}

#[cfg(unix)]
#[test]
fn implement_runs_all_tasks_in_one_batch_by_default() {
    let project_dir = create_project_dir("batch_default");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Setup\nSetup\n\n### Task 2: Build\nBuild\n",
    );

    let output = run_implement_cmd(&project_dir, &[]);
    assert!(
        output.status.success(),
        "implement should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let combined_task = read_state_file(&project_dir, "task.md");
    assert!(
        combined_task.contains("Implement ALL tasks below as one cohesive change set."),
        "combined task should use batch instruction prefix"
    );
    assert!(combined_task.contains("### Task 1: Setup"));
    assert!(combined_task.contains("### Task 2: Build"));

    // Batch mode writes aggregate lifecycle files (single entry).
    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["status"], "done");
    assert!(
        tasks[0]["title"]
            .as_str()
            .unwrap_or_default()
            .contains("Batch implementation"),
        "batch status entry should use aggregate title"
    );

    let metrics = read_task_metrics(&project_dir);
    let metric_entries = metrics["tasks"].as_array().expect("metrics tasks array");
    assert_eq!(metric_entries.len(), 1);
    assert!(
        metric_entries[0]["title"]
            .as_str()
            .unwrap_or_default()
            .contains("Batch implementation"),
        "batch metrics entry should use aggregate title"
    );
    assert!(
        metric_entries[0]["duration_ms"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "batch metrics should include duration"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_continue_on_fail_runs_remaining_tasks() {
    let project_dir = create_project_dir("continue_on_fail");
    create_agent_succeeds_on_nth(&project_dir, 2);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Fails\nA\n\n### Task 2: Passes\nB\n",
    );

    let output = run_implement_cmd(&project_dir, &["--per-task", "--continue-on-fail"]);
    assert!(
        !output.status.success(),
        "should exit non-zero with failures"
    );

    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["status"], "failed");
    assert_eq!(tasks[1]["status"], "done");

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_fail_fast_stops_after_first_failure_in_per_task_mode() {
    let project_dir = create_project_dir("fail_fast");
    create_agent_succeeds_on_nth(&project_dir, 99);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Fails\nA\n\n### Task 2: Not reached\nB\n",
    );

    let output = run_implement_cmd(&project_dir, &["--per-task", "--fail-fast"]);
    assert!(!output.status.success(), "implement should fail");

    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks[0]["status"], "failed");
    assert_eq!(tasks[1]["status"], "pending");

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_per_task_clears_implementation_sessions_between_tasks() {
    let project_dir = create_project_dir("per_task_session_isolation");
    create_resume_sensitive_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Baseline\nA\n\n### Task 2: No resume leakage\nB\n",
    );

    let output = run_implement_cmd(&project_dir, &["--per-task"]);
    assert!(
        output.status.success(),
        "implement should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let leak_marker = project_dir
        .join(".agent-loop")
        .join("state")
        .join("resume_leak_detected");
    assert!(
        !leak_marker.exists(),
        "per-task mode should not pass --resume into a new task's first round"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_per_task_progress_keeps_task_boundaries() {
    let project_dir = create_project_dir("per_task_progress_boundaries");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Setup\nA\n\n### Task 2: Ship\nB\n",
    );

    let output = run_implement_cmd(&project_dir, &["--per-task"]);
    assert!(
        output.status.success(),
        "implement should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let progress = read_state_file(&project_dir, "implement-progress.md");
    assert_eq!(
        progress.matches("### Task: ").count(),
        2,
        "each task should get its own progress heading: {progress}"
    );
    assert_eq!(
        progress.matches("## Round 1").count(),
        2,
        "same-numbered rounds from different tasks should not merge: {progress}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_uses_per_task_mode_when_batch_implement_disabled_in_toml() {
    let project_dir = create_project_dir("batch_implement_disabled");
    create_succeeding_agents(&project_dir);
    fs::write(
        project_dir.join(".agent-loop.toml"),
        "batch_implement = false\n",
    )
    .expect("toml should be written");

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Setup\nSetup\n\n### Task 2: Build\nBuild\n",
    );

    let output = run_implement_cmd(&project_dir, &[]);
    assert!(
        output.status.success(),
        "implement should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let status = read_task_status(&project_dir);
    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["status"], "done");
    assert_eq!(tasks[1]["status"], "done");

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_rejects_per_task_flags_without_per_task_mode() {
    let project_dir = create_project_dir("per_task_flags_require_mode");
    create_succeeding_agents(&project_dir);
    write_state_file(&project_dir, "tasks.md", "### Task 1: A\nA\n");

    let output = run_implement_cmd(&project_dir, &["--continue-on-fail"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "should fail without --per-task");
    assert!(stderr.contains("Per-task lifecycle flags require per-task mode"));

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_rejects_max_parallel_zero() {
    let project_dir = create_project_dir("max_parallel_zero");
    create_succeeding_agents(&project_dir);
    write_state_file(&project_dir, "tasks.md", "### Task 1: A\nA\n");

    let output = run_implement_cmd(&project_dir, &["--max-parallel", "0"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "should fail for max-parallel=0");
    assert!(stderr.contains("--max-parallel"));

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_batch_mode_falls_back_to_plan_when_tasks_missing() {
    let project_dir = create_project_dir("batch_fallback_to_plan_missing_tasks");
    create_succeeding_agents(&project_dir);

    write_state_file(&project_dir, "plan.md", "# Plan\n- Step 1\n- Step 2\n");
    write_state_file(&project_dir, "task.md", "Implement from plan only.\n");

    let output = run_implement_cmd(&project_dir, &[]);
    assert!(
        output.status.success(),
        "implement should succeed via plan fallback: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let combined_task = read_state_file(&project_dir, "task.md");
    assert!(
        combined_task.contains("Implement the approved plan below as one cohesive change set.")
    );
    assert!(combined_task.contains("PLAN:"));

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_batch_mode_falls_back_to_plan_when_tasks_empty() {
    let project_dir = create_project_dir("batch_fallback_to_plan_empty_tasks");
    create_succeeding_agents(&project_dir);

    write_state_file(&project_dir, "tasks.md", " \n\t\n");
    write_state_file(
        &project_dir,
        "plan.md",
        "# Plan\n- Recover from empty tasks\n",
    );

    let output = run_implement_cmd(&project_dir, &[]);
    assert!(
        output.status.success(),
        "implement should succeed via plan fallback when tasks.md is empty: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let combined_task = read_state_file(&project_dir, "task.md");
    assert!(
        combined_task.contains("Implement the approved plan below as one cohesive change set.")
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_batch_disabled_requires_tasks_even_with_valid_plan() {
    let project_dir = create_project_dir("batch_disabled_requires_tasks");
    create_succeeding_agents(&project_dir);
    fs::write(
        project_dir.join(".agent-loop.toml"),
        "batch_implement = false\n",
    )
    .expect("toml should be written");

    write_state_file(
        &project_dir,
        "plan.md",
        "# Plan\n- Should not be used when batch_implement=false\n",
    );

    let output = run_implement_cmd(&project_dir, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "implement should fail when tasks.md is missing and batch_implement=false"
    );
    assert!(stderr.contains("No tasks found. Run 'agent-loop tasks' first."));

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_per_task_flag_requires_tasks_even_with_valid_plan() {
    let project_dir = create_project_dir("per_task_requires_tasks_even_with_plan");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "plan.md",
        "# Plan\n- Explicit per-task should still require tasks.md\n",
    );

    let output = run_implement_cmd(&project_dir, &["--per-task"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "implement should fail without tasks.md"
    );
    assert!(stderr.contains("No tasks found. Run 'agent-loop tasks' first."));

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_wave_requires_tasks_even_with_valid_plan() {
    let project_dir = create_project_dir("wave_requires_tasks_even_with_plan");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "plan.md",
        "# Plan\n- Wave mode should still require tasks.md\n",
    );

    let output = run_implement_cmd(&project_dir, &["--wave"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "implement --wave should fail without tasks.md"
    );
    assert!(stderr.contains("No tasks found. Run 'agent-loop tasks' first."));

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_resume_skips_plan_fallback_path() {
    let project_dir = create_project_dir("resume_skips_plan_fallback");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "task.md",
        "Resume task should remain unchanged.\n",
    );
    write_state_file(
        &project_dir,
        "plan.md",
        "# Plan\n- This plan exists but must not be used by --resume\n",
    );
    write_state_file(&project_dir, "workflow.txt", "implement\n");
    write_state_file(
        &project_dir,
        "status.json",
        r#"{"status":"NEEDS_CHANGES","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"resume task","reason":"resume needed","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );

    let output = std::process::Command::new(agent_loop_bin())
        .arg("implement")
        .arg("--single-agent")
        .arg("--resume")
        .env("PATH", test_path(&project_dir))
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "30")
        .env("REVIEW_MAX_ROUNDS", "5")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop implement --resume should run");

    assert!(
        output.status.success(),
        "implement --resume should succeed without tasks.md fallback: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let task_after = read_state_file(&project_dir, "task.md");
    assert!(
        task_after.contains("Resume task should remain unchanged."),
        "resume path should preserve existing task.md"
    );
    assert!(
        !task_after.contains("Implement the approved plan below as one cohesive change set."),
        "resume path should not rewrite task.md via plan fallback"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_wave_resume_routes_to_wave_mode_without_resume_state() {
    let project_dir = create_project_dir("wave_resume_routes_to_wave_mode");
    create_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Wave resume route\nA\n",
    );
    write_state_file(&project_dir, "workflow.txt", "implement\n");
    write_state_file(
        &project_dir,
        "status.json",
        r#"{"status":"NEEDS_CHANGES","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","reason":"resume needed","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );

    let output = run_implement_cmd(&project_dir, &["--wave", "--resume"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stdout.contains("Wave mode:"),
        "--wave --resume should route into wave mode: stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stderr.contains("Cannot resume:"),
        "--wave --resume should not use generic resume preconditions: stderr={stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_wave_resume_preserves_task_local_progress_history() {
    let project_dir = create_project_dir("wave_resume_progress_history");
    create_prompt_path_succeeding_agents(&project_dir);

    write_state_file(
        &project_dir,
        "tasks.md",
        "### Task 1: Preserve progress\nA\n",
    );
    write_state_file(&project_dir, "workflow.txt", "implement\n");
    write_state_file(
        &project_dir,
        "status.json",
        r#"{"status":"NEEDS_CHANGES","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","reason":"resume needed","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );
    write_state_file(&project_dir, "implement-mode.txt", "wave\n");

    let task_state_dir = project_dir.join(".agent-loop").join("state").join(".wave-task-1");
    fs::create_dir_all(&task_state_dir).expect("task state dir should exist");
    fs::write(
        task_state_dir.join("implement-progress.md"),
        "## Round 1\nPrevious attempt\n",
    )
    .expect("progress seed should be written");

    let _output = run_implement_cmd(&project_dir, &["--resume"]);

    let progress = fs::read_to_string(task_state_dir.join("implement-progress.md"))
        .expect("task progress should exist");
    assert!(
        progress.contains("Previous attempt"),
        "resumed wave task should preserve prior task-local progress: {progress}"
    );
    assert!(
        progress.contains("Implementation:"),
        "resumed wave task should append new progress entries: {progress}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_resume_uses_persisted_wave_flags() {
    let project_dir = create_project_dir("resume_persisted_wave_flags");
    create_prompt_path_succeeding_agents(&project_dir);

    write_state_file(&project_dir, "tasks.md", "### Task 1: Persist flags\nA\n");
    write_state_file(&project_dir, "workflow.txt", "implement\n");
    write_state_file(
        &project_dir,
        "status.json",
        r#"{"status":"NEEDS_CHANGES","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","reason":"resume needed","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );
    write_state_file(&project_dir, "implement-mode.txt", "wave\n");
    write_state_file(
        &project_dir,
        "implement-flags.json",
        r#"{"per_task":false,"wave":true,"max_retries":2,"round_step":2,"continue_on_fail":false,"fail_fast":false,"max_parallel":4}"#,
    );

    let output = run_implement_cmd(&project_dir, &["--resume"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("max_parallel=4"),
        "resume should reuse persisted wave max_parallel: stdout={stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// ---------------------------------------------------------------------------
// Session namespace integration tests
// ---------------------------------------------------------------------------

/// Mock agents that discover the state dir from prompt args even when relative
/// paths are used (as happens with session namespaces where prompts contain
/// `.agent-loop/state/<session>/task.md` instead of absolute paths).
#[cfg(unix)]
fn create_session_aware_succeeding_agents(project_dir: &Path) {
    let script = r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "mock 1.0.0"; exit 0 ;;
    esac
done
ALL_ARGS="$*"
# Try absolute path first, then relative (session prompts use relative paths)
TASK_FILE_PATH=$(printf '%s' "$ALL_ARGS" | grep -o '/[^"[:space:]]*\.agent-loop/state[^"[:space:]]*/task\.md' | head -n1)
if [ -z "$TASK_FILE_PATH" ]; then
    # Relative path: extract .agent-loop/state/.../task.md
    REL_PATH=$(printf '%s' "$ALL_ARGS" | grep -o '\.agent-loop/state[^"[:space:]]*/task\.md' | head -n1)
    if [ -n "$REL_PATH" ]; then
        TASK_FILE_PATH="$(pwd)/$REL_PATH"
    fi
fi
if [ -n "$TASK_FILE_PATH" ]; then
    STATE_DIR=$(dirname "$TASK_FILE_PATH")
else
    STATE_DIR="$(pwd)/.agent-loop/state"
fi
mkdir -p "$STATE_DIR"
STATUS_FILE="$STATE_DIR/status.json"
FINDINGS_FILE="$STATE_DIR/findings.json"
CURRENT_TS=$(printf '%s' "$ALL_ARGS" | sed -n 's/.*"timestamp"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | tail -1)
if [ -z "$CURRENT_TS" ]; then
    CURRENT_TS="2026-02-16T00:00:00.000Z"
fi
printf '{"round":1,"findings":[]}' > "$FINDINGS_FILE"
printf '{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","rating":5,"timestamp":"%s"}' "$CURRENT_TS" > "$STATUS_FILE"
exit 0
"#;
    create_mock_agent(project_dir, "claude", script);
    create_mock_agent(project_dir, "codex", script);
}

#[cfg(unix)]
#[test]
fn session_isolation_two_sessions_write_to_separate_dirs() {
    let project_dir = create_project_dir("session_isolation");
    create_session_aware_succeeding_agents(&project_dir);

    // Write tasks for session "alpha"
    write_session_state_file(
        &project_dir,
        "alpha",
        "tasks.md",
        "### Task 1: Alpha work\nAlpha\n",
    );

    // Write tasks for session "beta"
    write_session_state_file(
        &project_dir,
        "beta",
        "tasks.md",
        "### Task 1: Beta work\nBeta\n",
    );

    // Run implement for each session
    let output_alpha = run_implement_cmd(&project_dir, &["--session", "alpha"]);
    assert!(
        output_alpha.status.success(),
        "session alpha should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output_alpha.stdout),
        String::from_utf8_lossy(&output_alpha.stderr)
    );

    let output_beta = run_implement_cmd(&project_dir, &["--session", "beta"]);
    assert!(
        output_beta.status.success(),
        "session beta should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output_beta.stdout),
        String::from_utf8_lossy(&output_beta.stderr)
    );

    // Each session should have its own status.json
    let alpha_status = read_session_state_file(&project_dir, "alpha", "status.json");
    let beta_status = read_session_state_file(&project_dir, "beta", "status.json");

    assert!(
        !alpha_status.is_empty(),
        "alpha session should have status.json"
    );
    assert!(
        !beta_status.is_empty(),
        "beta session should have status.json"
    );

    // Each session should have its own task.md
    let alpha_task = read_session_state_file(&project_dir, "alpha", "task.md");
    let beta_task = read_session_state_file(&project_dir, "beta", "task.md");

    assert!(
        alpha_task.contains("Alpha"),
        "alpha task should contain alpha content: {alpha_task}"
    );
    assert!(
        beta_task.contains("Beta"),
        "beta task should contain beta content: {beta_task}"
    );

    // Default state dir should be unaffected
    let default_status = read_state_file(&project_dir, "status.json");
    assert!(
        default_status.is_empty(),
        "default session should have no status.json"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn reset_session_only_clears_that_sessions_state() {
    let project_dir = create_project_dir("reset_session_scoped");

    // Seed state for session "feat" and default session
    write_session_state_file(&project_dir, "feat", "status.json", r#"{"status":"APPROVED"}"#);
    write_session_state_file(&project_dir, "feat", "task.md", "feat task");
    write_state_file(&project_dir, "status.json", r#"{"status":"APPROVED"}"#);
    write_state_file(&project_dir, "task.md", "default task");

    // Also seed a second session
    write_session_state_file(&project_dir, "other", "status.json", r#"{"status":"APPROVED"}"#);

    // Run reset --session feat
    let output = std::process::Command::new(agent_loop_bin())
        .args(["reset", "--session", "feat"])
        .current_dir(&project_dir)
        .output()
        .expect("reset should run");

    assert!(
        output.status.success(),
        "reset --session feat should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Session "feat" state should be cleared
    let feat_status = read_session_state_file(&project_dir, "feat", "status.json");
    assert!(
        feat_status.is_empty(),
        "feat session state should be cleared after reset"
    );

    // Default session state should be untouched
    let default_status = read_state_file(&project_dir, "status.json");
    assert!(
        !default_status.is_empty(),
        "default session state should survive reset --session feat"
    );

    // Other session state should be untouched
    let other_status = read_session_state_file(&project_dir, "other", "status.json");
    assert!(
        !other_status.is_empty(),
        "other session state should survive reset --session feat"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn status_session_reads_from_session_state_dir() {
    let project_dir = create_project_dir("status_session");

    // Seed session "dev" with specific status
    write_session_state_file(
        &project_dir,
        "dev",
        "status.json",
        r#"{"status":"NEEDS_CHANGES","round":3,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"setup","reason":"needs work","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );

    // Seed default session with different status
    write_state_file(
        &project_dir,
        "status.json",
        r#"{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","timestamp":"2026-02-16T00:00:00.000Z"}"#,
    );

    // Run status --session dev
    let output = std::process::Command::new(agent_loop_bin())
        .args(["status", "--session", "dev"])
        .current_dir(&project_dir)
        .output()
        .expect("status should run");

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "status --session dev should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Should show session dev's status, not default
    assert!(
        stdout.contains("NEEDS_CHANGES"),
        "status --session dev should show session's status: {stdout}"
    );
    assert!(
        stdout.contains("round: 3"),
        "status --session dev should show session's round: {stdout}"
    );

    // Verify default session shows different status
    let default_output = std::process::Command::new(agent_loop_bin())
        .args(["status"])
        .current_dir(&project_dir)
        .output()
        .expect("status should run");

    let default_stdout = String::from_utf8_lossy(&default_output.stdout);
    assert!(
        default_stdout.contains("APPROVED"),
        "default status should show APPROVED, not session dev's status: {default_stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}
