use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

use crate::error::AgentLoopError;

pub const DEFAULT_MAX_ROUNDS: u32 = 20;
pub const DEFAULT_PLANNING_MAX_ROUNDS: u32 = 3;
pub const DEFAULT_DECOMPOSITION_MAX_ROUNDS: u32 = 3;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
pub const DEFAULT_DIFF_MAX_LINES: u32 = 500;
#[allow(dead_code)]
pub const DEFAULT_CONTEXT_LINE_CAP: u32 = 200;
#[allow(dead_code)]
pub const DEFAULT_PLANNING_CONTEXT_EXCERPT_LINES: u32 = 100;
pub const DEFAULT_MAX_PARALLEL: u32 = 1;

const CONFIG_FILE_NAME: &str = ".agent-loop.toml";

/// Per-project configuration loaded from `.agent-loop.toml`.
///
/// All fields are optional — only explicitly set values override defaults.
/// Unknown keys are rejected (`deny_unknown_fields`) so typos surface early.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    /// Maximum implementation/review rounds before stopping.
    max_rounds: Option<u32>,
    /// Maximum planning consensus rounds.
    planning_max_rounds: Option<u32>,
    /// Maximum task decomposition review rounds.
    decomposition_max_rounds: Option<u32>,
    /// Idle timeout in seconds (resets on any agent output).
    timeout: Option<u64>,
    /// Which agent implements: `"claude"` or `"codex"`.
    implementer: Option<String>,
    /// Which agent reviews: `"claude"` or `"codex"`. Defaults to opposite of implementer.
    reviewer: Option<String>,
    /// Use the same agent for both roles.
    single_agent: Option<bool>,
    /// Auto-commit loop-owned changes after each round.
    auto_commit: Option<bool>,
    /// Run quality checks before review.
    auto_test: Option<bool>,
    /// Override auto-detected quality check command.
    auto_test_cmd: Option<String>,
    /// Plan and decompose only, no implementation.
    planning_only: Option<bool>,
    /// Maximum diff lines before truncation.
    diff_max_lines: Option<u32>,
    /// Maximum lines for project-context output.
    context_line_cap: Option<u32>,
    /// Maximum lines per file excerpt in planning context.
    planning_context_excerpt_lines: Option<u32>,
    /// Maximum parallel task execution (future-safe plumbing).
    max_parallel: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claude => write!(f, "claude"),
            Self::Codex => write!(f, "codex"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    SingleAgent,
    DualAgent,
}

impl fmt::Display for RunMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleAgent => write!(f, "single-agent"),
            Self::DualAgent => write!(f, "dual-agent"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub project_dir: PathBuf,
    pub state_dir: PathBuf,
    pub max_rounds: u32,
    pub planning_max_rounds: u32,
    pub decomposition_max_rounds: u32,
    pub timeout_seconds: u64,
    pub implementer: Agent,
    pub reviewer: Agent,
    pub single_agent: bool,
    pub run_mode: RunMode,
    pub auto_commit: bool,
    pub auto_test: bool,
    pub auto_test_cmd: Option<String>,
    pub planning_only: bool,
    pub diff_max_lines: Option<u32>,
    pub context_line_cap: Option<u32>,
    pub planning_context_excerpt_lines: Option<u32>,
    pub max_parallel: u32,
    pub verbose: bool,
}

impl Config {
    pub fn effective_diff_max_lines(&self) -> u32 {
        self.diff_max_lines.unwrap_or(DEFAULT_DIFF_MAX_LINES)
    }

    pub fn effective_context_line_cap(&self) -> u32 {
        self.context_line_cap.unwrap_or(DEFAULT_CONTEXT_LINE_CAP)
    }

    pub fn effective_planning_context_excerpt_lines(&self) -> u32 {
        self.planning_context_excerpt_lines
            .unwrap_or(DEFAULT_PLANNING_CONTEXT_EXCERPT_LINES)
    }

