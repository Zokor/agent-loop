mod agent;
mod config;
mod error;
mod git;
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
};

use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use config::{
    Config, DEFAULT_DECOMPOSITION_MAX_ROUNDS, DEFAULT_MAX_ROUNDS, DEFAULT_PLANNING_MAX_ROUNDS,
    DEFAULT_TIMEOUT_SECONDS,
};
use error::AgentLoopError;
use state::{LoopStatus, Status, StatusPatch};

const KNOWN_SUBCOMMANDS: [&str; 6] = ["run", "run-tasks", "init", "status", "version", "help"];

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
    Run(RunArgs),
    RunTasks(RunTasksArgs),
    Init,
    Status,
    Version,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct RunArgs {
    #[arg(value_name = "TASK")]
    task: Option<String>,
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    planning_only: bool,
    #[arg(long)]
    single_agent: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
struct RunTasksArgs {
    #[arg(long, value_name = "PATH")]
    tasks_file: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
    #[arg(long, default_value_t = 2)]
    round_step: u32,
    #[arg(long)]
    single_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Dispatch {
    ShowHelp,
    Run(RunArgs),
    RunTasks(RunTasksArgs),
    Init,
    Status,
    Version,
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
            let dispatch = dispatch_from_cli(cli);
            execute_dispatch(
                dispatch,
                run_command,
                run_tasks_command,
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

fn dispatch_from_cli(cli: Cli) -> Dispatch {
    match cli.command {
        Some(Commands::Run(args)) => Dispatch::Run(args),
        Some(Commands::RunTasks(args)) => Dispatch::RunTasks(args),
        Some(Commands::Init) => Dispatch::Init,
        Some(Commands::Status) => Dispatch::Status,
        Some(Commands::Version) => Dispatch::Version,
        None => Dispatch::ShowHelp,
    }
}

fn execute_dispatch<FRun, FRunTasks, FInit, FStatus, FVersion, FHelp>(
    dispatch: Dispatch,
    run_handler: FRun,
    run_tasks_handler: FRunTasks,
    init_handler: FInit,
    status_handler: FStatus,
    version_handler: FVersion,
    help_handler: FHelp,
) -> Result<i32, AgentLoopError>
where
    FRun: FnOnce(RunArgs) -> Result<i32, AgentLoopError>,
    FRunTasks: FnOnce(RunTasksArgs) -> Result<i32, AgentLoopError>,
    FInit: FnOnce() -> Result<i32, AgentLoopError>,
    FStatus: FnOnce() -> Result<i32, AgentLoopError>,
    FVersion: FnOnce() -> Result<i32, AgentLoopError>,
    FHelp: FnOnce() -> Result<(), AgentLoopError>,
{
    match dispatch {
        Dispatch::Run(args) => run_handler(args),
        Dispatch::RunTasks(args) => run_tasks_handler(args),
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
        "Configuration sources (highest precedence first):\n  1. CLI flags (--single-agent, --planning-only)\n  2. Environment variables\n  3. .agent-loop.toml (per-project config file)\n  4. Built-in defaults\n\nEnvironment variables:\n  MAX_ROUNDS            (default: {DEFAULT_MAX_ROUNDS})   Max implementation/review rounds\n  PLANNING_MAX_ROUNDS   (default: {DEFAULT_PLANNING_MAX_ROUNDS})  Max planning consensus rounds\n  DECOMPOSITION_MAX_ROUNDS (default: {DEFAULT_DECOMPOSITION_MAX_ROUNDS})  Max decomposition rounds\n  TIMEOUT               (default: {DEFAULT_TIMEOUT_SECONDS})  Idle timeout in seconds\n  IMPLEMENTER           (default: claude) Implementer agent: claude|codex\n  REVIEWER                              Reviewer agent: claude|codex (default: opposite of implementer)\n  SINGLE_AGENT          (default: 0)    Enable single-agent mode when truthy\n  AUTO_COMMIT           (default: 1)    Auto-commit loop-owned changes (0 disables)\n  AUTO_TEST             (default: 0)    Run quality checks before review when truthy\n  AUTO_TEST_CMD                         Override auto-detected quality check command\n\nPer-project config: place .agent-loop.toml in the project root (see README)."
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

fn validate_resume_args(args: &RunArgs) -> Result<(), AgentLoopError> {
    if args.task.is_some() || args.file.is_some() {
        return Err(AgentLoopError::Config(
            "--resume cannot be combined with TASK text or --file.".to_string(),
        ));
    }
    Ok(())
}

fn ensure_resume_state_exists(config: &Config) -> Result<(), AgentLoopError> {
    if !config.state_dir.is_dir() {
        return Err(AgentLoopError::State(
            "Cannot resume: .agent-loop/state does not exist. Run without --resume first."
                .to_string(),
        ));
    }

    let status_path = config.state_dir.join("status.json");
    if !status_path.is_file() {
        return Err(AgentLoopError::State(format!(
            "Cannot resume: '{}' is missing.",
            status_path.display()
        )));
    }

    Ok(())
}

fn resolve_task_for_resume(config: &Config) -> Result<String, AgentLoopError> {
    let task = state::read_state_file("task.md", config);
    if task.trim().is_empty() {
        return Err(AgentLoopError::State(format!(
            "Cannot resume: '{}' is empty.",
            config.state_dir.join("task.md").display()
        )));
    }

    Ok(task)
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

fn validate_run_tasks_args(args: &RunTasksArgs) -> Result<(), AgentLoopError> {
    if args.round_step == 0 {
        return Err(AgentLoopError::Config(
            "--round-step must be at least 1.".to_string(),
        ));
    }

    Ok(())
}

fn resolve_tasks_file_path(args: &RunTasksArgs, project_dir: &Path) -> PathBuf {
    args.tasks_file.clone().unwrap_or_else(|| {
        project_dir
            .join(".agent-loop")
            .join("state")
            .join("tasks.md")
    })
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
    planning_phase_fn: FPlanning,
    decomposition_phase_fn: FDecomposition,
    implementation_loop_fn: FImplementation,
    print_summary_fn: FSummary,
) -> i32
where
    FPlanning: FnOnce(&Config) -> bool,
    FDecomposition: FnOnce(&Config) -> bool,
    FImplementation: FnOnce(&Config, &HashSet<String>) -> bool,
    FSummary: FnOnce(&Config),
{
    let planning_succeeded = planning_phase_fn(config);
    if !planning_succeeded {
        return 1;
    }

    if config.planning_only {
        return phase_success_to_exit_code(decomposition_phase_fn(config));
    }

    let reached_consensus = implementation_loop_fn(config, baseline_set);
    print_summary_fn(config);
    phase_success_to_exit_code(reached_consensus)
}

fn execute_resume_phases(
    config: &Config,
    baseline_set: &HashSet<String>,
    print_summary_fn: impl FnOnce(&Config),
) -> i32 {
    if config.planning_only {
        return phase_success_to_exit_code(phases::task_decomposition_phase_resume(config));
    }

    let reached_consensus = phases::implementation_loop_resume(config, baseline_set);
    print_summary_fn(config);
    phase_success_to_exit_code(reached_consensus)
}

fn run_command_with_max_rounds(
    args: RunArgs,
    max_rounds_override: Option<u32>,
) -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let mut config = Config::from_cli(project_dir, args.single_agent, args.planning_only)?;
    if let Some(max_rounds) = max_rounds_override {
        config.max_rounds = max_rounds;
    }

    let task = if args.resume {
        validate_resume_args(&args)?;
        ensure_resume_state_exists(&config)?;
        resolve_task_for_resume(&config)?
    } else {
        resolve_task_for_run(&args)?
    };

    let baseline_vec = if git::is_git_repo(&config) {
        git::list_changed_files(&config)?
    } else {
        Vec::new()
    };
    let baseline_set = baseline_vec.iter().cloned().collect::<HashSet<_>>();

    let exit_code = if args.resume {
        ensure_tasks_file_exists(&config)?;
        execute_resume_phases(&config, &baseline_set, phases::print_summary)
    } else {
        state::init(task.as_str(), &config, &baseline_vec)?;
        ensure_tasks_file_exists(&config)?;
        execute_run_phases(
            &config,
            &baseline_set,
            phases::planning_phase,
            phases::task_decomposition_phase,
            phases::implementation_loop,
            phases::print_summary,
        )
    };
    persist_last_run_task(task.as_str(), &config)?;

    Ok(exit_code)
}

fn run_command(args: RunArgs) -> Result<i32, AgentLoopError> {
    run_command_with_max_rounds(args, None)
}

fn run_tasks_command(args: RunTasksArgs) -> Result<i32, AgentLoopError> {
    validate_run_tasks_args(&args)?;

    let project_dir = current_project_dir()?;
    let tasks_file = resolve_tasks_file_path(&args, &project_dir);
    let raw_tasks = fs::read_to_string(&tasks_file).map_err(|err| {
        AgentLoopError::Config(format!("Failed to read '{}': {err}", tasks_file.display()))
    })?;
    let parsed_tasks = parse_tasks_markdown(&raw_tasks)?;
    let base_max_rounds =
        Config::from_cli(project_dir.clone(), args.single_agent, false)?.max_rounds;

    println!(
        "Found {} tasks in {}",
        parsed_tasks.len(),
        tasks_file.display()
    );

    for (index, task) in parsed_tasks.iter().enumerate() {
        println!();
        println!("[{}/{}] {}", index + 1, parsed_tasks.len(), task.title);

        let mut retry = 0;
        let mut current_max_rounds = base_max_rounds;
        loop {
            let is_resume = retry > 0;
            if is_resume {
                println!(
                    "Resuming with MAX_ROUNDS={} (retry {}/{})",
                    current_max_rounds, retry, args.max_retries
                );
            } else {
                println!("Running with MAX_ROUNDS={current_max_rounds}");
            }

            let run_args = if is_resume {
                RunArgs {
                    task: None,
                    file: None,
                    resume: true,
                    planning_only: false,
                    single_agent: args.single_agent,
                }
            } else {
                RunArgs {
                    task: Some(task.content.clone()),
                    file: None,
                    resume: false,
                    planning_only: false,
                    single_agent: args.single_agent,
                }
            };

            let exit_code = run_command_with_max_rounds(run_args, Some(current_max_rounds))?;
            if exit_code == 0 {
                println!("Task completed: {}", task.title);
                break;
            }

            let status = read_current_status(&project_dir, args.single_agent);
            if !is_retryable_run_tasks_status(status.as_ref()) {
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' failed with status {}.",
                    task.title,
                    format_status_reason(status.as_ref())
                )));
            }

            if retry >= args.max_retries {
                return Err(AgentLoopError::Agent(format!(
                    "Task '{}' reached retry limit after {} attempt(s). Last status: {}.",
                    task.title,
                    retry + 1,
                    format_status_reason(status.as_ref())
                )));
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
            current_max_rounds = current_max_rounds.saturating_add(args.round_step);
        }
    }

    println!();
    println!("All tasks completed.");
    Ok(0)
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

fn status_command() -> Result<i32, AgentLoopError> {
    let project_dir = current_project_dir()?;
    let config = Config::from_cli(project_dir, false, false)?;
    let status_path = config.state_dir.join("status.json");

    if !config.state_dir.is_dir() || !status_path.is_file() {
        println!("not initialized");
        return Ok(0);
    }

    let current_status = state::read_status(&config);
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
        .filter(|value| !value.trim().is_empty())
    {
        println!("reason: {reason}");
    }
    println!("timestamp: {}", current_status.timestamp);

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

    fn test_config(planning_only: bool) -> Config {
        let project_dir = PathBuf::from("/tmp/agent-loop-main-tests");
        make_test_config(
            &project_dir,
            TestConfigOptions {
                planning_only,
                ..TestConfigOptions::default()
            },
        )
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

    fn run_tasks_args(tasks_file: Option<PathBuf>) -> RunTasksArgs {
        RunTasksArgs {
            tasks_file,
            max_retries: 2,
            round_step: 2,
            single_agent: false,
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
        assert_eq!(dispatch_from_cli(cli), Dispatch::Version);
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
    fn validate_resume_args_rejects_task_and_file_inputs() {
        let mut with_task = run_args(Some("Task"), None);
        with_task.resume = true;
        let task_err = validate_resume_args(&with_task).expect_err("task+resume should fail");
        assert!(task_err.to_string().contains("cannot be combined"));

        let mut with_file = run_args(None, Some(PathBuf::from("task.md")));
        with_file.resume = true;
        let file_err = validate_resume_args(&with_file).expect_err("file+resume should fail");
        assert!(file_err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn resolve_task_for_resume_reads_existing_task_file() {
        let root = unique_temp_file("agent_loop_resume_task_read");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state directory should be created");
        crate::state::write_state_file("task.md", "resume me", &config)
            .expect("task.md should be writable");

        let resolved = resolve_task_for_resume(&config).expect("resume task should resolve");
        assert_eq!(resolved, "resume me");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_task_for_resume_rejects_empty_task_file() {
        let root = unique_temp_file("agent_loop_resume_task_empty");
        let config = make_test_config(&root, TestConfigOptions::default());
        fs::create_dir_all(&config.state_dir).expect("state directory should be created");
        crate::state::write_state_file("task.md", " \n\t", &config)
            .expect("task.md should be writable");

        let err = resolve_task_for_resume(&config).expect_err("empty resume task should fail");
        assert!(err.to_string().contains("task.md"));
        assert!(err.to_string().contains("empty"));

        let _ = fs::remove_dir_all(root);
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
    fn validate_run_tasks_args_rejects_zero_round_step() {
        let mut args = run_tasks_args(None);
        args.round_step = 0;

        let err = validate_run_tasks_args(&args).expect_err("round_step=0 should fail");
        assert!(err.to_string().contains("--round-step"));
    }

    #[test]
    fn resolve_tasks_file_path_defaults_to_state_tasks_md() {
        let project_dir = PathBuf::from("/tmp/agent-loop");
        let path = resolve_tasks_file_path(&run_tasks_args(None), &project_dir);
        assert_eq!(
            path,
            project_dir
                .join(".agent-loop")
                .join("state")
                .join("tasks.md")
        );
    }

    #[test]
    fn resolve_tasks_file_path_uses_explicit_override() {
        let project_dir = PathBuf::from("/tmp/agent-loop");
        let custom = PathBuf::from("/tmp/custom/tasks.md");
        let path = resolve_tasks_file_path(&run_tasks_args(Some(custom.clone())), &project_dir);
        assert_eq!(path, custom);
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
    fn no_args_dispatch_uses_help_path_and_returns_zero() {
        let cli = Cli::try_parse_from(["agent-loop"]).expect("no-args parse should succeed");
        let dispatch = dispatch_from_cli(cli);
        assert_eq!(dispatch, Dispatch::ShowHelp);

        let mut help_called = false;
        let exit_code = execute_dispatch(
            dispatch,
            |_| panic!("run handler should not be called"),
            |_| panic!("run-tasks handler should not be called"),
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
        let config = test_config(true);
        let baseline = HashSet::new();

        let mut planning_called = false;
        let mut decomposition_called = false;
        let mut implementation_called = false;
        let mut summary_called = false;

        let success_exit = execute_run_phases(
            &config,
            &baseline,
            |_| {
                planning_called = true;
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

        let failure_exit =
            execute_run_phases(&config, &baseline, |_| true, |_| false, |_, _| true, |_| {});
        assert_eq!(failure_exit, 1);
    }

    #[test]
    fn execute_run_phases_maps_implementation_result_to_exit_code() {
        let config = test_config(false);
        let baseline = HashSet::new();

        let mut summary_called = false;
        let success_exit = execute_run_phases(
            &config,
            &baseline,
            |_| true,
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
            |_| true,
            |_| true,
            |_, _| false,
            |_| summary_called_for_failure = true,
        );
        assert_eq!(failure_exit, 1);
        assert!(summary_called_for_failure);
    }

    #[test]
    fn execute_run_phases_short_circuits_when_planning_fails() {
        let planning_config = test_config(true);
        let implementation_config = test_config(false);
        let baseline = HashSet::new();

        let planning_only_exit = execute_run_phases(
            &planning_config,
            &baseline,
            |_| false,
            |_| panic!("decomposition should not run when planning fails"),
            |_, _| panic!("implementation should not run when planning fails"),
            |_| panic!("summary should not run when planning fails"),
        );
        assert_eq!(planning_only_exit, 1);

        let implementation_exit = execute_run_phases(
            &implementation_config,
            &baseline,
            |_| false,
            |_| panic!("decomposition should not run in implementation mode"),
            |_, _| panic!("implementation should not run when planning fails"),
            |_| panic!("summary should not run when planning fails"),
        );
        assert_eq!(implementation_exit, 1);
    }
}
