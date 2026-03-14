mod agent;
mod agent_registry;
mod config;
mod error;
mod git;
mod interrupt;
mod phases;
mod preflight;
mod prompts;
mod state;
mod stuck;
#[cfg(test)]
mod test_support;
mod wave;
mod wave_runtime;

use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process,
    time::Instant,
};

use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use config::{
    Config, DEFAULT_DECOMPOSITION_MAX_ROUNDS, DEFAULT_DIFF_MAX_LINES, DEFAULT_PLANNING_MAX_ROUNDS,
    DEFAULT_REVIEW_MAX_ROUNDS, DEFAULT_TIMEOUT_SECONDS,
};
use error::AgentLoopError;
use serde::{Deserialize, Serialize};
use state::{
    LoopStatus, Status, StatusPatch, TASKS_FINDINGS_FILENAME, TaskMetricsEntry, TaskMetricsFile,
    TaskRunStatus, TaskStatusEntry, TaskStatusFile,
};

const KNOWN_SUBCOMMANDS: [&str; 11] = [
    "plan",
    "tasks",
    "implement",
    "plan-tasks-implement",
    "plan-implement",
    "tasks-implement",
    "reset",
    "status",
    "version",
    "help",
    "config",
];

#[derive(Debug, Parser)]
#[command(
    name = "agent-loop",
    version,
    about = "Run a collaborative implementation/review loop between coding agents."
)]
struct Cli {
    #[arg(long, global = true, value_name = "NAME")]
    session: Option<String>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Plan only
    Plan(PlanArgs),
    /// Decompose plan into tasks only
    Tasks(TasksArgs),
    /// Implement from tasks.md, inline task text, or task file
    Implement(ImplementArgs),
    /// Run plan -> tasks -> implement end-to-end
    #[command(name = "plan-tasks-implement")]
    PlanTasksImplement(PlanTasksImplementArgs),
    /// Run plan -> implement (skip task decomposition)
    #[command(name = "plan-implement")]
    PlanImplement(PlanImplementArgs),
    /// Run tasks -> implement (skip planning, assumes plan.md exists)
    #[command(name = "tasks-implement")]
    TasksImplement(TasksImplementArgs),
    /// Clear .agent-loop/state while preserving decisions.md
    Reset(ResetArgs),
    /// Show current loop status
    Status,
    /// Print version
    Version,
    /// Configuration management
    #[command(name = "config")]
    ConfigCmd {
        #[command(subcommand)]
        action: ConfigCommands,
    },
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct ResetArgs {
    /// Only remove the wave.lock file (force-clear a stale wave lock)
    #[arg(long)]
    wave_lock: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct RunArgs {
    #[arg(value_name = "TASK")]
    task: Option<String>,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long, hide = true)]
    resume: bool,
    #[arg(long, hide = true)]
    planning_only: bool,
    #[arg(long)]
    single_agent: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct PlanArgs {
    #[arg(value_name = "TASK")]
    task: Option<String>,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    single_agent: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct ImplementModeFlags {
    #[arg(long)]
    per_task: bool,
    #[arg(long)]
    wave: bool,
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
    #[arg(long, default_value_t = 2)]
    round_step: u32,
    #[arg(long)]
    continue_on_fail: bool,
    #[arg(long)]
    fail_fast: bool,
    #[arg(long)]
    max_parallel: Option<u32>,
}

impl Default for ImplementModeFlags {
    fn default() -> Self {
        Self {
            per_task: false,
            wave: false,
            max_retries: 2,
            round_step: 2,
            continue_on_fail: false,
            fail_fast: false,
            max_parallel: None,
        }
    }
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct ImplementArgs {
    #[arg(long, value_name = "TASK")]
    task: Option<String>,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    single_agent: bool,
    #[command(flatten)]
    flags: ImplementModeFlags,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct PlanTasksImplementArgs {
    #[arg(value_name = "TASK")]
    task: Option<String>,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    single_agent: bool,
    #[command(flatten)]
    flags: ImplementModeFlags,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct PlanImplementArgs {
    #[arg(value_name = "TASK")]
    task: Option<String>,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    single_agent: bool,
    #[command(flatten)]
    flags: ImplementModeFlags,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct TasksImplementArgs {
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    single_agent: bool,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[command(flatten)]
    flags: ImplementModeFlags,
}

#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Generate default .agent-loop.toml
    Init(ConfigInitArgs),
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct ConfigInitArgs {
    /// Overwrite existing .agent-loop.toml
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct TasksArgs {
    #[arg(long)]
    resume: bool,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long, value_name = "PATH", hide = true)]
    tasks_file: Option<PathBuf>,
    #[arg(long)]
    single_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Dispatch {
    ShowHelp,
    Plan(PlanArgs),
    Tasks(TasksArgs),
    Implement(ImplementArgs),
    PlanTasksImplement(PlanTasksImplementArgs),
    PlanImplement(PlanImplementArgs),
    TasksImplement(TasksImplementArgs),
    Reset(ResetArgs),
    Status,
    Version,
    ConfigInit(ConfigInitArgs),
}

impl Dispatch {
    /// Returns `true` for commands that spawn agent subprocesses and need
    /// graceful SIGINT handling. Simple introspection commands return `false`
    /// so that the default (immediate termination) behavior is preserved.
    fn needs_signal_handlers(&self) -> bool {
        match self {
            Dispatch::Version | Dispatch::Status | Dispatch::ShowHelp
            | Dispatch::Reset(_) | Dispatch::ConfigInit(_) => false,
            Dispatch::Plan(_) | Dispatch::Tasks(_) | Dispatch::Implement(_)
            | Dispatch::PlanTasksImplement(_) | Dispatch::PlanImplement(_)
            | Dispatch::TasksImplement(_) => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplementExecutionMode {
    Batch,
    PerTask,
    Wave,
}

impl ImplementExecutionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Batch => "batch",
            Self::PerTask => "per-task",
            Self::Wave => "wave",
        }
    }
}

impl std::fmt::Display for ImplementExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ImplementExecutionMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "batch" => Ok(Self::Batch),
            "per-task" => Ok(Self::PerTask),
            "wave" => Ok(Self::Wave),
            other => Err(format!("unknown implementation mode: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeOrigin {
    User,
}

impl TasksArgs {
    fn validate(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

impl ImplementModeFlags {
    fn validate(
        &self,
        allow_user_per_task_resume: bool,
        resume: bool,
    ) -> Result<(), AgentLoopError> {
        if self.wave && self.per_task {
            return Err(AgentLoopError::Config(
                "--wave and --per-task cannot be used together.".to_string(),
            ));
        }

        if resume && self.per_task && !allow_user_per_task_resume {
            return Err(AgentLoopError::Config(
                "--per-task cannot be combined with --resume.".to_string(),
            ));
        }

        if self.round_step == 0 {
            return Err(AgentLoopError::Config(
                "--round-step must be at least 1.".to_string(),
            ));
        }

        if self.continue_on_fail && self.fail_fast {
            return Err(AgentLoopError::Config(
                "--continue-on-fail and --fail-fast cannot be used together.".to_string(),
            ));
        }

        if let Some(0) = self.max_parallel {
            return Err(AgentLoopError::Config(
                "--max-parallel must be at least 1.".to_string(),
            ));
        }

        Ok(())
    }
}

impl ImplementArgs {
    fn validate(&self) -> Result<(), AgentLoopError> {
        if self.task.is_some() && self.file.is_some() {
            return Err(AgentLoopError::Config(
                "--task and --file cannot be used together.".to_string(),
            ));
        }

        if self.resume && (self.task.is_some() || self.file.is_some()) {
            return Err(AgentLoopError::Config(
                "--resume cannot be combined with --task or --file.".to_string(),
            ));
        }

        if self.flags.per_task && (self.task.is_some() || self.file.is_some()) {
            return Err(AgentLoopError::Config(
                "--per-task cannot be combined with --task or --file.".to_string(),
            ));
        }

        if self.flags.wave && (self.task.is_some() || self.file.is_some()) {
            return Err(AgentLoopError::Config(
                "--wave cannot be combined with --task or --file.".to_string(),
            ));
        }

        self.flags.validate(false, self.resume)
    }
}

impl PlanTasksImplementArgs {
    fn validate(&self) -> Result<(), AgentLoopError> {
        if self.task.is_some() && self.file.is_some() {
            return Err(AgentLoopError::Config(
                "task and --file cannot be used together.".to_string(),
            ));
        }

        if self.resume && (self.task.is_some() || self.file.is_some()) {
            return Err(AgentLoopError::Config(
                "--resume cannot be combined with task or --file.".to_string(),
            ));
        }

        if !self.resume && self.task.is_none() && self.file.is_none() {
            return Err(AgentLoopError::Config(
                "Task is required. Provide task text or --file <path>.".to_string(),
            ));
        }

        self.flags.validate(true, self.resume)
    }
}

impl PlanImplementArgs {
    fn validate(&self) -> Result<(), AgentLoopError> {
        if self.task.is_some() && self.file.is_some() {
            return Err(AgentLoopError::Config(
                "task and --file cannot be used together.".to_string(),
            ));
        }

        if self.resume && (self.task.is_some() || self.file.is_some()) {
            return Err(AgentLoopError::Config(
                "--resume cannot be combined with task or --file.".to_string(),
            ));
        }

        if !self.resume && self.task.is_none() && self.file.is_none() {
            return Err(AgentLoopError::Config(
                "Task is required. Provide task text or --file <path>.".to_string(),
            ));
        }

        self.flags.validate(true, self.resume)
    }
}

impl TasksImplementArgs {
    fn validate(&self) -> Result<(), AgentLoopError> {
        if self.resume && self.file.is_some() {
            return Err(AgentLoopError::Config(
                "--resume cannot be combined with --file.".to_string(),
            ));
        }

        self.flags.validate(true, self.resume)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTask {
    title: String,
    content: String,
    dependencies: Vec<usize>,
}

#[derive(Debug)]
enum ParseOutcome {
    Parsed(Cli),
    Exit(i32),
}

fn main() {
    let exit_code = match run() {
        Ok(code) => code,
        Err(AgentLoopError::Interrupted(msg)) => {
            eprintln!("Interrupted: {msg}");
            130
        }
        Err(err) => {
            eprintln!("{err}");
            1
        }
    };

    process::exit(exit_code);
}

fn run() -> Result<i32, AgentLoopError> {
    let parse_outcome = parse_cli_from(std::env::args_os())?;
    match parse_outcome {
        ParseOutcome::Exit(code) => Ok(code),
        ParseOutcome::Parsed(cli) => {
            let session = cli.session.clone();
            let dispatch = dispatch_from_cli(cli)?;
            // Only register custom signal handlers for commands that spawn
            // agents and need graceful shutdown. Simple commands (version,
            // status, help, etc.) keep the default SIGINT behavior so Ctrl-C
            // terminates immediately.
            if dispatch.needs_signal_handlers() {
                interrupt::register_signal_handlers();
            }
            execute_dispatch(dispatch, session.as_deref())
        }
    }
}

fn parse_cli_from<I, S>(args: I) -> Result<ParseOutcome, AgentLoopError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let normalized_argv = normalize_argv(args);
    match Cli::try_parse_from(normalized_argv) {
        Ok(cli) => Ok(ParseOutcome::Parsed(cli)),
        Err(err) => match err.kind() {
            ErrorKind::DisplayHelp => {
                print!("{err}");
                println!();
                println!("{}", environment_help(None));
                Ok(ParseOutcome::Exit(0))
            }
            ErrorKind::DisplayVersion => {
                print!("{err}");
                Ok(ParseOutcome::Exit(0))
            }
            _ => Err(AgentLoopError::Config(err.to_string())),
        },
    }
}

fn dispatch_from_cli(cli: Cli) -> Result<Dispatch, AgentLoopError> {
    match cli.command {
        Some(Commands::Plan(args)) => Ok(Dispatch::Plan(args)),
        Some(Commands::Tasks(args)) => {
            if args.tasks_file.is_some() {
                return Err(AgentLoopError::Config(
                    "'--tasks-file' has been removed. Use '--file' instead.".to_string(),
                ));
            }
            Ok(Dispatch::Tasks(args))
        }
        Some(Commands::Implement(args)) => Ok(Dispatch::Implement(args)),
        Some(Commands::PlanTasksImplement(args)) => Ok(Dispatch::PlanTasksImplement(args)),
        Some(Commands::PlanImplement(args)) => Ok(Dispatch::PlanImplement(args)),
        Some(Commands::TasksImplement(args)) => Ok(Dispatch::TasksImplement(args)),
        Some(Commands::Reset(args)) => Ok(Dispatch::Reset(args)),
        Some(Commands::Status) => Ok(Dispatch::Status),
        Some(Commands::Version) => Ok(Dispatch::Version),
        Some(Commands::ConfigCmd { action }) => match action {
            ConfigCommands::Init(args) => Ok(Dispatch::ConfigInit(args)),
        },
        None => Ok(Dispatch::ShowHelp),
    }
}

fn execute_dispatch(dispatch: Dispatch, session: Option<&str>) -> Result<i32, AgentLoopError> {
    match dispatch {
        Dispatch::Plan(args) => plan_command(args, session),
        Dispatch::Tasks(args) => tasks_command(args, session),
        Dispatch::Implement(args) => implement_command(args, session),
        Dispatch::PlanTasksImplement(args) => plan_tasks_implement_command(args, session),
        Dispatch::PlanImplement(args) => plan_implement_command(args, session),
        Dispatch::TasksImplement(args) => tasks_implement_command(args, session),
        Dispatch::Reset(args) => reset_command(&args, session),
        Dispatch::Status => status_command(session),
        Dispatch::Version => version_command(),
        Dispatch::ConfigInit(args) => config_init_command(args),
        Dispatch::ShowHelp => {
            print_help_message(session)?;
            Ok(0)
        }
    }
}

fn normalize_argv<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let _ = KNOWN_SUBCOMMANDS.len();
    args.into_iter().map(Into::into).collect::<Vec<_>>()
}

fn print_help_message(session: Option<&str>) -> Result<(), AgentLoopError> {
    let mut command = Cli::command();
    command.print_long_help()?;
    println!();
    println!();
    println!("{}", environment_help(session));
    Ok(())
}

fn session_state_dir_rel(session: Option<&str>) -> String {
    match session {
        Some(name) => format!(".agent-loop/state/{name}"),
        None => ".agent-loop/state".to_string(),
    }
}

fn environment_help(session: Option<&str>) -> String {
    let state_rel = session_state_dir_rel(session);
    format!(
        "Primary commands:\n  agent-loop plan <task>                    Planning only\n  agent-loop plan --file <path>             Planning only from file\n  agent-loop plan --resume                  Resume planning from existing plan.md\n  agent-loop tasks                          Decompose only\n  agent-loop tasks --resume                 Resume decomposition\n  agent-loop implement                      Implement tasks.md in batch, or fall back to plan.md when tasks are missing/empty\n  agent-loop implement --per-task           Implement tasks one-by-one (legacy mode)\n  agent-loop implement --task <t>           Implement one inline task\n  agent-loop implement --file <p>           Implement one task from file\n  agent-loop implement --resume             Resume implementation\n  agent-loop plan-tasks-implement <task>    Run plan -> tasks -> implement\n  agent-loop plan-implement <task>          Run plan -> implement (supports all implement-mode flags)\n  agent-loop tasks-implement                Run tasks -> implement from state/plan.md or --file\n  agent-loop reset                          Clear {state_rel}/ and preserve decisions.md\n  agent-loop config init                    Generate default .agent-loop.toml\n\nConfiguration sources (highest precedence first):\n  1. CLI flags and subcommands\n  2. Environment variables\n  3. .agent-loop.toml (per-project config file)\n  4. Built-in defaults\n\nRound limits: 0 = unlimited (timeout and stuck detection remain active).\nImplementation review gates:\n  - single-agent: reviewer gate only\n  - dual-agent: reviewer gate (same-context) -> reviewer gate (fresh-context) -> implementer signoff\n  REVIEW_MAX_ROUNDS applies to the full implementation loop across all gates.\nProgress logs:\n  - planning-progress.md tracks planning rounds\n  - tasks-progress.md tracks decomposition rounds\n  - implement-progress.md is canonical in the active implementation state dir\n    (root state for batch / sequential, task-local {state_rel}/.wave-task-N/ for wave)\n\nEnvironment variables:\n  REVIEW_MAX_ROUNDS     (default: {DEFAULT_REVIEW_MAX_ROUNDS})   Max implementation/review rounds (0 = unlimited)\n  PLANNING_MAX_ROUNDS   (default: {DEFAULT_PLANNING_MAX_ROUNDS})  Max planning consensus rounds (0 = unlimited)\n  DECOMPOSITION_MAX_ROUNDS (default: {DEFAULT_DECOMPOSITION_MAX_ROUNDS})  Max decomposition rounds (0 = unlimited)\n  TIMEOUT               (default: {DEFAULT_TIMEOUT_SECONDS})  Idle timeout in seconds\n  IMPLEMENTER           (default: claude) Implementer agent name (any registered agent)\n  REVIEWER                              Reviewer agent name (default: opposite of implementer)\n  PLANNER                               Planner agent name (default: same as implementer)\n  SINGLE_AGENT          (default: 0)    Enable single-agent mode when truthy\n  AUTO_COMMIT           (default: 1)    Auto-commit loop-owned changes (0 disables)\n  AUTO_TEST             (default: 0)    Run quality checks before review when truthy\n  AUTO_TEST_CMD                         Override auto-detected quality check command\n  COMPOUND              (default: 1)    Enable post-consensus compound learning phase\n  DECISIONS_ENABLED     (default: 1)    Master switch for decisions subsystem (0 disables all decisions)\n  DECISIONS_AUTO_REFERENCE (default: 1) Auto-sync managed decisions-reference blocks in AGENTS.md/CLAUDE.md\n  DECISIONS_MAX_LINES   (default: 50)   Number of decision lines injected into prompts\n  DIFF_MAX_LINES        (default: {DEFAULT_DIFF_MAX_LINES})  Max diff lines before truncation\n  CONTEXT_LINE_CAP      (default: 0)    Max lines for project context (0 = unlimited)\n  PLANNING_CONTEXT_EXCERPT_LINES (default: 0) Max lines per file excerpt in planning (0 = unlimited)\n  BATCH_IMPLEMENT       (default: 1)    Implement all tasks.md tasks in one loop by default\n  MAX_PARALLEL          (default: 1)    Maximum parallel task execution in wave mode\n  VERBOSE               (default: 0)    Enable verbose logging when truthy\n  PROGRESSIVE_CONTEXT   (default: 0)    Replace front-loaded context with on-demand manifest\n  PLANNING_ADVERSARIAL_REVIEW (default: 1) Adversarial second review of plans (dual-agent only)\n\n  Model selection:\n  IMPLEMENTER_MODEL                     Model override for implementer (e.g. claude-sonnet-4-6)\n  REVIEWER_MODEL                        Model override for reviewer (e.g. o3)\n  PLANNER_MODEL                         Model override for planning phase\n  PLANNER_PERMISSION_MODE               Planner permission mode: default|plan\n\n  Claude CLI tuning:\n  CLAUDE_FULL_ACCESS    (default: 1)    Use --dangerously-skip-permissions instead of --allowedTools\n  CLAUDE_ALLOWED_TOOLS  (default: Bash,Read,Edit,Write,Grep,Glob,WebFetch)\n  REVIEWER_ALLOWED_TOOLS (default: Read,Grep,Glob,WebFetch) Reviewer read-only sandbox\n  CLAUDE_SESSION_PERSISTENCE (default: 1) Persist Claude sessions across rounds\n  CLAUDE_EFFORT_LEVEL                   Thinking depth: low|medium|high\n  CLAUDE_MAX_OUTPUT_TOKENS              Max output tokens (1-64000)\n  CLAUDE_MAX_THINKING_TOKENS            Extended thinking token budget\n  IMPLEMENTER_EFFORT_LEVEL              Override effort level for implementer role\n  REVIEWER_EFFORT_LEVEL                 Override effort level for reviewer role\n\n  Codex CLI tuning:\n  CODEX_FULL_ACCESS     (default: 1)    Use --dangerously-bypass-approvals-and-sandbox instead of --full-auto\n  CODEX_SESSION_PERSISTENCE (default: 1) Persist Codex sessions across rounds\n\n  Stuck detection:\n  STUCK_DETECTION_ENABLED (default: 0)  Enable stuck detection in implementation loop\n  STUCK_NO_DIFF_ROUNDS   (default: 3)   Consecutive no-diff rounds before signalling\n  STUCK_THRESHOLD_MINUTES (default: 10)  Wall-clock minutes before signalling\n  STUCK_ACTION           (default: warn) Action on stuck: abort|warn|retry\n\n  Wave runtime:\n  WAVE_LOCK_STALE_SECONDS (default: 30)  Seconds before a wave lock is considered stale\n  WAVE_SHUTDOWN_GRACE_MS  (default: 30000) Grace period (ms) for in-flight tasks on interrupt\n\n  Observability:\n  TRANSCRIPT_ENABLED    (default: 0)    Write human-readable agent I/O transcript to {state_rel}/transcript.log\n\nMigration note: max_rounds / MAX_ROUNDS has been renamed to review_max_rounds / REVIEW_MAX_ROUNDS.\n\nPer-project config: place .agent-loop.toml in the project root (see README)."
    )
}

fn current_project_dir() -> Result<PathBuf, AgentLoopError> {
    std::env::current_dir().map_err(AgentLoopError::from)
}

fn session_state_dir(project_dir: &Path, session: Option<&str>) -> Result<PathBuf, AgentLoopError> {
    if let Some(name) = session {
        config::validate_session_name(name)?;
    }
    let base = project_dir.join(".agent-loop").join("state");
    Ok(match session {
        Some(name) => base.join(name),
        None => base,
    })
}

fn session_wave_lock_path(project_dir: &Path, session: Option<&str>) -> Result<PathBuf, AgentLoopError> {
    if let Some(name) = session {
        config::validate_session_name(name)?;
    }
    let agent_loop_dir = project_dir.join(".agent-loop");
    Ok(match session {
        Some(name) => agent_loop_dir.join(format!("wave-{name}.lock")),
        None => agent_loop_dir.join("wave.lock"),
    })
}

fn resolve_task_for_run(args: &RunArgs) -> Result<String, AgentLoopError> {
    if let Some(task_file_path) = args.file.as_ref() {
        let file_contents = fs::read_to_string(task_file_path).map_err(|err| {
            AgentLoopError::Config(format!(
                "Failed to read task file '{}': {err}",
                task_file_path.display()
            ))
        })?;

        if file_contents.trim().is_empty() {
            return Err(AgentLoopError::Config(format!(
                "Task file '{}' is empty.",
                task_file_path.display()
            )));
        }

        return Ok(file_contents);
    }

    if let Some(task_text) = args.task.as_ref() {
        if task_text.trim().is_empty() {
            return Err(AgentLoopError::Config("Task cannot be empty.".to_string()));
        }
        return Ok(task_text.clone());
    }

    Err(AgentLoopError::Config(
        "Task is required. Provide task text or --file <path>.".to_string(),
    ))
}

fn resolve_task_for_plan(args: &PlanArgs) -> Result<String, AgentLoopError> {
    if let Some(task_file_path) = args.file.as_ref() {
        let file_contents = fs::read_to_string(task_file_path).map_err(|err| {
            AgentLoopError::Config(format!(
                "Failed to read task file '{}': {err}",
                task_file_path.display()
            ))
        })?;

        if file_contents.trim().is_empty() {
            return Err(AgentLoopError::Config(format!(
                "Task file '{}' is empty.",
                task_file_path.display()
            )));
        }

        return Ok(file_contents);
    }

    if let Some(task_text) = args.task.as_ref() {
        if task_text.trim().is_empty() {
            return Err(AgentLoopError::Config("Task cannot be empty.".to_string()));
        }
        return Ok(task_text.clone());
    }

    Err(AgentLoopError::Config(
        "Task is required. Provide task text or --file <path>.".to_string(),
    ))
}

fn resolve_task_for_implement(args: &ImplementArgs) -> Result<String, AgentLoopError> {
    if let Some(task_file_path) = args.file.as_ref() {
        let file_contents = fs::read_to_string(task_file_path).map_err(|err| {
            AgentLoopError::Config(format!(
                "Failed to read task file '{}': {err}",
                task_file_path.display()
            ))
        })?;

        if file_contents.trim().is_empty() {
            return Err(AgentLoopError::Config(format!(
                "Task file '{}' is empty.",
                task_file_path.display()
            )));
        }

        return Ok(file_contents);
    }

    if let Some(task_text) = args.task.as_ref() {
        if task_text.trim().is_empty() {
            return Err(AgentLoopError::Config("Task cannot be empty.".to_string()));
        }
        return Ok(task_text.clone());
    }

    Err(AgentLoopError::Config(
        "Task is required. Provide --task <text> or --file <path>.".to_string(),
    ))
}

fn task_heading(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !(trimmed.starts_with("## ") || trimmed.starts_with("### ")) {
        return None;
    }

    let without_hashes = trimmed.trim_start_matches('#').trim();
    if !without_hashes.starts_with("Task ") {
        return None;
    }

    Some(without_hashes.to_string())
}

fn parse_tasks_markdown(raw_tasks: &str) -> Result<Vec<ParsedTask>, AgentLoopError> {
    let lines = raw_tasks.lines().collect::<Vec<_>>();
    let mut heading_indices = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if let Some(title) = task_heading(line) {
            heading_indices.push((title, index));
        }
    }

    if heading_indices.is_empty() {
        return Err(AgentLoopError::Config(
            "No tasks found in tasks.md. Expected headings like '### Task 1: ...'.".to_string(),
        ));
    }

    let mut parsed = Vec::with_capacity(heading_indices.len());
    for (index, (title, start_line)) in heading_indices.iter().enumerate() {
        let end_line = heading_indices
            .get(index + 1)
            .map(|(_, line)| *line)
            .unwrap_or(lines.len());

        let content = lines[*start_line..end_line].join("\n").trim().to_string();
        // Parse dependencies from the body after the heading line so the
        // heading doesn't consume one of the 3 non-blank line slots.
        let body_start = *start_line + 1;
        let body = if body_start < end_line {
            lines[body_start..end_line].join("\n")
        } else {
            String::new()
        };
        let dependencies = wave::parse_dependencies(&body);
        parsed.push(ParsedTask {
            title: title.clone(),
            content,
            dependencies,
        });
    }

    Ok(parsed)
}

fn parse_tasks_file(raw_tasks: &str, tasks_file: &Path) -> Result<Vec<ParsedTask>, AgentLoopError> {
    if raw_tasks.trim().is_empty() {
        return Err(AgentLoopError::Config(format!(
            "Tasks file '{}' is empty. Run 'agent-loop plan --file <PLAN.md>' first or provide --file with a populated tasks markdown.",
            tasks_file.display()
        )));
    }

    parse_tasks_markdown(raw_tasks)
}

fn build_batch_implementation_task(raw_tasks: &str) -> String {
    format!(
        "Implement ALL tasks below as one cohesive change set.\n\
         Treat cross-task dependencies holistically and ensure every task is fully satisfied.\n\n\
         TASKS:\n{tasks}",
        tasks = raw_tasks.trim()
    )
}

fn build_plan_implementation_task(original_task: &str, plan: &str) -> String {
    let original_task = original_task.trim();
    let plan = plan.trim();
    if original_task.is_empty() {
        return format!(
            "Implement the approved plan below as one cohesive change set.\n\
             Treat dependencies across steps holistically and ensure all plan outcomes are satisfied.\n\n\
             PLAN:\n{plan}"
        );
    }

    format!(
        "Implement the approved plan below as one cohesive change set.\n\
         Treat dependencies across steps holistically and ensure all plan outcomes are satisfied.\n\n\
         ORIGINAL TASK:\n{original_task}\n\n\
         PLAN:\n{plan}"
    )
}

fn build_plan_fallback_tasks(original_task: &str, plan: &str) -> String {
    format!(
        "## Task 1: Implement approved plan\n{}\n",
        build_plan_implementation_task(original_task, plan).trim()
    )
}

fn build_decomposition_task_from_plan(plan: &str) -> String {
    format!(
        "Use the approved plan below as the source of truth for task decomposition.\n\nPLAN:\n{}",
        plan.trim()
    )
}

fn resolve_compound_task(
    task: Option<&String>,
    file: Option<&PathBuf>,
) -> Result<String, AgentLoopError> {
    if let Some(task_file_path) = file {
        let file_contents = fs::read_to_string(task_file_path).map_err(|err| {
            AgentLoopError::Config(format!(
                "Failed to read task file '{}': {err}",
                task_file_path.display()
            ))
        })?;

        if file_contents.trim().is_empty() {
            return Err(AgentLoopError::Config(format!(
                "Task file '{}' is empty.",
                task_file_path.display()
            )));
        }

        return Ok(file_contents);
    }

    if let Some(task_text) = task {
        if task_text.trim().is_empty() {
            return Err(AgentLoopError::Config("Task cannot be empty.".to_string()));
        }
        return Ok(task_text.clone());
    }

    Err(AgentLoopError::Config(
        "Task is required. Provide task text or --file <path>.".to_string(),
    ))
}

const IMPLEMENT_MODE_FILENAME: &str = "implement-mode.txt";
const IMPLEMENT_FLAGS_FILENAME: &str = "implement-flags.json";

fn write_implement_mode(
    mode: ImplementExecutionMode,
    config: &Config,
) -> Result<(), AgentLoopError> {
    state::write_state_file(IMPLEMENT_MODE_FILENAME, &format!("{mode}\n"), config)?;
    Ok(())
}

fn write_implement_flags(
    flags: &ImplementModeFlags,
    config: &Config,
) -> Result<(), AgentLoopError> {
    let serialized = serde_json::to_string_pretty(flags).map_err(|err| {
        AgentLoopError::Config(format!("Failed to serialize implement flags: {err}"))
    })?;
    state::write_state_file(IMPLEMENT_FLAGS_FILENAME, &serialized, config)?;
    Ok(())
}

fn read_implement_mode(config: &Config) -> Option<ImplementExecutionMode> {
    state::read_state_file(IMPLEMENT_MODE_FILENAME, config)
        .trim()
        .parse::<ImplementExecutionMode>()
        .ok()
}

fn read_implement_flags(config: &Config) -> Option<ImplementModeFlags> {
    let raw = state::read_state_file(IMPLEMENT_FLAGS_FILENAME, config);
    if raw.trim().is_empty() {
        return None;
    }

    serde_json::from_str(raw.trim()).ok()
}

fn persist_implement_settings(
    mode: ImplementExecutionMode,
    flags: &ImplementModeFlags,
    config: &Config,
) -> Result<(), AgentLoopError> {
    write_implement_mode(mode, config)?;
    write_implement_flags(flags, config)?;
    Ok(())
}

fn resolve_fresh_implement_mode(
    flags: &ImplementModeFlags,
    config: &Config,
) -> ImplementExecutionMode {
    if flags.wave {
        ImplementExecutionMode::Wave
    } else if flags.per_task || !config.batch_implement {
        ImplementExecutionMode::PerTask
    } else {
        ImplementExecutionMode::Batch
    }
}

fn effective_explicit_resume_mode(flags: &ImplementModeFlags) -> Option<ImplementExecutionMode> {
    if flags.wave {
        Some(ImplementExecutionMode::Wave)
    } else if flags.per_task {
        Some(ImplementExecutionMode::PerTask)
    } else {
        None
    }
}

fn resolve_resume_implement_mode(
    flags: &ImplementModeFlags,
    config: &Config,
    workflow: state::WorkflowKind,
    origin: ResumeOrigin,
) -> Result<ImplementExecutionMode, AgentLoopError> {
    let persisted = read_implement_mode(config);
    let explicit = effective_explicit_resume_mode(flags);

    if let (Some(explicit_mode), Some(persisted_mode)) = (explicit, persisted)
        && explicit_mode != persisted_mode
    {
        return Err(AgentLoopError::Config(format!(
            "Resume mode mismatch: state was started in {persisted_mode} mode but CLI requested {explicit_mode}."
        )));
    }

    let mode = if let Some(persisted_mode) = persisted {
        persisted_mode
    } else if let Some(explicit_mode) = explicit {
        explicit_mode
    } else if workflow != state::WorkflowKind::Implement {
        resolve_fresh_implement_mode(flags, config)
    } else if !config.batch_implement {
        return Err(AgentLoopError::Config(
            "Cannot resume implementation in per-task mode without persisted mode metadata. Re-run without --resume."
                .to_string(),
        ));
    } else {
        ImplementExecutionMode::Batch
    };

    if workflow == state::WorkflowKind::Implement
        && mode == ImplementExecutionMode::PerTask
        && origin == ResumeOrigin::User
    {
        return Err(AgentLoopError::Config(
            "--per-task cannot be combined with --resume.".to_string(),
        ));
    }

    Ok(mode)
}

fn normalize_flags_for_mode(
    mut flags: ImplementModeFlags,
    mode: ImplementExecutionMode,
) -> ImplementModeFlags {
    flags.wave = mode == ImplementExecutionMode::Wave;
    flags.per_task = mode == ImplementExecutionMode::PerTask;
    flags
}

fn resolve_resume_implement_settings(
    requested_flags: &ImplementModeFlags,
    config: &Config,
    workflow: state::WorkflowKind,
    origin: ResumeOrigin,
) -> Result<(ImplementExecutionMode, ImplementModeFlags), AgentLoopError> {
    let mode = resolve_resume_implement_mode(requested_flags, config, workflow, origin)?;
    let flags = read_implement_flags(config).unwrap_or_else(|| requested_flags.clone());
    Ok((mode, normalize_flags_for_mode(flags, mode)))
}

fn read_current_status(project_dir: &Path, single_agent: bool, session: Option<&str>) -> Option<LoopStatus> {
    let config = Config::from_cli(project_dir.to_path_buf(), single_agent, false, session).ok()?;
    if !config.state_dir.join("status.json").is_file() {
        return None;
    }

    Some(state::read_status(&config))
}

fn is_timeout_reason(reason: Option<&str>) -> bool {
    reason
        .map(|value| {
            let lower = value.to_ascii_lowercase();
            lower.contains("timed out") || lower.contains("timeout")
        })
        .unwrap_or(false)
}

fn is_retryable_run_tasks_status(status: Option<&LoopStatus>) -> bool {
    match status {
        Some(value) if value.status == Status::MaxRounds => true,
        Some(value) if value.status == Status::Stuck => true,
        Some(value) if value.status == Status::Error => is_timeout_reason(value.reason.as_deref()),
        _ => false,
    }
}

fn format_status_reason(status: Option<&LoopStatus>) -> String {
    match status {
        Some(value) => match value.reason.as_deref() {
            Some(reason) if !reason.trim().is_empty() => {
                format!("{} ({reason})", value.status)
            }
            _ => value.status.to_string(),
        },
        None => "UNKNOWN".to_string(),
    }
}

fn persist_last_run_task(task: &str, config: &Config) -> Result<(), AgentLoopError> {
    state::write_status(
        StatusPatch {
            last_run_task: Some(task.to_string()),
            ..StatusPatch::default()
        },
        config,
    )?;
    Ok(())
}

fn phase_success_to_exit_code(success: bool) -> i32 {
    if success { 0 } else { 1 }
}

#[derive(Debug, Clone)]
struct ImplementationRunOptions {
    review_max_rounds_override: Option<u32>,
    clear_progress: bool,
    write_mode: Option<ImplementExecutionMode>,
    persist_flags: Option<ImplementModeFlags>,
    cleanup_on_success: bool,
}

/// Internal helper used by `implement --task`, `implement --file`, and
/// `implement` task-list execution attempts.
fn run_command_with_review_max_rounds(
    args: RunArgs,
    options: ImplementationRunOptions,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    if args.resume {
        return Err(AgentLoopError::Config(
            "run_command_with_review_max_rounds must not be called with resume=true. \
             Use 'implement --resume' instead."
                .to_string(),
        ));
    }
    if args.planning_only {
        return Err(AgentLoopError::Config(
            "run_command_with_review_max_rounds must not be called with planning_only=true. \
             Use 'plan' subcommand instead."
                .to_string(),
        ));
    }

    let project_dir = current_project_dir()?;
    let mut config = Config::from_cli_with_overrides(
        project_dir,
        args.single_agent,
        false,
        options.review_max_rounds_override,
        session,
    )?;
    preflight::run_preflight(&mut config)?;

    let task = resolve_task_for_run(&args)?;

    run_command_with_review_max_rounds_using(task, config, options, phases::implementation_loop)
}

fn run_command_with_review_max_rounds_using<F>(
    task: String,
    config: Config,
    options: ImplementationRunOptions,
    run_loop: F,
) -> Result<i32, AgentLoopError>
where
    F: FnOnce(&Config, &HashSet<String>) -> bool,
{
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    let baseline_set = baseline_vec.iter().cloned().collect::<HashSet<_>>();

    let existing_plan = state::read_state_file("plan.md", &config);
    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Implement,
    )?;
    if let Some(mode) = options.write_mode {
        write_implement_mode(mode, &config)?;
    }
    if let Some(flags) = options.persist_flags.as_ref() {
        write_implement_flags(flags, &config)?;
    }
    if options.clear_progress {
        state::clear_implement_progress(&config);
    }
    if !existing_plan.trim().is_empty() {
        state::write_state_file("plan.md", &existing_plan, &config)?;
    }

    let reached_consensus = run_loop(&config, &baseline_set);
    phases::print_summary(&config);
    let exit_code = phase_success_to_exit_code(reached_consensus);
    persist_last_run_task(task.as_str(), &config)?;

    // Check if the loop was interrupted by a signal and propagate as the
    // Interrupted error variant so main() can exit with code 130.
    if exit_code != 0 {
        let final_status = state::read_status(&config);
        if final_status.status == state::Status::Interrupted {
            return Err(AgentLoopError::Interrupted(
                final_status
                    .reason
                    .unwrap_or_else(|| "Interrupted by signal".to_string()),
            ));
        }
    }

    cleanup_session_files_on_success_if_requested(&config, exit_code, options.cleanup_on_success);
    Ok(exit_code)
}

/// Resume helper used by `implement` task retries.
fn resume_for_tasks(
    single_agent: bool,
    review_max_rounds_override: Option<u32>,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    implementation_resume_with_review_max_rounds(single_agent, review_max_rounds_override, session)
}

fn plan_command(args: PlanArgs, session: Option<&str>) -> Result<i32, AgentLoopError> {
    let started = Instant::now();

    if args.resume {
        let project_dir = current_project_dir()?;
        let state_dir = session_state_dir(&project_dir, session)?;
        ensure_resume_state_dir_exists(&state_dir)?;
        let config = Config::from_cli(project_dir, args.single_agent, false, session)?;
        let workflow = state::read_workflow(&config);
        if workflow != Some(state::WorkflowKind::Plan) {
            return Err(AgentLoopError::State(
                "Cannot resume planning: workflow is not 'plan'.".to_string(),
            ));
        }
        let plan_content = state::read_state_file("plan.md", &config);
        if plan_content.trim().is_empty() {
            return Err(AgentLoopError::State(
                "Cannot resume planning: plan.md is empty.".to_string(),
            ));
        }

        let planned = phases::planning_phase_resume(&config);
        let exit_code = phase_success_to_exit_code(planned);
        if planned {
            phases::print_summary(&config);
        }
        println!(
            "Elapsed: {}",
            format_duration_ms(started.elapsed().as_millis() as u64)
        );

        if exit_code != 0 {
            let final_status = state::read_status(&config);
            if final_status.status == state::Status::Interrupted {
                return Err(AgentLoopError::Interrupted(
                    final_status
                        .reason
                        .unwrap_or_else(|| "Interrupted by signal".to_string()),
                ));
            }
        }
        cleanup_session_files_on_success(&config, exit_code);
        return Ok(exit_code);
    }

    let task = resolve_task_for_plan(&args)?;
    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir, args.single_agent, false, session)?;

    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };

    reset_state_dir(&config.project_dir, session)?;
    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Plan,
    )?;
    state::write_workflow(state::WorkflowKind::Plan, &config)?;

    let planned = phases::planning_phase(&config, true);
    let exit_code = phase_success_to_exit_code(planned);
    if planned {
        phases::print_summary(&config);
    }

    println!(
        "Elapsed: {}",
        format_duration_ms(started.elapsed().as_millis() as u64)
    );

    persist_last_run_task(task.as_str(), &config)?;

    // Check if the loop was interrupted by a signal and propagate as the
    // Interrupted error variant so main() can exit with code 130.
    if exit_code != 0 {
        let final_status = state::read_status(&config);
        if final_status.status == state::Status::Interrupted {
            return Err(AgentLoopError::Interrupted(
                final_status
                    .reason
                    .unwrap_or_else(|| "Interrupted by signal".to_string()),
            ));
        }
    }

    cleanup_session_files_on_success(&config, exit_code);
    Ok(exit_code)
}

/// Pre-config helper: validate that the state directory and status.json exist.
/// Operates on raw paths so it can run before `Config` is built.
fn ensure_resume_state_dir_exists(state_dir: &Path) -> Result<(), AgentLoopError> {
    if !state_dir.is_dir() {
        return Err(AgentLoopError::State(
            format!("Cannot resume: {} does not exist. Run a command first.", state_dir.display()),
        ));
    }

    let status_path = state_dir.join("status.json");
    if !status_path.is_file() {
        return Err(AgentLoopError::State(format!(
            "Cannot resume: '{}' is missing.",
            status_path.display()
        )));
    }

    Ok(())
}

/// Pre-config helper: read the task text from persisted state.
fn read_resume_task_from_state_dir(state_dir: &Path) -> Result<String, AgentLoopError> {
    let task_path = state_dir.join("task.md");
    let task = fs::read_to_string(&task_path).map_err(|err| {
        AgentLoopError::State(format!(
            "Cannot resume: failed to read '{}': {err}",
            task_path.display()
        ))
    })?;
    if task.trim().is_empty() {
        return Err(AgentLoopError::State(format!(
            "Cannot resume: '{}' is empty.",
            task_path.display()
        )));
    }
    Ok(task)
}

fn reconcile_task_status(parsed_tasks: &[ParsedTask], config: &Config) -> Vec<TaskStatusEntry> {
    let persisted = state::read_task_status_with_warnings(config);
    let persisted_entries = persisted.status_file.tasks;

    // Reconcile by index first, but only reuse persisted values when the title
    // still matches. This avoids carrying stale batch aggregate entries into
    // per-task mode while still preserving task state across normal reruns.
    parsed_tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            if let Some(entry) = persisted_entries.get(i)
                && entry.title == task.title
            {
                TaskStatusEntry {
                    title: task.title.clone(),
                    ..entry.clone()
                }
            } else {
                TaskStatusEntry {
                    title: task.title.clone(),
                    status: TaskRunStatus::Pending,
                    retries: 0,
                    last_error: None,
                    skip_reason: None,
                    wave_index: None,
                }
            }
        })
        .collect()
}

fn reconcile_task_metrics(parsed_tasks: &[ParsedTask], config: &Config) -> Vec<TaskMetricsEntry> {
    let persisted = state::read_task_metrics(config);
    let persisted_entries = persisted.tasks;

    // Reconcile by index first, but only reuse persisted values when the title
    // still matches. This avoids carrying stale batch aggregate entries into
    // per-task mode while still preserving task metrics across normal reruns.
    parsed_tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            if let Some(entry) = persisted_entries.get(i)
                && entry.title == task.title
            {
                TaskMetricsEntry {
                    title: task.title.clone(),
                    ..entry.clone()
                }
            } else {
                TaskMetricsEntry {
                    title: task.title.clone(),
                    task_started_at: None,
                    task_ended_at: None,
                    duration_ms: None,
                    agent_calls: None,
                    input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    cost_usd_micros: None,
                }
            }
        })
        .collect()
}

