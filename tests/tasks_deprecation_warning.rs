//! Integration test for removed `tasks --tasks-file` flag.

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
            "agent_loop_tasks_migration_{}_{}_{}",
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

#[test]
fn tasks_tasks_file_returns_migration_error() {
    let tmp = TempDir::new("tasks_file");

    let output = Command::new(agent_loop_bin())
        .args(["tasks", "--tasks-file", "legacy.md"])
        .current_dir(tmp.path())
        .output()
        .expect("agent-loop should execute");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("Error: '--tasks-file' has been removed. Use '--file' instead."),
        "stderr should contain migration error, got: {stderr}"
    );
}
