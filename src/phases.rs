use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{
    agent::run_agent,
    config::{Agent, Config},
    error::AgentLoopError,
    git::{git_checkpoint, git_diff_for_review, git_rev_parse_head},
    prompts::{
        compound_prompt,
        decomposition_initial_prompt, decomposition_reviewer_prompt, decomposition_revision_prompt,
        gather_project_context, implementation_adversarial_review_prompt,
        implementation_consensus_prompt, implementation_implementer_prompt,
        implementation_reviewer_prompt, phase_paths, planning_implementer_revision_prompt,
        planning_initial_prompt, planning_reviewer_prompt,
    },
    state::{
        LoopStatus, Status, StatusPatch, append_decision, is_status_stale, log, read_decisions,
        read_state_file, read_status, summarize_task, timestamp, write_status,
    },
};

const STALE_TIMESTAMP_REASON: &str = "Agent did not write status (stale timestamp detected)";
const DECOMPOSITION_REVISION_FALLBACK_REASON: &str =
    "Reviewer did not provide explicit consensus; continuing revision loop.";
const DECOMPOSITION_MAX_ROUNDS_REASON: &str =
    "Task breakdown did not reach consensus within the decomposition round limit.";
const PLANNING_CONSENSUS_REQUIRED_REASON: &str =
    "Planning-only mode requires consensus before task decomposition.";
const IMPLEMENTATION_ZERO_MAX_ROUNDS_REASON: &str =
    "MAX_ROUNDS is set to 0; no implementation rounds will run.";
const CHECKPOINT_SUMMARY_MAX_LEN: usize = 80;
const IMPLEMENTATION_CHECKPOINT_FALLBACK: &str = "implementation updates";
const QUALITY_CHECK_TIMEOUT_SECS: u64 = 120;
const QUALITY_CHECK_MAX_LINES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanningReviewerAction {
    Approved,
    NeedsRevision,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecompositionStatusDecision {
    Consensus,
    NeedsRevision,
    ForceNeedsRevision,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplementationReviewerDecision {
    Approved,
    NeedsChanges,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplementationConsensusDecision {
    Consensus,
    Disputed,
    Error,
    Continue,
}

fn planning_reviewer_action(status: Status) -> PlanningReviewerAction {
    match status {
        Status::Approved => PlanningReviewerAction::Approved,
        Status::NeedsRevision => PlanningReviewerAction::NeedsRevision,
        Status::Error => PlanningReviewerAction::Error,
        other => {
            eprintln!(
                "⚠️ planning_reviewer_action: unexpected status '{other}', falling back to NeedsRevision"
            );
            PlanningReviewerAction::NeedsRevision
        }
    }
}

fn planning_implementer_reached_consensus(status: Status) -> bool {
    status == Status::Consensus
}

fn decomposition_status_decision(status: Status) -> DecompositionStatusDecision {
    match status {
        Status::Consensus => DecompositionStatusDecision::Consensus,
        Status::NeedsRevision => DecompositionStatusDecision::NeedsRevision,
        Status::Error => DecompositionStatusDecision::Error,
        _ => DecompositionStatusDecision::ForceNeedsRevision,
    }
}

fn decomposition_forced_revision_reason(status: Status) -> Option<&'static str> {
    if status == Status::NeedsRevision {
        None
    } else {
        Some(DECOMPOSITION_REVISION_FALLBACK_REASON)
    }
}

fn round_limit_reached(round: u32, max_rounds: u32) -> bool {
    round >= max_rounds
}

fn planning_next_step_command() -> &'static str {
    "agent-loop implement --task \"Task 1: ...\""
}

fn implementation_checkpoint_message(round: u32, changes: &str) -> String {
    let summary = summarize_task(changes, Some(CHECKPOINT_SUMMARY_MAX_LEN));
    if summary.is_empty() {
        return format!("round-{round}-implementation: {IMPLEMENTATION_CHECKPOINT_FALLBACK}");
    }

    format!("round-{round}-implementation: {summary}")
}

fn struggle_date() -> String {
    timestamp()
        .split('T')
        .next()
        .unwrap_or("1970-01-01")
        .to_string()
}

fn record_struggle_signal(task: &str, issue: &str, round: u32, config: &Config) {
    let task_summary = summarize_task(task, Some(120));
    let safe_task = if task_summary.trim().is_empty() {
        "(unknown task)"
    } else {
        task_summary.as_str()
    };
    let safe_issue = if issue.trim().is_empty() {
        "(unknown issue)"
    } else {
        issue.trim()
    };
    let entry = format!(
        "- [STRUGGLE] Task: {safe_task} | Issue: {safe_issue} | Round: {round} | Date: {}",
        struggle_date()
    );
    if let Err(err) = append_decision(&entry, config) {
        let _ = log(
            &format!("WARN: failed to append struggle signal: {err}"),
            config,
        );
    }
}

fn run_compound_phase_with_runner<FRunAgent, FLog>(
    task: &str,
    plan: &str,
    config: &Config,
    run_agent_fn: &mut FRunAgent,
    log_fn: &mut FLog,
) where
    FRunAgent: FnMut(crate::config::Agent, &str, &Config) -> Result<(), AgentLoopError>,
    FLog: FnMut(&str, &Config),
{
    if !config.compound {
        return;
    }

    log_fn("🧠 Running compound learning phase...", config);
    if let Err(err) = run_agent_fn(config.implementer, &compound_prompt(task, plan), config) {
        log_fn(
            &format!("WARN: compound phase failed (continuing): {err}"),
            config,
        );
    } else {
        log_fn("✅ Compound learning phase complete.", config);
    }
}

#[allow(dead_code)]
pub fn compound_phase(task: &str, plan: &str, config: &Config) {
    run_compound_phase_with_runner(
        task,
        plan,
        config,
        &mut |agent, prompt, current_config| run_agent(agent, prompt, current_config).map(|_| ()),
        &mut |message, current_config| {
            let _ = log(message, current_config);
        },
    );
}

fn implementation_reviewer_decision(status: Status) -> ImplementationReviewerDecision {
    match status {
        Status::Approved => ImplementationReviewerDecision::Approved,
        Status::NeedsChanges => ImplementationReviewerDecision::NeedsChanges,
        Status::Error => ImplementationReviewerDecision::Error,
        other => {
            eprintln!(
                "⚠️ implementation_reviewer_decision: unexpected status '{other}', falling back to NeedsChanges"
            );
            ImplementationReviewerDecision::NeedsChanges
        }
    }
}

fn implementation_consensus_decision(status: Status) -> ImplementationConsensusDecision {
    match status {
        Status::Consensus => ImplementationConsensusDecision::Consensus,
        Status::Disputed => ImplementationConsensusDecision::Disputed,
        Status::Error => ImplementationConsensusDecision::Error,
        _ => ImplementationConsensusDecision::Continue,
    }
}

fn warn_on_status_write(transition: &str, patch: StatusPatch, config: &Config) {
    if let Err(err) = write_status(patch, config) {
        let _ = log(
            &format!("WARN: failed to write status transition '{transition}': {err}"),
            config,
        );
    }
}

fn transition_label(patch: &StatusPatch) -> &'static str {
    match patch.status {
        Some(status) => match status {
            Status::Pending => "PENDING",
            Status::Planning => "PLANNING",
            Status::Implementing => "IMPLEMENTING",
            Status::Reviewing => "REVIEWING",
            Status::Approved => "APPROVED",
            Status::Consensus => "CONSENSUS",
            Status::Disputed => "DISPUTED",
            Status::NeedsChanges => "NEEDS_CHANGES",
            Status::NeedsRevision => "NEEDS_REVISION",
            Status::MaxRounds => "MAX_ROUNDS",
            Status::Error => "ERROR",
            Status::Interrupted => "INTERRUPTED",
        },
        None => {
            if patch.rating.is_some() {
                return "rating-update";
            }
            "status-update"
        }
    }
}

