//! Integration tests for new command semantics.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn agent_loop_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-loop"))
}

fn create_project_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agent_loop_command_semantics_{}_{}_{}",
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

#[cfg(unix)]
fn create_mock_agent(project_dir: &Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = project_dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("bin dir should be created");

    let path = bin_dir.join(name);
    fs::write(&path, script).expect("agent script should be written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("agent script should be executable");
}

#[cfg(unix)]
fn create_planning_agents(project_dir: &Path) {
    let script = r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "mock 1.0.0"; exit 0 ;;
    esac
done
STATE_DIR="$(pwd)/.agent-loop/state"
mkdir -p "$STATE_DIR"
printf '# Plan\n- Step 1\n' > "$STATE_DIR/plan.md"
ALL_ARGS="$*"
CURRENT_TS=$(printf '%s' "$ALL_ARGS" | sed -n 's/.*"timestamp"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | tail -1)
if [ -z "$CURRENT_TS" ]; then
    CURRENT_TS="2026-02-16T00:00:00.000Z"
fi
printf '{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","timestamp":"%s"}' "$CURRENT_TS" > "$STATE_DIR/status.json"
exit 0
"#;
    create_mock_agent(project_dir, "claude", script);
    create_mock_agent(project_dir, "codex", script);
}

#[cfg(unix)]
fn create_implementation_agents(project_dir: &Path) {
    let script = r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "mock 1.0.0"; exit 0 ;;
    esac
done
STATE_DIR="$(pwd)/.agent-loop/state"
mkdir -p "$STATE_DIR"
STATUS_FILE="$STATE_DIR/status.json"
FINDINGS_FILE="$STATE_DIR/findings.json"
ALL_ARGS="$*"
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
fn create_dual_signoff_agents(project_dir: &Path) {
    let implementer_script = r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "mock 1.0.0"; exit 0 ;;
    esac
done

STATE_DIR="$(pwd)/.agent-loop/state"
mkdir -p "$STATE_DIR"
ALL_ARGS="$*"
CURRENT_TS=$(printf '%s' "$ALL_ARGS" | grep -Eo '[0-9]{4}-[0-9]{2}-[0-9]{2}T[^"]+Z' | tail -1)
if [ -z "$CURRENT_TS" ]; then
    CURRENT_TS="2026-02-16T00:00:00.000Z"
fi

if printf '%s' "$ALL_ARGS" | grep -q "The reviewer has APPROVED your plan"; then
    : > "$STATE_DIR/plan_signoff_seen"
    printf '{"status":"CONSENSUS","round":1,"implementer":"claude","reviewer":"codex","mode":"dual-agent","lastRunTask":"","timestamp":"%s"}' "$CURRENT_TS" > "$STATE_DIR/status.json"
    exit 0
fi

if printf '%s' "$ALL_ARGS" | grep -q "The reviewer has APPROVED the task breakdown"; then
    : > "$STATE_DIR/tasks_signoff_seen"
    printf '{"status":"CONSENSUS","round":1,"implementer":"claude","reviewer":"codex","mode":"dual-agent","lastRunTask":"","timestamp":"%s"}' "$CURRENT_TS" > "$STATE_DIR/status.json"
    exit 0
fi

if printf '%s' "$ALL_ARGS" | grep -q "Read the task below and propose a detailed development plan"; then
    printf '# Plan\n- Dual-agent signoff path\n' > "$STATE_DIR/plan.md"
    exit 0
fi

if printf '%s' "$ALL_ARGS" | grep -q "Both agents have agreed on the following development plan"; then
    printf '### Task 1: Signed Off\nDo the work.\n' > "$STATE_DIR/tasks.md"
    exit 0
fi

exit 0
"#;

    let reviewer_script = r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "mock 1.0.0"; exit 0 ;;
    esac
done

STATE_DIR="$(pwd)/.agent-loop/state"
mkdir -p "$STATE_DIR"
ALL_ARGS="$*"
CURRENT_TS=$(printf '%s' "$ALL_ARGS" | grep -Eo '[0-9]{4}-[0-9]{2}-[0-9]{2}T[^"]+Z' | tail -1)
if [ -z "$CURRENT_TS" ]; then
    CURRENT_TS="2026-02-16T00:00:00.000Z"
fi

if printf '%s' "$ALL_ARGS" | grep -q "Review this development plan against the original task"; then
    printf '{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"codex","mode":"dual-agent","lastRunTask":"","timestamp":"%s"}' "$CURRENT_TS" > "$STATE_DIR/status.json"
    exit 0
fi

if printf '%s' "$ALL_ARGS" | grep -q "The implementer has broken down the agreed plan into discrete tasks"; then
    printf '{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"codex","mode":"dual-agent","lastRunTask":"","timestamp":"%s"}' "$CURRENT_TS" > "$STATE_DIR/status.json"
    exit 0
fi

exit 0
"#;

    create_mock_agent(project_dir, "claude", implementer_script);
    create_mock_agent(project_dir, "codex", reviewer_script);
}

fn run_cmd(project_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(agent_loop_bin())
        .args(args)
        .current_dir(project_dir)
        .output()
        .expect("agent-loop should execute")
}

#[cfg(unix)]
#[test]
fn plan_creates_plan_md_and_not_tasks_md() {
    let project_dir = create_project_dir("plan_only");
    create_planning_agents(&project_dir);

    let output = Command::new(agent_loop_bin())
        .args(["plan", "test planning task", "--single-agent"])
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                project_dir.join("bin").display()
            ),
        )
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "30")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop plan should execute");

    assert!(
        output.status.success(),
        "plan should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let plan_path = project_dir
        .join(".agent-loop")
        .join("state")
        .join("plan.md");
    let tasks_path = project_dir
        .join(".agent-loop")
        .join("state")
        .join("tasks.md");
    assert!(
        plan_path.is_file(),
        "plan.md should exist after plan command"
    );
    assert!(
        !tasks_path.exists(),
        "tasks.md should not be created by plan command"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn tasks_errors_without_plan() {
    let project_dir = create_project_dir("tasks_no_plan");

    let output = run_cmd(&project_dir, &["tasks"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("No plan found. Run 'agent-loop plan' first."));

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn implement_errors_without_tasks() {
    let project_dir = create_project_dir("implement_no_tasks");

    let output = run_cmd(&project_dir, &["implement"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("No tasks found and no plan found."));

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn plan_resets_state_dir_before_run_and_preserves_decisions() {
    let project_dir = create_project_dir("plan_resets_state");
    create_planning_agents(&project_dir);

    let state_dir = project_dir.join(".agent-loop").join("state");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    fs::write(state_dir.join("stale.tmp"), "old").expect("stale file should be written");
    fs::write(state_dir.join("tasks.md"), "stale tasks").expect("stale tasks should be written");

    let decisions_path = project_dir.join(".agent-loop").join("decisions.md");
    fs::create_dir_all(decisions_path.parent().expect("decisions parent")).expect("parent");
    fs::write(&decisions_path, "- [PATTERN] preserve me\n").expect("decisions should be written");

    let output = Command::new(agent_loop_bin())
        .args(["plan", "reset verification", "--single-agent"])
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                project_dir.join("bin").display()
            ),
        )
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "30")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop plan should execute");

    assert!(
        output.status.success(),
        "plan should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !state_dir.join("stale.tmp").exists(),
        "stale state file should be removed"
    );
    assert!(
        !state_dir.join("tasks.md").exists(),
        "stale tasks.md should be removed on fresh plan run"
    );
    assert!(
        state_dir.join("plan.md").exists(),
        "new plan.md should exist"
    );

    let decisions = fs::read_to_string(&decisions_path).expect("decisions should still exist");
    assert!(decisions.contains("preserve me"));

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn implement_falls_back_to_plan_when_tasks_missing() {
    let project_dir = create_project_dir("implement_fallback_plan");
    create_implementation_agents(&project_dir);

    let state_dir = project_dir.join(".agent-loop").join("state");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    fs::write(
        state_dir.join("plan.md"),
        "# Plan\n1. Add fallback behavior\n2. Keep per-task strictness\n",
    )
    .expect("plan.md should be written");
    fs::write(state_dir.join("task.md"), "Implement fallback support.\n")
        .expect("task.md should be written");

    let output = Command::new(agent_loop_bin())
        .args(["implement", "--single-agent"])
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                project_dir.join("bin").display()
            ),
        )
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "30")
        .env("REVIEW_MAX_ROUNDS", "1")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop implement should execute");

    assert!(
        output.status.success(),
        "implement should succeed with plan fallback: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined_task = fs::read_to_string(state_dir.join("task.md")).expect("task.md");
    assert!(
        combined_task.contains("Implement the approved plan below as one cohesive change set."),
        "fallback combined task should use plan instruction prefix"
    );
    assert!(
        combined_task.contains("PLAN:"),
        "fallback combined task should include plan context"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn dual_agent_plan_requires_and_executes_implementer_signoff() {
    let project_dir = create_project_dir("dual_plan_signoff");
    create_dual_signoff_agents(&project_dir);

    let output = Command::new(agent_loop_bin())
        .args(["plan", "verify dual signoff"])
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                project_dir.join("bin").display()
            ),
        )
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "30")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop plan should execute");

    assert!(
        output.status.success(),
        "dual-agent plan should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let state_dir = project_dir.join(".agent-loop").join("state");
    assert!(
        state_dir.join("plan_signoff_seen").exists(),
        "implementer plan signoff prompt should be executed"
    );
    let status_raw = fs::read_to_string(state_dir.join("status.json")).expect("status.json");
    let status: serde_json::Value =
        serde_json::from_str(&status_raw).expect("status.json should be valid JSON");
    assert_eq!(
        status["status"].as_str(),
        Some("CONSENSUS"),
        "final plan status should be CONSENSUS in dual-agent flow: {status_raw}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[cfg(unix)]
#[test]
fn dual_agent_tasks_requires_and_executes_implementer_signoff() {
    let project_dir = create_project_dir("dual_tasks_signoff");
    create_dual_signoff_agents(&project_dir);

    let state_dir = project_dir.join(".agent-loop").join("state");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    fs::write(
        state_dir.join("plan.md"),
        "# Plan\n- Task decomposition should require signoff\n",
    )
    .expect("plan.md should be seeded");

    let output = Command::new(agent_loop_bin())
        .args(["tasks"])
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                project_dir.join("bin").display()
            ),
        )
        .env("AUTO_COMMIT", "0")
        .env("TIMEOUT", "30")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop tasks should execute");

    assert!(
        output.status.success(),
        "dual-agent tasks should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        state_dir.join("tasks_signoff_seen").exists(),
        "implementer decomposition signoff prompt should be executed"
    );
    let status_raw = fs::read_to_string(state_dir.join("status.json")).expect("status.json");
    let status: serde_json::Value =
        serde_json::from_str(&status_raw).expect("status.json should be valid JSON");
    assert_eq!(
        status["status"].as_str(),
        Some("CONSENSUS"),
        "final tasks status should be CONSENSUS in dual-agent flow: {status_raw}"
    );
    assert!(
        state_dir.join("tasks.md").exists(),
        "tasks.md should be produced"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn reset_clears_state_and_preserves_decisions() {
    let project_dir = create_project_dir("reset_preserves_decisions");
    let state_dir = project_dir.join(".agent-loop").join("state");
    fs::create_dir_all(&state_dir).expect("state dir should be created");
    fs::write(state_dir.join("status.json"), "{}\n").expect("status file should be written");

    let decisions_path = project_dir.join(".agent-loop").join("decisions.md");
    fs::create_dir_all(decisions_path.parent().unwrap()).expect(".agent-loop should be created");
    fs::write(&decisions_path, "- [PATTERN] Keep state small\n")
        .expect("decisions file should be written");

    let output = run_cmd(&project_dir, &["reset"]);
    assert!(
        output.status.success(),
        "reset should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(state_dir.is_dir(), "state dir should exist after reset");
    let state_entries = fs::read_dir(&state_dir)
        .expect("state dir should be readable")
        .count();
    assert_eq!(state_entries, 0, "state dir should be empty after reset");

    let decisions = fs::read_to_string(&decisions_path).expect("decisions should still exist");
    assert!(decisions.contains("[PATTERN] Keep state small"));

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn status_reads_wave_lock_and_journal_from_agent_loop_dir() {
    let project_dir = create_project_dir("status_wave_paths");
    let agent_loop_dir = project_dir.join(".agent-loop");
    let state_dir = agent_loop_dir.join("state");
    fs::create_dir_all(&state_dir).expect("state dir should be created");

    // status.json lives under state/
    fs::write(
        state_dir.join("status.json"),
        r#"{"status":"APPROVED","round":1,"implementer":"claude","reviewer":"claude","mode":"single-agent","lastRunTask":"","timestamp":"2026-01-01T00:00:00Z"}"#,
    )
    .expect("status.json should be written");

    // wave.lock lives under .agent-loop/ (NOT state/)
    fs::write(
        agent_loop_dir.join("wave.lock"),
        r#"{"pid":99999,"started_at":"2026-01-01T00:00:00Z","mode":"wave","max_parallel":2}"#,
    )
    .expect("wave.lock should be written");

    // wave-progress.jsonl also lives under .agent-loop/
    fs::write(
        agent_loop_dir.join("wave-progress.jsonl"),
        "{\"type\":\"RunStart\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"max_parallel\":2,\"total_tasks\":1,\"total_waves\":1}\n",
    )
    .expect("wave-progress.jsonl should be written");

    let output = run_cmd(&project_dir, &["status"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "status should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("wave lock:"),
        "status should report wave lock read from .agent-loop/: {stdout}"
    );
    assert!(
        stdout.contains("Recent wave events:"),
        "status should report journal events read from .agent-loop/: {stdout}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

// -----------------------------------------------------------------------
// config init integration tests
// -----------------------------------------------------------------------

#[test]
fn config_init_creates_toml_file() {
    let project_dir = create_project_dir("config_init_creates");

    let output = run_cmd(&project_dir, &["config", "init"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "config init should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Generated .agent-loop.toml"),
        "should print confirmation: {stdout}"
    );

    let config_path = project_dir.join(".agent-loop.toml");
    assert!(config_path.is_file(), ".agent-loop.toml should be created");

    let content = fs::read_to_string(&config_path).expect("config should be readable");
    // Verify key section markers from the template
    assert!(
        content.contains("# ── Core"),
        "template should contain Core section"
    );
    assert!(
        content.contains("# ── Agents"),
        "template should contain Agents section"
    );
    assert!(
        content.contains("# ── Stuck detection"),
        "template should contain Stuck detection section"
    );
    // Verify all value lines are commented out (safe to deploy)
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        panic!("found uncommented value line in generated config: {trimmed}");
    }

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn config_init_refuses_overwrite_without_force() {
    let project_dir = create_project_dir("config_init_no_overwrite");
    let config_path = project_dir.join(".agent-loop.toml");
    fs::write(&config_path, "# existing config\n").expect("seed config should be written");

    let output = run_cmd(&project_dir, &["config", "init"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "config init should exit 1 when file exists"
    );
    assert!(
        stderr.contains("already exists") && stderr.contains("--force"),
        "stderr should mention existing file and --force flag: {stderr}"
    );

    // File should be unchanged
    let content = fs::read_to_string(&config_path).expect("config should be readable");
    assert_eq!(
        content, "# existing config\n",
        "existing file should be preserved"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn config_init_force_overwrites_existing_file() {
    let project_dir = create_project_dir("config_init_force");
    let config_path = project_dir.join(".agent-loop.toml");
    fs::write(&config_path, "old content").expect("seed config should be written");

    let output = run_cmd(&project_dir, &["config", "init", "--force"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "config init --force should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Generated .agent-loop.toml"),
        "should print confirmation: {stdout}"
    );

    let content = fs::read_to_string(&config_path).expect("config should be readable");
    assert!(
        !content.contains("old content"),
        "old content should be replaced"
    );
    assert!(
        content.contains("# ── Core"),
        "new template should contain Core section"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

#[test]
fn config_init_generated_file_is_valid_toml() {
    let project_dir = create_project_dir("config_init_valid_toml");

    let output = run_cmd(&project_dir, &["config", "init"]);
    assert!(
        output.status.success(),
        "config init should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    // After generating, running `status` should work without parse errors
    // (all lines are commented so it parses as empty valid TOML)
    let status_output = run_cmd(&project_dir, &["status"]);
    let stderr = String::from_utf8_lossy(&status_output.stderr);

    // Assert the status command itself succeeded — a TOML parse error would
    // cause a non-zero exit code, not just a substring in stderr.
    assert!(
        status_output.status.success(),
        "status should succeed with generated config: stderr={stderr}"
    );
    // Belt-and-suspenders: also check no parse-error message
    assert!(
        !stderr.contains("failed to parse"),
        "generated config should be valid TOML: {stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

/// Verifies the hint is absent when running via `Command::output()` with CI=true.
///
/// NOTE: `Command::output()` uses piped stderr (non-TTY), so *both* the
/// `is_terminal` guard and the `CI` guard suppress the hint here. This test
/// validates the end-to-end integration path (piped CI = no hint), but it
/// cannot isolate the CI branch alone. The CI-specific guard is exercised in
/// the unit test `config::tests::hint_suppressed_when_ci_set_even_with_terminal`,
/// which injects `is_terminal=true` to isolate the CI check.
#[test]
fn missing_config_hint_not_shown_in_piped_ci_context() {
    let project_dir = create_project_dir("config_hint_ci");

    let output = Command::new(agent_loop_bin())
        .args(["status"])
        .env("CI", "true")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("agent-loop config init"),
        "missing-config hint should be suppressed in piped CI context: {stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}

/// Verifies the hint is also absent when running piped (non-TTY) without CI.
/// This confirms the `is_terminal` guard works independently of CI.
#[test]
fn missing_config_hint_not_shown_when_piped_without_ci() {
    let project_dir = create_project_dir("config_hint_piped");

    let output = Command::new(agent_loop_bin())
        .args(["status"])
        .env_remove("CI")
        .current_dir(&project_dir)
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("agent-loop config init"),
        "missing-config hint should be suppressed in non-TTY (piped) context: {stderr}"
    );

    let _ = fs::remove_dir_all(&project_dir);
}
