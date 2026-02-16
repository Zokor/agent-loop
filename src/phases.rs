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
        decomposition_initial_prompt, decomposition_reviewer_prompt, decomposition_revision_prompt,
        gather_project_context, implementation_adversarial_review_prompt,
        implementation_consensus_prompt, implementation_implementer_prompt,
        implementation_reviewer_prompt, phase_paths, planning_implementer_revision_prompt,
        planning_initial_prompt, planning_reviewer_prompt,
    },
    state::{
        LoopStatus, Status, StatusPatch, is_status_stale, log, read_state_file, read_status,
        summarize_task, timestamp, write_status,
    },
};

#[cfg(test)]
use crate::prompts::single_agent_reviewer_preamble;

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
    "agent-loop run \"Task 1: ...\""
}

fn implementation_checkpoint_message(round: u32, changes: &str) -> String {
    let summary = summarize_task(changes, Some(CHECKPOINT_SUMMARY_MAX_LEN));
    if summary.is_empty() {
        return format!("round-{round}-implementation: {IMPLEMENTATION_CHECKPOINT_FALLBACK}");
    }

    format!("round-{round}-implementation: {summary}")
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
        },
        CheckCommand {
            label: "cargo test".to_string(),
            command: "cargo test".to_string(),
        },
    ];
    if clippy_available() {
        commands.push(CheckCommand {
            label: "cargo clippy".to_string(),
            command: "cargo clippy -- -D warnings".to_string(),
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
            });
        }
    }
    commands
}

