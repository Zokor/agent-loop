use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::{
    agent_registry::OutputFormat,
    config::{Agent, Config},
    error::AgentLoopError,
    prompts::AgentRole,
    state::{
        AgentCallMeta, FALLBACK_PHASE, FALLBACK_WORKFLOW,
        begin_transcript_entry, complete_transcript_entry,
        TranscriptHandle, TranscriptCompletionStatus,
        log, timestamp,
    },
};

pub(crate) fn resolve_command(
    agent: &Agent,
    prompt: &str,
    config: &Config,
    system_prompt: Option<&str>,
    session_id: Option<&str>,
    role: Option<AgentRole>,
) -> (&'static str, Vec<String>) {
    let spec = agent.spec();

    // Layer 1: Registry command builder — base CLI args (agent-specific syntax).
    let mut args = (spec.command_builder)(prompt, agent.model());

    // Layer 2: Config-driven overrides applied after base args.

    // Session resumption: agent-specific resume flags.
    if let Some(sid) = session_id {
        match agent.name() {
            "claude" => {
                // --resume <id> must precede -p
                args.insert(0, sid.to_string());
                args.insert(0, "--resume".to_string());
            }
            "codex" => {
                // Replace `exec` with `exec resume <id>` — prompt is already the last arg.
                if let Some(pos) = args.iter().position(|a| a == "exec") {
                    args.insert(pos + 1, sid.to_string());
                    args.insert(pos + 1, "resume".to_string());
                }
            }
            _ => {}
        }
    }

    // System prompt delivery — must happen while the prompt text is still the
    // last arg from command_builder.  Claude uses --append-system-prompt; other
    // agents (e.g. Codex) don't support that flag, so the manifest/preamble is
    // prepended directly to the prompt text.
    if let Some(sp) = system_prompt {
        if agent.name() == "claude" {
            args.push("--append-system-prompt".to_string());
            args.push(sp.to_string());
        } else if !sp.is_empty() {
            // Prepend to the prompt arg (last element from command_builder).
            if let Some(last) = args.last_mut() {
                *last = format!("{sp}\n\n{last}");
            }
            // Sanity-check the command_builder contract in debug builds.
            debug_assert!(
                args.iter().any(|arg| arg.contains(prompt)),
                "system prompt injection lost prompt text; check command_builder contract"
            );
        }
    }

    // Permission / sandbox policy — added after system prompt so that sandbox
    // flags never accidentally become the target of prepend logic above.
    if agent.name() == "claude" {
        let planner_plan_mode =
            role == Some(AgentRole::Planner) && config.planner_permission_mode == "plan";
        if planner_plan_mode {
            args.push("--permission-mode".to_string());
            args.push("plan".to_string());
        } else if config.claude_full_access {
            args.push("--dangerously-skip-permissions".to_string());
        } else {
            args.push("--allowedTools".to_string());
            // Reviewer role gets read-only tools by default.
            let tools = if role == Some(AgentRole::Reviewer) {
                config.reviewer_allowed_tools.clone()
            } else {
                config.claude_allowed_tools.clone()
            };
            args.push(tools);
        }
    }
    if agent.name() == "codex" {
        if config.codex_full_access {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        } else {
            args.push("--full-auto".to_string());
        }
    }

    (spec.binary, args)
}

const SUPERVISOR_TICK: Duration = Duration::from_secs(1);
const FORCE_KILL_GRACE_MS: u64 = 2_000;
const RESPONSE_BLOCK_MAX_LINES: usize = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub agent_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd_micros: u64,
}

impl UsageSnapshot {
    pub fn is_zero(self) -> bool {
        self.agent_calls == 0
            && self.input_tokens == 0
            && self.output_tokens == 0
            && self.total_tokens == 0
            && self.cost_usd_micros == 0
    }

    pub fn saturating_add(self, rhs: Self) -> Self {
        Self {
            agent_calls: self.agent_calls.saturating_add(rhs.agent_calls),
            input_tokens: self.input_tokens.saturating_add(rhs.input_tokens),
            output_tokens: self.output_tokens.saturating_add(rhs.output_tokens),
            total_tokens: self.total_tokens.saturating_add(rhs.total_tokens),
            cost_usd_micros: self.cost_usd_micros.saturating_add(rhs.cost_usd_micros),
        }
    }