fn format_duration_ms(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_u64(value: u64) -> String {
    let mut chars = value.to_string().chars().rev().collect::<Vec<_>>();
    let mut result = String::new();
    for (index, ch) in chars.drain(..).enumerate() {
        if index > 0 && index % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

fn format_cost_usd_micros(cost_usd_micros: u64) -> String {
    let dollars = cost_usd_micros / 1_000_000;
    let micros = cost_usd_micros % 1_000_000;
    format!("${dollars}.{micros:06}")
}

fn task_usage_snapshot(entry: &TaskMetricsEntry) -> agent::UsageSnapshot {
    agent::UsageSnapshot {
        agent_calls: entry.agent_calls.unwrap_or(0),
        input_tokens: entry.input_tokens.unwrap_or(0),
        output_tokens: entry.output_tokens.unwrap_or(0),
        total_tokens: entry.total_tokens.unwrap_or(0),
        cost_usd_micros: entry.cost_usd_micros.unwrap_or(0),
    }
}

fn apply_task_usage(entry: &mut TaskMetricsEntry, usage: agent::UsageSnapshot) {
    entry.agent_calls = (usage.agent_calls > 0).then_some(usage.agent_calls);
    entry.input_tokens = (usage.input_tokens > 0).then_some(usage.input_tokens);
    entry.output_tokens = (usage.output_tokens > 0).then_some(usage.output_tokens);
    entry.total_tokens = (usage.total_tokens > 0).then_some(usage.total_tokens);
    entry.cost_usd_micros = (usage.cost_usd_micros > 0).then_some(usage.cost_usd_micros);
}

fn print_task_duration_summary(metrics: &[TaskMetricsEntry]) {
    println!();
    println!("Task Metrics:");
    let mut total_ms: u64 = 0;
    let mut total_usage = agent::UsageSnapshot::default();
    for entry in metrics {
        let duration_str = match entry.duration_ms {
            Some(ms) => {
                total_ms += ms;
                format_duration_ms(ms)
            }
            None => "n/a".to_string(),
        };

        let usage = task_usage_snapshot(entry);
        total_usage = total_usage.saturating_add(usage);
        if usage.is_zero() {
            println!("  {} — {}", entry.title, duration_str);
        } else {
            println!(
                "  {} — {} | calls {} | tokens in:{} out:{} total:{}{}",
                entry.title,
                duration_str,
                format_u64(usage.agent_calls),
                format_u64(usage.input_tokens),
                format_u64(usage.output_tokens),
                format_u64(usage.total_tokens),
                if usage.cost_usd_micros > 0 {
                    format!(" | cost {}", format_cost_usd_micros(usage.cost_usd_micros))
                } else {
                    String::new()
                }
            );
        }
    }
    if total_usage.is_zero() {
        println!("  Total — {}", format_duration_ms(total_ms));
    } else {
        println!(
            "  Total — {} | calls {} | tokens in:{} out:{} total:{}{}",
            format_duration_ms(total_ms),
            format_u64(total_usage.agent_calls),
            format_u64(total_usage.input_tokens),
            format_u64(total_usage.output_tokens),
            format_u64(total_usage.total_tokens),
            if total_usage.cost_usd_micros > 0 {
                format!(
                    " | cost {}",
                    format_cost_usd_micros(total_usage.cost_usd_micros)
                )
            } else {
                String::new()
            }
        );
    }
}

fn batch_metrics_title(task_count: usize) -> String {
    format!("Batch implementation ({task_count} tasks)")
}

#[allow(clippy::too_many_arguments)]
fn persist_batch_task_state(
    title: &str,
    task_status: TaskRunStatus,
    last_error: Option<String>,
    started_at: String,
    ended_at: String,
    duration_ms: u64,
    usage: agent::UsageSnapshot,
    config: &Config,
) -> Result<(), AgentLoopError> {
    state::write_task_status(
        &TaskStatusFile {
            tasks: vec![TaskStatusEntry {
                title: title.to_string(),
                status: task_status,
                retries: 0,
                last_error,
                skip_reason: None,
                wave_index: None,
            }],
        },
        config,
    )?;

    let mut metrics_entry = TaskMetricsEntry {
        title: title.to_string(),
        task_started_at: Some(started_at),
        task_ended_at: Some(ended_at),
        duration_ms: Some(duration_ms),
        agent_calls: None,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        cost_usd_micros: None,
    };
    apply_task_usage(&mut metrics_entry, usage);

    state::write_task_metrics(
        &TaskMetricsFile {
            tasks: vec![metrics_entry.clone()],
        },
        config,
    )?;
    print_task_duration_summary(&[metrics_entry]);
    Ok(())
}

fn implementation_resume_with_review_max_rounds_internal(
    single_agent: bool,
    review_max_rounds_override: Option<u32>,
    print_elapsed: bool,
    cleanup_on_success: bool,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    let started = Instant::now();
    let project_dir = current_project_dir()?;
    let state_dir = session_state_dir(&project_dir, session)?;
    ensure_resume_state_dir_exists(&state_dir)?;
    let task = read_resume_task_from_state_dir(&state_dir)?;

    let mut config = Config::from_cli_with_overrides(
        project_dir,
        single_agent,
        false,
        review_max_rounds_override,
        session,
    )?;
    preflight::run_preflight(&mut config)?;
    let workflow = state::read_workflow(&config);
    if workflow != Some(state::WorkflowKind::Implement) {
        return Err(AgentLoopError::State(
            "Cannot resume implementation: workflow is not 'implement'.".to_string(),
        ));
    }

    implementation_resume_with_review_max_rounds_internal_using(
        started,
        task,
        config,
        print_elapsed,
        cleanup_on_success,
        phases::implementation_loop_resume,
    )
}

fn implementation_resume_with_review_max_rounds_internal_using<F>(
    started: Instant,
    task: String,
    config: Config,
    print_elapsed: bool,
    cleanup_on_success: bool,
    run_loop: F,
) -> Result<i32, AgentLoopError>
where
    F: FnOnce(&Config, &HashSet<String>) -> bool,
{
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    let baseline_set = baseline_vec.iter().cloned().collect::<HashSet<_>>();

    let reached_consensus = run_loop(&config, &baseline_set);
    phases::print_summary(&config);
    let exit_code = phase_success_to_exit_code(reached_consensus);
    persist_last_run_task(task.as_str(), &config)?;

    if exit_code != 0 {
        let final_status = state::read_status(&config);
        if final_status.status == state::Status::Interrupted {
            return Err(AgentLoopError::Interrupted(
                final_status
                    .reason
                    .unwrap_or_else(|| "Interrupted by signal".to_string()),
            ));
        }
    }

    if print_elapsed {
        println!(
            "Elapsed: {}",
            format_duration_ms(started.elapsed().as_millis() as u64)
        );
    }

    cleanup_session_files_on_success_if_requested(&config, exit_code, cleanup_on_success);
    Ok(exit_code)
}

fn implementation_resume_with_review_max_rounds(
    single_agent: bool,
    review_max_rounds_override: Option<u32>,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    implementation_resume_with_review_max_rounds_internal(
        single_agent,
        review_max_rounds_override,
        true,
        true,
        session,
    )
}

fn load_tasks_markdown(tasks_file: &Path) -> Result<Option<String>, AgentLoopError> {
    match fs::read_to_string(tasks_file) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(AgentLoopError::Config(format!(
            "Failed to read '{}': {err}",
            tasks_file.display()
        ))),
    }
}

fn implement_all_tasks_command_with_mode(
    args: &ImplementArgs,
    config: &Config,
    project_dir: &Path,
    mode: ImplementExecutionMode,
    cleanup_on_success: bool,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    implement_all_tasks_command_with_mode_using(
        args,
        config,
        project_dir,
        mode,
        cleanup_on_success,
        implement_all_tasks_wave,
        implement_all_tasks_per_task,
        session,
    )
}

#[allow(clippy::too_many_arguments)]
fn implement_all_tasks_command_with_mode_using<W, P>(
    args: &ImplementArgs,
    config: &Config,
    project_dir: &Path,
    mode: ImplementExecutionMode,
    cleanup_on_success: bool,
    run_wave: W,
    run_per_task: P,
    session: Option<&str>,
) -> Result<i32, AgentLoopError>
where
    W: FnOnce(&ImplementArgs, &[ParsedTask], &Config, &Path) -> Result<i32, AgentLoopError>,
    P: FnOnce(&ImplementArgs, &[ParsedTask], &Config, &Path) -> Result<i32, AgentLoopError>,
{
    let tasks_file = config.state_dir.join("tasks.md");
    let raw_tasks = load_tasks_markdown(&tasks_file)?;

    if per_task_only_flags_present(args) && mode == ImplementExecutionMode::Batch {
        return Err(AgentLoopError::Config(
            "Per-task lifecycle flags require per-task mode. Use '--per-task' or set 'batch_implement = false'."
                .to_string(),
        ));
    }

    let result = match mode {
        ImplementExecutionMode::Wave => {
            let raw_tasks = raw_tasks.as_deref().ok_or_else(|| {
                AgentLoopError::State("No tasks found. Run 'agent-loop tasks' first.".to_string())
            })?;
            let parsed_tasks = parse_tasks_file(raw_tasks, &tasks_file)?;
            persist_implement_settings(mode, &args.flags, config)?;
            println!(
                "Found {} tasks in {}",
                parsed_tasks.len(),
                tasks_file.display()
            );
            run_wave(args, &parsed_tasks, config, project_dir)
        }
        ImplementExecutionMode::PerTask => {
            let raw_tasks = raw_tasks.as_deref().ok_or_else(|| {
                AgentLoopError::State("No tasks found. Run 'agent-loop tasks' first.".to_string())
            })?;
            let parsed_tasks = parse_tasks_file(raw_tasks, &tasks_file)?;
            persist_implement_settings(mode, &args.flags, config)?;
            state::clear_implement_progress(config);
            println!(
                "Found {} tasks in {}",
                parsed_tasks.len(),
                tasks_file.display()
            );
            run_per_task(args, &parsed_tasks, config, project_dir)
        }
        ImplementExecutionMode::Batch => {
            if let Some(raw_tasks) = raw_tasks.as_deref()
                && !raw_tasks.trim().is_empty()
            {
                let parsed_tasks = parse_tasks_file(raw_tasks, &tasks_file)?;
                persist_implement_settings(mode, &args.flags, config)?;
                println!(
                    "Found {} tasks in {}",
                    parsed_tasks.len(),
                    tasks_file.display()
                );
                return implement_all_tasks_batch(
                    args,
                    &parsed_tasks,
                    config,
                    project_dir,
                    raw_tasks,
                    cleanup_on_success,
                    session,
                );
            }

            let raw_plan = state::read_state_file("plan.md", config);
            if !raw_plan.trim().is_empty() {
                persist_implement_settings(mode, &args.flags, config)?;
                return implement_plan_batch(
                    args,
                    config,
                    project_dir,
                    &raw_plan,
                    cleanup_on_success,
                    session,
                );
            }

            Err(AgentLoopError::State(
                "No tasks found and no plan found. Run 'agent-loop plan' first, or generate tasks with 'agent-loop tasks'.".to_string(),
            ))
        }
    };

    if cleanup_on_success && mode != ImplementExecutionMode::Batch {
        cleanup_result_session_files_on_success(config, &result);
    }

    result
}

fn resume_implementation_for_mode(
    args: &ImplementArgs,
    config: &Config,
    project_dir: &Path,
    mode: ImplementExecutionMode,
    print_elapsed: bool,
    cleanup_on_success: bool,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    match mode {
        ImplementExecutionMode::Batch => implementation_resume_with_review_max_rounds_internal(
            args.single_agent,
            None,
            print_elapsed,
            cleanup_on_success,
            session,
        ),
        ImplementExecutionMode::Wave => implement_all_tasks_command_with_mode(
            args,
            config,
            project_dir,
            mode,
            cleanup_on_success,
            session,
        ),
        ImplementExecutionMode::PerTask => Err(AgentLoopError::Config(
            "--per-task cannot be combined with --resume.".to_string(),
        )),
    }
}

fn implement_command(args: ImplementArgs, session: Option<&str>) -> Result<i32, AgentLoopError> {
    args.validate()?;

    if args.resume {
        let project_dir = current_project_dir()?;
        let state_dir = session_state_dir(&project_dir, session)?;
        ensure_resume_state_dir_exists(&state_dir)?;
        let config = Config::from_cli(project_dir.clone(), args.single_agent, false, session)?;
        let workflow = state::read_workflow(&config);
        if workflow != Some(state::WorkflowKind::Implement) {
            return Err(AgentLoopError::State(
                "Cannot resume implementation: workflow is not 'implement'.".to_string(),
            ));
        }
        let (mode, effective_flags) = resolve_resume_implement_settings(
            &args.flags,
            &config,
            state::WorkflowKind::Implement,
            ResumeOrigin::User,
        )?;
        let mut resume_args = args.clone();
        resume_args.flags = effective_flags;
        return resume_implementation_for_mode(
            &resume_args,
            &config,
            &project_dir,
            mode,
            true,
            true,
            session,
        );
    }

    if args.task.is_some() || args.file.is_some() {
        let task = resolve_task_for_implement(&args)?;
        return run_command_with_review_max_rounds(
            RunArgs {
                task: Some(task),
                file: None,
                resume: false,
                planning_only: false,
                single_agent: args.single_agent,
            },
            ImplementationRunOptions {
                review_max_rounds_override: None,
                clear_progress: true,
                write_mode: Some(ImplementExecutionMode::Batch),
                persist_flags: Some(args.flags.clone()),
                cleanup_on_success: true,
            },
            session,
        );
    }

    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir.clone(), args.single_agent, false, session)?;
    let mode = resolve_fresh_implement_mode(&args.flags, &config);
    implement_all_tasks_command_with_mode(&args, &config, &project_dir, mode, true, session)
}

fn tasks_command(args: TasksArgs, session: Option<&str>) -> Result<i32, AgentLoopError> {
    let started = Instant::now();
    args.validate()?;

    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir.clone(), args.single_agent, false, session)?;

    if args.resume {
        let state_dir = session_state_dir(&project_dir, session)?;
        ensure_resume_state_dir_exists(&state_dir)?;
        let workflow = state::read_workflow(&config);
        if workflow != Some(state::WorkflowKind::Decompose) {
            return Err(AgentLoopError::State(
                "Cannot resume tasks decomposition: workflow is not 'decompose'.".to_string(),
            ));
        }

        let exit_code =
            phase_success_to_exit_code(phases::task_decomposition_phase_resume(&config));
        println!(
            "Elapsed: {}",
            format_duration_ms(started.elapsed().as_millis() as u64)
        );
        if exit_code != 0 {
            let final_status = state::read_status(&config);
            if final_status.status == state::Status::Interrupted {
                return Err(AgentLoopError::Interrupted(
                    final_status
                        .reason
                        .unwrap_or_else(|| "Interrupted by signal".to_string()),
                ));
            }
        }
        cleanup_session_files_on_success(&config, exit_code);
        return Ok(exit_code);
    }

    let plan_content = if let Some(path) = args.file.as_ref() {
        fs::read_to_string(path).map_err(|err| {
            AgentLoopError::Config(format!(
                "Failed to read plan file '{}': {err}",
                path.display()
            ))
        })?
    } else {
        fs::read_to_string(config.state_dir.join("plan.md")).unwrap_or_default()
    };

    if plan_content.trim().is_empty() {
        return Err(AgentLoopError::State(
            "No plan found. Run 'agent-loop plan' first.".to_string(),
        ));
    }

    let existing_task = state::read_state_file("task.md", &config);
    let task = if existing_task.trim().is_empty() {
        plan_content.clone()
    } else {
        existing_task
    };

    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Decompose,
    )?;
    state::write_state_file("plan.md", &plan_content, &config)?;
    state::write_workflow(state::WorkflowKind::Decompose, &config)?;

    let succeeded = phases::task_decomposition_phase(&config);
    let exit_code = phase_success_to_exit_code(succeeded);
    if succeeded {
        let tasks_content = state::read_state_file("tasks.md", &config);
        let task_count = parse_tasks_markdown(&tasks_content)
            .map(|tasks| tasks.len())
            .unwrap_or(0);
        println!(
            "Created {} tasks in {}",
            task_count,
            config.state_dir.join("tasks.md").display()
        );
    }

    println!(
        "Elapsed: {}",
        format_duration_ms(started.elapsed().as_millis() as u64)
    );

    if exit_code != 0 {
        let final_status = state::read_status(&config);
        if final_status.status == state::Status::Interrupted {
            return Err(AgentLoopError::Interrupted(
                final_status
                    .reason
                    .unwrap_or_else(|| "Interrupted by signal".to_string()),
            ));
        }
    }

    cleanup_session_files_on_success(&config, exit_code);
    Ok(exit_code)
}

