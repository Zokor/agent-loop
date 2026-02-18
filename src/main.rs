mod agent;
mod config;
mod error;
mod git;
mod interrupt;
mod phases;
mod prompts;
mod state;
#[cfg(test)]
mod test_support;

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

const KNOWN_SUBCOMMANDS: [&str; 9] = [
    "run",
    "plan",
    "resume",
    "tasks",
    "run-tasks",
    "init",
    "status",
    "version",
    "help",
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
    /// Run a task through the implementation/review loop
    Run(RunArgs),
    /// Plan and decompose a task (no implementation)
    Plan(PlanArgs),
    /// Resume an interrupted loop from saved state
    Resume(ResumeArgs),
    /// Execute all tasks from the tasks file
    Tasks(TasksArgs),
    #[command(name = "run-tasks", hide = true)]
    RunTasksDeprecated(TasksArgs),
    /// Create .agent-loop/state/ in current directory
    Init,
    /// Show current loop status
    Status,
    /// Print version
    Version,
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
struct ResumeArgs {
    #[arg(long)]
    single_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeDispatch {
    args: ResumeArgs,
    planning_only_legacy_override: Option<bool>,
    /// When set, overrides the max rounds in Config (used by tasks retry loop).
    max_rounds_override: Option<u32>,
    /// When true, skip workflow resolution and force implementation mode
    /// (used by tasks retry loop where resume always means "continue implementing").
    force_implementation: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct TasksArgs {
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long, value_name = "PATH", hide = true)]
    tasks_file: Option<PathBuf>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Dispatch {
    ShowHelp,
    Run(RunArgs),
    Plan(PlanArgs),
    Resume(ResumeDispatch),
    Tasks(TasksArgs),
    Init,
    Status,
    Version,
}

impl TasksArgs {
    fn validate(&self) -> Result<(), AgentLoopError> {
        if self.file.is_some() && self.tasks_file.is_some() {
            return Err(AgentLoopError::Config(
                "--file and --tasks-file cannot be used together.".to_string(),
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

    fn effective_file(&self, project_dir: &Path) -> PathBuf {
        if let Some(ref file) = self.file {
            return file.clone();
        }

        if let Some(ref tasks_file) = self.tasks_file {
            eprintln!("Warning: --tasks-file is deprecated. Use --file instead.");
            return tasks_file.clone();
        }

        project_dir
            .join(".agent-loop")
            .join("state")
            .join("tasks.md")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTask {
    title: String,
    content: String,
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
        ParseOutcome::Parsed(cli) => {
            let dispatch = dispatch_from_cli(cli)?;
            execute_dispatch(
                dispatch,
                run_command,
                plan_command,
                resume_command,
                tasks_command,
                init_command,
                status_command,
                version_command,
                print_help_message,
            )
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
        Some(Commands::Run(args)) if args.resume && args.planning_only => {
            // Legacy: `run --planning-only --resume`
            if args.task.is_some() || args.file.is_some() {
                return Err(AgentLoopError::Config(
                    "--resume cannot be combined with TASK text or --file.".to_string(),
                ));
            }
            eprintln!(
                "Warning: 'run --planning-only --resume' is deprecated. Use 'resume' instead."
            );
            Ok(Dispatch::Resume(ResumeDispatch {
                args: ResumeArgs {
                    single_agent: args.single_agent,
                },
                planning_only_legacy_override: Some(true),
                max_rounds_override: None,
                force_implementation: false,
            }))
        }
        Some(Commands::Run(args)) if args.resume => {
            // Legacy: `run --resume`
            if args.task.is_some() || args.file.is_some() {
                return Err(AgentLoopError::Config(
                    "--resume cannot be combined with TASK text or --file.".to_string(),
                ));
            }
            eprintln!("Warning: 'run --resume' is deprecated. Use 'resume' instead.");
            Ok(Dispatch::Resume(ResumeDispatch {
                args: ResumeArgs {
                    single_agent: args.single_agent,
                },
                planning_only_legacy_override: None,
                max_rounds_override: None,
                force_implementation: false,
            }))
        }
        Some(Commands::Run(args)) if args.planning_only => {
            // Legacy: `run --planning-only`
            eprintln!("Warning: 'run --planning-only' is deprecated. Use 'plan' instead.");
            Ok(Dispatch::Plan(PlanArgs {
                task: args.task,
                file: args.file,
                single_agent: args.single_agent,
            }))
        }
        Some(Commands::Run(args)) => Ok(Dispatch::Run(args)),
        Some(Commands::Plan(args)) => Ok(Dispatch::Plan(args)),
        Some(Commands::Resume(args)) => Ok(Dispatch::Resume(ResumeDispatch {
            args,
            planning_only_legacy_override: None,
            max_rounds_override: None,
            force_implementation: false,
        })),
        Some(Commands::Tasks(args)) => Ok(Dispatch::Tasks(args)),
        Some(Commands::RunTasksDeprecated(args)) => {
            eprintln!("Warning: 'run-tasks' is deprecated. Use 'tasks' instead.");
            Ok(Dispatch::Tasks(args))
        }
        Some(Commands::Init) => Ok(Dispatch::Init),
        Some(Commands::Status) => Ok(Dispatch::Status),
        Some(Commands::Version) => Ok(Dispatch::Version),
        None => Ok(Dispatch::ShowHelp),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_dispatch<FRun, FPlan, FResume, FTasks, FInit, FStatus, FVersion, FHelp>(
    dispatch: Dispatch,
    run_handler: FRun,
    plan_handler: FPlan,
    resume_handler: FResume,
    tasks_handler: FTasks,
    init_handler: FInit,
    status_handler: FStatus,
    version_handler: FVersion,
    help_handler: FHelp,
) -> Result<i32, AgentLoopError>
where
    FRun: FnOnce(RunArgs) -> Result<i32, AgentLoopError>,
    FPlan: FnOnce(PlanArgs) -> Result<i32, AgentLoopError>,
    FResume: FnOnce(ResumeDispatch) -> Result<i32, AgentLoopError>,
    FTasks: FnOnce(TasksArgs) -> Result<i32, AgentLoopError>,
    FInit: FnOnce() -> Result<i32, AgentLoopError>,
    FStatus: FnOnce() -> Result<i32, AgentLoopError>,
    FVersion: FnOnce() -> Result<i32, AgentLoopError>,
    FHelp: FnOnce() -> Result<(), AgentLoopError>,
{
    match dispatch {
        Dispatch::Run(args) => run_handler(args),
        Dispatch::Plan(args) => plan_handler(args),
        Dispatch::Resume(dispatch) => resume_handler(dispatch),
        Dispatch::Tasks(args) => tasks_handler(args),
        Dispatch::Init => init_handler(),
        Dispatch::Status => status_handler(),
        Dispatch::Version => version_handler(),
        Dispatch::ShowHelp => {
            help_handler()?;
            Ok(0)
        }
    }
}

fn normalize_argv<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut normalized = args.into_iter().map(Into::into).collect::<Vec<_>>();
    if normalized.len() <= 1 {
        return normalized;
    }

    let first_user_arg = normalized[1].to_string_lossy();
    let is_flag = first_user_arg.starts_with('-');
    let is_known_subcommand = KNOWN_SUBCOMMANDS.contains(&first_user_arg.as_ref());

    if !is_flag && !is_known_subcommand {
        normalized.insert(1, OsString::from("run"));
    }

    normalized
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
        "Primary commands:\n  agent-loop run <task>      Run implementation/review loop\n  agent-loop plan <task>     Plan and decompose only (no code)\n  agent-loop resume          Continue from saved state\n  agent-loop tasks           Execute all tasks from tasks file\n\nDeprecated flags (still supported):\n  agent-loop run --planning-only   → use 'plan' instead\n  agent-loop run --resume          → use 'resume' instead\n\nConfiguration sources (highest precedence first):\n  1. CLI flags and subcommands (--single-agent, plan, resume, tasks)\n  2. Environment variables\n  3. .agent-loop.toml (per-project config file)\n  4. Built-in defaults\n\nEnvironment variables:\n  MAX_ROUNDS            (default: {DEFAULT_MAX_ROUNDS})   Max implementation/review rounds\n  PLANNING_MAX_ROUNDS   (default: {DEFAULT_PLANNING_MAX_ROUNDS})  Max planning consensus rounds\n  DECOMPOSITION_MAX_ROUNDS (default: {DEFAULT_DECOMPOSITION_MAX_ROUNDS})  Max decomposition rounds\n  TIMEOUT               (default: {DEFAULT_TIMEOUT_SECONDS})  Idle timeout in seconds\n  IMPLEMENTER           (default: claude) Implementer agent: claude|codex\n  REVIEWER                              Reviewer agent: claude|codex (default: opposite of implementer)\n  SINGLE_AGENT          (default: 0)    Enable single-agent mode when truthy\n  AUTO_COMMIT           (default: 1)    Auto-commit loop-owned changes (0 disables)\n  AUTO_TEST             (default: 0)    Run quality checks before review when truthy\n  AUTO_TEST_CMD                         Override auto-detected quality check command\n\nPer-project config: place .agent-loop.toml in the project root (see README)."
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
        parsed.push(ParsedTask {
            title: title.clone(),
            content,
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

fn ensure_tasks_file_exists(config: &Config) -> Result<(), AgentLoopError> {
    let tasks_path = config.state_dir.join("tasks.md");
    if tasks_path.exists() {
        return Ok(());
    }

    state::write_state_file("tasks.md", "", config)?;
    Ok(())
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

fn execute_run_phases<FPlanning, FDecomposition, FImplementation, FSummary>(
    config: &Config,
    baseline_set: &HashSet<String>,
    planning_only: bool,
    planning_phase_fn: FPlanning,
    decomposition_phase_fn: FDecomposition,
    implementation_loop_fn: FImplementation,
    print_summary_fn: FSummary,
) -> i32
where
    FPlanning: FnOnce(&Config, bool) -> bool,
    FDecomposition: FnOnce(&Config) -> bool,
    FImplementation: FnOnce(&Config, &HashSet<String>) -> bool,
    FSummary: FnOnce(&Config),
{
    let planning_succeeded = planning_phase_fn(config, planning_only);
    if !planning_succeeded {
        return 1;
    }

    if planning_only {
        return phase_success_to_exit_code(decomposition_phase_fn(config));
    }

    let reached_consensus = implementation_loop_fn(config, baseline_set);
    print_summary_fn(config);
    phase_success_to_exit_code(reached_consensus)
}

/// Internal helper used by `run_command` and `tasks_command` (fresh runs only).
///
/// This function handles exclusively fresh implementation runs. Resume and
/// planning-only paths are handled by `resume_command` and `plan_command`
/// respectively, through dispatch rewriting.
///
/// # Panics / Errors
///
/// Returns `AgentLoopError::Config` if `args.resume` or `args.planning_only`
/// are set — those flags must never reach this path since dispatch rewrites
/// them to `Dispatch::Resume` and `Dispatch::Plan`.
fn run_command_with_max_rounds(
    args: RunArgs,
    max_rounds_override: Option<u32>,
) -> Result<i32, AgentLoopError> {
    if args.resume {
        return Err(AgentLoopError::Config(
            "run_command_with_max_rounds must not be called with resume=true. \
             Use 'resume' subcommand instead."
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

    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Run,
    )?;
    ensure_tasks_file_exists(&config)?;
    let exit_code = execute_run_phases(
        &config,
        &baseline_set,
        false, // planning_only — fresh runs always implement
        phases::planning_phase,
        phases::task_decomposition_phase,
        phases::implementation_loop,
        phases::print_summary,
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

    Ok(exit_code)
}

/// Resume helper used by `tasks_command` for resuming interrupted task attempts.
///
/// Delegates to `resume_command_inner` with `force_implementation: true` to skip
/// workflow resolution (tasks always resume in implementation mode) and the
/// provided `max_rounds_override` for retry-budget control.
fn resume_for_tasks(
    single_agent: bool,
    max_rounds_override: Option<u32>,
) -> Result<i32, AgentLoopError> {
    let dispatch = ResumeDispatch {
        args: ResumeArgs { single_agent },
        planning_only_legacy_override: None,
        max_rounds_override,
        force_implementation: true,
    };
    resume_command_inner(
        dispatch,
        current_project_dir,
        phases::task_decomposition_phase_resume,
        phases::implementation_loop_resume,
        phases::print_summary,
    )
}

fn run_command(args: RunArgs) -> Result<i32, AgentLoopError> {
    run_command_with_max_rounds(args, None)
}

fn plan_command(args: PlanArgs) -> Result<i32, AgentLoopError> {
    plan_command_inner(
        args,
        current_project_dir,
        phases::planning_phase,
        phases::task_decomposition_phase,
    )
}

fn plan_command_inner<FProjectDir, FPlanning, FDecomposition>(
    args: PlanArgs,
    project_dir_fn: FProjectDir,
    planning_phase_fn: FPlanning,
    decomposition_phase_fn: FDecomposition,
) -> Result<i32, AgentLoopError>
where
    FProjectDir: FnOnce() -> Result<PathBuf, AgentLoopError>,
    FPlanning: FnOnce(&Config, bool) -> bool,
    FDecomposition: FnOnce(&Config) -> bool,
{
    let task = resolve_task_for_plan(&args)?;
    let project_dir = project_dir_fn()?;
    let config = Config::from_cli(project_dir, args.single_agent, false)?;

    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    let baseline_set = baseline_vec.iter().cloned().collect::<HashSet<_>>();

    state::init(
        task.as_str(),
        &config,
        &baseline_vec,
        state::WorkflowKind::Plan,
    )?;
    ensure_tasks_file_exists(&config)?;

    // Route through execute_run_phases with planning_only=true, so
    // execute_run_phases will run planning + decomposition only (the
    // implementation and summary closures are never called). We pass
    // unreachable guards to make this guarantee explicit.
    let exit_code = execute_run_phases(
        &config,
        &baseline_set,
        true, // planning_only
        planning_phase_fn,
        decomposition_phase_fn,
        |_, _| unreachable!("implementation must not run in plan_command"),
        |_| unreachable!("summary must not run in plan_command"),
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

    Ok(exit_code)
}

fn resume_command(dispatch: ResumeDispatch) -> Result<i32, AgentLoopError> {
    resume_command_inner(
        dispatch,
        current_project_dir,
        phases::task_decomposition_phase_resume,
        phases::implementation_loop_resume,
        phases::print_summary,
    )
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

/// Pre-config helper: read the persisted workflow marker.
fn read_workflow_marker_from_state_dir(state_dir: &Path) -> Option<state::WorkflowKind> {
    let raw = fs::read_to_string(state_dir.join("workflow.txt")).unwrap_or_default();
    raw.trim().parse().ok()
}

/// Infer the workflow kind from pre-migration artifacts using strong signals only.
///
/// Returns `Some(WorkflowKind::Plan)` when `tasks.md` contains task headings
/// (`## Task …` or `### Task …`) and no implementation status artifacts exist.
///
/// Returns `Some(WorkflowKind::Run)` when the status indicates an implementation
/// phase (implementing, reviewing, needs_changes) or implementation artifacts
/// (`changes.md`, `review.md`) have non-trivial content.
///
/// Returns `None` when signals are absent or conflicting (no unsafe guess).
fn infer_workflow_from_artifacts(state_dir: &Path) -> Option<state::WorkflowKind> {
    let tasks_content = fs::read_to_string(state_dir.join("tasks.md")).unwrap_or_default();
    let has_task_headings = tasks_content
        .lines()
        .any(|line| task_heading(line).is_some());

    // Parse status from status.json for signal classification.
    let status_str: Option<String> = {
        let raw = fs::read_to_string(state_dir.join("status.json")).unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|parsed| {
                parsed
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
    };

    // Check implementation-specific signals from status.json.
    // Only statuses that exclusively belong to the implementation/review loop
    // are strong run signals. DISPUTED, APPROVED, CONSENSUS, and
    // NEEDS_REVISION can also appear in planning/decomposition flows, so
    // they are not reliable implementation indicators.
    let has_impl_status = matches!(
        status_str.as_deref(),
        Some("IMPLEMENTING" | "REVIEWING" | "NEEDS_CHANGES")
    );

    // Decomposition-terminal statuses: these indicate that decomposition
    // completed successfully. They are ambiguous because they can appear in
    // both plan workflows (where decomposition IS the final output) and run
    // workflows (where decomposition is phase 1 before implementation).
    let has_decomposition_terminal_status =
        matches!(status_str.as_deref(), Some("CONSENSUS" | "APPROVED"));

    // Check for non-trivial implementation artifacts.
    let changes_content = fs::read_to_string(state_dir.join("changes.md")).unwrap_or_default();
    let review_content = fs::read_to_string(state_dir.join("review.md")).unwrap_or_default();
    let has_impl_artifacts =
        !changes_content.trim().is_empty() || !review_content.trim().is_empty();

    let plan_signal = has_task_headings;
    let run_signal = has_impl_status || has_impl_artifacts;

    // Conflicting signals → no guess.
    if plan_signal && run_signal {
        return None;
    }

    if run_signal {
        return Some(state::WorkflowKind::Run);
    }

    if plan_signal {
        // Task headings alone are a plan signal, but when combined with a
        // decomposition-terminal status (CONSENSUS/APPROVED) and no
        // implementation artifacts, we cannot distinguish between:
        //   - A plan workflow that completed successfully
        //   - A run workflow interrupted between decomposition and implementation
        // Return None (ambiguous) to avoid misclassifying run sessions as plan,
        // which would cause resume to re-enter decomposition instead of
        // starting implementation.
        if has_decomposition_terminal_status {
            return None;
        }
        return Some(state::WorkflowKind::Plan);
    }

    None
}

/// Resolve the workflow kind for a resume operation.
///
/// Precedence (strict):
///   1. `dispatch.planning_only_legacy_override` from `run --planning-only --resume`
///   2. Persisted `workflow.txt` marker in state directory
///   3. Pre-migration inference from artifacts (tasks.md, status, changes/review)
///   4. Error: cannot determine workflow
fn resolve_workflow_for_resume(
    dispatch: &ResumeDispatch,
    state_dir: &Path,
) -> Result<state::WorkflowKind, AgentLoopError> {
    // 1. Legacy override takes absolute precedence.
    if let Some(planning_only) = dispatch.planning_only_legacy_override {
        return Ok(if planning_only {
            state::WorkflowKind::Plan
        } else {
            state::WorkflowKind::Run
        });
    }

    // 2. Persisted workflow marker.
    if let Some(workflow) = read_workflow_marker_from_state_dir(state_dir) {
        return Ok(workflow);
    }

    // 3. Pre-migration inference from artifacts.
    if let Some(workflow) = infer_workflow_from_artifacts(state_dir) {
        eprintln!(
            "Warning: workflow.txt is missing. Inferred workflow '{}' from state artifacts. \
             Consider running with an explicit workflow to persist the marker.",
            workflow
        );
        return Ok(workflow);
    }

    // 4. Cannot determine workflow.
    Err(AgentLoopError::State(
        "Cannot resume: unable to determine whether the session is a 'plan' or 'run' workflow. \
         No workflow.txt marker found and no inferrable artifacts. Please either:\n  \
         - Write 'plan' or 'run' to .agent-loop/state/workflow.txt, or\n  \
         - Start a fresh session with 'agent-loop plan' or 'agent-loop run'."
            .to_string(),
    ))
}

fn resume_command_inner<FProjectDir, FDecompositionResume, FImplementationResume, FSummary>(
    dispatch: ResumeDispatch,
    project_dir_fn: FProjectDir,
    decomposition_resume_fn: FDecompositionResume,
    implementation_resume_fn: FImplementationResume,
    print_summary_fn: FSummary,
) -> Result<i32, AgentLoopError>
where
    FProjectDir: FnOnce() -> Result<PathBuf, AgentLoopError>,
    FDecompositionResume: FnOnce(&Config) -> bool,
    FImplementationResume: FnOnce(&Config, &HashSet<String>) -> bool,
    FSummary: FnOnce(&Config),
{
    let project_dir = project_dir_fn()?;
    let state_dir = project_dir.join(".agent-loop").join("state");

    // Step 1: Pre-config validation (no Config needed yet).
    ensure_resume_state_dir_exists(&state_dir)?;
    let task = read_resume_task_from_state_dir(&state_dir)?;

    // Step 2: Resolve workflow with documented precedence.
    // When force_implementation is set (tasks retry loop), skip resolution
    // and always resume in implementation mode.
    let workflow = if dispatch.force_implementation {
        state::WorkflowKind::Run
    } else {
        resolve_workflow_for_resume(&dispatch, &state_dir)?
    };

    // Step 3: Build config and determine planning_only from resolved workflow.
    let planning_only = matches!(workflow, state::WorkflowKind::Plan);
    let config = Config::from_cli_with_overrides(
        project_dir,
        dispatch.args.single_agent,
        false, // verbose
        dispatch.max_rounds_override,
    )?;

    // Step 4: Collect baseline.
    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    let baseline_set = baseline_vec.iter().cloned().collect::<HashSet<_>>();

    // Step 5: Ensure tasks file exists and execute resume phases.
    ensure_tasks_file_exists(&config)?;

    let exit_code = if planning_only {
        phase_success_to_exit_code(decomposition_resume_fn(&config))
    } else {
        let reached_consensus = implementation_resume_fn(&config, &baseline_set);
        print_summary_fn(&config);
        phase_success_to_exit_code(reached_consensus)
    };

    // Step 6: Persist last run task.
    persist_last_run_task(task.as_str(), &config)?;

    // Step 7: Check for interrupt signal propagation.
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

fn reconcile_task_status(parsed_tasks: &[ParsedTask], config: &Config) -> Vec<TaskStatusEntry> {
    let persisted = state::read_task_status_with_warnings(config);
    let persisted_entries = persisted.status_file.tasks;

    // Positional reconciliation: match by index, not by title.
    // Always use the current parsed title so edits between runs are reflected.
    parsed_tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            if let Some(entry) = persisted_entries.get(i) {
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

    // Always use the current parsed title so edits between runs are reflected.
    parsed_tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            if let Some(entry) = persisted_entries.get(i) {
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

fn tasks_command(args: TasksArgs) -> Result<i32, AgentLoopError> {
    args.validate()?;

    let project_dir = current_project_dir()?;
    let tasks_file = args.effective_file(&project_dir);
    let raw_tasks = fs::read_to_string(&tasks_file).map_err(|err| {
        AgentLoopError::Config(format!("Failed to read '{}': {err}", tasks_file.display()))
    })?;
    let parsed_tasks = parse_tasks_file(&raw_tasks, &tasks_file)?;
    let config = Config::from_cli(project_dir.clone(), args.single_agent, false)?;
    let base_max_rounds = config.max_rounds;

    // Resolve effective max_parallel: CLI > config > default(1).
    let effective_max_parallel = args.max_parallel.unwrap_or(config.max_parallel);
    if effective_max_parallel > 1 {
        eprintln!(
            "Parallel task execution is not yet supported; running sequentially with max_parallel=1"
        );
    }

    // Reconcile persisted task status and metrics with current task list.
    let mut task_statuses = reconcile_task_status(&parsed_tasks, &config);
    let mut task_metrics = reconcile_task_metrics(&parsed_tasks, &config);

    println!(
        "Found {} tasks in {}",
        parsed_tasks.len(),
        tasks_file.display()
    );

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
            persist_task_state(&task_statuses, &task_metrics, &config)?;
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
                persist_task_state(&task_statuses, &task_metrics, &config)?;
                had_failures = true;
                continue;
            } else {
                persist_task_state(&task_statuses, &task_metrics, &config)?;
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' failed with retries exhausted ({persisted_retries}/{}).",
                    task.title, args.max_retries
                )));
            }
        }

        if entry_status == TaskRunStatus::Running && persisted_retries > args.max_retries {
            // Running task beyond retry boundary — mark failed immediately.
            task_statuses[index].status = TaskRunStatus::Failed;
            persist_task_state(&task_statuses, &task_metrics, &config)?;
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
        persist_task_state(&task_statuses, &task_metrics, &config)?;

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

            let status = read_current_status(&project_dir, args.single_agent);
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
            persist_task_state(&task_statuses, &task_metrics, &config)?;
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
            let status = read_current_status(&project_dir, args.single_agent);
            task_statuses[index].last_error = Some(format_status_reason(status.as_ref()));
            had_failures = true;

            if !args.continue_on_fail {
                // Fail-fast: persist and exit.
                persist_task_state(&task_statuses, &task_metrics, &config)?;
                print_task_duration_summary(&task_metrics);
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' failed with status {}.",
                    task.title,
                    format_status_reason(status.as_ref())
                )));
            }
        }

        persist_task_state(&task_statuses, &task_metrics, &config)?;
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

fn init_command() -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir, false, false)?;

    for file in [
        "task.md",
        "plan.md",
        "tasks.md",
        "changes.md",
        "review.md",
        "log.txt",
        "status.json",
        "task_status.json",
        "task_metrics.json",
    ] {
        state::write_state_file(file, "", &config)?;
    }

    state::write_status(
        StatusPatch {
            status: Some(Status::Pending),
            round: Some(0),
            implementer: Some(config.implementer.to_string()),
            reviewer: Some(config.reviewer.to_string()),
            mode: Some(config.run_mode.to_string()),
            last_run_task: Some(String::new()),
            ..StatusPatch::default()
        },
        &config,
    )?;

    println!("initialized {}", config.state_dir.display());
    Ok(0)
}

fn is_terminal_status(status: &state::Status) -> bool {
    matches!(
        status,
        state::Status::MaxRounds | state::Status::Error | state::Status::Interrupted
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
        println!("Hint: status.json may be corrupted. Run `agent-loop init` to reset state.");
    }

    // Print resume/init hints for terminal or stale statuses.
    let show_resume = is_terminal_status(&current_status.status)
        || has_stale_reason(current_status.reason.as_deref());

    if show_resume {
        if warnings.is_empty() {
            println!();
        }
        println!("Hint: run `agent-loop resume` to continue, or `agent-loop init` to start fresh.");
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
    use crate::test_support::{TestConfigOptions, make_test_config, unique_temp_path};
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn os_argv(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    /// Clear env vars that affect `Config::from_cli_with_overrides`. Must be called
    /// after acquiring `env_lock()` in tests that build real configs from the env.
    fn clear_config_env() {
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
            // SAFETY: tests serialize env mutation with env_lock().
            unsafe {
                std::env::remove_var(key);
            }
        }
    }

    fn test_config() -> Config {
        let project_dir = PathBuf::from("/tmp/agent-loop-main-tests");
        make_test_config(&project_dir, TestConfigOptions::default())
    }

    fn run_args(task: Option<&str>, file: Option<PathBuf>) -> RunArgs {
        RunArgs {
            task: task.map(ToOwned::to_owned),
            file,
            resume: false,
            planning_only: false,
            single_agent: false,
        }
    }

    fn tasks_args(file: Option<PathBuf>, tasks_file: Option<PathBuf>) -> TasksArgs {
        TasksArgs {
            file,
            tasks_file,
            max_retries: 2,
            round_step: 2,
            single_agent: false,
            continue_on_fail: false,
            fail_fast: false,
            max_parallel: None,
        }
    }

    fn loop_status(status: Status, reason: Option<&str>) -> LoopStatus {
        LoopStatus {
            status,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "Task 1".to_string(),
            reason: reason.map(ToOwned::to_owned),
            rating: None,
            timestamp: "2026-02-15T15:09:44.850Z".to_string(),
        }
    }

    fn unique_temp_file(prefix: &str) -> PathBuf {
        unique_temp_path(prefix)
    }

    fn write_temp_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("temp parent directory should be created");
        }
        fs::write(path, content).expect("temp file should be written");
    }

    #[test]
    fn normalize_argv_injects_run_for_bare_task() {
        let normalized = normalize_argv(os_argv(&["agent-loop", "sample task"]));
        assert_eq!(normalized, os_argv(&["agent-loop", "run", "sample task"]));
    }

    #[test]
    fn normalize_argv_leaves_explicit_subcommands_unchanged() {
        for command in KNOWN_SUBCOMMANDS {
            let normalized = normalize_argv(os_argv(&["agent-loop", command]));
            assert_eq!(normalized, os_argv(&["agent-loop", command]));
        }
    }

    #[test]
    fn normalize_argv_does_not_rewrite_flags() {
        let normalized = normalize_argv(os_argv(&["agent-loop", "--single-agent", "task"]));
        assert_eq!(
            normalized,
            os_argv(&["agent-loop", "--single-agent", "task"])
        );
    }

    #[test]
    fn normalize_argv_leaves_version_subcommand_unchanged() {
        let normalized = normalize_argv(os_argv(&["agent-loop", "version"]));
        assert_eq!(normalized, os_argv(&["agent-loop", "version"]));
    }

    #[test]
    fn parse_cli_from_version_subcommand_dispatches_version() {
        let parsed = parse_cli_from(os_argv(&["agent-loop", "version"]))
            .expect("version subcommand should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("version subcommand should produce parsed CLI");
        };
        assert_eq!(
            dispatch_from_cli(cli).expect("version dispatch should succeed"),
            Dispatch::Version
        );
    }

    #[test]
    fn parse_cli_from_version_flag_exits_successfully() {
        let parsed = parse_cli_from(os_argv(&["agent-loop", "--version"]))
            .expect("version flag should parse");
        assert!(matches!(parsed, ParseOutcome::Exit(0)));
    }

    #[test]
    fn resolve_task_for_run_prefers_file_over_positional_text() {
        let task_file = unique_temp_file("agent_loop_task_precedence");
        write_temp_file(&task_file, "task from file");

        let args = run_args(Some("task from positional"), Some(task_file.clone()));
        let resolved = resolve_task_for_run(&args).expect("task should resolve from file");
        assert_eq!(resolved, "task from file");

        let _ = fs::remove_file(task_file);
    }

    #[test]
    fn resolve_task_for_run_uses_positional_task_when_file_is_missing() {
        let args = run_args(Some("task from positional"), None);
        let resolved = resolve_task_for_run(&args).expect("positional task should be used");
        assert_eq!(resolved, "task from positional");
    }

    #[test]
    fn resolve_task_for_run_rejects_missing_or_empty_tasks() {
        let missing_task_error =
            resolve_task_for_run(&run_args(None, None)).expect_err("missing task should fail");
        assert!(missing_task_error.to_string().contains("Task is required"));

        let empty_positional_error =
            resolve_task_for_run(&run_args(Some("   "), None)).expect_err("empty task should fail");
        assert!(
            empty_positional_error
                .to_string()
                .contains("cannot be empty")
        );

        let task_file = unique_temp_file("agent_loop_task_empty_file");
        write_temp_file(&task_file, "  \n\t");

        let empty_file_error = resolve_task_for_run(&run_args(None, Some(task_file.clone())))
            .expect_err("empty file task should fail");
        assert!(empty_file_error.to_string().contains("is empty"));

        let _ = fs::remove_file(task_file);
    }

    #[test]
    fn task_heading_detects_supported_markdown_levels() {
        assert_eq!(
            task_heading("### Task 1: Build parser"),
            Some("Task 1: Build parser".to_string())
        );
        assert_eq!(
            task_heading("## Task 2: Add retries"),
            Some("Task 2: Add retries".to_string())
        );
        assert_eq!(task_heading("#### Task 3: Ignore"), None);
        assert_eq!(task_heading("### Phase 1"), None);
    }

    #[test]
    fn parse_tasks_markdown_extracts_full_sections() {
        let markdown = r#"
# Tasks

### Task 1: First
Line A

Line B

### Task 2: Second
Line C
"#;

        let tasks = parse_tasks_markdown(markdown).expect("tasks should parse");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "Task 1: First");
        assert!(tasks[0].content.contains("Line A"));
        assert!(tasks[0].content.contains("Line B"));
        assert_eq!(tasks[1].title, "Task 2: Second");
        assert!(tasks[1].content.contains("Line C"));
    }

    #[test]
    fn parse_tasks_markdown_rejects_missing_task_headings() {
        let err = parse_tasks_markdown("# No tasks here")
            .expect_err("missing task headings should return error");
        assert!(err.to_string().contains("No tasks found"));
    }

    #[test]
    fn parse_tasks_file_rejects_empty_input_with_actionable_message() {
        let tasks_file = Path::new("/tmp/.agent-loop/state/tasks.md");
        let err = parse_tasks_file("   \n\t", tasks_file)
            .expect_err("empty tasks file should return error");
        let msg = err.to_string();
        assert!(msg.contains("is empty"), "error should mention empty file");
        assert!(
            msg.contains(tasks_file.to_string_lossy().as_ref()),
            "error should include tasks file path"
        );
        assert!(
            msg.contains("agent-loop plan --file <PLAN.md>"),
            "error should suggest a recovery command"
        );
    }

    #[test]
    fn parse_tasks_file_accepts_valid_task_markdown() {
        let tasks_file = Path::new("/tmp/.agent-loop/state/tasks.md");
        let parsed = parse_tasks_file("### Task 1: Build parser\ncontent\n", tasks_file)
            .expect("valid tasks markdown should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Task 1: Build parser");
    }

    #[test]
    fn validate_rejects_zero_round_step() {
        let mut args = tasks_args(None, None);
        args.round_step = 0;

        let err = args.validate().expect_err("round_step=0 should fail");
        assert!(err.to_string().contains("--round-step"));
    }

    #[test]
    fn validate_rejects_file_and_tasks_file_together() {
        let args = tasks_args(Some(PathBuf::from("new.md")), Some(PathBuf::from("old.md")));

        let err = args.validate().expect_err("file+tasks_file should fail");
        assert!(err.to_string().contains("cannot be used together"));
    }

    #[test]
    fn validate_rejects_continue_on_fail_with_fail_fast() {
        let mut args = tasks_args(None, None);
        args.continue_on_fail = true;
        args.fail_fast = true;

        let err = args
            .validate()
            .expect_err("continue_on_fail+fail_fast should fail");
        assert!(err.to_string().contains("cannot be used together"));
    }

    #[test]
    fn validate_rejects_zero_max_parallel() {
        let mut args = tasks_args(None, None);
        args.max_parallel = Some(0);

        let err = args.validate().expect_err("max_parallel=0 should fail");
        assert!(err.to_string().contains("--max-parallel"));
    }

    #[test]
    fn validate_accepts_valid_args() {
        let args = tasks_args(Some(PathBuf::from("tasks.md")), None);
        args.validate().expect("valid args should pass validation");

        let args2 = tasks_args(None, Some(PathBuf::from("old.md")));
        args2
            .validate()
            .expect("deprecated tasks_file alone should pass validation");

        let args3 = tasks_args(None, None);
        args3
            .validate()
            .expect("no file args should pass validation");
    }

    #[test]
    fn effective_file_prefers_file() {
        let project_dir = PathBuf::from("/tmp/agent-loop");
        let args = tasks_args(
            Some(PathBuf::from("/tmp/new.md")),
            Some(PathBuf::from("/tmp/old.md")),
        );
        let path = args.effective_file(&project_dir);
        assert_eq!(path, PathBuf::from("/tmp/new.md"));
    }

    #[test]
    fn effective_file_uses_tasks_file_with_deprecation_path_selection() {
        let project_dir = PathBuf::from("/tmp/agent-loop");
        let args = tasks_args(None, Some(PathBuf::from("/tmp/old.md")));
        let path = args.effective_file(&project_dir);
        assert_eq!(path, PathBuf::from("/tmp/old.md"));
    }

    #[test]
    fn effective_file_defaults_to_state_tasks_md() {
        let project_dir = PathBuf::from("/tmp/agent-loop");
        let args = tasks_args(None, None);
        let path = args.effective_file(&project_dir);
        assert_eq!(
            path,
            project_dir
                .join(".agent-loop")
                .join("state")
                .join("tasks.md")
        );
    }

    #[test]
    fn is_timeout_reason_matches_timeout_phrases() {
        assert!(is_timeout_reason(Some(
            "claude timed out after 600s of inactivity"
        )));
        assert!(is_timeout_reason(Some("Idle timeout: no output for 600s")));
        assert!(!is_timeout_reason(Some("process exited with code 1")));
        assert!(!is_timeout_reason(None));
    }

    #[test]
    fn retryable_run_tasks_status_covers_max_rounds_and_timeout_error() {
        let max_rounds = loop_status(Status::MaxRounds, None);
        let timeout_error = loop_status(Status::Error, Some("codex timed out after 300s"));
        let non_timeout_error = loop_status(Status::Error, Some("spawn failed"));
        let needs_changes = loop_status(Status::NeedsChanges, None);

        assert!(is_retryable_run_tasks_status(Some(&max_rounds)));
        assert!(is_retryable_run_tasks_status(Some(&timeout_error)));
        assert!(!is_retryable_run_tasks_status(Some(&non_timeout_error)));
        assert!(!is_retryable_run_tasks_status(Some(&needs_changes)));
        assert!(!is_retryable_run_tasks_status(None));
    }

    #[test]
    fn format_status_reason_includes_reason_when_present() {
        let timeout_error = loop_status(Status::Error, Some("claude timed out after 600s"));
        assert_eq!(
            format_status_reason(Some(&timeout_error)),
            "ERROR (claude timed out after 600s)"
        );

        let max_rounds = loop_status(Status::MaxRounds, None);
        assert_eq!(format_status_reason(Some(&max_rounds)), "MAX_ROUNDS");
        assert_eq!(format_status_reason(None), "UNKNOWN");
    }

    #[test]
    fn apply_task_usage_sets_optional_fields() {
        let mut entry = TaskMetricsEntry {
            title: "Task 1".to_string(),
            task_started_at: None,
            task_ended_at: None,
            duration_ms: None,
            agent_calls: None,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cost_usd_micros: None,
        };
        apply_task_usage(
            &mut entry,
            agent::UsageSnapshot {
                agent_calls: 3,
                input_tokens: 1_200,
                output_tokens: 450,
                total_tokens: 1_650,
                cost_usd_micros: 12_345,
            },
        );

        assert_eq!(entry.agent_calls, Some(3));
        assert_eq!(entry.input_tokens, Some(1_200));
        assert_eq!(entry.output_tokens, Some(450));
        assert_eq!(entry.total_tokens, Some(1_650));
        assert_eq!(entry.cost_usd_micros, Some(12_345));
    }

    #[test]
    fn task_usage_snapshot_reads_missing_fields_as_zero() {
        let entry = TaskMetricsEntry {
            title: "Task 1".to_string(),
            task_started_at: None,
            task_ended_at: None,
            duration_ms: None,
            agent_calls: None,
            input_tokens: Some(10),
            output_tokens: None,
            total_tokens: None,
            cost_usd_micros: None,
        };

        let usage = task_usage_snapshot(&entry);
        assert_eq!(usage.agent_calls, 0);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
        assert_eq!(usage.cost_usd_micros, 0);
    }

    #[test]
    fn no_args_dispatch_uses_help_path_and_returns_zero() {
        let cli = Cli::try_parse_from(["agent-loop"]).expect("no-args parse should succeed");
        let dispatch = dispatch_from_cli(cli).expect("no-args dispatch should succeed");
        assert_eq!(dispatch, Dispatch::ShowHelp);

        let mut help_called = false;
        let exit_code = execute_dispatch(
            dispatch,
            |_| panic!("run handler should not be called"),
            |_| panic!("plan handler should not be called"),
            |_| panic!("resume handler should not be called"),
            |_| panic!("tasks handler should not be called"),
            || panic!("init handler should not be called"),
            || panic!("status handler should not be called"),
            || panic!("version handler should not be called"),
            || {
                help_called = true;
                Ok(())
            },
        )
        .expect("help dispatch should succeed");

        assert!(help_called);
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn execute_run_phases_maps_planning_only_result_to_exit_code() {
        let config = test_config();
        let baseline = HashSet::new();

        let mut planning_called = false;
        let mut decomposition_called = false;
        let mut implementation_called = false;
        let mut summary_called = false;

        let success_exit = execute_run_phases(
            &config,
            &baseline,
            true, // planning_only
            |_, po| {
                planning_called = true;
                assert!(po, "planning_only should be true");
                true
            },
            |_| {
                decomposition_called = true;
                true
            },
            |_, _| {
                implementation_called = true;
                true
            },
            |_| summary_called = true,
        );

        assert_eq!(success_exit, 0);
        assert!(planning_called);
        assert!(decomposition_called);
        assert!(!implementation_called);
        assert!(!summary_called);

        let failure_exit = execute_run_phases(
            &config,
            &baseline,
            true,
            |_, _| true,
            |_| false,
            |_, _| true,
            |_| {},
        );
        assert_eq!(failure_exit, 1);
    }

    #[test]
    fn execute_run_phases_maps_implementation_result_to_exit_code() {
        let config = test_config();
        let baseline = HashSet::new();

        let mut summary_called = false;
        let success_exit = execute_run_phases(
            &config,
            &baseline,
            false, // planning_only
            |_, _| true,
            |_| true,
            |_, _| true,
            |_| summary_called = true,
        );
        assert_eq!(success_exit, 0);
        assert!(summary_called);

        let mut summary_called_for_failure = false;
        let failure_exit = execute_run_phases(
            &config,
            &baseline,
            false,
            |_, _| true,
            |_| true,
            |_, _| false,
            |_| summary_called_for_failure = true,
        );
        assert_eq!(failure_exit, 1);
        assert!(summary_called_for_failure);
    }

    #[test]
    fn execute_run_phases_short_circuits_when_planning_fails() {
        let config = test_config();
        let baseline = HashSet::new();

        let planning_only_exit = execute_run_phases(
            &config,
            &baseline,
            true, // planning_only
            |_, _| false,
            |_| panic!("decomposition should not run when planning fails"),
            |_, _| panic!("implementation should not run when planning fails"),
            |_| panic!("summary should not run when planning fails"),
        );
        assert_eq!(planning_only_exit, 1);

        let implementation_exit = execute_run_phases(
            &config,
            &baseline,
            false, // planning_only
            |_, _| false,
            |_| panic!("decomposition should not run in implementation mode"),
            |_, _| panic!("implementation should not run when planning fails"),
            |_| panic!("summary should not run when planning fails"),
        );
        assert_eq!(implementation_exit, 1);
    }

    // --- New subcommand parsing tests ---

    #[test]
    fn parse_plan_subcommand_with_task() {
        let cli = Cli::try_parse_from(["agent-loop", "plan", "my task"])
            .expect("plan with task should parse");
        let Some(Commands::Plan(args)) = cli.command else {
            panic!("expected Commands::Plan variant");
        };
        assert_eq!(args.task.as_deref(), Some("my task"));
        assert!(args.file.is_none());
        assert!(!args.single_agent);
    }

    #[test]
    fn parse_plan_subcommand_with_file() {
        let cli = Cli::try_parse_from(["agent-loop", "plan", "--file", "plan.md"])
            .expect("plan with file should parse");
        let Some(Commands::Plan(args)) = cli.command else {
            panic!("expected Commands::Plan variant");
        };
        assert_eq!(args.file.as_deref(), Some(Path::new("plan.md")));
        assert!(args.task.is_none());
    }

    #[test]
    fn parse_resume_subcommand() {
        let cli = Cli::try_parse_from(["agent-loop", "resume"]).expect("resume should parse");
        let Some(Commands::Resume(args)) = cli.command else {
            panic!("expected Commands::Resume variant");
        };
        assert!(!args.single_agent);
    }

    #[test]
    fn parse_resume_with_single_agent() {
        let cli = Cli::try_parse_from(["agent-loop", "resume", "--single-agent"])
            .expect("resume with single-agent should parse");
        let Some(Commands::Resume(args)) = cli.command else {
            panic!("expected Commands::Resume variant");
        };
        assert!(args.single_agent);
    }

    #[test]
    fn parse_tasks_subcommand() {
        let cli = Cli::try_parse_from(["agent-loop", "tasks"]).expect("tasks should parse");
        assert!(matches!(cli.command, Some(Commands::Tasks(_))));
    }

    #[test]
    fn parse_tasks_with_file_flag() {
        let cli = Cli::try_parse_from(["agent-loop", "tasks", "--file", "custom.md"])
            .expect("tasks with file should parse");
        let Some(Commands::Tasks(args)) = cli.command else {
            panic!("expected Commands::Tasks variant");
        };
        assert_eq!(args.file.as_deref(), Some(Path::new("custom.md")));
    }

    #[test]
    fn parse_run_tasks_deprecated_maps_to_correct_variant() {
        let cli = Cli::try_parse_from(["agent-loop", "run-tasks"]).expect("run-tasks should parse");
        assert!(matches!(cli.command, Some(Commands::RunTasksDeprecated(_))));
    }

    #[test]
    fn normalize_argv_recognizes_new_subcommands() {
        for subcmd in &["plan", "resume", "tasks"] {
            let normalized = normalize_argv(os_argv(&["agent-loop", subcmd]));
            assert_eq!(
                normalized,
                os_argv(&["agent-loop", subcmd]),
                "{subcmd} should not be rewritten"
            );
        }
    }

    #[test]
    fn hidden_flags_still_parse_on_run() {
        let resume_cli = Cli::try_parse_from(["agent-loop", "run", "--resume"])
            .expect("run --resume should parse");
        let Some(Commands::Run(resume_args)) = resume_cli.command else {
            panic!("expected Commands::Run variant");
        };
        assert!(resume_args.resume);

        let planning_cli = Cli::try_parse_from(["agent-loop", "run", "--planning-only", "task"])
            .expect("run --planning-only should parse");
        let Some(Commands::Run(planning_args)) = planning_cli.command else {
            panic!("expected Commands::Run variant");
        };
        assert!(planning_args.planning_only);
        assert_eq!(planning_args.task.as_deref(), Some("task"));
    }

    #[test]
    fn dispatch_plan_maps_to_plan() {
        let cli =
            Cli::try_parse_from(["agent-loop", "plan", "my task"]).expect("plan should parse");
        let dispatch = dispatch_from_cli(cli).expect("plan dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Plan(PlanArgs {
                task: Some("my task".to_string()),
                file: None,
                single_agent: false,
            })
        );
    }

    #[test]
    fn dispatch_resume_maps_to_resume() {
        let cli = Cli::try_parse_from(["agent-loop", "resume"]).expect("resume should parse");
        let dispatch = dispatch_from_cli(cli).expect("resume dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Resume(ResumeDispatch {
                args: ResumeArgs {
                    single_agent: false,
                },
                planning_only_legacy_override: None,
                max_rounds_override: None,
                force_implementation: false,
            })
        );
    }

    #[test]
    fn dispatch_tasks_maps_to_tasks() {
        let cli = Cli::try_parse_from(["agent-loop", "tasks", "--file", "f.md"])
            .expect("tasks should parse");
        let dispatch = dispatch_from_cli(cli).expect("tasks dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Tasks(TasksArgs {
                file: Some(PathBuf::from("f.md")),
                tasks_file: None,
                max_retries: 2,
                round_step: 2,
                single_agent: false,
                continue_on_fail: false,
                fail_fast: false,
                max_parallel: None,
            })
        );
    }

    #[test]
    fn dispatch_run_tasks_deprecated_maps_to_tasks() {
        let cli = Cli::try_parse_from(["agent-loop", "run-tasks", "--file", "f.md"])
            .expect("run-tasks should parse");
        let dispatch = dispatch_from_cli(cli).expect("run-tasks dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Tasks(TasksArgs {
                file: Some(PathBuf::from("f.md")),
                tasks_file: None,
                max_retries: 2,
                round_step: 2,
                single_agent: false,
                continue_on_fail: false,
                fail_fast: false,
                max_parallel: None,
            })
        );
    }

    #[test]
    fn parse_tasks_with_deprecated_tasks_file_flag() {
        let cli = Cli::try_parse_from(["agent-loop", "tasks", "--tasks-file", "old.md"])
            .expect("tasks --tasks-file should parse");
        let Some(Commands::Tasks(args)) = cli.command else {
            panic!("expected Commands::Tasks variant");
        };
        assert!(args.file.is_none());
        assert_eq!(args.tasks_file.as_deref(), Some(Path::new("old.md")));
    }

    #[test]
    fn parse_run_tasks_deprecated_with_tasks_file_flag() {
        let cli = Cli::try_parse_from(["agent-loop", "run-tasks", "--tasks-file", "old.md"])
            .expect("run-tasks --tasks-file should parse");
        let Some(Commands::RunTasksDeprecated(args)) = cli.command else {
            panic!("expected Commands::RunTasksDeprecated variant");
        };
        assert!(args.file.is_none());
        assert_eq!(args.tasks_file.as_deref(), Some(Path::new("old.md")));
    }

    #[test]
    fn dispatch_tasks_file_wins_over_deprecated_tasks_file() {
        let cli = Cli::try_parse_from([
            "agent-loop",
            "tasks",
            "--file",
            "new.md",
            "--tasks-file",
            "old.md",
        ])
        .expect("tasks with both flags should parse");
        let dispatch = dispatch_from_cli(cli).expect("tasks dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Tasks(TasksArgs {
                file: Some(PathBuf::from("new.md")),
                tasks_file: Some(PathBuf::from("old.md")),
                max_retries: 2,
                round_step: 2,
                single_agent: false,
                continue_on_fail: false,
                fail_fast: false,
                max_parallel: None,
            })
        );
    }

    // --- Dispatch rewriting tests for legacy flag combinations ---

    #[test]
    fn dispatch_run_resume_rewrites_to_resume() {
        let cli = Cli::try_parse_from(["agent-loop", "run", "--resume"])
            .expect("run --resume should parse");
        let dispatch = dispatch_from_cli(cli).expect("run --resume dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Resume(ResumeDispatch {
                args: ResumeArgs {
                    single_agent: false,
                },
                planning_only_legacy_override: None,
                max_rounds_override: None,
                force_implementation: false,
            })
        );
    }

    #[test]
    fn dispatch_run_planning_only_rewrites_to_plan() {
        let cli = Cli::try_parse_from(["agent-loop", "run", "--planning-only", "task"])
            .expect("run --planning-only should parse");
        let dispatch = dispatch_from_cli(cli).expect("run --planning-only dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Plan(PlanArgs {
                task: Some("task".to_string()),
                file: None,
                single_agent: false,
            })
        );
    }

    #[test]
    fn dispatch_run_planning_only_resume_rewrites_to_resume_with_override() {
        let cli = Cli::try_parse_from(["agent-loop", "run", "--planning-only", "--resume"])
            .expect("run --planning-only --resume should parse");
        let dispatch =
            dispatch_from_cli(cli).expect("run --planning-only --resume dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Resume(ResumeDispatch {
                args: ResumeArgs {
                    single_agent: false,
                },
                planning_only_legacy_override: Some(true),
                max_rounds_override: None,
                force_implementation: false,
            })
        );
    }

    #[test]
    fn dispatch_run_resume_with_task_returns_error() {
        let cli = Cli::try_parse_from(["agent-loop", "run", "--resume", "task"])
            .expect("run --resume task should parse");
        let err = dispatch_from_cli(cli).expect_err("run --resume with task should error");
        assert!(err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn dispatch_run_resume_with_file_returns_error() {
        let cli = Cli::try_parse_from(["agent-loop", "run", "--resume", "--file", "f.md"])
            .expect("run --resume --file should parse");
        let err = dispatch_from_cli(cli).expect_err("run --resume with file should error");
        assert!(err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn dispatch_plain_run_maps_to_run() {
        let cli =
            Cli::try_parse_from(["agent-loop", "run", "task"]).expect("run task should parse");
        let dispatch = dispatch_from_cli(cli).expect("plain run dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Run(RunArgs {
                task: Some("task".to_string()),
                file: None,
                resume: false,
                planning_only: false,
                single_agent: false,
            })
        );
    }

    // --- resolve_task_for_plan tests ---

    fn plan_args(task: Option<&str>, file: Option<PathBuf>) -> PlanArgs {
        PlanArgs {
            task: task.map(ToOwned::to_owned),
            file,
            single_agent: false,
        }
    }

    #[test]
    fn resolve_task_for_plan_prefers_file_over_positional_text() {
        let task_file = unique_temp_file("agent_loop_plan_task_precedence");
        write_temp_file(&task_file, "plan from file");

        let args = plan_args(Some("plan from positional"), Some(task_file.clone()));
        let resolved = resolve_task_for_plan(&args).expect("task should resolve from file");
        assert_eq!(resolved, "plan from file");

        let _ = fs::remove_file(task_file);
    }

    #[test]
    fn resolve_task_for_plan_uses_positional_task_when_file_is_missing() {
        let args = plan_args(Some("plan from positional"), None);
        let resolved = resolve_task_for_plan(&args).expect("positional task should be used");
        assert_eq!(resolved, "plan from positional");
    }

    #[test]
    fn resolve_task_for_plan_rejects_missing_or_empty_tasks() {
        let missing_task_error =
            resolve_task_for_plan(&plan_args(None, None)).expect_err("missing task should fail");
        assert!(missing_task_error.to_string().contains("Task is required"));

        let empty_positional_error = resolve_task_for_plan(&plan_args(Some("   "), None))
            .expect_err("empty task should fail");
        assert!(
            empty_positional_error
                .to_string()
                .contains("cannot be empty")
        );

        let task_file = unique_temp_file("agent_loop_plan_task_empty_file");
        write_temp_file(&task_file, "  \n\t");

        let empty_file_error = resolve_task_for_plan(&plan_args(None, Some(task_file.clone())))
            .expect_err("empty file task should fail");
        assert!(empty_file_error.to_string().contains("is empty"));

        let _ = fs::remove_file(task_file);
    }

    #[test]
    fn resolve_task_for_plan_rejects_unreadable_file() {
        let bad_path = PathBuf::from("/nonexistent/path/to/plan.md");
        let err = resolve_task_for_plan(&plan_args(None, Some(bad_path)))
            .expect_err("unreadable file should fail");
        assert!(err.to_string().contains("Failed to read task file"));
    }

    // --- plan_command integration-style phase test ---

    #[test]
    fn plan_command_inner_runs_planning_and_decomposition_only() {
        use crate::test_support::env_lock;
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = env_lock();
        clear_config_env();
        let project = crate::test_support::TestProject::builder("plan_cmd_phases").build();
        let project_root = project.root.clone();

        let planning_called = AtomicBool::new(false);
        let decomposition_called = AtomicBool::new(false);

        let args = plan_args(Some("plan this task"), None);

        let exit_code = plan_command_inner(
            args,
            move || Ok(project_root),
            |_config, _planning_only| {
                planning_called.store(true, Ordering::SeqCst);
                true
            },
            |_config| {
                decomposition_called.store(true, Ordering::SeqCst);
                true
            },
        )
        .expect("plan_command_inner should succeed");

        assert_eq!(exit_code, 0);
        assert!(
            planning_called.load(Ordering::SeqCst),
            "planning phase must be called"
        );
        assert!(
            decomposition_called.load(Ordering::SeqCst),
            "decomposition phase must be called"
        );

        // Verify workflow marker is Plan.
        let workflow = crate::state::read_workflow(&project.config);
        assert_eq!(workflow, Some(crate::state::WorkflowKind::Plan));

        // Verify task was persisted.
        let task_content = crate::state::read_state_file("task.md", &project.config);
        assert_eq!(task_content, "plan this task");
    }

    #[test]
    fn plan_command_inner_skips_decomposition_when_planning_fails() {
        use crate::test_support::env_lock;

        let _guard = env_lock();
        clear_config_env();
        let project = crate::test_support::TestProject::builder("plan_cmd_planning_fail").build();
        let project_root = project.root.clone();

        let args = plan_args(Some("fail planning"), None);

        let exit_code = plan_command_inner(
            args,
            move || Ok(project_root),
            |_config, _po| false, // planning fails
            |_config| panic!("decomposition must not be called when planning fails"),
        )
        .expect("plan_command_inner should return Ok even on planning failure");

        assert_eq!(exit_code, 1);
    }

    #[test]
    fn plan_command_inner_returns_failure_exit_code_on_decomposition_failure() {
        use crate::test_support::env_lock;

        let _guard = env_lock();
        clear_config_env();
        let project = crate::test_support::TestProject::builder("plan_cmd_decomp_fail").build();
        let project_root = project.root.clone();

        let args = plan_args(Some("decomposition fails"), None);

        let exit_code = plan_command_inner(
            args,
            move || Ok(project_root),
            |_config, _po| true,  // planning succeeds
            |_config| false, // decomposition fails
        )
        .expect("plan_command_inner should return Ok even on decomposition failure");

        assert_eq!(exit_code, 1);
    }

    // --- Dispatch routing test for Dispatch::Plan ---

    #[test]
    fn execute_dispatch_routes_plan_to_plan_handler() {
        let dispatch = Dispatch::Plan(PlanArgs {
            task: Some("my plan task".to_string()),
            file: None,
            single_agent: false,
        });

        let exit_code = execute_dispatch(
            dispatch,
            |_| panic!("run handler should not be called"),
            |args| {
                assert_eq!(args.task.as_deref(), Some("my plan task"));
                Ok(0)
            },
            |_| panic!("resume handler should not be called"),
            |_| panic!("tasks handler should not be called"),
            || panic!("init handler should not be called"),
            || panic!("status handler should not be called"),
            || panic!("version handler should not be called"),
            || panic!("help handler should not be called"),
        )
        .expect("plan dispatch should succeed");

        assert_eq!(exit_code, 0);
    }

    // --- run() wiring: end-to-end parse → dispatch → handler chain ---

    /// Verify the full parse → dispatch → execute_dispatch chain routes
    /// `plan <task>` to the plan handler (same wiring as `run()`).
    #[test]
    fn run_wiring_routes_plan_subcommand_to_plan_handler() {
        let parsed = parse_cli_from(os_argv(&["agent-loop", "plan", "test task"]))
            .expect("plan subcommand should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("plan subcommand should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("plan dispatch should succeed");

        // Verify the dispatch variant is Plan
        assert!(
            matches!(&dispatch, Dispatch::Plan(args) if args.task.as_deref() == Some("test task")),
            "dispatch should be Plan with correct task"
        );

        // Execute through the same execute_dispatch that run() uses,
        // verifying plan_handler is called (not run/resume/tasks).
        let mut plan_handler_called = false;
        let exit_code = execute_dispatch(
            dispatch,
            |_| panic!("run handler should not be called for plan subcommand"),
            |args| {
                plan_handler_called = true;
                assert_eq!(args.task.as_deref(), Some("test task"));
                Ok(0)
            },
            |_| panic!("resume handler should not be called for plan subcommand"),
            |_| panic!("tasks handler should not be called for plan subcommand"),
            || panic!("init handler should not be called for plan subcommand"),
            || panic!("status handler should not be called for plan subcommand"),
            || panic!("version handler should not be called for plan subcommand"),
            || panic!("help handler should not be called for plan subcommand"),
        )
        .expect("plan dispatch should succeed");

        assert!(plan_handler_called, "plan handler must be called");
        assert_eq!(exit_code, 0);
    }

    /// Verify `run --planning-only <task>` is rewritten to Plan dispatch,
    /// exercising the same chain as run().
    #[test]
    fn run_wiring_routes_legacy_planning_only_to_plan_handler() {
        let parsed = parse_cli_from(os_argv(&[
            "agent-loop",
            "run",
            "--planning-only",
            "legacy task",
        ]))
        .expect("run --planning-only should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("dispatch should succeed");

        assert!(
            matches!(&dispatch, Dispatch::Plan(args) if args.task.as_deref() == Some("legacy task")),
            "legacy --planning-only should rewrite to Dispatch::Plan"
        );
    }

    // --- run_command_with_max_rounds guardrails ---

    #[test]
    fn run_command_with_max_rounds_rejects_resume_true() {
        let args = RunArgs {
            task: None,
            file: None,
            resume: true,
            planning_only: false,
            single_agent: false,
        };
        let err =
            run_command_with_max_rounds(args, None).expect_err("resume=true should be rejected");
        assert!(
            err.to_string()
                .contains("must not be called with resume=true"),
            "error should mention resume rejection, got: {err}"
        );
    }

    #[test]
    fn run_command_with_max_rounds_rejects_planning_only_true() {
        let args = RunArgs {
            task: None,
            file: None,
            resume: false,
            planning_only: true,
            single_agent: false,
        };
        let err = run_command_with_max_rounds(args, None)
            .expect_err("planning_only=true should be rejected");
        assert!(
            err.to_string()
                .contains("must not be called with planning_only=true"),
            "error should mention planning_only rejection, got: {err}"
        );
    }

    // --- resume_command: pre-config state helpers ---

    #[test]
    fn ensure_resume_state_dir_exists_rejects_missing_dir() {
        let root = unique_temp_file("resume_state_missing");
        let state_dir = root.join(".agent-loop").join("state");
        let err =
            ensure_resume_state_dir_exists(&state_dir).expect_err("missing state dir should fail");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn ensure_resume_state_dir_exists_rejects_missing_status_json() {
        let root = unique_temp_file("resume_state_no_status");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let err = ensure_resume_state_dir_exists(&state_dir)
            .expect_err("missing status.json should fail");
        assert!(err.to_string().contains("status.json"));
        assert!(err.to_string().contains("missing"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ensure_resume_state_dir_exists_accepts_valid_state() {
        let root = unique_temp_file("resume_state_valid");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("status.json"), "{}").unwrap();
        ensure_resume_state_dir_exists(&state_dir).expect("valid state dir should pass");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_resume_task_from_state_dir_reads_task() {
        let root = unique_temp_file("resume_task_read");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("task.md"), "my task").unwrap();
        let task = read_resume_task_from_state_dir(&state_dir).expect("task should be readable");
        assert_eq!(task, "my task");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_resume_task_from_state_dir_rejects_empty() {
        let root = unique_temp_file("resume_task_empty");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("task.md"), "  \n\t").unwrap();
        let err = read_resume_task_from_state_dir(&state_dir).expect_err("empty task should fail");
        assert!(err.to_string().contains("empty"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_workflow_marker_from_state_dir_reads_plan() {
        let root = unique_temp_file("resume_workflow_plan");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("workflow.txt"), "plan\n").unwrap();
        assert_eq!(
            read_workflow_marker_from_state_dir(&state_dir),
            Some(state::WorkflowKind::Plan)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_workflow_marker_from_state_dir_reads_run() {
        let root = unique_temp_file("resume_workflow_run");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("workflow.txt"), "run\n").unwrap();
        assert_eq!(
            read_workflow_marker_from_state_dir(&state_dir),
            Some(state::WorkflowKind::Run)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_workflow_marker_from_state_dir_returns_none_when_missing() {
        let root = unique_temp_file("resume_workflow_missing");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        assert_eq!(read_workflow_marker_from_state_dir(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    // --- resume_command: artifact inference ---

    #[test]
    fn infer_workflow_from_artifacts_detects_plan_from_task_headings() {
        let root = unique_temp_file("resume_infer_plan");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("tasks.md"),
            "## Task 1: Build parser\nsome content\n### Task 2: Add tests\n",
        )
        .unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"PENDING","round":0}"#,
        )
        .unwrap();
        assert_eq!(
            infer_workflow_from_artifacts(&state_dir),
            Some(state::WorkflowKind::Plan)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_from_artifacts_detects_run_from_status() {
        let root = unique_temp_file("resume_infer_run_status");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"IMPLEMENTING","round":2}"#,
        )
        .unwrap();
        assert_eq!(
            infer_workflow_from_artifacts(&state_dir),
            Some(state::WorkflowKind::Run)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_from_artifacts_detects_run_from_changes() {
        let root = unique_temp_file("resume_infer_run_changes");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("changes.md"), "Some implementation changes").unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"PENDING","round":0}"#,
        )
        .unwrap();
        assert_eq!(
            infer_workflow_from_artifacts(&state_dir),
            Some(state::WorkflowKind::Run)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_from_artifacts_returns_none_on_conflict() {
        let root = unique_temp_file("resume_infer_conflict");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // Both plan signal (task headings) and run signal (impl status)
        fs::write(state_dir.join("tasks.md"), "### Task 1: Build parser\n").unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"IMPLEMENTING","round":2}"#,
        )
        .unwrap();
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_from_artifacts_returns_none_when_empty() {
        let root = unique_temp_file("resume_infer_empty");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"PENDING","round":0}"#,
        )
        .unwrap();
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_needs_revision_is_not_run_signal() {
        // NEEDS_REVISION can appear in planning/decomposition flows, so it
        // should NOT be treated as an implementation signal.
        let root = unique_temp_file("resume_infer_needs_revision");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"NEEDS_REVISION","round":2}"#,
        )
        .unwrap();
        // No task headings, no impl artifacts — should be None (not Run).
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_needs_revision_with_plan_headings_infers_plan() {
        // NEEDS_REVISION with task headings should infer Plan, not conflict.
        let root = unique_temp_file("resume_infer_needs_rev_plan");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("tasks.md"),
            "### Task 1: Build parser\nsome content\n",
        )
        .unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"NEEDS_REVISION","round":2}"#,
        )
        .unwrap();
        assert_eq!(
            infer_workflow_from_artifacts(&state_dir),
            Some(state::WorkflowKind::Plan)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_approved_with_plan_headings_is_ambiguous() {
        // APPROVED with task headings but no implementation artifacts is
        // ambiguous: could be a completed plan workflow OR a run workflow
        // interrupted between decomposition and implementation. Return None.
        let root = unique_temp_file("resume_infer_approved_ambiguous");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("tasks.md"),
            "### Task 1: Build parser\nsome content\n",
        )
        .unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"APPROVED","round":1}"#,
        )
        .unwrap();
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_consensus_with_plan_headings_is_ambiguous() {
        // CONSENSUS with task headings but no implementation artifacts is
        // ambiguous: could be a completed plan workflow OR a run workflow
        // interrupted between decomposition and implementation. Return None.
        let root = unique_temp_file("resume_infer_consensus_ambiguous");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("tasks.md"),
            "### Task 1: Build parser\nsome content\n",
        )
        .unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"CONSENSUS","round":3}"#,
        )
        .unwrap();
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_disputed_without_task_headings_does_not_infer_run() {
        // DISPUTED can appear in planning/decomposition flows (e.g. stale
        // timestamp fallback during decomposition consensus). Without task
        // headings or implementation artifacts, it should NOT infer Run.
        let root = unique_temp_file("resume_infer_disputed_no_headings");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"DISPUTED","round":2}"#,
        )
        .unwrap();
        // No task headings, no impl artifacts — should be None (not Run).
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_disputed_with_plan_headings_infers_plan() {
        // DISPUTED with task headings and no implementation artifacts should
        // infer Plan (not conflict).
        let root = unique_temp_file("resume_infer_disputed_plan");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("tasks.md"),
            "### Task 1: Build parser\nsome content\n",
        )
        .unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"DISPUTED","round":2}"#,
        )
        .unwrap();
        assert_eq!(
            infer_workflow_from_artifacts(&state_dir),
            Some(state::WorkflowKind::Plan)
        );
        let _ = fs::remove_dir_all(root);
    }

    // --- resume_command: ambiguous decomposition-terminal inference ---

    #[test]
    fn infer_workflow_consensus_without_headings_returns_none() {
        // CONSENSUS alone (no task headings, no impl artifacts) is not enough
        // to infer any workflow.
        let root = unique_temp_file("resume_infer_consensus_alone");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"CONSENSUS","round":3}"#,
        )
        .unwrap();
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_approved_without_headings_returns_none() {
        // APPROVED alone (no task headings, no impl artifacts) is not enough
        // to infer any workflow.
        let root = unique_temp_file("resume_infer_approved_alone");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"APPROVED","round":1}"#,
        )
        .unwrap();
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_consensus_with_headings_and_impl_artifacts_returns_none() {
        // CONSENSUS + task headings + implementation artifacts = conflicting
        // signals (plan_signal && run_signal), should return None.
        let root = unique_temp_file("resume_infer_consensus_conflict");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("tasks.md"),
            "### Task 1: Build parser\nsome content\n",
        )
        .unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"CONSENSUS","round":3}"#,
        )
        .unwrap();
        fs::write(state_dir.join("changes.md"), "Implementation changes").unwrap();
        assert_eq!(infer_workflow_from_artifacts(&state_dir), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_planning_with_headings_infers_plan() {
        // PLANNING status with task headings is safe to infer as Plan — the
        // session is still actively decomposing, not in a decomposition-terminal
        // state. This distinguishes it from the CONSENSUS/APPROVED ambiguity.
        let root = unique_temp_file("resume_infer_planning_headings");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("tasks.md"),
            "## Task 1: Build parser\nsome content\n",
        )
        .unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"PLANNING","round":1}"#,
        )
        .unwrap();
        assert_eq!(
            infer_workflow_from_artifacts(&state_dir),
            Some(state::WorkflowKind::Plan)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_workflow_pending_with_headings_infers_plan() {
        // PENDING status with task headings is safe to infer as Plan — the
        // session hasn't started yet, so no risk of misclassifying a run
        // workflow that completed decomposition.
        let root = unique_temp_file("resume_infer_pending_headings");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("tasks.md"),
            "## Task 1: Build parser\nsome content\n### Task 2: Add tests\n",
        )
        .unwrap();
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"PENDING","round":0}"#,
        )
        .unwrap();
        assert_eq!(
            infer_workflow_from_artifacts(&state_dir),
            Some(state::WorkflowKind::Plan)
        );
        let _ = fs::remove_dir_all(root);
    }

    // --- resume_command: task read error handling ---

    #[test]
    fn read_resume_task_from_state_dir_missing_file_gives_read_error() {
        // Missing task.md should report a read failure, not "empty".
        let root = unique_temp_file("resume_task_missing_file");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // Do NOT create task.md
        let err =
            read_resume_task_from_state_dir(&state_dir).expect_err("missing task.md should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to read"),
            "error should mention read failure, got: {msg}"
        );
        let _ = fs::remove_dir_all(root);
    }

    // --- resume_command: workflow resolution precedence ---

    fn make_resume_dispatch(single_agent: bool, legacy_override: Option<bool>) -> ResumeDispatch {
        ResumeDispatch {
            args: ResumeArgs { single_agent },
            planning_only_legacy_override: legacy_override,
            max_rounds_override: None,
            force_implementation: false,
        }
    }

    #[test]
    fn resolve_workflow_legacy_override_beats_persisted_marker() {
        let root = unique_temp_file("resume_resolve_override");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // Persisted marker says "run"
        fs::write(state_dir.join("workflow.txt"), "run\n").unwrap();
        // Legacy override says planning_only = true => Plan
        let dispatch = make_resume_dispatch(false, Some(true));
        let workflow =
            resolve_workflow_for_resume(&dispatch, &state_dir).expect("should resolve");
        assert_eq!(workflow, state::WorkflowKind::Plan);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workflow_persisted_marker_resolves() {
        let root = unique_temp_file("resume_resolve_marker");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // Persisted marker says "plan"
        fs::write(state_dir.join("workflow.txt"), "plan\n").unwrap();
        let dispatch = make_resume_dispatch(false, None);
        let workflow =
            resolve_workflow_for_resume(&dispatch, &state_dir).expect("should resolve");
        assert_eq!(workflow, state::WorkflowKind::Plan);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workflow_returns_error_when_unresolvable() {
        use crate::test_support::env_lock;

        let _guard = env_lock();
        clear_config_env();

        let root = unique_temp_file("resume_resolve_error");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // No workflow.txt, no artifacts, no env
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"PENDING","round":0}"#,
        )
        .unwrap();
        let dispatch = make_resume_dispatch(false, None);
        let err = resolve_workflow_for_resume(&dispatch, &state_dir)
            .expect_err("should fail when unresolvable");
        let msg = err.to_string();
        assert!(msg.contains("unable to determine"), "got: {msg}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_workflow_artifact_inference_resolves() {
        let root = unique_temp_file("resume_resolve_artifact");
        let state_dir = root.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        // No workflow.txt, but implementation artifacts (run signal)
        fs::write(
            state_dir.join("status.json"),
            r#"{"status":"IMPLEMENTING","round":2}"#,
        )
        .unwrap();
        let dispatch = make_resume_dispatch(false, None);
        let workflow = resolve_workflow_for_resume(&dispatch, &state_dir)
            .expect("should resolve from artifacts");
        assert_eq!(workflow, state::WorkflowKind::Run);
        let _ = fs::remove_dir_all(root);
    }

    // --- resume_command_inner: phase routing ---

    #[test]
    fn resume_command_inner_plan_runs_decomposition_only() {
        use crate::test_support::env_lock;
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = env_lock();
        clear_config_env();

        let project = crate::test_support::TestProject::builder("resume_plan_phases").build();
        let project_root = project.root.clone();

        // Set up state for resume: status.json + task.md + workflow.txt
        fs::create_dir_all(&project.config.state_dir).unwrap();
        crate::state::write_state_file("task.md", "plan task", &project.config).unwrap();
        fs::write(project.config.state_dir.join("status.json"), r#"{"status":"PLANNING","round":1,"implementer":"claude","reviewer":"codex","mode":"dual-agent","lastRunTask":"plan task","timestamp":"2026-02-15T15:09:44.850Z"}"#).unwrap();
        fs::write(project.config.state_dir.join("workflow.txt"), "plan\n").unwrap();

        let decomposition_called = AtomicBool::new(false);
        let implementation_called = AtomicBool::new(false);
        let summary_called = AtomicBool::new(false);

        let dispatch = make_resume_dispatch(false, None);

        let exit_code = resume_command_inner(
            dispatch,
            move || Ok(project_root),
            |_config| {
                decomposition_called.store(true, Ordering::SeqCst);
                true
            },
            |_config, _baseline| {
                implementation_called.store(true, Ordering::SeqCst);
                true
            },
            |_config| {
                summary_called.store(true, Ordering::SeqCst);
            },
        )
        .expect("resume_command_inner should succeed");

        assert_eq!(exit_code, 0);
        assert!(
            decomposition_called.load(Ordering::SeqCst),
            "decomposition phase must be called for Plan workflow"
        );
        assert!(
            !implementation_called.load(Ordering::SeqCst),
            "implementation must not be called for Plan workflow"
        );
        assert!(
            !summary_called.load(Ordering::SeqCst),
            "summary must not be called for Plan workflow"
        );
    }

    #[test]
    fn resume_command_inner_run_runs_implementation_and_summary() {
        use crate::test_support::env_lock;
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = env_lock();
        clear_config_env();

        let project = crate::test_support::TestProject::builder("resume_run_phases").build();
        let project_root = project.root.clone();

        // Set up state for resume: status.json + task.md + workflow.txt
        fs::create_dir_all(&project.config.state_dir).unwrap();
        crate::state::write_state_file("task.md", "run task", &project.config).unwrap();
        fs::write(project.config.state_dir.join("status.json"), r#"{"status":"IMPLEMENTING","round":2,"implementer":"claude","reviewer":"codex","mode":"dual-agent","lastRunTask":"run task","timestamp":"2026-02-15T15:09:44.850Z"}"#).unwrap();
        fs::write(project.config.state_dir.join("workflow.txt"), "run\n").unwrap();

        let decomposition_called = AtomicBool::new(false);
        let implementation_called = AtomicBool::new(false);
        let summary_called = AtomicBool::new(false);

        let dispatch = make_resume_dispatch(false, None);

        let exit_code = resume_command_inner(
            dispatch,
            move || Ok(project_root),
            |_config| {
                decomposition_called.store(true, Ordering::SeqCst);
                true
            },
            |_config, _baseline| {
                implementation_called.store(true, Ordering::SeqCst);
                true
            },
            |_config| {
                summary_called.store(true, Ordering::SeqCst);
            },
        )
        .expect("resume_command_inner should succeed");

        assert_eq!(exit_code, 0);
        assert!(
            !decomposition_called.load(Ordering::SeqCst),
            "decomposition must not be called for Run workflow"
        );
        assert!(
            implementation_called.load(Ordering::SeqCst),
            "implementation phase must be called for Run workflow"
        );
        assert!(
            summary_called.load(Ordering::SeqCst),
            "summary must be called for Run workflow"
        );
    }

    // ── Task 8: help text and environment_help tests ──────────────────

    #[test]
    fn environment_help_mentions_new_subcommands() {
        let help = environment_help();
        assert!(
            help.contains("agent-loop plan"),
            "environment_help should mention 'agent-loop plan'"
        );
        assert!(
            help.contains("agent-loop resume"),
            "environment_help should mention 'agent-loop resume'"
        );
        assert!(
            help.contains("agent-loop tasks"),
            "environment_help should mention 'agent-loop tasks'"
        );
    }

    #[test]
    fn environment_help_mentions_deprecated_flags() {
        let help = environment_help();
        assert!(
            help.contains("--planning-only"),
            "environment_help should mention deprecated --planning-only flag"
        );
        assert!(
            help.contains("--resume"),
            "environment_help should mention deprecated --resume flag"
        );
        assert!(
            help.contains("Deprecated"),
            "environment_help should label deprecated flags"
        );
    }

    #[test]
    fn environment_help_does_not_list_run_tasks_as_primary() {
        let help = environment_help();
        // The primary commands section should not list run-tasks
        let primary_section = help
            .split("Deprecated flags")
            .next()
            .expect("should have a section before Deprecated flags");
        assert!(
            !primary_section.contains("run-tasks"),
            "run-tasks should not appear in the primary commands section"
        );
    }

    #[test]
    fn help_output_shows_subcommand_descriptions() {
        let mut buf = Vec::new();
        Cli::command().write_long_help(&mut buf).unwrap();
        let help_text = String::from_utf8(buf).unwrap();

        assert!(
            help_text.contains("plan") && help_text.contains("Plan and decompose"),
            "help should show 'plan' subcommand with description"
        );
        assert!(
            help_text.contains("resume") && help_text.contains("Resume an interrupted"),
            "help should show 'resume' subcommand with description"
        );
        assert!(
            help_text.contains("tasks") && help_text.contains("Execute all tasks"),
            "help should show 'tasks' subcommand with description"
        );
    }

    #[test]
    fn help_output_hides_run_tasks() {
        let mut buf = Vec::new();
        Cli::command().write_long_help(&mut buf).unwrap();
        let help_text = String::from_utf8(buf).unwrap();

        assert!(
            !help_text.contains("run-tasks"),
            "help output should not show hidden 'run-tasks' subcommand"
        );
    }

    // ── Task 9: Comprehensive backward-compatibility regression tests ──

    /// Full chain: `run --planning-only --resume` → parse → dispatch → resume handler
    /// with `planning_only_legacy_override = Some(true)`.
    #[test]
    fn regression_legacy_planning_only_resume_full_chain() {
        let parsed = parse_cli_from(os_argv(&[
            "agent-loop",
            "run",
            "--planning-only",
            "--resume",
        ]))
        .expect("run --planning-only --resume should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("dispatch should succeed");

        // Verify dispatch variant
        assert_eq!(
            dispatch,
            Dispatch::Resume(ResumeDispatch {
                args: ResumeArgs {
                    single_agent: false,
                },
                planning_only_legacy_override: Some(true),
                max_rounds_override: None,
                force_implementation: false,
            })
        );

        // Verify routing through execute_dispatch
        let mut resume_called = false;
        let exit_code = execute_dispatch(
            dispatch,
            |_| panic!("run handler must not be called"),
            |_| panic!("plan handler must not be called"),
            |rd| {
                resume_called = true;
                assert_eq!(rd.planning_only_legacy_override, Some(true));
                Ok(0)
            },
            |_| panic!("tasks handler must not be called"),
            || panic!("init handler must not be called"),
            || panic!("status handler must not be called"),
            || panic!("version handler must not be called"),
            || panic!("help handler must not be called"),
        )
        .expect("dispatch should succeed");

        assert!(resume_called, "resume handler must be called");
        assert_eq!(exit_code, 0);
    }

    /// Full chain: `run-tasks --tasks-file old.md` → dispatch → tasks handler.
    /// Verifies the deprecated `run-tasks` subcommand continues to work.
    #[test]
    fn regression_run_tasks_deprecated_with_tasks_file_full_chain() {
        let parsed = parse_cli_from(os_argv(&[
            "agent-loop",
            "run-tasks",
            "--tasks-file",
            "old.md",
        ]))
        .expect("run-tasks --tasks-file should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("dispatch should succeed");

        // Verify dispatch variant is Tasks (not RunTasksDeprecated)
        assert!(
            matches!(&dispatch, Dispatch::Tasks(args) if args.tasks_file.as_deref() == Some(Path::new("old.md"))),
            "run-tasks should rewrite to Dispatch::Tasks with tasks_file"
        );

        // Verify routing
        let mut tasks_called = false;
        let exit_code = execute_dispatch(
            dispatch,
            |_| panic!("run handler must not be called"),
            |_| panic!("plan handler must not be called"),
            |_| panic!("resume handler must not be called"),
            |args| {
                tasks_called = true;
                assert_eq!(args.tasks_file.as_deref(), Some(Path::new("old.md")));
                Ok(0)
            },
            || panic!("init handler must not be called"),
            || panic!("status handler must not be called"),
            || panic!("version handler must not be called"),
            || panic!("help handler must not be called"),
        )
        .expect("dispatch should succeed");

        assert!(tasks_called, "tasks handler must be called");
        assert_eq!(exit_code, 0);
    }

    /// Full chain: `tasks --file new.md --tasks-file old.md` parses, dispatches,
    /// but validate() rejects the combination.
    #[test]
    fn regression_tasks_file_and_tasks_file_together_validation_error() {
        let parsed = parse_cli_from(os_argv(&[
            "agent-loop",
            "tasks",
            "--file",
            "new.md",
            "--tasks-file",
            "old.md",
        ]))
        .expect("tasks with both flags should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("dispatch should succeed");

        // Dispatch succeeds (both flags are valid at parse time)
        let Dispatch::Tasks(ref args) = dispatch else {
            panic!("dispatch should be Tasks");
        };

        // But validate() rejects the combination
        let err = args
            .validate()
            .expect_err("file+tasks_file should fail validation");
        assert!(err.to_string().contains("cannot be used together"));
    }

    /// Full chain: `run --resume "task"` still returns a validation error.
    #[test]
    fn regression_run_resume_with_task_still_errors() {
        let parsed = parse_cli_from(os_argv(&["agent-loop", "run", "--resume", "some task"]))
            .expect("run --resume task should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let err = dispatch_from_cli(cli).expect_err("run --resume with task should error");
        assert!(
            err.to_string().contains("cannot be combined"),
            "error should mention cannot be combined, got: {err}"
        );
    }

    /// Full chain: `run --resume --file task.md` still returns a validation error.
    #[test]
    fn regression_run_resume_with_file_still_errors() {
        let parsed = parse_cli_from(os_argv(&[
            "agent-loop",
            "run",
            "--resume",
            "--file",
            "task.md",
        ]))
        .expect("run --resume --file should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let err = dispatch_from_cli(cli).expect_err("run --resume with file should error");
        assert!(
            err.to_string().contains("cannot be combined"),
            "error should mention cannot be combined, got: {err}"
        );
    }

    /// `agent-loop "task"` shorthand is normalized to `run "task"` and dispatches correctly.
    #[test]
    fn regression_bare_task_shorthand_full_chain() {
        let parsed = parse_cli_from(os_argv(&["agent-loop", "my cool task"]))
            .expect("bare task should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("bare task dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Run(RunArgs {
                task: Some("my cool task".to_string()),
                file: None,
                resume: false,
                planning_only: false,
                single_agent: false,
            })
        );
    }

    /// All new subcommands (`plan`, `resume`, `tasks`, `run-tasks`) are in
    /// KNOWN_SUBCOMMANDS and normalize_argv does NOT inject `run` before them.
    #[test]
    fn regression_new_subcommands_not_rewritten_by_normalize() {
        for cmd in ["plan", "resume", "tasks", "run-tasks"] {
            assert!(
                KNOWN_SUBCOMMANDS.contains(&cmd),
                "{cmd} must be in KNOWN_SUBCOMMANDS"
            );
            let normalized = normalize_argv(os_argv(&["agent-loop", cmd]));
            assert_eq!(
                normalized,
                os_argv(&["agent-loop", cmd]),
                "normalize_argv must not inject 'run' before '{cmd}'"
            );
        }
    }

    /// Full chain: `run-tasks` (no args) dispatches to Tasks handler.
    #[test]
    fn regression_run_tasks_no_args_dispatches_to_tasks() {
        let parsed =
            parse_cli_from(os_argv(&["agent-loop", "run-tasks"])).expect("run-tasks should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("run-tasks dispatch should succeed");
        assert!(
            matches!(&dispatch, Dispatch::Tasks(_)),
            "run-tasks should map to Dispatch::Tasks"
        );

        let mut tasks_called = false;
        execute_dispatch(
            dispatch,
            |_| panic!("run handler must not be called"),
            |_| panic!("plan handler must not be called"),
            |_| panic!("resume handler must not be called"),
            |_| {
                tasks_called = true;
                Ok(0)
            },
            || panic!("init handler must not be called"),
            || panic!("status handler must not be called"),
            || panic!("version handler must not be called"),
            || panic!("help handler must not be called"),
        )
        .expect("dispatch should succeed");
        assert!(tasks_called);
    }

    /// Full chain: `resume` routes to resume handler (new-style, no legacy override).
    #[test]
    fn regression_resume_subcommand_full_chain() {
        let parsed =
            parse_cli_from(os_argv(&["agent-loop", "resume"])).expect("resume should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("resume dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Resume(ResumeDispatch {
                args: ResumeArgs {
                    single_agent: false,
                },
                planning_only_legacy_override: None,
                max_rounds_override: None,
                force_implementation: false,
            })
        );

        let mut resume_called = false;
        execute_dispatch(
            dispatch,
            |_| panic!("run handler must not be called"),
            |_| panic!("plan handler must not be called"),
            |rd| {
                resume_called = true;
                assert!(rd.planning_only_legacy_override.is_none());
                Ok(0)
            },
            |_| panic!("tasks handler must not be called"),
            || panic!("init handler must not be called"),
            || panic!("status handler must not be called"),
            || panic!("version handler must not be called"),
            || panic!("help handler must not be called"),
        )
        .expect("dispatch should succeed");
        assert!(resume_called);
    }

    /// Full chain: `resume` uses persisted workflow (run) from workflow.txt.
    #[test]
    fn regression_resume_persisted_workflow_routes_correctly() {
        use crate::test_support::env_lock;
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = env_lock();
        clear_config_env();

        let project = crate::test_support::TestProject::builder("resume_precedence_full").build();
        let project_root = project.root.clone();

        // Seed state: status.json + task.md + workflow.txt=run
        fs::create_dir_all(&project.config.state_dir).unwrap();
        crate::state::write_state_file("task.md", "implementation task", &project.config).unwrap();
        fs::write(
            project.config.state_dir.join("status.json"),
            r#"{"status":"IMPLEMENTING","round":2,"implementer":"claude","reviewer":"codex","mode":"dual-agent","lastRunTask":"implementation task","timestamp":"2026-02-15T15:09:44.850Z"}"#,
        ).unwrap();
        fs::write(project.config.state_dir.join("workflow.txt"), "run\n").unwrap();

        let decomposition_called = AtomicBool::new(false);
        let implementation_called = AtomicBool::new(false);
        let summary_called = AtomicBool::new(false);

        // Parse + dispatch `resume` from CLI.
        let parsed =
            parse_cli_from(os_argv(&["agent-loop", "resume"])).expect("resume should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("resume dispatch should succeed");
        let Dispatch::Resume(resume_dispatch) = dispatch else {
            panic!("dispatch should be Resume");
        };

        // Execute resume_command_inner with stub phase fns.
        let exit_code = resume_command_inner(
            resume_dispatch,
            move || Ok(project_root),
            |_config| {
                decomposition_called.store(true, Ordering::SeqCst);
                true
            },
            |_config, _baseline| {
                implementation_called.store(true, Ordering::SeqCst);
                true
            },
            |_config| {
                summary_called.store(true, Ordering::SeqCst);
            },
        )
        .expect("resume_command_inner should succeed");

        assert_eq!(exit_code, 0);
        // Persisted workflow is "run" so implementation path must be chosen.
        assert!(
            !decomposition_called.load(Ordering::SeqCst),
            "decomposition must NOT be called for Run workflow"
        );
        assert!(
            implementation_called.load(Ordering::SeqCst),
            "implementation must be called for Run workflow"
        );
        assert!(
            summary_called.load(Ordering::SeqCst),
            "summary must be called for Run workflow"
        );
    }

    /// Full chain: `resume --single-agent` preserves single_agent flag.
    #[test]
    fn regression_resume_single_agent_preserved() {
        let parsed = parse_cli_from(os_argv(&["agent-loop", "resume", "--single-agent"]))
            .expect("resume --single-agent should parse");
        let ParseOutcome::Parsed(cli) = parsed else {
            panic!("should produce parsed CLI");
        };
        let dispatch = dispatch_from_cli(cli).expect("dispatch should succeed");
        assert_eq!(
            dispatch,
            Dispatch::Resume(ResumeDispatch {
                args: ResumeArgs { single_agent: true },
                planning_only_legacy_override: None,
                max_rounds_override: None,
                force_implementation: false,
            })
        );
    }
}