    pub fn saturating_sub(self, rhs: Self) -> Self {
        Self {
            agent_calls: self.agent_calls.saturating_sub(rhs.agent_calls),
            input_tokens: self.input_tokens.saturating_sub(rhs.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(rhs.output_tokens),
            total_tokens: self.total_tokens.saturating_sub(rhs.total_tokens),
            cost_usd_micros: self.cost_usd_micros.saturating_sub(rhs.cost_usd_micros),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AgentUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cost_usd_micros: u64,
}

static AGENT_CALLS_TOTAL: AtomicU64 = AtomicU64::new(0);
static INPUT_TOKENS_TOTAL: AtomicU64 = AtomicU64::new(0);
static OUTPUT_TOKENS_TOTAL: AtomicU64 = AtomicU64::new(0);
static TOTAL_TOKENS_TOTAL: AtomicU64 = AtomicU64::new(0);
static COST_USD_MICROS_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn usage_snapshot() -> UsageSnapshot {
    UsageSnapshot {
        agent_calls: AGENT_CALLS_TOTAL.load(Ordering::Relaxed),
        input_tokens: INPUT_TOKENS_TOTAL.load(Ordering::Relaxed),
        output_tokens: OUTPUT_TOKENS_TOTAL.load(Ordering::Relaxed),
        total_tokens: TOTAL_TOKENS_TOTAL.load(Ordering::Relaxed),
        cost_usd_micros: COST_USD_MICROS_TOTAL.load(Ordering::Relaxed),
    }
}

fn record_agent_invocation(usage: AgentUsage) {
    AGENT_CALLS_TOTAL.fetch_add(1, Ordering::Relaxed);
    if usage.input_tokens > 0 {
        INPUT_TOKENS_TOTAL.fetch_add(usage.input_tokens, Ordering::Relaxed);
    }
    if usage.output_tokens > 0 {
        OUTPUT_TOKENS_TOTAL.fetch_add(usage.output_tokens, Ordering::Relaxed);
    }
    if usage.total_tokens > 0 {
        TOTAL_TOKENS_TOTAL.fetch_add(usage.total_tokens, Ordering::Relaxed);
    }
    if usage.cost_usd_micros > 0 {
        COST_USD_MICROS_TOTAL.fetch_add(usage.cost_usd_micros, Ordering::Relaxed);
    }
}

/// Truncate `output` to at most `max_lines` lines using a ring-buffer approach.
/// Returns `(kept_output, dropped_count)`.
fn truncate_response_lines(output: &str, max_lines: usize) -> (String, usize) {
    use std::collections::VecDeque;

    let mut ring = VecDeque::with_capacity(max_lines);
    let mut total_lines = 0usize;

    for line in output.lines() {
        if ring.len() == max_lines {
            ring.pop_front();
        }
        ring.push_back(line);
        total_lines += 1;
    }

    let dropped = total_lines.saturating_sub(max_lines);
    let kept: Vec<&str> = ring.into_iter().collect();
    (kept.join("\n"), dropped)
}

fn append_response_block(agent: &Agent, output: &str, config: &Config) -> std::io::Result<()> {
    if output.trim().is_empty() {
        return Ok(());
    }

    let log_path = config.state_dir.join("log.txt");
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let (kept_output, dropped) = truncate_response_lines(output, RESPONSE_BLOCK_MAX_LINES);
    if dropped > 0 {
        let total = dropped + RESPONSE_BLOCK_MAX_LINES;
        let _ = log(
            &format!(
                "⚠ Response block truncated: {total} lines -> {RESPONSE_BLOCK_MAX_LINES} lines"
            ),
            config,
        );
    }

    let truncation_marker = if dropped > 0 {
        format!("[... {dropped} lines truncated ...]\n")
    } else {
        String::new()
    };

    let separator = "─".repeat(60);
    let block = format!(
        "\n{separator}\n[{}] {} response:\n{separator}\n{truncation_marker}{kept_output}\n{separator}\n\n",
        timestamp(),
        agent
    );

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    file.write_all(block.as_bytes())
}

fn assistant_text_from_stream_event(value: &serde_json::Value) -> Option<String> {
    let content = value.get("message")?.get("content")?.as_array()?;
    let mut text = String::new();

    for block in content {
        if block.get("type").and_then(|value| value.as_str()) == Some("text")
            && let Some(segment) = block.get("text").and_then(|value| value.as_str())
        {
            text.push_str(segment);
        }
    }

    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn extract_claude_stream_json_text(output: &str) -> Option<String> {
    let mut result_text = None;
    let mut assistant_text = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        match value.get("type").and_then(|value| value.as_str()) {
            Some("result") => {
                if let Some(text) = value.get("result").and_then(|value| value.as_str()) {
                    result_text = Some(text.to_string());
                }
            }
            Some("assistant") => {
                if let Some(text) = assistant_text_from_stream_event(&value) {
                    assistant_text = Some(text);
                }
            }
            _ => {}
        }
    }

    result_text.or(assistant_text)
}

fn parse_u64_field(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text.parse::<u64>().ok(),
        _ => None,
    }
}

fn parse_cost_usd_micros(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64().and_then(|v| {
            if v.is_finite() && v >= 0.0 {
                Some((v * 1_000_000.0).round() as u64)
            } else {
                None
            }
        }),
        serde_json::Value::String(text) => text.parse::<f64>().ok().and_then(|v| {
            if v.is_finite() && v >= 0.0 {
                Some((v * 1_000_000.0).round() as u64)
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn update_usage_from_object(
    map: &serde_json::Map<String, serde_json::Value>,
    usage: &mut AgentUsage,
) {
    if let Some(value) = map.get("input_tokens").and_then(parse_u64_field) {
        usage.input_tokens = value;
    }
    if let Some(value) = map.get("output_tokens").and_then(parse_u64_field) {
        usage.output_tokens = value;
    }
    if let Some(value) = map.get("total_tokens").and_then(parse_u64_field) {
        usage.total_tokens = value;
    }

    for cost_key in ["total_cost_usd", "cost_usd", "estimated_cost_usd"] {
        if let Some(value) = map.get(cost_key).and_then(parse_cost_usd_micros) {
            usage.cost_usd_micros = value;
        }
    }
}

fn extract_stream_json_usage(output: &str) -> AgentUsage {
    let mut usage = AgentUsage::default();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        if let Some(root) = value.as_object() {
            update_usage_from_object(root, &mut usage);
            if let Some(usage_obj) = root.get("usage").and_then(|v| v.as_object()) {
                update_usage_from_object(usage_obj, &mut usage);
            }
            if let Some(message_obj) = root.get("message").and_then(|v| v.as_object()) {
                update_usage_from_object(message_obj, &mut usage);
                if let Some(message_usage) = message_obj.get("usage").and_then(|v| v.as_object()) {
                    update_usage_from_object(message_usage, &mut usage);
                }
            }
            if let Some(result_obj) = root.get("result").and_then(|v| v.as_object()) {
                update_usage_from_object(result_obj, &mut usage);
                if let Some(result_usage) = result_obj.get("usage").and_then(|v| v.as_object()) {
                    update_usage_from_object(result_usage, &mut usage);
                }
            }
        }
    }

    if usage.total_tokens == 0 && (usage.input_tokens > 0 || usage.output_tokens > 0) {
        usage.total_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
    }

    usage
}

/// Extract the `session_id` from Claude's stream-json output.
///
/// The session_id typically appears in the `result` event as a top-level field.
fn extract_claude_session_id(output: &str) -> Option<String> {
    for line in output.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        if let Some(sid) = value.get("session_id").and_then(|v| v.as_str())
            && !sid.is_empty()
        {
            return Some(sid.to_string());
        }

        // Also check nested in result object
        if let Some(result) = value.get("result")
            && let Some(sid) = result.get("session_id").and_then(|v| v.as_str())
            && !sid.is_empty()
        {
            return Some(sid.to_string());
        }
    }

    None
}

/// Extract a `session_id` from Codex `--json` NDJSON output.
///
/// Codex may emit a `session_id` field in its final status/result event.
fn extract_codex_session_id(output: &str) -> Option<String> {
    for line in output.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        if let Some(sid) = value.get("session_id").and_then(|v| v.as_str())
            && !sid.is_empty()
        {
            return Some(sid.to_string());
        }
    }

    None
}

fn session_file_path(config: &Config, session_key: &str) -> std::path::PathBuf {
    config.state_dir.join(format!("{session_key}_session_id"))
}

fn read_session_id(config: &Config, session_key: &str) -> Option<String> {
    let path = session_file_path(config, session_key);
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_session_id(config: &Config, session_key: &str, session_id: &str) {
    let path = session_file_path(config, session_key);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, session_id);
}

fn clear_session_id(config: &Config, session_key: &str) {
    let path = session_file_path(config, session_key);
    let _ = fs::remove_file(path);
}

/// Error patterns that indicate a stale or invalid session ID.
const SESSION_ERROR_PATTERNS: &[&str] = &[
    "session not found",
    "session has expired",
    "session_id is invalid",
    "invalid session",
    "could not resume session",
    "failed to resume",
    "no such session",
    "session does not exist",
    "resume failed",
];

/// Check whether a single line of text matches any session error pattern.
fn line_matches_session_error(line: &str) -> bool {
    let lower = line.to_lowercase();
    SESSION_ERROR_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Detect session resume errors in the raw agent output even when the process
/// exits 0.
///
/// Unlike the previous implementation which scanned normalized assistant text,
/// this restricts pattern matching to structured error channels to avoid
/// false-positives when the assistant's *content* happens to discuss sessions:
///
/// - **Non-JSON lines** (stderr, plain error output): checked for patterns.
/// - **JSON events with `type: "error"` or `type: "system"`**: checked.
/// - **`type: "assistant"` / `type: "result"` content**: NOT checked, since
///   these contain the agent's actual response text.
fn has_session_resume_error(raw_output: &str) -> bool {
    for line in raw_output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Non-JSON lines (typically stderr output): check for patterns.
        if !trimmed.starts_with('{') {
            if line_matches_session_error(trimmed) {
                return true;
            }
            continue;
        }

        // JSON lines: only check error/system event types, skip assistant content.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // Malformed JSON line — treat like plain text.
            if line_matches_session_error(trimmed) {
                return true;
            }
            continue;
        };

        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            // Error and system events are genuine error channels.
            "error" | "system" => {
                // Check the entire JSON line for error patterns.
                if line_matches_session_error(trimmed) {
                    return true;
                }
            }
            // Assistant/result events contain the agent's response text —
            // skip these to avoid false-positives.
            "assistant" | "result" | "message" => {}
            // Unknown event types: check conservatively.
            _ => {
                if line_matches_session_error(trimmed) {
                    return true;
                }
            }
        }
    }
    false
}

/// Extract the final message text from Codex `--json` NDJSON output.
///
/// Codex emits newline-delimited JSON events. The final assistant message
/// is typically in an event with `"type": "message"` and `"role": "assistant"`.
/// Falls back to the raw output if no structured message is found.
fn extract_codex_json_text(output: &str) -> Option<String> {
    fn extract_text_field(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(content) => {
                if content.trim().is_empty() {
                    None
                } else {
                    Some(content.to_string())
                }
            }
            serde_json::Value::Array(content_arr) => {
                let mut text = String::new();
                for block in content_arr {
                    if let Some(s) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(s);
                    } else if let Some(s) = block.as_str() {
                        text.push_str(s);
                    }
                }
                if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        }
    }

    let mut last_text = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Legacy/newer Codex JSON event with top-level assistant content.
        if event_type == "message" {
            if let Some(content) = value.get("content").and_then(extract_text_field) {
                last_text = Some(content);
            }
            continue;
        }

        // Current Codex NDJSON commonly emits item lifecycle events where the
        // user-visible model reply is in:
        // {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
        if event_type.starts_with("item.")
            && let Some(item) = value.get("item").and_then(|v| v.as_object())
        {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(item_type, "agent_message" | "assistant_message" | "message") {
                if let Some(text) = item
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                {
                    last_text = Some(text.to_string());
                    continue;
                }

                if let Some(content) = item.get("content").and_then(extract_text_field) {
                    last_text = Some(content);
                    continue;
                }

                if let Some(content) = item
                    .get("message")
                    .and_then(|v| v.get("content"))
                    .and_then(extract_text_field)
                {
                    last_text = Some(content);
                }
            }
        }
    }

    last_text
}

/// Extract usage data from Codex `--json` NDJSON output.
fn extract_codex_json_usage(output: &str) -> AgentUsage {
    let mut usage = AgentUsage::default();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        if let Some(root) = value.as_object() {
            update_usage_from_object(root, &mut usage);
            if let Some(usage_obj) = root.get("usage").and_then(|v| v.as_object()) {
                update_usage_from_object(usage_obj, &mut usage);
            }
        }
    }

    if usage.total_tokens == 0 && (usage.input_tokens > 0 || usage.output_tokens > 0) {
        usage.total_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
    }

    usage
}

fn normalize_agent_output(agent: &Agent, output: String) -> String {
    match agent.spec().output_format {
        OutputFormat::ClaudeStreamJson => {
            extract_claude_stream_json_text(&output).unwrap_or(output)
        }
        OutputFormat::PlainText => {
            // For PlainText agents that emit structured JSON (e.g. Codex with --json),
            // attempt to extract the message text from the JSON envelope.
            extract_codex_json_text(&output).unwrap_or(output)
        }
    }
}

fn terminate_for_timeout(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child was spawned in its own process group (setpgid(0,0)),
        // so child.id() == pgid. Send SIGTERM to the entire group via -pgid.
        let pgid = child.id() as libc::pid_t;
        let rc = unsafe { libc::kill(-pgid, libc::SIGTERM) };
        if rc == -1 {
            let _ = child.kill();
        }
    }

    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