    pub fn from_cli(
        project_dir: PathBuf,
        single_agent_flag: bool,
        planning_only_flag: bool,
        verbose_flag: bool,
    ) -> Result<Self, AgentLoopError> {
        let file = load_file_config(&project_dir)?;

        // --- single_agent: CLI > env > TOML > default ---
        let single_agent = if single_agent_flag {
            true
        } else {
            env_bool("SINGLE_AGENT")
                .or(file.single_agent)
                .unwrap_or(false)
        };

        // --- implementer: env > TOML > default ---
        let implementer = env_agent("IMPLEMENTER")
            .or_else(|| parse_agent(file.implementer.as_deref()))
            .unwrap_or(Agent::Claude);

        // --- reviewer: env > TOML > derived default ---
        let reviewer = if single_agent {
            implementer
        } else {
            env_agent("REVIEWER")
                .or_else(|| parse_agent(file.reviewer.as_deref()))
                .unwrap_or_else(|| opposite_agent(implementer))
        };

        // --- numeric: env > TOML > default ---
        let max_rounds = parse_env("MAX_ROUNDS")
            .or(file.max_rounds)
            .unwrap_or(DEFAULT_MAX_ROUNDS);
        let planning_max_rounds = parse_env("PLANNING_MAX_ROUNDS")
            .or(file.planning_max_rounds)
            .unwrap_or(DEFAULT_PLANNING_MAX_ROUNDS);
        let decomposition_max_rounds = parse_env("DECOMPOSITION_MAX_ROUNDS")
            .or(file.decomposition_max_rounds)
            .unwrap_or(DEFAULT_DECOMPOSITION_MAX_ROUNDS);
        let timeout_seconds = parse_env("TIMEOUT")
            .or(file.timeout)
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS);

        // --- auto_commit: env > TOML > default (true) ---
        let auto_commit = env_bool_auto_commit().or(file.auto_commit).unwrap_or(true);

        // --- auto_test: env > TOML > default (false) ---
        let auto_test = env_bool("AUTO_TEST").or(file.auto_test).unwrap_or(false);

        // --- auto_test_cmd: env > TOML > None ---
        let auto_test_cmd = env_trimmed_string("AUTO_TEST_CMD").or_else(|| {
            file.auto_test_cmd
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        });

        // --- diff/context limits: env > TOML > None (defaults via effective helpers) ---
        let diff_max_lines = parse_env("DIFF_MAX_LINES").or(file.diff_max_lines);
        let context_line_cap = parse_env("CONTEXT_LINE_CAP").or(file.context_line_cap);
        let planning_context_excerpt_lines =
            parse_env("PLANNING_CONTEXT_EXCERPT_LINES").or(file.planning_context_excerpt_lines);

        // --- max_parallel: TOML > default ---
        let max_parallel = file.max_parallel.unwrap_or(DEFAULT_MAX_PARALLEL);

        // --- verbose: CLI flag > env > default (false) ---
        let verbose = verbose_flag || env_bool("VERBOSE").unwrap_or(false);

        // --- planning_only: CLI > TOML > default ---
        let planning_only = if planning_only_flag {
            true
        } else {
            file.planning_only.unwrap_or(false)
        };

        let config = Self {
            state_dir: project_dir.join(".agent-loop").join("state"),
            run_mode: resolve_run_mode(single_agent),
            project_dir,
            max_rounds,
            planning_max_rounds,
            decomposition_max_rounds,
            timeout_seconds,
            implementer,
            reviewer,
            single_agent,
            auto_commit,
            auto_test,
            auto_test_cmd,
            planning_only,
            diff_max_lines,
            context_line_cap,
            planning_context_excerpt_lines,
            max_parallel,
            verbose,
        };

        validate_config_bounds(&config)?;
        emit_config_warnings(&config);

        Ok(config)
    }
}

/// Load `.agent-loop.toml` from `project_dir`. Returns default on missing file.
/// Returns an error on I/O failures (other than not-found) or parse failures.
fn load_file_config(project_dir: &Path) -> Result<FileConfig, AgentLoopError> {
    let path = project_dir.join(CONFIG_FILE_NAME);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(FileConfig::default()),
        Err(err) => {
            return Err(AgentLoopError::Config(format!(
                "failed to read {}: {err}",
                path.display()
            )));
        }
    };

    let config = toml::from_str::<FileConfig>(&content).map_err(|err| {
        AgentLoopError::Config(format!("failed to parse {}: {err}", path.display()))
    })?;
    Ok(config)
}

