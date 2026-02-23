//! Integration tests verifying removed legacy commands produce clap parse errors
//! and renamed config keys produce actionable migration errors.

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

fn run_with_env(tmp: &TempDir, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(agent_loop_bin());
    cmd.args(args).current_dir(tmp.path());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("agent-loop should execute")
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

// -----------------------------------------------------------------------
// Renamed config key migration errors (MAX_ROUNDS → REVIEW_MAX_ROUNDS)
// -----------------------------------------------------------------------

/// Setting the legacy `MAX_ROUNDS` env var should produce a rename migration
/// error telling the user to switch to `REVIEW_MAX_ROUNDS`.
#[test]
fn max_rounds_env_var_rejected_with_rename_guidance() {
    let tmp = TempDir::new("max_rounds_env");
    let output = run_with_env(&tmp, &["status"], &[("MAX_ROUNDS", "10")]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "MAX_ROUNDS env var should cause a failure, stderr: {stderr}"
    );
    assert!(
        stderr.contains("renamed to `REVIEW_MAX_ROUNDS`"),
        "expected rename guidance for MAX_ROUNDS env var, got: {stderr}"
    );
}

/// A TOML file containing the legacy `max_rounds` key should produce a rename
/// migration error pointing at `review_max_rounds`.
#[test]
fn max_rounds_toml_key_rejected_with_rename_guidance() {
    let tmp = TempDir::new("max_rounds_toml");
    fs::write(
        tmp.path().join(".agent-loop.toml"),
        "max_rounds = 10\n",
    )
    .expect("toml should be written");

    let output = run_in_tmp(&tmp, &["status"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "max_rounds TOML key should cause a failure, stderr: {stderr}"
    );
    assert!(
        stderr.contains("renamed to `review_max_rounds`"),
        "expected rename guidance for max_rounds TOML key, got: {stderr}"
    );
}

/// `REVIEW_MAX_ROUNDS=0` (unlimited) should be accepted without error.
/// The `status` command should succeed and not reject the value.
#[test]
fn review_max_rounds_zero_accepted_at_cli_level() {
    let tmp = TempDir::new("review_max_rounds_zero");
    let output = run_with_env(&tmp, &["status"], &[("REVIEW_MAX_ROUNDS", "0")]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "REVIEW_MAX_ROUNDS=0 should be accepted, stderr: {stderr}"
    );
}

/// `PLANNING_MAX_ROUNDS=0` (unlimited) should be accepted without error.
#[test]
fn planning_max_rounds_zero_accepted_at_cli_level() {
    let tmp = TempDir::new("planning_max_rounds_zero");
    let output = run_with_env(&tmp, &["status"], &[("PLANNING_MAX_ROUNDS", "0")]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "PLANNING_MAX_ROUNDS=0 should be accepted, stderr: {stderr}"
    );
}

/// `DECOMPOSITION_MAX_ROUNDS=0` (unlimited) should be accepted without error.
#[test]
fn decomposition_max_rounds_zero_accepted_at_cli_level() {
    let tmp = TempDir::new("decomp_max_rounds_zero");
    let output = run_with_env(&tmp, &["status"], &[("DECOMPOSITION_MAX_ROUNDS", "0")]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "DECOMPOSITION_MAX_ROUNDS=0 should be accepted, stderr: {stderr}"
    );
}

/// Negative `REVIEW_MAX_ROUNDS` should be rejected at the binary level.
#[test]
fn review_max_rounds_negative_rejected_at_cli_level() {
    let tmp = TempDir::new("review_max_rounds_neg");
    let output = run_with_env(&tmp, &["status"], &[("REVIEW_MAX_ROUNDS", "-1")]);
    assert!(
        !output.status.success(),
        "REVIEW_MAX_ROUNDS=-1 should be rejected"
    );
}

/// Non-numeric `REVIEW_MAX_ROUNDS` should be rejected at the binary level.
#[test]
fn review_max_rounds_non_numeric_rejected_at_cli_level() {
    let tmp = TempDir::new("review_max_rounds_nan");
    let output = run_with_env(&tmp, &["status"], &[("REVIEW_MAX_ROUNDS", "abc")]);
    assert!(
        !output.status.success(),
        "REVIEW_MAX_ROUNDS=abc should be rejected"
    );
}