fn spawn_reader_thread<R>(
    mut reader: R,
    output: Arc<Mutex<Vec<u8>>>,
    last_output_ms: Arc<AtomicU64>,
    start: Instant,
    to_stderr: bool,
) -> JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];

        loop {
            let read_count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(value) => value,
                Err(_) => break,
            };

            let bytes = &buffer[..read_count];
            if to_stderr {
                let mut stderr = std::io::stderr().lock();
                let _ = stderr.write_all(bytes);
                let _ = stderr.flush();
            } else {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(bytes);
                let _ = stdout.flush();
            }

            if let Ok(mut combined) = output.lock() {
                combined.extend_from_slice(bytes);
            }

            last_output_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
    })
}

fn join_reader_thread(handle: Option<JoinHandle<()>>) {
    if let Some(join_handle) = handle {
        let _ = join_handle.join();
    }
}

#[cfg(test)]
pub(crate) fn run_agent(
    agent: &Agent,
    prompt: &str,
    config: &Config,
    system_prompt: Option<&str>,
) -> Result<String, AgentLoopError> {
    run_agent_inner(agent, prompt, config, system_prompt, None, None, None)
}

/// Run an agent with optional session persistence and explicit role.
///
/// When `session_key` is `Some("implementer-claude")` etc. and the
/// agent supports session resume with persistence enabled, the function:
/// 1. Reads the previous session_id from the state dir (if any)
/// 2. Passes `--resume <session_id>` to continue the session
/// 3. After a successful run, extracts the new session_id and stores it
/// 4. On failure with `--resume`, clears the session file so the next call starts fresh
///
/// `role` is threaded through to `resolve_command` for role-based tool selection.
pub fn run_agent_with_session(
    agent: &Agent,
    prompt: &str,
    config: &Config,
    system_prompt: Option<&str>,
    session_key: Option<&str>,
    role: Option<AgentRole>,
    meta: Option<&AgentCallMeta>,
) -> Result<String, AgentLoopError> {
    run_agent_inner(
        agent,
        prompt,
        config,
        system_prompt,
        session_key,
        role,
        meta,
    )
}

/// Select the effective Claude effort level based on the agent's role.
///
/// Role-specific levels (`implementer_effort_level`, `reviewer_effort_level`)
/// take precedence, falling back to the generic `claude_effort_level`.
fn effective_effort_level(role: Option<AgentRole>, config: &Config) -> Option<&str> {
    match role {
        Some(AgentRole::Implementer) => config
            .implementer_effort_level
            .as_deref()
            .or(config.claude_effort_level.as_deref()),
        Some(AgentRole::Reviewer) => config
            .reviewer_effort_level
            .as_deref()
            .or(config.claude_effort_level.as_deref()),
        _ => config.claude_effort_level.as_deref(),
    }
}

