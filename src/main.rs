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
    Config, DEFAULT_DECOMPOSITION_MAX_ROUNDS, DEFAULT_MAX_ROUNDS, DEFAULT_PLANNING_MAX_ROUNDS,
    DEFAULT_TIMEOUT_SECONDS,
};
use error::AgentLoopError;
use state::{
    LoopStatus, Status, StatusPatch, TaskMetricsEntry, TaskMetricsFile, TaskRunStatus,
    TaskStatusEntry, TaskStatusFile,
};

const KNOWN_SUBCOMMANDS: [&str; 11] = [
    "plan",
    "tasks",
    "implement",
    "reset",
    "status",
    "version",
    "help",
    "run",
    "run-tasks",
    "init",
    "resume",
];

#[derive(Debug, Parser)]
#[command(
    name = "agent-loop",
    version,
    about = "Run a collaborative implementation/review loop between coding agents."
)]
struct Cli {
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
    /// Clear .agent-loop/state while preserving decisions.md
    Reset(ResetArgs),
    /// Show current loop status
    Status,
    /// Print version
    Version,
    #[command(name = "run", hide = true)]
    Run(RunArgs),
    #[command(name = "run-tasks", hide = true)]
    RunTasksDeprecated(LegacyRunTasksArgs),
    #[command(name = "init", hide = true)]
    Init,
    #[command(name = "resume", hide = true)]
    Resume(ResumeArgs),
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
    single_agent: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct ImplementArgs {
    #[arg(long, value_name = "TASK")]
    task: Option<String>,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long)]
    per_task: bool,
    #[arg(long)]
    wave: bool,
    #[arg(long)]
    resume: bool,
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
    #[arg(long, default_value_t = 2)]
    round_step: u32,
    #[arg(long)]
    single_agent: bool,
    #[arg(long)]
    continue_on_fail: bool,
    #[arg(long)]
    fail_fast: bool,
    #[arg(long)]
    max_parallel: Option<u32>,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct ResumeArgs {
    #[arg(long)]
    single_agent: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct LegacyRunTasksArgs {
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    tasks_file: Option<PathBuf>,
    #[arg(long)]
    per_task: bool,
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
    #[arg(long, default_value_t = 2)]
    round_step: u32,
    #[arg(long)]
    single_agent: bool,
    #[arg(long)]
    continue_on_fail: bool,
    #[arg(long)]
    fail_fast: bool,
    #[arg(long)]
    max_parallel: Option<u32>,
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
    Reset(ResetArgs),
    Status,
    Version,
    MigrationError(String),
}

impl TasksArgs {
    fn validate(&self) -> Result<(), AgentLoopError> {
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

        if self.per_task && (self.task.is_some() || self.file.is_some() || self.resume) {
            return Err(AgentLoopError::Config(
                "--per-task cannot be combined with --task, --file, or --resume.".to_string(),
            ));
        }

        if self.wave && self.per_task {
            return Err(AgentLoopError::Config(
                "--wave and --per-task cannot be used together.".to_string(),
            ));
        }

        if self.wave && (self.task.is_some() || self.file.is_some() || self.resume) {
            return Err(AgentLoopError::Config(
                "--wave cannot be combined with --task, --file, or --resume.".to_string(),
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
    interrupt::register_signal_handlers();
    let parse_outcome = parse_cli_from(std::env::args_os())?;
    match parse_outcome {
        ParseOutcome::Exit(code) => Ok(code),
        ParseOutcome::Parsed(cli) => execute_dispatch(dispatch_from_cli(cli)?),
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
                println!("{}", environment_help());
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
                return Ok(Dispatch::MigrationError(
                    "'--tasks-file' has been removed. Use '--file' instead.".to_string(),
                ));
            }
            Ok(Dispatch::Tasks(args))
        }
        Some(Commands::Implement(args)) => Ok(Dispatch::Implement(args)),
        Some(Commands::Reset(args)) => Ok(Dispatch::Reset(args)),
        Some(Commands::Status) => Ok(Dispatch::Status),
        Some(Commands::Version) => Ok(Dispatch::Version),
        Some(Commands::Run(args)) => {
            if args.planning_only {
                Ok(Dispatch::MigrationError(
                    "'run --planning-only' has been removed. Use 'plan'.".to_string(),
                ))
            } else if args.resume {
                Ok(Dispatch::MigrationError(
                    "'run --resume' has been removed. Use 'implement --resume' or 'tasks --resume'."
                        .to_string(),
                ))
            } else {
                Ok(Dispatch::MigrationError(
                    "'run' has been removed. Use 'implement'.".to_string(),
                ))
            }
        }
        Some(Commands::RunTasksDeprecated(_)) => Ok(Dispatch::MigrationError(
            "'run-tasks' has been removed. Use 'implement'.".to_string(),
        )),
        Some(Commands::Init) => Ok(Dispatch::MigrationError(
            "'init' has been removed. Use 'plan' or 'implement' — state is created automatically."
                .to_string(),
        )),
        Some(Commands::Resume(_)) => Ok(Dispatch::MigrationError(
            "'resume' has been removed. Use 'implement --resume' or 'tasks --resume'.".to_string(),
        )),
        None => Ok(Dispatch::ShowHelp),
    }
}

fn execute_dispatch(dispatch: Dispatch) -> Result<i32, AgentLoopError> {
    match dispatch {
        Dispatch::Plan(args) => plan_command(args),
        Dispatch::Tasks(args) => tasks_command(args),
        Dispatch::Implement(args) => implement_command(args),
        Dispatch::Reset(args) => reset_command(&args),
        Dispatch::Status => status_command(),
        Dispatch::Version => version_command(),
        Dispatch::MigrationError(message) => {
            eprintln!("Error: {message}");
            Ok(1)
        }
        Dispatch::ShowHelp => {
            print_help_message()?;
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

fn print_help_message() -> Result<(), AgentLoopError> {
    let mut command = Cli::command();
    command.print_long_help()?;
    println!();
    println!();
    println!("{}", environment_help());
    Ok(())
}

fn environment_help() -> String {
    format!(
        "Primary commands:\n  agent-loop plan <task>           Planning only\n  agent-loop plan --file <path>    Planning only from file\n  agent-loop tasks                 Decompose only\n  agent-loop tasks --resume        Resume decomposition\n  agent-loop implement             Implement all tasks from .agent-loop/state/tasks.md in one loop\n  agent-loop implement --per-task  Implement tasks one-by-one (legacy mode)\n  agent-loop implement --task <t>  Implement one inline task\n  agent-loop implement --file <p>  Implement one task from file\n  agent-loop implement --resume    Resume implementation\n  agent-loop reset                 Clear .agent-loop/state/ and preserve decisions.md\n\nConfiguration sources (highest precedence first):\n  1. CLI flags and subcommands\n  2. Environment variables\n  3. .agent-loop.toml (per-project config file)\n  4. Built-in defaults\n\nEnvironment variables:\n  MAX_ROUNDS            (default: {DEFAULT_MAX_ROUNDS})   Max implementation/review rounds\n  PLANNING_MAX_ROUNDS   (default: {DEFAULT_PLANNING_MAX_ROUNDS})  Max planning consensus rounds\n  DECOMPOSITION_MAX_ROUNDS (default: {DEFAULT_DECOMPOSITION_MAX_ROUNDS})  Max decomposition rounds\n  TIMEOUT               (default: {DEFAULT_TIMEOUT_SECONDS})  Idle timeout in seconds\n  IMPLEMENTER           (default: claude) Implementer agent: claude|codex\n  REVIEWER                              Reviewer agent: claude|codex (default: opposite of implementer)\n  SINGLE_AGENT          (default: 0)    Enable single-agent mode when truthy\n  AUTO_COMMIT           (default: 1)    Auto-commit loop-owned changes (0 disables)\n  AUTO_TEST             (default: 0)    Run quality checks before review when truthy\n  AUTO_TEST_CMD                         Override auto-detected quality check command\n  COMPOUND              (default: 1)    Enable post-consensus compound learning phase\n  DECISIONS_MAX_LINES   (default: 50)   Number of decision lines injected into prompts\n  BATCH_IMPLEMENT       (default: 1)    Implement all tasks.md tasks in one loop by default\n\n  Model selection:\n  IMPLEMENTER_MODEL                     Model override for implementer (e.g. claude-sonnet-4-6)\n  REVIEWER_MODEL                        Model override for reviewer (e.g. o3)\n  PLANNER_MODEL                         Model override for planning phase\n\n  Claude CLI tuning:\n  CLAUDE_FULL_ACCESS    (default: 0)    Use --dangerously-skip-permissions instead of --allowedTools\n  CLAUDE_ALLOWED_TOOLS  (default: Bash,Read,Edit,Write,Grep,Glob,WebFetch)\n  REVIEWER_ALLOWED_TOOLS (default: Read,Grep,Glob,WebFetch) Reviewer read-only sandbox\n  CLAUDE_SESSION_PERSISTENCE (default: 1) Persist Claude sessions across rounds\n  CLAUDE_EFFORT_LEVEL                   Thinking depth: low|medium|high\n  CLAUDE_MAX_OUTPUT_TOKENS              Max output tokens (1-64000)\n  CLAUDE_MAX_THINKING_TOKENS            Extended thinking token budget\n  IMPLEMENTER_EFFORT_LEVEL              Override effort level for implementer role\n  REVIEWER_EFFORT_LEVEL                 Override effort level for reviewer role\n\n  Codex CLI tuning:\n  CODEX_FULL_ACCESS     (default: 0)    Use --dangerously-bypass-approvals-and-sandbox instead of --full-auto\n\n  Stuck detection:\n  STUCK_DETECTION_ENABLED (default: 0)  Enable stuck detection in implementation loop\n  STUCK_NO_DIFF_ROUNDS   (default: 3)   Consecutive no-diff rounds before signalling\n  STUCK_THRESHOLD_MINUTES (default: 10)  Wall-clock minutes before signalling\n  STUCK_ACTION           (default: warn) Action on stuck: abort|warn|retry\n\n  Wave runtime:\n  WAVE_LOCK_STALE_SECONDS (default: 300) Seconds before a wave lock is considered stale\n  WAVE_SHUTDOWN_GRACE_MS  (default: 30000) Grace period (ms) for in-flight tasks on interrupt\n\nPer-project config: place .agent-loop.toml in the project root (see README)."
    )
}

fn current_project_dir() -> Result<PathBuf, AgentLoopError> {
    std::env::current_dir().map_err(AgentLoopError::from)
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
        let dependencies = wave::parse_dependencies(&content);
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

fn read_current_status(project_dir: &Path, single_agent: bool) -> Option<LoopStatus> {
    let config = Config::from_cli(project_dir.to_path_buf(), single_agent, false).ok()?;
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

/// Internal helper used by `implement --task`, `implement --file`, and
/// `implement` task-list execution attempts.
fn run_command_with_max_rounds(
    args: RunArgs,
    max_rounds_override: Option<u32>,
) -> Result<i32, AgentLoopError> {
    if args.resume {
        return Err(AgentLoopError::Config(
            "run_command_with_max_rounds must not be called with resume=true. \
             Use 'implement --resume' instead."
                .to_string(),
        ));
    }
    if args.planning_only {
        return Err(AgentLoopError::Config(
            "run_command_with_max_rounds must not be called with planning_only=true. \
             Use 'plan' subcommand instead."
                .to_string(),
        ));
    }

    let project_dir = current_project_dir()?;
    let config = Config::from_cli_with_overrides(
        project_dir,
        args.single_agent,
        false,
        max_rounds_override,
    )?;

    let task = resolve_task_for_run(&args)?;

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
    if !existing_plan.trim().is_empty() {
        state::write_state_file("plan.md", &existing_plan, &config)?;
    }

    let reached_consensus = phases::implementation_loop(&config, &baseline_set);
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

    Ok(exit_code)
}

/// Resume helper used by `implement` task retries.
fn resume_for_tasks(
    single_agent: bool,
    max_rounds_override: Option<u32>,
) -> Result<i32, AgentLoopError> {
    implementation_resume_with_max_rounds(single_agent, max_rounds_override)
}

fn plan_command(args: PlanArgs) -> Result<i32, AgentLoopError> {
    let task = resolve_task_for_plan(&args)?;
    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir, args.single_agent, false)?;

    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };

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

    Ok(exit_code)
}

/// Pre-config helper: validate that the state directory and status.json exist.
/// Operates on raw paths so it can run before `Config` is built.
fn ensure_resume_state_dir_exists(state_dir: &Path) -> Result<(), AgentLoopError> {
    if !state_dir.is_dir() {
        return Err(AgentLoopError::State(
            "Cannot resume: .agent-loop/state does not exist. Run a command first.".to_string(),
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
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes > 0 {
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

fn implementation_resume_with_max_rounds(
    single_agent: bool,
    max_rounds_override: Option<u32>,
) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let state_dir = project_dir.join(".agent-loop").join("state");
    ensure_resume_state_dir_exists(&state_dir)?;
    let task = read_resume_task_from_state_dir(&state_dir)?;

    let config =
        Config::from_cli_with_overrides(project_dir, single_agent, false, max_rounds_override)?;
    let workflow = state::read_workflow(&config);
    if workflow != Some(state::WorkflowKind::Implement) {
        return Err(AgentLoopError::State(
            "Cannot resume implementation: workflow is not 'implement'.".to_string(),
        ));
    }

    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    let baseline_set = baseline_vec.iter().cloned().collect::<HashSet<_>>();

    let reached_consensus = phases::implementation_loop_resume(&config, &baseline_set);
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

    Ok(exit_code)
}

fn implement_command(args: ImplementArgs) -> Result<i32, AgentLoopError> {
    args.validate()?;

    if args.resume {
        return implementation_resume_with_max_rounds(args.single_agent, None);
    }

    if args.task.is_some() || args.file.is_some() {
        let task = resolve_task_for_implement(&args)?;
        return run_command_with_max_rounds(
            RunArgs {
                task: Some(task),
                file: None,
                resume: false,
                planning_only: false,
                single_agent: args.single_agent,
            },
            None,
        );
    }

    implement_all_tasks_command(args)
}

fn tasks_command(args: TasksArgs) -> Result<i32, AgentLoopError> {
    args.validate()?;

    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir.clone(), args.single_agent, false)?;

    if args.resume {
        let state_dir = project_dir.join(".agent-loop").join("state");
        ensure_resume_state_dir_exists(&state_dir)?;
        let workflow = state::read_workflow(&config);
        if workflow != Some(state::WorkflowKind::Decompose) {
            return Err(AgentLoopError::State(
                "Cannot resume tasks decomposition: workflow is not 'decompose'.".to_string(),
            ));
        }

        let exit_code =
            phase_success_to_exit_code(phases::task_decomposition_phase_resume(&config));
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

    Ok(exit_code)
}

fn per_task_only_flags_present(args: &ImplementArgs) -> bool {
    args.continue_on_fail
        || args.fail_fast
        || args.max_parallel.is_some()
        || args.max_retries != 2
        || args.round_step != 2
}

fn implement_all_tasks_batch(
    args: &ImplementArgs,
    parsed_tasks: &[ParsedTask],
    config: &Config,
    project_dir: &Path,
    raw_tasks: &str,
) -> Result<i32, AgentLoopError> {
    println!("Running batch implementation for all tasks in a single loop.");
    let title = batch_metrics_title(parsed_tasks.len());
    let started_at = state::timestamp();
    let timer = Instant::now();
    let usage_before = agent::usage_snapshot();
    let combined_task = build_batch_implementation_task(raw_tasks);
    let run_result = run_command_with_max_rounds(
        RunArgs {
            task: Some(combined_task),
            file: None,
            resume: false,
            planning_only: false,
            single_agent: args.single_agent,
        },
        None,
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
                read_current_status(project_dir, args.single_agent).as_ref(),
            )),
        ),
        Err(err) => (TaskRunStatus::Failed, Some(err.to_string())),
    };

    persist_batch_task_state(
        &title,
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

    let deps: Vec<Vec<usize>> = parsed_tasks.iter().map(|t| t.dependencies.clone()).collect();
    let schedule = wave::compute_wave_schedule(parsed_tasks.len(), &deps)
        .map_err(|e| AgentLoopError::Wave(e.to_string()))?;

    let effective_max_parallel = args
        .max_parallel
        .unwrap_or(config.max_parallel)
        .max(1) as usize;

    // Acquire wave lock to prevent concurrent wave runs.
    let lock_path = config.state_dir.join("wave.lock");
    let wave_lock = wave_runtime::WaveRunLock::acquire(
        lock_path,
        "wave",
        effective_max_parallel as u32,
        config.wave_lock_stale_seconds,
    )
    .map_err(|e| AgentLoopError::Wave(e))?;

    // Journal file for progress events.
    let journal_path = config.state_dir.join("wave-progress.jsonl");

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

    // Load persisted wave status for resume support.
    let mut wave_statuses: Vec<TaskStatusEntry> = parsed_tasks
        .iter()
        .map(|t| TaskStatusEntry {
            title: t.title.clone(),
            status: TaskRunStatus::Pending,
            retries: 0,
            last_error: None,
        })
        .collect();
    let persisted = state::read_task_status(config);
    for (i, entry) in persisted.tasks.iter().enumerate() {
        if i < wave_statuses.len() && entry.title == wave_statuses[i].title {
            wave_statuses[i] = entry.clone();
        }
    }

    let mut task_results: Vec<Option<bool>> = vec![None; parsed_tasks.len()];
    // Pre-fill results from persisted state.
    for (i, entry) in wave_statuses.iter().enumerate() {
        if entry.status == TaskRunStatus::Done {
            task_results[i] = Some(true);
        }
    }
    let mut had_failures = false;
    let run_start = std::time::Instant::now();

    for (wave_idx, wave_tasks) in schedule.waves.iter().enumerate() {
        // Check for interrupt before starting each wave.
        if interrupt::is_interrupted() {
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
                println!("✅ '{}' — already done, skipping", parsed_tasks[task_idx].title);
                continue;
            }

            let deps_ok = deps[task_idx]
                .iter()
                .all(|&dep| task_results[dep] == Some(true));
            if deps_ok {
                runnable.push(task_idx);
            } else {
                println!(
                    "⏭ Skipping '{}' — dependency failed",
                    parsed_tasks[task_idx].title
                );
                task_results[task_idx] = Some(false);
                wave_statuses[task_idx].status = TaskRunStatus::Skipped;
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
            let (task_idx, succeeded) = rx.recv().map_err(|e| {
                AgentLoopError::Wave(format!("wave task channel error: {e}"))
            })?;
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

            if !succeeded && args.fail_fast {
                // Drain remaining in-flight results.
                while in_flight > 0 {
                    let _ = rx.recv();
                    in_flight -= 1;
                }
                wave_lock.release();
                println!("Aborting (--fail-fast).");
                return Ok(1);
            }

            // Check interrupt before launching next task.
            if interrupt::is_interrupted() {
                // Drain remaining in-flight.
                while in_flight > 0 {
                    let _ = rx.recv();
                    in_flight -= 1;
                }
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

    let total_passed = task_results.iter().filter(|r| **r == Some(true)).count();
    let total_failed = task_results.iter().filter(|r| **r == Some(false)).count();
    let total_skipped = task_results.iter().filter(|r| r.is_none()).count();

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
    _args: &ImplementArgs,
    tx: &std::sync::mpsc::Sender<(usize, bool)>,
) {

    let task_title = parsed_tasks[task_idx].title.clone();
    let task_content = parsed_tasks[task_idx].content.clone();
    let task_state_dir = config
        .state_dir
        .parent()
        .unwrap_or(&config.state_dir)
        .join(format!("task-{}", task_idx + 1));
    let task_config = config.with_state_dir(task_state_dir);
    let tx = tx.clone();

    println!("🚀 Starting: {task_title}");

    std::thread::spawn(move || {
        // Ensure task state directory exists.
        let _ = std::fs::create_dir_all(&task_config.state_dir);

        let result = run_command_with_config(task_content, &task_config);
        let succeeded = matches!(result, Ok(0));
        let _ = tx.send((task_idx, succeeded));
    });
}

/// Run a single implementation task with an explicit config (used by wave orchestrator).
fn run_command_with_config(
    task: String,
    config: &Config,
) -> Result<i32, AgentLoopError> {
    let baseline_vec = if git::is_git_repo(config) {
        git::list_changed_files(config)?
    } else {
        Vec::new()
    };
    let baseline_set: HashSet<String> = baseline_vec.iter().cloned().collect();

    state::init(&task, config, &baseline_vec, state::WorkflowKind::Implement)?;
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
    let base_max_rounds = config.max_rounds;

    // Resolve effective max_parallel: CLI > config > default(1).
    let effective_max_parallel = args.max_parallel.unwrap_or(config.max_parallel);
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
        if args.continue_on_fail && entry_status == TaskRunStatus::Failed {
            println!("{} — previously failed, skipping", task.title);
            task_statuses[index].status = TaskRunStatus::Skipped;
            persist_task_state(&task_statuses, &task_metrics, config)?;
            had_failures = true;
            continue;
        }

        // Check retry budget: for failed tasks, retries >= max_retries means exhausted.
        // For running tasks, retries > max_retries means truly exhausted (beyond boundary).
        // retries == max_retries for running means final attempt allowed.
        if entry_status == TaskRunStatus::Failed && persisted_retries >= args.max_retries {
            // Exhausted — do not re-execute.
            if args.continue_on_fail {
                println!("{} — retries exhausted, skipping", task.title);
                task_statuses[index].status = TaskRunStatus::Skipped;
                persist_task_state(&task_statuses, &task_metrics, config)?;
                had_failures = true;
                continue;
            } else {
                persist_task_state(&task_statuses, &task_metrics, config)?;
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' failed with retries exhausted ({persisted_retries}/{}).",
                    task.title, args.max_retries
                )));
            }
        }

        if entry_status == TaskRunStatus::Running && persisted_retries > args.max_retries {
            // Running task beyond retry boundary — mark failed immediately.
            task_statuses[index].status = TaskRunStatus::Failed;
            persist_task_state(&task_statuses, &task_metrics, config)?;
            if args.continue_on_fail {
                had_failures = true;
                continue;
            } else {
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' failed with retries exhausted ({persisted_retries}/{}).",
                    task.title, args.max_retries
                )));
            }
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
        let mut current_max_rounds = if is_resume_initial {
            base_max_rounds.saturating_add(args.round_step.saturating_mul(retry))
        } else {
            base_max_rounds
        };
        let mut accumulated_usage = agent::UsageSnapshot::default();

        let mut task_succeeded = false;
        let mut first_attempt = true;

        loop {
            let is_resume = !first_attempt || is_resume_initial;
            if !first_attempt {
                println!(
                    "Retrying with MAX_ROUNDS={} (retry {}/{})",
                    current_max_rounds, retry, args.max_retries
                );
            } else if is_resume_initial {
                println!(
                    "Resuming with MAX_ROUNDS={} (retry {}/{})",
                    current_max_rounds, retry, args.max_retries
                );
            } else {
                println!("Running with MAX_ROUNDS={current_max_rounds}");
            }

            let usage_before_attempt = agent::usage_snapshot();
            let exit_code = if is_resume {
                resume_for_tasks(args.single_agent, Some(current_max_rounds))?
            } else {
                let run_args = RunArgs {
                    task: Some(task.content.clone()),
                    file: None,
                    resume: false,
                    planning_only: false,
                    single_agent: args.single_agent,
                };
                run_command_with_max_rounds(run_args, Some(current_max_rounds))?
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

            let status = read_current_status(project_dir, args.single_agent);
            if !is_retryable_run_tasks_status(status.as_ref()) {
                // Non-retryable failure.
                break;
            }

            if retry >= args.max_retries {
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
            current_max_rounds = current_max_rounds.saturating_add(args.round_step);

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
            let status = read_current_status(project_dir, args.single_agent);
            task_statuses[index].last_error = Some(format_status_reason(status.as_ref()));
            had_failures = true;

            if !args.continue_on_fail {
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

fn implement_all_tasks_command(args: ImplementArgs) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let tasks_file = project_dir
        .join(".agent-loop")
        .join("state")
        .join("tasks.md");
    if !tasks_file.is_file() {
        return Err(AgentLoopError::State(
            "No tasks found. Run 'agent-loop tasks' first.".to_string(),
        ));
    }
    let raw_tasks = fs::read_to_string(&tasks_file).map_err(|err| {
        AgentLoopError::Config(format!("Failed to read '{}': {err}", tasks_file.display()))
    })?;
    let parsed_tasks = parse_tasks_file(&raw_tasks, &tasks_file)?;
    let config = Config::from_cli(project_dir.clone(), args.single_agent, false)?;

    println!(
        "Found {} tasks in {}",
        parsed_tasks.len(),
        tasks_file.display()
    );

    if args.wave {
        return implement_all_tasks_wave(&args, &parsed_tasks, &config, &project_dir);
    }

    let use_per_task_mode = args.per_task || !config.batch_implement;
    if per_task_only_flags_present(&args) && !use_per_task_mode {
        return Err(AgentLoopError::Config(
            "Per-task lifecycle flags require per-task mode. Use '--per-task' or set 'batch_implement = false'."
                .to_string(),
        ));
    }

    if use_per_task_mode {
        return implement_all_tasks_per_task(&args, &parsed_tasks, &config, &project_dir);
    }

    implement_all_tasks_batch(&args, &parsed_tasks, &config, &project_dir, &raw_tasks)
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

fn reset_command(args: &ResetArgs) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let state_dir = project_dir.join(".agent-loop").join("state");

    if args.wave_lock {
        let lock_path = state_dir.join("wave.lock");
        if lock_path.exists() {
            fs::remove_file(&lock_path)?;
            println!("Wave lock removed.");
        } else {
            println!("No wave lock found.");
        }
        return Ok(0);
    }

    if state_dir.exists() {
        fs::remove_dir_all(&state_dir)?;
    }
    fs::create_dir_all(&state_dir)?;
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

fn status_command() -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir, false, false)?;
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
    if let Some(rating) = current_status.rating {
        println!("rating: {rating}");
    }
    if let Some(reason) = current_status
        .reason
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        println!("reason: {reason}");
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

    if show_resume {
        if warnings.is_empty() {
            println!();
        }
        println!(
            "Hint: run `agent-loop implement --resume` or `agent-loop tasks --resume` to continue, or `agent-loop reset` to start fresh."
        );
    }

    // Wave lock info.
    let lock_path = config.state_dir.join("wave.lock");
    if lock_path.exists() {
        if let Ok(raw) = fs::read_to_string(&lock_path) {
            if let Ok(lock) = serde_json::from_str::<wave_runtime::LockFileContent>(&raw) {
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
        }
    }

    // Recent wave progress events.
    let journal_path = config.state_dir.join("wave-progress.jsonl");
    let recent = wave_runtime::read_recent_events(&journal_path, 5);
    if !recent.is_empty() {
        println!();
        println!("Recent wave events:");
        for event in &recent {
            match event {
                wave_runtime::WaveProgressEvent::RunStart { timestamp, total_tasks, total_waves, max_parallel } => {
                    println!("  [{timestamp}] RunStart: {total_tasks} tasks, {total_waves} waves, parallel={max_parallel}");
                }
                wave_runtime::WaveProgressEvent::WaveStart { timestamp, wave_index, task_count } => {
                    println!("  [{timestamp}] WaveStart: wave {}, {} tasks", wave_index + 1, task_count);
                }
                wave_runtime::WaveProgressEvent::TaskEnd { timestamp, task_index, title, success, .. } => {
                    let icon = if *success { "ok" } else { "FAIL" };
                    println!("  [{timestamp}] TaskEnd: task {} '{}' — {icon}", task_index + 1, title);
                }
                wave_runtime::WaveProgressEvent::WaveEnd { timestamp, wave_index, passed, failed } => {
                    println!("  [{timestamp}] WaveEnd: wave {} — {passed} passed, {failed} failed", wave_index + 1);
                }
                wave_runtime::WaveProgressEvent::RunEnd { timestamp, total_passed, total_failed, total_skipped } => {
                    println!("  [{timestamp}] RunEnd: {total_passed} passed, {total_failed} failed, {total_skipped} skipped");
                }
                wave_runtime::WaveProgressEvent::RunInterrupted { timestamp, reason } => {
                    println!("  [{timestamp}] RunInterrupted: {reason}");
                }
                wave_runtime::WaveProgressEvent::TaskStart { timestamp, task_index, title, .. } => {
                    println!("  [{timestamp}] TaskStart: task {} '{}'", task_index + 1, title);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn os_argv(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn normalize_argv_does_not_inject_legacy_run() {
        let normalized = normalize_argv(os_argv(&["agent-loop", "plain", "text"]));
        assert_eq!(normalized, os_argv(&["agent-loop", "plain", "text"]));
    }

    #[test]
    fn dispatch_tasks_with_removed_tasks_file_returns_migration_error() {
        let cli = Cli::try_parse_from(["agent-loop", "tasks", "--tasks-file", "old.md"])
            .expect("tasks --tasks-file should parse for migration");
        let dispatch = dispatch_from_cli(cli).expect("dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::MigrationError(
                "'--tasks-file' has been removed. Use '--file' instead.".to_string()
            )
        );
    }

    #[test]
    fn dispatch_legacy_run_variants_return_exact_migration_messages() {
        let run =
            Cli::try_parse_from(["agent-loop", "run", "task text"]).expect("run should parse");
        let dispatch = dispatch_from_cli(run).expect("dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::MigrationError("'run' has been removed. Use 'implement'.".to_string())
        );

        let run_planning = Cli::try_parse_from(["agent-loop", "run", "--planning-only"])
            .expect("run planning should parse");
        let dispatch = dispatch_from_cli(run_planning).expect("dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::MigrationError(
                "'run --planning-only' has been removed. Use 'plan'.".to_string()
            )
        );

        let run_resume = Cli::try_parse_from(["agent-loop", "run", "--resume"])
            .expect("run resume should parse");
        let dispatch = dispatch_from_cli(run_resume).expect("dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::MigrationError(
                "'run --resume' has been removed. Use 'implement --resume' or 'tasks --resume'."
                    .to_string()
            )
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

        let reset = dispatch_from_cli(Cli::try_parse_from(["agent-loop", "reset"]).unwrap())
            .expect("reset dispatch should succeed");
        assert!(matches!(reset, Dispatch::Reset(_)));
    }
}
