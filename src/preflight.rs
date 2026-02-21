use std::process::Command;
use std::time::Duration;

use crate::config::Config;
use crate::error::AgentLoopError;

/// Maximum time to wait for a `--version` probe before assuming the binary is
/// unresponsive.  This prevents preflight from stalling startup indefinitely
/// if a CLI hangs.
const BINARY_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn install_hint(binary: &str) -> String {
    if binary == "git" {
        return "Install git: https://git-scm.com/downloads".to_string();
    }
    // Look up agent-specific install hint from registry
    if let Some(spec) = crate::agent_registry::get_agent_spec(binary) {
        return spec.install_hint.to_string();
    }
    String::new()
}

fn check_binary_available(binary: &str, probe_args: &[&str]) -> bool {
    let mut cmd = Command::new(binary);
    cmd.args(probe_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
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

fn required_agents(config: &Config) -> Vec<&crate::config::Agent> {
    let mut agents = vec![&config.implementer];

    if config.reviewer.spec().binary != config.implementer.spec().binary {
        agents.push(&config.reviewer);
    }

    agents
}

fn check_agent_binaries(config: &Config) -> Result<(), AgentLoopError> {
    let agents = required_agents(config);
    let mut missing = Vec::new();

    for agent in &agents {
        let spec = agent.spec();
        if !check_binary_available(spec.binary, spec.probe_args) {
            missing.push(spec);
        } else if spec.tier == crate::agent_registry::Tier::Experimental {
            eprintln!(
                "⚠ Agent '{}' is experimental — behavior may change between releases.",
                spec.name,
            );
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    let details = missing
        .iter()
        .map(|spec| {
            let hint = install_hint(spec.binary);
            if hint.is_empty() {
                format!("  - '{}' not found in PATH", spec.binary)
            } else {
                format!("  - '{}' not found in PATH. {hint}", spec.binary)
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

    // Detect git-backed projects by walking ancestor directories for a `.git`
    // entry (file or directory).  This intentionally avoids invoking the `git`
    // binary so that a missing `git` installation doesn't cause the check to
    // silently return `false`.
    has_git_ancestor(&config.project_dir)
}

fn has_git_ancestor(start: &std::path::Path) -> bool {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return true;
        }
        if !dir.pop() {
            return false;
        }
    }
}

fn check_git_when_required(config: &Config) -> Result<(), AgentLoopError> {
    if !repo_requires_git(config) {
        return Ok(());
    }

    if check_binary_available("git", &["--version"]) {
        return Ok(());
    }

    Err(AgentLoopError::Config(format!(
        "git is required but not found in PATH. {}",
        install_hint("git")
    )))
}

fn validate_session_resume_support(config: &Config) {
    if !config.claude_session_persistence {
        return;
    }
    for agent in required_agents(config) {
        if !agent.spec().supports_session_resume {
            eprintln!(
                "⚠ Agent '{}' does not support session resume. Session persistence disabled for this agent.",
                agent.name(),
            );
        }
    }
}

fn validate_model_support(config: &mut Config) {
    if config.implementer.model().is_some() && !config.implementer.spec().supports_model_flag {
        eprintln!(
            "⚠ Agent '{}' does not support --model. Clearing implementer_model.",
            config.implementer.name(),
        );
        config.implementer.clear_model();
    }
    if config.reviewer.model().is_some() && !config.reviewer.spec().supports_model_flag {
        eprintln!(
            "⚠ Agent '{}' does not support --model. Clearing reviewer_model.",
            config.reviewer.name(),
        );
        config.reviewer.clear_model();
    }
}

pub fn run_preflight(config: &mut Config) -> Result<(), AgentLoopError> {
    check_agent_binaries(config)?;
    validate_model_support(config);
    validate_session_resume_support(config);
    check_git_when_required(config)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestProject, env_lock};

    #[cfg(unix)]
    #[test]
    fn check_binary_available_finds_existing_binary() {
        // `/bin/sh` is the POSIX-mandated shell and is reliably available on all
        // Unix-like systems (macOS, Linux).  Using it avoids portability issues
        // with `echo` which is a shell built-in on some platforms.
        assert!(check_binary_available("/bin/sh", &["--version"]));
    }

    #[test]
    fn check_binary_available_returns_false_for_missing_binary() {
        assert!(!check_binary_available(
            "agent_loop_nonexistent_binary_xyz_42",
            &["--version"],
        ));
    }

    #[test]
    fn required_agents_single_agent_returns_one() {
        let project = TestProject::builder("preflight_single")
            .single_agent(true)
            .build();
        let agents = required_agents(&project.config);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].spec().binary, "claude");
    }

    #[test]
    fn required_agents_dual_agent_returns_two() {
        let project = TestProject::builder("preflight_dual").build();
        let agents = required_agents(&project.config);
        assert_eq!(agents.len(), 2);
        let binaries: Vec<&str> = agents.iter().map(|a| a.spec().binary).collect();
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
        check_git_when_required(&project.config).expect("should skip git check when not required");
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
        let mut project = TestProject::builder("preflight_run_missing")
            .auto_commit(false)
            .build();

        let _path_override = project.with_path_override();

        let err = run_preflight(&mut project.config)
            .expect_err("should fail when agent binaries are missing");
        assert!(err.to_string().contains("not found in PATH"));
    }

    #[cfg(unix)]
    #[test]
    fn run_preflight_succeeds_with_all_binaries_present() {
        let _guard = env_lock();
        let mut project = TestProject::builder("preflight_run_ok")
            .auto_commit(false)
            .build();

        project.create_executable("claude", "#!/bin/sh\necho claude\n");
        project.create_executable("codex", "#!/bin/sh\necho codex\n");
        let _path_override = project.with_path_override();

        run_preflight(&mut project.config).expect("should succeed with all binaries present");
    }

    #[cfg(unix)]
    #[test]
    fn run_preflight_checks_git_after_agent_binaries() {
        let _guard = env_lock();
        let mut project = TestProject::builder("preflight_run_git")
            .auto_commit(true)
            .build();

        project.create_executable("claude", "#!/bin/sh\necho claude\n");
        project.create_executable("codex", "#!/bin/sh\necho codex\n");
        // No git in PATH
        let _path_override = project.with_path_override();

        let err = run_preflight(&mut project.config)
            .expect_err("should fail when git is required but missing");
        assert!(err.to_string().contains("git is required"));
    }

    #[cfg(unix)]
    #[test]
    fn run_preflight_fails_for_git_backed_repo_without_git_binary() {
        // Regression: when the project is inside a git work tree but
        // `auto_commit = false`, preflight must still detect that git is
        // required (via the `.git` ancestor walk) and error when the `git`
        // binary is absent from PATH.
        let _guard = env_lock();
        let mut project = TestProject::builder("preflight_git_backed_no_bin")
            .auto_commit(false)
            .with_git()
            .build();

        project.create_executable("claude", "#!/bin/sh\necho claude\n");
        project.create_executable("codex", "#!/bin/sh\necho codex\n");
        // No git in PATH — only agent binaries
        let _path_override = project.with_path_override();

        let err = run_preflight(&mut project.config)
            .expect_err("should fail when git is required but binary is missing");
        let msg = err.to_string();
        assert!(
            msg.contains("git is required"),
            "error should mention git: {msg}"
        );
        assert!(
            msg.contains("git-scm.com"),
            "error should contain install hint: {msg}"
        );
    }

    #[test]
    fn has_git_ancestor_detects_dot_git_in_parent() {
        let project = TestProject::builder("preflight_git_ancestor")
            .auto_commit(false)
            .build();

        // Create a bare `.git` directory (no real git init needed).
        std::fs::create_dir_all(project.root.join(".git")).expect(".git dir should be created");

        let subdir = project.root.join("deep").join("nested");
        std::fs::create_dir_all(&subdir).expect("subdir should be created");

        assert!(has_git_ancestor(&project.root));
        assert!(has_git_ancestor(&subdir));
    }

    #[test]
    fn has_git_ancestor_false_without_dot_git() {
        let project = TestProject::builder("preflight_no_git_ancestor")
            .auto_commit(false)
            .build();
        assert!(!has_git_ancestor(&project.root));
    }

    #[test]
    fn install_hint_returns_appropriate_hints() {
        assert!(install_hint("claude").contains("npm install"));
        assert!(install_hint("codex").contains("npm install"));
        assert!(install_hint("git").contains("git-scm.com"));
        assert!(install_hint("unknown").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn check_agent_binaries_succeeds_for_experimental_agent() {
        // Verifies the Tier::Experimental branch in check_agent_binaries is
        // exercised: when an experimental agent binary is found, preflight
        // succeeds (and the warning is printed to stderr).
        let _guard = env_lock();
        let project = TestProject::builder("preflight_experimental")
            .single_agent(true)
            .build();

        // Replace the implementer with an experimental agent (gemini)
        let mut config = project.config.clone();
        config.implementer = crate::config::Agent::known("gemini");
        config.reviewer = crate::config::Agent::known("gemini");

        // Create a fake gemini binary
        project.create_executable("gemini", "#!/bin/sh\necho gemini\n");
        let _path_override = project.with_path_override();

        // Should succeed (not error) — the experimental warning is printed
        // to stderr but doesn't cause failure
        check_agent_binaries(&config).expect("experimental agent with valid binary should pass");

        // Verify gemini is indeed experimental
        assert_eq!(
            config.implementer.spec().tier,
            crate::agent_registry::Tier::Experimental
        );
    }

    #[test]
    fn validate_model_support_clears_unsupported_model() {
        let project = TestProject::builder("preflight_model_clear").build();
        let mut config = project.config.clone();

        // Set a model on an agent that doesn't support it (gemini)
        config.implementer = crate::config::Agent::known("gemini").with_model(Some("gpt-4".to_string()));
        assert!(config.implementer.model().is_some());

        validate_model_support(&mut config);

        // Model should have been cleared
        assert!(
            config.implementer.model().is_none(),
            "model should be cleared for agents that don't support --model"
        );
    }
}
