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
        .env("MAX_ROUNDS", "1")
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
        .env("MAX_ROUNDS", "5")
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