fn cleanup_session_files_on_success(config: &Config, exit_code: i32) {
    if exit_code != 0 {
        return;
    }

    if let Err(err) = state::cleanup_session_files(config) {
        eprintln!("Warning: failed to clean up session files: {err}");
    }
}

fn cleanup_session_files_on_success_if_requested(
    config: &Config,
    exit_code: i32,
    cleanup_on_success: bool,
) {
    if cleanup_on_success {
        cleanup_session_files_on_success(config, exit_code);
    }
}

fn cleanup_result_session_files_on_success(config: &Config, result: &Result<i32, AgentLoopError>) {
    if let Ok(exit_code) = result {
        cleanup_session_files_on_success(config, *exit_code);
    }
}

fn build_implement_args(
    flags: ImplementModeFlags,
    single_agent: bool,
    resume: bool,
) -> ImplementArgs {
    ImplementArgs {
        task: None,
        file: None,
        resume,
        single_agent,
        flags,
    }
}

fn finish_command_exit(
    started: Instant,
    config: &Config,
    exit_code: i32,
) -> Result<i32, AgentLoopError> {
    println!(
        "Elapsed: {}",
        format_duration_ms(started.elapsed().as_millis() as u64)
    );
    if exit_code != 0 {
        let final_status = state::read_status(config);
        if final_status.status == state::Status::Interrupted {
            return Err(AgentLoopError::Interrupted(
                final_status
                    .reason
                    .unwrap_or_else(|| "Interrupted by signal".to_string()),
            ));
        }
    }
    cleanup_session_files_on_success(config, exit_code);
    Ok(exit_code)
}

