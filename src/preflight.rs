use std::process::Command;
use std::time::Duration;

use crate::config::{Agent, Config};
use crate::error::AgentLoopError;

/// Maximum time to wait for a `--version` probe before assuming the binary is
/// unresponsive.  This prevents preflight from stalling startup indefinitely
/// if a CLI hangs.
const BINARY_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn install_hint(binary: &str) -> &str {
    match binary {
        "claude" => "Install claude: npm install -g @anthropic-ai/claude-code",
        "codex" => "Install codex: npm install -g @openai/codex",
        "git" => "Install git: https://git-scm.com/downloads",
        _ => "",
    }
}

fn check_binary_available(binary: &str) -> bool {
    let mut child = match Command::new(binary)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    // Wait with a timeout so a hanging binary doesn't block startup.
    let deadline = std::time::Instant::now() + BINARY_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

fn required_agent_binaries(config: &Config) -> Vec<&'static str> {
    let mut binaries = Vec::new();

    let agent_binary = |agent: Agent| -> &'static str {
        match agent {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    };

    let implementer_bin = agent_binary(config.implementer);
    binaries.push(implementer_bin);

    let reviewer_bin = agent_binary(config.reviewer);
    if reviewer_bin != implementer_bin {
        binaries.push(reviewer_bin);
    }

    binaries
}

