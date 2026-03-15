use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;

use crate::db::Db;
use crate::error::AgentLoopError;

/// Default review/implementation round limit. `0` = unlimited (no cap).
pub const DEFAULT_REVIEW_MAX_ROUNDS: u32 = 0;
/// Default planning consensus round limit. `0` = unlimited (no cap).
pub const DEFAULT_PLANNING_MAX_ROUNDS: u32 = 0;
/// Default task-decomposition round limit. `0` = unlimited (no cap).
pub const DEFAULT_DECOMPOSITION_MAX_ROUNDS: u32 = 0;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
pub const DEFAULT_DIFF_MAX_LINES: u32 = 500;
#[allow(dead_code)]
pub const DEFAULT_CONTEXT_LINE_CAP: u32 = 0;
#[allow(dead_code)]
pub const DEFAULT_PLANNING_CONTEXT_EXCERPT_LINES: u32 = 0;
pub const DEFAULT_MAX_PARALLEL: u32 = 1;
pub const DEFAULT_DECISIONS_MAX_LINES: u32 = 50;
pub const DEFAULT_CLAUDE_ALLOWED_TOOLS: &str = "Bash,Read,Edit,Write,Grep,Glob,WebFetch";
pub const DEFAULT_REVIEWER_ALLOWED_TOOLS: &str = "Read,Grep,Glob,WebFetch";
pub const DEFAULT_STUCK_NO_DIFF_ROUNDS: u32 = 3;
pub const DEFAULT_STUCK_THRESHOLD_MINUTES: u64 = 10;
/// Rounds a planning finding stays open before triggering a role swap
/// (reviewer fixes, implementer reviews). `0` = disabled.
pub const DEFAULT_PLANNING_ROLE_SWAP_AFTER: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckAction {
    Abort,
    Warn,
    Retry,
}

impl FromStr for StuckAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "abort" => Ok(Self::Abort),
            "warn" => Ok(Self::Warn),
            "retry" => Ok(Self::Retry),
            _ => Err(format!(
                "invalid stuck action '{s}': expected abort, warn, or retry"
            )),
        }
    }
}

impl StuckAction {
    pub fn from_str_opt(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl fmt::Display for StuckAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Abort => write!(f, "abort"),
            Self::Warn => write!(f, "warn"),
            Self::Retry => write!(f, "retry"),
        }
    }
}

const CONFIG_FILE_NAME: &str = ".agent-loop.toml";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct QualityCommand {
    pub command: String,
    pub remediation: Option<String>,
}

/// Per-project configuration loaded from `.agent-loop.toml`.
///
/// All fields are optional — only explicitly set values override defaults.
/// Unknown keys are rejected (`deny_unknown_fields`) so typos surface early.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    /// Maximum implementation/review rounds before stopping (0 = unlimited).
    review_max_rounds: Option<u32>,
    /// Deprecated — renamed to `review_max_rounds`. Kept for migration error.
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
    /// Which agent plans: `"claude"` or `"codex"`. Defaults to implementer.
    planner: Option<String>,
    /// Use the same agent for both roles.
    single_agent: Option<bool>,
    /// Auto-commit loop-owned changes after each round.
    auto_commit: Option<bool>,
    /// Run quality checks before review.
    auto_test: Option<bool>,
    /// Override auto-detected quality check command.
    auto_test_cmd: Option<String>,
    /// Explicit quality checks that override auto-detection when set.
    quality_commands: Option<Vec<QualityCommand>>,
    /// Run post-consensus compound phase to extract reusable learnings.
    compound: Option<bool>,
    /// Master switch for the decisions subsystem (default false).
    decisions_enabled: Option<bool>,
    /// Auto-sync managed decisions-reference blocks in AGENTS.md/CLAUDE.md (default true).
    decisions_auto_reference: Option<bool>,
    /// Number of trailing decisions lines to inject in prompts.
    decisions_max_lines: Option<u32>,
    /// Maximum diff lines before truncation.
    diff_max_lines: Option<u32>,
    /// Maximum lines for project-context output.
    context_line_cap: Option<u32>,
    /// Maximum lines per file excerpt in planning context.
    planning_context_excerpt_lines: Option<u32>,
    /// Maximum parallel task execution (future-safe plumbing).
    max_parallel: Option<u32>,
    /// Implement all tasks from tasks.md in one batch loop (default true).
    batch_implement: Option<bool>,

    /// Replace front-loaded project context with on-demand state manifest.
    progressive_context: Option<bool>,

    /// Run an adversarial second review of plans (dual-agent only, default true).
    planning_adversarial_review: Option<bool>,

    /// Rounds a planning finding stays open before swapping roles (0 = disabled).
    planning_role_swap_after: Option<u32>,

    // ── Stuck detection ─────────────────────────────────────────────
    /// Enable stuck detection in the implementation loop.
    stuck_detection_enabled: Option<bool>,
    /// Consecutive no-diff rounds before signalling.
    stuck_no_diff_rounds: Option<u32>,
    /// Wall-clock minutes before signalling.
    stuck_threshold_minutes: Option<u64>,
    /// Action on stuck: `"abort"`, `"warn"`, `"retry"`.
    stuck_action: Option<String>,

    // ── Wave runtime ────────────────────────────────────────────────
    /// Seconds before a wave lock file is considered stale (default 30).
    wave_lock_stale_seconds: Option<u64>,
    /// Milliseconds to wait for in-flight tasks before forceful shutdown (default 30000).
    wave_shutdown_grace_ms: Option<u64>,

    // ── Model selection ─────────────────────────────────────────────
    /// Model override for the implementer role.
    implementer_model: Option<String>,
    /// Model override for the reviewer role.
    reviewer_model: Option<String>,
    /// Model override for the planning phase.
    planner_model: Option<String>,
    /// Planner permission mode: "default" or "plan".
    planner_permission_mode: Option<String>,

    // ── Claude CLI tuning ──────────────────────────────────────────
    /// Bypass allowedTools and use `--dangerously-skip-permissions`.
    claude_full_access: Option<bool>,
    /// Comma-separated list of tools Claude is allowed to use.
    claude_allowed_tools: Option<String>,
    /// Comma-separated list of tools the reviewer is allowed to use (read-only by default).
    reviewer_allowed_tools: Option<String>,
    /// Persist Claude sessions across implementation rounds.
    claude_session_persistence: Option<bool>,
    /// Global Claude effort level: `"low"`, `"medium"`, `"high"`.
    claude_effort_level: Option<String>,
    /// Max output tokens for Claude (up to 64000).
    claude_max_output_tokens: Option<u32>,
    /// Max thinking tokens for Claude extended thinking.
    claude_max_thinking_tokens: Option<u32>,
    /// Effort level override for the implementer role.
    implementer_effort_level: Option<String>,
    /// Effort level override for the reviewer role.
    reviewer_effort_level: Option<String>,

    // ── Codex CLI tuning ───────────────────────────────────────────
    /// Bypass --full-auto and use `--dangerously-bypass-approvals-and-sandbox`.
    codex_full_access: Option<bool>,
    /// Persist Codex sessions across rounds (mirrors claude_session_persistence).
    codex_session_persistence: Option<bool>,

    // ── Observability ────────────────────────────────────────────────
    /// Write a human-readable agent I/O transcript to `.agent-loop/state/transcript.log` (default true).
    transcript_enabled: Option<bool>,

    // ── SQLite persistence ──────────────────────────────────────────
    /// Use SQLite for state persistence instead of flat files (default false).
    sqlite_state: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    name: String,
    model: Option<String>,
}

impl Agent {
    /// Create a new agent, validating against the registry.
    pub fn new(name: &str) -> Result<Self, AgentLoopError> {
        if !crate::agent_registry::is_known_agent(name) {
            return Err(AgentLoopError::Config(format!("unknown agent '{name}'")));
        }
        Ok(Self {
            name: name.to_string(),
            model: None,
        })
    }

    /// Convenience constructor that panics if the name is not registered.
    pub fn known(name: &str) -> Self {
        Self::new(name).unwrap_or_else(|_| panic!("'{name}' is not a registered agent"))
    }

