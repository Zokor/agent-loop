//! Integration tests verifying removed legacy commands produce clap parse errors.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn agent_loop_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agent-loop"))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agent_loop_migration_{}_{}_{}",
            name,
            std::process::id(),
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

fn run_in_tmp(tmp: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(agent_loop_bin())
        .args(args)
        .current_dir(tmp.path())
        .output()
        .expect("agent-loop should execute")
}

#[test]
fn run_returns_clap_parse_error() {
    let tmp = TempDir::new("run");
    let output = run_in_tmp(&tmp, &["run"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected clap parse error for 'run', got: {stderr}"
    );
}

#[test]
fn run_planning_only_returns_clap_parse_error() {
    let tmp = TempDir::new("run_planning_only");
    let output = run_in_tmp(&tmp, &["run", "--planning-only"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected clap parse error for 'run --planning-only', got: {stderr}"
    );
}

#[test]
fn run_resume_returns_clap_parse_error() {
    let tmp = TempDir::new("run_resume");
    let output = run_in_tmp(&tmp, &["run", "--resume"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected clap parse error for 'run --resume', got: {stderr}"
    );
}

#[test]
fn run_tasks_returns_clap_parse_error() {
    let tmp = TempDir::new("run_tasks");
    let output = run_in_tmp(&tmp, &["run-tasks"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected clap parse error for 'run-tasks', got: {stderr}"
    );
}

#[test]
fn init_returns_clap_parse_error() {
    let tmp = TempDir::new("init");
    let output = run_in_tmp(&tmp, &["init"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected clap parse error for 'init', got: {stderr}"
    );
}

#[test]
fn resume_returns_clap_parse_error() {
    let tmp = TempDir::new("resume");
    let output = run_in_tmp(&tmp, &["resume"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("unrecognized subcommand"),
        "expected clap parse error for 'resume', got: {stderr}"
    );
}