#[allow(clippy::too_many_arguments)]
fn transition_to_implementation(
    task: &str,
    plan_content: &str,
    tasks_content: Option<&str>,
    mode: ImplementExecutionMode,
    implement_args: &ImplementArgs,
    config: &Config,
    baseline_vec: &[String],
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    state::init(task, config, baseline_vec, state::WorkflowKind::Implement)?;
    persist_implement_settings(mode, &implement_args.flags, config)?;
    if !plan_content.trim().is_empty() {
        state::write_state_file("plan.md", plan_content, config)?;
    }
    if let Some(tasks_content) = tasks_content.filter(|value| !value.trim().is_empty()) {
        state::write_state_file("tasks.md", tasks_content, config)?;
    }

    implement_all_tasks_command_with_mode(implement_args, config, &config.project_dir, mode, false, session)
}

fn plan_tasks_implement_command(args: PlanTasksImplementArgs, session: Option<&str>) -> Result<i32, AgentLoopError> {
    args.validate()?;
    let started = Instant::now();

    if args.resume {
        return plan_tasks_implement_resume(args, started, session);
    }

    let task = resolve_compound_task(args.task.as_ref(), args.file.as_ref())?;
    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir, args.single_agent, false, session)?;
    let mode = resolve_fresh_implement_mode(&args.flags, &config);
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };

    reset_state_dir(&config.project_dir, session)?;
    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Plan,
    )?;
    persist_implement_settings(mode, &args.flags, &config)?;

    if !phases::planning_phase(&config, true) {
        return finish_command_exit(started, &config, 1);
    }

    let plan_content = state::read_state_file("plan.md", &config);
    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Decompose,
    )?;
    persist_implement_settings(mode, &args.flags, &config)?;
    state::write_state_file("plan.md", &plan_content, &config)?;

    if !phases::task_decomposition_phase(&config) {
        return finish_command_exit(started, &config, 1);
    }

    let tasks_content = state::read_state_file("tasks.md", &config);
    let implement_args = build_implement_args(args.flags.clone(), args.single_agent, false);
    let exit_code = transition_to_implementation(
        task.as_str(),
        &plan_content,
        Some(tasks_content.as_str()),
        mode,
        &implement_args,
        &config,
        &baseline_vec,
        session,
    )?;

    finish_command_exit(started, &config, exit_code)
}

fn plan_tasks_implement_resume(
    args: PlanTasksImplementArgs,
    started: Instant,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let state_dir = session_state_dir(&project_dir, session)?;
    ensure_resume_state_dir_exists(&state_dir)?;
    let config = Config::from_cli(project_dir, args.single_agent, false, session)?;
    let workflow = state::read_workflow(&config).ok_or_else(|| {
        AgentLoopError::State("Cannot resume: workflow state not found.".to_string())
    })?;
    let (mode, effective_flags) =
        resolve_resume_implement_settings(&args.flags, &config, workflow, ResumeOrigin::User)?;
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    let task = read_resume_task_from_state_dir(&state_dir)?;
    let implement_args = build_implement_args(effective_flags.clone(), args.single_agent, false);

    match workflow {
        state::WorkflowKind::Plan => {
            if !phases::planning_phase_resume(&config) {
                return finish_command_exit(started, &config, 1);
            }
            let plan_content = state::read_state_file("plan.md", &config);
            state::init(
                task.as_str(),
                &config,
                &baseline_vec,
                state::WorkflowKind::Decompose,
            )?;
            persist_implement_settings(mode, &effective_flags, &config)?;
            state::write_state_file("plan.md", &plan_content, &config)?;

            if !phases::task_decomposition_phase(&config) {
                return finish_command_exit(started, &config, 1);
            }

            let tasks_content = state::read_state_file("tasks.md", &config);
            let exit_code = transition_to_implementation(
                task.as_str(),
                &plan_content,
                Some(tasks_content.as_str()),
                mode,
                &implement_args,
                &config,
                &baseline_vec,
                session,
            )?;
            finish_command_exit(started, &config, exit_code)
        }
        state::WorkflowKind::Decompose => {
            if !phases::task_decomposition_phase_resume(&config) {
                return finish_command_exit(started, &config, 1);
            }
            let plan_content = state::read_state_file("plan.md", &config);
            let tasks_content = state::read_state_file("tasks.md", &config);
            let exit_code = transition_to_implementation(
                task.as_str(),
                &plan_content,
                Some(tasks_content.as_str()),
                mode,
                &implement_args,
                &config,
                &baseline_vec,
                session,
            )?;
            finish_command_exit(started, &config, exit_code)
        }
        state::WorkflowKind::Implement => {
            let resume_args = build_implement_args(effective_flags, args.single_agent, true);
            let exit_code = resume_implementation_for_mode(
                &resume_args,
                &config,
                &config.project_dir,
                mode,
                false,
                false,
                session,
            )?;
            finish_command_exit(started, &config, exit_code)
        }
    }
}

fn plan_implement_command(args: PlanImplementArgs, session: Option<&str>) -> Result<i32, AgentLoopError> {
    args.validate()?;
    let started = Instant::now();

    if args.resume {
        return plan_implement_resume(args, started, session);
    }

    let task = resolve_compound_task(args.task.as_ref(), args.file.as_ref())?;
    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir, args.single_agent, false, session)?;
    let mode = resolve_fresh_implement_mode(&args.flags, &config);
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };

    reset_state_dir(&config.project_dir, session)?;
    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Plan,
    )?;
    persist_implement_settings(mode, &args.flags, &config)?;

    if !phases::planning_phase(&config, true) {
        return finish_command_exit(started, &config, 1);
    }

    let plan_content = state::read_state_file("plan.md", &config);
    let tasks_content = if mode == ImplementExecutionMode::Batch {
        None
    } else {
        Some(build_plan_fallback_tasks(task.as_str(), &plan_content))
    };
    let implement_args = build_implement_args(args.flags.clone(), args.single_agent, false);
    let exit_code = transition_to_implementation(
        task.as_str(),
        &plan_content,
        tasks_content.as_deref(),
        mode,
        &implement_args,
        &config,
        &baseline_vec,
        session,
    )?;

    finish_command_exit(started, &config, exit_code)
}

fn plan_implement_resume(args: PlanImplementArgs, started: Instant, session: Option<&str>) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let state_dir = session_state_dir(&project_dir, session)?;
    ensure_resume_state_dir_exists(&state_dir)?;
    let config = Config::from_cli(project_dir, args.single_agent, false, session)?;
    let workflow = state::read_workflow(&config).ok_or_else(|| {
        AgentLoopError::State("Cannot resume: workflow state not found.".to_string())
    })?;
    let (mode, effective_flags) =
        resolve_resume_implement_settings(&args.flags, &config, workflow, ResumeOrigin::User)?;
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };

    match workflow {
        state::WorkflowKind::Plan => {
            if !phases::planning_phase_resume(&config) {
                return finish_command_exit(started, &config, 1);
            }
            let task = read_resume_task_from_state_dir(&state_dir)?;
            let plan_content = state::read_state_file("plan.md", &config);
            let tasks_content = if mode == ImplementExecutionMode::Batch {
                None
            } else {
                Some(build_plan_fallback_tasks(task.as_str(), &plan_content))
            };
            let implement_args =
                build_implement_args(effective_flags.clone(), args.single_agent, false);
            let exit_code = transition_to_implementation(
                task.as_str(),
                &plan_content,
                tasks_content.as_deref(),
                mode,
                &implement_args,
                &config,
                &baseline_vec,
                session,
            )?;
            finish_command_exit(started, &config, exit_code)
        }
        state::WorkflowKind::Implement => {
            let resume_args = build_implement_args(effective_flags, args.single_agent, true);
            let exit_code =
                resume_implementation_for_mode(
                    &resume_args,
                    &config,
                    &config.project_dir,
                    mode,
                    false,
                    false,
                    session,
                )?;
            finish_command_exit(started, &config, exit_code)
        }
        state::WorkflowKind::Decompose => Err(AgentLoopError::State(
            "Cannot resume plan-implement: workflow is in decomposition phase. Use `plan-tasks-implement --resume` instead."
                .to_string(),
        )),
    }
}

fn tasks_implement_command(args: TasksImplementArgs, session: Option<&str>) -> Result<i32, AgentLoopError> {
    args.validate()?;
    let started = Instant::now();

    if args.resume {
        return tasks_implement_resume(args, started, session);
    }

    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir.clone(), args.single_agent, false, session)?;
    let plan_content = if let Some(path) = args.file.as_ref() {
        fs::read_to_string(path).map_err(|err| {
            AgentLoopError::Config(format!(
                "Failed to read plan file '{}': {err}",
                path.display()
            ))
        })?
    } else {
        state::read_state_file("plan.md", &config)
    };
    if plan_content.trim().is_empty() {
        return Err(AgentLoopError::Config(
            "No plan.md found. Run 'agent-loop plan' first or provide --file.".to_string(),
        ));
    }

    let task = if args.file.is_some() {
        build_decomposition_task_from_plan(&plan_content)
    } else {
        let existing_task = state::read_state_file("task.md", &config);
        if existing_task.trim().is_empty() {
            build_decomposition_task_from_plan(&plan_content)
        } else {
            existing_task
        }
    };
    let mode = resolve_fresh_implement_mode(&args.flags, &config);

    reset_state_dir(&project_dir, session)?;
    let config = Config::from_cli(project_dir, args.single_agent, false, session)?;
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Decompose,
    )?;
    persist_implement_settings(mode, &args.flags, &config)?;
    state::write_state_file("plan.md", &plan_content, &config)?;

    if !phases::task_decomposition_phase(&config) {
        return finish_command_exit(started, &config, 1);
    }

    let tasks_content = state::read_state_file("tasks.md", &config);
    let implement_args = build_implement_args(args.flags.clone(), args.single_agent, false);
    let exit_code = transition_to_implementation(
        task.as_str(),
        &plan_content,
        Some(tasks_content.as_str()),
        mode,
        &implement_args,
        &config,
        &baseline_vec,
        session,
    )?;

    finish_command_exit(started, &config, exit_code)
}

fn tasks_implement_resume(
    args: TasksImplementArgs,
    started: Instant,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let state_dir = session_state_dir(&project_dir, session)?;
    ensure_resume_state_dir_exists(&state_dir)?;
    let config = Config::from_cli(project_dir, args.single_agent, false, session)?;
    let workflow = state::read_workflow(&config).ok_or_else(|| {
        AgentLoopError::State("Cannot resume: workflow state not found.".to_string())
    })?;
    let (mode, effective_flags) =
        resolve_resume_implement_settings(&args.flags, &config, workflow, ResumeOrigin::User)?;
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };

    match workflow {
        state::WorkflowKind::Decompose => {
            if !phases::task_decomposition_phase_resume(&config) {
                return finish_command_exit(started, &config, 1);
            }
            let task = read_resume_task_from_state_dir(&state_dir)?;
            let plan_content = state::read_state_file("plan.md", &config);
            let tasks_content = state::read_state_file("tasks.md", &config);
            let implement_args =
                build_implement_args(effective_flags.clone(), args.single_agent, false);
            let exit_code = transition_to_implementation(
                task.as_str(),
                &plan_content,
                Some(tasks_content.as_str()),
                mode,
                &implement_args,
                &config,
                &baseline_vec,
                session,
            )?;
            finish_command_exit(started, &config, exit_code)
        }
        state::WorkflowKind::Implement => {
            let resume_args = build_implement_args(effective_flags, args.single_agent, true);
            let exit_code =
                resume_implementation_for_mode(
                    &resume_args,
                    &config,
                    &config.project_dir,
                    mode,
                    false,
                    false,
                    session,
                )?;
            finish_command_exit(started, &config, exit_code)
        }
        state::WorkflowKind::Plan => Err(AgentLoopError::State(
            "Cannot resume tasks-implement: workflow is in planning phase. Use `plan-tasks-implement --resume` or `plan --resume` instead."
                .to_string(),
        )),
    }
}