    /// Return a clone with the given model set.
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Clear the model selection (e.g. when the agent doesn't support it).
    pub fn clear_model(&mut self) {
        self.model = None;
    }

    pub fn spec(&self) -> &'static crate::agent_registry::AgentSpec {
        crate::agent_registry::get_agent_spec(&self.name)
            .expect("agent should be validated at construction time")
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
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

#[derive(Debug, Clone)]
pub struct Config {
    pub project_dir: PathBuf,
    pub state_dir: PathBuf,
    pub session: Option<String>,
    pub review_max_rounds: u32,
    pub planning_max_rounds: u32,
    pub decomposition_max_rounds: u32,
    pub timeout_seconds: u64,
    pub implementer: Agent,
    pub reviewer: Agent,
    pub planner: Agent,
    pub single_agent: bool,
    pub run_mode: RunMode,
    pub auto_commit: bool,
    pub auto_test: bool,
    pub auto_test_cmd: Option<String>,
    pub quality_commands: Vec<QualityCommand>,
    pub compound: bool,
    /// Master switch for the decisions subsystem.
    pub decisions_enabled: bool,
    /// Auto-sync managed decisions-reference blocks in AGENTS.md/CLAUDE.md.
    pub decisions_auto_reference: bool,
    pub decisions_max_lines: u32,
    pub diff_max_lines: Option<u32>,
    #[allow(dead_code)]
    pub context_line_cap: Option<u32>,
    #[allow(dead_code)]
    pub planning_context_excerpt_lines: Option<u32>,
    pub max_parallel: u32,
    pub batch_implement: bool,
    #[allow(dead_code)]
    pub verbose: bool,

    // ── Progressive context ────────────────────────────────────────
    /// When true, replace front-loaded context with a compact state manifest.
    pub progressive_context: bool,

    // ── Planning adversarial review ──────────────────────────────
    /// Run an adversarial second review of plans (dual-agent only).
    pub planning_adversarial_review: bool,

    // ── Planning role swap ─────────────────────────────────────
    /// Rounds a finding stays open before swapping roles (0 = disabled).
    pub planning_role_swap_after: u32,

    // ── Stuck detection ─────────────────────────────────────────────
    /// Enable stuck detection in the implementation loop.
    pub stuck_detection_enabled: bool,
    /// Consecutive no-diff rounds before signalling.
    pub stuck_no_diff_rounds: u32,
    /// Wall-clock minutes before signalling.
    pub stuck_threshold_minutes: u64,
    /// Action to take when stuck is detected.
    pub stuck_action: StuckAction,

    // ── Wave runtime ────────────────────────────────────────────────
    /// Seconds before a wave lock is considered stale (default 30).
    pub wave_lock_stale_seconds: u64,
    /// Grace period (ms) for in-flight tasks on interrupt (default 30_000).
    pub wave_shutdown_grace_ms: u64,

    // ── Model selection ─────────────────────────────────────────────
    /// Planner permission mode: "default" (normal) or "plan" (Claude read-only).
    pub planner_permission_mode: String,

    // ── Claude CLI tuning ──────────────────────────────────────────
    /// When true, use `--dangerously-skip-permissions` instead of `--allowedTools`.
    pub claude_full_access: bool,
    /// Comma-separated list of tools Claude is allowed to use.
    pub claude_allowed_tools: String,
    /// Comma-separated list of tools the reviewer is allowed to use.
    pub reviewer_allowed_tools: String,
    /// Persist Claude sessions across implementation rounds.
    pub claude_session_persistence: bool,
    /// Global Claude effort level: `"low"`, `"medium"`, `"high"`.
    pub claude_effort_level: Option<String>,
    /// Max output tokens for Claude (up to 64000).
    pub claude_max_output_tokens: Option<u32>,
    /// Max thinking tokens for Claude extended thinking.
    pub claude_max_thinking_tokens: Option<u32>,
    /// Effort level override for the implementer role.
    pub implementer_effort_level: Option<String>,
    /// Effort level override for the reviewer role.
    pub reviewer_effort_level: Option<String>,

    // ── Codex CLI tuning ───────────────────────────────────────────
    /// When true, use `--dangerously-bypass-approvals-and-sandbox` instead of `--full-auto`.
    pub codex_full_access: bool,
    /// Persist Codex sessions across rounds (default true).
    pub codex_session_persistence: bool,

    // ── Observability ─────────────────────────────────────────────────
    /// When true, write a human-readable agent I/O transcript.
    pub transcript_enabled: bool,

    // ── SQLite persistence ──────────────────────────────────────────
    /// When set, state is persisted to SQLite instead of flat files.
    pub db: Option<Arc<Db>>,
}

impl Config {
    pub fn effective_diff_max_lines(&self) -> u32 {
        self.diff_max_lines.unwrap_or(DEFAULT_DIFF_MAX_LINES)
    }

    #[allow(dead_code)]
    pub fn effective_context_line_cap(&self) -> u32 {
        match self.context_line_cap.unwrap_or(DEFAULT_CONTEXT_LINE_CAP) {
            0 => u32::MAX,
            v => v,
        }
    }

    #[allow(dead_code)]
    pub fn effective_planning_context_excerpt_lines(&self) -> u32 {
        match self
            .planning_context_excerpt_lines
            .unwrap_or(DEFAULT_PLANNING_CONTEXT_EXCERPT_LINES)
        {
            0 => u32::MAX,
            v => v,
        }
    }

    pub fn agent_loop_dir(&self) -> PathBuf {
        self.project_dir.join(".agent-loop")
    }

    /// Relative state dir path for use in prompt/display text.
    ///
    /// Derived from the actual `state_dir` by stripping `project_dir`, so it
    /// stays correct for wave task configs where `with_state_dir()` points
    /// `state_dir` at a task-local subdirectory.
    pub fn state_dir_rel(&self) -> String {
        self.state_dir
            .strip_prefix(&self.project_dir)
            .unwrap_or(&self.state_dir)
            .to_string_lossy()
            .into_owned()
    }

    pub fn wave_lock_path(&self) -> PathBuf {
        match &self.session {
            Some(name) => self.agent_loop_dir().join(format!("wave-{name}.lock")),
            None => self.agent_loop_dir().join("wave.lock"),
        }
    }

    pub fn wave_journal_path(&self) -> PathBuf {
        match &self.session {
            Some(name) => self.agent_loop_dir().join(format!("wave-progress-{name}.jsonl")),
            None => self.agent_loop_dir().join("wave-progress.jsonl"),
        }
    }

    /// Clone this config with a different state_dir and auto_commit disabled.
    /// Used by wave orchestrator to give each task its own state directory.
    pub fn with_state_dir(&self, state_dir: PathBuf) -> Self {
        let mut cloned = self.clone();
        cloned.state_dir = state_dir;
        cloned.auto_commit = false;
        cloned
    }

    pub fn from_cli(
        project_dir: PathBuf,
        single_agent_flag: bool,
        verbose_flag: bool,
        session: Option<&str>,
    ) -> Result<Self, AgentLoopError> {
        Self::from_cli_with_overrides(project_dir, single_agent_flag, verbose_flag, None, session)
    }

    /// Build config with optional overrides applied **before** validation.
    ///
    /// `review_max_rounds_override` takes highest precedence (above env vars and TOML)
    /// and is validated together with the rest of the config.
    pub fn from_cli_with_overrides(
        project_dir: PathBuf,
        single_agent_flag: bool,
        verbose_flag: bool,
        review_max_rounds_override: Option<u32>,
        session: Option<&str>,
    ) -> Result<Self, AgentLoopError> {
        if let Some(name) = session {
            validate_session_name(name)?;
        }
        let FileConfigResult {
            config: file,
            file_found: config_file_found,
        } = load_file_config(&project_dir)?;

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
            .unwrap_or_else(|| Agent::known("claude"));

        // --- reviewer: env > TOML > derived default ---
        let reviewer = if single_agent {
            implementer.clone()
        } else {
            env_agent("REVIEWER")
                .or_else(|| parse_agent(file.reviewer.as_deref()))
                .unwrap_or_else(|| default_reviewer_for(&implementer))
        };

        // --- planner: env > TOML > implementer (resolved after model application) ---
        let explicit_planner = if single_agent {
            None
        } else {
            env_agent("PLANNER").or_else(|| parse_agent(file.planner.as_deref()))
        };

        // --- model selection: env > TOML > None ---
        let implementer_model = env_trimmed_string("IMPLEMENTER_MODEL").or(file.implementer_model);
        let reviewer_model = env_trimmed_string("REVIEWER_MODEL").or(file.reviewer_model);
        let planner_model = env_trimmed_string("PLANNER_MODEL").or(file.planner_model);
        let planner_permission_mode = env_trimmed_string("PLANNER_PERMISSION_MODE")
            .or(file.planner_permission_mode)
            .unwrap_or_else(|| "default".to_string())
            .to_ascii_lowercase();

        // Apply models to agents.
        let implementer = implementer.with_model(implementer_model);
        let reviewer = if single_agent {
            if reviewer_model.is_some() {
                eprintln!("reviewer_model is ignored when single_agent=true");
            }
            reviewer
        } else {
            reviewer.with_model(reviewer_model)
        };

        // Planner defaults to implementer (inheriting its model). planner_model
        // is only applied when explicitly set so None doesn't clear inherited model.
        let planner = if single_agent {
            if planner_model.is_some() {
                eprintln!("planner_model is ignored when single_agent=true");
            }
            implementer.clone()
        } else {
            let base = explicit_planner.unwrap_or_else(|| implementer.clone());
            if let Some(model) = planner_model.clone() {
                base.with_model(Some(model))
            } else {
                base
            }
        };

        // --- migration error: old MAX_ROUNDS env var ---
        if env::var("MAX_ROUNDS").is_ok() {
            return Err(AgentLoopError::Config(
                "`MAX_ROUNDS` was renamed to `REVIEW_MAX_ROUNDS`. \
                 Please update your environment variable."
                    .to_string(),
            ));
        }

        // --- numeric: override > env > TOML > default ---
        // Round-limit env vars use strict parsing: invalid values fail instead
        // of silently falling back to defaults.
        let review_max_rounds = match review_max_rounds_override {
            Some(v) => v,
            None => strict_parse_env("REVIEW_MAX_ROUNDS")?
                .or(file.review_max_rounds)
                .unwrap_or(DEFAULT_REVIEW_MAX_ROUNDS),
        };
        let planning_max_rounds = strict_parse_env("PLANNING_MAX_ROUNDS")?
            .or(file.planning_max_rounds)
            .unwrap_or(DEFAULT_PLANNING_MAX_ROUNDS);
        let decomposition_max_rounds = strict_parse_env("DECOMPOSITION_MAX_ROUNDS")?
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
        let quality_commands = file.quality_commands.unwrap_or_default();
        let compound = env_bool("COMPOUND").or(file.compound).unwrap_or(true);
        let decisions_enabled = env_bool("DECISIONS_ENABLED")
            .or(file.decisions_enabled)
            .unwrap_or(false);
        let decisions_auto_reference = env_bool("DECISIONS_AUTO_REFERENCE")
            .or(file.decisions_auto_reference)
            .unwrap_or(true);
        let decisions_max_lines = parse_env("DECISIONS_MAX_LINES")
            .or(file.decisions_max_lines)
            .unwrap_or(DEFAULT_DECISIONS_MAX_LINES);

        // --- diff/context limits: env > TOML > None (defaults via effective helpers) ---
        let diff_max_lines = parse_env("DIFF_MAX_LINES").or(file.diff_max_lines);
        let context_line_cap = parse_env("CONTEXT_LINE_CAP").or(file.context_line_cap);
        let planning_context_excerpt_lines =
            parse_env("PLANNING_CONTEXT_EXCERPT_LINES").or(file.planning_context_excerpt_lines);

        // --- max_parallel: env > TOML > default ---
        let max_parallel = parse_env("MAX_PARALLEL")
            .or(file.max_parallel)
            .unwrap_or(DEFAULT_MAX_PARALLEL);
        let batch_implement = env_bool("BATCH_IMPLEMENT")
            .or(file.batch_implement)
            .unwrap_or(true);
        let progressive_context = env_bool("PROGRESSIVE_CONTEXT")
            .or(file.progressive_context)
            .unwrap_or(false);
        let planning_adversarial_review = env_bool("PLANNING_ADVERSARIAL_REVIEW")
            .or(file.planning_adversarial_review)
            .unwrap_or(true);
        let planning_role_swap_after = parse_env("PLANNING_ROLE_SWAP_AFTER")
            .or(file.planning_role_swap_after)
            .unwrap_or(DEFAULT_PLANNING_ROLE_SWAP_AFTER);

        // --- stuck detection: env > TOML > default ---
        let stuck_detection_enabled = env_bool("STUCK_DETECTION_ENABLED")
            .or(file.stuck_detection_enabled)
            .unwrap_or(false);
        let stuck_no_diff_rounds = parse_env("STUCK_NO_DIFF_ROUNDS")
            .or(file.stuck_no_diff_rounds)
            .unwrap_or(DEFAULT_STUCK_NO_DIFF_ROUNDS);
        let stuck_threshold_minutes = parse_env("STUCK_THRESHOLD_MINUTES")
            .or(file.stuck_threshold_minutes)
            .unwrap_or(DEFAULT_STUCK_THRESHOLD_MINUTES);
        let stuck_action = env_trimmed_string("STUCK_ACTION")
            .or(file.stuck_action)
            .and_then(|s| StuckAction::from_str_opt(&s))
            .unwrap_or(StuckAction::Warn);

        // --- Wave runtime: env > TOML > default ---
        let wave_lock_stale_seconds = parse_env("WAVE_LOCK_STALE_SECONDS")
            .or(file.wave_lock_stale_seconds)
            .unwrap_or(30);
        let wave_shutdown_grace_ms = parse_env("WAVE_SHUTDOWN_GRACE_MS")
            .or(file.wave_shutdown_grace_ms)
            .unwrap_or(30_000);

        // --- verbose: CLI flag > env > default (false) ---
        let verbose = verbose_flag || env_bool("VERBOSE").unwrap_or(false);

        // --- Claude CLI tuning: env > TOML > default ---
        let claude_full_access_explicit =
            env_bool("CLAUDE_FULL_ACCESS").is_some() || file.claude_full_access.is_some();
        let claude_full_access = env_bool("CLAUDE_FULL_ACCESS")
            .or(file.claude_full_access)
            .unwrap_or(true);
        let claude_allowed_tools = env_trimmed_string("CLAUDE_ALLOWED_TOOLS")
            .or(file.claude_allowed_tools)
            .unwrap_or_else(|| DEFAULT_CLAUDE_ALLOWED_TOOLS.to_string());
        let reviewer_allowed_tools = env_trimmed_string("REVIEWER_ALLOWED_TOOLS")
            .or(file.reviewer_allowed_tools)
            .unwrap_or_else(|| DEFAULT_REVIEWER_ALLOWED_TOOLS.to_string());
        let claude_session_persistence = env_bool("CLAUDE_SESSION_PERSISTENCE")
            .or(file.claude_session_persistence)
            .unwrap_or(true);
        let claude_effort_level =
            env_trimmed_string("CLAUDE_EFFORT_LEVEL").or(file.claude_effort_level);
        let claude_max_output_tokens: Option<u32> =
            parse_env("CLAUDE_MAX_OUTPUT_TOKENS").or(file.claude_max_output_tokens);
        let claude_max_thinking_tokens: Option<u32> =
            parse_env("CLAUDE_MAX_THINKING_TOKENS").or(file.claude_max_thinking_tokens);
        let implementer_effort_level =
            env_trimmed_string("IMPLEMENTER_EFFORT_LEVEL").or(file.implementer_effort_level);
        let reviewer_effort_level =
            env_trimmed_string("REVIEWER_EFFORT_LEVEL").or(file.reviewer_effort_level);

        // --- Codex CLI tuning: env > TOML > default ---
        let codex_full_access_explicit =
            env_bool("CODEX_FULL_ACCESS").is_some() || file.codex_full_access.is_some();
        let codex_full_access = env_bool("CODEX_FULL_ACCESS")
            .or(file.codex_full_access)
            .unwrap_or(true);
        let codex_session_persistence = env_bool("CODEX_SESSION_PERSISTENCE")
            .or(file.codex_session_persistence)
            .unwrap_or(true);

        // --- Observability: env > TOML > default ---
        let transcript_enabled = env_bool("TRANSCRIPT_ENABLED")
            .or(file.transcript_enabled)
            .unwrap_or(true);

        let state_dir = match session {
            Some(name) => project_dir.join(".agent-loop").join("state").join(name),
            None => project_dir.join(".agent-loop").join("state"),
        };

        let mut config = Self {
            state_dir,
            session: session.map(|s| s.to_string()),
            run_mode: resolve_run_mode(single_agent),
            project_dir,
            review_max_rounds,
            planning_max_rounds,
            decomposition_max_rounds,
            timeout_seconds,
            implementer,
            reviewer,
            planner,
            single_agent,
            auto_commit,
            auto_test,
            auto_test_cmd,
            quality_commands,
            compound,
            decisions_enabled,
            decisions_auto_reference,
            decisions_max_lines,
            diff_max_lines,
            context_line_cap,
            planning_context_excerpt_lines,
            max_parallel,
            batch_implement,
            verbose,
            progressive_context,
            planning_adversarial_review,
            planning_role_swap_after,
            stuck_detection_enabled,
            stuck_no_diff_rounds,
            stuck_threshold_minutes,
            stuck_action,
            wave_lock_stale_seconds,
            wave_shutdown_grace_ms,
            planner_permission_mode,
            claude_full_access,
            claude_allowed_tools,
            reviewer_allowed_tools,
            claude_session_persistence,
            claude_effort_level,
            claude_max_output_tokens,
            claude_max_thinking_tokens,
            implementer_effort_level,
            reviewer_effort_level,
            codex_full_access,
            codex_session_persistence,
            transcript_enabled,
            db: None,
        };

        // Open SQLite database for state persistence (opt-in via env var or config)
        let sqlite_enabled = env_bool("SQLITE_STATE")
            .or(file.sqlite_state)
            .unwrap_or(false);
        if sqlite_enabled {
            let db_path = config.state_dir.join("agent-loop.db");
            if let Some(parent) = db_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match Db::open(&db_path) {
                Ok(db) => config.db = Some(Arc::new(db)),
                Err(err) => {
                    eprintln!("\u{26a0} failed to open SQLite database, falling back to flat files: {err}");
                }
            }
        }

        validate_config_bounds(&config)?;
        emit_config_warnings(
            &config,
            config_file_found,
            claude_full_access_explicit,
            codex_full_access_explicit,
        );

        Ok(config)
    }
}

/// Validate a session name: alphanumeric, hyphens, underscores only.
/// Max 64 characters. No path separators, dots, or spaces.
pub(crate) fn validate_session_name(name: &str) -> Result<(), AgentLoopError> {
    if name.is_empty() {
        return Err(AgentLoopError::Config(
            "Session name cannot be empty.".to_string(),
        ));
    }
    if name.len() > 64 {
        return Err(AgentLoopError::Config(format!(
            "Session name too long ({} chars, max 64): {name}",
            name.len()
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AgentLoopError::Config(format!(
            "Invalid session name '{name}': only alphanumeric, hyphens, and underscores allowed."
        )));
    }
    Ok(())
}

/// Validate that agent string fields in the config contain known values.
fn validate_file_config(config: &FileConfig, path: &Path) -> Result<(), AgentLoopError> {
    // Migration error: old `max_rounds` TOML key renamed to `review_max_rounds`.
    if config.max_rounds.is_some() {
        return Err(AgentLoopError::Config(format!(
            "`max_rounds` was renamed to `review_max_rounds` in {}. \
             Please update your config file.",
            path.display(),
        )));
    }

    if let Some(ref value) = config.implementer
        && !crate::agent_registry::is_known_agent(value)
    {
        return Err(AgentLoopError::Config(format!(
            "invalid implementer '{}' in {}: not a registered agent",
            value,
            path.display(),
        )));
    }
    if let Some(ref value) = config.reviewer
        && !crate::agent_registry::is_known_agent(value)
    {
        return Err(AgentLoopError::Config(format!(
            "invalid reviewer '{}' in {}: not a registered agent",
            value,
            path.display(),
        )));
    }
    if let Some(ref value) = config.planner
        && !crate::agent_registry::is_known_agent(value)
    {
        return Err(AgentLoopError::Config(format!(
            "invalid planner '{}' in {}: not a registered agent",
            value,
            path.display(),
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct FileConfigResult {
    config: FileConfig,
    file_found: bool,
}

/// Load `.agent-loop.toml` from `project_dir`. Returns default on missing file.
/// Returns an error on I/O failures (other than not-found) or parse failures.
fn load_file_config(project_dir: &Path) -> Result<FileConfigResult, AgentLoopError> {
    let path = project_dir.join(CONFIG_FILE_NAME);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileConfigResult {
                config: FileConfig::default(),
                file_found: false,
            });
        }
        Err(err) => {
            return Err(AgentLoopError::Config(format!(
                "failed to read {}: {err}",
                path.display()
            )));
        }
    };

    // Pre-parse check: detect legacy `max_rounds` key before serde
    // deserialization so that non-u32 values (e.g. -1, "foo") still get the
    // migration rename error instead of a generic type/parse failure.
    if let Ok(table) = content.parse::<toml::Table>()
        && table.contains_key("max_rounds")
    {
        return Err(AgentLoopError::Config(format!(
            "`max_rounds` was renamed to `review_max_rounds` in {}. \
             Please update your config file.",
            path.display()
        )));
    }

    let config = toml::from_str::<FileConfig>(&content).map_err(|err| {
        AgentLoopError::Config(format!("failed to parse {}: {err}", path.display()))
    })?;
    validate_file_config(&config, &path)?;
    Ok(FileConfigResult {
        config,
        file_found: true,
    })
}

pub fn is_truthy(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

pub fn is_falsy(value: &str) -> bool {
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
    Agent::new(value?).ok()
}

fn env_agent(key: &str) -> Option<Agent> {
    parse_agent(env::var(key).ok().as_deref())
}

pub fn default_reviewer_for(agent: &Agent) -> Agent {
    Agent::known(agent.spec().default_reviewer)
}

#[cfg(test)]
pub fn resolve_implementer(env_value: Option<&str>) -> Agent {
    if env_value == Some("codex") {
        Agent::known("codex")
    } else {
        Agent::known("claude")
    }
}

#[cfg(test)]
pub fn resolve_reviewer(implementer: &Agent, single_agent: bool) -> Agent {
    if single_agent {
        return implementer.clone();
    }
    default_reviewer_for(implementer)
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

/// Like `parse_env` but returns an error when the env var is set to a
/// non-parseable value instead of silently falling back to `None`.
fn strict_parse_env<T: FromStr>(key: &str) -> Result<Option<T>, AgentLoopError> {
    match env::var(key) {
        Err(_) => Ok(None),
        Ok(value) => value.parse::<T>().map(Some).map_err(|_| {
            AgentLoopError::Config(format!(
                "invalid value '{value}' for {key}: expected a non-negative integer"
            ))
        }),
    }
}

fn validate_config_bounds(config: &Config) -> Result<(), AgentLoopError> {
    // review_max_rounds, planning_max_rounds, decomposition_max_rounds:
    // 0 means unlimited — no validation needed for these fields.

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

    if config.decisions_enabled && config.decisions_max_lines == 0 {
        return Err(AgentLoopError::Config(
            "decisions_max_lines must be > 0 when decisions are enabled. \
             Set DECISIONS_MAX_LINES or decisions_max_lines in .agent-loop.toml to a positive value."
                .to_string(),
        ));
    }

    fn validate_effort_level(value: Option<&str>, field_name: &str) -> Result<(), AgentLoopError> {
        if let Some(level) = value
            && !matches!(level, "low" | "medium" | "high")
        {
            return Err(AgentLoopError::Config(format!(
                "{field_name} must be one of [\"low\", \"medium\", \"high\"], got \"{level}\""
            )));
        }
        Ok(())
    }

    validate_effort_level(config.claude_effort_level.as_deref(), "claude_effort_level")?;
    validate_effort_level(
        config.implementer_effort_level.as_deref(),
        "implementer_effort_level",
    )?;
    validate_effort_level(
        config.reviewer_effort_level.as_deref(),
        "reviewer_effort_level",
    )?;

    if !matches!(config.planner_permission_mode.as_str(), "default" | "plan") {
        return Err(AgentLoopError::Config(format!(
            "planner_permission_mode must be one of [\"default\", \"plan\"], got \"{}\"",
            config.planner_permission_mode
        )));
    }

    if let Some(max) = config.claude_max_output_tokens
        && (max == 0 || max > 64000)
    {
        return Err(AgentLoopError::Config(format!(
            "claude_max_output_tokens must be between 1 and 64000, got {max}"
        )));
    }

    Ok(())
}

/// Returns true if the missing-config hint should be emitted based on environment
/// conditions, excluding the one-time process guard. Extracted as a pure function
/// for unit testability (integration tests run with piped stderr so the
/// `is_terminal` branch cannot be exercised there).
fn should_emit_missing_config_hint(
    config_file_found: bool,
    ci_set: bool,
    is_terminal: bool,
) -> bool {
    if config_file_found {
        return false;
    }
    if ci_set {
        return false;
    }
    if !is_terminal {
        return false;
    }
    true
}

/// Combines environment checks with a one-time guard. Returns true if the hint
/// should be printed (first eligible call only). Takes `guard` as a parameter
/// so unit tests can supply their own `AtomicBool` instead of sharing the
/// process-wide static.
fn try_emit_missing_config_hint(
    config_file_found: bool,
    ci_set: bool,
    is_terminal: bool,
    guard: &std::sync::atomic::AtomicBool,
) -> bool {
    if !should_emit_missing_config_hint(config_file_found, ci_set, is_terminal) {
        return false;
    }
    // swap returns the *previous* value; true means already emitted.
    !guard.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// Returns true if the full-access default warning should be emitted.
/// Fires when full-access is on for either agent AND the user has not
/// explicitly set the value via TOML or env.
fn should_emit_full_access_warning(
    claude_full_access: bool,
    claude_explicit: bool,
    codex_full_access: bool,
    codex_explicit: bool,
    ci_set: bool,
    is_terminal: bool,
) -> bool {
    if ci_set || !is_terminal {
        return false;
    }
    // Warn only when full-access is still the implicit default.
    (claude_full_access && !claude_explicit) || (codex_full_access && !codex_explicit)
}

fn try_emit_full_access_warning(
    claude_full_access: bool,
    claude_explicit: bool,
    codex_full_access: bool,
    codex_explicit: bool,
    ci_set: bool,
    is_terminal: bool,
    guard: &std::sync::atomic::AtomicBool,
) -> bool {
    if !should_emit_full_access_warning(
        claude_full_access,
        claude_explicit,
        codex_full_access,
        codex_explicit,
        ci_set,
        is_terminal,
    ) {
        return false;
    }
    !guard.swap(true, std::sync::atomic::Ordering::Relaxed)
}

fn emit_config_warnings(
    config: &Config,
    config_file_found: bool,
    claude_full_access_explicit: bool,
    codex_full_access_explicit: bool,
) {
    use std::sync::atomic::AtomicBool;

    static MISSING_CONFIG_HINT_EMITTED: AtomicBool = AtomicBool::new(false);
    static FULL_ACCESS_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

    let ci_set = std::env::var_os("CI").is_some();
    let is_terminal = std::io::IsTerminal::is_terminal(&std::io::stderr());

    if try_emit_missing_config_hint(
        config_file_found,
        ci_set,
        is_terminal,
        &MISSING_CONFIG_HINT_EMITTED,
    ) {
        eprintln!("Hint: no .agent-loop.toml found. Run 'agent-loop config init' to generate one.");
    }

    if try_emit_full_access_warning(
        config.claude_full_access,
        claude_full_access_explicit,
        config.codex_full_access,
        codex_full_access_explicit,
        ci_set,
        is_terminal,
        &FULL_ACCESS_WARNING_EMITTED,
    ) {
        eprintln!(
            "Warning: full-access mode is active by default (--dangerously-skip-permissions / \
             --dangerously-bypass-approvals-and-sandbox). \
             Set claude_full_access=false or codex_full_access=false in .agent-loop.toml to constrain."
        );
    }
}

/// Generate a fully-commented TOML config template with all settings organized
/// by section and values sourced from `DEFAULT_*` constants.
pub fn generate_default_config_template() -> String {
    format!(
        r#"# agent-loop configuration
# All settings are optional — uncomment and modify as needed.
# Precedence: CLI flags > environment variables > this file > built-in defaults.
# Round limits: 0 = unlimited (default); positive values cap rounds.

# ── Core ─────────────────────────────────────────────────────────────────────
# review_max_rounds = {review_max_rounds}
# planning_max_rounds = {planning_max_rounds}
# decomposition_max_rounds = {decomposition_max_rounds}
# timeout = {timeout}
# auto_commit = true
# auto_test = false
# auto_test_cmd = ""
# compound = true
# decisions_enabled = false
# decisions_auto_reference = true
# decisions_max_lines = {decisions_max_lines}
# diff_max_lines = {diff_max_lines}
# context_line_cap = {context_line_cap}
# planning_context_excerpt_lines = {planning_context_excerpt_lines}
# batch_implement = true
# progressive_context = false
# planning_adversarial_review = true      # adversarial second review of plans (dual-agent only)
# planning_role_swap_after = {planning_role_swap_after}       # rounds before swapping reviewer/implementer on stuck findings (0 = disabled)

# ── Agents ───────────────────────────────────────────────────────────────────
# implementer = "claude"
# reviewer = "codex"
# planner = "claude"
# single_agent = false

# ── Model selection ──────────────────────────────────────────────────────────
# implementer_model = ""
# reviewer_model = ""
# planner_model = ""
# planner_permission_mode = "default"

# ── Claude CLI tuning ────────────────────────────────────────────────────────
# claude_full_access = true
# If you need constraints, set claude_full_access = false and configure:
# claude_allowed_tools = "{claude_allowed_tools}"
# reviewer_allowed_tools = "{reviewer_allowed_tools}"
# claude_session_persistence = true
# claude_effort_level = ""
# claude_max_output_tokens = 16000
# claude_max_thinking_tokens = 10000
# implementer_effort_level = ""
# reviewer_effort_level = ""

# ── Codex CLI tuning ────────────────────────────────────────────────────────
# codex_full_access = true
# If you need reduced permissions, set codex_full_access = false.
# codex_session_persistence = true

# ── Stuck detection ──────────────────────────────────────────────────────────
# stuck_detection_enabled = false
# stuck_no_diff_rounds = {stuck_no_diff_rounds}
# stuck_threshold_minutes = {stuck_threshold_minutes}
# stuck_action = "warn"

# ── Wave runtime ─────────────────────────────────────────────────────────────
# max_parallel = {max_parallel}
# wave_lock_stale_seconds = 30
# wave_shutdown_grace_ms = 30000

# ── Observability ─────────────────────────────────────────────────────────────
# transcript_enabled = false

# ── Quality commands ─────────────────────────────────────────────────────────
# Uncomment and customize to define explicit quality checks.
# [[quality_commands]]
# command = "cargo clippy -- -D warnings"
# remediation = "Fix all clippy warnings."
#
# [[quality_commands]]
# command = "cargo test"
# remediation = "Fix failing tests."
"#,
        review_max_rounds = DEFAULT_REVIEW_MAX_ROUNDS,
        planning_max_rounds = DEFAULT_PLANNING_MAX_ROUNDS,
        decomposition_max_rounds = DEFAULT_DECOMPOSITION_MAX_ROUNDS,
        timeout = DEFAULT_TIMEOUT_SECONDS,
        decisions_max_lines = DEFAULT_DECISIONS_MAX_LINES,
        diff_max_lines = DEFAULT_DIFF_MAX_LINES,
        context_line_cap = DEFAULT_CONTEXT_LINE_CAP,
        planning_context_excerpt_lines = DEFAULT_PLANNING_CONTEXT_EXCERPT_LINES,
        claude_allowed_tools = DEFAULT_CLAUDE_ALLOWED_TOOLS,
        reviewer_allowed_tools = DEFAULT_REVIEWER_ALLOWED_TOOLS,
        stuck_no_diff_rounds = DEFAULT_STUCK_NO_DIFF_ROUNDS,
        stuck_threshold_minutes = DEFAULT_STUCK_THRESHOLD_MINUTES,
        max_parallel = DEFAULT_MAX_PARALLEL,
        planning_role_swap_after = DEFAULT_PLANNING_ROLE_SWAP_AFTER,
    )
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
            "PLANNER",
            "AUTO_COMMIT",
            "AUTO_TEST",
            "AUTO_TEST_CMD",
            "COMPOUND",
            "DECISIONS_ENABLED",
            "DECISIONS_AUTO_REFERENCE",
            "DECISIONS_MAX_LINES",
            "REVIEW_MAX_ROUNDS",
            "MAX_ROUNDS", // deprecated — kept so migration error tests start clean
            "PLANNING_MAX_ROUNDS",
            "DECOMPOSITION_MAX_ROUNDS",
            "TIMEOUT",
            "DIFF_MAX_LINES",
            "CONTEXT_LINE_CAP",
            "PLANNING_CONTEXT_EXCERPT_LINES",
            "MAX_PARALLEL",
            "BATCH_IMPLEMENT",
            "VERBOSE",
            "PROGRESSIVE_CONTEXT",
            "PLANNING_ADVERSARIAL_REVIEW",
            // Stuck detection
            "STUCK_DETECTION_ENABLED",
            "STUCK_NO_DIFF_ROUNDS",
            "STUCK_THRESHOLD_MINUTES",
            "STUCK_ACTION",
            // Wave runtime
            "WAVE_LOCK_STALE_SECONDS",
            "WAVE_SHUTDOWN_GRACE_MS",
            // Model selection
            "IMPLEMENTER_MODEL",
            "REVIEWER_MODEL",
            "PLANNER_MODEL",
            "PLANNER_PERMISSION_MODE",
            // Claude CLI tuning
            "CLAUDE_FULL_ACCESS",
            "CLAUDE_ALLOWED_TOOLS",
            "REVIEWER_ALLOWED_TOOLS",
            "CLAUDE_SESSION_PERSISTENCE",
            "CLAUDE_EFFORT_LEVEL",
            "CLAUDE_MAX_OUTPUT_TOKENS",
            "CLAUDE_MAX_THINKING_TOKENS",
            "IMPLEMENTER_EFFORT_LEVEL",
            "REVIEWER_EFFORT_LEVEL",
            // Codex CLI tuning
            "CODEX_FULL_ACCESS",
            "CODEX_SESSION_PERSISTENCE",
            // Observability
            "TRANSCRIPT_ENABLED",
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
    // StuckAction FromStr
    // -----------------------------------------------------------------------

    #[test]
    fn stuck_action_from_str_parses_valid_values() {
        assert_eq!("abort".parse::<StuckAction>().unwrap(), StuckAction::Abort);
        assert_eq!("Warn".parse::<StuckAction>().unwrap(), StuckAction::Warn);
        assert_eq!("RETRY".parse::<StuckAction>().unwrap(), StuckAction::Retry);
    }

    #[test]
    fn stuck_action_from_str_rejects_invalid_values() {
        assert!("invalid".parse::<StuckAction>().is_err());
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
        assert_eq!(resolve_implementer(None), Agent::known("claude"));
        assert_eq!(resolve_implementer(Some("claude")), Agent::known("claude"));
        assert_eq!(resolve_implementer(Some("CODEX")), Agent::known("claude"));
    }

    #[test]
    fn resolve_implementer_uses_codex_only_on_exact_match() {
        assert_eq!(resolve_implementer(Some("codex")), Agent::known("codex"));
    }

    #[test]
    fn resolve_reviewer_matches_single_agent_mode() {
        assert_eq!(
            resolve_reviewer(&Agent::known("claude"), true),
            Agent::known("claude")
        );
        assert_eq!(
            resolve_reviewer(&Agent::known("codex"), true),
            Agent::known("codex")
        );
    }

    #[test]
    fn resolve_reviewer_uses_opposite_agent_in_dual_mode() {
        assert_eq!(
            resolve_reviewer(&Agent::known("claude"), false),
            Agent::known("codex")
        );
        assert_eq!(
            resolve_reviewer(&Agent::known("codex"), false),
            Agent::known("claude")
        );
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
        let result = load_file_config(&dir).expect("missing file should return Ok(default)");
        assert!(!result.file_found);
        assert!(result.config.review_max_rounds.is_none());
        assert!(result.config.implementer.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_valid_full_file() {
        let dir = create_temp_project_root("toml_full");
        write_toml(
            &dir,
            r#"
review_max_rounds = 10
planning_max_rounds = 5
decomposition_max_rounds = 4
timeout = 300
implementer = "codex"
reviewer = "claude"
planner = "claude"
single_agent = true
auto_commit = false
auto_test = true
auto_test_cmd = "cargo test"
compound = false
decisions_max_lines = 75
diff_max_lines = 250
context_line_cap = 150
planning_context_excerpt_lines = 80
max_parallel = 4
batch_implement = false

[[quality_commands]]
command = "cargo clippy -- -D warnings"
remediation = "Fix all clippy warnings."

[[quality_commands]]
command = "cargo test"
"#,
        );
        let result = load_file_config(&dir).expect("valid full file should parse");
        assert!(result.file_found);
        let config = result.config;
        assert_eq!(config.review_max_rounds, Some(10));
        assert_eq!(config.planning_max_rounds, Some(5));
        assert_eq!(config.decomposition_max_rounds, Some(4));
        assert_eq!(config.timeout, Some(300));
        assert_eq!(config.implementer.as_deref(), Some("codex"));
        assert_eq!(config.reviewer.as_deref(), Some("claude"));
        assert_eq!(config.planner.as_deref(), Some("claude"));
        assert_eq!(config.single_agent, Some(true));
        assert_eq!(config.auto_commit, Some(false));
        assert_eq!(config.auto_test, Some(true));
        assert_eq!(config.auto_test_cmd.as_deref(), Some("cargo test"));
        assert_eq!(config.compound, Some(false));
        assert_eq!(config.decisions_max_lines, Some(75));
        let quality_commands = config
            .quality_commands
            .as_ref()
            .expect("quality_commands should parse");
        assert_eq!(quality_commands.len(), 2);
        assert_eq!(quality_commands[0].command, "cargo clippy -- -D warnings");
        assert_eq!(
            quality_commands[0].remediation.as_deref(),
            Some("Fix all clippy warnings.")
        );
        assert_eq!(quality_commands[1].command, "cargo test");
        assert_eq!(quality_commands[1].remediation, None);
        assert_eq!(config.diff_max_lines, Some(250));
        assert_eq!(config.context_line_cap, Some(150));
        assert_eq!(config.planning_context_excerpt_lines, Some(80));
        assert_eq!(config.max_parallel, Some(4));
        assert_eq!(config.batch_implement, Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_partial_file() {
        let dir = create_temp_project_root("toml_partial");
        write_toml(&dir, "review_max_rounds = 7\n");
        let result = load_file_config(&dir).expect("partial file should parse");
        assert!(result.file_found);
        assert_eq!(result.config.review_max_rounds, Some(7));
        assert!(result.config.implementer.is_none());
        assert!(result.config.auto_test.is_none());
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
        write_toml(&dir, "review_max_rounds = \"not a number\"\n");
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
        let config =
            Config::from_cli(project_dir.clone(), false, false, None).expect("from_cli should succeed");

        assert_eq!(config.project_dir, project_dir);
        assert_eq!(config.state_dir, project_dir.join(".agent-loop/state"));
        assert_eq!(config.review_max_rounds, DEFAULT_REVIEW_MAX_ROUNDS);
        assert_eq!(config.planning_max_rounds, DEFAULT_PLANNING_MAX_ROUNDS);
        assert_eq!(
            config.decomposition_max_rounds,
            DEFAULT_DECOMPOSITION_MAX_ROUNDS
        );
        assert_eq!(config.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(config.implementer, Agent::known("claude"));
        assert_eq!(config.reviewer, Agent::known("codex"));
        assert!(!config.single_agent);
        assert_eq!(config.run_mode, RunMode::DualAgent);
        assert!(config.auto_commit);
        assert!(!config.auto_test);
        assert_eq!(config.auto_test_cmd, None);
        assert!(config.compound);
        assert_eq!(config.decisions_max_lines, DEFAULT_DECISIONS_MAX_LINES);
        assert!(config.quality_commands.is_empty());
        assert!(config.batch_implement);
        assert!(config.claude_full_access);
        assert!(config.codex_full_access);
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
        set_env("REVIEW_MAX_ROUNDS", "42");
        set_env("PLANNING_MAX_ROUNDS", "7");
        set_env("DECOMPOSITION_MAX_ROUNDS", "8");
        set_env("TIMEOUT", "900");

        let project_dir = create_temp_project_root("cfg_env_overrides");
        let config =
            Config::from_cli(project_dir.clone(), false, false, None).expect("from_cli should succeed");

        assert_eq!(config.review_max_rounds, 42);
        assert_eq!(config.planning_max_rounds, 7);
        assert_eq!(config.decomposition_max_rounds, 8);
        assert_eq!(config.timeout_seconds, 900);
        assert_eq!(config.implementer, Agent::known("codex"));
        assert_eq!(config.reviewer, Agent::known("codex"));
        assert!(config.single_agent);
        assert_eq!(config.run_mode, RunMode::SingleAgent);
        assert!(!config.auto_commit);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_invalid_round_limit_env_vars_fail_instead_of_defaulting() {
        // Round-limit env vars use strict parsing and produce config errors
        // on invalid values (F-001). Non-round-limit env vars like TIMEOUT
        // still fall back to defaults via parse_env.
        let _guard = env_lock();
        clear_env();
        set_env("REVIEW_MAX_ROUNDS", "not-a-number");

        let project_dir = create_temp_project_root("cfg_invalid_env");
        let err = Config::from_cli(project_dir.clone(), false, false, None)
            .expect_err("invalid round-limit env should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid value 'not-a-number' for REVIEW_MAX_ROUNDS"),
            "expected strict parse error, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_non_round_limit_env_still_falls_back_on_invalid() {
        // TIMEOUT (non-round-limit) still uses lenient parse_env.
        let _guard = env_lock();
        clear_env();
        set_env("TIMEOUT", "-1");

        let project_dir = create_temp_project_root("cfg_invalid_timeout_env");
        let config =
            Config::from_cli(project_dir.clone(), false, false, None).expect("from_cli should succeed");

        assert_eq!(config.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_zero_review_max_rounds_is_valid_unlimited() {
        let _guard = env_lock();
        clear_env();
        set_env("REVIEW_MAX_ROUNDS", "0");

        let project_dir = create_temp_project_root("cfg_zero_rounds");
        let config =
            Config::from_cli(project_dir.clone(), false, false, None).expect("0 should be valid");
        assert_eq!(config.review_max_rounds, 0);
        let _ = std::fs::remove_dir_all(&project_dir);
    }

    #[test]
    fn from_cli_auto_test_defaults_to_off() {
        let _guard = env_lock();
        clear_env();

        let project_dir = create_temp_project_root("cfg_auto_test_off");
        let config =
            Config::from_cli(project_dir.clone(), false, false, None).expect("from_cli should succeed");
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
        let config =
            Config::from_cli(project_dir.clone(), false, false, None).expect("from_cli should succeed");
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
        let config =
            Config::from_cli(project_dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.auto_test_cmd, Some("make test".to_string()));

        set_env("AUTO_TEST_CMD", "   ");
        let config2 =
            Config::from_cli(project_dir.clone(), false, false, None).expect("from_cli should succeed");
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
        let config =
            Config::from_cli(project_dir.clone(), true, false, None).expect("from_cli should succeed");

        assert!(config.single_agent);
        assert_eq!(config.reviewer, Agent::known("codex"));
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
review_max_rounds = 10
planning_max_rounds = 5
decomposition_max_rounds = 4
timeout = 300
implementer = "codex"
reviewer = "codex"
single_agent = true
auto_commit = false
auto_test = true
auto_test_cmd = "cargo test"
compound = false
decisions_max_lines = 90
batch_implement = false
"#,
        );

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");

        assert_eq!(config.review_max_rounds, 10);
        assert_eq!(config.planning_max_rounds, 5);
        assert_eq!(config.decomposition_max_rounds, 4);
        assert_eq!(config.timeout_seconds, 300);
        assert_eq!(config.implementer, Agent::known("codex"));
        assert_eq!(config.reviewer, Agent::known("codex"));
        assert!(config.single_agent);
        assert!(!config.auto_commit);
        assert!(config.auto_test);
        assert_eq!(config.auto_test_cmd, Some("cargo test".to_string()));
        assert!(!config.compound);
        assert_eq!(config.decisions_max_lines, 90);
        assert!(!config.batch_implement);
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
review_max_rounds = 10
timeout = 300
implementer = "codex"
single_agent = true
auto_commit = false
auto_test = false
auto_test_cmd = "make test"
compound = true
decisions_max_lines = 40
batch_implement = true
"#,
        );

        set_env("REVIEW_MAX_ROUNDS", "50");
        set_env("TIMEOUT", "1200");
        set_env("IMPLEMENTER", "claude");
        set_env("SINGLE_AGENT", "false");
        set_env("AUTO_COMMIT", "1");
        set_env("AUTO_TEST", "1");
        set_env("AUTO_TEST_CMD", "npm test");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");

        assert_eq!(config.review_max_rounds, 50);
        assert_eq!(config.timeout_seconds, 1200);
        assert_eq!(config.implementer, Agent::known("claude"));
        assert!(!config.single_agent);
        assert!(config.auto_commit);
        assert!(config.auto_test);
        assert_eq!(config.auto_test_cmd, Some("npm test".to_string()));
        assert!(config.compound);
        assert_eq!(config.decisions_max_lines, 40);
        assert!(config.batch_implement);
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

        let config = Config::from_cli(dir.clone(), true, false, None).expect("from_cli should succeed");
        assert!(config.single_agent);
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

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
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

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
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

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
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

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        // Both claude -> explicit reviewer override honored
        assert_eq!(config.implementer, Agent::known("claude"));
        assert_eq!(config.reviewer, Agent::known("claude"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reviewer_from_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_reviewer_env");
        write_toml(&dir, "reviewer = \"claude\"\n");
        set_env("REVIEWER", "codex");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.reviewer, Agent::known("codex"));
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

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(config.single_agent);
        assert_eq!(config.implementer, Agent::known("codex"));
        assert_eq!(config.reviewer, Agent::known("codex"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_agent_cli_forces_reviewer_even_with_env_reviewer() {
        let _guard = env_lock();
        clear_env();
        set_env("REVIEWER", "codex");
        set_env("IMPLEMENTER", "claude");

        let dir = create_temp_project_root("cfg_reviewer_sa_env");
        let config = Config::from_cli(dir.clone(), true, false, None).expect("from_cli should succeed");
        assert_eq!(config.reviewer, Agent::known("claude"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Planner tests
    // -----------------------------------------------------------------------

    #[test]
    fn from_cli_planner_defaults_to_implementer() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_planner_default");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.planner, config.implementer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_planner_from_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_planner_toml");
        write_toml(&dir, "planner = \"codex\"\n");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.planner.name(), "codex");
        assert_eq!(config.planner.model(), None);
        assert_eq!(config.implementer.name(), "claude");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_planner_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_planner_env");
        write_toml(&dir, "planner = \"claude\"\n");
        set_env("PLANNER", "codex");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.planner, Agent::known("codex"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_single_agent_forces_planner_equals_implementer() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_planner_sa");
        write_toml(
            &dir,
            "implementer = \"codex\"\nplanner = \"claude\"\nsingle_agent = true\n",
        );

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(config.single_agent);
        assert_eq!(config.planner, config.implementer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_rejects_invalid_planner() {
        let dir = create_temp_project_root("toml_invalid_planner");
        write_toml(&dir, "planner = \"gpt4\"\n");
        let err = load_file_config(&dir).expect_err("invalid planner should fail");
        let msg = err.to_string();
        assert!(msg.contains("invalid planner"), "got: {msg}");
        assert!(msg.contains("gpt4"), "got: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_planner_model_applied_to_planner_agent() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_planner_model");
        write_toml(&dir, "planner = \"codex\"\nplanner_model = \"o3\"\n");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.planner.name(), "codex");
        assert_eq!(config.planner.model(), Some("o3"));
        // Implementer should be unaffected
        assert_eq!(config.implementer.name(), "claude");
        assert_eq!(config.implementer.model(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_default_planner_inherits_implementer_model_when_planner_model_unset() {
        let _guard = env_lock();
        clear_env();
        set_env("IMPLEMENTER_MODEL", "claude-sonnet-4-6");

        let dir = create_temp_project_root("cfg_planner_inherit_model");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        // Planner defaults to implementer and inherits its model
        assert_eq!(config.planner.name(), "claude");
        assert_eq!(config.planner.model(), Some("claude-sonnet-4-6"));
        assert_eq!(config.implementer.model(), Some("claude-sonnet-4-6"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_explicit_planner_does_not_inherit_implementer_model() {
        let _guard = env_lock();
        clear_env();
        set_env("IMPLEMENTER_MODEL", "claude-sonnet-4-6");

        let dir = create_temp_project_root("cfg_planner_no_inherit");
        write_toml(&dir, "planner = \"codex\"\n");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        // Explicit planner should NOT inherit implementer model
        assert_eq!(config.planner.name(), "codex");
        assert_eq!(config.planner.model(), None);
        // Implementer still has its model
        assert_eq!(config.implementer.model(), Some("claude-sonnet-4-6"));
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
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(config.auto_commit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_only_auto_commit_non_zero_is_true() {
        let _guard = env_lock();
        clear_env();
        set_env("AUTO_COMMIT", "anything");

        let dir = create_temp_project_root("cfg_ac_nonzero");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(config.auto_commit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Config bounds validation
    // -----------------------------------------------------------------------

    #[test]
    fn zero_planning_max_rounds_is_valid_unlimited() {
        let _guard = env_lock();
        clear_env();
        set_env("PLANNING_MAX_ROUNDS", "0");

        let dir = create_temp_project_root("cfg_zero_planning");
        let config =
            Config::from_cli(dir.clone(), false, false, None).expect("0 should be valid (unlimited)");
        assert_eq!(config.planning_max_rounds, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_decomposition_max_rounds_is_valid_unlimited() {
        let _guard = env_lock();
        clear_env();
        set_env("DECOMPOSITION_MAX_ROUNDS", "0");

        let dir = create_temp_project_root("cfg_zero_decomp");
        let config =
            Config::from_cli(dir.clone(), false, false, None).expect("0 should be valid (unlimited)");
        assert_eq!(config.decomposition_max_rounds, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_zero_timeout() {
        let _guard = env_lock();
        clear_env();
        set_env("TIMEOUT", "0");

        let dir = create_temp_project_root("cfg_zero_timeout");
        let err = Config::from_cli(dir.clone(), false, false, None).expect_err("timeout=0 should fail");
        assert!(err.to_string().contains("timeout must be > 0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_zero_decisions_max_lines() {
        let _guard = env_lock();
        clear_env();
        set_env("DECISIONS_ENABLED", "true");
        set_env("DECISIONS_MAX_LINES", "0");

        let dir = create_temp_project_root("cfg_zero_decisions_max_lines");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("decisions_max_lines=0 should fail");
        assert!(err.to_string().contains("decisions_max_lines must be > 0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_compound_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_compound_env_overrides_toml");
        write_toml(&dir, "compound = true\n");
        set_env("COMPOUND", "0");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(!config.compound);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_decisions_max_lines_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_decisions_lines_env_overrides_toml");
        write_toml(&dir, "decisions_max_lines = 20\n");
        set_env("DECISIONS_MAX_LINES", "80");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.decisions_max_lines, 80);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_batch_implement_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_batch_implement_env_overrides_toml");
        write_toml(&dir, "batch_implement = true\n");
        set_env("BATCH_IMPLEMENT", "0");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(!config.batch_implement);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_planner_permission_mode_defaults_to_default() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_planner_perm_default");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.planner_permission_mode, "default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_planner_permission_mode_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_planner_perm_env_overrides_toml");
        write_toml(&dir, "planner_permission_mode = \"default\"\n");
        set_env("PLANNER_PERMISSION_MODE", "plan");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.planner_permission_mode, "plan");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_rejects_invalid_planner_permission_mode() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_invalid_planner_perm");
        write_toml(&dir, "planner_permission_mode = \"invalid\"\n");

        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("invalid planner_permission_mode should fail");
        assert!(
            err.to_string()
                .contains("planner_permission_mode must be one of")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_parses_quality_commands_with_and_without_remediation() {
        let dir = create_temp_project_root("toml_quality_commands");
        write_toml(
            &dir,
            r#"
[[quality_commands]]
command = "cargo clippy -- -D warnings"
remediation = "Fix clippy warnings."

[[quality_commands]]
command = "cargo test"
"#,
        );

        let result = load_file_config(&dir).expect("quality_commands should parse");
        assert!(result.file_found);
        let quality_commands = result
            .config
            .quality_commands
            .expect("quality_commands should exist");
        assert_eq!(quality_commands.len(), 2);
        assert_eq!(quality_commands[0].command, "cargo clippy -- -D warnings");
        assert_eq!(
            quality_commands[0].remediation.as_deref(),
            Some("Fix clippy warnings.")
        );
        assert_eq!(quality_commands[1].command, "cargo test");
        assert_eq!(quality_commands[1].remediation, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_passes_for_valid_defaults() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_valid_defaults");
        let config =
            Config::from_cli(dir.clone(), false, false, None).expect("default config should be valid");
        assert_eq!(config.review_max_rounds, 0); // unlimited by default
        assert_eq!(config.planning_max_rounds, 0); // unlimited by default
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
        let result = load_file_config(&dir).expect("should parse new-only fields");
        assert!(result.file_found);
        assert_eq!(result.config.diff_max_lines, Some(300));
        assert_eq!(result.config.context_line_cap, Some(150));
        assert_eq!(result.config.planning_context_excerpt_lines, Some(75));
        // Other fields remain None
        assert!(result.config.review_max_rounds.is_none());
        assert!(result.config.implementer.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_new_fields_default_to_none() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_new_fields_default");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.diff_max_lines, None);
        assert_eq!(config.context_line_cap, None);
        assert_eq!(config.planning_context_excerpt_lines, None);
        // Effective helpers return defaults (0 = unlimited → u32::MAX)
        assert_eq!(config.effective_diff_max_lines(), DEFAULT_DIFF_MAX_LINES);
        assert_eq!(config.effective_context_line_cap(), u32::MAX);
        assert_eq!(config.effective_planning_context_excerpt_lines(), u32::MAX);
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
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
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

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
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

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
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

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
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
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(!config.verbose);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_verbose_flag_enables_verbose() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_verbose_flag");
        let config = Config::from_cli(dir.clone(), false, true, None).expect("from_cli should succeed");
        assert!(config.verbose);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_verbose_env_enables_verbose_when_flag_is_absent() {
        let _guard = env_lock();
        clear_env();
        set_env("VERBOSE", "1");

        let dir = create_temp_project_root("cfg_verbose_env");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(config.verbose);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_verbose_flag_overrides_falsy_env() {
        let _guard = env_lock();
        clear_env();
        set_env("VERBOSE", "0");

        let dir = create_temp_project_root("cfg_verbose_flag_over_env");
        let config = Config::from_cli(dir.clone(), false, true, None).expect("from_cli should succeed");
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
        let result = load_file_config(&dir).expect("max_parallel should parse");
        assert!(result.file_found);
        assert_eq!(result.config.max_parallel, Some(4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_max_parallel_defaults_to_1() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_max_parallel_default");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.max_parallel, DEFAULT_MAX_PARALLEL);
        assert_eq!(config.max_parallel, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_env_overrides_max_parallel() {
        let _guard = env_lock();
        clear_env();
        set_env("MAX_PARALLEL", "8");

        let dir = create_temp_project_root("cfg_env_max_parallel");
        write_toml(&dir, "max_parallel = 4\n");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.max_parallel, 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_toml_overrides_max_parallel() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_toml_max_parallel");
        write_toml(&dir, "max_parallel = 4\n");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(config.max_parallel, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_zero_max_parallel_in_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_zero_max_parallel");
        write_toml(&dir, "max_parallel = 0\n");
        let err =
            Config::from_cli(dir.clone(), false, false, None).expect_err("max_parallel=0 should fail");
        assert!(err.to_string().contains("max_parallel must be >= 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Invalid agent string validation in TOML
    // -----------------------------------------------------------------------

    #[test]
    fn load_file_config_rejects_invalid_implementer() {
        let dir = create_temp_project_root("toml_invalid_implementer");
        write_toml(&dir, "implementer = \"invalid-agent\"\n");
        let err = load_file_config(&dir).expect_err("invalid implementer should fail");
        let msg = err.to_string();
        assert!(msg.contains("invalid implementer"), "got: {msg}");
        assert!(msg.contains("invalid-agent"), "got: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_rejects_invalid_reviewer() {
        let dir = create_temp_project_root("toml_invalid_reviewer");
        write_toml(&dir, "reviewer = \"gpt4\"\n");
        let err = load_file_config(&dir).expect_err("invalid reviewer should fail");
        let msg = err.to_string();
        assert!(msg.contains("invalid reviewer"), "got: {msg}");
        assert!(msg.contains("gpt4"), "got: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_accepts_valid_agent_values() {
        let dir = create_temp_project_root("toml_valid_agents");
        write_toml(
            &dir,
            "implementer = \"claude\"\nreviewer = \"codex\"\nplanner = \"codex\"\n",
        );
        let result = load_file_config(&dir).expect("valid agents should parse");
        assert!(result.file_found);
        assert_eq!(result.config.implementer.as_deref(), Some("claude"));
        assert_eq!(result.config.reviewer.as_deref(), Some("codex"));
        assert_eq!(result.config.planner.as_deref(), Some("codex"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_rejects_invalid_implementer_in_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_invalid_impl");
        write_toml(&dir, "implementer = \"typo-agent\"\n");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("invalid implementer should fail from_cli");
        assert!(err.to_string().contains("invalid implementer"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_cli_rejects_invalid_reviewer_in_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_invalid_rev");
        write_toml(&dir, "reviewer = \"gpt4\"\n");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("invalid reviewer should fail from_cli");
        assert!(err.to_string().contains("invalid reviewer"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // from_cli_with_overrides — pre-validation override semantics
    // -----------------------------------------------------------------------

    #[test]
    fn overrides_review_max_rounds_applied_before_validation() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_override_mr");
        let config = Config::from_cli_with_overrides(dir.clone(), false, false, Some(42), None)
            .expect("override review_max_rounds should succeed");
        assert_eq!(config.review_max_rounds, 42);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overrides_review_max_rounds_wins_over_env_and_toml() {
        let _guard = env_lock();
        clear_env();
        set_env("REVIEW_MAX_ROUNDS", "100");

        let dir = create_temp_project_root("cfg_override_mr_env");
        write_toml(&dir, "review_max_rounds = 50\n");

        let config = Config::from_cli_with_overrides(dir.clone(), false, false, Some(7), None)
            .expect("override review_max_rounds should win");
        assert_eq!(config.review_max_rounds, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // generate_default_config_template
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Missing-config hint: should_emit_missing_config_hint (pure logic)
    // -----------------------------------------------------------------------

    #[test]
    fn hint_suppressed_when_config_file_found() {
        assert!(
            !should_emit_missing_config_hint(true, false, true),
            "hint should not emit when config file exists"
        );
    }

    #[test]
    fn hint_suppressed_when_ci_set_even_with_terminal() {
        // This is the key test the reviewer flagged: integration tests cannot
        // exercise the CI branch because Command::output() pipes stderr
        // (non-TTY). Here we explicitly pass is_terminal=true to isolate the
        // CI guard.
        assert!(
            !should_emit_missing_config_hint(false, true, true),
            "hint should not emit when CI is set, even with a real terminal"
        );
    }

    #[test]
    fn hint_suppressed_when_not_terminal() {
        assert!(
            !should_emit_missing_config_hint(false, false, false),
            "hint should not emit when stderr is not a terminal"
        );
    }

    #[test]
    fn hint_emits_when_all_conditions_met() {
        assert!(
            should_emit_missing_config_hint(false, false, true),
            "hint should emit when no config, no CI, and terminal"
        );
    }

    // -----------------------------------------------------------------------
    // Missing-config hint: try_emit (one-time guard semantics)
    // -----------------------------------------------------------------------

    #[test]
    fn one_time_guard_allows_first_call_only() {
        use std::sync::atomic::AtomicBool;

        let guard = AtomicBool::new(false);

        // First call with eligible conditions: should emit
        assert!(
            try_emit_missing_config_hint(false, false, true, &guard),
            "first eligible call should return true"
        );
        // Second call: guard blocks
        assert!(
            !try_emit_missing_config_hint(false, false, true, &guard),
            "second call should be blocked by one-time guard"
        );
        // Third call: still blocked
        assert!(
            !try_emit_missing_config_hint(false, false, true, &guard),
            "subsequent calls should remain blocked"
        );
    }

    #[test]
    fn one_time_guard_not_consumed_when_conditions_fail() {
        use std::sync::atomic::AtomicBool;

        let guard = AtomicBool::new(false);

        // Call with CI set — should not emit AND should not consume the guard
        assert!(
            !try_emit_missing_config_hint(false, true, true, &guard),
            "CI suppression should prevent emission"
        );
        // Now call with eligible conditions — guard should still allow
        assert!(
            try_emit_missing_config_hint(false, false, true, &guard),
            "guard should not have been consumed by the failed CI call"
        );
    }

    // -----------------------------------------------------------------------
    // generate_default_config_template
    // -----------------------------------------------------------------------

    #[test]
    fn generate_default_config_template_contains_sections_and_defaults() {
        let template = generate_default_config_template();

        // Section markers
        assert!(template.contains("# ── Core"), "missing Core section");
        assert!(template.contains("# ── Agents"), "missing Agents section");
        assert!(
            template.contains("# ── Model selection"),
            "missing Model selection section"
        );
        assert!(
            template.contains("# ── Claude CLI tuning"),
            "missing Claude CLI tuning section"
        );
        assert!(
            template.contains("# ── Codex CLI tuning"),
            "missing Codex CLI tuning section"
        );
        assert!(
            template.contains("# ── Stuck detection"),
            "missing Stuck detection section"
        );
        assert!(
            template.contains("# ── Wave runtime"),
            "missing Wave runtime section"
        );
        assert!(
            template.contains("# ── Quality commands"),
            "missing Quality commands section"
        );

        // DEFAULT_* constant values
        assert!(
            template.contains(&format!(
                "# review_max_rounds = {}",
                DEFAULT_REVIEW_MAX_ROUNDS
            )),
            "missing DEFAULT_REVIEW_MAX_ROUNDS"
        );
        assert!(
            template.contains(&format!(
                "# planning_max_rounds = {}",
                DEFAULT_PLANNING_MAX_ROUNDS
            )),
            "missing DEFAULT_PLANNING_MAX_ROUNDS"
        );
        assert!(
            template.contains(&format!("# timeout = {}", DEFAULT_TIMEOUT_SECONDS)),
            "missing DEFAULT_TIMEOUT_SECONDS"
        );
        assert!(
            template.contains(&format!(
                "# decisions_max_lines = {}",
                DEFAULT_DECISIONS_MAX_LINES
            )),
            "missing DEFAULT_DECISIONS_MAX_LINES"
        );
        assert!(
            template.contains(&format!(
                "# stuck_no_diff_rounds = {}",
                DEFAULT_STUCK_NO_DIFF_ROUNDS
            )),
            "missing DEFAULT_STUCK_NO_DIFF_ROUNDS"
        );
        assert!(
            template.contains(DEFAULT_CLAUDE_ALLOWED_TOOLS),
            "missing DEFAULT_CLAUDE_ALLOWED_TOOLS"
        );
        assert!(
            template.contains(DEFAULT_REVIEWER_ALLOWED_TOOLS),
            "missing DEFAULT_REVIEWER_ALLOWED_TOOLS"
        );
        assert!(
            template.contains("# planner_permission_mode = \"default\""),
            "missing planner_permission_mode default line"
        );
        assert!(
            template.contains("# planner = \"claude\""),
            "missing planner agent default line"
        );

        // planning_adversarial_review should appear in the template
        assert!(
            template.contains("planning_adversarial_review"),
            "missing planning_adversarial_review in template"
        );

        // All value lines should be commented out
        for line in template.lines() {
            let trimmed = line.trim();
            // Skip blank lines, section headers, and comment-only lines
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            panic!("found uncommented value line: {trimmed}");
        }
    }

    #[test]
    fn planning_adversarial_review_defaults_to_true() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_adversarial_default");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(
            config.planning_adversarial_review,
            "planning_adversarial_review should default to true"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn planning_adversarial_review_can_be_disabled_via_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_adversarial_toml");
        write_toml(&dir, "planning_adversarial_review = false\n");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(
            !config.planning_adversarial_review,
            "planning_adversarial_review should be disabled via TOML"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn planning_adversarial_review_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_adversarial_env");
        write_toml(&dir, "planning_adversarial_review = true\n");
        set_env("PLANNING_ADVERSARIAL_REVIEW", "0");

        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(
            !config.planning_adversarial_review,
            "env PLANNING_ADVERSARIAL_REVIEW=0 should override TOML"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn effective_context_line_cap_treats_zero_as_unlimited() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_ctx_zero");
        write_toml(&dir, "context_line_cap = 0\n");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(
            config.effective_context_line_cap(),
            u32::MAX,
            "context_line_cap = 0 should map to u32::MAX"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn effective_planning_context_excerpt_lines_treats_zero_as_unlimited() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_excerpt_zero");
        write_toml(&dir, "planning_context_excerpt_lines = 0\n");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert_eq!(
            config.effective_planning_context_excerpt_lines(),
            u32::MAX,
            "planning_context_excerpt_lines = 0 should map to u32::MAX"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Migration error: old max_rounds TOML key
    // -----------------------------------------------------------------------

    #[test]
    fn toml_max_rounds_rejected_with_rename_guidance() {
        let dir = create_temp_project_root("toml_old_max_rounds");
        write_toml(&dir, "max_rounds = 10\n");
        let err = load_file_config(&dir).expect_err("old max_rounds key should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("renamed to `review_max_rounds`"),
            "expected rename guidance, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_max_rounds_rejected_with_rename_guidance() {
        let _guard = env_lock();
        clear_env();
        set_env("MAX_ROUNDS", "10");

        let dir = create_temp_project_root("cfg_old_max_rounds_env");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("old MAX_ROUNDS env should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("renamed to `REVIEW_MAX_ROUNDS`"),
            "expected rename guidance, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_file_config_catches_max_rounds_as_safety_net() {
        // The pre-parse TOML table scan in load_file_config normally catches
        // the legacy `max_rounds` key before serde deserializes. This test
        // exercises the validate_file_config safety net directly, proving the
        // belt-and-suspenders path produces the same rename guidance even if
        // the pre-parse is bypassed.
        let dir = create_temp_project_root("toml_validate_max_rounds");
        let config = FileConfig {
            max_rounds: Some(10),
            ..Default::default()
        };
        let path = dir.join(CONFIG_FILE_NAME);
        let err =
            validate_file_config(&config, &path).expect_err("max_rounds should fail validation");
        let msg = err.to_string();
        assert!(
            msg.contains("renamed to `review_max_rounds`"),
            "expected rename guidance from validate_file_config, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Full-access defaults
    // -----------------------------------------------------------------------

    #[test]
    fn full_access_defaults_to_true_for_claude_and_codex() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_full_access_default");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(
            config.claude_full_access,
            "claude_full_access should default to true"
        );
        assert!(
            config.codex_full_access,
            "codex_full_access should default to true"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_access_can_be_disabled_via_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_full_access_toml");
        write_toml(
            &dir,
            "claude_full_access = false\ncodex_full_access = false\n",
        );
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(!config.claude_full_access);
        assert!(!config.codex_full_access);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_access_env_overrides_toml() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_full_access_env");
        write_toml(
            &dir,
            "claude_full_access = true\ncodex_full_access = true\n",
        );
        set_env("CLAUDE_FULL_ACCESS", "0");
        set_env("CODEX_FULL_ACCESS", "0");
        let config = Config::from_cli(dir.clone(), false, false, None).expect("from_cli should succeed");
        assert!(!config.claude_full_access);
        assert!(!config.codex_full_access);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Template: full-access and review_max_rounds
    // -----------------------------------------------------------------------

    #[test]
    fn template_contains_full_access_true_defaults() {
        let template = generate_default_config_template();
        assert!(
            template.contains("# claude_full_access = true"),
            "template should show claude_full_access = true"
        );
        assert!(
            template.contains("# codex_full_access = true"),
            "template should show codex_full_access = true"
        );
    }

    #[test]
    fn template_contains_review_max_rounds_not_max_rounds() {
        let template = generate_default_config_template();
        assert!(
            template.contains("# review_max_rounds = 0"),
            "template should contain review_max_rounds = 0"
        );
        assert!(
            !template.contains("# max_rounds ="),
            "template should not contain old max_rounds key"
        );
    }

    #[test]
    fn template_contains_round_limit_note() {
        let template = generate_default_config_template();
        assert!(
            template.contains("Round limits: 0 = unlimited"),
            "template should contain round limit note"
        );
    }

    #[test]
    fn sample_toml_matches_generated_template() {
        // The repository-root `.agent-loop.toml` sample must stay in sync with
        // generate_default_config_template(). This test fails when the template
        // is updated but the sample is not (or vice versa).
        let template = generate_default_config_template();
        let sample_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".agent-loop.toml");
        let sample = std::fs::read_to_string(&sample_path).unwrap_or_else(|err| {
            panic!(
                "failed to read sample .agent-loop.toml at {}: {err}",
                sample_path.display()
            )
        });
        assert_eq!(
            template.trim(),
            sample.trim(),
            "sample .agent-loop.toml and generate_default_config_template() have drifted"
        );
    }

    #[test]
    fn sample_toml_is_valid_parseable_config() {
        // Verify the sample .agent-loop.toml parses as valid TOML that serde
        // can deserialize to FileConfig. Catches stale keys or type mismatches.
        let sample_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".agent-loop.toml");
        let content = std::fs::read_to_string(&sample_path).unwrap_or_else(|err| {
            panic!(
                "failed to read sample .agent-loop.toml at {}: {err}",
                sample_path.display()
            )
        });
        // All lines are comments, so it should deserialize to a default FileConfig.
        let config: FileConfig = toml::from_str(&content).unwrap_or_else(|err| {
            panic!("sample .agent-loop.toml is not valid FileConfig TOML: {err}")
        });
        // All comment-only config should yield None for everything.
        assert!(
            config.review_max_rounds.is_none(),
            "commented-out sample should not set review_max_rounds"
        );
        assert!(
            config.implementer.is_none(),
            "commented-out sample should not set implementer"
        );
    }

    // -----------------------------------------------------------------------
    // from_cli_with_overrides: unlimited (0) override
    // -----------------------------------------------------------------------

    #[test]
    fn overrides_review_max_rounds_zero_is_valid_unlimited() {
        let _guard = env_lock();
        clear_env();
        // Even when env/TOML set a finite limit, override with Some(0) should
        // take highest precedence and set unlimited.
        set_env("REVIEW_MAX_ROUNDS", "20");

        let dir = create_temp_project_root("cfg_override_mr_zero");
        let config = Config::from_cli_with_overrides(dir.clone(), false, false, Some(0), None)
            .expect("override review_max_rounds=0 should succeed");
        assert_eq!(
            config.review_max_rounds, 0,
            "Some(0) override should set unlimited (0)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Full-access warning: should_emit_full_access_warning (pure logic)
    // -----------------------------------------------------------------------

    #[test]
    fn full_access_warning_fires_when_defaults_active() {
        assert!(
            should_emit_full_access_warning(true, false, true, false, false, true),
            "warning should fire when both full-access on and not explicit"
        );
    }

    #[test]
    fn full_access_warning_suppressed_when_claude_explicit() {
        assert!(
            !should_emit_full_access_warning(true, true, true, true, false, true),
            "warning should be suppressed when user explicitly set values"
        );
    }

    #[test]
    fn full_access_warning_suppressed_in_ci() {
        assert!(
            !should_emit_full_access_warning(true, false, true, false, true, true),
            "warning should be suppressed in CI"
        );
    }

    #[test]
    fn full_access_warning_suppressed_when_not_terminal() {
        assert!(
            !should_emit_full_access_warning(true, false, true, false, false, false),
            "warning should be suppressed when stderr is not a terminal"
        );
    }

    #[test]
    fn full_access_warning_fires_when_only_claude_implicit() {
        // codex explicit but claude still implicit
        assert!(
            should_emit_full_access_warning(true, false, true, true, false, true),
            "warning should fire when at least one agent uses implicit full-access"
        );
    }

    #[test]
    fn full_access_warning_suppressed_when_both_disabled() {
        assert!(
            !should_emit_full_access_warning(false, false, false, false, false, true),
            "warning should not fire when full-access is off"
        );
    }

    #[test]
    fn full_access_warning_one_time_guard() {
        use std::sync::atomic::AtomicBool;

        let guard = AtomicBool::new(false);
        assert!(
            try_emit_full_access_warning(true, false, true, false, false, true, &guard),
            "first call should emit"
        );
        assert!(
            !try_emit_full_access_warning(true, false, true, false, false, true, &guard),
            "second call should be blocked by guard"
        );
    }

    // -----------------------------------------------------------------------
    // F-001: Invalid round-limit env vars fail with config errors
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_review_max_rounds_env_fails_with_config_error() {
        let _guard = env_lock();
        clear_env();
        set_env("REVIEW_MAX_ROUNDS", "not-a-number");

        let dir = create_temp_project_root("cfg_invalid_review_env");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("invalid REVIEW_MAX_ROUNDS env should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid value 'not-a-number' for REVIEW_MAX_ROUNDS"),
            "expected strict parse error, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_planning_max_rounds_env_fails_with_config_error() {
        let _guard = env_lock();
        clear_env();
        set_env("PLANNING_MAX_ROUNDS", "-1");

        let dir = create_temp_project_root("cfg_invalid_planning_env");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("invalid PLANNING_MAX_ROUNDS env should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid value '-1' for PLANNING_MAX_ROUNDS"),
            "expected strict parse error, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_decomposition_max_rounds_env_fails_with_config_error() {
        let _guard = env_lock();
        clear_env();
        set_env("DECOMPOSITION_MAX_ROUNDS", "abc");

        let dir = create_temp_project_root("cfg_invalid_decomp_env");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("invalid DECOMPOSITION_MAX_ROUNDS env should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid value 'abc' for DECOMPOSITION_MAX_ROUNDS"),
            "expected strict parse error, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_round_limit_env_values_still_work() {
        let _guard = env_lock();
        clear_env();
        set_env("REVIEW_MAX_ROUNDS", "10");
        set_env("PLANNING_MAX_ROUNDS", "5");
        set_env("DECOMPOSITION_MAX_ROUNDS", "0");

        let dir = create_temp_project_root("cfg_valid_round_env");
        let config = Config::from_cli(dir.clone(), false, false, None)
            .expect("valid round env values should succeed");
        assert_eq!(config.review_max_rounds, 10);
        assert_eq!(config.planning_max_rounds, 5);
        assert_eq!(config.decomposition_max_rounds, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // F-002: Legacy max_rounds TOML rename guidance for non-u32 values
    // -----------------------------------------------------------------------

    #[test]
    fn toml_max_rounds_negative_value_gives_rename_guidance() {
        let dir = create_temp_project_root("toml_old_max_rounds_neg");
        write_toml(&dir, "max_rounds = -1\n");
        let err = load_file_config(&dir).expect_err("legacy max_rounds = -1 should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("renamed to `review_max_rounds`"),
            "expected rename guidance even for non-u32 value, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_max_rounds_string_value_gives_rename_guidance() {
        let dir = create_temp_project_root("toml_old_max_rounds_str");
        write_toml(&dir, "max_rounds = \"foo\"\n");
        let err = load_file_config(&dir).expect_err("legacy max_rounds = \"foo\" should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("renamed to `review_max_rounds`"),
            "expected rename guidance even for string value, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Template: constraint comments
    // -----------------------------------------------------------------------

    #[test]
    fn template_contains_claude_constraint_comment() {
        let template = generate_default_config_template();
        assert!(
            template.contains("If you need constraints, set claude_full_access = false"),
            "template should contain Claude constraint comment"
        );
    }

    #[test]
    fn template_contains_codex_constraint_comment() {
        let template = generate_default_config_template();
        assert!(
            template.contains("If you need reduced permissions, set codex_full_access = false"),
            "template should contain Codex constraint comment"
        );
    }

    // -----------------------------------------------------------------------
    // TOML rejection of negative values for NEW round-limit keys
    // -----------------------------------------------------------------------

    #[test]
    fn toml_negative_review_max_rounds_rejected() {
        // TOML `-1` is a valid TOML integer, but serde cannot deserialize it
        // to Option<u32>. Ensure a clear parse error is produced.
        let dir = create_temp_project_root("toml_neg_review_mr");
        write_toml(&dir, "review_max_rounds = -1\n");
        let err = load_file_config(&dir).expect_err("review_max_rounds = -1 should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse"),
            "expected parse error for negative u32, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_negative_planning_max_rounds_rejected() {
        let dir = create_temp_project_root("toml_neg_planning_mr");
        write_toml(&dir, "planning_max_rounds = -5\n");
        let err = load_file_config(&dir).expect_err("planning_max_rounds = -5 should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse"),
            "expected parse error for negative u32, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_negative_decomposition_max_rounds_rejected() {
        let dir = create_temp_project_root("toml_neg_decomp_mr");
        write_toml(&dir, "decomposition_max_rounds = -10\n");
        let err = load_file_config(&dir).expect_err("decomposition_max_rounds = -10 should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse"),
            "expected parse error for negative u32, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Env: negative numeric values rejected by strict_parse_env
    // -----------------------------------------------------------------------

    #[test]
    fn from_cli_negative_review_max_rounds_env_fails() {
        let _guard = env_lock();
        clear_env();
        set_env("REVIEW_MAX_ROUNDS", "-5");

        let dir = create_temp_project_root("cfg_neg_review_env");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("REVIEW_MAX_ROUNDS=-5 should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid value '-5' for REVIEW_MAX_ROUNDS"),
            "expected strict parse error for negative, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // decisions_enabled / decisions_auto_reference / transcript_enabled
    // -----------------------------------------------------------------------

    #[test]
    fn decisions_enabled_defaults_to_false() {
        let _guard = env_lock();
        clear_env();
        let dir = create_temp_project_root("cfg_decisions_default");
        let config = Config::from_cli(dir.clone(), false, false, None).unwrap();
        assert!(!config.decisions_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decisions_enabled_env_override() {
        let _guard = env_lock();
        clear_env();
        set_env("DECISIONS_ENABLED", "true");

        let dir = create_temp_project_root("cfg_decisions_env");
        let config = Config::from_cli(dir.clone(), false, false, None).unwrap();
        assert!(config.decisions_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decisions_auto_reference_env_override() {
        let _guard = env_lock();
        clear_env();
        set_env("DECISIONS_AUTO_REFERENCE", "0");

        let dir = create_temp_project_root("cfg_decisions_ref_env");
        let config = Config::from_cli(dir.clone(), false, false, None).unwrap();
        assert!(!config.decisions_auto_reference);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decisions_enabled_toml_override() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_decisions_toml");
        write_toml(&dir, "decisions_enabled = true\n");
        let config = Config::from_cli(dir.clone(), false, false, None).unwrap();
        assert!(config.decisions_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decisions_max_lines_zero_accepted_when_decisions_disabled() {
        let _guard = env_lock();
        clear_env();
        set_env("DECISIONS_ENABLED", "false");
        set_env("DECISIONS_MAX_LINES", "0");

        let dir = create_temp_project_root("cfg_decisions_zero_ok");
        let config = Config::from_cli(dir.clone(), false, false, None).unwrap();
        assert_eq!(config.decisions_max_lines, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decisions_max_lines_zero_rejected_when_decisions_enabled() {
        let _guard = env_lock();
        clear_env();
        set_env("DECISIONS_ENABLED", "true");
        set_env("DECISIONS_MAX_LINES", "0");

        let dir = create_temp_project_root("cfg_decisions_zero_err");
        let err = Config::from_cli(dir.clone(), false, false, None)
            .expect_err("decisions_max_lines=0 with decisions_enabled should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("decisions_max_lines must be > 0"),
            "got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_enabled_defaults_to_true() {
        let _guard = env_lock();
        clear_env();
        let dir = create_temp_project_root("cfg_transcript_default");
        let config = Config::from_cli(dir.clone(), false, false, None).unwrap();
        assert!(config.transcript_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_enabled_env_override() {
        let _guard = env_lock();
        clear_env();
        set_env("TRANSCRIPT_ENABLED", "true");

        let dir = create_temp_project_root("cfg_transcript_env");
        let config = Config::from_cli(dir.clone(), false, false, None).unwrap();
        assert!(config.transcript_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_enabled_toml_override() {
        let _guard = env_lock();
        clear_env();

        let dir = create_temp_project_root("cfg_transcript_toml");
        write_toml(&dir, "transcript_enabled = true\n");
        let config = Config::from_cli(dir.clone(), false, false, None).unwrap();
        assert!(config.transcript_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