fn status_for_error(err: &AgentLoopError) -> Status {
    if matches!(err, AgentLoopError::Interrupted(_)) {
        Status::Interrupted
    } else {
        Status::Error
    }
}

fn write_error_status(config: &Config, round: Option<u32>, reason: String) {
    let _ = log(&format!("❌ {reason}"), config);
    warn_on_status_write(
        "ERROR",
        StatusPatch {
            status: Some(Status::Error),
            round,
            reason: Some(reason),
            ..StatusPatch::default()
        },
        config,
    );
}

fn status_error_reason(status: &LoopStatus) -> String {
    status
        .reason
        .clone()
        .unwrap_or_else(|| "status.json is in ERROR state".to_string())
}

fn run_agent_or_record_error(
    config: &Config,
    agent: Agent,
    prompt: &str,
    round: Option<u32>,
) -> bool {
    match run_agent(agent, prompt, config) {
        Ok(_) => true,
        Err(AgentLoopError::Interrupted(reason)) => {
            let _ = write_status(
                StatusPatch {
                    status: Some(Status::Interrupted),
                    round,
                    reason: Some(reason),
                    ..StatusPatch::default()
                },
                config,
            );
            false
        }
        Err(err) => {
            write_error_status(config, round, err.to_string());
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Quality checks (AUTO_TEST feature)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectType {
    Rust,
    JsTs,
    Unknown,
}

#[derive(Debug, Clone)]
struct CheckCommand {
    label: String,
    command: String,
    remediation: Option<String>,
}

fn detect_project_type(project_dir: &Path) -> ProjectType {
    if project_dir.join("Cargo.toml").exists() {
        ProjectType::Rust
    } else if project_dir.join("package.json").exists() {
        ProjectType::JsTs
    } else {
        ProjectType::Unknown
    }
}

fn is_npm_script_stub(script_value: &str) -> bool {
    let trimmed = script_value.trim();
    if trimmed.is_empty() {
        return true;
    }
    // Common stubs: "echo \"Error: no test specified\" && exit 1"
    // or just an echo-only placeholder
    if trimmed.contains("no test specified") || trimmed.contains("no test command") {
        return true;
    }
    // Pure echo commands with exit (common npm init stub)
    if trimmed.starts_with("echo ") && trimmed.contains("&& exit") {
        return true;
    }
    false
}

fn clippy_available() -> bool {
    Command::new("cargo")
        .args(["clippy", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn resolve_rust_commands() -> Vec<CheckCommand> {
    let mut commands = vec![
        CheckCommand {
            label: "cargo build".to_string(),
            command: "cargo build".to_string(),
            remediation: None,
        },
        CheckCommand {
            label: "cargo test".to_string(),
            command: "cargo test".to_string(),
            remediation: None,
        },
    ];
    if clippy_available() {
        commands.push(CheckCommand {
            label: "cargo clippy".to_string(),
            command: "cargo clippy -- -D warnings".to_string(),
            remediation: None,
        });
    }
    commands
}

fn resolve_jsts_commands(project_dir: &Path) -> Vec<CheckCommand> {
    let package_json_path = project_dir.join("package.json");
    let Ok(contents) = std::fs::read_to_string(&package_json_path) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return Vec::new();
    };

    let scripts = match parsed.get("scripts").and_then(|v| v.as_object()) {
        Some(s) => s,
        None => return Vec::new(),
    };

    let mut commands = Vec::new();
    for script_name in ["build", "test", "lint"] {
        if let Some(script_value) = scripts.get(script_name).and_then(|v| v.as_str())
            && !is_npm_script_stub(script_value)
        {
            commands.push(CheckCommand {
                label: format!("npm run {script_name}"),
                command: format!("npm run {script_name}"),
                remediation: None,
            });
        }
    }
    commands
}

fn resolve_quality_commands(config: &Config) -> Vec<CheckCommand> {
    if !config.quality_commands.is_empty() {
        return config
            .quality_commands
            .iter()
            .map(|quality| CheckCommand {
                label: quality.command.clone(),
                command: quality.command.clone(),
                remediation: quality.remediation.clone(),
            })
            .collect();
    }

    if let Some(override_cmd) = &config.auto_test_cmd {
        return vec![CheckCommand {
            label: "custom".to_string(),
            command: override_cmd.clone(),
            remediation: None,
        }];
    }

    match detect_project_type(&config.project_dir) {
        ProjectType::Rust => resolve_rust_commands(),
        ProjectType::JsTs => resolve_jsts_commands(&config.project_dir),
        ProjectType::Unknown => Vec::new(),
    }
}

#[derive(Debug, Clone)]
struct CheckResult {
    label: String,
    success: bool,
    timed_out: bool,
    remediation: Option<String>,
    output: String,
}

fn truncate_output(raw: &str, max_lines: usize) -> (String, bool) {
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= max_lines {
        return (raw.to_string(), false);
    }
    let kept = &lines[lines.len() - max_lines..];
    let truncated_output = format!(
        "... ({} lines truncated, showing last {max_lines}) ...\n{}",
        lines.len() - max_lines,
        kept.join("\n")
    );
    (truncated_output, true)
}

/// A bounded ring buffer that keeps at most the last `capacity` lines.
/// Used to cap memory usage while reading process output.
struct LineBuf {
    lines: std::collections::VecDeque<String>,
    capacity: usize,
    total: usize,
}

impl LineBuf {
    fn new(capacity: usize) -> Self {
        Self {
            lines: std::collections::VecDeque::with_capacity(capacity),
            capacity,
            total: 0,
        }
    }

    fn push(&mut self, line: String) {
        self.total += 1;
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn into_output(self) -> (String, bool) {
        let truncated = self.total > self.capacity;
        let dropped = self.total.saturating_sub(self.capacity);
        if truncated {
            let mut result = format!(
                "... ({dropped} lines truncated, showing last {}) ...\n",
                self.capacity
            );
            for (i, line) in self.lines.iter().enumerate() {
                if i > 0 {
                    result.push('\n');
                }
                result.push_str(line);
            }
            (result, true)
        } else {
            let mut result = String::new();
            for (i, line) in self.lines.iter().enumerate() {
                if i > 0 {
                    result.push('\n');
                }
                result.push_str(line);
            }
            (result, false)
        }
    }
}

/// Read lines from a reader into a bounded ring buffer, keeping only the last
/// `max_lines` lines. This prevents unbounded memory growth from noisy commands.
fn bounded_read_lines<R: std::io::Read>(reader: R, max_lines: usize) -> LineBuf {
    use std::io::BufRead;
    let mut buf = LineBuf::new(max_lines);
    let reader = std::io::BufReader::new(reader);
    for line in reader.lines() {
        match line {
            Ok(l) => buf.push(l),
            Err(_) => break,
        }
    }
    buf
}

/// Send SIGKILL to an entire process group (unix only).
#[cfg(unix)]
fn sigkill_process_group(pgid: libc::pid_t) {
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

fn run_single_check(check: &CheckCommand, project_dir: &Path) -> CheckResult {
    run_single_check_with_timeout(check, project_dir, QUALITY_CHECK_TIMEOUT_SECS)
}

fn make_quality_shell_command(command: &str, project_dir: &Path) -> Command {
    // Use the platform-native shell so quality checks work without requiring
    // Unix tooling on Windows.
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    };

    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    };

    cmd.current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn run_single_check_with_timeout(
    check: &CheckCommand,
    project_dir: &Path,
    timeout_secs: u64,
) -> CheckResult {
    let mut cmd = make_quality_shell_command(&check.command, project_dir);

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return CheckResult {
                label: check.label.clone(),
                success: false,
                timed_out: false,
                remediation: check.remediation.clone(),
                output: format!("Failed to spawn: {err}"),
            };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let max_lines = QUALITY_CHECK_MAX_LINES;

    // Spawn reader threads with bounded buffering (ring buffer of last N lines).
    // This ensures memory stays bounded even for extremely noisy commands.
    let stdout_handle =
        stdout.map(|r| std::thread::spawn(move || bounded_read_lines(r, max_lines)));
    let stderr_handle =
        stderr.map(|r| std::thread::spawn(move || bounded_read_lines(r, max_lines)));

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut timed_out = false;

    // Phase 1: Wait for child process exit or timeout.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_process_tree(&mut child);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }

    // Phase 2: Join reader threads with a bounded deadline.
    // Even after the direct child exits, descendants may hold pipe FDs open.
    // We give reader threads up to 5 seconds past the original deadline to
    // finish. If they still block, SIGKILL the process group to force pipe
    // closure, then join with a final 2-second grace period.
    let join_deadline = deadline + Duration::from_secs(5);
    let (stdout_buf, stderr_buf) =
        join_readers_bounded(stdout_handle, stderr_handle, join_deadline);

    // If reader joins timed out (descendants holding pipes), force-kill and retry.
    let (stdout_buf, stderr_buf) = if stdout_buf.is_none() || stderr_buf.is_none() {
        // Readers are still blocked — descendants must still hold FDs.
        timed_out = true;
        #[cfg(unix)]
        {
            let pgid = child.id() as libc::pid_t;
            sigkill_process_group(pgid);
        }
        #[cfg(not(unix))]
        {
            let _ = child.kill();
        }
        let _ = child.wait();
        // Give a final 2s for readers to see EOF after SIGKILL.
        // We already consumed the handles above, so just use what we got.
        (stdout_buf, stderr_buf)
    } else {
        (stdout_buf, stderr_buf)
    };

    // Combine stdout and stderr, then apply final truncation.
    let mut combined = String::new();
    if let Some(buf) = stdout_buf {
        let (text, _) = buf.into_output();
        combined.push_str(&text);
    }
    if let Some(buf) = stderr_buf {
        let (text, _) = buf.into_output();
        if !combined.is_empty() && !text.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&text);
    }

    let (output, _truncated) = truncate_output(&combined, QUALITY_CHECK_MAX_LINES);

    let success = if timed_out {
        false
    } else {
        child.try_wait().ok().flatten().is_some_and(|s| s.success())
    };

    CheckResult {
        label: check.label.clone(),
        success,
        timed_out,
        remediation: check.remediation.clone(),
        output,
    }
}

/// Kill the child process and its entire process group.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pgid = child.id() as libc::pid_t;
        // First try graceful SIGTERM to the process group
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        // Brief grace period for cleanup
        std::thread::sleep(Duration::from_millis(500));
        // Force SIGKILL the entire process group to ensure all
        // descendants die and release their pipe FDs.
        sigkill_process_group(pgid);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    // Reap the child to avoid zombies
    let _ = child.wait();
}

/// Join reader threads with a bounded deadline. Returns `None` for any thread
/// that did not complete within the deadline (its handle is consumed but the
/// result is lost).
fn join_readers_bounded(
    stdout_handle: Option<std::thread::JoinHandle<LineBuf>>,
    stderr_handle: Option<std::thread::JoinHandle<LineBuf>>,
    deadline: Instant,
) -> (Option<LineBuf>, Option<LineBuf>) {
    let mut stdout_buf: Option<LineBuf> = None;
    let mut stderr_buf: Option<LineBuf> = None;

    // Try to join each handle, polling with short sleeps until the deadline.
    let handles: Vec<(&str, Option<std::thread::JoinHandle<LineBuf>>)> =
        vec![("stdout", stdout_handle), ("stderr", stderr_handle)];

    let mut pending: Vec<(&str, std::thread::JoinHandle<LineBuf>)> = handles
        .into_iter()
        .filter_map(|(name, h)| h.map(|h| (name, h)))
        .collect();

    while !pending.is_empty() && Instant::now() < deadline {
        let mut still_pending = Vec::new();
        for (name, handle) in pending {
            if handle.is_finished() {
                if let Ok(buf) = handle.join() {
                    match name {
                        "stdout" => stdout_buf = Some(buf),
                        "stderr" => stderr_buf = Some(buf),
                        _ => {}
                    }
                }
            } else {
                still_pending.push((name, handle));
            }
        }
        pending = still_pending;
        if !pending.is_empty() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Any remaining handles are abandoned (threads will be detached on drop).
    // This is intentional — we don't want to block indefinitely.

    (stdout_buf, stderr_buf)
}

fn format_quality_checks(results: &[CheckResult]) -> String {
    let mut lines = vec!["QUALITY CHECKS:".to_string()];

    for result in results {
        let status_label = if result.timed_out {
            "TIMEOUT"
        } else if result.success {
            "PASS"
        } else {
            "FAIL"
        };

        lines.push(format!("\n--- {} [{}] ---", result.label, status_label));
        let remediation = result
            .remediation
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::trim);
        let output = result.output.trim();

        match (remediation, output.is_empty()) {
            (Some(hint), false) => lines.push(format!("REMEDIATION: {hint}\n{}", result.output)),
            (Some(hint), true) => lines.push(format!("REMEDIATION: {hint}")),
            (None, false) => lines.push(result.output.clone()),
            (None, true) => {}
        }
    }

    lines.join("\n")
}

fn run_quality_checks(config: &Config) -> Option<String> {
    if !config.auto_test {
        return None;
    }

    let commands = resolve_quality_commands(config);
    if commands.is_empty() {
        return None;
    }

    let _ = log("🧪 Running quality checks...", config);

    let mut results = Vec::new();
    for check in &commands {
        let _ = log(&format!("  ▶ {}", check.label), config);
        let result = run_single_check(check, &config.project_dir);
        let _ = log(
            &format!(
                "  {} {} [{}]",
                if result.success { "✅" } else { "❌" },
                result.label,
                if result.timed_out {
                    "TIMEOUT"
                } else if result.success {
                    "PASS"
                } else {
                    "FAIL"
                }
            ),
            config,
        );
        results.push(result);
    }

    Some(format_quality_checks(&results))
}

fn format_summary_block(status: &LoopStatus) -> String {
    let border = "═".repeat(60);
    let task_summary = summarize_task(status.last_run_task.as_str(), None);
    let task_summary = if task_summary.is_empty() {
        "(empty)"
    } else {
        task_summary.as_str()
    };

    let mut lines = vec![
        format!("\n{border}"),
        "  AGENT LOOP SUMMARY".to_string(),
        border.clone(),
        format!("  Status:      {}", status.status),
        format!("  Rounds:      {}", status.round),
        format!("  Implementer: {}", status.implementer),
        format!("  Reviewer:    {}", status.reviewer),
        format!("  Mode:        {}", status.mode),
        format!("  Last Task:   {task_summary}"),
    ];

    if let Some(rating) = status.rating {
        lines.push(format!("  Rating:      {rating}/5"));
    }

    if let Some(reason) = status.reason.as_ref().filter(|value| !value.is_empty()) {
        lines.push(format!("  Note:        {reason}"));
    }

    lines.push(border);
    lines.push("\n📁 State files in: .agent-loop/state/".to_string());
    lines.push(
        "   - task.md, plan.md, tasks.md, changes.md, review.md, status.json, log.txt".to_string(),
    );
    lines.push(String::new());

    lines.join("\n")
}

fn print_planning_complete_summary(status: &LoopStatus, task: &str) {
    let last_task_source = if status.last_run_task.trim().is_empty() {
        task
    } else {
        status.last_run_task.as_str()
    };
    let summary = summarize_task(last_task_source, None);
    let summary = if summary.is_empty() {
        "(empty)"
    } else {
        summary.as_str()
    };

    println!("\n{}", "═".repeat(60));
    println!("  PLANNING COMPLETE");
    println!("{}", "═".repeat(60));
    println!("  Status:      {}", status.status);
    println!("  Implementer: {}", status.implementer);
    println!("  Reviewer:    {}", status.reviewer);
    println!("  Mode:        {}", status.mode);
    println!("  Last Task:   {summary}");
    println!("{}", "═".repeat(60));
    println!("\n📋 Next steps:");
    println!("   1. Review tasks in: .agent-loop/state/tasks.md");
    println!("   2. Run each task: {}", planning_next_step_command());
    println!("   3. Or extract a task: cat .agent-loop/state/tasks.md");
    println!();
}

pub fn planning_phase(config: &Config, planning_only: bool) -> bool {
    let _ = log("━━━ Planning Phase ━━━", config);
    warn_on_status_write(
        "PLANNING",
        StatusPatch {
            status: Some(Status::Planning),
            ..StatusPatch::default()
        },
        config,
    );

    let task = read_state_file("task.md", config);
    let paths = phase_paths(config);
    let project_context = gather_project_context(
        &config.project_dir,
        config.effective_context_line_cap() as usize,
        config.effective_planning_context_excerpt_lines() as usize,
    );
    let decisions = read_decisions(config);

    let _ = log("📝 Implementer proposing plan...", config);
    if !run_agent_or_record_error(
        config,
        config.implementer,
        &planning_initial_prompt(&task, &project_context, &decisions, &paths),
        Some(0),
    ) {
        return false;
    }

    let mut planning_round = 0;
    let mut reached_consensus = false;
    let mut dispute_reason: Option<String> = None;

    while planning_round < config.planning_max_rounds {
        planning_round += 1;
        let _ = log(
            &format!(
                "🔄 Planning consensus round {planning_round}/{}",
                config.planning_max_rounds
            ),
            config,
        );

        let plan = read_state_file("plan.md", config);

        let _ = log("🔍 Reviewer evaluating plan...", config);
        let reviewer_prompt_timestamp = timestamp();
        if !run_agent_or_record_error(
            config,
            config.reviewer,
            &planning_reviewer_prompt(
                config,
                &task,
                &plan,
                planning_round,
                &reviewer_prompt_timestamp,
                &paths,
                dispute_reason.as_deref(),
            ),
            Some(planning_round),
        ) {
            return false;
        }

        let mut review_status = read_status(config);
        if review_status.status != Status::Error
            && is_status_stale(&reviewer_prompt_timestamp, &review_status)
        {
            let _ = log(
                "⚠️ Stale status detected after planning reviewer — writing NeedsRevision fallback",
                config,
            );
            warn_on_status_write(
                "NEEDS_REVISION",
                StatusPatch {
                    status: Some(Status::NeedsRevision),
                    round: Some(planning_round),
                    reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                    ..StatusPatch::default()
                },
                config,
            );
            review_status = read_status(config);
        }

        match planning_reviewer_action(review_status.status) {
            PlanningReviewerAction::NeedsRevision => {
                let _ = log(
                    &format!(
                        "📝 Reviewer requested changes: {}",
                        review_status.reason.as_deref().unwrap_or("see plan.md")
                    ),
                    config,
                );

                let _ = log("🔍 Implementer reviewing revised plan...", config);
                let revised_plan = read_state_file("plan.md", config);
                let implementer_prompt_timestamp = timestamp();
                if !run_agent_or_record_error(
                    config,
                    config.implementer,
                    &planning_implementer_revision_prompt(
                        config,
                        &task,
                        &revised_plan,
                        review_status
                            .reason
                            .as_deref()
                            .unwrap_or("See plan revisions"),
                        planning_round,
                        &implementer_prompt_timestamp,
                        &paths,
                    ),
                    Some(planning_round),
                ) {
                    return false;
                }

                let mut implementer_status = read_status(config);
                if implementer_status.status != Status::Error
                    && is_status_stale(&implementer_prompt_timestamp, &implementer_status)
                {
                    let _ = log(
                        "⚠️ Stale status detected after planning implementer revision — writing NeedsRevision fallback",
                        config,
                    );
                    warn_on_status_write(
                        "NEEDS_REVISION",
                        StatusPatch {
                            status: Some(Status::NeedsRevision),
                            round: Some(planning_round),
                            reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                            ..StatusPatch::default()
                        },
                        config,
                    );
                    implementer_status = read_status(config);
                }

                if implementer_status.status == Status::Error {
                    write_error_status(
                        config,
                        Some(planning_round),
                        status_error_reason(&implementer_status),
                    );
                    return false;
                }

                if planning_implementer_reached_consensus(implementer_status.status) {
                    let _ = log("✅ Both agents agreed on the plan!", config);
                    warn_on_status_write(
                        "APPROVED",
                        StatusPatch {
                            status: Some(Status::Approved),
                            round: Some(planning_round),
                            ..StatusPatch::default()
                        },
                        config,
                    );
                    reached_consensus = true;
                    break;
                }

                dispute_reason = if implementer_status.status == Status::Disputed {
                    implementer_status
                        .reason
                        .as_deref()
                        .filter(|r| !r.trim().is_empty())
                        .map(ToOwned::to_owned)
                } else {
                    None
                };

                let _ = log(
                    &format!(
                        "📝 Implementer has concerns: {}",
                        implementer_status
                            .reason
                            .as_deref()
                            .unwrap_or("see plan.md")
                    ),
                    config,
                );
            }
            PlanningReviewerAction::Approved => {
                let _ = log("✅ Reviewer approved the plan!", config);
                reached_consensus = true;
                break;
            }
            PlanningReviewerAction::Error => {
                write_error_status(
                    config,
                    Some(planning_round),
                    status_error_reason(&review_status),
                );
                return false;
            }
        }

        if round_limit_reached(planning_round, config.planning_max_rounds) {
            let message = if planning_only {
                "⚠️ Max planning rounds reached without consensus"
            } else {
                "⚠️ Max planning rounds reached - proceeding with current plan"
            };
            let _ = log(message, config);
        }
    }

    if planning_only && !reached_consensus {
        warn_on_status_write(
            "MAX_ROUNDS",
            StatusPatch {
                status: Some(Status::MaxRounds),
                round: Some(planning_round),
                reason: Some(PLANNING_CONSENSUS_REQUIRED_REASON.to_string()),
                ..StatusPatch::default()
            },
            config,
        );
        let _ = log(
            "⏰ Planning-only mode requires consensus; decomposition skipped",
            config,
        );
        return false;
    }

    let final_status = read_status(config);
    let _ = log(
        &format!(
            "✅ Planning phase complete — status: {}",
            final_status.status
        ),
        config,
    );

    true
}

fn decomposition_start_round(config: &Config, resume: bool) -> u32 {
    if !resume {
        return 1;
    }

    let previous = read_status(config);
    if previous.status == Status::Consensus {
        return 1;
    }

    previous.round.saturating_add(1).max(1)
}

fn task_decomposition_phase_internal(config: &Config, resume: bool) -> bool {
    let _ = log("━━━ Task Decomposition Phase ━━━", config);

    let task = read_state_file("task.md", config);
    let plan = read_state_file("plan.md", config);
    let paths = phase_paths(config);
    if resume {
        let current = read_status(config);
        if current.status == Status::Consensus {
            let _ = log("✅ Task decomposition already reached consensus.", config);
            print_planning_complete_summary(&current, &task);
            return true;
        }
    }
    let start_round = decomposition_start_round(config, resume);

    if resume {
        let _ = log(
            &format!(
                "↪ Resuming task decomposition from round {start_round}/{}",
                config.decomposition_max_rounds
            ),
            config,
        );
    }

    if start_round > config.decomposition_max_rounds {
        warn_on_status_write(
            "MAX_ROUNDS",
            StatusPatch {
                status: Some(Status::MaxRounds),
                round: Some(config.decomposition_max_rounds),
                reason: Some(DECOMPOSITION_MAX_ROUNDS_REASON.to_string()),
                ..StatusPatch::default()
            },
            config,
        );
        let _ = log(
            &format!(
                "⏰ Max decomposition rounds ({}) reached without consensus",
                config.decomposition_max_rounds
            ),
            config,
        );
        return false;
    }

    for round in start_round..=config.decomposition_max_rounds {
        let previous_status = read_status(config);
        if previous_status.status == Status::Error {
            write_error_status(config, Some(round), status_error_reason(&previous_status));
            return false;
        }

        warn_on_status_write(
            "PLANNING",
            StatusPatch {
                status: Some(Status::Planning),
                round: Some(round),
                ..StatusPatch::default()
            },
            config,
        );

        if round == 1 {
            let _ = log("📋 Implementer breaking down plan into tasks...", config);
            let project_context = gather_project_context(
                &config.project_dir,
                config.effective_context_line_cap() as usize,
                config.effective_planning_context_excerpt_lines() as usize,
            );
            if !run_agent_or_record_error(
                config,
                config.implementer,
                &decomposition_initial_prompt(&task, &plan, &project_context, &paths),
                Some(round),
            ) {
                return false;
            }
        } else {
            let current_tasks = read_state_file("tasks.md", config);
            let _ = log(
                &format!(
                    "📝 Implementer revising task breakdown (round {round}/{})...",
                    config.decomposition_max_rounds
                ),
                config,
            );

            let implementer_prompt_timestamp = timestamp();
            if !run_agent_or_record_error(
                config,
                config.implementer,
                &decomposition_revision_prompt(
                    config,
                    &task,
                    &plan,
                    &current_tasks,
                    previous_status
                        .reason
                        .as_deref()
                        .unwrap_or("Needs revision"),
                    round,
                    &implementer_prompt_timestamp,
                    &paths,
                ),
                Some(round),
            ) {
                return false;
            }

            let impl_revision_status = read_status(config);
            if impl_revision_status.status != Status::Error
                && is_status_stale(&implementer_prompt_timestamp, &impl_revision_status)
            {
                let _ = log(
                    "⚠️ Stale status detected after decomposition implementer revision — writing NeedsRevision fallback",
                    config,
                );
                warn_on_status_write(
                    "NEEDS_REVISION",
                    StatusPatch {
                        status: Some(Status::NeedsRevision),
                        round: Some(round),
                        reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                        ..StatusPatch::default()
                    },
                    config,
                );
            }
        }

        let _ = log(
            &format!(
                "🔍 Reviewer validating task breakdown (round {round}/{})...",
                config.decomposition_max_rounds
            ),
            config,
        );
        let tasks = read_state_file("tasks.md", config);
        let reviewer_prompt_timestamp = timestamp();

        if !run_agent_or_record_error(
            config,
            config.reviewer,
            &decomposition_reviewer_prompt(
                config,
                &plan,
                &tasks,
                round,
                &reviewer_prompt_timestamp,
                &paths,
            ),
            Some(round),
        ) {
            return false;
        }

        let mut status = read_status(config);
        if status.status != Status::Error && is_status_stale(&reviewer_prompt_timestamp, &status) {
            let _ = log(
                "⚠️ Stale status detected after decomposition reviewer — writing NeedsRevision fallback",
                config,
            );
            warn_on_status_write(
                "NEEDS_REVISION",
                StatusPatch {
                    status: Some(Status::NeedsRevision),
                    round: Some(round),
                    reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                    ..StatusPatch::default()
                },
                config,
            );
            status = read_status(config);
        }

        match decomposition_status_decision(status.status) {
            DecompositionStatusDecision::Consensus => {
                let _ = log("✅ Task breakdown approved!", config);
                print_planning_complete_summary(&status, &task);
                return true;
            }
            DecompositionStatusDecision::NeedsRevision => {}
            DecompositionStatusDecision::Error => {
                write_error_status(config, Some(round), status_error_reason(&status));
                return false;
            }
            DecompositionStatusDecision::ForceNeedsRevision => {
                if let Some(reason) = decomposition_forced_revision_reason(status.status) {
                    warn_on_status_write(
                        "NEEDS_REVISION",
                        StatusPatch {
                            status: Some(Status::NeedsRevision),
                            round: Some(round),
                            reason: Some(reason.to_string()),
                            ..StatusPatch::default()
                        },
                        config,
                    );
                }
            }
        }

        let latest_status = read_status(config);
        let _ = log(
            &format!(
                "⚠️ Task breakdown needs revision: {}",
                latest_status.reason.as_deref().unwrap_or("see tasks.md")
            ),
            config,
        );
    }

    warn_on_status_write(
        "MAX_ROUNDS",
        StatusPatch {
            status: Some(Status::MaxRounds),
            round: Some(config.decomposition_max_rounds),
            reason: Some(DECOMPOSITION_MAX_ROUNDS_REASON.to_string()),
            ..StatusPatch::default()
        },
        config,
    );
    let _ = log(
        &format!(
            "⏰ Max decomposition rounds ({}) reached without consensus",
            config.decomposition_max_rounds
        ),
        config,
    );

    false
}

pub fn task_decomposition_phase(config: &Config) -> bool {
    task_decomposition_phase_internal(config, false)
}

pub fn task_decomposition_phase_resume(config: &Config) -> bool {
    task_decomposition_phase_internal(config, true)
}

const HISTORY_MAX_LINES: usize = 20;

#[allow(clippy::too_many_arguments)]
fn implementation_loop_internal<
    FRunAgent,
    FCheckpoint,
    FLog,
    FWriteStatus,
    FReadStateFile,
    FReadStatus,
    FDiffForReview,
    FTimestamp,
    FReadHistory,
    FAppendHistory,
>(
    config: &Config,
    baseline_files: &HashSet<String>,
    mut run_agent_fn: FRunAgent,
    mut git_checkpoint_fn: FCheckpoint,
    mut log_fn: FLog,
    mut write_status_fn: FWriteStatus,
    mut read_state_file_fn: FReadStateFile,
    mut read_status_fn: FReadStatus,
    mut git_diff_for_review_fn: FDiffForReview,
    mut timestamp_fn: FTimestamp,
    mut read_history_fn: FReadHistory,
    mut append_history_fn: FAppendHistory,
    resume: bool,
) -> bool
where
    FRunAgent: FnMut(crate::config::Agent, &str, &Config) -> Result<(), AgentLoopError>,
    FCheckpoint: FnMut(&str, &Config, &HashSet<String>),
    FLog: FnMut(&str, &Config),
    FWriteStatus: FnMut(StatusPatch, &Config),
    FReadStateFile: FnMut(&str, &Config) -> String,
    FReadStatus: FnMut(&Config) -> LoopStatus,
    FDiffForReview: FnMut(Option<&str>, &Config) -> String,
    FTimestamp: FnMut() -> String,
    FReadHistory: FnMut(&Config, usize) -> String,
    FAppendHistory: FnMut(u32, &str, &str, &Config),
{
    let paths = phase_paths(config);
    let phase_decisions = read_decisions(config);

    if config.max_rounds == 0 {
        let task = read_state_file_fn("task.md", config);
        log_fn(
            &format!("\n⏰ {}", IMPLEMENTATION_ZERO_MAX_ROUNDS_REASON),
            config,
        );
        write_status_fn(
            StatusPatch {
                status: Some(Status::MaxRounds),
                round: Some(0),
                reason: Some(IMPLEMENTATION_ZERO_MAX_ROUNDS_REASON.to_string()),
                ..StatusPatch::default()
            },
            config,
        );
        git_checkpoint_fn("max-rounds-reached", config, baseline_files);
        record_struggle_signal(&task, IMPLEMENTATION_ZERO_MAX_ROUNDS_REASON, 0, config);
        return false;
    }

    let start_round = if resume {
        let previous = read_status_fn(config);
        if previous.status == Status::Consensus {
            log_fn("✅ Implementation already reached consensus.", config);
            return true;
        }
        previous.round.saturating_add(1).max(1)
    } else {
        1
    };

    if resume {
        log_fn(
            &format!(
                "↪ Resuming implementation from round {start_round}/{}",
                config.max_rounds
            ),
            config,
        );
    }

    if start_round > config.max_rounds {
        let task = read_state_file_fn("task.md", config);
        log_fn(
            &format!(
                "\n⏰ Max rounds ({}) reached without consensus",
                config.max_rounds
            ),
            config,
        );
        write_status_fn(
            StatusPatch {
                status: Some(Status::MaxRounds),
                round: Some(config.max_rounds),
                ..StatusPatch::default()
            },
            config,
        );
        git_checkpoint_fn("max-rounds-reached", config, baseline_files);
        record_struggle_signal(
            &task,
            "max rounds already exhausted before resume",
            config.max_rounds,
            config,
        );
        return false;
    }

    for round in start_round..=config.max_rounds {
        log_fn(
            &format!("━━━ Round {round}/{} ━━━", config.max_rounds),
            config,
        );
        write_status_fn(
            StatusPatch {
                status: Some(Status::Implementing),
                round: Some(round),
                ..StatusPatch::default()
            },
            config,
        );

        let pre_impl_head = git_rev_parse_head(config);

        let task = read_state_file_fn("task.md", config);
        let plan = read_state_file_fn("plan.md", config);
        let previous_review = read_state_file_fn("review.md", config);

        let impl_history = read_history_fn(config, HISTORY_MAX_LINES);

        log_fn("🔨 Implementer working...", config);
        if let Err(err) = run_agent_fn(
            config.implementer,
            &implementation_implementer_prompt(
                round,
                &task,
                &plan,
                &previous_review,
                &phase_decisions,
                &paths,
                &impl_history,
            ),
            config,
        ) {
            let status = status_for_error(&err);
            let reason = err.to_string();
            log_fn(&format!("❌ {reason}"), config);
            write_status_fn(
                StatusPatch {
                    status: Some(status),
                    round: Some(round),
                    reason: Some(reason),
                    ..StatusPatch::default()
                },
                config,
            );
            record_struggle_signal(&task, &err.to_string(), round, config);
            return false;
        }

        let changes = read_state_file_fn("changes.md", config);
        append_history_fn(round, "implementation", &changes, config);

        let checkpoint_message = implementation_checkpoint_message(round, &changes);
        git_checkpoint_fn(&checkpoint_message, config, baseline_files);

        let diff = git_diff_for_review_fn(pre_impl_head.as_deref(), config);

        let quality_checks_output = run_quality_checks(config);

        let reviewer_history = read_history_fn(config, HISTORY_MAX_LINES);

        write_status_fn(
            StatusPatch {
                status: Some(Status::Reviewing),
                ..StatusPatch::default()
            },
            config,
        );
        log_fn("🔍 Reviewer evaluating implementation...", config);

        let reviewer_prompt_timestamp = timestamp_fn();
        if let Err(err) = run_agent_fn(
            config.reviewer,
            &implementation_reviewer_prompt(
                config,
                &task,
                &plan,
                &changes,
                &diff,
                round,
                &reviewer_prompt_timestamp,
                &paths,
                quality_checks_output.as_deref(),
                &phase_decisions,
                &reviewer_history,
            ),
            config,
        ) {
            let status = status_for_error(&err);
            let reason = err.to_string();
            log_fn(&format!("❌ {reason}"), config);
            write_status_fn(
                StatusPatch {
                    status: Some(status),
                    round: Some(round),
                    reason: Some(reason),
                    ..StatusPatch::default()
                },
                config,
            );
            record_struggle_signal(&task, &err.to_string(), round, config);
            return false;
        }

        let mut status = read_status_fn(config);
        if status.status != Status::Error && is_status_stale(&reviewer_prompt_timestamp, &status) {
            log_fn(
                "⚠️ Stale status detected after implementation reviewer — writing NeedsChanges fallback",
                config,
            );
            write_status_fn(
                StatusPatch {
                    status: Some(Status::NeedsChanges),
                    round: Some(round),
                    reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                    ..StatusPatch::default()
                },
                config,
            );
            status = read_status_fn(config);
        }
        log_fn(&format!("📊 Review result: {}", status.status), config);

        let review_summary = match status.status {
            Status::Approved => "APPROVED".to_string(),
            Status::NeedsChanges => {
                format!(
                    "NEEDS_CHANGES — {}",
                    status.reason.as_deref().unwrap_or("see review.md")
                )
            }
            other => format!("{other}"),
        };
        append_history_fn(round, "review", &review_summary, config);

        match implementation_reviewer_decision(status.status) {
            ImplementationReviewerDecision::Approved => {
                let reviewer_rating = status.rating;
                let is_perfect_score = reviewer_rating == Some(5);

                if is_perfect_score && config.single_agent {
                    // === BRANCH 1: Single-agent 5/5 — auto-consensus ===
                    log_fn("🎉 Single-agent 5/5 — auto-consensus", config);
                    write_status_fn(
                        StatusPatch {
                            status: Some(Status::Consensus),
                            round: Some(round),
                            rating: reviewer_rating,
                            ..StatusPatch::default()
                        },
                        config,
                    );
                    append_history_fn(
                        round,
                        "consensus",
                        "AUTO-CONSENSUS (single-agent 5/5)",
                        config,
                    );
                    git_checkpoint_fn(&format!("consensus-round-{round}"), config, baseline_files);
                    run_compound_phase_with_runner(
                        &task,
                        &plan,
                        config,
                        &mut run_agent_fn,
                        &mut log_fn,
                    );
                    return true;
                } else if is_perfect_score {
                    // === BRANCH 2: Dual-agent 5/5 — adversarial second review ===
                    log_fn(
                        "🔍 Perfect score — running adversarial second review...",
                        config,
                    );
                    let first_review = read_state_file_fn("review.md", config);

                    // Write intermediate Reviewing status before adversarial call.
                    // This prevents false consensus if the adversarial agent fails to
                    // update status.json — the status stays Reviewing (not Approved),
                    // so implementation_reviewer_decision returns NeedsChanges.
                    write_status_fn(
                        StatusPatch {
                            status: Some(Status::Reviewing),
                            round: Some(round),
                            reason: Some("Awaiting adversarial second review".to_string()),
                            ..StatusPatch::default()
                        },
                        config,
                    );

                    let adversarial_timestamp = timestamp_fn();

                    if let Err(err) = run_agent_fn(
                        config.reviewer,
                        &implementation_adversarial_review_prompt(
                            config,
                            &task,
                            &plan,
                            &changes,
                            &diff,
                            &first_review,
                            round,
                            &adversarial_timestamp,
                            &paths,
                            quality_checks_output.as_deref(),
                        ),
                        config,
                    ) {
                        let status = status_for_error(&err);
                        let reason = err.to_string();
                        log_fn(&format!("❌ {reason}"), config);
                        write_status_fn(
                            StatusPatch {
                                status: Some(status),
                                round: Some(round),
                                reason: Some(reason),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                        record_struggle_signal(&task, &err.to_string(), round, config);
                        return false;
                    }

                    let mut adversarial_status = read_status_fn(config);
                    if adversarial_status.status != Status::Error
                        && is_status_stale(&adversarial_timestamp, &adversarial_status)
                    {
                        log_fn(
                            "⚠️ Stale status after adversarial review — writing NeedsChanges fallback",
                            config,
                        );
                        write_status_fn(
                            StatusPatch {
                                status: Some(Status::NeedsChanges),
                                round: Some(round),
                                reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                        adversarial_status = read_status_fn(config);
                    }

                    let adversarial_summary = match adversarial_status.status {
                        Status::Approved => "APPROVED (adversarial)".to_string(),
                        Status::NeedsChanges => format!(
                            "NEEDS_CHANGES (adversarial) — {}",
                            adversarial_status
                                .reason
                                .as_deref()
                                .unwrap_or("see review.md")
                        ),
                        other => format!("{other} (adversarial)"),
                    };
                    append_history_fn(round, "adversarial-review", &adversarial_summary, config);

                    match implementation_reviewer_decision(adversarial_status.status) {
                        ImplementationReviewerDecision::Approved => {
                            log_fn(
                                "🤝 Adversarial review approved — running implementer self-review...",
                                config,
                            );

                            let review = read_state_file_fn("review.md", config);
                            let consensus_prompt_timestamp = timestamp_fn();
                            if let Err(err) = run_agent_fn(
                                config.implementer,
                                &implementation_consensus_prompt(
                                    config,
                                    &task,
                                    &plan,
                                    &review,
                                    round,
                                    &consensus_prompt_timestamp,
                                    &paths,
                                ),
                                config,
                            ) {
                                let status = status_for_error(&err);
                                let reason = err.to_string();
                                log_fn(&format!("❌ {reason}"), config);
                                write_status_fn(
                                    StatusPatch {
                                        status: Some(status),
                                        round: Some(round),
                                        reason: Some(reason),
                                        ..StatusPatch::default()
                                    },
                                    config,
                                );
                                record_struggle_signal(&task, &err.to_string(), round, config);
                                return false;
                            }

                            let mut final_status = read_status_fn(config);
                            if final_status.status != Status::Error
                                && is_status_stale(&consensus_prompt_timestamp, &final_status)
                            {
                                log_fn(
                                    "⚠️ Stale status detected after implementation consensus — writing Disputed fallback",
                                    config,
                                );
                                write_status_fn(
                                    StatusPatch {
                                        status: Some(Status::Disputed),
                                        round: Some(round),
                                        reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                                        ..StatusPatch::default()
                                    },
                                    config,
                                );
                                final_status = read_status_fn(config);
                            }

                            if final_status.rating.is_none()
                                && let Some(r) = reviewer_rating
                            {
                                write_status_fn(
                                    StatusPatch {
                                        rating: Some(r),
                                        ..StatusPatch::default()
                                    },
                                    config,
                                );
                            }

                            let consensus_summary = match final_status.status {
                                Status::Consensus => "CONSENSUS".to_string(),
                                Status::Disputed => {
                                    format!(
                                        "DISPUTED — {}",
                                        final_status
                                            .reason
                                            .as_deref()
                                            .unwrap_or("see status.json")
                                    )
                                }
                                other => format!("{other}"),
                            };
                            append_history_fn(round, "consensus", &consensus_summary, config);

                            match implementation_consensus_decision(final_status.status) {
                                ImplementationConsensusDecision::Consensus => {
                                    log_fn(
                                        &format!("\n🎉 CONSENSUS reached in round {round}!"),
                                        config,
                                    );
                                    git_checkpoint_fn(
                                        &format!("consensus-round-{round}"),
                                        config,
                                        baseline_files,
                                    );
                                    run_compound_phase_with_runner(
                                        &task,
                                        &plan,
                                        config,
                                        &mut run_agent_fn,
                                        &mut log_fn,
                                    );
                                    return true;
                                }
                                ImplementationConsensusDecision::Disputed
                                | ImplementationConsensusDecision::Continue => {
                                    log_fn(
                                        &format!(
                                            "⚠ Implementer disputed: {}",
                                            final_status
                                                .reason
                                                .as_deref()
                                                .unwrap_or("see status.json")
                                        ),
                                        config,
                                    );
                                }
                                ImplementationConsensusDecision::Error => {
                                    let reason = status_error_reason(&final_status);
                                    log_fn(&format!("❌ {reason}"), config);
                                    write_status_fn(
                                        StatusPatch {
                                            status: Some(Status::Error),
                                            round: Some(round),
                                            reason: Some(reason.clone()),
                                            ..StatusPatch::default()
                                        },
                                        config,
                                    );
                                    record_struggle_signal(&task, &reason, round, config);
                                    return false;
                                }
                            }
                        }
                        ImplementationReviewerDecision::NeedsChanges => {
                            log_fn(
                                &format!(
                                    "⚠ Adversarial review found issues: {}",
                                    adversarial_status
                                        .reason
                                        .as_deref()
                                        .unwrap_or("see review.md")
                                ),
                                config,
                            );
                            // NeedsChanges status already written by the agent; loop continues
                        }
                        ImplementationReviewerDecision::Error => {
                            let reason = status_error_reason(&adversarial_status);
                            log_fn(&format!("❌ {reason}"), config);
                            write_status_fn(
                                StatusPatch {
                                    status: Some(Status::Error),
                                    round: Some(round),
                                    reason: Some(reason.clone()),
                                    ..StatusPatch::default()
                                },
                                config,
                            );
                            record_struggle_signal(&task, &reason, round, config);
                            return false;
                        }
                    }
                } else {
                    // === BRANCH 3: Non-5/5 APPROVED — existing consensus flow ===
                    log_fn(
                        "🤝 Reviewer approved — checking implementer consensus...",
                        config,
                    );

                    let review = read_state_file_fn("review.md", config);
                    let consensus_prompt_timestamp = timestamp_fn();
                    if let Err(err) = run_agent_fn(
                        config.implementer,
                        &implementation_consensus_prompt(
                            config,
                            &task,
                            &plan,
                            &review,
                            round,
                            &consensus_prompt_timestamp,
                            &paths,
                        ),
                        config,
                    ) {
                        let status = status_for_error(&err);
                        let reason = err.to_string();
                        log_fn(&format!("❌ {reason}"), config);
                        write_status_fn(
                            StatusPatch {
                                status: Some(status),
                                round: Some(round),
                                reason: Some(reason),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                        record_struggle_signal(&task, &err.to_string(), round, config);
                        return false;
                    }

                    let mut final_status = read_status_fn(config);
                    if final_status.status != Status::Error
                        && is_status_stale(&consensus_prompt_timestamp, &final_status)
                    {
                        log_fn(
                            "⚠️ Stale status detected after implementation consensus — writing Disputed fallback",
                            config,
                        );
                        write_status_fn(
                            StatusPatch {
                                status: Some(Status::Disputed),
                                round: Some(round),
                                reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                        final_status = read_status_fn(config);
                    }

                    if final_status.rating.is_none()
                        && let Some(r) = reviewer_rating
                    {
                        write_status_fn(
                            StatusPatch {
                                rating: Some(r),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                    }

                    let consensus_summary = match final_status.status {
                        Status::Consensus => "CONSENSUS".to_string(),
                        Status::Disputed => {
                            format!(
                                "DISPUTED — {}",
                                final_status.reason.as_deref().unwrap_or("see status.json")
                            )
                        }
                        other => format!("{other}"),
                    };
                    append_history_fn(round, "consensus", &consensus_summary, config);

                    match implementation_consensus_decision(final_status.status) {
                        ImplementationConsensusDecision::Consensus => {
                            log_fn(&format!("\n🎉 CONSENSUS reached in round {round}!"), config);
                            git_checkpoint_fn(
                                &format!("consensus-round-{round}"),
                                config,
                                baseline_files,
                            );
                            run_compound_phase_with_runner(
                                &task,
                                &plan,
                                config,
                                &mut run_agent_fn,
                                &mut log_fn,
                            );
                            return true;
                        }
                        ImplementationConsensusDecision::Disputed
                        | ImplementationConsensusDecision::Continue => {
                            log_fn(
                                &format!(
                                    "⚠ Implementer disputed: {}",
                                    final_status.reason.as_deref().unwrap_or("see status.json")
                                ),
                                config,
                            );
                        }
                        ImplementationConsensusDecision::Error => {
                            let reason = status_error_reason(&final_status);
                            log_fn(&format!("❌ {reason}"), config);
                            write_status_fn(
                                StatusPatch {
                                    status: Some(Status::Error),
                                    round: Some(round),
                                    reason: Some(reason.clone()),
                                    ..StatusPatch::default()
                                },
                                config,
                            );
                            record_struggle_signal(&task, &reason, round, config);
                            return false;
                        }
                    }
                }
            }
            ImplementationReviewerDecision::NeedsChanges => {}
            ImplementationReviewerDecision::Error => {
                let reason = status_error_reason(&status);
                log_fn(&format!("❌ {reason}"), config);
                write_status_fn(
                    StatusPatch {
                        status: Some(Status::Error),
                        round: Some(round),
                        reason: Some(reason.clone()),
                        ..StatusPatch::default()
                    },
                    config,
                );
                record_struggle_signal(&task, &reason, round, config);
                return false;
            }
        }

        if round == config.max_rounds {
            let issue = read_status_fn(config)
                .reason
                .unwrap_or_else(|| "max rounds reached without consensus".to_string());
            log_fn(
                &format!(
                    "\n⏰ Max rounds ({}) reached without consensus",
                    config.max_rounds
                ),
                config,
            );
            write_status_fn(
                StatusPatch {
                    status: Some(Status::MaxRounds),
                    round: Some(round),
                    ..StatusPatch::default()
                },
                config,
            );
            git_checkpoint_fn("max-rounds-reached", config, baseline_files);
            record_struggle_signal(&task, &issue, round, config);
            return false;
        }
    }

    false
}

pub fn implementation_loop(config: &Config, baseline_files: &HashSet<String>) -> bool {
    implementation_loop_internal(
        config,
        baseline_files,
        |agent, prompt, current_config| run_agent(agent, prompt, current_config).map(|_| ()),
        |message, current_config, current_baseline| {
            git_checkpoint(message, current_config, current_baseline);
        },
        |message, current_config| {
            let _ = log(message, current_config);
        },
        |patch, current_config| {
            let label = transition_label(&patch);
            warn_on_status_write(label, patch, current_config);
        },
        read_state_file,
        read_status,
        git_diff_for_review,
        timestamp,
        crate::state::read_recent_history,
        |round, phase, summary, current_config| {
            if let Err(err) =
                crate::state::append_round_summary(round, phase, summary, current_config)
            {
                let _ = log(
                    &format!("WARN: failed to append round summary: {err}"),
                    current_config,
                );
            }
        },
        false,
    )
}

pub fn implementation_loop_resume(config: &Config, baseline_files: &HashSet<String>) -> bool {
    implementation_loop_internal(
        config,
        baseline_files,
        |agent, prompt, current_config| run_agent(agent, prompt, current_config).map(|_| ()),
        |message, current_config, current_baseline| {
            git_checkpoint(message, current_config, current_baseline);
        },
        |message, current_config| {
            let _ = log(message, current_config);
        },
        |patch, current_config| {
            let label = transition_label(&patch);
            warn_on_status_write(label, patch, current_config);
        },
        read_state_file,
        read_status,
        git_diff_for_review,
        timestamp,
        crate::state::read_recent_history,
        |round, phase, summary, current_config| {
            if let Err(err) =
                crate::state::append_round_summary(round, phase, summary, current_config)
            {
                let _ = log(
                    &format!("WARN: failed to append round summary: {err}"),
                    current_config,
                );
            }
        },
        true,
    )
}

pub fn print_summary(config: &Config) {
    let status = read_status(config);
    println!("{}", format_summary_block(&status));
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QualityCommand;
    use crate::test_support::{TestConfigOptions, create_temp_project_root, make_test_config};

    fn test_config() -> Config {
        let root = create_temp_project_root("phases_tests");
        make_test_config(&root, TestConfigOptions::default())
    }

    #[test]
    fn resolve_quality_commands_prefers_quality_commands_over_auto_test_cmd() {
        let options = TestConfigOptions {
            auto_test_cmd: Some("cargo test".to_string()),
            quality_commands: vec![
                QualityCommand {
                    command: "cargo clippy -- -D warnings".to_string(),
                    remediation: Some("Fix all clippy warnings.".to_string()),
                },
                QualityCommand {
                    command: "cargo test".to_string(),
                    remediation: None,
                },
            ],
            ..Default::default()
        };

        let root = create_temp_project_root("phases_quality_override");
        let config = make_test_config(&root, options);
        let commands = resolve_quality_commands(&config);

        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].command, "cargo clippy -- -D warnings");
        assert_eq!(
            commands[0].remediation.as_deref(),
            Some("Fix all clippy warnings.")
        );
        assert_eq!(commands[1].command, "cargo test");
        assert_eq!(commands[1].remediation, None);
    }

    #[test]
    fn make_quality_shell_command_uses_platform_native_shell() {
        let root = create_temp_project_root("phases_native_shell");
        let command = make_quality_shell_command("echo quality-check", &root);
        let program = command.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        #[cfg(windows)]
        {
            assert_eq!(program, "cmd");
            assert_eq!(
                args,
                vec!["/C".to_string(), "echo quality-check".to_string()]
            );
        }

        #[cfg(not(windows))]
        {
            assert_eq!(program, "sh");
            assert_eq!(
                args,
                vec!["-c".to_string(), "echo quality-check".to_string()]
            );
        }
    }

    #[test]
    fn format_quality_checks_prepends_remediation_hint_when_present() {
        let checks = vec![CheckResult {
            label: "cargo clippy -- -D warnings".to_string(),
            success: false,
            timed_out: false,
            remediation: Some("Run cargo clippy --fix first.".to_string()),
            output: "warning: dead_code".to_string(),
        }];

        let rendered = format_quality_checks(&checks);
        assert!(rendered.contains("REMEDIATION: Run cargo clippy --fix first."));
        assert!(rendered.contains("warning: dead_code"));
    }

    #[test]
    fn compound_phase_respects_compound_flag() {
        let mut enabled = test_config();
        enabled.compound = true;

        let mut disabled = test_config();
        disabled.compound = false;

        let mut enabled_calls = 0u32;
        run_compound_phase_with_runner(
            "Task",
            "Plan",
            &enabled,
            &mut |_agent, _prompt, _config| {
                enabled_calls += 1;
                Ok(())
            },
            &mut |_message, _config| {},
        );
        assert_eq!(enabled_calls, 1, "compound should run when enabled");

        let mut disabled_calls = 0u32;
        run_compound_phase_with_runner(
            "Task",
            "Plan",
            &disabled,
            &mut |_agent, _prompt, _config| {
                disabled_calls += 1;
                Ok(())
            },
            &mut |_message, _config| {},
        );
        assert_eq!(disabled_calls, 0, "compound should be skipped when disabled");
    }

    #[test]
    fn record_struggle_signal_appends_expected_shape() {
        let config = test_config();
        record_struggle_signal("Task 7: Handle retries", "timeout", 3, &config);

        let content = crate::state::read_decisions(&config);
        assert!(content.contains("- [STRUGGLE] Task:"));
        assert!(content.contains("| Issue: timeout"));
        assert!(content.contains("| Round: 3 | Date: "));

        // Date should be YYYY-MM-DD (10 chars) right after the Date label.
        let date_start = content
            .find("| Date: ")
            .expect("date field should exist")
            + "| Date: ".len();
        let date = &content[date_start..date_start + 10];
        assert_eq!(date.len(), 10);
        assert!(date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-');
    }
}