fn per_task_only_flags_present(args: &ImplementArgs) -> bool {
    args.flags.continue_on_fail
        || args.flags.fail_fast
        || args.flags.max_parallel.is_some()
        || args.flags.max_retries != 2
        || args.flags.round_step != 2
}

/// Remove persisted implementation session IDs in the active state directory.
/// This prevents context leakage across independent per-task runs.
fn clear_implementation_session_cache(config: &Config) -> Result<usize, AgentLoopError> {
    if !config.state_dir.is_dir() {
        return Ok(0);
    }

    let mut cleared = 0usize;
    for entry in fs::read_dir(&config.state_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("implement-") && name.ends_with("_session_id") {
            fs::remove_file(entry.path())?;
            cleared += 1;
        }
    }

    Ok(cleared)
}

fn implement_all_tasks_batch(
    args: &ImplementArgs,
    parsed_tasks: &[ParsedTask],
    config: &Config,
    project_dir: &Path,
    raw_tasks: &str,
    cleanup_on_success: bool,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    implement_all_tasks_batch_using(
        args,
        parsed_tasks,
        config,
        project_dir,
        raw_tasks,
        cleanup_on_success,
        |a, c, p, t, ct, cl| run_batch_implementation_with_title(a, c, p, t, ct, cl, session),
    )
}

fn implement_all_tasks_batch_using<F>(
    args: &ImplementArgs,
    parsed_tasks: &[ParsedTask],
    config: &Config,
    project_dir: &Path,
    raw_tasks: &str,
    cleanup_on_success: bool,
    run_batch: F,
) -> Result<i32, AgentLoopError>
where
    F: FnOnce(&ImplementArgs, &Config, &Path, &str, String, bool) -> Result<i32, AgentLoopError>,
{
    println!("Running batch implementation for all tasks in a single loop.");
    let title = batch_metrics_title(parsed_tasks.len());
    let combined_task = build_batch_implementation_task(raw_tasks);
    run_batch(
        args,
        config,
        project_dir,
        &title,
        combined_task,
        cleanup_on_success,
    )
}

fn implement_plan_batch(
    args: &ImplementArgs,
    config: &Config,
    project_dir: &Path,
    plan: &str,
    cleanup_on_success: bool,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    println!("No tasks found; falling back to plan.md for batch implementation.");
    let original_task = state::read_state_file("task.md", config);
    let combined_task = build_plan_implementation_task(&original_task, plan);
    run_batch_implementation_with_title(
        args,
        config,
        project_dir,
        "Batch implementation (plan fallback)",
        combined_task,
        cleanup_on_success,
        session,
    )
}

fn run_batch_implementation_with_title(
    args: &ImplementArgs,
    config: &Config,
    project_dir: &Path,
    title: &str,
    combined_task: String,
    cleanup_on_success: bool,
    session: Option<&str>,
) -> Result<i32, AgentLoopError> {
    run_batch_implementation_with_title_using(
        args,
        config,
        project_dir,
        title,
        combined_task,
        cleanup_on_success,
        |run_args, opts| run_command_with_review_max_rounds(run_args, opts, session),
    )
}

fn run_batch_implementation_with_title_using<F>(
    args: &ImplementArgs,
    config: &Config,
    project_dir: &Path,
    title: &str,
    combined_task: String,
    cleanup_on_success: bool,
    run_impl: F,
) -> Result<i32, AgentLoopError>
where
    F: FnOnce(RunArgs, ImplementationRunOptions) -> Result<i32, AgentLoopError>,
{
    let started_at = state::timestamp();
    let timer = Instant::now();
    let usage_before = agent::usage_snapshot();
    let run_result = run_impl(
        RunArgs {
            task: Some(combined_task),
            file: None,
            resume: false,
            planning_only: false,
            single_agent: args.single_agent,
        },
        ImplementationRunOptions {
            review_max_rounds_override: None,
            clear_progress: true,
            write_mode: Some(ImplementExecutionMode::Batch),
            persist_flags: Some(args.flags.clone()),
            cleanup_on_success,
        },
    );
    let usage_after = agent::usage_snapshot();
    let usage = usage_after.saturating_sub(usage_before);
    let ended_at = state::timestamp();
    let duration_ms = timer.elapsed().as_millis() as u64;

    let (status, last_error) = match &run_result {
        Ok(0) => (TaskRunStatus::Done, None),
        Ok(_) => (
            TaskRunStatus::Failed,
            Some(format_status_reason(
                read_current_status(project_dir, args.single_agent, config.session.as_deref()).as_ref(),
            )),
        ),
        Err(err) => (TaskRunStatus::Failed, Some(err.to_string())),
    };

    persist_batch_task_state(
        title,
        status,
        last_error,
        started_at,
        ended_at,
        duration_ms,
        usage,
        config,
    )?;

    run_result
}

fn implement_all_tasks_wave(
    args: &ImplementArgs,
    parsed_tasks: &[ParsedTask],
    config: &Config,
    _project_dir: &Path,
) -> Result<i32, AgentLoopError> {
    use std::sync::mpsc;

    let deps: Vec<Vec<usize>> = parsed_tasks
        .iter()
        .map(|t| t.dependencies.clone())
        .collect();
    let schedule = wave::compute_wave_schedule(parsed_tasks.len(), &deps)
        .map_err(|e| AgentLoopError::Wave(e.to_string()))?;

    let effective_max_parallel = args
        .flags
        .max_parallel
        .unwrap_or(config.max_parallel)
        .max(1) as usize;

    // Acquire wave lock to prevent concurrent wave runs.
    let lock_path = config.wave_lock_path();
    let wave_lock = wave_runtime::WaveRunLock::acquire(
        lock_path,
        "wave",
        effective_max_parallel as u32,
        config.wave_lock_stale_seconds,
    )
    .map_err(AgentLoopError::Wave)?;

    // Journal file for progress events (append-only, survives reset).
    let journal_path = config.wave_journal_path();

    println!(
        "Wave mode: {} task(s), {} wave(s), max_parallel={}",
        parsed_tasks.len(),
        schedule.waves.len(),
        effective_max_parallel
    );
    if effective_max_parallel > 1 {
        eprintln!("⚠ Tasks in the same wave should modify different files to avoid conflicts.");
    }

    // Journal: RunStart event.
    let _ = wave_runtime::append_journal_event(
        &journal_path,
        &wave_runtime::WaveProgressEvent::RunStart {
            timestamp: state::timestamp(),
            max_parallel: effective_max_parallel as u32,
            total_tasks: parsed_tasks.len(),
            total_waves: schedule.waves.len(),
        },
    );

    // Initialize wave statuses; on fresh run start all Pending, on --resume reuse Done entries.
    let mut wave_statuses: Vec<TaskStatusEntry> = parsed_tasks
        .iter()
        .enumerate()
        .map(|(i, t)| TaskStatusEntry {
            title: t.title.clone(),
            status: TaskRunStatus::Pending,
            retries: 0,
            last_error: None,
            skip_reason: None,
            wave_index: schedule.task_wave.get(i).copied(),
        })
        .collect();

    if args.resume {
        // Resume: load persisted statuses; Done tasks are kept, non-done tasks are reset.
        let persisted = state::read_task_status(config);
        for (i, entry) in persisted.tasks.iter().enumerate() {
            if i < wave_statuses.len() && entry.title == wave_statuses[i].title {
                wave_statuses[i] = entry.clone();
            }
        }
    } else {
        // Fresh run: overwrite any stale persisted state so reset is authoritative.
        persist_wave_statuses(&wave_statuses, config)?;
    }

    let mut task_results: Vec<Option<bool>> = vec![None; parsed_tasks.len()];
    // Pre-fill results from persisted state; reset non-done tasks so they are re-evaluated.
    for (i, entry) in wave_statuses.iter_mut().enumerate() {
        if entry.status == TaskRunStatus::Done {
            task_results[i] = Some(true);
        } else if entry.status == TaskRunStatus::Skipped || entry.status == TaskRunStatus::Failed {
            entry.status = TaskRunStatus::Pending;
            entry.skip_reason = None;
            entry.last_error = None;
        }
    }
    let mut had_failures = false;
    let run_start = std::time::Instant::now();

    for (wave_idx, wave_tasks) in schedule.waves.iter().enumerate() {
        // Check for interrupt before starting each wave.
        if interrupt::is_interrupted() {
            terminalize_interrupted_tasks(&mut wave_statuses);
            persist_wave_statuses(&wave_statuses, config)?;
            let _ = wave_runtime::append_journal_event(
                &journal_path,
                &wave_runtime::WaveProgressEvent::RunInterrupted {
                    timestamp: state::timestamp(),
                    reason: format!("Interrupted before wave {}", wave_idx + 1),
                },
            );
            wave_lock.release();
            println!("\nWave run interrupted.");
            return Ok(1);
        }

        // Skip tasks whose dependencies failed or that are already done.
        let mut runnable: Vec<usize> = Vec::new();
        for &task_idx in wave_tasks {
            // Already done — skip.
            if task_results[task_idx] == Some(true) {
                println!(
                    "✅ '{}' — already done, skipping",
                    parsed_tasks[task_idx].title
                );
                continue;
            }

            let failed_deps: Vec<&str> = deps[task_idx]
                .iter()
                .filter(|&&dep| task_results[dep] != Some(true))
                .map(|&dep| parsed_tasks[dep].title.as_str())
                .collect();
            if failed_deps.is_empty() {
                runnable.push(task_idx);
            } else {
                let reason = format!("dependency failed: {}", failed_deps.join(", "));
                println!("⏭ Skipping '{}' — {}", parsed_tasks[task_idx].title, reason);
                task_results[task_idx] = Some(false);
                wave_statuses[task_idx].status = TaskRunStatus::Skipped;
                wave_statuses[task_idx].skip_reason = Some(reason);
                persist_wave_statuses(&wave_statuses, config)?;
                had_failures = true;
            }
        }

        if runnable.is_empty() {
            continue;
        }

        println!(
            "\n━━━ Wave {}/{} ({} task{}) ━━━",
            wave_idx + 1,
            schedule.waves.len(),
            runnable.len(),
            if runnable.len() == 1 { "" } else { "s" }
        );

        // Journal: WaveStart event.
        let _ = wave_runtime::append_journal_event(
            &journal_path,
            &wave_runtime::WaveProgressEvent::WaveStart {
                timestamp: state::timestamp(),
                wave_index: wave_idx,
                task_count: runnable.len(),
            },
        );

        // Execute tasks in this wave, up to max_parallel at a time.
        let (tx, rx) = mpsc::channel::<(usize, bool)>();

        let mut in_flight = 0usize;
        let mut runnable_iter = runnable.iter();
        let mut wave_passed = 0usize;
        let mut wave_failed = 0usize;

        // Launch initial batch (check interrupt before each spawn).
        while in_flight < effective_max_parallel {
            if interrupt::is_interrupted() {
                break;
            }
            if let Some(&task_idx) = runnable_iter.next() {
                let _ = wave_runtime::append_journal_event(
                    &journal_path,
                    &wave_runtime::WaveProgressEvent::TaskStart {
                        timestamp: state::timestamp(),
                        wave_index: wave_idx,
                        task_index: task_idx,
                        title: parsed_tasks[task_idx].title.clone(),
                    },
                );
                launch_wave_task(task_idx, parsed_tasks, config, args, &tx);
                in_flight += 1;
            } else {
                break;
            }
        }

        // Collect results and launch more as slots free up.
        while in_flight > 0 {
            let (task_idx, succeeded) = rx
                .recv()
                .map_err(|e| AgentLoopError::Wave(format!("wave task channel error: {e}")))?;
            in_flight -= 1;
            task_results[task_idx] = Some(succeeded);

            let _ = wave_runtime::append_journal_event(
                &journal_path,
                &wave_runtime::WaveProgressEvent::TaskEnd {
                    timestamp: state::timestamp(),
                    wave_index: wave_idx,
                    task_index: task_idx,
                    title: parsed_tasks[task_idx].title.clone(),
                    success: succeeded,
                },
            );

            if succeeded {
                println!("✅ Completed: {}", parsed_tasks[task_idx].title);
                wave_statuses[task_idx].status = TaskRunStatus::Done;
                wave_passed += 1;
            } else {
                println!("❌ Failed: {}", parsed_tasks[task_idx].title);
                wave_statuses[task_idx].status = TaskRunStatus::Failed;
                wave_failed += 1;
                had_failures = true;
            }
            persist_wave_statuses(&wave_statuses, config)?;

            if !succeeded && args.flags.fail_fast {
                // Drain remaining in-flight results, capturing outcomes.
                while in_flight > 0 {
                    if let Ok((idx, ok)) = rx.recv() {
                        task_results[idx] = Some(ok);
                        wave_statuses[idx].status = if ok {
                            TaskRunStatus::Done
                        } else {
                            TaskRunStatus::Failed
                        };
                    }
                    in_flight -= 1;
                }
                // Mark any tasks that were never started or still pending as skipped.
                for (i, result) in task_results.iter().enumerate() {
                    if result.is_none() {
                        wave_statuses[i].status = TaskRunStatus::Skipped;
                        wave_statuses[i].skip_reason = Some("fail-fast".to_string());
                    }
                }
                persist_wave_statuses(&wave_statuses, config)?;
                // wave_lock released by Drop
                println!("Aborting (--fail-fast).");
                return Ok(1);
            }

            // Check interrupt before launching next task.
            if interrupt::is_interrupted() {
                // Drain remaining in-flight with a grace period.
                let grace = std::time::Duration::from_millis(config.wave_shutdown_grace_ms);
                let deadline = std::time::Instant::now() + grace;
                while in_flight > 0 {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    match rx.recv_timeout(remaining) {
                        Ok((idx, ok)) => {
                            in_flight -= 1;
                            task_results[idx] = Some(ok);
                            wave_statuses[idx].status = if ok {
                                TaskRunStatus::Done
                            } else {
                                TaskRunStatus::Failed
                            };
                        }
                        Err(_) => {
                            // Grace period expired; mark remaining in-flight as interrupted.
                            break;
                        }
                    }
                }
                terminalize_interrupted_tasks(&mut wave_statuses);
                persist_wave_statuses(&wave_statuses, config)?;
                let _ = wave_runtime::append_journal_event(
                    &journal_path,
                    &wave_runtime::WaveProgressEvent::RunInterrupted {
                        timestamp: state::timestamp(),
                        reason: format!("Interrupted during wave {}", wave_idx + 1),
                    },
                );
                wave_lock.release();
                println!("\nWave run interrupted.");
                return Ok(1);
            }

            // Launch next task if available.
            if let Some(&next_idx) = runnable_iter.next() {
                let _ = wave_runtime::append_journal_event(
                    &journal_path,
                    &wave_runtime::WaveProgressEvent::TaskStart {
                        timestamp: state::timestamp(),
                        wave_index: wave_idx,
                        task_index: next_idx,
                        title: parsed_tasks[next_idx].title.clone(),
                    },
                );
                launch_wave_task(next_idx, parsed_tasks, config, args, &tx);
                in_flight += 1;
            }
        }

        // Journal: WaveEnd event.
        let _ = wave_runtime::append_journal_event(
            &journal_path,
            &wave_runtime::WaveProgressEvent::WaveEnd {
                timestamp: state::timestamp(),
                wave_index: wave_idx,
                passed: wave_passed,
                failed: wave_failed,
            },
        );

        // Git checkpoint at wave boundary (serialized, safe).
        if config.auto_commit && git::is_git_repo(config) {
            let msg = format!("wave-{}-complete", wave_idx + 1);
            let baseline = git::list_changed_files(config)
                .unwrap_or_default()
                .into_iter()
                .collect();
            git::git_checkpoint(&msg, config, &baseline);
        }
    }

    // Derive summary counts from wave_statuses (authoritative) so that tasks
    // skipped due to dependency failures are counted as skipped, not failed.
    let total_passed = wave_statuses
        .iter()
        .filter(|s| s.status == TaskRunStatus::Done)
        .count();
    let total_failed = wave_statuses
        .iter()
        .filter(|s| s.status == TaskRunStatus::Failed)
        .count();
    let total_skipped = wave_statuses
        .iter()
        .filter(|s| s.status == TaskRunStatus::Skipped)
        .count();

    // Journal: RunEnd event.
    let _ = wave_runtime::append_journal_event(
        &journal_path,
        &wave_runtime::WaveProgressEvent::RunEnd {
            timestamp: state::timestamp(),
            total_passed,
            total_failed,
            total_skipped,
        },
    );

    // Release wave lock on clean exit.
    wave_lock.release();

    println!();
    if had_failures {
        println!(
            "Wave execution completed with failures ({} passed, {} failed, {} skipped, {:.1}s).",
            total_passed,
            total_failed,
            total_skipped,
            run_start.elapsed().as_secs_f64()
        );
        Ok(1)
    } else {
        println!(
            "All waves completed successfully ({} passed, {:.1}s).",
            total_passed,
            run_start.elapsed().as_secs_f64()
        );
        Ok(0)
    }
}

fn launch_wave_task(
    task_idx: usize,
    parsed_tasks: &[ParsedTask],
    config: &Config,
    args: &ImplementArgs,
    tx: &std::sync::mpsc::Sender<(usize, bool)>,
) {
    let task_title = parsed_tasks[task_idx].title.clone();
    let task_content = parsed_tasks[task_idx].content.clone();
    let task_state_dir = config.state_dir.join(format!(".wave-task-{}", task_idx + 1));
    let task_config = config.with_state_dir(task_state_dir);
    let clear_progress = !args.resume;
    let tx = tx.clone();

    println!("🚀 Starting: {task_title}");

    std::thread::spawn(move || {
        // Ensure task state directory exists.
        let _ = std::fs::create_dir_all(&task_config.state_dir);

        let result = run_command_with_config(task_content, &task_config, clear_progress);
        let succeeded = matches!(result, Ok(0));
        let _ = tx.send((task_idx, succeeded));
    });
}

