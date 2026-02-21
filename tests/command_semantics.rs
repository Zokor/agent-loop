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
    assert!(stderr.contains("No tasks found. Run 'agent-loop tasks' first."));

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