fn check_agent_binaries(config: &Config) -> Result<(), AgentLoopError> {
    let binaries = required_agent_binaries(config);
    let missing: Vec<&str> = binaries
        .into_iter()
        .filter(|bin| !check_binary_available(bin))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let details = missing
        .iter()
        .map(|bin| {
            let hint = install_hint(bin);
            if hint.is_empty() {
                format!("  - '{bin}' not found in PATH")
            } else {
                format!("  - '{bin}' not found in PATH. {hint}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    Err(AgentLoopError::Config(format!(
        "Required agent binary not found:\n{details}"
    )))
}

fn repo_requires_git(config: &Config) -> bool {
    if config.auto_commit {
        return true;
    }

    // Use `git rev-parse` to detect git-backed projects even when running from
    // a subdirectory (where `.git` may not exist directly under project_dir).
    is_inside_git_work_tree(config)
}

fn is_inside_git_work_tree(config: &Config) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(&config.project_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn check_git_when_required(config: &Config) -> Result<(), AgentLoopError> {
    if !repo_requires_git(config) {
        return Ok(());
    }

    if check_binary_available("git") {
        return Ok(());
    }

    Err(AgentLoopError::Config(format!(
        "git is required but not found in PATH. {}",
        install_hint("git")
    )))
}

pub fn run_preflight(config: &Config) -> Result<(), AgentLoopError> {
    check_agent_binaries(config)?;
    check_git_when_required(config)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestProject, env_lock};

    #[test]
    fn check_binary_available_finds_existing_binary() {
        // `/bin/sh` is the POSIX-mandated shell and is reliably available on all
        // Unix-like systems (macOS, Linux).  Using it avoids portability issues
        // with `echo` which is a shell built-in on some platforms.
        assert!(check_binary_available("/bin/sh"));
    }

    #[test]
    fn check_binary_available_returns_false_for_missing_binary() {
        assert!(!check_binary_available(
            "agent_loop_nonexistent_binary_xyz_42"
        ));
    }

    #[test]
    fn required_agent_binaries_single_agent_returns_one() {
        let project = TestProject::builder("preflight_single").single_agent(true).build();
        let binaries = required_agent_binaries(&project.config);
        assert_eq!(binaries.len(), 1);
        assert_eq!(binaries[0], "claude");
    }

    #[test]
    fn required_agent_binaries_dual_agent_returns_two() {
        let project = TestProject::builder("preflight_dual").build();
        let binaries = required_agent_binaries(&project.config);
        assert_eq!(binaries.len(), 2);
        assert!(binaries.contains(&"claude"));
        assert!(binaries.contains(&"codex"));
    }

    #[test]
    fn check_agent_binaries_returns_error_with_hints_for_missing() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_missing_bin").build();

        // Override PATH to an empty directory so no binaries are found
        let _path_override = project.with_path_override();

        let err = check_agent_binaries(&project.config)
            .expect_err("should fail when agent binaries are missing");
        let msg = err.to_string();
        assert!(msg.contains("not found in PATH"), "error: {msg}");
        assert!(
            msg.contains("npm install -g"),
            "should contain install hint: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_agent_binaries_succeeds_with_fake_binaries() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_fake_bins").build();

        project.create_executable("claude", "#!/bin/sh\necho claude\n");
        project.create_executable("codex", "#!/bin/sh\necho codex\n");
        let _path_override = project.with_path_override();

        check_agent_binaries(&project.config).expect("should succeed with fake binaries in PATH");
    }

    #[test]
    fn repo_requires_git_true_when_auto_commit() {
        let project = TestProject::builder("preflight_git_ac")
            .auto_commit(true)
            .build();
        assert!(repo_requires_git(&project.config));
    }

    #[test]
    fn repo_requires_git_true_when_git_initialized() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_git_dir")
            .auto_commit(false)
            .with_git()
            .build();
        assert!(repo_requires_git(&project.config));
    }

    #[test]
    fn repo_requires_git_true_for_subdirectory_of_git_repo() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_git_subdir")
            .auto_commit(false)
            .with_git()
            .build();

        // Create a subdirectory and build a config pointing to it.
        let subdir = project.root.join("nested").join("subdir");
        std::fs::create_dir_all(&subdir).expect("subdir should be created");
        let sub_config = Config {
            project_dir: subdir,
            ..project.config.clone()
        };

        // Even though `.git` is not directly in `subdir`, the project is
        // still inside a git work tree and should be detected.
        assert!(repo_requires_git(&sub_config));
    }

    #[test]
    fn repo_requires_git_false_when_no_git_context() {
        let project = TestProject::builder("preflight_no_git")
            .auto_commit(false)
            .build();
        assert!(!repo_requires_git(&project.config));
    }

    #[test]
    fn check_git_when_required_skips_when_not_required() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_git_skip")
            .auto_commit(false)
            .build();

        // Even with broken PATH, should pass since git is not required
        let _path_override = project.with_path_override();
        check_git_when_required(&project.config)
            .expect("should skip git check when not required");
    }

    #[test]
    fn check_git_when_required_errors_when_missing_and_required() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_git_missing")
            .auto_commit(true)
            .build();

        let _path_override = project.with_path_override();

        let err = check_git_when_required(&project.config)
            .expect_err("should fail when git is required but missing");
        let msg = err.to_string();
        assert!(msg.contains("git is required"), "error: {msg}");
        assert!(
            msg.contains("git-scm.com"),
            "should contain install hint: {msg}"
        );
    }

    #[test]
    fn run_preflight_errors_on_missing_agent_binary() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_run_missing")
            .auto_commit(false)
            .build();

        let _path_override = project.with_path_override();

        let err = run_preflight(&project.config)
            .expect_err("should fail when agent binaries are missing");
        assert!(err.to_string().contains("not found in PATH"));
    }

    #[cfg(unix)]
    #[test]
    fn run_preflight_succeeds_with_all_binaries_present() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_run_ok")
            .auto_commit(false)
            .build();

        project.create_executable("claude", "#!/bin/sh\necho claude\n");
        project.create_executable("codex", "#!/bin/sh\necho codex\n");
        let _path_override = project.with_path_override();

        run_preflight(&project.config).expect("should succeed with all binaries present");
    }

    #[cfg(unix)]
    #[test]
    fn run_preflight_checks_git_after_agent_binaries() {
        let _guard = env_lock();
        let project = TestProject::builder("preflight_run_git")
            .auto_commit(true)
            .build();

        project.create_executable("claude", "#!/bin/sh\necho claude\n");
        project.create_executable("codex", "#!/bin/sh\necho codex\n");
        // No git in PATH
        let _path_override = project.with_path_override();

        let err = run_preflight(&project.config)
            .expect_err("should fail when git is required but missing");
        assert!(err.to_string().contains("git is required"));
    }

    #[test]
    fn install_hint_returns_appropriate_hints() {
        assert!(install_hint("claude").contains("npm install"));
        assert!(install_hint("codex").contains("npm install"));
        assert!(install_hint("git").contains("git-scm.com"));
        assert!(install_hint("unknown").is_empty());
    }
}