/// Run a single implementation task with an explicit config (used by wave orchestrator).
fn run_command_with_config(
    task: String,
    config: &Config,
    clear_progress: bool,
) -> Result<i32, AgentLoopError> {
    let baseline_vec = if git::is_git_repo(config) {
        git::list_changed_files(config)?
    } else {
        Vec::new()
    };
    let baseline_set: HashSet<String> = baseline_vec.iter().cloned().collect();

    state::init(&task, config, &baseline_vec, state::WorkflowKind::Implement)?;
    write_implement_mode(ImplementExecutionMode::Wave, config)?;
    if clear_progress {
        state::clear_implement_progress(config);
    }
    state::write_state_file("task.md", &task, config)?;
    state::write_workflow(state::WorkflowKind::Implement, config)?;

    let mut config = config.clone();
    preflight::run_preflight(&mut config)?;

    let succeeded = phases::implementation_loop(&config, &baseline_set);
    Ok(phase_success_to_exit_code(succeeded))
}

fn implement_all_tasks_per_task(
    args: &ImplementArgs,
    parsed_tasks: &[ParsedTask],
    config: &Config,
    project_dir: &Path,
) -> Result<i32, AgentLoopError> {
    println!("Running per-task implementation mode.");
    if !args.resume {
        state::clear_implement_progress(config);
    }
    let base_review_max_rounds = config.review_max_rounds;

    // Resolve effective max_parallel: CLI > config > default(1).
    let effective_max_parallel = args.flags.max_parallel.unwrap_or(config.max_parallel);
    if effective_max_parallel > 1 {
        eprintln!(
            "Parallel task execution is not yet supported; running sequentially with max_parallel=1"
        );
    }

    // Reconcile persisted task status and metrics with current task list.
    let mut task_statuses = reconcile_task_status(parsed_tasks, config);
    let mut task_metrics = reconcile_task_metrics(parsed_tasks, config);

    let mut had_failures = false;

    for (index, task) in parsed_tasks.iter().enumerate() {
        println!();
        println!("[{}/{}] {}", index + 1, parsed_tasks.len(), task.title);

        // Copy entry fields to avoid borrow conflicts.
        let entry_status = task_statuses[index].status;
        let persisted_retries = task_statuses[index].retries;

        // Skip tasks that are already done.
        if entry_status == TaskRunStatus::Done {
            println!("{} — already done, skipping", task.title);
            continue;
        }

        // In continue-on-fail mode, skip previously failed tasks.
        if args.flags.continue_on_fail && entry_status == TaskRunStatus::Failed {
            println!("{} — previously failed, skipping", task.title);
            task_statuses[index].status = TaskRunStatus::Skipped;
            persist_task_state(&task_statuses, &task_metrics, config)?;
            had_failures = true;
            continue;
        }

        // Check retry budget: for failed tasks, retries >= max_retries means exhausted.
        // For running tasks, retries > max_retries means truly exhausted (beyond boundary).
        // retries == max_retries for running means final attempt allowed.
        if entry_status == TaskRunStatus::Failed && persisted_retries >= args.flags.max_retries {
            // Exhausted — do not re-execute.
            if args.flags.continue_on_fail {
                println!("{} — retries exhausted, skipping", task.title);
                task_statuses[index].status = TaskRunStatus::Skipped;
                persist_task_state(&task_statuses, &task_metrics, config)?;
                had_failures = true;
                continue;
            } else {
                persist_task_state(&task_statuses, &task_metrics, config)?;
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' failed with retries exhausted ({persisted_retries}/{}).",
                    task.title, args.flags.max_retries
                )));
            }
        }

        if entry_status == TaskRunStatus::Running && persisted_retries > args.flags.max_retries {
            // Running task beyond retry boundary — mark failed immediately.
            task_statuses[index].status = TaskRunStatus::Failed;
            persist_task_state(&task_statuses, &task_metrics, config)?;
            if args.flags.continue_on_fail {
                had_failures = true;
                continue;
            } else {
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' failed with retries exhausted ({persisted_retries}/{}).",
                    task.title, args.flags.max_retries
                )));
            }
        }

        // Start untouched/skipped tasks with fresh implementation sessions so
        // previous task context cannot leak via CLI --resume.
        if entry_status == TaskRunStatus::Pending || entry_status == TaskRunStatus::Skipped {
            let cleared = clear_implementation_session_cache(config)?;
            if cleared > 0 {
                println!("Cleared {cleared} cached implementation session(s).");
            }
            state::append_implement_progress_task(&task.title, config);
        }

        // Mark as running and clear stale metrics for re-execution.
        task_statuses[index].status = TaskRunStatus::Running;
        let start_ts = state::timestamp();
        task_metrics[index] = TaskMetricsEntry {
            title: task.title.clone(),
            task_started_at: Some(start_ts),
            task_ended_at: None,
            duration_ms: None,
            agent_calls: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cost_usd_micros: None,
        };
        persist_task_state(&task_statuses, &task_metrics, config)?;

        let timer = Instant::now();

        // Determine initial retry count from persisted state.
        let mut retry = persisted_retries;
        let is_resume_initial = entry_status == TaskRunStatus::Running;
        let mut current_review_max_rounds = if is_resume_initial {
            base_review_max_rounds.saturating_add(args.flags.round_step.saturating_mul(retry))
        } else {
            base_review_max_rounds
        };
        let mut accumulated_usage = agent::UsageSnapshot::default();

        let mut task_succeeded = false;
        let mut first_attempt = true;

        loop {
            let is_resume = !first_attempt || is_resume_initial;
            if !first_attempt {
                println!(
                    "Retrying with REVIEW_MAX_ROUNDS={} (retry {}/{})",
                    current_review_max_rounds, retry, args.flags.max_retries
                );
            } else if is_resume_initial {
                println!(
                    "Resuming with REVIEW_MAX_ROUNDS={} (retry {}/{})",
                    current_review_max_rounds, retry, args.flags.max_retries
                );
            } else {
                println!("Running with REVIEW_MAX_ROUNDS={current_review_max_rounds}");
            }

            let usage_before_attempt = agent::usage_snapshot();
            let exit_code = if is_resume {
                resume_for_tasks(args.single_agent, Some(current_review_max_rounds), config.session.as_deref())?
            } else {
                let run_args = RunArgs {
                    task: Some(task.content.clone()),
                    file: None,
                    resume: false,
                    planning_only: false,
                    single_agent: args.single_agent,
                };
                run_command_with_review_max_rounds(
                    run_args,
                    ImplementationRunOptions {
                        review_max_rounds_override: Some(current_review_max_rounds),
                        clear_progress: false,
                        write_mode: None,
                        persist_flags: None,
                        cleanup_on_success: true,
                    },
                    config.session.as_deref(),
                )?
            };
            let usage_after_attempt = agent::usage_snapshot();
            accumulated_usage = accumulated_usage
                .saturating_add(usage_after_attempt.saturating_sub(usage_before_attempt));
            apply_task_usage(&mut task_metrics[index], accumulated_usage);
            if exit_code == 0 {
                println!("Task completed: {}", task.title);
                task_succeeded = true;
                break;
            }

            let status = read_current_status(project_dir, args.single_agent, config.session.as_deref());
            if !is_retryable_run_tasks_status(status.as_ref()) {
                // Non-retryable failure.
                break;
            }

            if retry >= args.flags.max_retries {
                // Retry budget exhausted.
                break;
            }

            if let Some(status_value) = status.as_ref()
                && status_value.status == Status::Error
            {
                println!(
                    "Retrying '{}' after timeout error: {}",
                    task.title,
                    status_value.reason.as_deref().unwrap_or("timeout")
                );
            }

            retry += 1;
            first_attempt = false;
            current_review_max_rounds =
                current_review_max_rounds.saturating_add(args.flags.round_step);

            // Persist updated retry count.
            task_statuses[index].retries = retry;
            persist_task_state(&task_statuses, &task_metrics, config)?;
        }

        let elapsed = timer.elapsed();
        let end_ts = state::timestamp();
        task_metrics[index].task_ended_at = Some(end_ts);
        task_metrics[index].duration_ms = Some(elapsed.as_millis() as u64);
        apply_task_usage(&mut task_metrics[index], accumulated_usage);

        if task_succeeded {
            task_statuses[index].status = TaskRunStatus::Done;
            task_statuses[index].retries = retry;
            task_statuses[index].last_error = None;
        } else {
            task_statuses[index].status = TaskRunStatus::Failed;
            task_statuses[index].retries = retry;
            let status = read_current_status(project_dir, args.single_agent, config.session.as_deref());
            task_statuses[index].last_error = Some(format_status_reason(status.as_ref()));
            had_failures = true;

            if !args.flags.continue_on_fail {
                // Fail-fast: persist and exit.
                persist_task_state(&task_statuses, &task_metrics, config)?;
                print_task_duration_summary(&task_metrics);
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' failed with status {}.",
                    task.title,
                    format_status_reason(status.as_ref())
                )));
            }
        }

        persist_task_state(&task_statuses, &task_metrics, config)?;
    }

    print_task_duration_summary(&task_metrics);
    println!();
    if had_failures {
        println!("Tasks completed with failures.");
        Ok(1)
    } else {
        println!("All tasks completed.");
        Ok(0)
    }
}

fn persist_task_state(
    statuses: &[TaskStatusEntry],
    metrics: &[TaskMetricsEntry],
    config: &Config,
) -> Result<(), AgentLoopError> {
    state::write_task_status(
        &TaskStatusFile {
            tasks: statuses.to_vec(),
        },
        config,
    )?;
    state::write_task_metrics(
        &TaskMetricsFile {
            tasks: metrics.to_vec(),
        },
        config,
    )?;
    Ok(())
}

fn persist_wave_statuses(
    statuses: &[TaskStatusEntry],
    config: &Config,
) -> Result<(), AgentLoopError> {
    Ok(state::write_task_status(
        &TaskStatusFile {
            tasks: statuses.to_vec(),
        },
        config,
    )?)
}

/// Mark any Pending or Running tasks as Skipped with an "interrupted" reason,
/// so that `--resume` knows they were never completed.
fn terminalize_interrupted_tasks(statuses: &mut [TaskStatusEntry]) {
    for entry in statuses.iter_mut() {
        if entry.status == TaskRunStatus::Pending || entry.status == TaskRunStatus::Running {
            entry.status = TaskRunStatus::Skipped;
            entry.skip_reason = Some("interrupted".to_string());
        }
    }
}

fn reset_state_dir(project_dir: &Path, session: Option<&str>) -> Result<(), AgentLoopError> {
    if let Some(name) = session {
        config::validate_session_name(name)?;
    }
    let state_dir = match session {
        Some(name) => project_dir.join(".agent-loop").join("state").join(name),
        None => project_dir.join(".agent-loop").join("state"),
    };
    match session {
        Some(_) => {
            // Session-scoped: remove the session directory entirely
            if state_dir.is_dir() {
                fs::remove_dir_all(&state_dir)?;
            }
            fs::create_dir_all(&state_dir)?;
        }
        None => {
            // Default: remove top-level files and wave task dirs, preserve session dirs
            if state_dir.is_dir() {
                let agent_loop_dir = project_dir.join(".agent-loop");
                let sentinel_exists =
                    state::wave_task_migration_sentinel(&agent_loop_dir).exists();
                for entry in fs::read_dir(&state_dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() {
                        fs::remove_file(&path)?;
                    } else if path.is_dir() {
                        let name = entry.file_name();
                        let name_str = name.to_string_lossy();
                        if name_str.starts_with(".wave-task-") {
                            fs::remove_dir_all(&path)?;
                        } else if !sentinel_exists
                            && state::is_legacy_wave_task_dir(&name_str, &path)
                        {
                            // Pre-sentinel only: safe because no --session dirs exist yet.
                            // Post-sentinel: any task-N dir could be a session.
                            fs::remove_dir_all(&path)?;
                        }
                        // Other subdirectories (session namespaces) are left untouched
                    }
                }
            } else {
                fs::create_dir_all(&state_dir)?;
            }
        }
    }
    Ok(())
}

fn reset_command(args: &ResetArgs, session: Option<&str>) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;

    if args.wave_lock {
        // Lock lives in .agent-loop/ (parent of state/) to survive reset.
        let lock_path = session_wave_lock_path(&project_dir, session)?;
        if lock_path.exists() {
            fs::remove_file(&lock_path)?;
            println!("Wave lock removed.");
        } else {
            println!("No wave lock found.");
        }
        return Ok(0);
    }

    reset_state_dir(&project_dir, session)?;
    println!("State cleared. decisions.md preserved.");
    Ok(0)
}

fn is_terminal_status(status: &state::Status) -> bool {
    matches!(
        status,
        state::Status::MaxRounds
            | state::Status::Stuck
            | state::Status::Error
            | state::Status::Interrupted
    )
}

fn has_stale_reason(reason: Option<&str>) -> bool {
    reason
        .map(|r| {
            let lower = r.to_ascii_lowercase();
            lower.contains("stale") || lower.contains("timestamp")
        })
        .unwrap_or(false)
}

fn resume_hint_for_workflow(workflow: Option<state::WorkflowKind>) -> &'static str {
    match workflow {
        Some(state::WorkflowKind::Plan) => {
            "Hint: run `agent-loop plan --resume`, `agent-loop plan-tasks-implement --resume`, or `agent-loop plan-implement --resume` to continue."
        }
        Some(state::WorkflowKind::Decompose) => {
            "Hint: run `agent-loop tasks --resume`, `agent-loop plan-tasks-implement --resume`, or `agent-loop tasks-implement --resume` to continue."
        }
        Some(state::WorkflowKind::Implement) => {
            "Hint: run `agent-loop implement --resume`, `agent-loop plan-tasks-implement --resume`, `agent-loop plan-implement --resume`, or `agent-loop tasks-implement --resume` to continue."
        }
        None => {
            "Hint: run `agent-loop implement --resume`, `agent-loop tasks --resume`, or `agent-loop reset` to continue."
        }
    }
}

fn task_local_implement_progress_paths(config: &Config) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(entries) = fs::read_dir(&config.state_dir) else {
        return paths;
    };

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(".wave-task-") {
            continue;
        }
        let progress_path = entry.path().join("implement-progress.md");
        if progress_path.is_file() {
            paths.push(progress_path);
        }
    }

    paths.sort();
    paths
}

fn status_command(session: Option<&str>) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir, false, false, session)?;
    let status_path = config.state_dir.join("status.json");

    if !config.state_dir.is_dir() || !status_path.is_file() {
        println!("not initialized");
        return Ok(0);
    }

    let result = state::read_status_with_warnings(&config);
    let current_status = result.status;
    let warnings = result.warnings;

    println!("status: {}", current_status.status);
    println!("round: {}", current_status.round);
    println!("implementer: {}", current_status.implementer);
    println!("reviewer: {}", current_status.reviewer);
    println!("mode: {}", current_status.mode);
    println!("lastRunTask: {}", current_status.last_run_task);
    if let Some(reason) = current_status
        .reason
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        // Extract gate-source tag if present (e.g. "[gate:reviewer]")
        if let Some(gate) = reason
            .strip_prefix("[gate:")
            .and_then(|rest| rest.split_once(']'))
        {
            println!("gate: {}", gate.0);
            let rest = gate.1.trim();
            if !rest.is_empty() {
                println!("reason: {rest}");
            }
        } else {
            println!("reason: {reason}");
        }
    }
    println!("timestamp: {}", current_status.timestamp);

    // Print warnings section if there were parse/validation issues.
    if !warnings.is_empty() {
        println!();
        println!("Warnings:");
        for w in &warnings {
            println!("  - {w}");
        }
        println!();
        println!("Hint: status.json may be corrupted. Run `agent-loop reset` to reset state.");
    }

    // Print resume/init hints for terminal or stale statuses.
    let show_resume = is_terminal_status(&current_status.status)
        || has_stale_reason(current_status.reason.as_deref());
    let workflow = state::read_workflow(&config);

    if show_resume {
        if warnings.is_empty() {
            println!();
        }
        println!("{}", resume_hint_for_workflow(workflow));
    }

    // Wave lock info. Durable artifacts live in .agent-loop/ (parent of state/).
    let lock_path = config.wave_lock_path();
    if lock_path.exists()
        && let Ok(raw) = fs::read_to_string(&lock_path)
        && let Ok(lock) = serde_json::from_str::<wave_runtime::LockFileContent>(&raw)
    {
        println!();
        println!(
            "wave lock: PID {} since {} (mode={}, parallel={})",
            lock.pid, lock.started_at, lock.mode, lock.max_parallel
        );
        if !wave_runtime::is_pid_alive(lock.pid) {
            println!(
                "  ⚠ Lock holder (PID {}) is dead. Run `agent-loop reset --wave-lock` to clear.",
                lock.pid
            );
        }
    }

    // One-time migration for read-only entry points that bypass state::init.
    let agent_loop_dir = config.agent_loop_dir();
    let base_state_dir = agent_loop_dir.join("state");
    state::migrate_legacy_wave_task_dirs(&base_state_dir, &agent_loop_dir)?;

    // Planning artifacts.
    let planning_progress_path = config.state_dir.join("planning-progress.md");
    let tasks_findings_path = config.state_dir.join(TASKS_FINDINGS_FILENAME);
    let tasks_progress_path = config.state_dir.join("tasks-progress.md");
    let implement_progress_path = config.state_dir.join("implement-progress.md");
    let conversation_path = config.state_dir.join("conversation.md");
    let findings_path = config.state_dir.join(state::FINDINGS_FILENAME);
    let task_status_path = config.state_dir.join("task_status.json");
    let task_metrics_path = config.state_dir.join("task_metrics.json");
    let task_local_progress = task_local_implement_progress_paths(&config);
    if planning_progress_path.exists() || tasks_findings_path.exists() {
        println!();
        println!("Planning artifacts:");
        if planning_progress_path.exists() {
            println!("  - {}", planning_progress_path.display());
        }
        if tasks_findings_path.exists() {
            println!("  - {}", tasks_findings_path.display());
        }
    }

    if tasks_progress_path.exists() {
        println!();
        println!("Tasks artifacts:");
        println!("  - {}", tasks_progress_path.display());
    }

    if implement_progress_path.exists()
        || !task_local_progress.is_empty()
        || conversation_path.exists()
        || findings_path.exists()
        || task_status_path.exists()
        || task_metrics_path.exists()
    {
        println!();
        println!("Implementation artifacts:");
        if implement_progress_path.exists() {
            println!("  - {}", implement_progress_path.display());
        }
        for path in &task_local_progress {
            println!("  - {}", path.display());
        }
        if conversation_path.exists() {
            println!("  - {}", conversation_path.display());
        }
        if findings_path.exists() {
            println!("  - {}", findings_path.display());
        }
        if task_status_path.exists() {
            println!("  - {}", task_status_path.display());
        }
        if task_metrics_path.exists() {
            println!("  - {}", task_metrics_path.display());
        }
    }

    // Recent wave progress events (journal also lives in .agent-loop/).
    let journal_path = config.wave_journal_path();
    let recent = wave_runtime::read_recent_events(&journal_path, 5);
    if !recent.is_empty() {
        println!();
        println!("Recent wave events:");
        for event in &recent {
            match event {
                wave_runtime::WaveProgressEvent::RunStart {
                    timestamp,
                    total_tasks,
                    total_waves,
                    max_parallel,
                } => {
                    println!(
                        "  [{timestamp}] RunStart: {total_tasks} tasks, {total_waves} waves, parallel={max_parallel}"
                    );
                }
                wave_runtime::WaveProgressEvent::WaveStart {
                    timestamp,
                    wave_index,
                    task_count,
                } => {
                    println!(
                        "  [{timestamp}] WaveStart: wave {}, {} tasks",
                        wave_index + 1,
                        task_count
                    );
                }
                wave_runtime::WaveProgressEvent::TaskEnd {
                    timestamp,
                    task_index,
                    title,
                    success,
                    ..
                } => {
                    let icon = if *success { "ok" } else { "FAIL" };
                    println!(
                        "  [{timestamp}] TaskEnd: task {} '{}' — {icon}",
                        task_index + 1,
                        title
                    );
                }
                wave_runtime::WaveProgressEvent::WaveEnd {
                    timestamp,
                    wave_index,
                    passed,
                    failed,
                } => {
                    println!(
                        "  [{timestamp}] WaveEnd: wave {} — {passed} passed, {failed} failed",
                        wave_index + 1
                    );
                }
                wave_runtime::WaveProgressEvent::RunEnd {
                    timestamp,
                    total_passed,
                    total_failed,
                    total_skipped,
                } => {
                    println!(
                        "  [{timestamp}] RunEnd: {total_passed} passed, {total_failed} failed, {total_skipped} skipped"
                    );
                }
                wave_runtime::WaveProgressEvent::RunInterrupted { timestamp, reason } => {
                    println!("  [{timestamp}] RunInterrupted: {reason}");
                }
                wave_runtime::WaveProgressEvent::TaskStart {
                    timestamp,
                    task_index,
                    title,
                    ..
                } => {
                    println!(
                        "  [{timestamp}] TaskStart: task {} '{}'",
                        task_index + 1,
                        title
                    );
                }
            }
        }
    }

    Ok(0)
}