fn run_agent_inner(
    agent: &Agent,
    prompt: &str,
    config: &Config,
    system_prompt: Option<&str>,
    session_key: Option<&str>,
    role: Option<AgentRole>,
    caller_meta: Option<&AgentCallMeta>,
) -> Result<String, AgentLoopError> {
    let _ = log(&format!("▶ Running {agent}..."), config);

    // Session persistence: read existing session_id if applicable.
    // Uses the registry's `supports_session_resume` capability flag; the
    // per-agent config knob controls whether to actually persist.  Unknown
    // agents with `supports_session_resume=true` default to enabled.
    let session_persistence_enabled = if !agent.spec().supports_session_resume {
        false
    } else {
        match agent.name() {
            "claude" => config.claude_session_persistence,
            "codex" => config.codex_session_persistence,
            _ => true, // resumable agent with no dedicated config — default on
        }
    };
    let session_id = if agent.spec().supports_session_resume
        && session_persistence_enabled
        && let Some(key) = session_key
    {
        read_session_id(config, key)
    } else {
        None
    };

    if session_id.is_some() {
        let _ = log(
            &format!("  ↳ Resuming existing {} session", agent.name()),
            config,
        );
    }

    let (command, args) = resolve_command(
        agent,
        prompt,
        config,
        system_prompt,
        session_id.as_deref(),
        role,
    );
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(&config.project_dir)
        // Agent invocations are non-interactive; do not let children read caller TTY input.
        // This keeps Ctrl+C handling at the loop level predictable.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDECODE");

    // Claude-specific environment variable pass-through.
    if agent.name() == "claude" {
        cmd.env("CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR", "1");

        if let Some(effort) = effective_effort_level(role, config) {
            cmd.env("CLAUDE_CODE_EFFORT_LEVEL", effort);
        }
        if let Some(max) = config.claude_max_output_tokens {
            cmd.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", max.to_string());
        }
        if let Some(max) = config.claude_max_thinking_tokens {
            cmd.env("MAX_THINKING_TOKENS", max.to_string());
        }
    }

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Build agent call metadata for transcript logging.
    let meta = if let Some(m) = caller_meta {
        m.clone()
    } else {
        let role_str = match role {
            Some(AgentRole::Implementer) => "implementer",
            Some(AgentRole::Reviewer) => "reviewer",
            Some(AgentRole::Planner) => "planner",
            None => "unknown",
        };
        AgentCallMeta {
            workflow: FALLBACK_WORKFLOW.to_string(),
            phase: FALLBACK_PHASE.to_string(),
            round: 0,
            agent_name: agent.name().to_string(),
            role: role_str.to_string(),
            session_hint: session_key.map(ToString::to_string),
        }
    };

    // Phase 1: log the prompt before the agent starts.
    let handle: Option<TranscriptHandle> =
        begin_transcript_entry(config, &meta, prompt, system_prompt);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let reason = format!("{agent} failed to start: {err}");
            let _ = log(&format!("⚠ {reason}"), config);
            complete_transcript_entry(
                handle.as_ref(),
                TranscriptCompletionStatus::Failed,
                Some(&reason),
                "",
            );
            return Err(AgentLoopError::Agent(reason));
        }
    };
    let start = Instant::now();
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let last_output_ms = Arc::new(AtomicU64::new(start.elapsed().as_millis() as u64));

    let stdout_handle = child.stdout.take().map(|stdout| {
        spawn_reader_thread(
            stdout,
            Arc::clone(&output),
            Arc::clone(&last_output_ms),
            start,
            false,
        )
    });
    let stderr_handle = child.stderr.take().map(|stderr| {
        spawn_reader_thread(
            stderr,
            Arc::clone(&output),
            Arc::clone(&last_output_ms),
            start,
            true,
        )
    });

    let mut status = None;
    let mut timed_out = false;
    let mut timed_out_at_ms = None;
    let mut force_kill_attempted = false;
    let mut run_error = None::<String>;
    let mut interrupted = false;

    loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                status = Some(exit_status);
                break;
            }
            Ok(None) => {
                // Check for interrupt signal before anything else.
                if !interrupted && crate::interrupt::is_interrupted() {
                    let _ = log("Interrupted by signal — terminating child process", config);
                    terminate_for_timeout(&mut child);
                    interrupted = true;
                    timed_out_at_ms = Some(start.elapsed().as_millis() as u64);
                }

                let now_ms = start.elapsed().as_millis() as u64;
                let idle_ms = now_ms.saturating_sub(last_output_ms.load(Ordering::Relaxed));
                if !timed_out
                    && !interrupted
                    && idle_ms > config.timeout_seconds.saturating_mul(1000)
                {
                    let _ = log(
                        &format!("⏱️ Idle timeout: no output for {}s", config.timeout_seconds),
                        config,
                    );
                    terminate_for_timeout(&mut child);
                    timed_out = true;
                    timed_out_at_ms = Some(now_ms);
                }

                if (timed_out || interrupted)
                    && !force_kill_attempted
                    && let Some(timeout_started_ms) = timed_out_at_ms
                    && now_ms.saturating_sub(timeout_started_ms) >= FORCE_KILL_GRACE_MS
                {
                    #[cfg(unix)]
                    {
                        let pgid = child.id() as libc::pid_t;
                        let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    }
                    let _ = child.kill();
                    force_kill_attempted = true;
                }
                thread::sleep(SUPERVISOR_TICK);
            }
            Err(err) => {
                let reason = format!("{agent} process error: {err}");
                let _ = log(&format!("⚠ {reason}"), config);
                run_error = Some(reason);
                break;
            }
        }
    }

    if status.is_none() && run_error.is_none() {
        match child.wait() {
            Ok(exit_status) => status = Some(exit_status),
            Err(err) => {
                let reason = format!("{agent} wait error: {err}");
                let _ = log(&format!("⚠ {reason}"), config);
                run_error = Some(reason);
            }
        }
    }

    if run_error.is_some() {
        // Ensure pipes close so reader threads can finish even after try_wait/wait failures.
        let _ = child.kill();
        let _ = child.wait();
    }

    join_reader_thread(stdout_handle);
    join_reader_thread(stderr_handle);

    let raw_bytes = output
        .lock()
        .map(|mut guard| std::mem::take(&mut *guard))
        .unwrap_or_default();
    let combined_output = String::from_utf8_lossy(&raw_bytes).into_owned();
    let usage = match agent.spec().output_format {
        OutputFormat::ClaudeStreamJson => extract_stream_json_usage(&combined_output),
        OutputFormat::PlainText => extract_codex_json_usage(&combined_output),
    };
    record_agent_invocation(usage);

    // Session persistence: extract and store session_id from agent output.
    if agent.spec().supports_session_resume
        && session_persistence_enabled
        && let Some(key) = session_key
    {
        let sid = match agent.name() {
            "claude" => extract_claude_session_id(&combined_output),
            "codex" => extract_codex_session_id(&combined_output),
            _ => None,
        };
        if let Some(sid) = sid {
            let _ = log(
                &format!(
                    "  ↳ Captured {} session_id: {}",
                    agent.name(),
                    &sid[..sid.len().min(12)]
                ),
                config,
            );
            write_session_id(config, key, &sid);
        }
    }

    // Check for session resume errors in the raw output BEFORE normalization
    // strips structural information.  This scans error/system JSON events and
    // non-JSON (stderr) lines only — assistant content is excluded to avoid
    // false-positives when the agent merely discusses sessions.
    let raw_has_resume_error = session_id.is_some() && has_session_resume_error(&combined_output);

    let normalized_output = normalize_agent_output(agent, combined_output);

    // Finalize per-attempt transcript and response block on every exit path.

    // Process-level failures (interrupted, timed_out, run_error, missing exit status)
    // are handled first — these take priority over stale-session retry logic.
    let is_process_failure = interrupted || timed_out || run_error.is_some() || status.is_none();
    if is_process_failure {
        let failure_reason = if interrupted {
            "Interrupted by signal".to_string()
        } else if timed_out {
            format!(
                "{agent} timed out after {}s of inactivity",
                config.timeout_seconds
            )
        } else if let Some(ref r) = run_error {
            r.clone()
        } else {
            format!("{agent} did not report an exit status")
        };

        complete_transcript_entry(
            handle.as_ref(),
            TranscriptCompletionStatus::Failed,
            Some(&failure_reason),
            &normalized_output,
        );
        if let Err(err) = append_response_block(agent, &normalized_output, config) {
            let _ = log(&format!("⚠ {agent} error: {err}"), config);
        }

        if interrupted {
            let _ = log(&format!("⚠ {failure_reason}"), config);
            return Err(AgentLoopError::Interrupted(failure_reason));
        }
        if timed_out {
            let _ = log(&format!("⚠ {failure_reason}"), config);
            return Err(AgentLoopError::Agent(failure_reason));
        }
        if run_error.is_some() {
            return Err(AgentLoopError::Agent(failure_reason));
        }
        // status.is_none()
        let _ = log(&format!("⚠ {failure_reason}"), config);
        return Err(AgentLoopError::Agent(failure_reason));
    }

    // At this point we have a valid exit status.
    let exit_status = status.unwrap();

    // Detect session resume failures — either via non-zero exit code OR
    // error patterns in the raw output's error channels (some agents exit 0
    // but signal resume failure via error events or stderr).
    let is_resume_failure = !exit_status.success() || raw_has_resume_error;

    if is_resume_failure
        && session_id.is_some()
        && let Some(key) = session_key
    {
        let role_str = match role {
            Some(AgentRole::Implementer) => "implementer",
            Some(AgentRole::Reviewer) => "reviewer",
            Some(AgentRole::Planner) => "planner",
            None => "unknown",
        };
        let _ = log(
            &format!(
                "Session resume failed for {role_str}/{}, retrying fresh",
                agent.name()
            ),
            config,
        );

        // Finalize this failed attempt before recursing for a fresh retry.
        let reason = format!("Session resume failed for {role_str}/{}", agent.name());
        complete_transcript_entry(
            handle.as_ref(),
            TranscriptCompletionStatus::Failed,
            Some(&reason),
            &normalized_output,
        );
        if let Err(err) = append_response_block(agent, &normalized_output, config) {
            let _ = log(&format!("⚠ {agent} error: {err}"), config);
        }

        clear_session_id(config, key);

        // Retry once without session resume.
        return run_agent_inner(
            agent,
            prompt,
            config,
            system_prompt,
            session_key,
            role,
            caller_meta,
        );
    }

    // Normal exit path: success or non-zero exit.
    let (completion_status, maybe_failure_reason) = if !exit_status.success() {
        let code = exit_status
            .code()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string());
        (
            TranscriptCompletionStatus::Failed,
            Some(format!("{agent} exited with code {code}")),
        )
    } else {
        (TranscriptCompletionStatus::Completed, None)
    };

    complete_transcript_entry(
        handle.as_ref(),
        completion_status,
        maybe_failure_reason.as_deref(),
        &normalized_output,
    );
    if let Err(err) = append_response_block(agent, &normalized_output, config) {
        let _ = log(&format!("⚠ {agent} error: {err}"), config);
    }

    if let Some(reason) = maybe_failure_reason {
        let _ = log(&format!("⚠ {reason}"), config);
        return Err(AgentLoopError::Agent(reason));
    }

    Ok(normalized_output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScopedEnvVar, TestProject, env_lock};

    fn new_project(timeout_seconds: u64) -> TestProject {
        TestProject::builder("agent_loop_agent_test")
            .timeout_seconds(timeout_seconds)
            .build()
    }

    #[test]
    fn resolve_command_builds_claude_with_full_access_by_default() {
        let project = new_project(5);
        let (command, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            None,
        );
        assert_eq!(command, "claude");
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--allowedTools".to_string()));
    }

    #[test]
    fn resolve_command_builds_claude_with_allowed_tools_when_full_access_disabled() {
        let mut project = new_project(5);
        project.config.claude_full_access = false;
        let (command, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            None,
        );
        assert_eq!(command, "claude");
        assert!(args.contains(&"--allowedTools".to_string()));
        assert!(args.contains(&crate::config::DEFAULT_CLAUDE_ALLOWED_TOOLS.to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn resolve_command_reviewer_role_uses_read_only_tools_when_full_access_disabled() {
        let mut project = new_project(5);
        project.config.claude_full_access = false;
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Reviewer),
        );
        assert!(args.contains(&"--allowedTools".to_string()));
        assert!(args.contains(&crate::config::DEFAULT_REVIEWER_ALLOWED_TOOLS.to_string()));
        // Reviewer should NOT get full implementer tools
        assert!(!args.contains(&crate::config::DEFAULT_CLAUDE_ALLOWED_TOOLS.to_string()));
    }

    #[test]
    fn resolve_command_reviewer_uses_full_access_when_enabled() {
        let project = new_project(5);
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Reviewer),
        );
        // With full_access=true (default), reviewer also uses dangerously flag
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--allowedTools".to_string()));
    }

    #[test]
    fn resolve_command_implementer_role_uses_full_tools_when_full_access_disabled() {
        let mut project = new_project(5);
        project.config.claude_full_access = false;
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Implementer),
        );
        assert!(args.contains(&"--allowedTools".to_string()));
        assert!(args.contains(&crate::config::DEFAULT_CLAUDE_ALLOWED_TOOLS.to_string()));
    }

    #[test]
    fn resolve_command_reviewer_custom_allowed_tools() {
        let mut project = new_project(5);
        project.config.claude_full_access = false;
        project.config.reviewer_allowed_tools = "Read,Grep".to_string();
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Reviewer),
        );
        assert!(args.contains(&"Read,Grep".to_string()));
    }

    #[test]
    fn resolve_command_codex_reviewer_uses_full_access_by_default() {
        let project = new_project(5);
        let (_, args) = resolve_command(
            &Agent::known("codex"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Reviewer),
        );
        // Codex with full_access=true (default) uses dangerously flag
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(!args.contains(&"--full-auto".to_string()));
    }

    #[test]
    fn resolve_command_codex_reviewer_uses_full_auto_when_full_access_disabled() {
        let mut project = new_project(5);
        project.config.codex_full_access = false;
        let (_, args) = resolve_command(
            &Agent::known("codex"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Reviewer),
        );
        // Codex should get --full-auto regardless of role when full_access=false
        assert!(args.contains(&"--full-auto".to_string()));
        // No --allowedTools for Codex
        assert!(!args.contains(&"--allowedTools".to_string()));
    }

    #[test]
    fn resolve_command_experimental_agent_no_special_flags() {
        let project = new_project(5);
        let (command, args) = resolve_command(
            &Agent::known("gemini"),
            "hello",
            &project.config,
            None,
            None,
            None,
        );
        assert_eq!(command, "gemini");
        // Experimental agents don't get --allowedTools or --full-auto
        assert!(!args.contains(&"--allowedTools".to_string()));
        assert!(!args.contains(&"--full-auto".to_string()));
    }

    #[test]
    fn resolve_command_claude_planner_plan_mode_uses_permission_mode_plan() {
        let mut project = new_project(5);
        project.config.planner_permission_mode = "plan".to_string();
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Planner),
        );
        assert!(args.contains(&"--permission-mode".to_string()));
        assert!(args.contains(&"plan".to_string()));
        assert!(!args.contains(&"--allowedTools".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn resolve_command_planner_plan_mode_overrides_claude_full_access() {
        let mut project = new_project(5);
        project.config.claude_full_access = true;
        project.config.planner_permission_mode = "plan".to_string();
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Planner),
        );
        assert!(args.contains(&"--permission-mode".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn resolve_command_planner_permission_mode_does_not_affect_non_planner_role() {
        let mut project = new_project(5);
        project.config.planner_permission_mode = "plan".to_string();
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            None,
            Some(AgentRole::Implementer),
        );
        // Implementer with full_access=true (default) uses dangerously flag, not plan mode
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn resolve_command_claude_with_model_produces_model_flag() {
        let project = new_project(5);
        let agent = Agent::known("claude").with_model(Some("claude-sonnet-4-6".to_string()));
        let (_, args) = resolve_command(&agent, "hello", &project.config, None, None, None);
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"claude-sonnet-4-6".to_string()));
    }

    #[test]
    fn resolve_command_codex_with_model_produces_m_flag() {
        let project = new_project(5);
        let agent = Agent::known("codex").with_model(Some("o3".to_string()));
        let (_, args) = resolve_command(&agent, "hello", &project.config, None, None, None);
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"o3".to_string()));
    }

    #[test]
    fn resolve_command_appends_system_prompt_when_provided() {
        let project = new_project(5);
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            Some("system instructions"),
            None,
            None,
        );
        assert!(args.contains(&"--append-system-prompt".to_string()));
        assert!(args.contains(&"system instructions".to_string()));
    }

    #[test]
    fn resolve_command_non_claude_prepends_system_prompt_into_prompt_text() {
        let project = new_project(5);
        let (_, args) = resolve_command(
            &Agent::known("codex"),
            "do the work",
            &project.config,
            Some("system instructions"),
            None,
            None,
        );
        // Non-Claude agents must NOT use --append-system-prompt
        assert!(!args.contains(&"--append-system-prompt".to_string()));
        // The system prompt must be prepended to the prompt text arg — find it by
        // looking for an arg that contains both the preamble and the original prompt.
        let combined = args
            .iter()
            .find(|a| a.contains("system instructions") && a.contains("do the work"));
        assert!(
            combined.is_some(),
            "no arg found containing both system prompt and original prompt; args={args:?}"
        );
    }

    #[test]
    fn resolve_command_adds_resume_flag_with_session_id() {
        let project = new_project(5);
        let (_, args) = resolve_command(
            &Agent::known("claude"),
            "hello",
            &project.config,
            None,
            Some("abc-123-session"),
            None,
        );
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"abc-123-session".to_string()));
        // Still has -p for the new prompt
        assert!(args.contains(&"-p".to_string()));
    }

    #[test]
    fn resolve_command_adds_codex_resume_subcommand_with_session_id() {
        let project = new_project(5);
        let (_, args) = resolve_command(
            &Agent::known("codex"),
            "hello",
            &project.config,
            None,
            Some("sess_codex_42"),
            None,
        );
        // Should have `exec resume <id>` in sequence
        let exec_pos = args.iter().position(|a| a == "exec").expect("exec arg");
        assert_eq!(args[exec_pos + 1], "resume");
        assert_eq!(args[exec_pos + 2], "sess_codex_42");
        // Prompt still present
        assert!(args.contains(&"hello".to_string()));
    }

    #[test]
    fn resolve_command_builds_codex_with_full_access_by_default() {
        let project = new_project(5);
        let (command, args) = resolve_command(
            &Agent::known("codex"),
            "hello",
            &project.config,
            None,
            None,
            None,
        );
        assert_eq!(command, "codex");
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"never".to_string()));
        assert!(!args.contains(&"--full-auto".to_string()));
    }

    #[test]
    fn resolve_command_builds_codex_with_full_auto_when_full_access_disabled() {
        let mut project = new_project(5);
        project.config.codex_full_access = false;
        let (command, args) = resolve_command(
            &Agent::known("codex"),
            "hello",
            &project.config,
            None,
            None,
            None,
        );
        assert_eq!(command, "codex");
        assert!(args.contains(&"--full-auto".to_string()));
        assert!(!args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_removes_claudecode_from_child_environment() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable(
            "claude",
            "#!/bin/sh\nif [ \"${CLAUDECODE+x}\" = \"x\" ]; then\n  printf 'CLAUDECODE=%s\\n' \"$CLAUDECODE\"\nelse\n  echo 'CLAUDECODE=unset'\nfi\n",
        );
        let _path_guard = project.with_path_override();
        let _claudecode_guard = ScopedEnvVar::set("CLAUDECODE", "nested-agent-marker");

        let output = run_agent(&Agent::known("claude"), "ignored", &project.config, None)
            .expect("agent should succeed");
        assert!(output.contains("CLAUDECODE=unset"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_collects_output_from_stdout_and_stderr() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable(
            "claude",
            "#!/bin/sh\nprintf 'out-1\\n'\n/bin/sleep 0.1\nprintf 'err-1\\n' >&2\n/bin/sleep 0.1\nprintf 'out-2\\n'\n",
        );
        let _path_guard = project.with_path_override();

        let output = run_agent(&Agent::known("claude"), "ignored", &project.config, None)
            .expect("agent should succeed");

        assert!(output.contains("out-1"));
        assert!(output.contains("err-1"));
        assert!(output.contains("out-2"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_logs_idle_timeout_once_and_terminates_process() {
        let _env_guard = env_lock();
        let project = new_project(1);
        project.create_executable("claude", "#!/bin/sh\nexec /bin/sleep 10\n");
        let _path_guard = project.with_path_override();

        let started = Instant::now();
        let result = run_agent(&Agent::known("claude"), "ignored", &project.config, None);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(8),
            "timed out process should terminate quickly"
        );
        assert!(result.is_err());
        let logs = project.read_log();
        assert_eq!(logs.matches("⏱️ Idle timeout: no output for 1s").count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_force_kills_process_that_ignores_sigterm() {
        let _env_guard = env_lock();
        let project = new_project(1);
        project.create_executable(
            "claude",
            "#!/bin/sh\ntrap '' TERM\nwhile true; do\n  /bin/sleep 1\ndone\n",
        );
        let _path_guard = project.with_path_override();

        let started = Instant::now();
        let result = run_agent(&Agent::known("claude"), "ignored", &project.config, None);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(8),
            "TERM-ignoring process should be force-killed after grace period"
        );
        assert!(result.is_err());
        let logs = project.read_log();
        assert_eq!(logs.matches("⏱️ Idle timeout: no output for 1s").count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_writes_response_block_for_non_empty_output() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable("claude", "#!/bin/sh\nprintf 'hello'\n");
        let _path_guard = project.with_path_override();

        let output = run_agent(&Agent::known("claude"), "ignored", &project.config, None)
            .expect("agent should succeed");
        assert_eq!(output, "hello");

        let separator = "─".repeat(60);
        let logs = project.read_log();
        assert!(logs.contains("claude response:"));
        assert!(logs.contains(&separator));
        assert!(logs.contains(&format!("hello\n{separator}\n\n")));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_skips_response_block_for_whitespace_only_output() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable("claude", "#!/bin/sh\nprintf '  \\n\\t  '\n");
        let _path_guard = project.with_path_override();

        let output = run_agent(&Agent::known("claude"), "ignored", &project.config, None)
            .expect("agent should succeed");
        assert_eq!(output, "  \n\t  ");

        let logs = project.read_log();
        assert!(!logs.contains("claude response:"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_logs_warning_and_returns_error_on_non_zero_exit() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable("claude", "#!/bin/sh\nprintf 'partial-output'\nexit 7\n");
        let _path_guard = project.with_path_override();

        let result = run_agent(&Agent::known("claude"), "ignored", &project.config, None);
        assert!(result.is_err());

        let logs = project.read_log();
        assert!(logs.contains("⚠ claude exited with code 7"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_logs_warning_with_null_for_signal_exit() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable(
            "claude",
            "#!/bin/sh\nprintf 'signal-output\\n'\nkill -TERM $$\n",
        );
        let _path_guard = project.with_path_override();

        let result = run_agent(&Agent::known("claude"), "ignored", &project.config, None);
        assert!(result.is_err());

        let logs = project.read_log();
        assert!(logs.contains("⚠ claude exited with code null"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_logs_spawn_error_and_returns_error() {
        let _env_guard = env_lock();
        let project = new_project(5);
        fs::create_dir_all(project.bin_dir()).expect("bin dir should exist");
        let _path_guard = project.with_path_override();

        let result = run_agent(&Agent::known("claude"), "ignored", &project.config, None);
        assert!(result.is_err());

        let logs = project.read_log();
        assert!(logs.contains("⚠ claude failed to start:"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_timeout_kills_spawned_subprocesses() {
        let _env_guard = env_lock();
        let project = new_project(1);

        let pid_file = project.root.join("child.pid");
        let pid_file_str = pid_file.display().to_string();

        // Script spawns a background sleep, records its PID via $!, then
        // replaces itself with another sleep. On timeout the entire process
        // group should be killed, including the background child.
        let script =
            format!("#!/bin/sh\n/bin/sleep 60 &\necho $! > {pid_file_str}\nexec /bin/sleep 60\n");
        project.create_executable("claude", &script);
        let _path_guard = project.with_path_override();

        let result = run_agent(&Agent::known("claude"), "ignored", &project.config, None);
        assert!(result.is_err(), "agent should time out");

        // The PID file must exist and contain a valid PID.
        let pid_contents = fs::read_to_string(&pid_file)
            .expect("child.pid must exist — script should have written it");
        let child_pid: libc::pid_t = pid_contents
            .trim()
            .parse()
            .expect("child.pid must contain a numeric PID");
        assert!(child_pid > 0, "child PID must be positive");

        // Poll for up to 2 seconds to confirm the subprocess is dead.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let alive = unsafe { libc::kill(child_pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "spawned subprocess (pid {child_pid}) should have been killed by group signal"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// A `Read` implementation that yields predefined byte slices, one per call.
    struct ChunkedReader {
        chunks: Vec<Vec<u8>>,
        index: usize,
    }

    impl ChunkedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self { chunks, index: 0 }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.index >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &self.chunks[self.index];
            self.index += 1;
            let len = chunk.len().min(buf.len());
            buf[..len].copy_from_slice(&chunk[..len]);
            Ok(len)
        }
    }

    #[test]
    fn reader_thread_preserves_multibyte_utf8_split_across_chunks() {
        // 🦀 is U+1F980, encoded as 4 bytes: F0 9F A6 80
        // Split across two chunks to reproduce the old per-chunk lossy decode bug.
        let chunks = vec![
            b"hello \xF0".to_vec(),           // first byte of 🦀
            b"\x9F\xA6\x80 world\n".to_vec(), // remaining 3 bytes + text
        ];
        let reader = ChunkedReader::new(chunks);

        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let last_output_ms = Arc::new(AtomicU64::new(0));
        let start = Instant::now();

        let handle = spawn_reader_thread(reader, Arc::clone(&output), last_output_ms, start, false);
        handle.join().expect("reader thread should not panic");

        let raw = output.lock().map(|mut g| std::mem::take(&mut *g)).unwrap();
        let result = String::from_utf8_lossy(&raw).into_owned();

        assert_eq!(result, "hello 🦀 world\n");
        assert!(
            !result.contains('\u{FFFD}'),
            "output must not contain replacement characters"
        );
    }

    #[test]
    fn extract_claude_stream_json_text_prefers_result_payload() {
        let output = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"assistant text\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"final result text\"}\n"
        );

        let extracted = extract_claude_stream_json_text(output);
        assert_eq!(extracted.as_deref(), Some("final result text"));
    }

    #[test]
    fn extract_stream_json_usage_reads_tokens_and_cost() {
        let output = concat!(
            "{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":120,\"output_tokens\":45}}}\n",
            "{\"type\":\"result\",\"usage\":{\"total_tokens\":165,\"total_cost_usd\":0.012345}}\n"
        );

        let usage = extract_stream_json_usage(output);
        assert_eq!(
            usage,
            AgentUsage {
                input_tokens: 120,
                output_tokens: 45,
                total_tokens: 165,
                cost_usd_micros: 12_345,
            }
        );
    }

    #[test]
    fn extract_stream_json_usage_derives_total_from_input_plus_output() {
        let output = "{\"usage\":{\"input_tokens\":1000,\"output_tokens\":250}}\n";
        let usage = extract_stream_json_usage(output);
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 250);
        assert_eq!(usage.total_tokens, 1250);
    }

    #[test]
    fn usage_snapshot_saturating_math_is_safe() {
        let lhs = UsageSnapshot {
            agent_calls: 2,
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            cost_usd_micros: 1_000,
        };
        let rhs = UsageSnapshot {
            agent_calls: 1,
            input_tokens: 60,
            output_tokens: 30,
            total_tokens: 90,
            cost_usd_micros: 700,
        };

        let sum = lhs.saturating_add(rhs);
        assert_eq!(
            sum,
            UsageSnapshot {
                agent_calls: 3,
                input_tokens: 160,
                output_tokens: 80,
                total_tokens: 240,
                cost_usd_micros: 1_700,
            }
        );

        let diff = sum.saturating_sub(lhs);
        assert_eq!(diff, rhs);
    }

    #[test]
    fn normalize_agent_output_leaves_plain_text_unchanged_for_claude() {
        let output = "plain text output".to_string();
        let normalized = normalize_agent_output(&Agent::known("claude"), output.clone());
        assert_eq!(normalized, output);
    }

    #[test]
    fn truncate_response_lines_keeps_last_n_lines_when_over_cap() {
        let lines: Vec<String> = (1..=600).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");

        let (kept, dropped) = truncate_response_lines(&input, 500);
        assert_eq!(dropped, 100);

        let kept_lines: Vec<&str> = kept.lines().collect();
        assert_eq!(kept_lines.len(), 500);
        assert_eq!(kept_lines[0], "line 101");
        assert_eq!(kept_lines[499], "line 600");
    }

    #[test]
    fn truncate_response_lines_does_not_truncate_within_cap() {
        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");

        let (kept, dropped) = truncate_response_lines(&input, 500);
        assert_eq!(dropped, 0);

        let kept_lines: Vec<&str> = kept.lines().collect();
        assert_eq!(kept_lines.len(), 100);
        assert_eq!(kept_lines[0], "line 1");
        assert_eq!(kept_lines[99], "line 100");
    }

    #[test]
    fn append_response_block_truncates_output_exceeding_line_cap() {
        let project = new_project(5);
        // Ensure state dir exists for log file
        std::fs::create_dir_all(&project.config.state_dir).expect("state dir should be created");

        let lines: Vec<String> = (1..=600).map(|i| format!("line {i}")).collect();
        let output = lines.join("\n");

        append_response_block(&Agent::known("claude"), &output, &project.config)
            .expect("append should succeed");

        let logs = project.read_log();
        assert!(
            logs.contains("[... 100 lines truncated ...]"),
            "block should contain truncation marker"
        );
        assert!(
            logs.contains("⚠ Response block truncated: 600 lines -> 500 lines"),
            "log should contain truncation warning"
        );
        assert!(logs.contains("line 600"), "should keep last lines");
        assert!(logs.contains("line 101"), "should keep line 101");
        assert!(
            !logs.contains("\nline 100\n"),
            "should not keep line 100 as a distinct line"
        );
    }

    #[test]
    fn append_response_block_does_not_truncate_output_within_cap() {
        let project = new_project(5);
        std::fs::create_dir_all(&project.config.state_dir).expect("state dir should be created");

        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        let output = lines.join("\n");

        append_response_block(&Agent::known("claude"), &output, &project.config)
            .expect("append should succeed");

        let logs = project.read_log();
        assert!(
            !logs.contains("truncated"),
            "no truncation marker or warning expected"
        );
        assert!(logs.contains("line 1"), "should contain first line");
        assert!(logs.contains("line 100"), "should contain last line");
    }

    // -----------------------------------------------------------------------
    // Session persistence helpers
    // -----------------------------------------------------------------------

    #[test]
    fn extract_claude_session_id_from_result_event() {
        let output = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"done\",\"session_id\":\"sess_abc123\"}\n"
        );
        assert_eq!(
            extract_claude_session_id(output),
            Some("sess_abc123".to_string())
        );
    }

    #[test]
    fn extract_claude_session_id_returns_none_when_absent() {
        let output = "{\"type\":\"result\",\"result\":\"done\"}\n";
        assert_eq!(extract_claude_session_id(output), None);
    }

    #[test]
    fn session_id_round_trip_through_state_files() {
        let project = new_project(5);
        std::fs::create_dir_all(&project.config.state_dir).expect("state dir should be created");

        assert!(read_session_id(&project.config, "implementer").is_none());

        write_session_id(&project.config, "implementer", "sess_test_123");
        assert_eq!(
            read_session_id(&project.config, "implementer"),
            Some("sess_test_123".to_string())
        );

        clear_session_id(&project.config, "implementer");
        assert!(read_session_id(&project.config, "implementer").is_none());
    }

    #[test]
    fn extract_codex_session_id_from_output() {
        let output = concat!(
            "{\"type\":\"message\",\"content\":\"hello\"}\n",
            "{\"type\":\"status\",\"value\":\"done\",\"session_id\":\"csess_xyz789\"}\n",
        );
        assert_eq!(
            extract_codex_session_id(output),
            Some("csess_xyz789".to_string())
        );
    }

    #[test]
    fn extract_codex_session_id_returns_none_when_absent() {
        let output = "{\"type\":\"status\",\"value\":\"done\"}\n";
        assert_eq!(extract_codex_session_id(output), None);
    }

    #[test]
    fn session_id_round_trip_with_agent_aware_key() {
        let project = new_project(5);
        std::fs::create_dir_all(&project.config.state_dir).expect("state dir should be created");

        // Keys now include agent name for disambiguation
        let key = "implementer-claude";
        assert!(read_session_id(&project.config, key).is_none());

        write_session_id(&project.config, key, "sess_agent_aware_001");
        assert_eq!(
            read_session_id(&project.config, key),
            Some("sess_agent_aware_001".to_string())
        );

        // Different agent gets a separate file
        let key2 = "implementer-codex";
        assert!(read_session_id(&project.config, key2).is_none());

        write_session_id(&project.config, key2, "csess_codex_002");
        assert_eq!(
            read_session_id(&project.config, key2),
            Some("csess_codex_002".to_string())
        );

        // Both keys are independent
        assert_eq!(
            read_session_id(&project.config, key),
            Some("sess_agent_aware_001".to_string())
        );

        clear_session_id(&project.config, key);
        assert!(read_session_id(&project.config, key).is_none());
        // Other agent's session is unaffected
        assert_eq!(
            read_session_id(&project.config, key2),
            Some("csess_codex_002".to_string())
        );
    }

    #[test]
    fn extract_codex_json_text_from_message_event() {
        let output = "{\"type\":\"message\",\"content\":\"hello from codex\"}\n";
        assert_eq!(
            extract_codex_json_text(output),
            Some("hello from codex".to_string())
        );
    }

    #[test]
    fn extract_codex_json_text_returns_none_for_empty() {
        let output = "{\"type\":\"status\",\"value\":\"done\"}\n";
        assert_eq!(extract_codex_json_text(output), None);
    }

    #[test]
    fn extract_codex_json_text_from_item_completed_agent_message() {
        let output = concat!(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"reasoning\",\"text\":\"thinking\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"final from codex\"}}\n",
        );
        assert_eq!(
            extract_codex_json_text(output),
            Some("final from codex".to_string())
        );
    }

    #[test]
    fn extract_codex_json_text_ignores_non_json_diagnostics_when_agent_message_exists() {
        let output = concat!(
            "2026-02-25T09:23:36Z ERROR codex_core::auth: Failed to refresh token\n",
            "{\"type\":\"turn.started\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}\n",
        );
        assert_eq!(extract_codex_json_text(output), Some("ok".to_string()));
    }

    // -----------------------------------------------------------------------
    // Session resume error detection (output-signaled failures)
    // -----------------------------------------------------------------------

    // Plain-text (stderr) error lines — detected
    #[test]
    fn has_session_resume_error_detects_session_not_found() {
        assert!(has_session_resume_error(
            "Error: session not found for the given ID"
        ));
    }

    #[test]
    fn has_session_resume_error_detects_session_expired() {
        assert!(has_session_resume_error("The session has expired."));
    }

    #[test]
    fn has_session_resume_error_detects_invalid_session() {
        assert!(has_session_resume_error(
            "session_id is invalid: sess_old123"
        ));
    }

    #[test]
    fn has_session_resume_error_detects_could_not_resume() {
        assert!(has_session_resume_error(
            "could not resume session: connection reset"
        ));
    }

    #[test]
    fn has_session_resume_error_detects_failed_to_resume() {
        assert!(has_session_resume_error("Failed to resume session"));
    }

    #[test]
    fn has_session_resume_error_detects_no_such_session() {
        assert!(has_session_resume_error("no such session"));
    }

    #[test]
    fn has_session_resume_error_detects_resume_failed() {
        assert!(has_session_resume_error(
            "Resume failed: session data corrupted"
        ));
    }

    #[test]
    fn has_session_resume_error_returns_false_for_normal_output() {
        assert!(!has_session_resume_error("Hello, I completed the task."));
    }

    #[test]
    fn has_session_resume_error_returns_false_for_empty_output() {
        assert!(!has_session_resume_error(""));
    }

    #[test]
    fn has_session_resume_error_is_case_insensitive() {
        assert!(has_session_resume_error("SESSION NOT FOUND"));
        assert!(has_session_resume_error("Invalid Session ID"));
    }

    // JSON error events — detected
    #[test]
    fn has_session_resume_error_detects_json_error_event() {
        let raw = "{\"type\":\"error\",\"message\":\"session not found\"}\n";
        assert!(has_session_resume_error(raw));
    }

    #[test]
    fn has_session_resume_error_detects_json_system_event() {
        let raw = "{\"type\":\"system\",\"error\":\"invalid session\"}\n";
        assert!(has_session_resume_error(raw));
    }

    // Assistant/result content — NOT detected (false-positive guard)
    #[test]
    fn has_session_resume_error_ignores_assistant_content_discussing_sessions() {
        // An assistant event whose content happens to discuss "session not found"
        // should NOT trigger a retry.
        let raw = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"The error 'session not found' occurs when...\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"To fix 'invalid session' errors, check your config.\"}\n"
        );
        assert!(
            !has_session_resume_error(raw),
            "assistant/result content should not trigger session error detection"
        );
    }

    #[test]
    fn has_session_resume_error_ignores_codex_message_content_discussing_sessions() {
        // A Codex message event whose content discusses session errors.
        let raw = "{\"type\":\"message\",\"content\":\"The session not found error means the ID expired.\"}\n";
        assert!(
            !has_session_resume_error(raw),
            "message content should not trigger session error detection"
        );
    }

    #[test]
    fn has_session_resume_error_mixed_events_detects_error_channel_only() {
        // Mix of assistant content (discussing sessions) and an actual error event.
        let raw = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"session not found in the docs\"}]}}\n",
            "{\"type\":\"error\",\"message\":\"failed to resume: session expired\"}\n"
        );
        assert!(
            has_session_resume_error(raw),
            "should detect error in the error event, not the assistant content"
        );
    }

    // -----------------------------------------------------------------------
    // Stale session retry integration tests
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn run_agent_with_session_retries_on_nonzero_exit_with_stale_session() {
        let _env_guard = env_lock();
        let project = new_project(5);
        std::fs::create_dir_all(&project.config.state_dir).expect("state dir");

        // Write a stale session ID
        let session_key = "implement-implementer-claude";
        write_session_id(&project.config, session_key, "sess_stale_001");

        // Agent script: if --resume is passed, exit 1 (simulating stale session).
        // Otherwise, succeed with normal output.
        project.create_executable(
            "claude",
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"--resume\" ]; then\n    echo 'session error' >&2\n    exit 1\n  fi\ndone\necho 'fresh session output'\n",
        );
        let _path_guard = project.with_path_override();

        let result = run_agent_with_session(
            &Agent::known("claude"),
            "test prompt",
            &project.config,
            None,
            Some(session_key),
            Some(AgentRole::Implementer),
            None,
        );

        // Should succeed because the retry without session resume works
        assert!(result.is_ok(), "expected retry to succeed: {result:?}");
        let output = result.unwrap();
        assert!(
            output.trim() == "fresh session output",
            "unexpected output: {output:?}"
        );

        // Session ID should have been cleared
        assert!(
            read_session_id(&project.config, session_key).is_none(),
            "stale session ID should be cleared after retry"
        );

        // Log should contain the retry message
        let logs = project.read_log();
        assert!(
            logs.contains("Session resume failed for implementer/claude, retrying fresh"),
            "expected retry log message"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_with_session_retries_on_output_error_with_exit_zero() {
        let _env_guard = env_lock();
        let project = new_project(5);
        std::fs::create_dir_all(&project.config.state_dir).expect("state dir");

        // Write a stale session ID
        let session_key = "implement-implementer-claude";
        write_session_id(&project.config, session_key, "sess_stale_002");

        // Agent script: if --resume is passed, exit 0 but emit "session not found"
        // in the output (simulating an output-signaled resume failure).
        // Otherwise, succeed with normal output.
        project.create_executable(
            "claude",
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"--resume\" ]; then\n    echo 'Error: session not found for sess_stale_002'\n    exit 0\n  fi\ndone\necho 'fresh session output'\n",
        );
        let _path_guard = project.with_path_override();

        let result = run_agent_with_session(
            &Agent::known("claude"),
            "test prompt",
            &project.config,
            None,
            Some(session_key),
            Some(AgentRole::Implementer),
            None,
        );

        // Should succeed because the retry without session resume works
        assert!(
            result.is_ok(),
            "expected output-error retry to succeed: {result:?}"
        );
        let output = result.unwrap();
        assert!(
            output.trim() == "fresh session output",
            "unexpected output: {output:?}"
        );

        // Session ID should have been cleared
        assert!(
            read_session_id(&project.config, session_key).is_none(),
            "stale session ID should be cleared after output-error retry"
        );

        // Log should contain the retry message
        let logs = project.read_log();
        assert!(
            logs.contains("Session resume failed for implementer/claude, retrying fresh"),
            "expected retry log message for output-signaled failure"
        );
    }

    #[test]
    fn session_persistence_disabled_for_non_resumable_agents() {
        // Experimental agents with supports_session_resume=false should never
        // have session persistence enabled, regardless of config settings.
        let agent = Agent::known("gemini");
        assert!(
            !agent.spec().supports_session_resume,
            "gemini should not support session resume"
        );
    }

    // -----------------------------------------------------------------------
    // Role-specific effort level selection (regression for session key format)
    // -----------------------------------------------------------------------

    #[test]
    fn effective_effort_level_uses_implementer_effort_for_implementer_role() {
        let mut project = new_project(5);
        project.config.implementer_effort_level = Some("high".to_string());
        project.config.reviewer_effort_level = Some("low".to_string());
        project.config.claude_effort_level = Some("medium".to_string());

        // Implementer role should use implementer_effort_level
        assert_eq!(
            effective_effort_level(Some(AgentRole::Implementer), &project.config),
            Some("high")
        );
    }

    #[test]
    fn effective_effort_level_uses_reviewer_effort_for_reviewer_role() {
        let mut project = new_project(5);
        project.config.implementer_effort_level = Some("high".to_string());
        project.config.reviewer_effort_level = Some("low".to_string());
        project.config.claude_effort_level = Some("medium".to_string());

        // Reviewer role should use reviewer_effort_level
        assert_eq!(
            effective_effort_level(Some(AgentRole::Reviewer), &project.config),
            Some("low")
        );
    }

    #[test]
    fn effective_effort_level_falls_back_to_generic_when_role_specific_unset() {
        let mut project = new_project(5);
        project.config.implementer_effort_level = None;
        project.config.reviewer_effort_level = None;
        project.config.claude_effort_level = Some("medium".to_string());

        // With no role-specific setting, falls back to generic
        assert_eq!(
            effective_effort_level(Some(AgentRole::Implementer), &project.config),
            Some("medium")
        );
        assert_eq!(
            effective_effort_level(Some(AgentRole::Reviewer), &project.config),
            Some("medium")
        );
    }

    #[test]
    fn effective_effort_level_returns_none_when_nothing_configured() {
        let project = new_project(5);
        // All effort levels default to None
        assert_eq!(
            effective_effort_level(Some(AgentRole::Implementer), &project.config),
            None
        );
        assert_eq!(
            effective_effort_level(Some(AgentRole::Reviewer), &project.config),
            None
        );
        assert_eq!(effective_effort_level(None, &project.config), None);
    }

    #[test]
    fn effective_effort_level_planner_role_uses_generic_fallback() {
        let mut project = new_project(5);
        project.config.implementer_effort_level = Some("high".to_string());
        project.config.reviewer_effort_level = Some("low".to_string());
        project.config.claude_effort_level = Some("medium".to_string());

        // Planner role has no dedicated field; falls back to generic
        assert_eq!(
            effective_effort_level(Some(AgentRole::Planner), &project.config),
            Some("medium")
        );
    }

    /// F-001: Prove that `run_agent_inner` uses `caller_meta` for transcript entries
    /// when metadata is provided, instead of falling back to defaults.
    #[test]
    #[cfg(unix)]
    fn run_agent_with_session_uses_caller_meta_for_transcript() {
        let _env_guard = env_lock();
        let mut project = new_project(5);
        project.config.transcript_enabled = true;
        std::fs::create_dir_all(&project.config.state_dir).unwrap();

        project.create_executable("claude", "#!/bin/sh\nprintf 'agent-output-here'\n");
        let _path_guard = project.with_path_override();

        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "gate-a-review".to_string(),
            round: 7,
            role: "reviewer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: Some("impl-reviewer-claude".to_string()),
        };

        let result = run_agent_with_session(
            &Agent::known("claude"),
            "test prompt content",
            &project.config,
            Some("system instructions here"),
            None,
            Some(AgentRole::Reviewer),
            Some(&meta),
        );
        assert!(result.is_ok(), "agent should succeed: {:?}", result);

        let transcript_path = project.config.state_dir.join("transcript.log");
        assert!(transcript_path.exists(), "transcript.log should be created");
        let transcript = std::fs::read_to_string(&transcript_path).unwrap();

        // Verify caller_meta fields are used (not fallback defaults)
        assert!(
            transcript.contains("workflow: implement"),
            "transcript must contain workflow from caller_meta"
        );
        assert!(
            transcript.contains("phase: gate-a-review"),
            "transcript must contain phase from caller_meta"
        );
        assert!(
            transcript.contains("round: 7"),
            "transcript must contain round from caller_meta"
        );
        assert!(
            transcript.contains("role: reviewer"),
            "transcript must contain role from caller_meta"
        );
        assert!(
            transcript.contains("agent: claude"),
            "transcript must contain agent_name from caller_meta"
        );
        assert!(
            transcript.contains("session_hint: impl-reviewer-claude"),
            "transcript must contain session_hint from caller_meta"
        );

        // Verify prompt and output content is captured
        assert!(
            transcript.contains("test prompt content"),
            "transcript must contain user prompt"
        );
        assert!(
            transcript.contains("system instructions here"),
            "transcript must contain system prompt"
        );
        assert!(
            transcript.contains("agent-output-here"),
            "transcript must contain normalized agent output"
        );
    }

    /// F-001: Verify that without caller_meta, transcript falls back to role/agent
    /// from function parameters (not completely empty).
    #[test]
    #[cfg(unix)]
    fn run_agent_with_session_transcript_fallback_without_caller_meta() {
        let _env_guard = env_lock();
        let mut project = new_project(5);
        project.config.transcript_enabled = true;
        std::fs::create_dir_all(&project.config.state_dir).unwrap();

        project.create_executable("claude", "#!/bin/sh\nprintf 'output'\n");
        let _path_guard = project.with_path_override();

        // Call without caller_meta — exercises the fallback path
        let result = run_agent_with_session(
            &Agent::known("claude"),
            "prompt",
            &project.config,
            None,
            None,
            Some(AgentRole::Implementer),
            None, // no caller_meta
        );
        assert!(result.is_ok());

        let transcript_path = project.config.state_dir.join("transcript.log");
        let transcript = std::fs::read_to_string(&transcript_path).unwrap();

        // Fallback should still populate role and agent_name from function args
        assert!(
            transcript.contains("role: implementer"),
            "fallback must derive role from AgentRole parameter"
        );
        assert!(
            transcript.contains("agent: claude"),
            "fallback must derive agent_name from agent parameter"
        );
        // Fallback uses named constants instead of empty strings
        assert!(
            transcript.contains(&format!("workflow: {}", crate::state::FALLBACK_WORKFLOW)),
            "fallback must use FALLBACK_WORKFLOW constant, got: {transcript}"
        );
        assert!(
            transcript.contains(&format!("phase: {}", crate::state::FALLBACK_PHASE)),
            "fallback must use FALLBACK_PHASE constant, got: {transcript}"
        );
        assert!(transcript.contains("round: 0"), "fallback has round 0");
        // Untracked entries get a tracking line
        assert!(
            transcript.contains("tracking: untracked"),
            "fallback entries must include tracking: untracked line"
        );
        // No session_key was passed, so session_hint line should be absent
        assert!(
            !transcript.contains("session_hint:"),
            "fallback without session_key must omit session_hint"
        );
    }

    /// F-001: Verify that the fallback path maps `session_key` → `session_hint`
    /// when `caller_meta` is `None` but a session key is supplied.
    #[test]
    #[cfg(unix)]
    fn run_agent_with_session_transcript_fallback_preserves_session_key() {
        let _env_guard = env_lock();
        let mut project = new_project(5);
        project.config.transcript_enabled = true;
        std::fs::create_dir_all(&project.config.state_dir).unwrap();

        project.create_executable("claude", "#!/bin/sh\nprintf 'ok'\n");
        let _path_guard = project.with_path_override();

        // Pass session_key but no caller_meta — exercises the fallback
        // branch at agent.rs:1004-1010 where session_key is mapped.
        let result = run_agent_with_session(
            &Agent::known("claude"),
            "prompt",
            &project.config,
            None,
            Some("impl-implementer-claude"),
            Some(AgentRole::Implementer),
            None, // no caller_meta
        );
        assert!(result.is_ok(), "agent should succeed: {:?}", result);

        let transcript_path = project.config.state_dir.join("transcript.log");
        let transcript = std::fs::read_to_string(&transcript_path).unwrap();

        assert!(
            transcript.contains("session_hint: impl-implementer-claude"),
            "fallback must map session_key to session_hint in transcript"
        );
        assert!(
            transcript.contains("role: implementer"),
            "fallback must still derive role"
        );
        assert!(
            transcript.contains("agent: claude"),
            "fallback must still derive agent_name"
        );
        // Fallback uses named constants for workflow/phase
        assert!(
            transcript.contains(&format!("workflow: {}", crate::state::FALLBACK_WORKFLOW)),
            "fallback must use FALLBACK_WORKFLOW constant even with session_key"
        );
        assert!(
            transcript.contains(&format!("phase: {}", crate::state::FALLBACK_PHASE)),
            "fallback must use FALLBACK_PHASE constant even with session_key"
        );
        // Untracked annotation line present
        assert!(
            transcript.contains("tracking: untracked"),
            "fallback entries must include tracking line even with session_key"
        );
    }
}