fn resolve_quality_commands(config: &Config) -> Vec<CheckCommand> {
    if let Some(override_cmd) = &config.auto_test_cmd {
        return vec![CheckCommand {
            label: "custom".to_string(),
            command: override_cmd.clone(),
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

fn run_single_check_with_timeout(
    check: &CheckCommand,
    project_dir: &Path,
    timeout_secs: u64,
) -> CheckResult {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", &check.command])
        .current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

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
        if !result.output.trim().is_empty() {
            lines.push(result.output.clone());
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

pub fn planning_phase(config: &Config) -> bool {
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

    let _ = log("📝 Implementer proposing plan...", config);
    if !run_agent_or_record_error(
        config,
        config.implementer,
        &planning_initial_prompt(&task, &project_context, &paths),
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
            let message = if config.planning_only {
                "⚠️ Max planning rounds reached without consensus"
            } else {
                "⚠️ Max planning rounds reached - proceeding with current plan"
            };
            let _ = log(message, config);
        }
    }

    if config.planning_only && !reached_consensus {
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

    if config.max_rounds == 0 {
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
                &paths,
                &impl_history,
            ),
            config,
        ) {
            let reason = err.to_string();
            log_fn(&format!("❌ {reason}"), config);
            write_status_fn(
                StatusPatch {
                    status: Some(Status::Error),
                    round: Some(round),
                    reason: Some(reason),
                    ..StatusPatch::default()
                },
                config,
            );
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
                &reviewer_history,
            ),
            config,
        ) {
            let reason = err.to_string();
            log_fn(&format!("❌ {reason}"), config);
            write_status_fn(
                StatusPatch {
                    status: Some(Status::Error),
                    round: Some(round),
                    reason: Some(reason),
                    ..StatusPatch::default()
                },
                config,
            );
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
                    return true;
                } else if is_perfect_score {
                    // === BRANCH 2: Dual-agent 5/5 — adversarial second review ===
                    log_fn(
                        "🔍 Perfect score — running adversarial second review...",
                        config,
                    );
                    let first_review = read_state_file_fn("review.md", config);
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
                        let reason = err.to_string();
                        log_fn(&format!("❌ {reason}"), config);
                        write_status_fn(
                            StatusPatch {
                                status: Some(Status::Error),
                                round: Some(round),
                                reason: Some(reason),
                                ..StatusPatch::default()
                            },
                            config,
                        );
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
                                &format!(
                                    "\n🎉 CONSENSUS reached in round {round}! (adversarial confirmed)"
                                ),
                                config,
                            );
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
                                "CONSENSUS (adversarial confirmed)",
                                config,
                            );
                            git_checkpoint_fn(
                                &format!("consensus-round-{round}"),
                                config,
                                baseline_files,
                            );
                            return true;
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
                                    reason: Some(reason),
                                    ..StatusPatch::default()
                                },
                                config,
                            );
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
                            &review,
                            round,
                            &consensus_prompt_timestamp,
                            &paths,
                        ),
                        config,
                    ) {
                        let reason = err.to_string();
                        log_fn(&format!("❌ {reason}"), config);
                        write_status_fn(
                            StatusPatch {
                                status: Some(Status::Error),
                                round: Some(round),
                                reason: Some(reason),
                                ..StatusPatch::default()
                            },
                            config,
                        );
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
                                    reason: Some(reason),
                                    ..StatusPatch::default()
                                },
                                config,
                            );
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
                        reason: Some(reason),
                        ..StatusPatch::default()
                    },
                    config,
                );
                return false;
            }
        }

        if round == config.max_rounds {
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
    use crate::test_support::{TestConfigOptions, TestProject, env_lock, make_test_config};
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        fs,
        path::PathBuf,
    };

    const TEST_TIMESTAMP: &str = "2026-02-14T00:00:00.000Z";

    fn new_project(single_agent: bool) -> TestProject {
        TestProject::builder("agent_loop_phase_test")
            .single_agent(single_agent)
            .auto_commit(false)
            .build()
    }

    fn test_config(single_agent: bool) -> Config {
        let project_dir = PathBuf::from("/tmp/agent-loop-phases-tests");
        make_test_config(
            &project_dir,
            TestConfigOptions {
                single_agent,
                auto_commit: true,
                ..TestConfigOptions::default()
            },
        )
    }

    fn test_config_with_rounds(single_agent: bool, max_rounds: u32) -> Config {
        let mut config = test_config(single_agent);
        config.max_rounds = max_rounds;
        config
    }

    fn test_loop_status(
        status: Status,
        round: u32,
        reason: Option<&str>,
        config: &Config,
    ) -> LoopStatus {
        test_loop_status_with_rating(status, round, reason, None, config)
    }

    fn test_loop_status_with_rating(
        status: Status,
        round: u32,
        reason: Option<&str>,
        rating: Option<u32>,
        config: &Config,
    ) -> LoopStatus {
        LoopStatus {
            status,
            round,
            implementer: config.implementer.to_string(),
            reviewer: config.reviewer.to_string(),
            mode: config.run_mode.to_string(),
            last_run_task: "Implement task".to_string(),
            reason: reason.map(ToOwned::to_owned),
            rating,
            timestamp: "2026-02-14T00:00:00.000Z".to_string(),
        }
    }

    #[test]
    fn single_agent_reviewer_preamble_is_empty_in_dual_agent_mode() {
        let config = test_config(false);
        assert_eq!(single_agent_reviewer_preamble(&config), "");
    }

    #[test]
    fn single_agent_reviewer_preamble_matches_typescript_text_exactly() {
        let config = test_config(true);
        assert_eq!(
            single_agent_reviewer_preamble(&config),
            "⚠️ SINGLE-AGENT REVIEWER MODE ⚠️
You are now switching roles from IMPLEMENTER to REVIEWER. You must adopt a completely independent, critical perspective.

CRITICAL INSTRUCTIONS:
- You MUST evaluate the work as if you did NOT write it.
- Do NOT assume correctness because the code \"looks familiar.\"
- Actively look for bugs, edge cases, missing tests, and design flaws.
- Apply the same scrutiny you would to a junior developer's first PR.
- If something is unclear or under-tested, flag it — do not give benefit of the doubt.
- Your approval carries weight: a false approval means bugs ship to production.

"
        );
    }

    #[test]
    fn phase_paths_are_derived_from_absolute_state_dir() {
        let config = test_config(false);
        let paths = phase_paths(&config);

        assert!(paths.task_md.is_absolute());
        assert!(paths.plan_md.is_absolute());
        assert!(paths.tasks_md.is_absolute());
        assert!(paths.changes_md.is_absolute());
        assert!(paths.review_md.is_absolute());
        assert!(paths.status_json.is_absolute());

        assert_eq!(paths.task_md, config.state_dir.join("task.md"));
        assert_eq!(paths.plan_md, config.state_dir.join("plan.md"));
        assert_eq!(paths.tasks_md, config.state_dir.join("tasks.md"));
        assert_eq!(paths.changes_md, config.state_dir.join("changes.md"));
        assert_eq!(paths.review_md, config.state_dir.join("review.md"));
        assert_eq!(paths.status_json, config.state_dir.join("status.json"));
    }

    #[test]
    fn planning_reviewer_prompt_contains_paths_and_status_templates() {
        let config = test_config(true);
        let paths = phase_paths(&config);
        let prompt = planning_reviewer_prompt(
            &config,
            "Build feature X",
            "1. Implement it",
            2,
            "2026-02-14T10:00:00.000Z",
            &paths,
            None,
        );

        assert!(prompt.starts_with("⚠️ SINGLE-AGENT REVIEWER MODE ⚠️"));
        assert!(prompt.contains(&format!(
            "write this exact JSON to {}:",
            paths.status_json.display()
        )));
        assert!(prompt.contains(&format!(
            "write your revised plan to {} and write this JSON to {}:",
            paths.plan_md.display(),
            paths.status_json.display()
        )));
        assert!(prompt.contains(
            "{\"status\": \"APPROVED\", \"round\": 2, \"implementer\": \"claude\", \"reviewer\": \"claude\", \"timestamp\": \"2026-02-14T10:00:00.000Z\"}"
        ));
        assert!(prompt.contains(
            "{\"status\": \"NEEDS_REVISION\", \"round\": 2, \"implementer\": \"claude\", \"reviewer\": \"claude\", \"reason\": \"your reason here\", \"timestamp\": \"2026-02-14T10:00:00.000Z\"}"
        ));
    }

    #[test]
    fn decomposition_reviewer_prompt_contains_paths_and_status_templates() {
        let config = test_config(false);
        let paths = phase_paths(&config);
        let prompt = decomposition_reviewer_prompt(
            &config,
            "Plan content",
            "Task list",
            3,
            "2026-02-14T11:30:15.250Z",
            &paths,
        );

        assert!(prompt.starts_with("You are the REVIEWER in a collaborative development loop."));
        assert!(prompt.contains(&format!(
            "write this JSON to {}:",
            paths.status_json.display()
        )));
        assert!(prompt.contains(&format!(
            "revise the task list in {} and write:",
            paths.tasks_md.display()
        )));
        assert!(prompt.contains(
            "{\"status\": \"CONSENSUS\", \"round\": 3, \"implementer\": \"claude\", \"reviewer\": \"codex\", \"timestamp\": \"2026-02-14T11:30:15.250Z\"}"
        ));
        assert!(prompt.contains(
            "{\"status\": \"NEEDS_REVISION\", \"round\": 3, \"implementer\": \"claude\", \"reviewer\": \"codex\", \"reason\": \"brief explanation\", \"timestamp\": \"2026-02-14T11:30:15.250Z\"}"
        ));
    }

    #[test]
    fn decomposition_revision_prompt_contains_feedback_and_disputed_status_template() {
        let config = test_config(false);
        let paths = phase_paths(&config);
        let prompt = decomposition_revision_prompt(
            &config,
            "Original task",
            "Agreed plan",
            "Current tasks",
            "Missing test coverage task",
            2,
            "2026-02-14T12:00:00.999Z",
            &paths,
        );

        assert!(prompt.contains("REVIEWER FEEDBACK:\nMissing test coverage task"));
        assert!(prompt.contains(&format!(
            "Revise {} to address all reviewer feedback",
            paths.tasks_md.display()
        )));
        assert!(prompt.contains(&format!(
            "Then write this JSON to {}:",
            paths.status_json.display()
        )));
        assert!(prompt.contains(
            "{\"status\": \"DISPUTED\", \"round\": 2, \"implementer\": \"claude\", \"reviewer\": \"codex\", \"timestamp\": \"2026-02-14T12:00:00.999Z\"}"
        ));
    }

    #[test]
    #[cfg(unix)]
    fn task_decomposition_phase_uses_previous_round_reason_for_revision_prompt() {
        let _env_guard = env_lock();
        let project = new_project(false);
        crate::state::write_state_file("task.md", "Build feature X", &project.config)
            .expect("task.md should be writable");
        crate::state::write_state_file("plan.md", "Plan details", &project.config)
            .expect("plan.md should be writable");
        crate::state::write_state_file("tasks.md", "Initial tasks", &project.config)
            .expect("tasks.md should be writable");

        project.create_executable(
            "claude",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.claude-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nif [ \"$COUNT\" -eq 2 ]; then\n  printf '%s' \"$2\" > \"$PWD/.round2-prompt.txt\"\nfi\nexit 0\n",
        );
        project.create_executable(
            "codex",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.codex-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nTS=$(echo \"$4\" | /usr/bin/grep -o '\"timestamp\": \"[^\"]*\"' | /usr/bin/head -1 | /usr/bin/sed 's/\"timestamp\": \"\\([^\"]*\\)\"/\\1/')\nSTATUS_PATH=\"$PWD/.agent-loop/state/status.json\"\nif [ \"$COUNT\" -eq 1 ]; then\n  printf '{\"status\":\"NEEDS_REVISION\",\"round\":1,\"reason\":\"tasks too coarse\",\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nelse\n  printf '{\"status\":\"CONSENSUS\",\"round\":2,\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nfi\nexit 0\n",
        );

        let _path_guard = project.with_path_override();
        let decomposed = task_decomposition_phase(&project.config);

        assert!(decomposed);
        let second_round_prompt = fs::read_to_string(project.path(".round2-prompt.txt"))
            .expect("round two prompt should be captured");
        assert!(second_round_prompt.contains("REVIEWER FEEDBACK:\ntasks too coarse"));
        assert!(!second_round_prompt.contains("REVIEWER FEEDBACK:\nNeeds revision"));
    }

    #[test]
    #[cfg(unix)]
    fn planning_phase_aborts_when_reviewer_writes_invalid_status_json() {
        let _env_guard = env_lock();
        let project = new_project(false);
        crate::state::write_state_file("task.md", "Build feature Y", &project.config)
            .expect("task.md should be writable");

        project.create_executable("claude", "#!/bin/sh\nexit 0\n");
        project.create_executable(
            "codex",
            "#!/bin/sh\nprintf '{broken' > \"$PWD/.agent-loop/state/status.json\"\nexit 0\n",
        );

        let _path_guard = project.with_path_override();
        let planned = planning_phase(&project.config);

        assert!(!planned);
        let status = read_status(&project.config);
        assert_eq!(status.status, Status::Error);
        assert!(
            status
                .reason
                .as_deref()
                .is_some_and(|value| value.starts_with("Invalid status.json:"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn task_decomposition_phase_resume_continues_from_next_round() {
        let _env_guard = env_lock();
        let mut project = new_project(false);
        project.config.decomposition_max_rounds = 5;
        crate::state::write_state_file("task.md", "Build feature Z", &project.config)
            .expect("task.md should be writable");
        crate::state::write_state_file("plan.md", "Plan details", &project.config)
            .expect("plan.md should be writable");
        crate::state::write_state_file("tasks.md", "Existing tasks", &project.config)
            .expect("tasks.md should be writable");
        crate::state::write_status(
            StatusPatch {
                status: Some(Status::NeedsRevision),
                round: Some(3),
                reason: Some("carry-forward reason".to_string()),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("status should be writable");

        project.create_executable(
            "claude",
            "#!/bin/sh\nprintf '%s' \"$2\" > \"$PWD/.resume-round4-prompt.txt\"\nexit 0\n",
        );
        project.create_executable(
            "codex",
            "#!/bin/sh\nTS=$(echo \"$4\" | /usr/bin/grep -o '\"timestamp\": \"[^\"]*\"' | /usr/bin/head -1 | /usr/bin/sed 's/\"timestamp\": \"\\([^\"]*\\)\"/\\1/')\nprintf '{\"status\":\"CONSENSUS\",\"round\":4,\"timestamp\":\"%s\"}' \"$TS\" > \"$PWD/.agent-loop/state/status.json\"\nexit 0\n",
        );

        let _path_guard = project.with_path_override();
        let resumed = task_decomposition_phase_resume(&project.config);

        assert!(resumed);
        let prompt = fs::read_to_string(project.path(".resume-round4-prompt.txt"))
            .expect("resume prompt should be captured");
        assert!(prompt.contains("REVIEWER FEEDBACK:\ncarry-forward reason"));
        assert!(prompt.contains("\"round\": 4"));
        assert!(!prompt.contains("Create a task breakdown file"));

        let log_contents = fs::read_to_string(project.path(".agent-loop/state/log.txt"))
            .expect("log should be readable");
        assert!(log_contents.contains("Resuming task decomposition from round 4/5"));
    }

    #[test]
    #[cfg(unix)]
    fn planning_reviewer_stale_status_triggers_needs_revision_fallback() {
        let _env_guard = env_lock();
        let mut project = new_project(false);
        project.config.planning_max_rounds = 1;
        crate::state::write_state_file("task.md", "Build feature S", &project.config)
            .expect("task.md should be writable");

        // Implementer writes an initial plan, does not touch status.
        project.create_executable("claude", "#!/bin/sh\nexit 0\n");
        // Reviewer does NOT write status.json — causes stale detection.
        project.create_executable("codex", "#!/bin/sh\nexit 0\n");

        let _path_guard = project.with_path_override();
        let _planned = planning_phase(&project.config);

        // In non-planning_only mode, planning_phase returns true even without consensus.
        // The key assertion is that stale detection fired (visible in the log).
        let log_contents = project.read_log();
        assert!(
            log_contents.contains("Stale status detected after planning reviewer"),
            "should log stale reviewer detection, got: {log_contents}"
        );
        assert!(
            log_contents.contains(STALE_TIMESTAMP_REASON),
            "stale reason should appear in log, got: {log_contents}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn planning_implementer_revision_stale_status_triggers_needs_revision_fallback() {
        let _env_guard = env_lock();
        let mut project = new_project(false);
        project.config.planning_max_rounds = 2;
        crate::state::write_state_file("task.md", "Build feature T", &project.config)
            .expect("task.md should be writable");

        // Implementer writes initial plan on first call, does NOT write status on revision.
        project.create_executable("claude", "#!/bin/sh\nexit 0\n");
        // Reviewer writes NeedsRevision on first call with matching timestamp (triggers the revision path),
        // then on second call does NOT write (loop exits after max rounds).
        project.create_executable(
            "codex",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.codex-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nTS=$(echo \"$4\" | /usr/bin/grep -o '\"timestamp\": \"[^\"]*\"' | /usr/bin/head -1 | /usr/bin/sed 's/\"timestamp\": \"\\([^\"]*\\)\"/\\1/')\nSTATUS_PATH=\"$PWD/.agent-loop/state/status.json\"\nif [ \"$COUNT\" -eq 1 ]; then\n  printf '{\"status\":\"NEEDS_REVISION\",\"round\":1,\"reason\":\"revise plan\",\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nfi\nexit 0\n",
        );

        let _path_guard = project.with_path_override();
        let _ = planning_phase(&project.config);

        let log_contents = project.read_log();
        assert!(
            log_contents.contains("Stale status detected after planning implementer revision"),
            "should log stale implementer revision detection, got: {log_contents}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn planning_loop_forwards_disputed_reason_to_next_reviewer() {
        let _env_guard = env_lock();
        let mut project = new_project(false);
        project.config.planning_max_rounds = 3;
        crate::state::write_state_file("task.md", "Build feature D", &project.config)
            .expect("task.md should be writable");

        // Implementer (claude): args are [-p, prompt, --dangerously-skip-permissions]
        //   Call 1: writes initial plan (no status).
        //   Call 2 (revision response): writes DISPUTED with reason, matching timestamp from $2.
        //   Call 3 (revision response): writes CONSENSUS, matching timestamp from $2.
        project.create_executable(
            "claude",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.claude-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nTS=$(echo \"$2\" | /usr/bin/grep -o '\"timestamp\": \"[^\"]*\"' | /usr/bin/head -1 | /usr/bin/sed 's/\"timestamp\": \"\\([^\"]*\\)\"/\\1/')\nSTATUS_PATH=\"$PWD/.agent-loop/state/status.json\"\nif [ \"$COUNT\" -eq 2 ]; then\n  printf '{\"status\":\"DISPUTED\",\"round\":1,\"reason\":\"rollback plan is unsafe\",\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nelif [ \"$COUNT\" -eq 3 ]; then\n  printf '{\"status\":\"CONSENSUS\",\"round\":2,\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nfi\nexit 0\n",
        );
        // Reviewer (codex): args are [exec, --skip-git-repo-check, --dangerously-bypass-approvals-and-sandbox, prompt]
        //   Call 1 (round 1): writes NEEDS_REVISION (triggers implementer revision).
        //   Call 2 (round 2): captures its prompt from $4, writes NEEDS_REVISION.
        project.create_executable(
            "codex",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.codex-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nTS=$(echo \"$4\" | /usr/bin/grep -o '\"timestamp\": \"[^\"]*\"' | /usr/bin/head -1 | /usr/bin/sed 's/\"timestamp\": \"\\([^\"]*\\)\"/\\1/')\nSTATUS_PATH=\"$PWD/.agent-loop/state/status.json\"\nif [ \"$COUNT\" -eq 1 ]; then\n  printf '{\"status\":\"NEEDS_REVISION\",\"round\":1,\"reason\":\"needs detail\",\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nelif [ \"$COUNT\" -eq 2 ]; then\n  printf '%s' \"$4\" > \"$PWD/.round2-reviewer-prompt.txt\"\n  printf '{\"status\":\"NEEDS_REVISION\",\"round\":2,\"reason\":\"still needs work\",\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nfi\nexit 0\n",
        );

        let _path_guard = project.with_path_override();
        let planned = planning_phase(&project.config);

        assert!(planned);
        let round2_prompt = fs::read_to_string(project.path(".round2-reviewer-prompt.txt"))
            .expect("round 2 reviewer prompt should be captured");
        assert!(
            round2_prompt.contains("IMPLEMENTER'S CONCERNS:\nrollback plan is unsafe"),
            "round 2 reviewer prompt should include the dispute reason, got: {round2_prompt}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn planning_loop_does_not_forward_concerns_when_no_dispute() {
        let _env_guard = env_lock();
        let mut project = new_project(false);
        project.config.planning_max_rounds = 3;
        crate::state::write_state_file("task.md", "Build feature E", &project.config)
            .expect("task.md should be writable");

        // Implementer (claude): args are [-p, prompt, --dangerously-skip-permissions]
        //   Call 1: writes initial plan (no status).
        //   Call 2 (revision response): writes CONSENSUS (no dispute), matching timestamp from $2.
        project.create_executable(
            "claude",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.claude-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nTS=$(echo \"$2\" | /usr/bin/grep -o '\"timestamp\": \"[^\"]*\"' | /usr/bin/head -1 | /usr/bin/sed 's/\"timestamp\": \"\\([^\"]*\\)\"/\\1/')\nSTATUS_PATH=\"$PWD/.agent-loop/state/status.json\"\nif [ \"$COUNT\" -eq 2 ]; then\n  printf '{\"status\":\"CONSENSUS\",\"round\":1,\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nfi\nexit 0\n",
        );
        // Reviewer (codex): args are [exec, --skip-git-repo-check, --dangerously-bypass-approvals-and-sandbox, prompt]
        //   Call 1 (round 1): captures its prompt from $4, writes NEEDS_REVISION.
        project.create_executable(
            "codex",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.codex-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nprintf '%s' \"$4\" > \"$PWD/.round1-reviewer-prompt.txt\"\nTS=$(echo \"$4\" | /usr/bin/grep -o '\"timestamp\": \"[^\"]*\"' | /usr/bin/head -1 | /usr/bin/sed 's/\"timestamp\": \"\\([^\"]*\\)\"/\\1/')\nSTATUS_PATH=\"$PWD/.agent-loop/state/status.json\"\nprintf '{\"status\":\"NEEDS_REVISION\",\"round\":1,\"reason\":\"needs detail\",\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nexit 0\n",
        );

        let _path_guard = project.with_path_override();
        let planned = planning_phase(&project.config);

        assert!(planned);
        let round1_prompt = fs::read_to_string(project.path(".round1-reviewer-prompt.txt"))
            .expect("round 1 reviewer prompt should be captured");
        assert!(
            !round1_prompt.contains("IMPLEMENTER'S CONCERNS:"),
            "first round reviewer prompt should not include concerns section, got: {round1_prompt}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn decomposition_reviewer_stale_status_triggers_needs_revision_fallback() {
        let _env_guard = env_lock();
        let mut project = new_project(false);
        project.config.decomposition_max_rounds = 1;
        crate::state::write_state_file("task.md", "Build feature U", &project.config)
            .expect("task.md should be writable");
        crate::state::write_state_file("plan.md", "Plan details", &project.config)
            .expect("plan.md should be writable");

        // Implementer writes tasks, does not touch status.
        project.create_executable("claude", "#!/bin/sh\nexit 0\n");
        // Reviewer does NOT write status.json — triggers stale detection.
        project.create_executable("codex", "#!/bin/sh\nexit 0\n");

        let _path_guard = project.with_path_override();
        let decomposed = task_decomposition_phase(&project.config);

        assert!(!decomposed);
        let log_contents = project.read_log();
        assert!(
            log_contents.contains("Stale status detected after decomposition reviewer"),
            "should log stale decomposition reviewer detection, got: {log_contents}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn decomposition_implementer_revision_stale_status_triggers_needs_revision_fallback() {
        let _env_guard = env_lock();
        let mut project = new_project(false);
        project.config.decomposition_max_rounds = 3;
        crate::state::write_state_file("task.md", "Build feature V", &project.config)
            .expect("task.md should be writable");
        crate::state::write_state_file("plan.md", "Plan details", &project.config)
            .expect("plan.md should be writable");
        crate::state::write_state_file("tasks.md", "Initial tasks", &project.config)
            .expect("tasks.md should be writable");

        // Implementer: on revision rounds (count > 1), writes status with a deliberately stale
        // timestamp to trigger stale detection.  On round 1 (initial decomposition) just exits.
        project.create_executable(
            "claude",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.claude-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nSTATUS_PATH=\"$PWD/.agent-loop/state/status.json\"\nif [ \"$COUNT\" -gt 1 ]; then\n  printf '{\"status\":\"PLANNING\",\"round\":2,\"timestamp\":\"1999-01-01T00:00:00.000Z\"}' > \"$STATUS_PATH\"\nfi\nexit 0\n",
        );
        // Reviewer: extracts timestamp from prompt and writes status with matching timestamp.
        // Round 1: writes NeedsRevision (triggers revision in round 2).
        // Round 2+: writes Consensus.
        project.create_executable(
            "codex",
            "#!/bin/sh\nCOUNT_FILE=\"$PWD/.codex-count\"\nCOUNT=0\nif [ -f \"$COUNT_FILE\" ]; then COUNT=$(/bin/cat \"$COUNT_FILE\"); fi\nCOUNT=$((COUNT + 1))\necho \"$COUNT\" > \"$COUNT_FILE\"\nTS=$(echo \"$4\" | /usr/bin/grep -o '\"timestamp\": \"[^\"]*\"' | /usr/bin/head -1 | /usr/bin/sed 's/\"timestamp\": \"\\([^\"]*\\)\"/\\1/')\nSTATUS_PATH=\"$PWD/.agent-loop/state/status.json\"\nif [ \"$COUNT\" -eq 1 ]; then\n  printf '{\"status\":\"NEEDS_REVISION\",\"round\":1,\"reason\":\"improve tasks\",\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nelse\n  printf '{\"status\":\"CONSENSUS\",\"round\":2,\"timestamp\":\"%s\"}' \"$TS\" > \"$STATUS_PATH\"\nfi\nexit 0\n",
        );

        let _path_guard = project.with_path_override();
        let decomposed = task_decomposition_phase(&project.config);

        // Reviewer writes matching timestamps, so decomposition reaches consensus.
        // But the implementer revision (round 2) is stale because claude writes an old timestamp.
        assert!(decomposed);
        let log_contents = project.read_log();
        assert!(
            log_contents.contains("Stale status detected after decomposition implementer revision"),
            "should log stale decomposition implementer revision detection, got: {log_contents}"
        );
    }

    #[test]
    fn implementation_implementer_prompt_switches_between_first_round_and_feedback_branches() {
        let config = test_config(false);
        let paths = phase_paths(&config);
        let first_round_prompt =
            implementation_implementer_prompt(1, "Task", "Plan", "", &paths, "");
        let feedback_prompt =
            implementation_implementer_prompt(2, "Task", "Plan", "Missing tests", &paths, "");

        assert!(first_round_prompt.contains("This is the first implementation round."));
        assert!(!first_round_prompt.contains("PREVIOUS REVIEW FEEDBACK (address all issues):"));
        assert!(
            feedback_prompt
                .contains("PREVIOUS REVIEW FEEDBACK (address all issues):\nMissing tests")
        );
        assert!(!feedback_prompt.contains("This is the first implementation round."));

        let changes_path = paths.changes_md.display().to_string();
        assert!(first_round_prompt.contains(&format!(
            "write a summary of all changes to: {changes_path}"
        )));
    }

    #[test]
    fn implementation_reviewer_prompt_contains_templates_paths_and_conditional_preamble() {
        let single_agent_config = test_config(true);
        let dual_agent_config = test_config(false);
        let single_paths = phase_paths(&single_agent_config);
        let dual_paths = phase_paths(&dual_agent_config);

        let single_prompt = implementation_reviewer_prompt(
            &single_agent_config,
            "Task",
            "Plan",
            "Changes",
            "Test diff content",
            3,
            "2026-02-14T13:45:22.111Z",
            &single_paths,
            None,
            "",
        );
        let dual_prompt = implementation_reviewer_prompt(
            &dual_agent_config,
            "Task",
            "Plan",
            "Changes",
            "Test diff content",
            3,
            "2026-02-14T13:45:22.111Z",
            &dual_paths,
            None,
            "",
        );

        assert!(single_prompt.starts_with("⚠️ SINGLE-AGENT REVIEWER MODE ⚠️"));
        assert!(
            dual_prompt.starts_with(
                "You are the REVIEWER in round 3 of a collaborative development loop."
            )
        );
        assert!(!dual_prompt.starts_with("⚠️ SINGLE-AGENT REVIEWER MODE ⚠️"));

        assert!(single_prompt.contains("ACTUAL CODE DIFF:\nTest diff content"));
        assert!(dual_prompt.contains("ACTUAL CODE DIFF:\nTest diff content"));
        assert!(dual_prompt.contains("Review the ACTUAL code changes shown in the diff above"));

        assert!(single_prompt.contains(&format!(
            "Write your detailed review to: {}",
            single_paths.review_md.display()
        )));
        assert!(single_prompt.contains(&format!(
            "Then write one of these to {}:",
            single_paths.status_json.display()
        )));
        assert!(single_prompt.contains(
            "{\"status\": \"APPROVED\", \"round\": 3, \"implementer\": \"claude\", \"reviewer\": \"claude\", \"rating\": 4, \"timestamp\": \"2026-02-14T13:45:22.111Z\"}"
        ));
        assert!(single_prompt.contains(
            "{\"status\": \"NEEDS_CHANGES\", \"round\": 3, \"implementer\": \"claude\", \"reviewer\": \"claude\", \"rating\": 2, \"reason\": \"brief summary\", \"timestamp\": \"2026-02-14T13:45:22.111Z\"}"
        ));
    }

    #[test]
    fn implementation_consensus_prompt_contains_consensus_and_disputed_templates() {
        let config = test_config(false);
        let paths = phase_paths(&config);
        let prompt = implementation_consensus_prompt(
            &config,
            "Looks good overall",
            4,
            "2026-02-14T14:20:00.500Z",
            &paths,
        );

        assert!(prompt.contains("The reviewer has APPROVED your implementation."));
        assert!(prompt.contains("REVIEW:\nLooks good overall"));
        assert!(prompt.contains(&format!("write this to {}:", paths.status_json.display())));
        assert!(prompt.contains(
            "{\"status\": \"CONSENSUS\", \"round\": 4, \"implementer\": \"claude\", \"reviewer\": \"codex\", \"timestamp\": \"2026-02-14T14:20:00.500Z\"}"
        ));
        assert!(prompt.contains(
            "{\"status\": \"DISPUTED\", \"round\": 4, \"implementer\": \"claude\", \"reviewer\": \"codex\", \"reason\": \"what was missed\", \"timestamp\": \"2026-02-14T14:20:00.500Z\"}"
        ));
    }

    #[test]
    fn format_summary_block_matches_typescript_order_and_content() {
        let status = LoopStatus {
            status: Status::NeedsChanges,
            round: 7,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "  Build   implementation summary output  ".to_string(),
            reason: Some("Reviewer requested one more test".to_string()),
            rating: None,
            timestamp: "2026-02-14T15:00:00.000Z".to_string(),
        };

        let border = "═".repeat(60);
        let expected = format!(
            "\n{border}\n  AGENT LOOP SUMMARY\n{border}\n  Status:      NEEDS_CHANGES\n  Rounds:      7\n  Implementer: claude\n  Reviewer:    codex\n  Mode:        dual-agent\n  Last Task:   Build implementation summary output\n  Note:        Reviewer requested one more test\n{border}\n\n📁 State files in: .agent-loop/state/\n   - task.md, plan.md, tasks.md, changes.md, review.md, status.json, log.txt\n"
        );

        assert_eq!(format_summary_block(&status), expected);
    }

    #[test]
    fn format_summary_block_uses_empty_placeholder_and_omits_note() {
        let status = LoopStatus {
            status: Status::Pending,
            round: 0,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "".to_string(),
            reason: None,
            rating: None,
            timestamp: "2026-02-14T15:00:00.000Z".to_string(),
        };

        let output = format_summary_block(&status);
        assert!(output.contains("  Last Task:   (empty)"));
        assert!(!output.contains("  Note:        "));
        assert!(!output.contains("  Rating:      "));
    }

    #[test]
    fn implementation_loop_returns_true_on_approved_then_consensus() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::from(["baseline.txt".to_string()]);
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(Status::Consensus, 1, None, &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        assert_eq!(
            checkpoints,
            vec![
                "round-1-implementation: Changes body".to_string(),
                "consensus-round-1".to_string()
            ]
        );
        assert_eq!(prompts.len(), 3);
        assert!(
            logs.iter()
                .any(|line| line.contains("📊 Review result: APPROVED"))
        );
        assert!(
            logs.iter()
                .any(|line| line.contains("🎉 CONSENSUS reached in round 1!"))
        );
        assert!(
            status_patches
                .iter()
                .any(|patch| patch.status == Some(Status::Implementing) && patch.round == Some(1))
        );
    }

    #[test]
    fn implementation_loop_continues_after_disputed_consensus_response() {
        let config = test_config_with_rounds(false, 2);
        let baseline_files = HashSet::new();
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut prompts = Vec::<String>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review details".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(Status::Disputed, 1, Some("missed rollback case"), &config),
            test_loop_status(Status::NeedsChanges, 2, Some("fix tests"), &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, prompt, _| {
                prompts.push(prompt.to_string());
                Ok(())
            },
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result);
        assert!(prompts.iter().any(|prompt| {
            prompt.starts_with(
                "You are the IMPLEMENTER in round 2 of a collaborative development loop.",
            )
        }));
        assert!(
            logs.iter()
                .any(|line| line.contains("⚠ Implementer disputed: missed rollback case"))
        );
        assert_eq!(
            checkpoints,
            vec![
                "round-1-implementation: Changes body".to_string(),
                "round-2-implementation: Changes body".to_string(),
                "max-rounds-reached".to_string()
            ]
        );
    }

    #[test]
    fn implementation_loop_resume_starts_from_next_round() {
        let config = test_config_with_rounds(false, 5);
        let baseline_files = HashSet::new();
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut prompts = Vec::<String>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review details".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::NeedsChanges, 3, Some("continue"), &config),
            test_loop_status(Status::Approved, 4, None, &config),
            test_loop_status(Status::Consensus, 4, None, &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, prompt, _| {
                prompts.push(prompt.to_string());
                Ok(())
            },
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            true,
        );

        assert!(result);
        assert_eq!(
            checkpoints,
            vec![
                "round-4-implementation: Changes body".to_string(),
                "consensus-round-4".to_string()
            ]
        );
        assert!(
            prompts.iter().any(|prompt| {
                prompt.contains("You are the IMPLEMENTER in round 4 of a collaborative")
            }),
            "resume should continue from round 4"
        );
        assert!(
            logs.iter()
                .any(|line| line.contains("Resuming implementation from round 4/5"))
        );
    }

    #[test]
    fn implementation_loop_writes_max_rounds_and_returns_false_when_limit_reached() {
        let config = test_config_with_rounds(false, 2);
        let baseline_files = HashSet::new();
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::NeedsChanges, 1, Some("add tests"), &config),
            test_loop_status(Status::NeedsChanges, 2, Some("still failing"), &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result);
        assert!(
            logs.iter()
                .any(|line| line.contains("⏰ Max rounds (2) reached without consensus"))
        );
        assert_eq!(
            checkpoints,
            vec![
                "round-1-implementation: Changes body".to_string(),
                "round-2-implementation: Changes body".to_string(),
                "max-rounds-reached".to_string()
            ]
        );
        assert_eq!(
            status_patches.last().and_then(|patch| patch.status),
            Some(Status::MaxRounds)
        );
        assert_eq!(status_patches.last().and_then(|patch| patch.round), Some(2));
    }

    #[test]
    fn implementation_loop_sets_terminal_round_when_reviewer_status_round_is_malformed() {
        let config = test_config_with_rounds(false, 2);
        let baseline_files = HashSet::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::NeedsChanges, 1, Some("add tests"), &config),
            // Simulates normalized malformed reviewer status payload with missing/invalid round.
            test_loop_status(Status::NeedsChanges, 0, Some("still failing"), &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |_, _| {},
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result);
        assert_eq!(
            status_patches.last().and_then(|patch| patch.status),
            Some(Status::MaxRounds)
        );
        assert_eq!(status_patches.last().and_then(|patch| patch.round), Some(2));
    }

    #[test]
    fn implementation_loop_with_zero_max_rounds_sets_status_and_exits_cleanly() {
        let config = test_config_with_rounds(false, 0);
        let baseline_files = HashSet::new();
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let mut run_agent_calls = 0usize;

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| {
                run_agent_calls += 1;
                Ok(())
            },
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |_, _| panic!("state files should not be read when max_rounds is zero"),
            |_| panic!("status should not be read when max_rounds is zero"),
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result);
        assert_eq!(run_agent_calls, 0);
        assert_eq!(checkpoints, vec!["max-rounds-reached".to_string()]);
        assert!(
            logs.iter()
                .any(|line| line.contains(IMPLEMENTATION_ZERO_MAX_ROUNDS_REASON))
        );
        assert_eq!(status_patches.len(), 1);
        assert_eq!(status_patches[0].status, Some(Status::MaxRounds));
        assert_eq!(status_patches[0].round, Some(0));
        assert_eq!(
            status_patches[0].reason.as_deref(),
            Some(IMPLEMENTATION_ZERO_MAX_ROUNDS_REASON)
        );
    }

    #[test]
    fn implementation_loop_stops_and_sets_error_when_agent_execution_fails() {
        let config = test_config_with_rounds(false, 2);
        let baseline_files = HashSet::new();
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Err(AgentLoopError::Agent("claude failed to start".to_string())),
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| panic!("status should not be read after agent execution failure"),
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result);
        assert!(checkpoints.is_empty());
        assert!(
            logs.iter()
                .any(|line| line.contains("claude failed to start"))
        );
        assert!(status_patches.iter().any(|patch| {
            patch.status == Some(Status::Error)
                && patch.round == Some(1)
                && patch
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.contains("claude failed to start"))
        }));
    }

    #[test]
    fn implementation_loop_stops_when_reviewer_status_is_error() {
        let config = test_config_with_rounds(false, 2);
        let baseline_files = HashSet::new();
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut run_agent_calls = 0usize;
        let mut read_statuses = VecDeque::from([test_loop_status(
            Status::Error,
            1,
            Some("Invalid status.json: expected value"),
            &config,
        )]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| {
                run_agent_calls += 1;
                Ok(())
            },
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result);
        assert_eq!(run_agent_calls, 2);
        assert_eq!(
            checkpoints,
            vec!["round-1-implementation: Changes body".to_string()]
        );
        assert!(
            logs.iter()
                .any(|line| { line.contains("❌ Invalid status.json: expected value") })
        );
        assert!(status_patches.iter().any(|patch| {
            patch.status == Some(Status::Error)
                && patch.round == Some(1)
                && patch.reason.as_deref() == Some("Invalid status.json: expected value")
        }));
    }

    #[test]
    fn decomposition_forced_revision_helper_only_forces_non_needs_revision() {
        assert_eq!(
            decomposition_forced_revision_reason(Status::NeedsRevision),
            None
        );
        assert_eq!(
            decomposition_forced_revision_reason(Status::Consensus),
            Some(DECOMPOSITION_REVISION_FALLBACK_REASON)
        );
        assert_eq!(
            decomposition_forced_revision_reason(Status::Approved),
            Some(DECOMPOSITION_REVISION_FALLBACK_REASON)
        );
    }

    #[test]
    fn planning_and_decomposition_transition_helpers_cover_key_branches() {
        assert_eq!(
            planning_reviewer_action(Status::Approved),
            PlanningReviewerAction::Approved
        );
        assert_eq!(
            planning_reviewer_action(Status::NeedsRevision),
            PlanningReviewerAction::NeedsRevision
        );
        assert_eq!(
            planning_reviewer_action(Status::Disputed),
            PlanningReviewerAction::NeedsRevision
        );
        assert_eq!(
            planning_reviewer_action(Status::Error),
            PlanningReviewerAction::Error
        );

        assert!(planning_implementer_reached_consensus(Status::Consensus));
        assert!(!planning_implementer_reached_consensus(Status::Disputed));

        assert_eq!(
            decomposition_status_decision(Status::Consensus),
            DecompositionStatusDecision::Consensus
        );
        assert_eq!(
            decomposition_status_decision(Status::NeedsRevision),
            DecompositionStatusDecision::NeedsRevision
        );
        assert_eq!(
            decomposition_status_decision(Status::Disputed),
            DecompositionStatusDecision::ForceNeedsRevision
        );
        assert_eq!(
            decomposition_status_decision(Status::Error),
            DecompositionStatusDecision::Error
        );

        assert_eq!(
            implementation_reviewer_decision(Status::Approved),
            ImplementationReviewerDecision::Approved
        );
        assert_eq!(
            implementation_reviewer_decision(Status::NeedsChanges),
            ImplementationReviewerDecision::NeedsChanges
        );
        assert_eq!(
            implementation_reviewer_decision(Status::Disputed),
            ImplementationReviewerDecision::NeedsChanges
        );
        assert_eq!(
            implementation_reviewer_decision(Status::Error),
            ImplementationReviewerDecision::Error
        );

        assert_eq!(
            implementation_consensus_decision(Status::Consensus),
            ImplementationConsensusDecision::Consensus
        );
        assert_eq!(
            implementation_consensus_decision(Status::Disputed),
            ImplementationConsensusDecision::Disputed
        );
        assert_eq!(
            implementation_consensus_decision(Status::Approved),
            ImplementationConsensusDecision::Continue
        );
        assert_eq!(
            implementation_consensus_decision(Status::Error),
            ImplementationConsensusDecision::Error
        );

        assert!(round_limit_reached(3, 3));
        assert!(!round_limit_reached(2, 3));
    }

    #[test]
    fn planning_next_step_command_points_to_current_rust_cli() {
        assert_eq!(
            planning_next_step_command(),
            "agent-loop run \"Task 1: ...\""
        );
        assert!(!planning_next_step_command().contains("tsx"));
    }

    #[test]
    fn implementation_reviewer_prompt_includes_rating_field_and_rubric() {
        let config = test_config(false);
        let paths = phase_paths(&config);
        let prompt = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "Test diff content",
            1,
            "2026-02-14T00:00:00.000Z",
            &paths,
            None,
            "",
        );

        assert!(prompt.contains("quality rating from 1-5"));
        assert!(prompt.contains("1 = poor"));
        assert!(prompt.contains("5 = excellent"));
        assert!(prompt.contains("\"rating\": 4"));
        assert!(prompt.contains("\"rating\": 2"));
    }

    #[test]
    fn implementation_checkpoint_message_preserves_round_and_adds_summary() {
        assert_eq!(
            implementation_checkpoint_message(3, "Updated admin order editing flow and tests"),
            "round-3-implementation: Updated admin order editing flow and tests"
        );
        assert_eq!(
            implementation_checkpoint_message(2, "   "),
            "round-2-implementation: implementation updates"
        );
    }

    #[test]
    fn format_summary_block_includes_rating_when_present() {
        let status = LoopStatus {
            status: Status::Consensus,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "Build feature".to_string(),
            reason: None,
            rating: Some(4),
            timestamp: "2026-02-14T00:00:00.000Z".to_string(),
        };

        let output = format_summary_block(&status);
        assert!(output.contains("  Rating:      4/5"));
    }

    #[test]
    fn implementation_loop_preserves_reviewer_rating_when_consensus_omits_it() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Reviewer returns APPROVED with rating=4, then consensus status has no rating
        let mut read_statuses = VecDeque::from([
            test_loop_status_with_rating(Status::Approved, 1, None, Some(4), &config),
            test_loop_status(Status::Consensus, 1, None, &config), // rating: None
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |_, _| {},
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        // Should have a rating-preserving patch
        assert!(
            status_patches.iter().any(|patch| patch.rating == Some(4)),
            "a status patch should preserve the reviewer rating of 4"
        );
    }

    #[test]
    fn transition_label_returns_status_name_or_contextual_fallback() {
        assert_eq!(
            transition_label(&StatusPatch {
                status: Some(Status::Implementing),
                ..StatusPatch::default()
            }),
            "IMPLEMENTING"
        );
        assert_eq!(
            transition_label(&StatusPatch {
                status: Some(Status::Error),
                ..StatusPatch::default()
            }),
            "ERROR"
        );
        assert_eq!(
            transition_label(&StatusPatch {
                rating: Some(4),
                ..StatusPatch::default()
            }),
            "rating-update"
        );
        assert_eq!(transition_label(&StatusPatch::default()), "status-update");
    }

    #[test]
    fn warn_on_status_write_logs_failure_when_status_path_is_blocked() {
        let project = new_project(false);
        // Ensure the state dir and log.txt exist so logging works.
        fs::create_dir_all(&project.config.state_dir).expect("state dir should be created");
        fs::write(project.config.state_dir.join("log.txt"), "").expect("log.txt seed");

        // Block status.json by creating a directory at that path.
        // write_status will fail because it cannot write a file where a directory exists.
        let status_path = project.config.state_dir.join("status.json");
        fs::create_dir_all(&status_path).expect("status.json dir should be created");

        warn_on_status_write(
            "test-transition",
            StatusPatch {
                status: Some(Status::Planning),
                ..StatusPatch::default()
            },
            &project.config,
        );

        let log_contents = fs::read_to_string(project.config.state_dir.join("log.txt"))
            .expect("log.txt should be readable");
        assert!(
            log_contents.contains("failed to write status transition 'test-transition'"),
            "log should contain the transition name, got: {log_contents}"
        );
        // The error message should contain some indication of the I/O failure.
        assert!(
            log_contents.contains("WARN:"),
            "log should contain WARN prefix, got: {log_contents}"
        );
    }

    #[test]
    fn implementation_loop_does_not_overwrite_rating_when_consensus_includes_it() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Reviewer APPROVED with rating=4, consensus also has rating=5
        let mut read_statuses = VecDeque::from([
            test_loop_status_with_rating(Status::Approved, 1, None, Some(4), &config),
            test_loop_status_with_rating(Status::Consensus, 1, None, Some(5), &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |_, _| {},
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        // Should NOT have a rating-preserving patch since consensus already has rating
        assert!(
            !status_patches.iter().any(|patch| patch.rating == Some(4)),
            "should not overwrite existing consensus rating"
        );
    }

    #[test]
    fn implementation_loop_passes_diff_to_reviewer_prompt() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(Status::Consensus, 1, None, &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "diff --git a/file.rs b/file.rs\n+new line".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        // Find the reviewer prompt (second prompt: implementer, reviewer, consensus)
        let reviewer_prompt = prompts
            .iter()
            .find(|(agent, _)| *agent == Agent::Codex)
            .expect("reviewer prompt should exist");
        assert!(
            reviewer_prompt.1.contains("ACTUAL CODE DIFF:"),
            "reviewer prompt should contain ACTUAL CODE DIFF section"
        );
        assert!(
            reviewer_prompt
                .1
                .contains("diff --git a/file.rs b/file.rs\n+new line"),
            "reviewer prompt should contain the actual diff content"
        );
    }

    const STALE_TEST_TIMESTAMP: &str = "2025-01-01T00:00:00.000Z";

    fn stale_loop_status(
        status: Status,
        round: u32,
        reason: Option<&str>,
        config: &Config,
    ) -> LoopStatus {
        LoopStatus {
            status,
            round,
            implementer: config.implementer.to_string(),
            reviewer: config.reviewer.to_string(),
            mode: config.run_mode.to_string(),
            last_run_task: "Implement task".to_string(),
            reason: reason.map(ToOwned::to_owned),
            rating: None,
            timestamp: STALE_TEST_TIMESTAMP.to_string(),
        }
    }

    #[test]
    fn implementation_loop_stale_reviewer_status_writes_needs_changes_with_stale_reason() {
        let config = test_config_with_rounds(false, 1);
        let baseline_files = HashSet::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Round 1: read_status returns stale → triggers NeedsChanges fallback.
        // After correction: NeedsChanges (matching timestamp).
        // Round 1 == max_rounds → exits with MaxRounds.
        let mut read_statuses = VecDeque::from([
            stale_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(
                Status::NeedsChanges,
                1,
                Some(STALE_TIMESTAMP_REASON),
                &config,
            ),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result);

        // Verify stale detection was logged
        assert!(
            logs.iter()
                .any(|line| line.contains("Stale status detected after implementation reviewer")),
            "should log stale detection for reviewer"
        );

        // Verify NeedsChanges fallback was written with stale reason
        assert!(
            status_patches.iter().any(|patch| {
                patch.status == Some(Status::NeedsChanges)
                    && patch.reason.as_deref() == Some(STALE_TIMESTAMP_REASON)
            }),
            "should write NeedsChanges with stale timestamp reason"
        );
    }

    #[test]
    fn implementation_loop_stale_consensus_status_writes_disputed_with_stale_reason() {
        let config = test_config_with_rounds(false, 1);
        let baseline_files = HashSet::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Reviewer: fresh APPROVED → proceeds to consensus.
        // Consensus: stale → Disputed fallback written.
        // Re-read: corrected Disputed.
        // Round 1 == max_rounds → exits with MaxRounds.
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config),
            stale_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(Status::Disputed, 1, Some(STALE_TIMESTAMP_REASON), &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result);

        // Verify stale detection was logged
        assert!(
            logs.iter()
                .any(|line| line.contains("Stale status detected after implementation consensus")),
            "should log stale detection for consensus"
        );

        // Verify Disputed fallback was written with stale reason
        assert!(
            status_patches.iter().any(|patch| {
                patch.status == Some(Status::Disputed)
                    && patch.reason.as_deref() == Some(STALE_TIMESTAMP_REASON)
            }),
            "should write Disputed with stale timestamp reason"
        );
    }

    #[test]
    fn unexpected_status_produces_conservative_fallback() {
        // Planning: unexpected statuses -> NeedsRevision
        assert_eq!(
            planning_reviewer_action(Status::Pending),
            PlanningReviewerAction::NeedsRevision
        );
        assert_eq!(
            planning_reviewer_action(Status::Disputed),
            PlanningReviewerAction::NeedsRevision
        );
        assert_eq!(
            planning_reviewer_action(Status::Implementing),
            PlanningReviewerAction::NeedsRevision
        );

        // Implementation: unexpected statuses -> NeedsChanges
        assert_eq!(
            implementation_reviewer_decision(Status::Pending),
            ImplementationReviewerDecision::NeedsChanges
        );
        assert_eq!(
            implementation_reviewer_decision(Status::Disputed),
            ImplementationReviewerDecision::NeedsChanges
        );
        assert_eq!(
            implementation_reviewer_decision(Status::Planning),
            ImplementationReviewerDecision::NeedsChanges
        );
    }

    // -----------------------------------------------------------------------
    // Quality check tests
    // -----------------------------------------------------------------------

    #[test]
    fn detect_project_type_identifies_rust_project() {
        let project = TestProject::builder("detect_project_rust").build();
        project.write_file("Cargo.toml", "[package]\nname = \"test\"");
        assert_eq!(detect_project_type(&project.root), ProjectType::Rust);
    }

    #[test]
    fn detect_project_type_identifies_jsts_project() {
        let project = TestProject::builder("detect_project_jsts").build();
        project.write_file("package.json", "{\"name\": \"test\"}");
        assert_eq!(detect_project_type(&project.root), ProjectType::JsTs);
    }

    #[test]
    fn detect_project_type_rust_takes_precedence_over_jsts() {
        let project = TestProject::builder("detect_project_both").build();
        project.write_file("Cargo.toml", "[package]\nname = \"test\"");
        project.write_file("package.json", "{\"name\": \"test\"}");
        assert_eq!(detect_project_type(&project.root), ProjectType::Rust);
    }

    #[test]
    fn detect_project_type_returns_unknown_for_empty_dir() {
        let project = TestProject::builder("detect_project_unknown").build();
        assert_eq!(detect_project_type(&project.root), ProjectType::Unknown);
    }

    #[test]
    fn is_npm_script_stub_detects_common_stubs() {
        assert!(is_npm_script_stub(""));
        assert!(is_npm_script_stub("   "));
        assert!(is_npm_script_stub(
            "echo \"Error: no test specified\" && exit 1"
        ));
        assert!(is_npm_script_stub("echo \"no test command\" && exit 1"));
        assert!(is_npm_script_stub("echo \"Error: ...\" && exit 1"));
    }

    #[test]
    fn is_npm_script_stub_accepts_real_scripts() {
        assert!(!is_npm_script_stub("jest"));
        assert!(!is_npm_script_stub("vitest run"));
        assert!(!is_npm_script_stub("eslint ."));
        assert!(!is_npm_script_stub("tsc --noEmit"));
    }

    #[test]
    fn resolve_jsts_commands_includes_real_scripts_only() {
        let project = TestProject::builder("resolve_jsts_cmds").build();
        let package_json = r#"{
            "scripts": {
                "build": "tsc",
                "test": "echo \"Error: no test specified\" && exit 1",
                "lint": "eslint .",
                "dev": "vite"
            }
        }"#;
        project.write_file("package.json", package_json);

        let commands = resolve_jsts_commands(&project.root);
        let labels: Vec<&str> = commands.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"npm run build"));
        assert!(!labels.contains(&"npm run test")); // stub
        assert!(labels.contains(&"npm run lint"));
        assert!(!labels.contains(&"npm run dev")); // not in build/test/lint
    }

    #[test]
    fn resolve_quality_commands_uses_override_when_set() {
        let project = TestProject::builder("resolve_override")
            .auto_test(true)
            .auto_test_cmd(Some("make test".to_string()))
            .build();
        project.write_file("Cargo.toml", "[package]\nname = \"test\"");

        let commands = resolve_quality_commands(&project.config);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].label, "custom");
        assert_eq!(commands[0].command, "make test");
    }

    #[test]
    fn resolve_quality_commands_returns_empty_for_unknown_project_type() {
        let project = TestProject::builder("resolve_unknown")
            .auto_test(true)
            .build();

        let commands = resolve_quality_commands(&project.config);
        assert!(commands.is_empty());
    }

    #[test]
    fn truncate_output_keeps_all_lines_under_limit() {
        let input = "line 1\nline 2\nline 3";
        let (output, truncated) = truncate_output(input, 5);
        assert_eq!(output, input);
        assert!(!truncated);
    }

    #[test]
    fn truncate_output_caps_at_max_lines_showing_tail() {
        let lines: Vec<String> = (1..=150).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let (output, truncated) = truncate_output(&input, 100);
        assert!(truncated);
        assert!(output.contains("(50 lines truncated, showing last 100)"));
        assert!(output.contains("line 51")); // first kept line
        assert!(output.contains("line 150")); // last line
        assert!(!output.contains("\nline 50\n")); // dropped line
    }

    #[test]
    fn format_quality_checks_builds_expected_output() {
        let results = vec![
            CheckResult {
                label: "cargo test".to_string(),
                success: true,
                timed_out: false,
                output: "test result: ok. 5 passed".to_string(),
            },
            CheckResult {
                label: "cargo clippy".to_string(),
                success: false,
                timed_out: false,
                output: "warning: unused variable".to_string(),
            },
            CheckResult {
                label: "slow cmd".to_string(),
                success: false,
                timed_out: true,
                output: "partial output...".to_string(),
            },
        ];

        let formatted = format_quality_checks(&results);
        assert!(formatted.starts_with("QUALITY CHECKS:"));
        assert!(formatted.contains("--- cargo test [PASS] ---"));
        assert!(formatted.contains("test result: ok. 5 passed"));
        assert!(formatted.contains("--- cargo clippy [FAIL] ---"));
        assert!(formatted.contains("warning: unused variable"));
        assert!(formatted.contains("--- slow cmd [TIMEOUT] ---"));
        assert!(formatted.contains("partial output..."));
    }

    #[test]
    fn run_quality_checks_returns_none_when_disabled() {
        let project = TestProject::builder("quality_disabled")
            .auto_test(false)
            .build();
        project.write_file("Cargo.toml", "[package]\nname = \"test\"");

        assert!(run_quality_checks(&project.config).is_none());
    }

    #[test]
    fn run_quality_checks_returns_none_for_unknown_project() {
        let project = TestProject::builder("quality_unknown")
            .auto_test(true)
            .build();

        assert!(run_quality_checks(&project.config).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn run_single_check_captures_output_and_exit_status() {
        let project = TestProject::builder("single_check_pass").build();
        let check = CheckCommand {
            label: "echo test".to_string(),
            command: "echo hello && echo world".to_string(),
        };

        let result = run_single_check(&check, &project.root);
        assert!(result.success);
        assert!(!result.timed_out);
        assert!(result.output.contains("hello"));
        assert!(result.output.contains("world"));
    }

    #[test]
    #[cfg(unix)]
    fn run_single_check_reports_failure_on_nonzero_exit() {
        let project = TestProject::builder("single_check_fail").build();
        let check = CheckCommand {
            label: "failing cmd".to_string(),
            command: "echo error output && exit 1".to_string(),
        };

        let result = run_single_check(&check, &project.root);
        assert!(!result.success);
        assert!(!result.timed_out);
        assert!(result.output.contains("error output"));
    }

    #[test]
    #[cfg(unix)]
    fn run_single_check_times_out_and_kills_process_group() {
        // Use run_single_check_with_timeout with a 2-second timeout and a
        // command that sleeps much longer. This exercises the real timeout path:
        // child process wait → deadline exceeded → SIGTERM/SIGKILL → reap.
        let project = TestProject::builder("single_check_timeout").build();

        let check = CheckCommand {
            label: "slow cmd".to_string(),
            // trap '' TERM: ignore SIGTERM so only SIGKILL will kill it.
            // echo partial output before sleeping to verify output capture.
            command: "trap '' TERM; echo before-timeout; sleep 300".to_string(),
        };

        let start = Instant::now();
        let result = run_single_check_with_timeout(&check, &project.root, 2);
        let elapsed = start.elapsed();

        assert!(result.timed_out, "command should be marked as timed out");
        assert!(
            !result.success,
            "timed-out command should not be marked successful"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "timeout should complete well within 15 seconds, took {:?}",
            elapsed
        );
        assert!(
            result.output.contains("before-timeout"),
            "should capture partial output written before timeout"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_single_check_timeout_kills_background_descendants() {
        // Verifies that background processes holding pipe FDs are also killed,
        // preventing reader thread joins from blocking indefinitely.
        // The shell exits quickly but spawns a background child that holds
        // stdout open and sleeps forever.
        let project = TestProject::builder("timeout_descendants").build();

        let check = CheckCommand {
            label: "bg descendants".to_string(),
            command: "echo started; (trap '' TERM; sleep 300) & sleep 300".to_string(),
        };

        let start = Instant::now();
        let result = run_single_check_with_timeout(&check, &project.root, 2);
        let elapsed = start.elapsed();

        assert!(result.timed_out, "should time out");
        assert!(
            elapsed < Duration::from_secs(15),
            "should not block on descendant pipe FDs, took {:?}",
            elapsed
        );
    }

    #[test]
    fn resolve_rust_commands_includes_build_and_test() {
        // resolve_rust_commands always includes cargo build and cargo test
        let commands = resolve_rust_commands();
        let labels: Vec<&str> = commands.iter().map(|c| c.label.as_str()).collect();
        assert!(
            labels.contains(&"cargo build"),
            "Rust commands should include cargo build"
        );
        assert!(
            labels.contains(&"cargo test"),
            "Rust commands should include cargo test"
        );
        // cargo clippy is conditional on availability; just verify it's either
        // present or absent (no panic)
        assert!(commands.len() >= 2);
        if labels.contains(&"cargo clippy") {
            assert_eq!(commands.len(), 3);
            assert_eq!(commands[2].command, "cargo clippy -- -D warnings");
        }
    }

    #[test]
    fn line_buf_keeps_last_n_lines() {
        let mut buf = LineBuf::new(3);
        buf.push("a".to_string());
        buf.push("b".to_string());
        assert_eq!(buf.total, 2);
        let (output, truncated) = buf.into_output();
        assert!(!truncated);
        assert_eq!(output, "a\nb");
    }

    #[test]
    fn line_buf_truncates_when_exceeding_capacity() {
        let mut buf = LineBuf::new(2);
        buf.push("a".to_string());
        buf.push("b".to_string());
        buf.push("c".to_string());
        buf.push("d".to_string());
        assert_eq!(buf.total, 4);
        assert_eq!(buf.lines.len(), 2);
        let (output, truncated) = buf.into_output();
        assert!(truncated);
        assert!(output.contains("2 lines truncated, showing last 2"));
        assert!(output.contains("c"));
        assert!(output.contains("d"));
        assert!(!output.contains("\na\n"));
    }

    #[test]
    #[cfg(unix)]
    fn run_quality_checks_with_custom_override() {
        let project = TestProject::builder("quality_custom_cmd")
            .auto_test(true)
            .auto_test_cmd(Some("echo custom-check-passed".to_string()))
            .build();

        let result = run_quality_checks(&project.config);
        assert!(result.is_some());
        let output = result.unwrap();
        assert!(output.contains("QUALITY CHECKS:"));
        assert!(output.contains("custom-check-passed"));
        assert!(output.contains("[PASS]"));
    }

    #[test]
    fn implementation_loop_includes_quality_checks_in_reviewer_prompt_when_enabled() {
        let mut config = test_config_with_rounds(false, 3);
        config.auto_test = true;
        config.auto_test_cmd = Some("echo quality-ok".to_string());
        // Point to an existing temp dir so the command can run
        let project = TestProject::builder("quality_in_loop")
            .auto_test(true)
            .auto_test_cmd(Some("echo quality-ok".to_string()))
            .build();

        let baseline_files = HashSet::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &project.config),
            test_loop_status(Status::Consensus, 1, None, &project.config),
        ]);

        let result = implementation_loop_internal(
            &project.config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        let reviewer_prompt = prompts
            .iter()
            .find(|(agent, _)| *agent == Agent::Codex)
            .expect("reviewer prompt should exist");
        assert!(
            reviewer_prompt.1.contains("QUALITY CHECKS:"),
            "reviewer prompt should contain QUALITY CHECKS section when auto_test is enabled"
        );
        assert!(
            reviewer_prompt.1.contains("quality-ok"),
            "reviewer prompt should contain quality check output"
        );
    }

    #[test]
    fn implementation_loop_omits_quality_checks_when_disabled() {
        let config = test_config_with_rounds(false, 3);
        // auto_test defaults to false in test_config
        let baseline_files = HashSet::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(Status::Consensus, 1, None, &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        let reviewer_prompt = prompts
            .iter()
            .find(|(agent, _)| *agent == Agent::Codex)
            .expect("reviewer prompt should exist");
        assert!(
            !reviewer_prompt.1.contains("QUALITY CHECKS:"),
            "reviewer prompt should NOT contain QUALITY CHECKS when auto_test is disabled"
        );
    }

    #[test]
    fn implementation_loop_passes_history_to_implementer_and_reviewer_prompts() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(Status::Consensus, 1, None, &config),
        ]);
        let mut history_read_count = 0u32;

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, max_lines| {
                history_read_count += 1;
                assert_eq!(max_lines, HISTORY_MAX_LINES);
                "Round 0 init: Setup".to_string()
            },
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        // History should be read twice per round: once before implementer, once before reviewer
        assert_eq!(
            history_read_count, 2,
            "read_history_fn should be called twice in one round (impl + reviewer)"
        );

        // Implementer prompt should contain ROUND HISTORY
        let impl_prompt = &prompts[0].1;
        assert!(
            impl_prompt.contains("ROUND HISTORY:\nRound 0 init: Setup"),
            "implementer prompt should include round history"
        );

        // Reviewer prompt should also contain ROUND HISTORY
        let reviewer_prompt = &prompts[1].1;
        assert!(
            reviewer_prompt.contains("ROUND HISTORY:\nRound 0 init: Setup"),
            "reviewer prompt should include round history"
        );
    }

    #[test]
    fn implementation_loop_appends_history_for_implementation_review_and_consensus() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(Status::Consensus, 1, None, &config),
        ]);
        let mut appended: Vec<(u32, String, String)> = Vec::new();

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |round, phase, summary, _| {
                appended.push((round, phase.to_string(), summary.to_string()));
            },
            false,
        );

        assert!(result);
        // Should have 3 appends: implementation, review, consensus
        assert_eq!(
            appended.len(),
            3,
            "should append history for implementation, review, and consensus; got: {appended:?}"
        );
        assert_eq!(appended[0].1, "implementation");
        assert_eq!(appended[0].2, "Changes body");
        assert_eq!(appended[1].1, "review");
        assert!(appended[1].2.contains("APPROVED"));
        assert_eq!(appended[2].1, "consensus");
        assert!(appended[2].2.contains("CONSENSUS"));
    }

    #[test]
    fn implementation_loop_appends_history_for_disputed_consensus() {
        let config = test_config_with_rounds(false, 2);
        let baseline_files = HashSet::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config),
            test_loop_status(Status::Disputed, 1, Some("missed rollback case"), &config),
            test_loop_status(Status::NeedsChanges, 2, Some("fix tests"), &config),
        ]);
        let mut appended: Vec<(u32, String, String)> = Vec::new();

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |round, phase, summary, _| {
                appended.push((round, phase.to_string(), summary.to_string()));
            },
            false,
        );

        assert!(!result);
        // Round 1: implementation, review (APPROVED), consensus (DISPUTED)
        // Round 2: implementation, review (NEEDS_CHANGES)
        assert_eq!(
            appended.len(),
            5,
            "should have 5 appends across 2 rounds; got: {appended:?}"
        );
        assert_eq!(appended[2].1, "consensus");
        assert!(
            appended[2].2.contains("DISPUTED"),
            "consensus append should be DISPUTED, got: {}",
            appended[2].2
        );
        assert!(
            appended[2].2.contains("missed rollback case"),
            "consensus append should include reason"
        );
    }

    #[test]
    fn implementation_loop_reviewer_history_sees_implementation_append() {
        use std::cell::Cell;

        // Verify that the reviewer's history read happens AFTER the implementation append.
        let config = test_config_with_rounds(false, 1);
        let baseline_files = HashSet::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        let mut read_statuses = VecDeque::from([test_loop_status(
            Status::NeedsChanges,
            1,
            Some("fix tests"),
            &config,
        )]);
        let mut history_reads: Vec<String> = Vec::new();
        let appended_count = Cell::new(0u32);

        let _result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| {
                // Return different content based on how many appends have occurred
                let count = appended_count.get();
                let result = format!("history-after-{count}-appends");
                history_reads.push(result.clone());
                result
            },
            |_, _, _, _| {
                appended_count.set(appended_count.get() + 1);
            },
            false,
        );

        // First history read (for implementer): before any appends
        assert_eq!(history_reads[0], "history-after-0-appends");
        // Second history read (for reviewer): after the implementation append
        assert_eq!(history_reads[1], "history-after-1-appends");
    }

    // -----------------------------------------------------------------------
    // Adversarial second review tests (Task 20)
    // -----------------------------------------------------------------------

    #[test]
    fn dual_agent_5_5_adversarial_also_approved_reaches_consensus() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "First review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Reviewer returns APPROVED with rating=5, then adversarial returns APPROVED
        let mut read_statuses = VecDeque::from([
            test_loop_status_with_rating(Status::Approved, 1, None, Some(5), &config),
            test_loop_status_with_rating(Status::Approved, 1, None, Some(5), &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);

        // Should use the reviewer agent for the adversarial call (3rd call: implementer, reviewer, adversarial-reviewer)
        assert_eq!(prompts.len(), 3, "should have 3 agent calls");
        assert_eq!(
            prompts[0].0,
            Agent::Claude,
            "first call should be implementer"
        );
        assert_eq!(prompts[1].0, Agent::Codex, "second call should be reviewer");
        assert_eq!(
            prompts[2].0,
            Agent::Codex,
            "third (adversarial) call should use reviewer agent"
        );

        // Adversarial prompt should contain adversarial framing and first review
        assert!(
            prompts[2].1.contains("find what the first reviewer missed"),
            "adversarial prompt should contain adversarial framing"
        );
        assert!(
            prompts[2].1.contains("First review body"),
            "adversarial prompt should contain the first review"
        );
        assert!(
            prompts[2].1.contains("(test diff)"),
            "adversarial prompt should contain the diff"
        );

        // Status patches should include Consensus with rating=5
        assert!(
            status_patches
                .iter()
                .any(|p| p.status == Some(Status::Consensus) && p.rating == Some(5)),
            "should write Consensus with rating=5"
        );

        // Log should mention adversarial
        assert!(
            logs.iter()
                .any(|line| line.contains("adversarial second review")),
            "log should mention adversarial second review"
        );
        assert!(
            logs.iter()
                .any(|line| line.contains("CONSENSUS") && line.contains("adversarial confirmed")),
            "log should confirm adversarial consensus"
        );

        assert_eq!(
            checkpoints,
            vec![
                "round-1-implementation: Changes body".to_string(),
                "consensus-round-1".to_string()
            ]
        );
    }

    #[test]
    fn dual_agent_5_5_adversarial_needs_changes_continues_loop() {
        let config = test_config_with_rounds(false, 2);
        let baseline_files = HashSet::new();
        let mut logs = Vec::<String>::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let mut appended: Vec<(u32, String, String)> = Vec::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "First review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Round 1: reviewer APPROVED 5/5, adversarial NEEDS_CHANGES
        // Round 2: reviewer APPROVED 4/5, normal consensus → CONSENSUS
        let mut read_statuses = VecDeque::from([
            test_loop_status_with_rating(Status::Approved, 1, None, Some(5), &config),
            test_loop_status_with_rating(
                Status::NeedsChanges,
                1,
                Some("missing edge case test"),
                Some(3),
                &config,
            ),
            test_loop_status_with_rating(Status::Approved, 2, None, Some(4), &config),
            test_loop_status(Status::Consensus, 2, None, &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |_, _, _| {},
            |message, _| logs.push(message.to_string()),
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |round, phase, summary, _| {
                appended.push((round, phase.to_string(), summary.to_string()));
            },
            false,
        );

        assert!(result, "should reach consensus in round 2");

        // Log should show adversarial found issues
        assert!(
            logs.iter()
                .any(|line| line.contains("Adversarial review found issues")),
            "log should show adversarial found issues"
        );

        // History should have adversarial-review entry in round 1
        assert!(
            appended.iter().any(|(round, phase, summary)| {
                *round == 1
                    && phase == "adversarial-review"
                    && summary.contains("NEEDS_CHANGES (adversarial)")
            }),
            "should append adversarial-review history entry; got: {appended:?}"
        );

        // Round 2 should use normal consensus flow (not adversarial)
        // The round 2 consensus call should be to the implementer (not reviewer)
        let round2_consensus_calls: Vec<_> = prompts
            .iter()
            .enumerate()
            .filter(|(_, (_, prompt))| {
                prompt.contains("The reviewer has APPROVED your implementation")
            })
            .collect();
        assert!(
            !round2_consensus_calls.is_empty(),
            "round 2 should use the normal consensus prompt (rating 4, not 5)"
        );
    }

    #[test]
    fn single_agent_5_5_auto_consensus() {
        let config = test_config_with_rounds(true, 3);
        let baseline_files = HashSet::new();
        let mut checkpoints = Vec::<String>::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let mut appended: Vec<(u32, String, String)> = Vec::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Reviewer returns APPROVED with rating=5
        let mut read_statuses = VecDeque::from([test_loop_status_with_rating(
            Status::Approved,
            1,
            None,
            Some(5),
            &config,
        )]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |message, _, _| checkpoints.push(message.to_string()),
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |round, phase, summary, _| {
                appended.push((round, phase.to_string(), summary.to_string()));
            },
            false,
        );

        assert!(result);

        // Only 2 agent calls: implementer + reviewer (no consensus/adversarial call)
        assert_eq!(
            prompts.len(),
            2,
            "single-agent 5/5 should have only 2 agent calls (implementer + reviewer)"
        );

        // Status should include Consensus with rating=5
        assert!(
            status_patches
                .iter()
                .any(|p| p.status == Some(Status::Consensus) && p.rating == Some(5)),
            "should write Consensus with rating=5"
        );

        // Log should mention auto-consensus
        assert!(
            logs.iter().any(|line| line.contains("auto-consensus")),
            "log should mention auto-consensus"
        );

        // History should have auto-consensus entry
        assert!(
            appended.iter().any(|(_, phase, summary)| {
                phase == "consensus" && summary.contains("AUTO-CONSENSUS (single-agent 5/5)")
            }),
            "should append auto-consensus history entry; got: {appended:?}"
        );

        assert_eq!(
            checkpoints,
            vec![
                "round-1-implementation: Changes body".to_string(),
                "consensus-round-1".to_string()
            ]
        );
    }

    #[test]
    fn dual_agent_approved_rating_4_uses_normal_consensus() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Reviewer returns APPROVED with rating=4, then normal consensus → CONSENSUS
        let mut read_statuses = VecDeque::from([
            test_loop_status_with_rating(Status::Approved, 1, None, Some(4), &config),
            test_loop_status(Status::Consensus, 1, None, &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        // 3 agent calls: implementer, reviewer, consensus (implementer)
        assert_eq!(prompts.len(), 3, "rating 4 should use normal 3-call flow");
        assert_eq!(
            prompts[2].0,
            Agent::Claude,
            "third call (consensus) should use implementer agent, not reviewer"
        );
        // Should use normal consensus prompt, not adversarial
        assert!(
            prompts[2]
                .1
                .contains("The reviewer has APPROVED your implementation"),
            "should use consensus prompt for rating 4"
        );
        assert!(
            !prompts[2].1.contains("find what the first reviewer missed"),
            "should NOT use adversarial prompt for rating 4"
        );
    }

    #[test]
    fn dual_agent_approved_no_rating_uses_normal_consensus() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let mut prompts = Vec::<(Agent, String)>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "Review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Reviewer returns APPROVED with no rating, then normal consensus → CONSENSUS
        let mut read_statuses = VecDeque::from([
            test_loop_status(Status::Approved, 1, None, &config), // rating: None
            test_loop_status(Status::Consensus, 1, None, &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |agent, prompt, _| {
                prompts.push((agent, prompt.to_string()));
                Ok(())
            },
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(result);
        // 3 agent calls: implementer, reviewer, consensus (implementer)
        assert_eq!(
            prompts.len(),
            3,
            "no-rating APPROVED should use normal 3-call flow"
        );
        assert_eq!(
            prompts[2].0,
            Agent::Claude,
            "third call (consensus) should use implementer agent"
        );
        // Should use normal consensus prompt
        assert!(
            prompts[2]
                .1
                .contains("The reviewer has APPROVED your implementation"),
            "should use consensus prompt for no-rating"
        );
    }

    #[test]
    fn dual_agent_5_5_adversarial_stale_timestamp_falls_back_to_needs_changes() {
        let config = test_config_with_rounds(false, 2);
        let baseline_files = HashSet::new();
        let mut logs = Vec::<String>::new();
        let mut status_patches = Vec::<StatusPatch>::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "First review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Round 1: reviewer APPROVED 5/5, adversarial returns stale status, then corrected NeedsChanges
        // Round 2: max rounds
        let mut read_statuses = VecDeque::from([
            test_loop_status_with_rating(Status::Approved, 1, None, Some(5), &config),
            stale_loop_status(Status::Approved, 1, None, &config), // adversarial stale
            test_loop_status(
                Status::NeedsChanges,
                1,
                Some(STALE_TIMESTAMP_REASON),
                &config,
            ), // corrected
            test_loop_status(Status::NeedsChanges, 2, Some("still failing"), &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |message, _| logs.push(message.to_string()),
            |patch, _| status_patches.push(patch),
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |_, _, _, _| {},
            false,
        );

        assert!(!result, "should not reach consensus (max rounds)");

        // Verify stale detection was logged
        assert!(
            logs.iter()
                .any(|line| line.contains("Stale status after adversarial review")),
            "should log stale detection for adversarial review"
        );

        // Verify NeedsChanges fallback was written with stale reason
        assert!(
            status_patches.iter().any(|patch| {
                patch.status == Some(Status::NeedsChanges)
                    && patch.reason.as_deref() == Some(STALE_TIMESTAMP_REASON)
            }),
            "should write NeedsChanges with stale timestamp reason"
        );
    }

    #[test]
    fn dual_agent_5_5_adversarial_history_tracking() {
        let config = test_config_with_rounds(false, 3);
        let baseline_files = HashSet::new();
        let mut appended: Vec<(u32, String, String)> = Vec::new();
        let state_files = HashMap::from([
            ("task.md".to_string(), "Task body".to_string()),
            ("plan.md".to_string(), "Plan body".to_string()),
            ("review.md".to_string(), "First review body".to_string()),
            ("changes.md".to_string(), "Changes body".to_string()),
        ]);
        // Reviewer APPROVED 5/5, adversarial also APPROVED
        let mut read_statuses = VecDeque::from([
            test_loop_status_with_rating(Status::Approved, 1, None, Some(5), &config),
            test_loop_status_with_rating(Status::Approved, 1, None, Some(5), &config),
        ]);

        let result = implementation_loop_internal(
            &config,
            &baseline_files,
            |_, _, _| Ok(()),
            |_, _, _| {},
            |_, _| {},
            |_, _| {},
            |name, _| state_files.get(name).cloned().unwrap_or_default(),
            |_| {
                read_statuses
                    .pop_front()
                    .expect("test should provide enough status reads")
            },
            |_, _| "(test diff)".to_string(),
            || TEST_TIMESTAMP.to_string(),
            |_, _| String::new(),
            |round, phase, summary, _| {
                appended.push((round, phase.to_string(), summary.to_string()));
            },
            false,
        );

        assert!(result);
        // Should have: implementation, review, adversarial-review, consensus
        assert_eq!(
            appended.len(),
            4,
            "should have 4 appends: implementation, review, adversarial-review, consensus; got: {appended:?}"
        );
        assert_eq!(appended[0].1, "implementation");
        assert_eq!(appended[1].1, "review");
        assert!(appended[1].2.contains("APPROVED"));
        assert_eq!(appended[2].1, "adversarial-review");
        assert!(appended[2].2.contains("APPROVED (adversarial)"));
        assert_eq!(appended[3].1, "consensus");
        assert!(appended[3].2.contains("CONSENSUS (adversarial confirmed)"));
    }
}