fn version_command() -> Result<i32, AgentLoopError> {
    println!("agent-loop {}", env!("CARGO_PKG_VERSION"));
    Ok(0)
}

fn config_init_command(args: ConfigInitArgs) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir().map_err(|e| {
        AgentLoopError::Config(format!("failed to determine project directory: {e}"))
    })?;
    config_init_to_dir(&args, &project_dir)
}

fn config_init_to_dir(args: &ConfigInitArgs, project_dir: &Path) -> Result<i32, AgentLoopError> {
    let config_path = project_dir.join(".agent-loop.toml");

    if config_path.exists() && !args.force {
        eprintln!(
            "Error: {} already exists. Use --force to overwrite.",
            config_path.display()
        );
        return Ok(1);
    }

    let template = config::generate_default_config_template();
    fs::write(&config_path, template).map_err(|err| {
        AgentLoopError::Config(format!("failed to write {}: {err}", config_path.display()))
    })?;

    println!("Generated .agent-loop.toml with defaults.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestConfigOptions, create_temp_project_root, make_test_config};

    fn os_argv(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn normalize_argv_does_not_inject_legacy_run() {
        let normalized = normalize_argv(os_argv(&["agent-loop", "plain", "text"]));
        assert_eq!(normalized, os_argv(&["agent-loop", "plain", "text"]));
    }

    #[test]
    fn dispatch_tasks_with_removed_tasks_file_returns_config_error() {
        let cli = Cli::try_parse_from(["agent-loop", "tasks", "--tasks-file", "old.md"])
            .expect("tasks --tasks-file should parse");
        let result = dispatch_from_cli(cli);
        assert!(
            matches!(result, Err(AgentLoopError::Config(ref msg)) if msg.contains("--tasks-file")),
            "expected Config error about --tasks-file, got: {result:?}"
        );
    }

    #[test]
    fn parse_removed_run_command_is_error() {
        let result = Cli::try_parse_from(["agent-loop", "run", "some-task"]);
        assert!(result.is_err(), "removed 'run' command should not parse");
    }

    #[test]
    fn parse_removed_run_tasks_command_is_error() {
        let result = Cli::try_parse_from(["agent-loop", "run-tasks"]);
        assert!(
            result.is_err(),
            "removed 'run-tasks' command should not parse"
        );
    }

    #[test]
    fn parse_removed_init_command_is_error() {
        let result = Cli::try_parse_from(["agent-loop", "init"]);
        assert!(result.is_err(), "removed 'init' command should not parse");
    }

    #[test]
    fn parse_removed_resume_command_is_error() {
        let result = Cli::try_parse_from(["agent-loop", "resume"]);
        assert!(result.is_err(), "removed 'resume' command should not parse");
    }

    #[test]
    fn dispatch_config_init() {
        let cli = Cli::try_parse_from(["agent-loop", "config", "init"])
            .expect("config init should parse");
        let dispatch = dispatch_from_cli(cli).expect("dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::ConfigInit(ConfigInitArgs { force: false })
        );
    }

    #[test]
    fn dispatch_config_init_force() {
        let cli = Cli::try_parse_from(["agent-loop", "config", "init", "--force"])
            .expect("config init --force should parse");
        let dispatch = dispatch_from_cli(cli).expect("dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::ConfigInit(ConfigInitArgs { force: true })
        );
    }

    #[test]
    fn dispatch_new_commands_map_to_new_variants() {
        let plan = dispatch_from_cli(Cli::try_parse_from(["agent-loop", "plan", "task"]).unwrap())
            .expect("plan dispatch should succeed");
        assert!(matches!(plan, Dispatch::Plan(_)));

        let tasks = dispatch_from_cli(Cli::try_parse_from(["agent-loop", "tasks"]).unwrap())
            .expect("tasks dispatch should succeed");
        assert!(matches!(tasks, Dispatch::Tasks(_)));

        let implement =
            dispatch_from_cli(Cli::try_parse_from(["agent-loop", "implement"]).unwrap())
                .expect("implement dispatch should succeed");
        assert!(matches!(implement, Dispatch::Implement(_)));

        let plan_tasks = dispatch_from_cli(
            Cli::try_parse_from(["agent-loop", "plan-tasks-implement", "task"]).unwrap(),
        )
        .expect("plan-tasks-implement dispatch should succeed");
        assert!(matches!(plan_tasks, Dispatch::PlanTasksImplement(_)));

        let plan_implement = dispatch_from_cli(
            Cli::try_parse_from(["agent-loop", "plan-implement", "task"]).unwrap(),
        )
        .expect("plan-implement dispatch should succeed");
        assert!(matches!(plan_implement, Dispatch::PlanImplement(_)));

        let tasks_implement =
            dispatch_from_cli(Cli::try_parse_from(["agent-loop", "tasks-implement"]).unwrap())
                .expect("tasks-implement dispatch should succeed");
        assert!(matches!(tasks_implement, Dispatch::TasksImplement(_)));

        let reset = dispatch_from_cli(Cli::try_parse_from(["agent-loop", "reset"]).unwrap())
            .expect("reset dispatch should succeed");
        assert!(matches!(reset, Dispatch::Reset(_)));
    }

    #[test]
    fn wave_resume_is_allowed() {
        // --wave --resume should parse and validate successfully (not error).
        let cli = Cli::try_parse_from(["agent-loop", "implement", "--wave", "--resume"])
            .expect("--wave --resume should parse");
        if let Some(Commands::Implement(args)) = cli.command {
            let result = args.validate();
            assert!(
                result.is_ok(),
                "--wave --resume should not be rejected by validate"
            );
        } else {
            panic!("expected Implement command");
        }
    }

    #[test]
    fn wave_task_and_file_still_rejected() {
        let cli = Cli::try_parse_from([
            "agent-loop",
            "implement",
            "--wave",
            "--task",
            "do something",
        ])
        .expect("--wave --task should parse");
        if let Some(Commands::Implement(args)) = cli.command {
            let err = args.validate();
            assert!(err.is_err(), "--wave --task should be rejected by validate");
        } else {
            panic!("expected Implement command");
        }
    }

    #[test]
    fn plan_implement_accepts_wave_with_task_input() {
        let cli = Cli::try_parse_from([
            "agent-loop",
            "plan-implement",
            "ship it",
            "--wave",
            "--max-parallel",
            "4",
        ])
        .expect("plan-implement --wave should parse");
        if let Some(Commands::PlanImplement(args)) = cli.command {
            assert!(
                args.validate().is_ok(),
                "compound command should accept task + --wave"
            );
            assert!(args.flags.wave);
            assert_eq!(args.flags.max_parallel, Some(4));
        } else {
            panic!("expected PlanImplement command");
        }
    }

    #[test]
    fn implement_still_rejects_explicit_per_task_resume() {
        let cli = Cli::try_parse_from(["agent-loop", "implement", "--per-task", "--resume"])
            .expect("implement --per-task --resume should parse");
        if let Some(Commands::Implement(args)) = cli.command {
            let err = args.validate();
            assert!(
                err.is_err(),
                "implement --per-task --resume should be rejected"
            );
        } else {
            panic!("expected Implement command");
        }
    }

    #[test]
    fn resolve_fresh_implement_mode_prefers_cli_over_config() {
        let root = create_temp_project_root("main_mode_resolution");
        let config = make_test_config(
            &root,
            TestConfigOptions {
                batch_implement: false,
                ..Default::default()
            },
        );

        assert_eq!(
            resolve_fresh_implement_mode(
                &ImplementModeFlags {
                    per_task: false,
                    wave: true,
                    max_retries: 2,
                    round_step: 2,
                    continue_on_fail: false,
                    fail_fast: false,
                    max_parallel: None,
                },
                &config,
            ),
            ImplementExecutionMode::Wave
        );
        assert_eq!(
            resolve_fresh_implement_mode(
                &ImplementModeFlags {
                    per_task: false,
                    wave: false,
                    max_retries: 2,
                    round_step: 2,
                    continue_on_fail: false,
                    fail_fast: false,
                    max_parallel: None,
                },
                &config,
            ),
            ImplementExecutionMode::PerTask
        );
    }

    #[test]
    fn resolve_resume_implement_mode_uses_persisted_mode_and_detects_conflicts() {
        let root = create_temp_project_root("main_resume_mode");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state dir should exist");
        write_implement_mode(ImplementExecutionMode::Wave, &config).expect("write mode");

        let flags = ImplementModeFlags {
            per_task: false,
            wave: false,
            max_retries: 2,
            round_step: 2,
            continue_on_fail: false,
            fail_fast: false,
            max_parallel: None,
        };
        let resolved = resolve_resume_implement_mode(
            &flags,
            &config,
            state::WorkflowKind::Implement,
            ResumeOrigin::User,
        )
        .expect("persisted wave mode should resolve");
        assert_eq!(resolved, ImplementExecutionMode::Wave);

        let conflict = resolve_resume_implement_mode(
            &ImplementModeFlags {
                per_task: true,
                ..flags.clone()
            },
            &config,
            state::WorkflowKind::Plan,
            ResumeOrigin::User,
        );
        assert!(
            matches!(conflict, Err(AgentLoopError::Config(ref msg)) if msg.contains("Resume mode mismatch")),
            "expected persisted/explicit mismatch error, got: {conflict:?}"
        );
    }

    #[test]
    fn resolve_resume_implement_settings_prefers_persisted_flags() {
        let root = create_temp_project_root("main_resume_flags");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state dir should exist");
        persist_implement_settings(
            ImplementExecutionMode::Wave,
            &ImplementModeFlags {
                wave: true,
                max_parallel: Some(4),
                fail_fast: true,
                ..ImplementModeFlags::default()
            },
            &config,
        )
        .expect("persist settings");

        let (mode, flags) = resolve_resume_implement_settings(
            &ImplementModeFlags::default(),
            &config,
            state::WorkflowKind::Implement,
            ResumeOrigin::User,
        )
        .expect("resume settings should resolve");

        assert_eq!(mode, ImplementExecutionMode::Wave);
        assert!(flags.wave, "persisted wave mode should be restored");
        assert_eq!(flags.max_parallel, Some(4));
        assert!(flags.fail_fast, "persisted fail_fast should be restored");
    }

    #[test]
    fn resume_hint_for_workflow_is_phase_specific() {
        assert!(
            resume_hint_for_workflow(Some(state::WorkflowKind::Plan)).contains("plan-implement")
        );
        assert!(
            resume_hint_for_workflow(Some(state::WorkflowKind::Decompose))
                .contains("tasks-implement")
        );
        assert!(
            resume_hint_for_workflow(Some(state::WorkflowKind::Implement))
                .contains("plan-tasks-implement")
        );
    }

    #[test]
    fn terminalize_interrupted_tasks_marks_pending_and_running_as_skipped() {
        let mut statuses = vec![
            TaskStatusEntry {
                title: "Task 1".into(),
                status: state::TaskRunStatus::Done,
                retries: 0,
                last_error: None,
                skip_reason: None,
                wave_index: None,
            },
            TaskStatusEntry {
                title: "Task 2".into(),
                status: state::TaskRunStatus::Pending,
                retries: 0,
                last_error: None,
                skip_reason: None,
                wave_index: None,
            },
            TaskStatusEntry {
                title: "Task 3".into(),
                status: state::TaskRunStatus::Running,
                retries: 0,
                last_error: None,
                skip_reason: None,
                wave_index: None,
            },
            TaskStatusEntry {
                title: "Task 4".into(),
                status: state::TaskRunStatus::Failed,
                retries: 1,
                last_error: Some("error".into()),
                skip_reason: None,
                wave_index: None,
            },
        ];

        terminalize_interrupted_tasks(&mut statuses);

        // Done and Failed should be unchanged.
        assert_eq!(statuses[0].status, state::TaskRunStatus::Done);
        assert!(statuses[0].skip_reason.is_none());

        assert_eq!(statuses[3].status, state::TaskRunStatus::Failed);
        assert!(statuses[3].skip_reason.is_none());

        // Pending and Running should become Skipped with reason.
        assert_eq!(statuses[1].status, state::TaskRunStatus::Skipped);
        assert_eq!(statuses[1].skip_reason.as_deref(), Some("interrupted"));

        assert_eq!(statuses[2].status, state::TaskRunStatus::Skipped);
        assert_eq!(statuses[2].skip_reason.as_deref(), Some("interrupted"));
    }

    #[test]
    fn finish_command_exit_cleans_session_files_on_success() {
        let root = create_temp_project_root("main_finish_cleanup_success");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(config.state_dir.join(".wave-task-1")).expect("state dir should exist");
        fs::write(
            config.state_dir.join("implement-implementer_session_id"),
            "root",
        )
        .expect("root session file should be written");
        fs::write(
            config
                .state_dir
                .join(".wave-task-1")
                .join("implement-reviewer_session_id"),
            "nested",
        )
        .expect("nested session file should be written");

        let exit_code =
            finish_command_exit(Instant::now(), &config, 0).expect("success should pass through");

        assert_eq!(exit_code, 0);
        assert!(
            !config
                .state_dir
                .join("implement-implementer_session_id")
                .exists()
        );
        assert!(
            !config
                .state_dir
                .join(".wave-task-1")
                .join("implement-reviewer_session_id")
                .exists()
        );
    }

    #[test]
    fn cleanup_session_files_on_success_skips_nonzero_exit_codes() {
        let root = create_temp_project_root("main_cleanup_nonzero_exit");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state dir should exist");
        let session_path = config.state_dir.join("implement-implementer_session_id");
        fs::write(&session_path, "keep").expect("session file should be written");

        cleanup_session_files_on_success(&config, 1);

        assert!(
            session_path.exists(),
            "nonzero exit code must preserve session files for resume"
        );
    }

    #[test]
    fn implement_all_tasks_batch_propagates_cleanup_flag() {
        let root = create_temp_project_root("main_batch_cleanup_flag");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state dir should exist");
        let args = ImplementArgs {
            task: None,
            file: None,
            resume: false,
            single_agent: false,
            flags: ImplementModeFlags::default(),
        };
        let parsed_tasks = vec![ParsedTask {
            title: "Task 1".to_string(),
            content: "Ship it".to_string(),
            dependencies: Vec::new(),
        }];
        let observed_cleanup = std::sync::Mutex::new(None);

        let exit_code = implement_all_tasks_batch_using(
            &args,
            &parsed_tasks,
            &config,
            &config.project_dir,
            "### Task 1: Ship it\nDo the work",
            false,
            |_args, _config, _project_dir, title, combined_task, cleanup_on_success| {
                assert_eq!(title, "Batch implementation (1 tasks)");
                assert!(combined_task.contains("Do the work"));
                *observed_cleanup.lock().expect("mutex should lock") = Some(cleanup_on_success);
                Ok(0)
            },
        )
        .expect("batch helper should succeed");

        assert_eq!(exit_code, 0);
        assert_eq!(
            *observed_cleanup.lock().expect("mutex should lock"),
            Some(false),
            "compound batch flows must defer cleanup to finish_command_exit"
        );
    }

    #[test]
    fn wave_mode_command_path_cleans_nested_session_files_on_success() {
        let root = create_temp_project_root("main_wave_cleanup");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state dir should exist");

        // Write a minimal tasks.md so the command path can parse it.
        fs::write(
            config.state_dir.join("tasks.md"),
            "### Task 1: Build widget\nImplement the widget.\n",
        )
        .expect("tasks.md should be written");

        // Seed session files in root and nested .wave-task-1 directory.
        let nested_dir = config.state_dir.join(".wave-task-1");
        fs::create_dir_all(&nested_dir).expect("nested state dir should exist");
        fs::write(
            config.state_dir.join("implement-implementer_session_id"),
            "root",
        )
        .expect("root session file should be written");
        fs::write(nested_dir.join("implement-reviewer_session_id"), "nested")
            .expect("nested session file should be written");

        // Preserve marker: workflow.txt must survive cleanup.
        fs::write(config.state_dir.join("workflow.txt"), "implement\n")
            .expect("workflow marker should be written");

        let args = ImplementArgs {
            task: None,
            file: None,
            resume: false,
            single_agent: false,
            flags: ImplementModeFlags::default(),
        };

        let exit_code = implement_all_tasks_command_with_mode_using(
            &args,
            &config,
            &config.project_dir,
            ImplementExecutionMode::Wave,
            true, // cleanup_on_success
            // Mock wave executor: return success without running real wave logic.
            |_args, _tasks, _config, _project_dir| Ok(0),
            // Mock per-task executor: unused for Wave mode.
            |_args, _tasks, _config, _project_dir| unreachable!("wave mode should not call per-task"),
            None,
        )
        .expect("wave mode should succeed");

        assert_eq!(exit_code, 0);
        assert!(
            !config
                .state_dir
                .join("implement-implementer_session_id")
                .exists(),
            "root session file should be cleaned up after successful wave run"
        );
        assert!(
            !nested_dir.join("implement-reviewer_session_id").exists(),
            "nested .wave-task-1 session file should be cleaned up after successful wave run"
        );
        assert!(
            config.state_dir.join("workflow.txt").exists(),
            "workflow.txt must be preserved"
        );
    }

    /// Command-path test: exercises `run_command_with_review_max_rounds_using`
    /// (the function body of `implement --task`/`--file`) with a mock loop that
    /// returns consensus. Proves the cleanup call at the end of the function
    /// actually removes root and nested session files on success.
    #[test]
    fn run_command_using_cleans_session_files_on_success() {
        let root = create_temp_project_root("main_run_cmd_cleanup");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state dir should exist");

        let exit_code = run_command_with_review_max_rounds_using(
            "test task".to_string(),
            config.clone(),
            ImplementationRunOptions {
                review_max_rounds_override: None,
                clear_progress: false,
                write_mode: None,
                persist_flags: None,
                cleanup_on_success: true,
            },
            // Mock implementation loop: return true (consensus reached).
            |_config, _baseline| true,
        )
        .expect("run_command_using should succeed");

        assert_eq!(exit_code, 0);

        // Seed session files AFTER state::init (which creates the state dir
        // and may clean it), then re-run to prove cleanup wiring.
        // We need a fresh config because init may have written state files.
        let nested_dir = config.state_dir.join(".wave-task-1");
        fs::create_dir_all(&nested_dir).expect("nested dir should exist");
        fs::write(
            config.state_dir.join("implement-implementer_session_id"),
            "root",
        )
        .expect("root session file should be written");
        fs::write(nested_dir.join("implement-reviewer_session_id"), "nested")
            .expect("nested session file should be written");
        fs::write(config.state_dir.join("workflow.txt"), "implement\n")
            .expect("workflow marker should be written");

        let exit_code = run_command_with_review_max_rounds_using(
            "test task".to_string(),
            config.clone(),
            ImplementationRunOptions {
                review_max_rounds_override: None,
                clear_progress: false,
                write_mode: None,
                persist_flags: None,
                cleanup_on_success: true,
            },
            |_config, _baseline| true,
        )
        .expect("second run should succeed");

        assert_eq!(exit_code, 0);
        assert!(
            !config
                .state_dir
                .join("implement-implementer_session_id")
                .exists(),
            "root session file should be removed after successful implement --task/--file path"
        );
        assert!(
            !nested_dir.join("implement-reviewer_session_id").exists(),
            "nested session file should be removed after successful implement --task/--file path"
        );
        assert!(
            config.state_dir.join("workflow.txt").exists(),
            "workflow.txt must be preserved"
        );
    }

    /// Command-path test: exercises
    /// `implementation_resume_with_review_max_rounds_internal_using`
    /// (the function body of `implement --resume`) with a mock loop that
    /// returns consensus. Proves the cleanup call at the end of the function
    /// actually removes session files on success.
    #[test]
    fn implementation_resume_using_cleans_session_files_on_success() {
        let root = create_temp_project_root("main_resume_cleanup");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state dir should exist");

        // Seed session files before calling the resume path.
        let nested_dir = config.state_dir.join(".wave-task-2");
        fs::create_dir_all(&nested_dir).expect("nested dir should exist");
        fs::write(
            config.state_dir.join("implement-implementer_session_id"),
            "root",
        )
        .expect("root session file should be written");
        fs::write(nested_dir.join("plan-reviewer_session_id"), "nested")
            .expect("nested session file should be written");
        fs::write(config.state_dir.join("changes.md"), "keep me")
            .expect("changes file should be written");

        let exit_code = implementation_resume_with_review_max_rounds_internal_using(
            Instant::now(),
            "resume task".to_string(),
            config.clone(),
            false, // print_elapsed
            true,  // cleanup_on_success
            // Mock resume loop: return true (consensus reached).
            |_config, _baseline| true,
        )
        .expect("resume_using should succeed");

        assert_eq!(exit_code, 0);
        assert!(
            !config
                .state_dir
                .join("implement-implementer_session_id")
                .exists(),
            "root session file should be removed after successful implement --resume path"
        );
        assert!(
            !nested_dir.join("plan-reviewer_session_id").exists(),
            "nested session file should be removed after successful implement --resume path"
        );
        assert!(
            config.state_dir.join("changes.md").exists(),
            "non-session state files must be preserved"
        );
    }

    fn test_loop_status(status: state::Status) -> state::LoopStatus {
        state::LoopStatus {
            status,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "claude".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: String::new(),
            reason: None,
            timestamp: "2026-02-21T00:00:00.000Z".to_string(),
        }
    }

    #[test]
    fn stuck_status_is_retryable() {
        let mut stuck = test_loop_status(state::Status::Stuck);
        stuck.round = 3;
        stuck.reason = Some("no diff for 3 rounds".to_string());
        assert!(
            is_retryable_run_tasks_status(Some(&stuck)),
            "Status::Stuck should be retryable"
        );
    }

    #[test]
    fn max_rounds_status_is_retryable() {
        let mut max_rounds = test_loop_status(state::Status::MaxRounds);
        max_rounds.round = 5;
        assert!(
            is_retryable_run_tasks_status(Some(&max_rounds)),
            "Status::MaxRounds should be retryable"
        );
    }

    #[test]
    fn approved_status_is_not_retryable() {
        let approved = test_loop_status(state::Status::Approved);
        assert!(
            !is_retryable_run_tasks_status(Some(&approved)),
            "Status::Approved should not be retryable"
        );
    }

    #[test]
    fn wave_dep_parsing_skips_heading_line() {
        // Simulate what parse_tasks_markdown now passes: body only (no heading).
        let body = "depends: 1, 3\nSome description of the task.";
        let deps = wave::parse_dependencies(body);
        assert_eq!(deps, vec![0, 2]);

        // With heading included (old behavior), the heading consumes a slot.
        // But parse_dependencies should still find depends: in first 3 non-blank lines.
        let with_heading = "### Task 2: Do something\ndepends: 1, 3\nSome description.";
        let deps2 = wave::parse_dependencies(with_heading);
        assert_eq!(deps2, vec![0, 2]);

        // When depends: is on line 4 (after heading), it would be missed with old heading-included content.
        // Now we pass body only, so depends: on line 3 of the body is still found.
        let body_line3 = "description line 1\nsome more info\ndepends: 2\n";
        let deps3 = wave::parse_dependencies(body_line3);
        assert_eq!(deps3, vec![1]);
    }

    #[test]
    fn config_init_writes_template_to_new_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = ConfigInitArgs { force: false };
        let result = config_init_to_dir(&args, dir.path());
        assert_eq!(result.unwrap(), 0);

        let config_path = dir.path().join(".agent-loop.toml");
        assert!(config_path.exists());
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("# ── Agents"),
            "template should contain Agents section"
        );
    }

    #[test]
    fn config_init_existing_file_without_force_returns_exit_1() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join(".agent-loop.toml");
        fs::write(&config_path, "existing").unwrap();

        let args = ConfigInitArgs { force: false };
        let result = config_init_to_dir(&args, dir.path());
        assert_eq!(result.unwrap(), 1);

        // File should be unchanged.
        let content = fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, "existing");
    }

    #[test]
    fn config_init_force_overwrites_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join(".agent-loop.toml");
        fs::write(&config_path, "old content").unwrap();

        let args = ConfigInitArgs { force: true };
        let result = config_init_to_dir(&args, dir.path());
        assert_eq!(result.unwrap(), 0);

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("# ── Agents"),
            "template should overwrite old content"
        );
        assert!(!content.contains("old content"));
    }

    /// Verify environment_help() documents all env vars recognized in from_cli().
    /// Enforces the CONSTRAINT: every env var in from_cli() must appear in
    /// environment_help() and clear_env(). This test prevents future omissions.
    #[test]
    fn environment_help_contains_all_recognized_env_vars() {
        let help = environment_help(None);
        // Every env var parsed in Config::from_cli_with_overrides must appear.
        for var in [
            "REVIEW_MAX_ROUNDS",
            "PLANNING_MAX_ROUNDS",
            "DECOMPOSITION_MAX_ROUNDS",
            "TIMEOUT",
            "IMPLEMENTER",
            "REVIEWER",
            "PLANNER",
            "SINGLE_AGENT",
            "AUTO_COMMIT",
            "AUTO_TEST",
            "AUTO_TEST_CMD",
            "COMPOUND",
            "DECISIONS_ENABLED",
            "DECISIONS_AUTO_REFERENCE",
            "DECISIONS_MAX_LINES",
            "DIFF_MAX_LINES",
            "CONTEXT_LINE_CAP",
            "PLANNING_CONTEXT_EXCERPT_LINES",
            "BATCH_IMPLEMENT",
            "MAX_PARALLEL",
            "VERBOSE",
            "PROGRESSIVE_CONTEXT",
            "PLANNING_ADVERSARIAL_REVIEW",
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
            // Stuck detection
            "STUCK_DETECTION_ENABLED",
            "STUCK_NO_DIFF_ROUNDS",
            "STUCK_THRESHOLD_MINUTES",
            "STUCK_ACTION",
            // Wave runtime
            "WAVE_LOCK_STALE_SECONDS",
            "WAVE_SHUTDOWN_GRACE_MS",
        ] {
            assert!(
                help.contains(var),
                "environment_help() is missing documentation for {var}"
            );
        }
    }

    #[test]
    fn config_init_write_failure_returns_config_error() {
        // Point to a non-existent directory so fs::write fails.
        let bad_dir = std::path::PathBuf::from("/nonexistent/path/that/does/not/exist");
        let args = ConfigInitArgs { force: false };
        let result = config_init_to_dir(&args, &bad_dir);
        assert!(
            matches!(result, Err(AgentLoopError::Config(ref msg)) if msg.contains("failed to write")),
            "expected Config error for write failure, got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // environment_help: migration note and default values
    // -----------------------------------------------------------------------

    #[test]
    fn environment_help_contains_migration_note() {
        let help = environment_help(None);
        assert!(
            help.contains(
                "max_rounds / MAX_ROUNDS has been renamed to review_max_rounds / REVIEW_MAX_ROUNDS"
            ),
            "environment_help() should contain migration note for max_rounds rename (both TOML and env forms)"
        );
    }

    #[test]
    fn environment_help_shows_unlimited_round_defaults() {
        let help = environment_help(None);
        // All three round-limit defaults should show 0 (unlimited)
        assert!(
            help.contains(&format!(
                "REVIEW_MAX_ROUNDS     (default: {})",
                DEFAULT_REVIEW_MAX_ROUNDS
            )),
            "REVIEW_MAX_ROUNDS default should be 0"
        );
        assert!(
            help.contains(&format!(
                "PLANNING_MAX_ROUNDS   (default: {})",
                DEFAULT_PLANNING_MAX_ROUNDS
            )),
            "PLANNING_MAX_ROUNDS default should be 0"
        );
        assert!(
            help.contains(&format!(
                "DECOMPOSITION_MAX_ROUNDS (default: {})",
                DEFAULT_DECOMPOSITION_MAX_ROUNDS
            )),
            "DECOMPOSITION_MAX_ROUNDS default should be 0"
        );
    }

    #[test]
    fn environment_help_shows_full_access_default_on() {
        let help = environment_help(None);
        assert!(
            help.contains("CLAUDE_FULL_ACCESS    (default: 1)"),
            "CLAUDE_FULL_ACCESS default should show 1"
        );
        assert!(
            help.contains("CODEX_FULL_ACCESS     (default: 1)"),
            "CODEX_FULL_ACCESS default should show 1"
        );
    }

    #[test]
    fn environment_help_does_not_list_max_rounds_as_active_setting() {
        let help = environment_help(None);
        // MAX_ROUNDS should only appear in the migration note, not as an active env var setting.
        // The active setting is REVIEW_MAX_ROUNDS.
        let lines: Vec<&str> = help.lines().collect();
        for line in &lines {
            let trimmed = line.trim();
            // Skip the migration note line
            if trimmed.contains("Migration note") {
                continue;
            }
            // No line should list MAX_ROUNDS as a primary env var with a default
            if trimmed.starts_with("MAX_ROUNDS") && trimmed.contains("(default:") {
                panic!("MAX_ROUNDS should not appear as an active env var setting: {trimmed}");
            }
        }
    }

    #[test]
    fn environment_help_contains_round_limit_semantics_note() {
        let help = environment_help(None);
        assert!(
            help.contains("Round limits: 0 = unlimited"),
            "environment_help() should document that 0 = unlimited for round limits"
        );
    }

    #[test]
    fn reset_state_dir_rejects_invalid_session_name() {
        let root = create_temp_project_root("reset_invalid_session");
        let result = reset_state_dir(&root, Some("../../escape"));
        assert!(
            result.is_err(),
            "reset_state_dir should reject session names with path separators"
        );
        let result = reset_state_dir(&root, Some("has.dot"));
        assert!(
            result.is_err(),
            "reset_state_dir should reject session names with dots"
        );
    }

    #[test]
    fn reset_state_dir_with_valid_session_only_clears_session_dir() {
        let root = create_temp_project_root("reset_valid_session");
        let state_dir = root.join(".agent-loop").join("state");
        let session_dir = state_dir.join("my-session");
        let other_dir = state_dir.join("other-session");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&other_dir).unwrap();
        fs::write(session_dir.join("status.json"), "{}").unwrap();
        fs::write(other_dir.join("status.json"), "{}").unwrap();
        fs::write(state_dir.join("plan.md"), "default").unwrap();

        reset_state_dir(&root, Some("my-session")).unwrap();

        // Session dir is recreated empty
        assert!(session_dir.is_dir());
        assert!(!session_dir.join("status.json").exists());
        // Other session and default state are untouched
        assert!(other_dir.join("status.json").exists());
        assert!(state_dir.join("plan.md").exists());
    }

    #[test]
    fn migrate_does_not_rename_session_named_task_n_after_sentinel() {
        let root = create_temp_project_root("migrate_session_task_n");
        let agent_loop_dir = root.join(".agent-loop");
        let state_dir = agent_loop_dir.join("state");
        let session_dir = state_dir.join("task-1");
        fs::create_dir_all(&session_dir).unwrap();
        // Simulate a session that has written markers
        fs::write(session_dir.join("implement-progress.md"), "progress").unwrap();
        fs::write(session_dir.join("implement_session_id"), "sid").unwrap();

        // Simulate the real upgrade path: run migration once on an empty state
        // dir to write the sentinel (no legacy dirs to rename).
        let empty_state = agent_loop_dir.join("empty-state");
        fs::create_dir_all(&empty_state).unwrap();
        state::migrate_legacy_wave_task_dirs(&empty_state, &agent_loop_dir).unwrap();
        assert!(
            state::wave_task_migration_sentinel(&agent_loop_dir).exists(),
            "sentinel should be written"
        );

        // Now simulate a session named task-1 being created after sentinel.
        // Run migration again — it should skip because sentinel exists.
        state::migrate_legacy_wave_task_dirs(&state_dir, &agent_loop_dir).unwrap();
        assert!(
            session_dir.is_dir(),
            "task-1 session directory must NOT be renamed after sentinel exists"
        );
        assert!(
            !state_dir.join(".wave-task-1").exists(),
            ".wave-task-1 should not be created from a session dir"
        );
    }

    #[test]
    fn migrate_is_idempotent_and_crash_safe() {
        // Simulate a partial migration (crash after renaming only one dir).
        let root = create_temp_project_root("migrate_crash_safe");
        let agent_loop_dir = root.join(".agent-loop");
        let state_dir = agent_loop_dir.join("state");
        fs::create_dir_all(&state_dir).unwrap();

        // Create two legacy wave task dirs
        let task1 = state_dir.join("task-1");
        let task2 = state_dir.join("task-2");
        fs::create_dir_all(&task1).unwrap();
        fs::create_dir_all(&task2).unwrap();
        fs::write(task1.join("implement-progress.md"), "p1").unwrap();
        fs::write(task2.join("implement-progress.md"), "p2").unwrap();

        // Manually rename task-1 but do NOT write sentinel (simulates crash).
        fs::rename(&task1, state_dir.join(".wave-task-1")).unwrap();
        assert!(!state::wave_task_migration_sentinel(&agent_loop_dir).exists());

        // Re-run migration — should pick up task-2 and write sentinel.
        state::migrate_legacy_wave_task_dirs(&state_dir, &agent_loop_dir).unwrap();
        assert!(
            state_dir.join(".wave-task-1").is_dir(),
            "previously renamed dir should still exist"
        );
        assert!(
            state_dir.join(".wave-task-2").is_dir(),
            "task-2 should be renamed on retry"
        );
        assert!(!task2.exists(), "task-2 should no longer exist");
        assert!(
            state::wave_task_migration_sentinel(&agent_loop_dir).exists(),
            "sentinel should be written after successful retry"
        );

        // Third call should be a no-op (sentinel exists).
        state::migrate_legacy_wave_task_dirs(&state_dir, &agent_loop_dir).unwrap();
    }
}