pub fn is_truthy(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn is_falsy(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Parse a boolean env var as a tri-state: `Some(true)`, `Some(false)`, or `None` (unset).
fn env_bool(key: &str) -> Option<bool> {
    let value = env::var(key).ok()?;
    if is_truthy(Some(&value)) {
        Some(true)
    } else if is_falsy(&value) {
        Some(false)
    } else {
        None
    }
}

/// `AUTO_COMMIT` uses a special convention: `"0"` means false, anything else means true.
fn env_bool_auto_commit() -> Option<bool> {
    let value = env::var("AUTO_COMMIT").ok()?;
    Some(value != "0")
}

fn env_trimmed_string(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_agent(value: Option<&str>) -> Option<Agent> {
    match value {
        Some("codex") => Some(Agent::Codex),
        Some("claude") => Some(Agent::Claude),
        _ => None,
    }
}

fn env_agent(key: &str) -> Option<Agent> {
    parse_agent(env::var(key).ok().as_deref())
}

fn opposite_agent(agent: Agent) -> Agent {
    match agent {
        Agent::Claude => Agent::Codex,
        Agent::Codex => Agent::Claude,
    }
}

#[cfg(test)]
pub fn resolve_implementer(env_value: Option<&str>) -> Agent {
    if env_value == Some("codex") {
        Agent::Codex
    } else {
        Agent::Claude
    }
}

#[cfg(test)]
pub fn resolve_reviewer(implementer: Agent, single_agent: bool) -> Agent {
    if single_agent {
        return implementer;
    }

    match implementer {
        Agent::Claude => Agent::Codex,
        Agent::Codex => Agent::Claude,
    }
}

pub fn resolve_run_mode(single_agent: bool) -> RunMode {
    if single_agent {
        RunMode::SingleAgent
    } else {
        RunMode::DualAgent
    }
}

fn parse_env<T: FromStr>(key: &str) -> Option<T> {
    env::var(key).ok().and_then(|value| value.parse::<T>().ok())
}

fn validate_config_bounds(config: &Config) -> Result<(), AgentLoopError> {
    if config.max_rounds == 0 && !config.planning_only {
        return Err(AgentLoopError::Config(
            "max_rounds must be > 0 in implementation mode. \
             Set MAX_ROUNDS or max_rounds in .agent-loop.toml to a positive value, \
             or use --planning-only."
                .to_string(),
        ));
    }

    if config.planning_max_rounds == 0 {
        return Err(AgentLoopError::Config(
            "planning_max_rounds must be > 0. \
             Set PLANNING_MAX_ROUNDS or planning_max_rounds in .agent-loop.toml to a positive value."
                .to_string(),
        ));
    }

    if config.planning_only && config.decomposition_max_rounds == 0 {
        return Err(AgentLoopError::Config(
            "decomposition_max_rounds must be > 0 in planning-only mode. \
             Set DECOMPOSITION_MAX_ROUNDS or decomposition_max_rounds in .agent-loop.toml to a positive value."
                .to_string(),
        ));
    }

    if config.timeout_seconds == 0 {
        return Err(AgentLoopError::Config(
            "timeout must be > 0. \
             Set TIMEOUT or timeout in .agent-loop.toml to a positive value."
                .to_string(),
        ));
    }

    if config.max_parallel == 0 {
        return Err(AgentLoopError::Config(
            "max_parallel must be >= 1. \
             Set max_parallel in .agent-loop.toml to a positive value."
                .to_string(),
        ));
    }

    Ok(())
}

fn emit_config_warnings(config: &Config) {
    if config.planning_only && config.max_rounds == 0 {
        eprintln!(
            "Warning: max_rounds=0 is ignored in planning-only mode (only planning_max_rounds and decomposition_max_rounds apply)."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{create_temp_project_root, env_lock};

    fn clear_env() {
        for key in [
            "SINGLE_AGENT",
            "IMPLEMENTER",
            "REVIEWER",
            "AUTO_COMMIT",
            "AUTO_TEST",
            "AUTO_TEST_CMD",
            "MAX_ROUNDS",
            "PLANNING_MAX_ROUNDS",
            "DECOMPOSITION_MAX_ROUNDS",
            "TIMEOUT",
            "DIFF_MAX_LINES",
            "CONTEXT_LINE_CAP",
            "PLANNING_CONTEXT_EXCERPT_LINES",
            "VERBOSE",
        ] {
            // SAFETY: tests serialize env mutation with a process-wide mutex.
            unsafe {
                env::remove_var(key);
            }
        }
    }

    fn set_env(key: &str, value: &str) {
        // SAFETY: tests serialize env mutation with a process-wide mutex.
        unsafe {
            env::set_var(key, value);
        }
    }

    fn write_toml(project_dir: &Path, content: &str) {
        std::fs::write(project_dir.join(CONFIG_FILE_NAME), content)
            .expect("TOML file should be written");
    }

    // -----------------------------------------------------------------------
    // is_truthy / is_falsy
    // -----------------------------------------------------------------------

    #[test]
    fn is_truthy_handles_expected_values() {
        for value in ["1", "true", "TRUE", "yes", "On"] {
            assert!(is_truthy(Some(value)));
        }
    }

    #[test]
    fn is_truthy_rejects_non_truthy_values() {
        for value in ["", "0", "false", "off", "random"] {
            assert!(!is_truthy(Some(value)));
        }
        assert!(!is_truthy(None));
    }

    #[test]
    fn is_falsy_handles_expected_values() {
        for value in ["0", "false", "FALSE", "no", "off", "Off"] {
            assert!(is_falsy(value), "{value} should be falsy");
        }
    }

    #[test]
    fn is_falsy_rejects_non_falsy_values() {
        for value in ["", "1", "true", "yes", "random"] {
            assert!(!is_falsy(value), "{value} should not be falsy");
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_implementer_defaults_to_claude() {
        assert_eq!(resolve_implementer(None), Agent::Claude);
        assert_eq!(resolve_implementer(Some("claude")), Agent::Claude);
        assert_eq!(resolve_implementer(Some("CODEX")), Agent::Claude);
    }

    #[test]
    fn resolve_implementer_uses_codex_only_on_exact_match() {
        assert_eq!(resolve_implementer(Some("codex")), Agent::Codex);
    }

    #[test]
    fn resolve_reviewer_matches_single_agent_mode() {
        assert_eq!(resolve_reviewer(Agent::Claude, true), Agent::Claude);
        assert_eq!(resolve_reviewer(Agent::Codex, true), Agent::Codex);
    }

    #[test]
    fn resolve_reviewer_uses_opposite_agent_in_dual_mode() {
        assert_eq!(resolve_reviewer(Agent::Claude, false), Agent::Codex);
        assert_eq!(resolve_reviewer(Agent::Codex, false), Agent::Claude);
    }

    #[test]
    fn resolve_run_mode_maps_from_single_agent_flag() {
        assert_eq!(resolve_run_mode(true), RunMode::SingleAgent);
        assert_eq!(resolve_run_mode(false), RunMode::DualAgent);
    }

    // -----------------------------------------------------------------------
    // FileConfig loading
    // -----------------------------------------------------------------------

    #[test]
    fn load_file_config_missing_file_returns_default() {
        let dir = create_temp_project_root("toml_missing");
        let config = load_file_config(&dir).expect("missing file should return Ok(default)");
        assert!(config.max_rounds.is_none());
        assert!(config.implementer.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_valid_full_file() {
        let dir = create_temp_project_root("toml_full");
        write_toml(
            &dir,
            r#"
max_rounds = 10
planning_max_rounds = 5
decomposition_max_rounds = 4
timeout = 300
implementer = "codex"
reviewer = "claude"
single_agent = true
auto_commit = false
auto_test = true
auto_test_cmd = "cargo test"
planning_only = true
diff_max_lines = 250
context_line_cap = 150
planning_context_excerpt_lines = 80
max_parallel = 4
"#,
        );
        let config = load_file_config(&dir).expect("valid full file should parse");
        assert_eq!(config.max_rounds, Some(10));
        assert_eq!(config.planning_max_rounds, Some(5));
        assert_eq!(config.decomposition_max_rounds, Some(4));
        assert_eq!(config.timeout, Some(300));
        assert_eq!(config.implementer.as_deref(), Some("codex"));
        assert_eq!(config.reviewer.as_deref(), Some("claude"));
        assert_eq!(config.single_agent, Some(true));
        assert_eq!(config.auto_commit, Some(false));
        assert_eq!(config.auto_test, Some(true));
        assert_eq!(config.auto_test_cmd.as_deref(), Some("cargo test"));
        assert_eq!(config.planning_only, Some(true));
        assert_eq!(config.diff_max_lines, Some(250));
        assert_eq!(config.context_line_cap, Some(150));
        assert_eq!(config.planning_context_excerpt_lines, Some(80));
        assert_eq!(config.max_parallel, Some(4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_partial_file() {
        let dir = create_temp_project_root("toml_partial");
        write_toml(&dir, "max_rounds = 7\n");
        let config = load_file_config(&dir).expect("partial file should parse");
        assert_eq!(config.max_rounds, Some(7));
        assert!(config.implementer.is_none());
        assert!(config.auto_test.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_malformed_toml_returns_error() {
        let dir = create_temp_project_root("toml_malformed");
        write_toml(&dir, "this is not valid toml {{{\n");
        let err = load_file_config(&dir).expect_err("malformed TOML should fail");
        assert!(matches!(err, AgentLoopError::Config(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_unknown_keys_returns_error() {
        let dir = create_temp_project_root("toml_unknown");
        write_toml(&dir, "unknown_key = 42\n");
        let err = load_file_config(&dir).expect_err("unknown keys should fail");
        assert!(matches!(err, AgentLoopError::Config(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_wrong_types_returns_error() {
        let dir = create_temp_project_root("toml_wrong_type");
        write_toml(&dir, "max_rounds = \"not a number\"\n");
        let err = load_file_config(&dir).expect_err("wrong type should fail");
        assert!(matches!(err, AgentLoopError::Config(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // from_cli — defaults (no TOML, no env)
    // -----------------------------------------------------------------------

    #[test]
    fn from_cli_builds_defaults() {
        let _guard = env_lock();
        clear_env();

        let project_dir = create_temp_project_root("cfg_defaults");
        let config = Config::from_cli(project_dir.clone(), false, false, false)
            .expect("from_cli should succeed");

        assert_eq!(config.project_dir, project_dir);
        assert_eq!(config.state_dir, project_dir.join(".agent-loop/state"));
        assert_eq!(config.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(config.planning_max_rounds, DEFAULT_PLANNING_MAX_ROUNDS);
        assert_eq!(
            config.decomposition_max_rounds,
            DEFAULT_DECOMPOSITION_MAX_ROUNDS
        );
        assert_eq!(config.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(config.implementer, Agent::Claude);
        assert_eq!(config.reviewer, Agent::Codex);
        assert!(!config.single_agent);
        assert_eq!(config.run_mode, RunMode::DualAgent);
        assert!(config.auto_commit);
        assert!(!config.auto_test);
        assert_eq!(config.auto_test_cmd, None);
        assert!(!config.planning_only);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    // -----------------------------------------------------------------------
    // from_cli — env overrides (no TOML)
    // -----------------------------------------------------------------------

    #[test]
    fn from_cli_applies_cli_and_env_overrides() {
        let _guard = env_lock();
        clear_env();
        set_env("SINGLE_AGENT", "true");
        set_env("IMPLEMENTER", "codex");
        set_env("AUTO_COMMIT", "0");
        set_env("MAX_ROUNDS", "42");
        set_env("PLANNING_MAX_ROUNDS", "7");
        set_env("DECOMPOSITION_MAX_ROUNDS", "8");
        set_env("TIMEOUT", "900");

        let project_dir = create_temp_project_root("cfg_env_overrides");
        let config = Config::from_cli(project_dir.clone(), false, true, false)
            .expect("from_cli should succeed");

        assert_eq!(config.max_rounds, 42);
        assert_eq!(config.planning_max_rounds, 7);
        assert_eq!(config.decomposition_max_rounds, 8);
        assert_eq!(config.timeout_seconds, 900);
        assert_eq!(config.implementer, Agent::Codex);
        assert_eq!(config.reviewer, Agent::Codex);
        assert!(config.single_agent);
        assert_eq!(config.run_mode, RunMode::SingleAgent);
        assert!(!config.auto_commit);
        assert!(config.planning_only);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_uses_safe_defaults_for_invalid_numeric_env_values() {
        let _guard = env_lock();
        clear_env();
        set_env("MAX_ROUNDS", "not-a-number");
        set_env("PLANNING_MAX_ROUNDS", "invalid");
        set_env("DECOMPOSITION_MAX_ROUNDS", "-1");
        set_env("TIMEOUT", "-1");

        let project_dir = create_temp_project_root("cfg_invalid_env");
        let config = Config::from_cli(project_dir.clone(), false, false, false)
            .expect("from_cli should succeed");

        assert_eq!(config.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(config.planning_max_rounds, DEFAULT_PLANNING_MAX_ROUNDS);
        assert_eq!(
            config.decomposition_max_rounds,
            DEFAULT_DECOMPOSITION_MAX_ROUNDS
        );
        assert_eq!(config.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_rejects_zero_max_rounds_in_implementation_mode() {
        let _guard = env_lock();
        clear_env();
        set_env("MAX_ROUNDS", "0");

        let project_dir = create_temp_project_root("cfg_zero_rounds");
        let err = Config::from_cli(project_dir.clone(), false, false, false)
            .expect_err("max_rounds=0 in implementation mode should fail");
        assert!(err.to_string().contains("max_rounds must be > 0"));
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_allows_zero_max_rounds_in_planning_only_mode() {
        let _guard = env_lock();
        clear_env();
        set_env("MAX_ROUNDS", "0");

        let project_dir = create_temp_project_root("cfg_zero_rounds_planning");
        let config = Config::from_cli(project_dir.clone(), false, true, false)
            .expect("max_rounds=0 in planning-only mode should succeed");
        assert_eq!(config.max_rounds, 0);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_auto_test_defaults_to_off() {
        let _guard = env_lock();
        clear_env();

        let project_dir = create_temp_project_root("cfg_auto_test_off");
        let config = Config::from_cli(project_dir.clone(), false, false, false)
            .expect("from_cli should succeed");
        assert!(!config.auto_test);
        assert_eq!(config.auto_test_cmd, None);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_auto_test_enabled_by_truthy_env() {
        let _guard = env_lock();
        clear_env();
        set_env("AUTO_TEST", "1");

        let project_dir = create_temp_project_root("cfg_auto_test_on");
        let config = Config::from_cli(project_dir.clone(), false, false, false)
            .expect("from_cli should succeed");
        assert!(config.auto_test);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_auto_test_cmd_parses_and_ignores_empty() {
        let _guard = env_lock();
        clear_env();
        set_env("AUTO_TEST", "1");
        set_env("AUTO_TEST_CMD", "make test");

        let project_dir = create_temp_project_root("cfg_auto_test_cmd");
        let config = Config::from_cli(project_dir.clone(), false, false, false)
            .expect("from_cli should succeed");
        assert_eq!(config.auto_test_cmd, Some("make test".to_string()));

        set_env("AUTO_TEST_CMD", "   ");
        let config2 = Config::from_cli(project_dir.clone(), false, false, false)
            .expect("from_cli should succeed");
        assert_eq!(config2.auto_test_cmd, None);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_cli_single_agent_flag_takes_precedence() {
        let _guard = env_lock();
        clear_env();
        set_env("SINGLE_AGENT", "0");
        set_env("IMPLEMENTER", "codex");

        let project_dir = create_temp_project_root("cfg_cli_flag");
        let config = Config::from_cli(project_dir.clone(), true, false, false)
            .expect("from_cli should succeed");

        assert!(config.single_agent);
        assert_eq!(config.reviewer, Agent::Codex);
        assert_eq!(config.run_mode, RunMode::SingleAgent);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    // -----------------------------------------------------------------------
    // from_cli — TOML overrides defaults
    // -----------------------------------------------------------------------

    #[test]
    fn toml_overrides_defaults() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_toml_overrides");
        write_toml(
            &dir,
            r#"
max_rounds = 10
planning_max_rounds = 5
decomposition_max_rounds = 4
timeout = 300
implementer = "codex"
reviewer = "codex"
single_agent = true
auto_commit = false
auto_test = true
auto_test_cmd = "cargo test"
planning_only = true
"#,
        );

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");

        assert_eq!(config.max_rounds, 10);
        assert_eq!(config.planning_max_rounds, 5);
        assert_eq!(config.decomposition_max_rounds, 4);
        assert_eq!(config.timeout_seconds, 300);
        assert_eq!(config.implementer, Agent::Codex);
        assert_eq!(config.reviewer, Agent::Codex);
        assert!(config.single_agent);
        assert!(!config.auto_commit);
        assert!(config.auto_test);
        assert_eq!(config.auto_test_cmd, Some("cargo test".to_string()));
        assert!(config.planning_only);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // from_cli — env overrides TOML
    // -----------------------------------------------------------------------

    #[test]
    fn env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_env_over_toml");
        write_toml(
            &dir,
            r#"
max_rounds = 10
timeout = 300
implementer = "codex"
single_agent = true
auto_commit = false
auto_test = false
auto_test_cmd = "make test"
"#,
        );

        set_env("MAX_ROUNDS", "50");
        set_env("TIMEOUT", "1200");
        set_env("IMPLEMENTER", "claude");
        set_env("SINGLE_AGENT", "false");
        set_env("AUTO_COMMIT", "1");
        set_env("AUTO_TEST", "1");
        set_env("AUTO_TEST_CMD", "npm test");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");

        assert_eq!(config.max_rounds, 50);
        assert_eq!(config.timeout_seconds, 1200);
        assert_eq!(config.implementer, Agent::Claude);
        assert!(!config.single_agent);
        assert!(config.auto_commit);
        assert!(config.auto_test);
        assert_eq!(config.auto_test_cmd, Some("npm test".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // from_cli — CLI overrides env and TOML
    // -----------------------------------------------------------------------

    #[test]
    fn cli_overrides_env_and_toml_for_single_agent() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_cli_over_all");
        write_toml(&dir, "single_agent = false\n");
        set_env("SINGLE_AGENT", "false");

        let config =
            Config::from_cli(dir.clone(), true, false, false).expect("from_cli should succeed");
        assert!(config.single_agent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_overrides_toml_for_planning_only() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_cli_planning");
        write_toml(&dir, "planning_only = false\n");

        let config =
            Config::from_cli(dir.clone(), false, true, false).expect("from_cli should succeed");
        assert!(config.planning_only);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Boolean false-override tests (critical for correct precedence)
    // -----------------------------------------------------------------------

    #[test]
    fn env_false_overrides_toml_true_for_single_agent() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_env_false_sa");
        write_toml(&dir, "single_agent = true\n");
        set_env("SINGLE_AGENT", "0");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert!(!config.single_agent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_false_overrides_toml_true_for_auto_test() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_env_false_at");
        write_toml(&dir, "auto_test = true\n");
        set_env("AUTO_TEST", "0");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert!(!config.auto_test);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_false_overrides_toml_true_for_auto_commit() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_env_false_ac");
        write_toml(&dir, "auto_commit = true\n");
        set_env("AUTO_COMMIT", "0");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert!(!config.auto_commit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Reviewer tests
    // -----------------------------------------------------------------------

    #[test]
    fn reviewer_from_toml_in_dual_agent_mode() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_reviewer_toml");
        write_toml(&dir, "reviewer = \"claude\"\nimplementer = \"claude\"\n");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        // Both claude -> explicit reviewer override honored
        assert_eq!(config.implementer, Agent::Claude);
        assert_eq!(config.reviewer, Agent::Claude);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reviewer_from_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_reviewer_env");
        write_toml(&dir, "reviewer = \"claude\"\n");
        set_env("REVIEWER", "codex");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert_eq!(config.reviewer, Agent::Codex);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_agent_forces_reviewer_equals_implementer() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_reviewer_sa");
        write_toml(
            &dir,
            "implementer = \"codex\"\nreviewer = \"claude\"\nsingle_agent = true\n",
        );

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert!(config.single_agent);
        assert_eq!(config.implementer, Agent::Codex);
        assert_eq!(config.reviewer, Agent::Codex);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_agent_cli_forces_reviewer_even_with_env_reviewer() {
        let _guard = env_lock();
        clear_env();
        set_env("REVIEWER", "codex");
        set_env("IMPLEMENTER", "claude");

        let dir = create_temp_project_root("cfg_reviewer_sa_env");
        let config =
            Config::from_cli(dir.clone(), true, false, false).expect("from_cli should succeed");
        assert_eq!(config.reviewer, Agent::Claude);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Regression: env-only behavior still works
    // -----------------------------------------------------------------------

    #[test]
    fn env_only_auto_commit_defaults_true_without_env_var() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_ac_default");
        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert!(config.auto_commit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_only_auto_commit_non_zero_is_true() {
        let _guard = env_lock();
        clear_env();
        set_env("AUTO_COMMIT", "anything");

        let dir = create_temp_project_root("cfg_ac_nonzero");
        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert!(config.auto_commit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Config bounds validation
    // -----------------------------------------------------------------------

    #[test]
    fn validate_rejects_zero_planning_max_rounds() {
        let _guard = env_lock();
        clear_env();
        set_env("PLANNING_MAX_ROUNDS", "0");

        let dir = create_temp_project_root("cfg_zero_planning");
        let err = Config::from_cli(dir.clone(), false, false, false)
            .expect_err("planning_max_rounds=0 should fail");
        assert!(err.to_string().contains("planning_max_rounds must be > 0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_zero_decomposition_max_rounds_in_planning_only() {
        let _guard = env_lock();
        clear_env();
        set_env("DECOMPOSITION_MAX_ROUNDS", "0");

        let dir = create_temp_project_root("cfg_zero_decomp_plan");
        let err = Config::from_cli(dir.clone(), false, true, false)
            .expect_err("decomposition_max_rounds=0 in planning-only should fail");
        assert!(
            err.to_string()
                .contains("decomposition_max_rounds must be > 0")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_zero_timeout() {
        let _guard = env_lock();
        clear_env();
        set_env("TIMEOUT", "0");

        let dir = create_temp_project_root("cfg_zero_timeout");
        let err =
            Config::from_cli(dir.clone(), false, false, false).expect_err("timeout=0 should fail");
        assert!(err.to_string().contains("timeout must be > 0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_passes_for_valid_defaults() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_valid_defaults");
        let config = Config::from_cli(dir.clone(), false, false, false)
            .expect("default config should be valid");
        assert!(config.max_rounds > 0);
        assert!(config.planning_max_rounds > 0);
        assert!(config.timeout_seconds > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // New fields: diff_max_lines, context_line_cap, planning_context_excerpt_lines
    // -----------------------------------------------------------------------

    #[test]
    fn load_file_config_parses_only_new_limit_fields() {
        let dir = create_temp_project_root("toml_new_fields_only");
        write_toml(
            &dir,
            r#"
diff_max_lines = 300
context_line_cap = 150
planning_context_excerpt_lines = 75
"#,
        );
        let config = load_file_config(&dir).expect("should parse new-only fields");
        assert_eq!(config.diff_max_lines, Some(300));
        assert_eq!(config.context_line_cap, Some(150));
        assert_eq!(config.planning_context_excerpt_lines, Some(75));
        // Other fields remain None
        assert!(config.max_rounds.is_none());
        assert!(config.implementer.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_new_fields_default_to_none() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_new_fields_default");
        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert_eq!(config.diff_max_lines, None);
        assert_eq!(config.context_line_cap, None);
        assert_eq!(config.planning_context_excerpt_lines, None);
        // Effective helpers still return defaults
        assert_eq!(config.effective_diff_max_lines(), DEFAULT_DIFF_MAX_LINES);
        assert_eq!(
            config.effective_context_line_cap(),
            DEFAULT_CONTEXT_LINE_CAP
        );
        assert_eq!(
            config.effective_planning_context_excerpt_lines(),
            DEFAULT_PLANNING_CONTEXT_EXCERPT_LINES
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_toml_sets_new_fields() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_toml_new_fields");
        write_toml(
            &dir,
            "diff_max_lines = 250\ncontext_line_cap = 120\nplanning_context_excerpt_lines = 60\n",
        );
        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert_eq!(config.diff_max_lines, Some(250));
        assert_eq!(config.context_line_cap, Some(120));
        assert_eq!(config.planning_context_excerpt_lines, Some(60));
        assert_eq!(config.effective_diff_max_lines(), 250);
        assert_eq!(config.effective_context_line_cap(), 120);
        assert_eq!(config.effective_planning_context_excerpt_lines(), 60);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_overrides_toml_for_diff_max_lines() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_env_diff");
        write_toml(&dir, "diff_max_lines = 250\n");
        set_env("DIFF_MAX_LINES", "999");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert_eq!(config.diff_max_lines, Some(999));
        assert_eq!(config.effective_diff_max_lines(), 999);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_overrides_toml_for_context_line_cap() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_env_ctx_cap");
        write_toml(&dir, "context_line_cap = 120\n");
        set_env("CONTEXT_LINE_CAP", "300");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert_eq!(config.context_line_cap, Some(300));
        assert_eq!(config.effective_context_line_cap(), 300);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_overrides_toml_for_planning_context_excerpt_lines() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_env_excerpt");
        write_toml(&dir, "planning_context_excerpt_lines = 60\n");
        set_env("PLANNING_CONTEXT_EXCERPT_LINES", "200");

        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert_eq!(config.planning_context_excerpt_lines, Some(200));
        assert_eq!(config.effective_planning_context_excerpt_lines(), 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Verbose flag
    // -----------------------------------------------------------------------

    #[test]
    fn from_cli_verbose_defaults_to_false() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_verbose_default");
        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert!(!config.verbose);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_verbose_flag_enables_verbose() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_verbose_flag");
        let config =
            Config::from_cli(dir.clone(), false, false, true).expect("from_cli should succeed");
        assert!(config.verbose);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_verbose_env_enables_verbose_when_flag_is_absent() {
        let _guard = env_lock();
        clear_env();
        set_env("VERBOSE", "1");

        let dir = create_temp_project_root("cfg_verbose_env");
        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert!(config.verbose);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_verbose_flag_overrides_falsy_env() {
        let _guard = env_lock();
        clear_env();
        set_env("VERBOSE", "0");

        let dir = create_temp_project_root("cfg_verbose_flag_over_env");
        let config =
            Config::from_cli(dir.clone(), false, false, true).expect("from_cli should succeed");
        assert!(config.verbose);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // max_parallel config field
    // -----------------------------------------------------------------------

    #[test]
    fn load_file_config_parses_max_parallel() {
        let dir = create_temp_project_root("toml_max_parallel");
        write_toml(&dir, "max_parallel = 4\n");
        let config = load_file_config(&dir).expect("max_parallel should parse");
        assert_eq!(config.max_parallel, Some(4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_max_parallel_defaults_to_1() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_max_parallel_default");
        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert_eq!(config.max_parallel, DEFAULT_MAX_PARALLEL);
        assert_eq!(config.max_parallel, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_toml_overrides_max_parallel() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_toml_max_parallel");
        write_toml(&dir, "max_parallel = 4\n");
        let config =
            Config::from_cli(dir.clone(), false, false, false).expect("from_cli should succeed");
        assert_eq!(config.max_parallel, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_zero_max_parallel_in_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_zero_max_parallel");
        write_toml(&dir, "max_parallel = 0\n");
        let err = Config::from_cli(dir.clone(), false, false, false)
            .expect_err("max_parallel=0 should fail");
        assert!(err.to_string().contains("max_parallel must be >= 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
