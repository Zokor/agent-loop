use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;

pub const FINDINGS_FILENAME: &str = "findings.json";
pub const TASKS_FINDINGS_FILENAME: &str = "tasks_findings.json";
pub const QUALITY_CHECKS_FILENAME: &str = "quality_checks.md";

const LAST_RUN_TASK_MAX_CHARS: usize = 500;
const CONVERSATION_MAX_LINES: usize = 200;
const DECISIONS_REFERENCE_START: &str = "<!-- agent-loop:decisions-reference:start -->";
const DECISIONS_REFERENCE_END: &str = "<!-- agent-loop:decisions-reference:end -->";

/// Maximum number of lines in the transcript log before rotation.
const TRANSCRIPT_MAX_LINES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Pending,
    Planning,
    Implementing,
    Reviewing,
    Approved,
    Consensus,
    Disputed,
    NeedsChanges,
    NeedsRevision,
    MaxRounds,
    Stuck,
    Error,
    Interrupted,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Pending => "PENDING",
            Self::Planning => "PLANNING",
            Self::Implementing => "IMPLEMENTING",
            Self::Reviewing => "REVIEWING",
            Self::Approved => "APPROVED",
            Self::Consensus => "CONSENSUS",
            Self::Disputed => "DISPUTED",
            Self::NeedsChanges => "NEEDS_CHANGES",
            Self::NeedsRevision => "NEEDS_REVISION",
            Self::MaxRounds => "MAX_ROUNDS",
            Self::Stuck => "STUCK",
            Self::Error => "ERROR",
            Self::Interrupted => "INTERRUPTED",
        };

        write!(f, "{label}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowKind {
    Plan,
    Decompose,
    Implement,
}

impl fmt::Display for WorkflowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Plan => "plan",
            Self::Decompose => "decompose",
            Self::Implement => "implement",
        };
        write!(f, "{label}")
    }
}

impl FromStr for WorkflowKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plan" => Ok(Self::Plan),
            "decompose" => Ok(Self::Decompose),
            "implement" | "run" => Ok(Self::Implement),
            other => Err(format!("unknown workflow kind: {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopStatus {
    pub status: Status,
    pub round: u32,
    pub implementer: String,
    pub reviewer: String,
    pub mode: String,
    #[serde(rename = "lastRunTask")]
    pub last_run_task: String,
    pub reason: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusPatch {
    pub status: Option<Status>,
    pub round: Option<u32>,
    pub implementer: Option<String>,
    pub reviewer: Option<String>,
    pub mode: Option<String>,
    pub last_run_task: Option<String>,
    pub reason: Option<String>,
}

fn state_file_path(name: &str, config: &Config) -> PathBuf {
    config.state_dir.join(Path::new(name))
}

pub fn decisions_path(config: &Config) -> PathBuf {
    config.project_dir.join(".agent-loop").join("decisions.md")
}

#[allow(dead_code)]
pub fn read_decisions(config: &Config) -> String {
    if !config.decisions_enabled {
        return String::new();
    }

    let Ok(content) = fs::read_to_string(decisions_path(config)) else {
        return String::new();
    };

    if content.trim().is_empty() {
        return String::new();
    }

    let lines: Vec<&str> = content.lines().collect();
    let max_lines = config.decisions_max_lines as usize;
    if lines.len() <= max_lines {
        return lines.join("\n");
    }

    lines[lines.len() - max_lines..].join("\n")
}

pub fn append_decision(entry: &str, config: &Config) -> io::Result<()> {
    if !config.decisions_enabled {
        return Ok(());
    }

    if entry.trim().is_empty() {
        return Ok(());
    }

    let path = decisions_path(config);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", entry.trim())
}

fn decisions_reference_block() -> String {
    format!(
        "{DECISIONS_REFERENCE_START}\n## Agent Loop Decisions\nReview `.agent-loop/decisions.md` before planning or implementation.\nAppend durable learnings as `- [CATEGORY] description`, where CATEGORY is one of: ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY.\n{DECISIONS_REFERENCE_END}"
    )
}

fn upsert_decisions_reference(content: &str) -> Option<String> {
    if content.contains(".agent-loop/decisions.md") && !content.contains(DECISIONS_REFERENCE_START)
    {
        return None;
    }

    let block = decisions_reference_block();

    if let Some(start_idx) = content.find(DECISIONS_REFERENCE_START)
        && let Some(end_rel_idx) = content[start_idx..].find(DECISIONS_REFERENCE_END)
    {
        let end_idx = start_idx + end_rel_idx + DECISIONS_REFERENCE_END.len();
        let mut updated = String::with_capacity(content.len().saturating_add(block.len()));
        updated.push_str(&content[..start_idx]);
        updated.push_str(&block);
        updated.push_str(&content[end_idx..]);
        return (updated != content).then_some(updated);
    }

    let mut updated = String::new();
    let trimmed = content.trim_end_matches('\n');
    if !trimmed.is_empty() {
        updated.push_str(trimmed);
        updated.push_str("\n\n");
    }
    updated.push_str(&block);
    updated.push('\n');

    (updated != content).then_some(updated)
}

fn ensure_decisions_reference_file(path: &Path) -> io::Result<bool> {
    let content = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };

    let Some(updated) = upsert_decisions_reference(&content) else {
        return Ok(false);
    };

    fs::write(path, updated)?;
    Ok(true)
}

fn ensure_project_guide_decisions_references(config: &Config) {
    for filename in ["AGENTS.md", "CLAUDE.md"] {
        let path = config.project_dir.join(filename);
        if let Err(err) = ensure_decisions_reference_file(&path) {
            eprintln!("⚠ failed to sync decisions reference in {filename}: {err}");
        }
    }
}

/// Strip the managed decisions-reference block from content, returning the
/// updated content if a change was made.
fn strip_decisions_reference(content: &str) -> Option<String> {
    let start_idx = content.find(DECISIONS_REFERENCE_START)?;
    let end_rel_idx = content[start_idx..].find(DECISIONS_REFERENCE_END)?;
    let end_idx = start_idx + end_rel_idx + DECISIONS_REFERENCE_END.len();

    // Remove the block and any surrounding blank lines that were added
    let before = content[..start_idx].trim_end_matches('\n');
    let after = content[end_idx..].trim_start_matches('\n');

    let mut updated = String::with_capacity(content.len());
    if !before.is_empty() {
        updated.push_str(before);
        if !after.is_empty() {
            updated.push('\n');
        }
    }
    if !after.is_empty() {
        updated.push_str(after);
    }
    // Ensure trailing newline for file
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }

    (updated != content).then_some(updated)
}

/// Remove managed decisions-reference blocks from AGENTS.md and CLAUDE.md.
fn remove_project_guide_decisions_references(config: &Config) {
    for filename in ["AGENTS.md", "CLAUDE.md"] {
        let path = config.project_dir.join(filename);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(updated) = strip_decisions_reference(&content)
            && let Err(err) = fs::write(&path, updated)
        {
            eprintln!("⚠ failed to remove decisions reference from {filename}: {err}");
        }
    }
}

// ---------------------------------------------------------------------------
// Transcript capture (human-readable agent I/O log)
// ---------------------------------------------------------------------------

/// Metadata for a single agent invocation, passed from the phase/runner layer.
/// Fallback workflow label for agent calls that originate outside the
/// phase system (e.g. direct `run_agent()` calls in tests or one-off use).
pub const FALLBACK_WORKFLOW: &str = "direct";
/// Fallback phase label for agent calls not associated with a named phase.
pub const FALLBACK_PHASE: &str = "untracked";

#[derive(Debug, Clone, Default)]
pub struct AgentCallMeta {
    pub workflow: String,
    pub phase: String,
    pub round: u32,
    pub role: String,
    pub agent_name: String,
    pub session_hint: Option<String>,
}

impl AgentCallMeta {
    /// Returns `true` when this metadata was populated by a phase caller
    /// (i.e. workflow and phase are not the fallback sentinel values).
    pub fn is_phase_tracked(&self) -> bool {
        self.workflow != FALLBACK_WORKFLOW || self.phase != FALLBACK_PHASE
    }
}

/// A handle to an in-progress transcript entry returned by [`begin_transcript_entry`].
/// Used to append the completion block in phase 2.
pub struct TranscriptHandle {
    pub path: PathBuf,
}

/// Completion status for a two-phase transcript entry.
pub enum TranscriptCompletionStatus {
    Completed,
    Failed,
}

/// Phase 1: Write the entry header, metadata block, and prompt sections before the agent runs.
/// Returns a [`TranscriptHandle`] on success (used to complete the entry in phase 2).
/// No-op when `!config.transcript_enabled`; returns `None`.
pub fn begin_transcript_entry(
    config: &Config,
    meta: &AgentCallMeta,
    user_prompt: &str,
    system_prompt: Option<&str>,
) -> Option<TranscriptHandle> {
    if !config.transcript_enabled {
        return None;
    }

    debug_assert!(
        !meta.workflow.is_empty() && !meta.phase.is_empty(),
        "transcript entry has empty workflow/phase — caller should provide AgentCallMeta"
    );

    let path = config.state_dir.join("transcript.log");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let ts = timestamp();
    let mut entry = String::new();
    entry.push_str(&format!("=== AGENT CALL [{ts}] ===\n"));
    if !meta.is_phase_tracked() {
        entry.push_str("tracking: untracked (direct agent call)\n");
    }
    entry.push_str(&format!("workflow: {}\n", meta.workflow));
    entry.push_str(&format!("phase: {}\n", meta.phase));
    entry.push_str(&format!("round: {}\n", meta.round));
    entry.push_str(&format!("role: {}\n", meta.role));
    entry.push_str(&format!("agent: {}\n", meta.agent_name));
    if let Some(hint) = &meta.session_hint {
        entry.push_str(&format!("session_hint: {hint}\n"));
    }
    entry.push_str("status: in_progress\n");

    entry.push_str("\n--- USER PROMPT ---\n");
    entry.push_str(user_prompt);
    if !user_prompt.ends_with('\n') {
        entry.push('\n');
    }

    if let Some(sp) = system_prompt {
        entry.push_str("\n--- SYSTEM PROMPT ---\n");
        entry.push_str(sp);
        if !sp.ends_with('\n') {
            entry.push('\n');
        }
    }

    let write_result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(entry.as_bytes()));

    if let Err(err) = write_result {
        eprintln!("⚠ transcript write failed: {err}");
        return None;
    }

    Some(TranscriptHandle { path })
}

/// Phase 2: Append the completion block and `=== END ===` to the transcript entry.
/// Runs transcript rotation after writing.
/// No-op when `handle` is `None`.
pub fn complete_transcript_entry(
    handle: Option<&TranscriptHandle>,
    status: TranscriptCompletionStatus,
    failure_reason: Option<&str>,
    normalized_output: &str,
) {
    let Some(handle) = handle else { return };

    let ended_at = timestamp();
    let status_str = match status {
        TranscriptCompletionStatus::Completed => "completed",
        TranscriptCompletionStatus::Failed => "failed",
    };

    let mut entry = String::new();
    entry.push_str("\n--- COMPLETION ---\n");
    entry.push_str(&format!("status: {status_str}\n"));
    entry.push_str(&format!("ended_at: {ended_at}\n"));
    if let Some(reason) = failure_reason {
        entry.push_str(&format!("failure_reason: {reason}\n"));
    }

    entry.push_str("\n--- NORMALIZED OUTPUT ---\n");
    entry.push_str(normalized_output);
    if !normalized_output.ends_with('\n') {
        entry.push('\n');
    }
    entry.push_str("=== END ===\n\n");

    let write_result = OpenOptions::new()
        .append(true)
        .open(&handle.path)
        .and_then(|mut f| f.write_all(entry.as_bytes()));

    if let Err(err) = write_result {
        eprintln!("⚠ transcript write failed: {err}");
        return;
    }

    // Cap/rotate: if file exceeds TRANSCRIPT_MAX_LINES, keep the last half.
    if let Ok(content) = fs::read_to_string(&handle.path) {
        let line_count = content.lines().count();
        if line_count > TRANSCRIPT_MAX_LINES {
            let keep_from = line_count - (TRANSCRIPT_MAX_LINES / 2);
            let trimmed: String = content
                .lines()
                .skip(keep_from)
                .collect::<Vec<_>>()
                .join("\n");
            let mut rotated = String::from("[transcript rotated]\n");
            rotated.push_str(&trimmed);
            if !rotated.ends_with('\n') {
                rotated.push('\n');
            }
            let _ = fs::write(&handle.path, rotated);
        }
    }
}

/// Append a human-readable transcript entry to `.agent-loop/state/transcript.log`.
///
/// Convenience wrapper around [`begin_transcript_entry`] and [`complete_transcript_entry`],
/// retained for backward compatibility with existing tests and direct call sites.
///
/// No-op when `!config.transcript_enabled`. Failures are best-effort (non-fatal).
pub fn append_transcript_entry(
    config: &Config,
    meta: &AgentCallMeta,
    user_prompt: &str,
    system_prompt: Option<&str>,
    normalized_output: &str,
) {
    let handle = begin_transcript_entry(config, meta, user_prompt, system_prompt);
    complete_transcript_entry(
        handle.as_ref(),
        TranscriptCompletionStatus::Completed,
        None,
        normalized_output,
    );
}

pub fn normalize_task_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn extract_task_title(value: &str) -> String {
    let mut in_code_fence = false;

    for line in value.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence || trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('#') {
            let heading = trimmed.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return normalize_task_text(heading);
            }
        }
    }

    // If no markdown heading is present, preserve existing behavior and keep
    // the full normalized task text.
    normalize_task_text(value)
}

pub fn summarize_task(value: &str, max_length: Option<usize>) -> String {
    let normalized = normalize_task_text(value);
    let limit = max_length.unwrap_or(120);

    if normalized.chars().count() <= limit {
        return normalized;
    }

    if limit <= 3 {
        return ".".repeat(limit);
    }

    let truncated = normalized.chars().take(limit - 3).collect::<String>();
    format!("{truncated}...")
}

pub fn resolve_last_run_task(explicit: Option<&str>, config: &Config) -> String {
    if let Some(value) = explicit
        && !value.trim().is_empty()
    {
        return extract_task_title(value);
    }

    let task_from_state = read_state_file("task.md", config);
    if !task_from_state.trim().is_empty() {
        return extract_task_title(&task_from_state);
    }

    String::new()
}

pub fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs() as i64;
    let millis = now.subsec_millis();

    let days_since_epoch = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let hour = (seconds_of_day / 3_600) as u32;
    let minute = ((seconds_of_day % 3_600) / 60) as u32;
    let second = (seconds_of_day % 60) as u32;
    let (year, month, day) = civil_from_days(days_since_epoch);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_piece = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_piece + 2) / 5 + 1;
    let month = month_piece + if month_piece < 10 { 3 } else { -9 };

    if month <= 2 {
        year += 1;
    }

    (year as i32, month as u32, day as u32)
}

pub fn default_status(config: &Config) -> LoopStatus {
    LoopStatus {
        status: Status::Pending,
        round: 0,
        implementer: config.implementer.to_string(),
        reviewer: config.reviewer.to_string(),
        mode: config.run_mode.to_string(),
        last_run_task: resolve_last_run_task(None, config),
        reason: None,
        timestamp: timestamp(),
    }
}

#[derive(Debug, Clone)]
pub struct StatusReadResult {
    pub status: LoopStatus,
    pub warnings: Vec<String>,
}

#[allow(dead_code)]
pub fn normalize_status_value(raw: &Value, config: &Config) -> LoopStatus {
    normalize_status_value_with_warnings(raw, config).status
}

/// Escape control characters in a string for safe terminal output.
#[cfg(test)]
fn sanitize_for_display(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\r' && c != '\t' {
                '\u{FFFD}' // Unicode replacement character
            } else {
                c
            }
        })
        .collect()
}

pub fn normalize_status_value_with_warnings(raw: &Value, config: &Config) -> StatusReadResult {
    let fallback = default_status(config);
    let mut warnings = Vec::new();

    let Some(map) = raw.as_object() else {
        warnings.push("root is not a JSON object; using defaults".to_string());
        return StatusReadResult {
            status: fallback,
            warnings,
        };
    };

    // --- status ---
    let status = match map.get("status") {
        Some(v) => match serde_json::from_value::<Status>(v.clone()) {
            Ok(s) => s,
            Err(_) => {
                warnings.push(format!(
                    "field 'status': invalid value {}; falling back to {}",
                    v, fallback.status
                ));
                fallback.status
            }
        },
        None => {
            warnings.push(format!(
                "field 'status': missing; falling back to {}",
                fallback.status
            ));
            fallback.status
        }
    };

    // --- round ---
    let round = match map.get("round") {
        Some(Value::Number(value)) => match value.as_u64().and_then(|v| u32::try_from(v).ok()) {
            Some(r) => r,
            None => {
                warnings.push(format!(
                    "field 'round': invalid number {}; falling back to {}",
                    value, fallback.round
                ));
                fallback.round
            }
        },
        Some(other) => {
            warnings.push(format!(
                "field 'round': expected number, got {}; falling back to {}",
                other, fallback.round
            ));
            fallback.round
        }
        None => {
            warnings.push(format!(
                "field 'round': missing; falling back to {}",
                fallback.round
            ));
            fallback.round
        }
    };

    // --- implementer / reviewer / mode ---
    // These fields are never part of the status.json template given to agents,
    // so silently fall back to the values the binary already knows.
    let implementer = map
        .get("implementer")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.implementer.clone());

    let reviewer = map
        .get("reviewer")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.reviewer.clone());

    let mode = map
        .get("mode")
        .and_then(|v| v.as_str())
        .filter(|s| matches!(*s, "single-agent" | "dual-agent"))
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.mode.clone());

    // --- timestamp ---
    let status_timestamp = match map.get("timestamp") {
        Some(v) => match v.as_str() {
            Some(s) => s.to_owned(),
            None => {
                warnings.push(format!(
                    "field 'timestamp': expected string, got {}; falling back to current time",
                    v
                ));
                fallback.timestamp.clone()
            }
        },
        None => {
            warnings.push("field 'timestamp': missing; falling back to current time".to_string());
            fallback.timestamp.clone()
        }
    };

    // --- lastRunTask (optional — warn only when present but invalid) ---
    let last_run_task = {
        let raw_value = map.get("lastRunTask");
        match raw_value {
            Some(v) if !v.is_string() && !v.is_null() => {
                warnings.push(format!(
                    "field 'lastRunTask': expected string, got {}; ignoring",
                    v
                ));
                resolve_last_run_task(None, config)
            }
            _ => resolve_last_run_task(raw_value.and_then(Value::as_str), config),
        }
    };

    // --- reason (optional — warn only when present but invalid) ---
    let reason = match map.get("reason") {
        Some(v) if !v.is_string() && !v.is_null() => {
            warnings.push(format!(
                "field 'reason': expected string, got {}; ignoring",
                v
            ));
            None
        }
        Some(v) => v.as_str().map(ToOwned::to_owned),
        None => None,
    };

    StatusReadResult {
        status: LoopStatus {
            status,
            round,
            implementer,
            reviewer,
            mode,
            last_run_task,
            reason,
            timestamp: status_timestamp,
        },
        warnings,
    }
}

pub fn read_state_file(name: &str, config: &Config) -> String {
    fs::read_to_string(state_file_path(name, config)).unwrap_or_default()
}

pub fn write_state_file(name: &str, content: &str, config: &Config) -> io::Result<()> {
    let target = state_file_path(name, config);

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    // Build temp path in the same directory as the target so that rename is
    // guaranteed to be a same-filesystem operation (required for atomic rename).
    let tmp = {
        let mut s = target.as_os_str().to_os_string();
        s.push(".tmp");
        PathBuf::from(s)
    };

    if let Err(write_err) = fs::write(&tmp, content) {
        let _ = fs::remove_file(&tmp);
        return Err(write_err);
    }

    // fs::rename is atomic on POSIX (rename(2) overwrites target in one syscall).
    // On some Windows filesystems, rename fails when the target already exists.
    // The fallback strategy differs by platform:
    //   - POSIX: rename should always work; propagate the error if it doesn't.
    //   - Windows: try rename → remove target + retry rename → non-atomic fs::write.
    //     The non-atomic fallback is intentional: better to persist data non-atomically
    //     than to lose the write entirely.
    match fs::rename(&tmp, &target) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            if cfg!(windows) {
                // Windows fallback: remove existing target, then retry rename.
                if target.exists() {
                    let _ = fs::remove_file(&target);
                }
                if fs::rename(&tmp, &target).is_ok() {
                    return Ok(());
                }
                // Last resort: non-atomic direct write. Clean up the temp file first.
                let _ = fs::remove_file(&tmp);
                fs::write(&target, content)?;
                Ok(())
            } else {
                // On POSIX, rename(2) atomically overwrites the target, so a failure
                // indicates a real problem (permissions, cross-device, etc.). Clean up
                // the temp file and propagate the original error.
                let _ = fs::remove_file(&tmp);
                Err(rename_err)
            }
        }
    }
}

pub fn is_status_stale(expected_ts: &str, status: &LoopStatus) -> bool {
    if status.timestamp == expected_ts {
        return false;
    }

    // Treat timestamp mismatch as stale only while the loop remains in an
    // in-progress phase. Terminal/verdict statuses may legitimately carry a
    // freshly generated timestamp from the agent instead of the echoed prompt
    // timestamp.
    matches!(
        status.status,
        Status::Pending | Status::Planning | Status::Implementing | Status::Reviewing
    )
}

pub fn read_status_with_warnings(config: &Config) -> StatusReadResult {
    let raw = read_state_file("status.json", config);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return StatusReadResult {
            status: default_status(config),
            warnings: Vec::new(),
        };
    }

    let result = match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => normalize_status_value_with_warnings(&value, config),
        Err(err) => {
            let mut fallback = default_status(config);
            fallback.status = Status::Error;
            fallback.reason = Some(format!("Invalid status.json: {err}"));
            StatusReadResult {
                status: fallback,
                warnings: vec![format!("invalid JSON: {err}")],
            }
        }
    };

    for warning in &result.warnings {
        eprintln!("\u{26a0} status.json: {warning}");
    }

    result
}

pub fn read_status(config: &Config) -> LoopStatus {
    read_status_with_warnings(config).status
}

pub fn write_status(patch: StatusPatch, config: &Config) -> io::Result<LoopStatus> {
    let current = read_status(config);
    let StatusPatch {
        status,
        round,
        implementer,
        reviewer,
        mode,
        last_run_task,
        reason,
    } = patch;

    let merged_task_input = match last_run_task.as_deref() {
        Some(value) => Some(value),
        None => Some(current.last_run_task.as_str()),
    };
    let next_status = status.unwrap_or(current.status);
    let clear_stale_diagnostics = status.is_some();
    let next_reason = match reason {
        Some(value) => Some(value),
        None if clear_stale_diagnostics => None,
        None => current.reason,
    };
    let resolved_task = resolve_last_run_task(merged_task_input, config);
    let original_task_len = resolved_task.chars().count();
    let truncated_task = summarize_task(&resolved_task, Some(LAST_RUN_TASK_MAX_CHARS));
    if original_task_len > LAST_RUN_TASK_MAX_CHARS {
        let _ = log(
            &format!(
                "⚠ last_run_task truncated: {original_task_len} chars -> {LAST_RUN_TASK_MAX_CHARS} chars"
            ),
            config,
        );
    }

    let updated = LoopStatus {
        status: next_status,
        round: round.unwrap_or(current.round),
        implementer: implementer.unwrap_or_else(|| config.implementer.to_string()),
        reviewer: reviewer.unwrap_or_else(|| config.reviewer.to_string()),
        mode: mode.unwrap_or_else(|| config.run_mode.to_string()),
        last_run_task: truncated_task,
        reason: next_reason,
        timestamp: timestamp(),
    };

    let serialized = serde_json::to_string_pretty(&updated).map_err(io::Error::other)?;
    write_state_file("status.json", &serialized, config)?;

    Ok(updated)
}

pub fn log(msg: &str, config: &Config) -> io::Result<()> {
    let line = format!("[{}] {}", timestamp(), msg);
    println!("{line}");

    let log_path = state_file_path("log.txt", config);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(file, "{line}")
}

/// Delete all `*_session_id` files from the state directory.
pub fn cleanup_session_files(config: &Config) -> io::Result<usize> {
    fn cleanup_dir(dir: &Path) -> io::Result<usize> {
        let mut deleted = 0usize;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                deleted += cleanup_dir(&entry.path())?;
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let name = entry.file_name();
            if name.to_string_lossy().ends_with("_session_id") {
                fs::remove_file(entry.path())?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    if config.session.is_some() {
        // Session-scoped: existing recursive behavior is correct.
        // config.state_dir is already scoped to the session directory.
        let deleted = match fs::read_dir(&config.state_dir) {
            Ok(_) => cleanup_dir(&config.state_dir)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };

        log(&format!("Cleaned up {deleted} session file(s)"), config)?;
        Ok(deleted)
    } else {
        // Default session: only walk top-level files and .wave-task-* dirs.
        let mut removed = 0;
        match fs::read_dir(&config.state_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() {
                        if path.to_string_lossy().ends_with("_session_id") {
                            fs::remove_file(&path)?;
                            removed += 1;
                        }
                    } else if path.is_dir() {
                        let name = entry.file_name();
                        if name.to_string_lossy().starts_with(".wave-task-") {
                            removed += cleanup_dir(&path)?;
                        }
                        // Skip session namespace directories
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        log(&format!("Cleaned up {removed} session file(s)"), config)?;
        Ok(removed)
    }
}

const WAVE_TASK_MIGRATION_SENTINEL: &str = ".wave-task-migration-v1";

/// One-time migration: renames pre-existing `task-{N}` wave task directories
/// to `.wave-task-{N}`.
///
/// Called from `state::init` (covers implementation/planning commands) and
/// `status_command` in main.rs (covers `agent-loop status`, which bypasses
/// `state::init`).
///
/// The sentinel is written **after** all renames complete so that a crash
/// mid-migration allows the next startup to retry remaining directories.
/// The rename loop is idempotent: `is_legacy_wave_task_dir` only matches
/// `task-\d+` directories with wave markers, and renames are skipped when
/// the target `.wave-task-{N}` already exists or the source has vanished
/// (concurrent process). Re-running after a partial migration simply
/// finishes any outstanding renames.
///
/// Session collision safety: migration runs in `state::init` after
/// `create_dir_all(config.state_dir)` but before any state files are
/// written. A session directory like `task-1/` created in the same call
/// is empty at scan time, so `is_legacy_wave_task_dir` returns false and
/// skips it. After the sentinel is written, no future startup will attempt
/// renames at all.
pub(crate) fn migrate_legacy_wave_task_dirs(
    state_dir: &Path,
    agent_loop_dir: &Path,
) -> Result<(), crate::error::AgentLoopError> {
    if !agent_loop_dir.is_dir() {
        return Ok(());
    }
    let sentinel = agent_loop_dir.join(WAVE_TASK_MIGRATION_SENTINEL);
    if sentinel.exists() {
        return Ok(());
    }
    if state_dir.is_dir() {
        for entry in fs::read_dir(state_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !is_legacy_wave_task_dir(&name_str, &path) {
                continue;
            }
            let suffix = &name_str["task-".len()..];
            let new_name = format!(".wave-task-{suffix}");
            let new_path = state_dir.join(&new_name);
            if !new_path.exists() {
                match fs::rename(&path, &new_path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        // Another process already renamed this directory
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    // Write sentinel after all renames so a crash mid-loop allows retry.
    fs::write(&sentinel, "")?;
    Ok(())
}

/// Detects pre-rename wave task directories: matches `task-\d+` names that
/// contain marker files (`implement-progress.md` or `*_session_id`).
pub(crate) fn is_legacy_wave_task_dir(name: &str, path: &Path) -> bool {
    if !name.starts_with("task-") {
        return false;
    }
    let suffix = &name["task-".len()..];
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    path.join("implement-progress.md").exists()
        || fs::read_dir(path).ok().is_some_and(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                e.file_name().to_string_lossy().ends_with("_session_id")
            })
        })
}

/// Returns the path to the migration sentinel file.
pub(crate) fn wave_task_migration_sentinel(agent_loop_dir: &Path) -> PathBuf {
    agent_loop_dir.join(WAVE_TASK_MIGRATION_SENTINEL)
}

pub fn append_round_summary(
    round: u32,
    phase: &str,
    summary: &str,
    config: &Config,
) -> io::Result<()> {
    let normalized = summarize_task(summary, Some(120));
    let line = format!("Round {round} {phase}: {normalized}\n");
    let path = state_file_path("conversation.md", config);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;

    cap_conversation_file(config)
}

fn cap_conversation_file(config: &Config) -> io::Result<()> {
    let path = state_file_path("conversation.md", config);
    let content = fs::read_to_string(&path)?;
    let lines: Vec<&str> = content.lines().collect();
    let original_count = lines.len();

    if original_count <= CONVERSATION_MAX_LINES {
        return Ok(());
    }

    let _ = log(
        &format!(
            "⚠ conversation.md capped: {original_count} lines -> {CONVERSATION_MAX_LINES} lines"
        ),
        config,
    );

    let kept = &lines[original_count - CONVERSATION_MAX_LINES..];
    let mut capped = kept.join("\n");
    capped.push('\n');
    fs::write(&path, capped)
}

// ---------------------------------------------------------------------------
// Reviewer findings persistence (findings.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingEntry {
    pub id: String,
    pub severity: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingsFile {
    pub round: u32,
    pub findings: Vec<FindingEntry>,
}

#[derive(Debug, Clone)]
pub struct FindingsReadResult {
    pub findings_file: FindingsFile,
    #[allow(dead_code)]
    pub warnings: Vec<String>,
}

pub fn read_findings_with_warnings(config: &Config) -> FindingsReadResult {
    let raw = read_state_file(FINDINGS_FILENAME, config);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return FindingsReadResult {
            findings_file: FindingsFile::default(),
            warnings: Vec::new(),
        };
    }

    match serde_json::from_str::<FindingsFile>(trimmed) {
        Ok(findings_file) => FindingsReadResult {
            findings_file,
            warnings: Vec::new(),
        },
        Err(err) => {
            let warning = format!("invalid {FINDINGS_FILENAME}: {err}; starting fresh");
            eprintln!("\u{26a0} {warning}");
            FindingsReadResult {
                findings_file: FindingsFile::default(),
                warnings: vec![warning],
            }
        }
    }
}

pub fn read_findings(config: &Config) -> FindingsFile {
    read_findings_with_warnings(config).findings_file
}

pub fn write_findings(findings: &FindingsFile, config: &Config) -> io::Result<()> {
    let serialized = serde_json::to_string_pretty(findings).map_err(io::Error::other)?;
    write_state_file(FINDINGS_FILENAME, &serialized, config)
}

/// Check if review text ends with a standalone "no findings" line (case-insensitive).
/// This is the simplified planning verdict: if the reviewer's last non-empty
/// line is exactly "no findings", the plan is approved.
/// A line like "I cannot say no findings" will NOT match.
pub fn review_has_no_findings(review_text: &str) -> bool {
    review_text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .is_some_and(|last| {
            let stripped = last.trim_end_matches(['.', '!', ':', ';']);
            stripped.eq_ignore_ascii_case("no findings")
        })
}

pub fn append_planning_progress(
    round: u32,
    summary: &str,
    findings_summary: Option<&str>,
    config: &Config,
) {
    let path = config.state_dir.join("planning-progress.md");
    let full_summary = match findings_summary {
        Some(findings) => format!("{summary}\n{findings}"),
        None => summary.to_string(),
    };
    let _ = append_round_progress(&path, round, &full_summary);
}

const IMPLEMENT_PROGRESS_TASK_HEADING_PREFIX: &str = "### Task: ";

fn last_progress_round(content: &str) -> Option<u32> {
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with(IMPLEMENT_PROGRESS_TASK_HEADING_PREFIX) {
            return None;
        }
        if let Some(round) = trimmed
            .strip_prefix("## Round ")
            .and_then(|value| value.parse::<u32>().ok())
        {
            return Some(round);
        }
    }
    None
}

fn append_round_progress(path: &Path, round: u32, summary: &str) -> io::Result<()> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let existing = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };

    let heading = format!("## Round {round}");
    let mut updated = if existing.trim().is_empty() {
        format!("{heading}\n{summary}\n")
    } else if last_progress_round(&existing) == Some(round) {
        let mut content = existing;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(summary);
        content.push('\n');
        content
    } else {
        let mut content = existing;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        if !content.ends_with("\n\n") {
            content.push('\n');
        }
        content.push_str(&heading);
        content.push('\n');
        content.push_str(summary);
        content.push('\n');
        content
    };

    // Keep files tidy when the previous contents were whitespace-only.
    if updated.starts_with('\n') {
        updated = updated.trim_start_matches('\n').to_string();
    }

    fs::write(path, updated)
}

fn clear_progress_file(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, "")
}

fn append_progress_heading(path: &Path, heading: &str) -> io::Result<()> {
    let heading = heading.trim();
    if heading.is_empty() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let existing = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };

    let mut updated = existing;
    if !updated.trim().is_empty() {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        if !updated.ends_with("\n\n") {
            updated.push('\n');
        }
    }
    updated.push_str(heading);
    updated.push('\n');

    fs::write(path, updated)
}

pub fn append_tasks_progress(
    round: u32,
    summary: &str,
    findings_summary: Option<&str>,
    config: &Config,
) {
    let path = config.state_dir.join("tasks-progress.md");
    let full_summary = match findings_summary {
        Some(findings) => format!("{summary}\n{findings}"),
        None => summary.to_string(),
    };
    let _ = append_round_progress(&path, round, &full_summary);
}

pub fn clear_tasks_progress(config: &Config) {
    let path = config.state_dir.join("tasks-progress.md");
    let _ = clear_progress_file(&path);
}

pub fn append_implement_progress(round: u32, summary: &str, config: &Config) {
    let path = config.state_dir.join("implement-progress.md");
    let _ = append_round_progress(&path, round, summary);
}

pub fn append_implement_progress_task(task_title: &str, config: &Config) {
    let path = config.state_dir.join("implement-progress.md");
    let heading = format!("{IMPLEMENT_PROGRESS_TASK_HEADING_PREFIX}{task_title}");
    let _ = append_progress_heading(&path, &heading);
}

pub fn clear_implement_progress(config: &Config) {
    let path = config.state_dir.join("implement-progress.md");
    let _ = clear_progress_file(&path);
}

// ---------------------------------------------------------------------------
// Tasks (decomposition) findings (tasks_findings.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TasksFindingStatus {
    Open,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasksFindingEntry {
    pub id: String,
    pub description: String,
    pub status: TasksFindingStatus,
    pub round_introduced: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round_resolved: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasksFindingsFile {
    pub findings: Vec<TasksFindingEntry>,
}

pub fn read_tasks_findings(config: &Config) -> TasksFindingsFile {
    let raw = read_state_file(TASKS_FINDINGS_FILENAME, config);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return TasksFindingsFile::default();
    }
    serde_json::from_str(trimmed).unwrap_or_default()
}

pub fn write_tasks_findings(findings: &TasksFindingsFile, config: &Config) -> io::Result<()> {
    let serialized = serde_json::to_string_pretty(findings).map_err(io::Error::other)?;
    write_state_file(TASKS_FINDINGS_FILENAME, &serialized, config)
}

pub fn clear_tasks_findings(config: &Config) {
    let path = config.state_dir.join(TASKS_FINDINGS_FILENAME);
    let _ = fs::remove_file(path);
}

pub fn open_tasks_findings_for_prompt(findings: &TasksFindingsFile) -> String {
    let open: Vec<&TasksFindingEntry> = findings
        .findings
        .iter()
        .filter(|f| f.status == TasksFindingStatus::Open)
        .collect();
    if open.is_empty() {
        return String::new();
    }
    let mut out = String::from("Open tasks findings:\n");
    for f in &open {
        out.push_str(&format!("- {}: {}\n", f.id, f.description));
    }
    out
}

pub fn next_tasks_finding_id(findings: &TasksFindingsFile) -> String {
    let max_num = findings
        .findings
        .iter()
        .filter_map(|f| f.id.strip_prefix("T-").and_then(|n| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(0);
    format!("T-{:03}", max_num + 1)
}

// ---------------------------------------------------------------------------
// Task lifecycle persistence (task_status.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskRunStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl fmt::Display for TaskRunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        };
        write!(f, "{label}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatusEntry {
    pub title: String,
    pub status: TaskRunStatus,
    pub retries: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wave_index: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatusFile {
    pub tasks: Vec<TaskStatusEntry>,
}

#[derive(Debug, Clone)]
pub struct TaskStatusReadResult {
    pub status_file: TaskStatusFile,
    #[allow(dead_code)]
    pub warnings: Vec<String>,
}

pub fn read_task_status_with_warnings(config: &Config) -> TaskStatusReadResult {
    let raw = read_state_file("task_status.json", config);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return TaskStatusReadResult {
            status_file: TaskStatusFile::default(),
            warnings: Vec::new(),
        };
    }

    match serde_json::from_str::<TaskStatusFile>(trimmed) {
        Ok(status_file) => TaskStatusReadResult {
            status_file,
            warnings: Vec::new(),
        },
        Err(err) => {
            let warning = format!("invalid task_status.json: {err}; starting fresh");
            eprintln!("\u{26a0} {warning}");
            TaskStatusReadResult {
                status_file: TaskStatusFile::default(),
                warnings: vec![warning],
            }
        }
    }
}

#[allow(dead_code)]
pub fn read_task_status(config: &Config) -> TaskStatusFile {
    read_task_status_with_warnings(config).status_file
}

pub fn write_task_status(status: &TaskStatusFile, config: &Config) -> io::Result<()> {
    let serialized = serde_json::to_string_pretty(status).map_err(io::Error::other)?;
    write_state_file("task_status.json", &serialized, config)
}

// ---------------------------------------------------------------------------
// Task timing metrics (task_metrics.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskMetricsEntry {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_ended_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_calls: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskMetricsFile {
    pub tasks: Vec<TaskMetricsEntry>,
}

#[derive(Debug, Clone)]
pub struct TaskMetricsReadResult {
    pub metrics_file: TaskMetricsFile,
    #[allow(dead_code)]
    pub warnings: Vec<String>,
}

pub fn read_task_metrics_with_warnings(config: &Config) -> TaskMetricsReadResult {
    let raw = read_state_file("task_metrics.json", config);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return TaskMetricsReadResult {
            metrics_file: TaskMetricsFile::default(),
            warnings: Vec::new(),
        };
    }

    match serde_json::from_str::<TaskMetricsFile>(trimmed) {
        Ok(metrics_file) => TaskMetricsReadResult {
            metrics_file,
            warnings: Vec::new(),
        },
        Err(err) => {
            let warning = format!("invalid task_metrics.json: {err}; starting fresh");
            eprintln!("\u{26a0} {warning}");
            TaskMetricsReadResult {
                metrics_file: TaskMetricsFile::default(),
                warnings: vec![warning],
            }
        }
    }
}

pub fn read_task_metrics(config: &Config) -> TaskMetricsFile {
    read_task_metrics_with_warnings(config).metrics_file
}

pub fn write_task_metrics(metrics: &TaskMetricsFile, config: &Config) -> io::Result<()> {
    let serialized = serde_json::to_string_pretty(metrics).map_err(io::Error::other)?;
    write_state_file("task_metrics.json", &serialized, config)
}

pub fn read_recent_history(config: &Config, max_lines: usize) -> String {
    let content = read_state_file("conversation.md", config);
    if content.trim().is_empty() {
        return String::new();
    }

    let non_empty_lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    if non_empty_lines.len() <= max_lines {
        non_empty_lines.join("\n")
    } else {
        non_empty_lines[non_empty_lines.len() - max_lines..].join("\n")
    }
}

pub fn write_workflow(kind: WorkflowKind, config: &Config) -> io::Result<()> {
    write_state_file("workflow.txt", &format!("{kind}\n"), config)
}

#[allow(dead_code)] // used in tests; resume_command uses the pre-config state_dir variant
pub fn read_workflow(config: &Config) -> Option<WorkflowKind> {
    let raw = read_state_file("workflow.txt", config);
    raw.trim().parse().ok()
}

pub fn init(
    task: &str,
    config: &Config,
    baseline_files: &[String],
    workflow: WorkflowKind,
) -> io::Result<()> {
    fs::create_dir_all(&config.state_dir)?;

    // One-time migration: rename legacy task-{N} wave task dirs to .wave-task-{N}.
    let agent_loop_dir = config.agent_loop_dir();
    let base_state_dir = agent_loop_dir.join("state");
    migrate_legacy_wave_task_dirs(&base_state_dir, &agent_loop_dir)
        .map_err(|e| io::Error::other(e.to_string()))?;

    // Decisions subsystem initialization
    if config.decisions_enabled {
        let decisions = decisions_path(config);
        if !decisions.exists() {
            if let Some(parent) = decisions.parent() {
                fs::create_dir_all(parent)?;
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(decisions)?;
        }

        if config.decisions_auto_reference {
            ensure_project_guide_decisions_references(config);
        } else {
            remove_project_guide_decisions_references(config);
        }
    } else {
        remove_project_guide_decisions_references(config);
    }
    // Accepted for API parity with the TypeScript implementation's checkpoint baseline flow.
    let _baseline_files = baseline_files;

    // Clear stale quality check output when starting a new workflow. A fresh
    // run should not inherit old results if checks are skipped this round.
    let quality_checks_path = state_file_path(QUALITY_CHECKS_FILENAME, config);
    match fs::remove_file(&quality_checks_path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    write_state_file("task.md", task, config)?;
    write_state_file("plan.md", "", config)?;
    write_state_file("review.md", "", config)?;
    write_findings(&FindingsFile::default(), config)?;
    write_state_file("changes.md", "", config)?;
    write_state_file("conversation.md", "", config)?;
    write_state_file("log.txt", "", config)?;
    write_workflow(workflow, config)?;

    write_status(
        StatusPatch {
            status: Some(Status::Pending),
            round: Some(0),
            implementer: Some(config.implementer.to_string()),
            reviewer: Some(config.reviewer.to_string()),
            mode: Some(config.run_mode.to_string()),
            ..StatusPatch::default()
        },
        config,
    )?;

    log("Agent loop initialized", config)?;
    log(
        &format!("Task: {}", summarize_task(task, Some(100))),
        config,
    )?;
    log(
        &format!(
            "Implementer: {} | Reviewer: {}",
            config.implementer, config.reviewer
        ),
        config,
    )?;
    log(&format!("Mode: {}", config.run_mode), config)?;
    log(
        &format!(
            "Max rounds: {} | Timeout: {}s",
            config.review_max_rounds, config.timeout_seconds
        ),
        config,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestProject;
    use serde_json::json;
    use std::thread;

    fn new_project() -> TestProject {
        TestProject::builder("agent_loop_state_test").build()
    }

    fn is_timestamp_shape(value: &str) -> bool {
        if value.len() != 24 {
            return false;
        }

        let bytes = value.as_bytes();
        let digit_positions = [
            0usize, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22,
        ];

        for idx in digit_positions {
            if !bytes[idx].is_ascii_digit() {
                return false;
            }
        }

        bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[19] == b'.'
            && bytes[23] == b'Z'
    }

    #[test]
    fn loop_status_serializes_last_run_task_in_camel_case() {
        let status = LoopStatus {
            status: Status::Pending,
            round: 0,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "task".to_string(),
            reason: None,

            timestamp: "2026-02-14T00:00:00.000Z".to_string(),
        };

        let value = serde_json::to_value(status).expect("status should serialize");
        let object = value.as_object().expect("status should be an object");

        assert!(object.contains_key("lastRunTask"));
        assert!(!object.contains_key("last_run_task"));
    }

    #[test]
    fn normalize_and_summarize_task_helpers_match_expected_boundaries() {
        assert_eq!(
            normalize_task_text("  Task 1:\n  build    feature   "),
            "Task 1: build feature"
        );
        assert_eq!(
            extract_task_title(
                "# Order Waiting List Status Implementation Plan\n\n## Summary\nLong body text"
            ),
            "Order Waiting List Status Implementation Plan"
        );
        assert_eq!(
            extract_task_title("   First line title  \nSecond line details"),
            "First line title Second line details"
        );
        assert_eq!(
            extract_task_title("```md\n# inside code fence\n```\n# Real Title"),
            "Real Title"
        );
        assert_eq!(summarize_task("short task", Some(20)), "short task");
        assert_eq!(
            summarize_task("12345678901234567890", Some(10)),
            "1234567..."
        );
        assert_eq!(summarize_task("abcdef", Some(3)), "...");
    }

    #[test]
    fn timestamp_uses_utc_iso8601_milliseconds_format() {
        let value = timestamp();
        assert!(is_timestamp_shape(&value));
    }

    #[test]
    fn normalize_status_value_falls_back_per_field_on_bad_types() {
        let project = new_project();
        write_state_file("task.md", "  fallback   task  ", &project.config)
            .expect("task.md should be writable");

        let raw = json!({
            "status": "REVIEWING",
            "round": "1",
            "implementer": "custom-implementer",
            "reviewer": 42,
            "mode": "single-agent",
            "lastRunTask": false,
            "reason": 12,
            "timestamp": "2026-02-14T12:30:15.987Z"
        });

        let normalized = normalize_status_value(&raw, &project.config);

        assert_eq!(normalized.status, Status::Reviewing);
        assert_eq!(normalized.round, 0);
        assert_eq!(normalized.implementer, "custom-implementer");
        assert_eq!(normalized.reviewer, "codex");
        assert_eq!(normalized.mode, "single-agent");
        assert_eq!(normalized.last_run_task, "fallback task");
        assert_eq!(normalized.reason, None);
        assert_eq!(normalized.timestamp, "2026-02-14T12:30:15.987Z");
    }

    #[test]
    fn read_status_uses_defaults_for_missing_empty_and_error_for_invalid_json() {
        let project = new_project();
        write_state_file(
            "task.md",
            "   # task from state   \n\nfull details",
            &project.config,
        )
        .expect("task.md should be writable");

        let missing = read_status(&project.config);
        assert_eq!(missing.status, Status::Pending);
        assert_eq!(missing.round, 0);
        assert_eq!(missing.last_run_task, "task from state");

        write_state_file("status.json", "   ", &project.config)
            .expect("empty status.json should be writable");
        let empty = read_status(&project.config);
        assert_eq!(empty.status, Status::Pending);
        assert_eq!(empty.round, 0);
        assert_eq!(empty.last_run_task, "task from state");

        write_state_file("status.json", "{broken", &project.config)
            .expect("invalid status.json should be writable");
        let invalid = read_status(&project.config);
        assert_eq!(invalid.status, Status::Error);
        assert_eq!(invalid.round, 0);
        assert_eq!(invalid.last_run_task, "task from state");
        assert!(
            invalid
                .reason
                .as_deref()
                .is_some_and(|value| value.starts_with("Invalid status.json:"))
        );
    }

    #[test]
    fn write_status_round_trip_clears_stale_reason_on_status_transition() {
        let project = new_project();
        write_state_file("task.md", "fallback task", &project.config)
            .expect("task.md should be writable");

        let first = write_status(
            StatusPatch {
                status: Some(Status::Planning),
                round: Some(2),
                implementer: Some("custom-impl".to_string()),
                reviewer: Some("custom-reviewer".to_string()),
                mode: Some("single-agent".to_string()),
                last_run_task: Some("  direct   task ".to_string()),
                reason: Some("needs follow-up".to_string()),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("first status write should succeed");

        assert_eq!(first.status, Status::Planning);
        assert_eq!(first.round, 2);
        assert_eq!(first.implementer, "custom-impl");
        assert_eq!(first.reviewer, "custom-reviewer");
        assert_eq!(first.mode, "single-agent");
        assert_eq!(first.last_run_task, "direct task");
        assert_eq!(first.reason, Some("needs follow-up".to_string()));

        let second = write_status(
            StatusPatch {
                status: Some(Status::Reviewing),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("second status write should succeed");

        assert_eq!(second.status, Status::Reviewing);
        assert_eq!(second.round, 2);
        assert_eq!(second.implementer, "claude");
        assert_eq!(second.reviewer, "codex");
        assert_eq!(second.mode, "dual-agent");
        assert_eq!(second.last_run_task, "direct task");
        assert_eq!(second.reason, None);

        let reloaded = read_status(&project.config);
        assert_eq!(reloaded, second);
    }

    #[test]
    fn init_creates_expected_files_and_initial_status() {
        let mut project = new_project();
        project.config.decisions_enabled = true;
        let baseline_files = vec!["src/main.rs".to_string()];

        init(
            "  Build\n a   robust  state module ",
            &project.config,
            &baseline_files,
            WorkflowKind::Implement,
        )
        .expect("init should succeed");

        for name in [
            "task.md",
            "plan.md",
            "review.md",
            "findings.json",
            "changes.md",
            "log.txt",
            "status.json",
            "workflow.txt",
        ] {
            assert!(
                state_file_path(name, &project.config).exists(),
                "{name} should exist after init"
            );
        }
        assert!(
            decisions_path(&project.config).exists(),
            "decisions.md should exist after init"
        );
        for guide in ["AGENTS.md", "CLAUDE.md"] {
            let guide_path = project.root.join(guide);
            let guide_content =
                fs::read_to_string(&guide_path).expect("guide should be created during init");
            assert!(
                guide_content.contains(".agent-loop/decisions.md"),
                "{guide} should reference decisions.md"
            );
            assert!(
                guide_content.contains(DECISIONS_REFERENCE_START),
                "{guide} should contain managed decisions reference marker"
            );
        }

        let status = read_status(&project.config);
        assert_eq!(status.status, Status::Pending);
        assert_eq!(status.round, 0);
        assert_eq!(status.implementer, "claude");
        assert_eq!(status.reviewer, "codex");
        assert_eq!(status.mode, "dual-agent");
        assert_eq!(status.last_run_task, "Build a robust state module");
        assert_eq!(status.reason, None);

        let workflow = read_workflow(&project.config);
        assert_eq!(workflow, Some(WorkflowKind::Implement));

        let log_content = read_state_file("log.txt", &project.config);
        assert!(log_content.contains("Agent loop initialized"));
        assert!(log_content.contains("Task: Build a robust state module"));
    }

    #[test]
    fn init_clears_stale_quality_checks_file() {
        let project = new_project();
        fs::create_dir_all(&project.config.state_dir).expect("state dir should be created");
        write_state_file(
            QUALITY_CHECKS_FILENAME,
            "QUALITY CHECKS:\n\n--- stale [PASS] ---\nold",
            &project.config,
        )
        .expect("stale quality checks should be written");

        init("fresh task", &project.config, &[], WorkflowKind::Implement)
            .expect("init should succeed");

        assert!(
            !state_file_path(QUALITY_CHECKS_FILENAME, &project.config).exists(),
            "quality checks file should be removed during init"
        );
    }

    #[test]
    fn cleanup_session_files_removes_root_and_nested_session_files_only() {
        let project = new_project();
        let nested_dir = project.config.state_dir.join(".wave-task-1");
        fs::create_dir_all(&nested_dir).expect("nested state dir should exist");

        fs::write(
            project
                .config
                .state_dir
                .join("implement-implementer_session_id"),
            "root",
        )
        .expect("root session file should be written");
        fs::write(nested_dir.join("plan-reviewer_session_id"), "nested")
            .expect("nested session file should be written");
        fs::write(project.config.state_dir.join("workflow.txt"), "implement\n")
            .expect("workflow marker should be written");
        fs::write(nested_dir.join("changes.md"), "summary")
            .expect("changes file should be written");

        let deleted = cleanup_session_files(&project.config).expect("cleanup should succeed");

        assert_eq!(deleted, 2, "expected both session files to be removed");
        assert!(
            !project
                .config
                .state_dir
                .join("implement-implementer_session_id")
                .exists()
        );
        assert!(!nested_dir.join("plan-reviewer_session_id").exists());
        assert!(
            project.config.state_dir.join("workflow.txt").exists(),
            "workflow.txt must be preserved"
        );
        assert!(
            nested_dir.join("changes.md").exists(),
            "changes.md must be preserved"
        );

        let log_content = fs::read_to_string(project.config.state_dir.join("log.txt"))
            .expect("log.txt should be readable");
        assert!(
            log_content.contains("Cleaned up 2 session file(s)"),
            "cleanup log line should be recorded, got:\n{log_content}"
        );
    }

    #[test]
    fn cleanup_session_files_returns_zero_and_logs_when_state_dir_is_missing() {
        use crate::test_support::{TestConfigOptions, create_temp_project_root, make_test_config};

        let root = create_temp_project_root("cleanup_missing_state_dir");
        let config = make_test_config(&root, TestConfigOptions::default());

        let deleted = cleanup_session_files(&config).expect("cleanup should succeed");

        assert_eq!(deleted, 0, "missing state dir should behave like no files");
        let log_content =
            fs::read_to_string(config.state_dir.join("log.txt")).expect("log.txt should exist");
        assert!(
            log_content.contains("Cleaned up 0 session file(s)"),
            "cleanup log line should be recorded, got:\n{log_content}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn init_decisions_reference_blocks_are_idempotent() {
        let mut project = new_project();
        project.config.decisions_enabled = true;
        let baseline_files = vec!["src/main.rs".to_string()];

        init(
            "First task",
            &project.config,
            &baseline_files,
            WorkflowKind::Implement,
        )
        .expect("first init should succeed");
        init(
            "Second task",
            &project.config,
            &baseline_files,
            WorkflowKind::Implement,
        )
        .expect("second init should succeed");

        for guide in ["AGENTS.md", "CLAUDE.md"] {
            let guide_content =
                fs::read_to_string(project.root.join(guide)).expect("guide should exist");
            assert_eq!(
                guide_content.matches(DECISIONS_REFERENCE_START).count(),
                1,
                "{guide} should contain exactly one managed decisions block"
            );
        }
    }

    #[test]
    fn manual_decisions_reference_is_preserved_without_managed_block() {
        let project = new_project();
        let path = project.root.join("AGENTS.md");
        let original = "Repository guide\nAlways check .agent-loop/decisions.md first.\n";
        fs::write(&path, original).expect("manual guide should be writable");

        let changed =
            ensure_decisions_reference_file(&path).expect("guide synchronization should succeed");
        assert!(!changed, "manual decisions reference should be preserved");
        assert_eq!(
            fs::read_to_string(&path).expect("guide should be readable"),
            original
        );
    }

    #[test]
    fn decisions_path_is_project_level_sibling_of_state_dir() {
        let project = new_project();
        let path = decisions_path(&project.config);
        assert_eq!(path, project.root.join(".agent-loop").join("decisions.md"));
    }

    #[test]
    fn read_decisions_missing_file_returns_empty() {
        let project = new_project();
        assert_eq!(read_decisions(&project.config), "");
    }

    #[test]
    fn append_decision_noops_for_empty_entry_and_appends_non_empty_entry() {
        let mut project = new_project();
        project.config.decisions_enabled = true;

        append_decision("   ", &project.config).expect("empty append should succeed");
        assert_eq!(read_decisions(&project.config), "");

        append_decision("- [PATTERN] Use helper modules", &project.config)
            .expect("append should succeed");
        append_decision("- [CONSTRAINT] Keep API stable", &project.config)
            .expect("append should succeed");

        let content = read_decisions(&project.config);
        assert!(content.contains("- [PATTERN] Use helper modules"));
        assert!(content.contains("- [CONSTRAINT] Keep API stable"));
    }

    #[test]
    fn read_decisions_returns_last_n_lines() {
        let mut project = new_project();
        project.config.decisions_enabled = true;
        project.config.decisions_max_lines = 2;

        append_decision("- [ARCHITECTURE] A", &project.config).expect("append should succeed");
        append_decision("- [PATTERN] B", &project.config).expect("append should succeed");
        append_decision("- [GOTCHA] C", &project.config).expect("append should succeed");

        let content = read_decisions(&project.config);
        assert!(!content.contains("- [ARCHITECTURE] A"));
        assert!(content.contains("- [PATTERN] B"));
        assert!(content.contains("- [GOTCHA] C"));
    }

    #[test]
    fn write_status_preserves_reason_when_status_is_unchanged() {
        let project = new_project();
        write_state_file("task.md", "test task", &project.config)
            .expect("task.md should be writable");

        let initial = write_status(
            StatusPatch {
                status: Some(Status::NeedsChanges),
                reason: Some("missing test coverage".to_string()),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("initial write should succeed");
        assert_eq!(initial.reason.as_deref(), Some("missing test coverage"));

        let updated_task = write_status(
            StatusPatch {
                last_run_task: Some("  updated task title ".to_string()),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("task-only write should succeed");

        assert_eq!(updated_task.status, Status::NeedsChanges);
        assert_eq!(
            updated_task.reason.as_deref(),
            Some("missing test coverage")
        );
        assert_eq!(updated_task.last_run_task, "updated task title");
    }

    #[test]
    fn status_serde_round_trip_covers_all_variants() {
        let variants = [
            Status::Pending,
            Status::Planning,
            Status::Implementing,
            Status::Reviewing,
            Status::Approved,
            Status::Consensus,
            Status::Disputed,
            Status::NeedsChanges,
            Status::NeedsRevision,
            Status::MaxRounds,
            Status::Error,
            Status::Interrupted,
        ];

        for variant in variants {
            let serialized = serde_json::to_value(variant)
                .unwrap_or_else(|_| panic!("{variant:?} should serialize"));
            let deserialized: Status = serde_json::from_value(serialized.clone())
                .unwrap_or_else(|_| panic!("{variant:?} should deserialize from {serialized}"));
            assert_eq!(
                variant, deserialized,
                "{variant:?} should survive round-trip"
            );
        }
    }

    #[test]
    fn status_display_matches_serde_serialization() {
        let variants = [
            Status::Pending,
            Status::Planning,
            Status::Implementing,
            Status::Reviewing,
            Status::Approved,
            Status::Consensus,
            Status::Disputed,
            Status::NeedsChanges,
            Status::NeedsRevision,
            Status::MaxRounds,
            Status::Error,
            Status::Interrupted,
        ];

        for variant in variants {
            let display_output = variant.to_string();
            let serde_value = serde_json::to_value(variant)
                .unwrap_or_else(|_| panic!("{variant:?} should serialize"));
            let serde_string = serde_value
                .as_str()
                .unwrap_or_else(|| panic!("{variant:?} should serialize to a string"));
            assert_eq!(
                display_output, serde_string,
                "Display and serde should produce identical output for {variant:?}"
            );
        }
    }

    #[test]
    fn normalize_status_value_falls_back_for_unknown_status_string() {
        let project = new_project();

        let raw = json!({
            "status": "UNKNOWN_STATUS",
            "round": 3
        });

        let normalized = normalize_status_value(&raw, &project.config);
        assert_eq!(
            normalized.status,
            Status::Pending,
            "unrecognized status string should fall back to Pending"
        );
        assert_eq!(normalized.round, 3, "other fields should still be parsed");
    }

    #[test]
    fn civil_from_days_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_from_days_leap_year() {
        // 2000 is a leap year (divisible by 400)
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn civil_from_days_century_boundary() {
        // 1900 is NOT a leap year (divisible by 100 but not 400)
        assert_eq!(civil_from_days(-25_508), (1900, 3, 1));
    }

    #[test]
    fn civil_from_days_recent_date() {
        assert_eq!(civil_from_days(20_254), (2025, 6, 15));
    }

    #[test]
    fn write_state_file_writes_valid_json() {
        let project = new_project();
        let payload = serde_json::json!({"status": "PENDING", "round": 0});
        let content = serde_json::to_string_pretty(&payload).unwrap();

        write_state_file("status.json", &content, &project.config).expect("write should succeed");

        let raw = fs::read_to_string(state_file_path("status.json", &project.config))
            .expect("file should be readable");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("file should contain valid JSON");
        assert_eq!(parsed["status"], "PENDING");
        assert_eq!(parsed["round"], 0);
    }

    #[test]
    fn write_state_file_creates_parent_directory_when_missing() {
        use crate::test_support::{TestConfigOptions, create_temp_project_root, make_test_config};

        let root = create_temp_project_root("atomic_write_parent_test");
        let nested_state_dir = root.join("deep").join("nested").join("state");
        let config = Config {
            state_dir: nested_state_dir.clone(),
            ..make_test_config(&root, TestConfigOptions::default())
        };

        assert!(!nested_state_dir.exists());

        write_state_file("data.json", "{}", &config)
            .expect("write with missing parent should succeed");

        assert!(nested_state_dir.exists());
        assert!(nested_state_dir.join("data.json").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_state_file_prevents_partial_reads_via_temp_then_rename() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        const WRITE_ITERATIONS: u64 = 200;

        let project = new_project();
        let status_path = state_file_path("status.json", &project.config);

        // Seed a valid initial file.
        let initial = serde_json::json!({"version": 0, "data": "x".repeat(512)});
        write_state_file(
            "status.json",
            &serde_json::to_string_pretty(&initial).unwrap(),
            &project.config,
        )
        .expect("seed write should succeed");

        let done = Arc::new(AtomicBool::new(false));
        let done_writer = Arc::clone(&done);

        let read_path = status_path.clone();
        let reader = thread::spawn(move || {
            let mut reads = 0u64;
            while !done.load(Ordering::Relaxed) {
                if let Ok(raw) = fs::read_to_string(&read_path)
                    && !raw.trim().is_empty()
                {
                    let parsed: serde_json::Value = serde_json::from_str(&raw)
                        .unwrap_or_else(|_| panic!("reader saw partial/invalid JSON: {raw}"));

                    // Assert the payload is one of the expected versions, not just
                    // any valid JSON. This catches corruption that happens to produce
                    // a parseable but unexpected document.
                    let version = parsed["version"]
                        .as_u64()
                        .unwrap_or_else(|| panic!("missing or non-integer 'version' field: {raw}"));
                    assert!(
                        version <= WRITE_ITERATIONS,
                        "version {version} out of expected range 0..={WRITE_ITERATIONS}"
                    );

                    let data = parsed["data"]
                        .as_str()
                        .unwrap_or_else(|| panic!("missing or non-string 'data' field: {raw}"));
                    assert_eq!(
                        data,
                        "x".repeat(512),
                        "data field corrupted for version {version}"
                    );
                }
                reads += 1;
            }
            reads
        });

        // Writer: repeatedly write full JSON payloads.
        let config_clone = project.config.clone();
        let writer = thread::spawn(move || {
            for i in 1..=WRITE_ITERATIONS {
                let payload = serde_json::json!({"version": i, "data": "x".repeat(512)});
                write_state_file(
                    "status.json",
                    &serde_json::to_string_pretty(&payload).unwrap(),
                    &config_clone,
                )
                .expect("concurrent write should succeed");
            }
            done_writer.store(true, Ordering::Relaxed);
        });

        writer.join().expect("writer thread should not panic");
        let total_reads = reader.join().expect("reader thread should not panic");
        assert!(
            total_reads > 0,
            "reader should have performed at least one read"
        );
    }

    #[test]
    fn write_state_file_ignores_stale_tmp_and_overwrites_cleanly() {
        let project = new_project();
        let target = state_file_path("status.json", &project.config);
        let tmp = {
            let mut s = target.as_os_str().to_os_string();
            s.push(".tmp");
            PathBuf::from(s)
        };

        // Ensure state dir exists and pre-create a stale .tmp file.
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&tmp, "stale leftover").unwrap();
        assert!(tmp.exists());

        let payload = serde_json::json!({"status": "APPROVED"});
        write_state_file(
            "status.json",
            &serde_json::to_string_pretty(&payload).unwrap(),
            &project.config,
        )
        .expect("write should succeed despite stale .tmp");

        let content = fs::read_to_string(&target).expect("target should be readable");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("target should contain valid JSON");
        assert_eq!(parsed["status"], "APPROVED");

        // The .tmp file should have been cleaned up (renamed to target).
        assert!(
            !tmp.exists(),
            "stale .tmp should not remain after successful write"
        );
    }

    #[test]
    fn is_status_stale_returns_true_when_timestamp_differs_for_in_progress_status() {
        let status = LoopStatus {
            status: Status::Reviewing,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "task".to_string(),
            reason: None,

            timestamp: "2026-02-14T00:00:00.000Z".to_string(),
        };

        assert!(is_status_stale("2026-02-14T12:00:00.000Z", &status));
    }

    #[test]
    fn is_status_stale_returns_false_when_timestamp_differs_for_verdict_status() {
        let status = LoopStatus {
            status: Status::Approved,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "task".to_string(),
            reason: None,
            timestamp: "2026-02-14T00:00:00.000Z".to_string(),
        };

        assert!(!is_status_stale("2026-02-14T12:00:00.000Z", &status));
    }

    #[test]
    fn is_status_stale_returns_false_when_timestamp_matches() {
        let status = LoopStatus {
            status: Status::Approved,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "task".to_string(),
            reason: None,

            timestamp: "2026-02-14T00:00:00.000Z".to_string(),
        };

        assert!(!is_status_stale("2026-02-14T00:00:00.000Z", &status));
    }

    #[test]
    fn append_round_summary_creates_file_and_appends_lines() {
        let project = new_project();

        append_round_summary(1, "implementation", "Added auth module", &project.config)
            .expect("first append should succeed");
        append_round_summary(
            1,
            "review",
            "NEEDS_CHANGES — missing validation",
            &project.config,
        )
        .expect("second append should succeed");
        append_round_summary(2, "implementation", "Added validation", &project.config)
            .expect("third append should succeed");

        let content = read_state_file("conversation.md", &project.config);
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "Round 1 implementation: Added auth module");
        assert_eq!(
            lines[1],
            "Round 1 review: NEEDS_CHANGES — missing validation"
        );
        assert_eq!(lines[2], "Round 2 implementation: Added validation");
    }

    #[test]
    fn append_round_summary_normalizes_and_truncates_summary() {
        let project = new_project();
        let long_summary = "a".repeat(200);

        append_round_summary(1, "implementation", &long_summary, &project.config)
            .expect("append should succeed");

        let content = read_state_file("conversation.md", &project.config);
        let line = content.lines().next().expect("should have one line");
        // 120 char limit + "..." = 120 chars total in the summary part
        assert!(
            line.len() <= "Round 1 implementation: ".len() + 120,
            "line should be bounded, got len={}",
            line.len()
        );
        assert!(line.ends_with("..."));
    }

    #[test]
    fn append_tasks_progress_groups_same_round_entries() {
        let project = new_project();

        append_tasks_progress(1, "Reviewer verdict: APPROVED", None, &project.config);
        append_tasks_progress(1, "Implementer signoff: CONSENSUS", None, &project.config);
        append_tasks_progress(2, "Reviewer verdict: NEEDS_REVISION", None, &project.config);

        let path = project.config.state_dir.join("tasks-progress.md");
        let content = fs::read_to_string(path).expect("tasks-progress.md should exist");
        assert_eq!(
            content,
            "## Round 1\nReviewer verdict: APPROVED\nImplementer signoff: CONSENSUS\n\n## Round 2\nReviewer verdict: NEEDS_REVISION\n"
        );
    }

    #[test]
    fn clear_tasks_progress_truncates_existing_content() {
        let project = new_project();

        append_tasks_progress(1, "Initial", None, &project.config);
        clear_tasks_progress(&project.config);

        let path = project.config.state_dir.join("tasks-progress.md");
        let content = fs::read_to_string(path).expect("tasks-progress.md should exist");
        assert!(content.is_empty(), "tasks-progress.md should be empty");
    }

    #[test]
    fn append_planning_progress_groups_same_round_entries() {
        let project = new_project();

        append_planning_progress(1, "Reviewer: NEEDS_REVISION", None, &project.config);
        append_planning_progress(1, "Role swap triggered after 3 consecutive revision rounds", None, &project.config);
        append_planning_progress(2, "Reviewer: APPROVED", None, &project.config);

        let path = project.config.state_dir.join("planning-progress.md");
        let content = fs::read_to_string(path).expect("planning-progress.md should exist");
        assert_eq!(
            content,
            "## Round 1\nReviewer: NEEDS_REVISION\nRole swap triggered after 3 consecutive revision rounds\n\n## Round 2\nReviewer: APPROVED\n"
        );
    }

    #[test]
    fn append_planning_progress_with_findings() {
        let project = new_project();

        append_planning_progress(
            1,
            "Reviewer: NEEDS_REVISION",
            Some("- missing error handling\n- unclear API boundary"),
            &project.config,
        );

        let path = project.config.state_dir.join("planning-progress.md");
        let content = fs::read_to_string(path).expect("planning-progress.md should exist");
        assert_eq!(
            content,
            "## Round 1\nReviewer: NEEDS_REVISION\n- missing error handling\n- unclear API boundary\n"
        );
    }

    #[test]
    fn append_tasks_progress_with_findings() {
        let project = new_project();

        append_tasks_progress(
            1,
            "Reviewer verdict: NeedsRevision (open findings: 2)",
            Some("- [F1] missing test coverage\n- [F2] unclear naming"),
            &project.config,
        );

        let path = project.config.state_dir.join("tasks-progress.md");
        let content = fs::read_to_string(path).expect("tasks-progress.md should exist");
        assert_eq!(
            content,
            "## Round 1\nReviewer verdict: NeedsRevision (open findings: 2)\n- [F1] missing test coverage\n- [F2] unclear naming\n"
        );
    }

    #[test]
    fn append_implement_progress_groups_same_round_entries() {
        let project = new_project();

        append_implement_progress(3, "Implementation summary", &project.config);
        append_implement_progress(3, "Gate A: APPROVED", &project.config);
        append_implement_progress(4, "CONSENSUS", &project.config);

        let path = project.config.state_dir.join("implement-progress.md");
        let content = fs::read_to_string(path).expect("implement-progress.md should exist");
        assert_eq!(
            content,
            "## Round 3\nImplementation summary\nGate A: APPROVED\n\n## Round 4\nCONSENSUS\n"
        );
    }

    #[test]
    fn append_implement_progress_separates_rounds_across_task_headings() {
        let project = new_project();

        append_implement_progress_task("Task 1: Setup", &project.config);
        append_implement_progress(1, "Implementation: first task", &project.config);
        append_implement_progress_task("Task 2: Ship", &project.config);
        append_implement_progress(1, "Implementation: second task", &project.config);

        let path = project.config.state_dir.join("implement-progress.md");
        let content = fs::read_to_string(path).expect("implement-progress.md should exist");
        assert_eq!(
            content,
            "### Task: Task 1: Setup\n\n## Round 1\nImplementation: first task\n\n### Task: Task 2: Ship\n\n## Round 1\nImplementation: second task\n"
        );
    }

    #[test]
    fn clear_implement_progress_truncates_existing_content() {
        let project = new_project();

        append_implement_progress(1, "Initial", &project.config);
        clear_implement_progress(&project.config);

        let path = project.config.state_dir.join("implement-progress.md");
        let content = fs::read_to_string(path).expect("implement-progress.md should exist");
        assert!(content.is_empty(), "implement-progress.md should be empty");
    }

    #[test]
    fn read_recent_history_returns_last_n_non_empty_lines() {
        let project = new_project();

        for i in 1..=25 {
            append_round_summary(i, "impl", &format!("change {i}"), &project.config)
                .expect("append should succeed");
        }

        let history = read_recent_history(&project.config, 20);
        let lines: Vec<&str> = history.lines().collect();
        assert_eq!(lines.len(), 20);
        assert!(lines[0].contains("change 6"));
        assert!(lines[19].contains("change 25"));
    }

    #[test]
    fn read_recent_history_returns_empty_for_missing_file() {
        let project = new_project();
        let history = read_recent_history(&project.config, 20);
        assert!(history.is_empty());
    }

    #[test]
    fn read_recent_history_returns_all_lines_when_under_limit() {
        let project = new_project();

        append_round_summary(1, "impl", "first change", &project.config)
            .expect("append should succeed");
        append_round_summary(1, "review", "APPROVED", &project.config)
            .expect("append should succeed");

        let history = read_recent_history(&project.config, 20);
        let lines: Vec<&str> = history.lines().collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn read_recent_history_filters_empty_lines() {
        let project = new_project();

        // Write content with empty lines mixed in
        let content =
            "Round 1 impl: change 1\n\nRound 1 review: APPROVED\n\n\nRound 2 impl: change 2\n";
        write_state_file("conversation.md", content, &project.config)
            .expect("write should succeed");

        let history = read_recent_history(&project.config, 20);
        let lines: Vec<&str> = history.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(!lines.iter().any(|l| l.trim().is_empty()));
    }

    #[test]
    fn init_creates_conversation_md() {
        let project = new_project();
        let baseline_files = vec!["src/main.rs".to_string()];

        init(
            "Test task",
            &project.config,
            &baseline_files,
            WorkflowKind::Implement,
        )
        .expect("init should succeed");

        assert!(
            state_file_path("conversation.md", &project.config).exists(),
            "conversation.md should exist after init"
        );
        let content = read_state_file("conversation.md", &project.config);
        assert!(
            content.is_empty(),
            "conversation.md should be empty after init"
        );
    }

    #[test]
    fn write_status_truncates_long_last_run_task() {
        let project = new_project();
        write_state_file("task.md", "", &project.config).expect("task.md should be writable");

        let long_task = "a".repeat(1000);
        let result = write_status(
            StatusPatch {
                status: Some(Status::Implementing),
                last_run_task: Some(long_task),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("write_status should succeed");

        assert!(
            result.last_run_task.chars().count() <= LAST_RUN_TASK_MAX_CHARS,
            "last_run_task should be bounded to {LAST_RUN_TASK_MAX_CHARS} chars, got {}",
            result.last_run_task.chars().count()
        );
        assert!(
            result.last_run_task.ends_with("..."),
            "truncated task should end with ellipsis"
        );

        let logs = read_state_file("log.txt", &project.config);
        assert!(
            logs.contains("⚠ last_run_task truncated: 1000 chars -> 500 chars"),
            "log should contain truncation warning"
        );
    }

    #[test]
    fn write_status_preserves_short_last_run_task_without_warning() {
        let project = new_project();
        write_state_file("task.md", "", &project.config).expect("task.md should be writable");

        let short_task = "short task description";
        let result = write_status(
            StatusPatch {
                status: Some(Status::Implementing),
                last_run_task: Some(short_task.to_string()),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("write_status should succeed");

        assert_eq!(result.last_run_task, short_task);

        let logs = read_state_file("log.txt", &project.config);
        assert!(
            !logs.contains("last_run_task truncated"),
            "no truncation warning expected for short task"
        );
    }

    #[test]
    fn write_status_uses_title_only_for_markdown_task_content() {
        let project = new_project();
        write_state_file("task.md", "", &project.config).expect("task.md should be writable");

        let result = write_status(
            StatusPatch {
                status: Some(Status::Implementing),
                last_run_task: Some(
                    "# Add waiting list status\n\n## Summary\nImplement full waiting list flow."
                        .to_string(),
                ),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("write_status should succeed");

        assert_eq!(result.last_run_task, "Add waiting list status");
        let logs = read_state_file("log.txt", &project.config);
        assert!(
            !logs.contains("last_run_task truncated"),
            "title-only extraction should avoid truncation warning"
        );
    }

    #[test]
    fn append_round_summary_caps_conversation_to_last_200_lines() {
        let project = new_project();

        // Append 250 summaries (each produces one line)
        for i in 1..=250 {
            append_round_summary(i, "impl", &format!("change {i}"), &project.config)
                .expect("append should succeed");
        }

        let content = read_state_file("conversation.md", &project.config);
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines.len(),
            CONVERSATION_MAX_LINES,
            "conversation.md should be capped to {CONVERSATION_MAX_LINES} lines"
        );
        // Should retain newest lines
        assert!(
            lines[0].contains("change 51"),
            "first kept line should be change 51, got: {}",
            lines[0]
        );
        assert!(
            lines[CONVERSATION_MAX_LINES - 1].contains("change 250"),
            "last kept line should be change 250, got: {}",
            lines[CONVERSATION_MAX_LINES - 1]
        );
    }

    #[test]
    fn cap_conversation_file_counts_raw_lines_including_empty() {
        let project = new_project();

        // Seed file with 250 raw lines including empty ones
        let mut content = String::new();
        for i in 1..=250 {
            if i % 3 == 0 {
                content.push('\n'); // empty line
            } else {
                content.push_str(&format!("line {i}\n"));
            }
        }
        write_state_file("conversation.md", &content, &project.config)
            .expect("write should succeed");

        cap_conversation_file(&project.config).expect("cap should succeed");

        let capped = read_state_file("conversation.md", &project.config);
        let lines: Vec<&str> = capped.lines().collect();
        assert_eq!(
            lines.len(),
            CONVERSATION_MAX_LINES,
            "capped file should have exactly {CONVERSATION_MAX_LINES} raw lines"
        );

        let logs = read_state_file("log.txt", &project.config);
        assert!(
            logs.contains("⚠ conversation.md capped: 250 lines -> 200 lines"),
            "log should contain cap warning"
        );
    }

    #[test]
    fn cap_conversation_file_does_not_truncate_within_limit() {
        let project = new_project();

        let mut content = String::new();
        for i in 1..=50 {
            content.push_str(&format!("line {i}\n"));
        }
        write_state_file("conversation.md", &content, &project.config)
            .expect("write should succeed");

        cap_conversation_file(&project.config).expect("cap should succeed");

        let result = read_state_file("conversation.md", &project.config);
        assert_eq!(result, content, "file within limit should not be modified");
    }

    // -----------------------------------------------------------------------
    // StatusReadResult / read_status_with_warnings tests
    // -----------------------------------------------------------------------

    #[test]
    fn warnings_invalid_json_produces_error_status_and_parse_warning() {
        let project = new_project();
        write_state_file("status.json", "{broken", &project.config).expect("write should succeed");

        let result = read_status_with_warnings(&project.config);
        assert_eq!(result.status.status, Status::Error);
        assert!(
            result
                .status
                .reason
                .as_deref()
                .unwrap()
                .starts_with("Invalid status.json:"),
            "reason should contain parse error"
        );
        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].starts_with("invalid JSON:"),
            "warning should mention invalid JSON, got: {}",
            result.warnings[0]
        );
    }

    #[test]
    fn warnings_non_object_root_produces_warning() {
        let project = new_project();
        let raw = json!("just a string");
        let result = normalize_status_value_with_warnings(&raw, &project.config);

        assert_eq!(result.status.status, Status::Pending);
        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].contains("not a JSON object"),
            "warning should mention non-object root, got: {}",
            result.warnings[0]
        );
    }

    #[test]
    fn warnings_missing_required_fields_produces_per_field_warnings() {
        let project = new_project();
        let raw = json!({});
        let result = normalize_status_value_with_warnings(&raw, &project.config);

        // Should produce warnings for: status, round, timestamp
        // (implementer, reviewer, mode silently fall back)
        let required_fields = ["status", "round", "timestamp"];
        for field in required_fields {
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|w| w.contains(&format!("'{field}'")) && w.contains("missing")),
                "expected missing warning for field '{field}', got: {:?}",
                result.warnings
            );
        }
        assert_eq!(
            result.warnings.len(),
            required_fields.len(),
            "exactly one warning per missing required field"
        );
    }

    #[test]
    fn warnings_wrong_types_produces_per_field_warnings() {
        let project = new_project();
        let raw = json!({
            "status": 42,
            "round": "not-a-number",
            "implementer": false,
            "reviewer": [],
            "mode": 99,
            "timestamp": 12345
        });

        let result = normalize_status_value_with_warnings(&raw, &project.config);

        // implementer, reviewer, mode silently fall back — no warnings
        for field in ["status", "round", "timestamp"] {
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|w| w.contains(&format!("'{field}'"))),
                "expected warning for field '{field}', got: {:?}",
                result.warnings
            );
        }
    }

    #[test]
    fn warnings_invalid_values_produces_specific_warnings() {
        let project = new_project();

        // Unknown status enum
        let raw = json!({"status": "UNKNOWN_VALUE", "round": 1, "implementer": "a", "reviewer": "b", "mode": "single-agent", "timestamp": "t"});
        let result = normalize_status_value_with_warnings(&raw, &project.config);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("'status'") && w.contains("invalid")),
            "expected warning for unknown status, got: {:?}",
            result.warnings
        );

        // Negative round
        let raw = json!({"status": "PENDING", "round": -5, "implementer": "a", "reviewer": "b", "mode": "single-agent", "timestamp": "t"});
        let result = normalize_status_value_with_warnings(&raw, &project.config);
        assert!(
            result.warnings.iter().any(|w| w.contains("'round'")),
            "expected warning for negative round, got: {:?}",
            result.warnings
        );

        // Float round
        let raw = json!({"status": "PENDING", "round": 1.5, "implementer": "a", "reviewer": "b", "mode": "single-agent", "timestamp": "t"});
        let result = normalize_status_value_with_warnings(&raw, &project.config);
        assert!(
            result.warnings.iter().any(|w| w.contains("'round'")),
            "expected warning for float round, got: {:?}",
            result.warnings
        );
    }

    #[test]
    fn warnings_valid_status_file_produces_no_warnings() {
        let project = new_project();
        let raw = json!({
            "status": "REVIEWING",
            "round": 3,
            "implementer": "claude",
            "reviewer": "codex",
            "mode": "dual-agent",
            "lastRunTask": "build feature",
            "reason": "all good",
            "timestamp": "2026-02-14T00:00:00.000Z"
        });

        let result = normalize_status_value_with_warnings(&raw, &project.config);
        assert!(
            result.warnings.is_empty(),
            "valid status should produce no warnings, got: {:?}",
            result.warnings
        );
        assert_eq!(result.status.status, Status::Reviewing);
        assert_eq!(result.status.round, 3);
    }

    #[test]
    fn warnings_optional_fields_only_warn_when_present_but_invalid() {
        let project = new_project();

        // reason wrong type
        let raw = json!({
            "status": "PENDING", "round": 0, "implementer": "a",
            "reviewer": "b", "mode": "dual-agent", "timestamp": "t",
            "reason": 42
        });
        let result = normalize_status_value_with_warnings(&raw, &project.config);
        assert!(
            result.warnings.iter().any(|w| w.contains("'reason'")),
            "expected reason type warning, got: {:?}",
            result.warnings
        );

        // lastRunTask wrong type
        let raw = json!({
            "status": "PENDING", "round": 0, "implementer": "a",
            "reviewer": "b", "mode": "dual-agent", "timestamp": "t",
            "lastRunTask": false
        });
        let result = normalize_status_value_with_warnings(&raw, &project.config);
        assert!(
            result.warnings.iter().any(|w| w.contains("'lastRunTask'")),
            "expected lastRunTask type warning, got: {:?}",
            result.warnings
        );
    }

    #[test]
    fn warnings_empty_file_produces_no_warnings() {
        let project = new_project();
        // Don't write status.json at all (missing file)
        let result = read_status_with_warnings(&project.config);
        assert!(result.warnings.is_empty());
        assert_eq!(result.status.status, Status::Pending);

        // Write empty content
        write_state_file("status.json", "  ", &project.config).expect("write should succeed");
        let result = read_status_with_warnings(&project.config);
        assert!(result.warnings.is_empty());
        assert_eq!(result.status.status, Status::Pending);
    }

    #[test]
    fn sanitize_for_display_replaces_control_characters() {
        assert_eq!(sanitize_for_display("normal text"), "normal text");
        assert_eq!(sanitize_for_display("tab\there"), "tab\there");
        assert_eq!(sanitize_for_display("line\nbreak"), "line\nbreak");
        // ESC (0x1B) and BEL (0x07) should be replaced
        assert_eq!(
            sanitize_for_display("evil\x1b[31mred\x1b[0m"),
            "evil\u{FFFD}[31mred\u{FFFD}[0m"
        );
        assert_eq!(sanitize_for_display("bell\x07here"), "bell\u{FFFD}here");
        assert_eq!(sanitize_for_display("null\x00byte"), "null\u{FFFD}byte");
    }

    // -----------------------------------------------------------------------
    // Findings persistence tests (findings.json)
    // -----------------------------------------------------------------------

    #[test]
    fn findings_round_trip_read_write() {
        let project = new_project();
        let findings = FindingsFile {
            round: 2,
            findings: vec![
                FindingEntry {
                    id: "F-001".to_string(),
                    severity: "HIGH".to_string(),
                    summary: "Recompute hash after ID migration".to_string(),
                    file_refs: vec![
                        "ActivityVariation.php:48".to_string(),
                        "StoreActivityVariationRequest.php:92".to_string(),
                    ],
                },
                FindingEntry {
                    id: "F-002".to_string(),
                    severity: "MEDIUM".to_string(),
                    summary: "Add validation rules for nested IDs".to_string(),
                    file_refs: vec![],
                },
            ],
        };

        write_findings(&findings, &project.config).expect("write_findings should succeed");
        let reloaded = read_findings(&project.config);
        assert_eq!(reloaded, findings);
    }

    #[test]
    fn findings_missing_or_empty_returns_default() {
        let project = new_project();

        // Missing file
        let result = read_findings(&project.config);
        assert_eq!(result, FindingsFile::default());
        assert!(result.findings.is_empty());

        // Empty file
        write_state_file("findings.json", "", &project.config).expect("empty write should succeed");
        let result = read_findings(&project.config);
        assert_eq!(result, FindingsFile::default());

        // Whitespace-only file
        write_state_file("findings.json", "   \n\t  ", &project.config)
            .expect("whitespace write should succeed");
        let result = read_findings(&project.config);
        assert_eq!(result, FindingsFile::default());
    }

    #[test]
    fn findings_corrupt_json_recovers_with_warning() {
        let project = new_project();
        write_state_file("findings.json", "{broken json", &project.config)
            .expect("corrupt write should succeed");

        let result = read_findings_with_warnings(&project.config);
        assert_eq!(result.findings_file, FindingsFile::default());
        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].contains("invalid findings.json"),
            "warning should mention corruption, got: {}",
            result.warnings[0]
        );
    }

    #[test]
    fn findings_omits_empty_file_refs() {
        let finding = FindingEntry {
            id: "F-123".to_string(),
            severity: "LOW".to_string(),
            summary: "Minor naming mismatch".to_string(),
            file_refs: vec![],
        };

        let json = serde_json::to_value(&finding).expect("finding should serialize");
        assert!(!json.as_object().unwrap().contains_key("file_refs"));

        let with_refs = FindingEntry {
            file_refs: vec!["src/main.rs:42".to_string()],
            ..finding
        };
        let json = serde_json::to_value(&with_refs).expect("finding with refs should serialize");
        assert_eq!(json["file_refs"][0], "src/main.rs:42");
    }

    // -----------------------------------------------------------------------
    // Task status persistence tests
    // -----------------------------------------------------------------------

    #[test]
    fn task_status_round_trip_read_write() {
        let project = new_project();
        let status_file = TaskStatusFile {
            tasks: vec![
                TaskStatusEntry {
                    title: "Task 1: Build parser".to_string(),
                    status: TaskRunStatus::Done,
                    retries: 1,
                    last_error: None,
                    skip_reason: None,
                    wave_index: None,
                },
                TaskStatusEntry {
                    title: "Task 2: Add retries".to_string(),
                    status: TaskRunStatus::Failed,
                    retries: 2,
                    last_error: Some("MAX_ROUNDS reached".to_string()),
                    skip_reason: None,
                    wave_index: None,
                },
                TaskStatusEntry {
                    title: "Task 3: Cleanup".to_string(),
                    status: TaskRunStatus::Pending,
                    retries: 0,
                    last_error: None,
                    skip_reason: None,
                    wave_index: None,
                },
            ],
        };

        write_task_status(&status_file, &project.config).expect("write_task_status should succeed");

        let reloaded = read_task_status(&project.config);
        assert_eq!(reloaded, status_file);
    }

    #[test]
    fn task_status_missing_or_empty_returns_default() {
        let project = new_project();

        // Missing file
        let result = read_task_status(&project.config);
        assert_eq!(result, TaskStatusFile::default());
        assert!(result.tasks.is_empty());

        // Empty file
        write_state_file("task_status.json", "", &project.config)
            .expect("empty write should succeed");
        let result = read_task_status(&project.config);
        assert_eq!(result, TaskStatusFile::default());

        // Whitespace-only file
        write_state_file("task_status.json", "   \n\t  ", &project.config)
            .expect("whitespace write should succeed");
        let result = read_task_status(&project.config);
        assert_eq!(result, TaskStatusFile::default());
    }

    #[test]
    fn task_status_corrupt_json_recovers_with_warning() {
        let project = new_project();
        write_state_file("task_status.json", "{broken json", &project.config)
            .expect("corrupt write should succeed");

        let result = read_task_status_with_warnings(&project.config);
        assert_eq!(result.status_file, TaskStatusFile::default());
        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].contains("invalid task_status.json"),
            "warning should mention corruption, got: {}",
            result.warnings[0]
        );
    }

    #[test]
    fn task_status_invalid_entry_types_recovers_with_warning() {
        let project = new_project();
        // "tasks" contains invalid entries (wrong type for status)
        let invalid_json =
            r#"{"tasks": [{"title": "Task 1", "status": "INVALID_STATUS", "retries": 0}]}"#;
        write_state_file("task_status.json", invalid_json, &project.config)
            .expect("invalid write should succeed");

        let result = read_task_status_with_warnings(&project.config);
        assert_eq!(result.status_file, TaskStatusFile::default());
        assert!(!result.warnings.is_empty());
    }

    #[test]
    fn task_status_serde_round_trip_all_variants() {
        let variants = [
            TaskRunStatus::Pending,
            TaskRunStatus::Running,
            TaskRunStatus::Done,
            TaskRunStatus::Failed,
            TaskRunStatus::Skipped,
        ];

        for variant in variants {
            let serialized = serde_json::to_value(variant)
                .unwrap_or_else(|_| panic!("{variant:?} should serialize"));
            let deserialized: TaskRunStatus = serde_json::from_value(serialized.clone())
                .unwrap_or_else(|_| panic!("{variant:?} should deserialize from {serialized}"));
            assert_eq!(variant, deserialized);
        }
    }

    #[test]
    fn task_status_display_matches_serde() {
        let variants = [
            (TaskRunStatus::Pending, "pending"),
            (TaskRunStatus::Running, "running"),
            (TaskRunStatus::Done, "done"),
            (TaskRunStatus::Failed, "failed"),
            (TaskRunStatus::Skipped, "skipped"),
        ];

        for (variant, expected) in variants {
            assert_eq!(variant.to_string(), expected);
            let serde_value = serde_json::to_value(variant).unwrap();
            assert_eq!(serde_value.as_str().unwrap(), expected);
        }
    }

    #[test]
    fn task_status_omits_none_last_error() {
        let entry = TaskStatusEntry {
            title: "Task 1".to_string(),
            status: TaskRunStatus::Done,
            retries: 0,
            last_error: None,
            skip_reason: None,
            wave_index: None,
        };

        let json = serde_json::to_value(&entry).unwrap();
        assert!(!json.as_object().unwrap().contains_key("last_error"));

        let with_error = TaskStatusEntry {
            last_error: Some("timeout".to_string()),
            ..entry
        };
        let json = serde_json::to_value(&with_error).unwrap();
        assert_eq!(json["last_error"], "timeout");
    }

    #[test]
    fn task_status_omits_none_skip_reason_and_includes_when_set() {
        let entry = TaskStatusEntry {
            title: "Task 1".to_string(),
            status: TaskRunStatus::Skipped,
            retries: 0,
            last_error: None,
            skip_reason: None,
            wave_index: None,
        };

        let json = serde_json::to_value(&entry).unwrap();
        assert!(!json.as_object().unwrap().contains_key("skip_reason"));

        let with_reason = TaskStatusEntry {
            skip_reason: Some("dependency failed: Task 0".to_string()),
            ..entry
        };
        let json = serde_json::to_value(&with_reason).unwrap();
        assert_eq!(json["skip_reason"], "dependency failed: Task 0");
    }

    #[test]
    fn task_status_write_uses_atomic_state_file() {
        let project = new_project();
        let status_file = TaskStatusFile {
            tasks: vec![TaskStatusEntry {
                title: "Task 1".to_string(),
                status: TaskRunStatus::Running,
                retries: 0,
                last_error: None,
                skip_reason: None,
                wave_index: None,
            }],
        };

        write_task_status(&status_file, &project.config).expect("write should succeed");

        // Verify the file exists and is valid JSON
        let raw = fs::read_to_string(state_file_path("task_status.json", &project.config))
            .expect("file should be readable");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("file should contain valid JSON");
        assert_eq!(parsed["tasks"][0]["status"], "running");
        assert_eq!(parsed["tasks"][0]["title"], "Task 1");
    }

    // -----------------------------------------------------------------------
    // Task metrics persistence tests
    // -----------------------------------------------------------------------

    #[test]
    fn task_metrics_round_trip_read_write() {
        let project = new_project();
        let metrics = TaskMetricsFile {
            tasks: vec![
                TaskMetricsEntry {
                    title: "Task 1: Build parser".to_string(),
                    task_started_at: Some("2026-02-16T10:00:00.000Z".to_string()),
                    task_ended_at: Some("2026-02-16T10:05:30.000Z".to_string()),
                    duration_ms: Some(330_000),
                    agent_calls: Some(6),
                    input_tokens: Some(2_400),
                    output_tokens: Some(1_100),
                    total_tokens: Some(3_500),
                    cost_usd_micros: Some(123_456),
                },
                TaskMetricsEntry {
                    title: "Task 2: Add retries".to_string(),
                    task_started_at: Some("2026-02-16T10:06:00.000Z".to_string()),
                    task_ended_at: Some("2026-02-16T10:10:15.000Z".to_string()),
                    duration_ms: Some(255_000),
                    agent_calls: Some(4),
                    input_tokens: Some(1_900),
                    output_tokens: Some(900),
                    total_tokens: Some(2_800),
                    cost_usd_micros: Some(98_765),
                },
            ],
        };

        write_task_metrics(&metrics, &project.config).expect("write_task_metrics should succeed");

        let reloaded = read_task_metrics(&project.config);
        assert_eq!(reloaded, metrics);
    }

    #[test]
    fn task_metrics_missing_or_empty_returns_default() {
        let project = new_project();

        // Missing file
        let result = read_task_metrics(&project.config);
        assert_eq!(result, TaskMetricsFile::default());
        assert!(result.tasks.is_empty());

        // Empty file
        write_state_file("task_metrics.json", "", &project.config)
            .expect("empty write should succeed");
        let result = read_task_metrics(&project.config);
        assert_eq!(result, TaskMetricsFile::default());

        // Whitespace-only file
        write_state_file("task_metrics.json", "   \n\t  ", &project.config)
            .expect("whitespace write should succeed");
        let result = read_task_metrics(&project.config);
        assert_eq!(result, TaskMetricsFile::default());
    }

    #[test]
    fn task_metrics_corrupt_json_recovers_with_warning() {
        let project = new_project();
        write_state_file("task_metrics.json", "{broken json", &project.config)
            .expect("corrupt write should succeed");

        let result = read_task_metrics_with_warnings(&project.config);
        assert_eq!(result.metrics_file, TaskMetricsFile::default());
        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].contains("invalid task_metrics.json"),
            "warning should mention corruption, got: {}",
            result.warnings[0]
        );
    }

    #[test]
    fn task_metrics_omits_none_fields() {
        let entry = TaskMetricsEntry {
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

        let json = serde_json::to_value(&entry).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("title"));
        assert!(!obj.contains_key("task_started_at"));
        assert!(!obj.contains_key("task_ended_at"));
        assert!(!obj.contains_key("duration_ms"));
        assert!(!obj.contains_key("agent_calls"));
        assert!(!obj.contains_key("input_tokens"));
        assert!(!obj.contains_key("output_tokens"));
        assert!(!obj.contains_key("total_tokens"));
        assert!(!obj.contains_key("cost_usd_micros"));

        // With values set
        let entry_with_values = TaskMetricsEntry {
            title: "Task 1".to_string(),
            task_started_at: Some("2026-02-16T10:00:00.000Z".to_string()),
            task_ended_at: Some("2026-02-16T10:05:00.000Z".to_string()),
            duration_ms: Some(300_000),
            agent_calls: Some(5),
            input_tokens: Some(1_500),
            output_tokens: Some(700),
            total_tokens: Some(2_200),
            cost_usd_micros: Some(77_000),
        };

        let json = serde_json::to_value(&entry_with_values).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["task_started_at"], "2026-02-16T10:00:00.000Z");
        assert_eq!(obj["task_ended_at"], "2026-02-16T10:05:00.000Z");
        assert_eq!(obj["duration_ms"], 300_000);
        assert_eq!(obj["agent_calls"], 5);
        assert_eq!(obj["input_tokens"], 1_500);
        assert_eq!(obj["output_tokens"], 700);
        assert_eq!(obj["total_tokens"], 2_200);
        assert_eq!(obj["cost_usd_micros"], 77_000);
    }

    // -----------------------------------------------------------------------
    // WorkflowKind tests
    // -----------------------------------------------------------------------

    #[test]
    fn workflow_kind_display_matches_expected_strings() {
        assert_eq!(WorkflowKind::Plan.to_string(), "plan");
        assert_eq!(WorkflowKind::Decompose.to_string(), "decompose");
        assert_eq!(WorkflowKind::Implement.to_string(), "implement");
    }

    #[test]
    fn workflow_kind_from_str_round_trip() {
        assert_eq!("plan".parse::<WorkflowKind>(), Ok(WorkflowKind::Plan));
        assert_eq!(
            "decompose".parse::<WorkflowKind>(),
            Ok(WorkflowKind::Decompose)
        );
        assert_eq!(
            "implement".parse::<WorkflowKind>(),
            Ok(WorkflowKind::Implement)
        );
    }

    #[test]
    fn workflow_kind_from_str_maps_legacy_run_to_implement() {
        assert_eq!("run".parse::<WorkflowKind>(), Ok(WorkflowKind::Implement));
    }

    #[test]
    fn workflow_kind_from_str_rejects_unknown() {
        assert!("unknown".parse::<WorkflowKind>().is_err());
        assert!("PLAN".parse::<WorkflowKind>().is_err());
        assert!("IMPLEMENT".parse::<WorkflowKind>().is_err());
        assert!("".parse::<WorkflowKind>().is_err());
        assert!("Plan".parse::<WorkflowKind>().is_err());
    }

    #[test]
    fn write_then_read_workflow_round_trip() {
        let project = new_project();

        write_workflow(WorkflowKind::Plan, &project.config)
            .expect("write_workflow Plan should succeed");
        assert_eq!(read_workflow(&project.config), Some(WorkflowKind::Plan));

        write_workflow(WorkflowKind::Decompose, &project.config)
            .expect("write_workflow Decompose should succeed");
        assert_eq!(
            read_workflow(&project.config),
            Some(WorkflowKind::Decompose)
        );

        write_workflow(WorkflowKind::Implement, &project.config)
            .expect("write_workflow Implement should succeed");
        assert_eq!(
            read_workflow(&project.config),
            Some(WorkflowKind::Implement)
        );
    }

    #[test]
    fn read_workflow_returns_none_for_missing_file() {
        let project = new_project();
        assert_eq!(read_workflow(&project.config), None);
    }

    #[test]
    fn read_workflow_returns_none_for_empty_content() {
        let project = new_project();
        write_state_file("workflow.txt", "", &project.config).expect("empty write should succeed");
        assert_eq!(read_workflow(&project.config), None);

        write_state_file("workflow.txt", "   \n\t  ", &project.config)
            .expect("whitespace write should succeed");
        assert_eq!(read_workflow(&project.config), None);
    }

    #[test]
    fn read_workflow_returns_none_for_unknown_content() {
        let project = new_project();
        write_state_file("workflow.txt", "garbage\n", &project.config)
            .expect("garbage write should succeed");
        assert_eq!(read_workflow(&project.config), None);

        write_state_file("workflow.txt", "PLAN\n", &project.config)
            .expect("uppercase write should succeed");
        assert_eq!(read_workflow(&project.config), None);
    }

    #[test]
    fn task_status_entry_wave_index_omitted_when_none() {
        let entry = TaskStatusEntry {
            title: "Task 1".to_string(),
            status: TaskRunStatus::Pending,
            retries: 0,
            last_error: None,
            skip_reason: None,
            wave_index: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("wave_index"),
            "wave_index should be omitted when None"
        );
    }

    #[test]
    fn task_status_entry_wave_index_included_when_set() {
        let entry = TaskStatusEntry {
            title: "Task 1".to_string(),
            status: TaskRunStatus::Pending,
            retries: 0,
            last_error: None,
            skip_reason: None,
            wave_index: Some(2),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["wave_index"], 2);
    }

    #[test]
    fn task_status_entry_wave_index_round_trip() {
        let project = new_project();
        let status_file = TaskStatusFile {
            tasks: vec![
                TaskStatusEntry {
                    title: "Task A".to_string(),
                    status: TaskRunStatus::Pending,
                    retries: 0,
                    last_error: None,
                    skip_reason: None,
                    wave_index: Some(0),
                },
                TaskStatusEntry {
                    title: "Task B".to_string(),
                    status: TaskRunStatus::Running,
                    retries: 0,
                    last_error: None,
                    skip_reason: None,
                    wave_index: Some(1),
                },
                TaskStatusEntry {
                    title: "Task C".to_string(),
                    status: TaskRunStatus::Done,
                    retries: 0,
                    last_error: None,
                    skip_reason: None,
                    wave_index: None,
                },
            ],
        };

        write_task_status(&status_file, &project.config).expect("write should succeed");
        let reloaded = read_task_status(&project.config);
        assert_eq!(reloaded.tasks[0].wave_index, Some(0));
        assert_eq!(reloaded.tasks[1].wave_index, Some(1));
        assert_eq!(reloaded.tasks[2].wave_index, None);
    }

    // -----------------------------------------------------------------------
    // Tasks findings (tasks_findings.json) tests
    // -----------------------------------------------------------------------

    #[test]
    fn read_tasks_findings_returns_empty_when_file_missing() {
        let project = new_project();
        let findings = read_tasks_findings(&project.config);
        assert!(findings.findings.is_empty());
    }

    #[test]
    fn write_and_read_tasks_findings_round_trips() {
        let project = new_project();
        let findings = TasksFindingsFile {
            findings: vec![TasksFindingEntry {
                id: "T-001".to_string(),
                description: "Missing dependency declaration".to_string(),
                status: TasksFindingStatus::Open,
                round_introduced: 1,
                round_resolved: None,
            }],
        };
        write_tasks_findings(&findings, &project.config).expect("write should succeed");
        let reloaded = read_tasks_findings(&project.config);
        assert_eq!(reloaded.findings.len(), 1);
        assert_eq!(reloaded.findings[0].id, "T-001");
        assert_eq!(reloaded.findings[0].status, TasksFindingStatus::Open);
    }

    #[test]
    fn clear_tasks_findings_removes_file() {
        let project = new_project();
        let findings = TasksFindingsFile {
            findings: vec![TasksFindingEntry {
                id: "T-001".to_string(),
                description: "test".to_string(),
                status: TasksFindingStatus::Open,
                round_introduced: 1,
                round_resolved: None,
            }],
        };
        write_tasks_findings(&findings, &project.config).expect("write should succeed");
        assert!(!read_tasks_findings(&project.config).findings.is_empty());

        clear_tasks_findings(&project.config);
        assert!(read_tasks_findings(&project.config).findings.is_empty());
    }

    #[test]
    fn open_tasks_findings_for_prompt_filters_to_open() {
        let findings = TasksFindingsFile {
            findings: vec![
                TasksFindingEntry {
                    id: "T-001".to_string(),
                    description: "resolved issue".to_string(),
                    status: TasksFindingStatus::Resolved,
                    round_introduced: 1,
                    round_resolved: Some(2),
                },
                TasksFindingEntry {
                    id: "T-002".to_string(),
                    description: "still open".to_string(),
                    status: TasksFindingStatus::Open,
                    round_introduced: 2,
                    round_resolved: None,
                },
            ],
        };
        let prompt = open_tasks_findings_for_prompt(&findings);
        assert!(prompt.contains("T-002"));
        assert!(prompt.contains("still open"));
        assert!(!prompt.contains("T-001"));
    }

    #[test]
    fn open_tasks_findings_for_prompt_empty_when_no_open() {
        let findings = TasksFindingsFile {
            findings: vec![TasksFindingEntry {
                id: "T-001".to_string(),
                description: "resolved".to_string(),
                status: TasksFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(2),
            }],
        };
        let prompt = open_tasks_findings_for_prompt(&findings);
        assert!(prompt.is_empty());
    }

    #[test]
    fn next_tasks_finding_id_auto_increments() {
        let findings = TasksFindingsFile {
            findings: vec![
                TasksFindingEntry {
                    id: "T-001".to_string(),
                    description: "a".to_string(),
                    status: TasksFindingStatus::Open,
                    round_introduced: 1,
                    round_resolved: None,
                },
                TasksFindingEntry {
                    id: "T-003".to_string(),
                    description: "b".to_string(),
                    status: TasksFindingStatus::Open,
                    round_introduced: 1,
                    round_resolved: None,
                },
            ],
        };
        assert_eq!(next_tasks_finding_id(&findings), "T-004");
    }

    #[test]
    fn next_tasks_finding_id_starts_at_001_when_empty() {
        let findings = TasksFindingsFile::default();
        assert_eq!(next_tasks_finding_id(&findings), "T-001");
    }

    #[test]
    fn tasks_findings_tolerant_read_on_invalid_json() {
        let project = new_project();
        // Write invalid JSON
        write_state_file("tasks_findings.json", "not json", &project.config)
            .expect("write should succeed");
        let findings = read_tasks_findings(&project.config);
        assert!(findings.findings.is_empty());
    }

    // -----------------------------------------------------------------------
    // decisions_enabled gating
    // -----------------------------------------------------------------------

    #[test]
    fn read_decisions_returns_empty_when_disabled() {
        let mut project = new_project();
        project.config.decisions_enabled = false;
        // Write some decisions
        let path = decisions_path(&project.config);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, "- [PATTERN] some pattern\n").unwrap();
        assert!(
            read_decisions(&project.config).is_empty(),
            "disabled decisions should return empty"
        );
    }

    #[test]
    fn append_decision_noop_when_disabled() {
        let mut project = new_project();
        project.config.decisions_enabled = false;
        let path = decisions_path(&project.config);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, "").unwrap();
        append_decision("- [GOTCHA] something", &project.config).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.is_empty(), "disabled decisions should not append");
    }

    // -----------------------------------------------------------------------
    // strip_decisions_reference
    // -----------------------------------------------------------------------

    #[test]
    fn strip_decisions_reference_removes_managed_block() {
        let content = format!(
            "# CLAUDE.md\n\nSome content\n\n{DECISIONS_REFERENCE_START}\n## Agent Loop\nBody\n{DECISIONS_REFERENCE_END}\n"
        );
        let updated = strip_decisions_reference(&content).expect("should produce update");
        assert!(!updated.contains(DECISIONS_REFERENCE_START));
        assert!(!updated.contains(DECISIONS_REFERENCE_END));
        assert!(updated.contains("Some content"));
    }

    #[test]
    fn strip_decisions_reference_returns_none_when_no_block() {
        let content = "# CLAUDE.md\n\nSome content\n";
        assert!(strip_decisions_reference(content).is_none());
    }

    // -----------------------------------------------------------------------
    // init gating
    // -----------------------------------------------------------------------

    #[test]
    fn init_does_not_create_decisions_file_when_disabled() {
        let mut project = new_project();
        project.config.decisions_enabled = false;
        init("Task", &project.config, &[], WorkflowKind::Implement).unwrap();
        assert!(!decisions_path(&project.config).exists());
    }

    #[test]
    fn init_creates_decisions_file_when_enabled() {
        let mut project = new_project();
        project.config.decisions_enabled = true;
        init("Task", &project.config, &[], WorkflowKind::Implement).unwrap();
        assert!(decisions_path(&project.config).exists());
    }

    #[test]
    fn init_removes_managed_blocks_when_decisions_disabled() {
        let mut project = new_project();
        // First create CLAUDE.md with a managed block
        let claude_md = project.config.project_dir.join("CLAUDE.md");
        let block = decisions_reference_block();
        fs::write(&claude_md, format!("# CLAUDE\n\n{block}\n")).unwrap();

        project.config.decisions_enabled = false;
        init("Task", &project.config, &[], WorkflowKind::Implement).unwrap();

        let content = fs::read_to_string(&claude_md).unwrap();
        assert!(
            !content.contains(DECISIONS_REFERENCE_START),
            "disabled decisions should remove managed blocks"
        );
    }

    #[test]
    fn init_removes_managed_blocks_when_auto_reference_disabled() {
        let mut project = new_project();
        // First create CLAUDE.md with a managed block
        let claude_md = project.config.project_dir.join("CLAUDE.md");
        let block = decisions_reference_block();
        fs::write(&claude_md, format!("# CLAUDE\n\n{block}\n")).unwrap();

        project.config.decisions_auto_reference = false;
        init("Task", &project.config, &[], WorkflowKind::Implement).unwrap();

        let content = fs::read_to_string(&claude_md).unwrap();
        assert!(
            !content.contains(DECISIONS_REFERENCE_START),
            "auto_reference=false should remove managed blocks"
        );
    }

    // -----------------------------------------------------------------------
    // transcript
    // -----------------------------------------------------------------------

    #[test]
    fn append_transcript_entry_noop_when_disabled() {
        let project = new_project();
        // Default: transcript_enabled = false
        let meta = AgentCallMeta {
            agent_name: "claude".to_string(),
            role: "implementer".to_string(),
            ..AgentCallMeta::default()
        };
        append_transcript_entry(&project.config, &meta, "prompt", None, "output");
        let path = project.config.state_dir.join("transcript.log");
        assert!(
            !path.exists(),
            "transcript should not be created when disabled"
        );
    }

    #[test]
    fn append_transcript_entry_writes_when_enabled() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "review".to_string(),
            round: 3,
            role: "reviewer".to_string(),
            agent_name: "codex".to_string(),
            session_hint: Some("implement-reviewer-codex".to_string()),
        };
        append_transcript_entry(
            &project.config,
            &meta,
            "user prompt text",
            Some("system prompt text"),
            "normalized output text",
        );

        let path = project.config.state_dir.join("transcript.log");
        assert!(path.exists(), "transcript.log should be created");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("AGENT CALL"));
        assert!(content.contains("workflow: implement"));
        assert!(content.contains("phase: review"));
        assert!(content.contains("round: 3"));
        assert!(content.contains("role: reviewer"));
        assert!(content.contains("agent: codex"));
        assert!(content.contains("session_hint: implement-reviewer-codex"));
        assert!(content.contains("--- USER PROMPT ---"));
        assert!(content.contains("user prompt text"));
        assert!(content.contains("--- SYSTEM PROMPT ---"));
        assert!(content.contains("system prompt text"));
        assert!(content.contains("--- NORMALIZED OUTPUT ---"));
        assert!(content.contains("normalized output text"));
        assert!(content.contains("=== END ==="));
    }

    #[test]
    fn append_transcript_entry_omits_session_hint_when_none() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "implementer".to_string(),
            round: 1,
            role: "implementer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None, // explicitly None
        };
        append_transcript_entry(&project.config, &meta, "p", None, "o");

        let path = project.config.state_dir.join("transcript.log");
        let content = fs::read_to_string(&path).unwrap();

        // The session_hint line must be absent when the field is None
        assert!(
            !content.contains("session_hint:"),
            "session_hint line must be omitted when None"
        );
        // Other metadata fields must still be present
        assert!(content.contains("phase: implementer"));
        assert!(content.contains("role: implementer"));
        assert!(content.contains("agent: claude"));
    }

    #[test]
    fn transcript_rotation_caps_large_files() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let path = project.config.state_dir.join("transcript.log");
        // Pre-seed a large file
        let mut big = String::new();
        for i in 0..TRANSCRIPT_MAX_LINES + 100 {
            big.push_str(&format!("line {i}\n"));
        }
        fs::write(&path, &big).unwrap();

        // Now append a new entry which triggers rotation
        let meta = AgentCallMeta {
            workflow: FALLBACK_WORKFLOW.to_string(),
            phase: FALLBACK_PHASE.to_string(),
            ..AgentCallMeta::default()
        };
        append_transcript_entry(&project.config, &meta, "p", None, "o");

        let content = fs::read_to_string(&path).unwrap();
        let line_count = content.lines().count();
        assert!(
            line_count <= TRANSCRIPT_MAX_LINES,
            "transcript should be rotated: got {line_count} lines"
        );
        assert!(content.starts_with("[transcript rotated]"));
    }

    #[test]
    fn agent_call_meta_is_phase_tracked_returns_true_for_real_phases() {
        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "gate-a-review".to_string(),
            round: 2,
            role: "reviewer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None,
        };
        assert!(meta.is_phase_tracked(), "real phase meta must be tracked");
    }

    #[test]
    fn agent_call_meta_is_phase_tracked_returns_false_for_fallback() {
        let meta = AgentCallMeta {
            workflow: FALLBACK_WORKFLOW.to_string(),
            phase: FALLBACK_PHASE.to_string(),
            round: 0,
            role: "implementer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None,
        };
        assert!(
            !meta.is_phase_tracked(),
            "fallback meta must not be tracked"
        );
    }

    #[test]
    fn transcript_entry_includes_untracked_annotation_for_fallback() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: FALLBACK_WORKFLOW.to_string(),
            phase: FALLBACK_PHASE.to_string(),
            round: 0,
            role: "unknown".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None,
        };
        append_transcript_entry(&project.config, &meta, "p", None, "o");

        let path = project.config.state_dir.join("transcript.log");
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("tracking: untracked (direct agent call)"),
            "fallback entries must include tracking annotation, got:\n{content}"
        );
    }

    #[test]
    fn transcript_entry_omits_untracked_annotation_for_phase_meta() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "implementer".to_string(),
            round: 1,
            role: "implementer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None,
        };
        append_transcript_entry(&project.config, &meta, "p", None, "o");

        let path = project.config.state_dir.join("transcript.log");
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("tracking: untracked"),
            "phase-tracked entries must NOT include untracked annotation, got:\n{content}"
        );
    }

    #[test]
    fn begin_transcript_entry_writes_full_metadata_and_prompts() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "implementer".to_string(),
            round: 2,
            role: "implementer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: Some("hint-abc".to_string()),
        };
        let handle = begin_transcript_entry(
            &project.config,
            &meta,
            "user prompt text",
            Some("system prompt text"),
        );
        assert!(handle.is_some(), "should return a handle when enabled");

        let path = project.config.state_dir.join("transcript.log");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("=== AGENT CALL ["));
        assert!(content.contains("workflow: implement"));
        assert!(content.contains("phase: implementer"));
        assert!(content.contains("round: 2"));
        assert!(content.contains("role: implementer"));
        assert!(content.contains("agent: claude"));
        assert!(content.contains("session_hint: hint-abc"));
        assert!(content.contains("status: in_progress"));
        assert!(content.contains("--- USER PROMPT ---"));
        assert!(content.contains("user prompt text"));
        assert!(content.contains("--- SYSTEM PROMPT ---"));
        assert!(content.contains("system prompt text"));
        // Phase 1 must NOT write the completion section or end marker
        assert!(!content.contains("=== END ==="), "phase-1 must not contain END marker");
        assert!(!content.contains("--- COMPLETION ---"), "phase-1 must not contain COMPLETION section");
        assert!(!content.contains("--- NORMALIZED OUTPUT ---"), "phase-1 must not contain NORMALIZED OUTPUT");
    }

    #[test]
    fn begin_transcript_entry_includes_untracked_annotation_for_fallback() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: FALLBACK_WORKFLOW.to_string(),
            phase: FALLBACK_PHASE.to_string(),
            round: 0,
            role: "unknown".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None,
        };
        let _handle = begin_transcript_entry(&project.config, &meta, "p", None);

        let path = project.config.state_dir.join("transcript.log");
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("tracking: untracked (direct agent call)"),
            "fallback begin entries must include tracking annotation"
        );
    }

    #[test]
    fn begin_transcript_entry_returns_none_when_disabled() {
        let project = new_project(); // transcript_enabled = false by default
        let meta = AgentCallMeta {
            agent_name: "claude".to_string(),
            role: "implementer".to_string(),
            ..AgentCallMeta::default()
        };
        let handle = begin_transcript_entry(&project.config, &meta, "prompt", None);
        assert!(handle.is_none(), "should return None when transcript disabled");
        let path = project.config.state_dir.join("transcript.log");
        assert!(!path.exists(), "no file should be created when disabled");
    }

    #[test]
    fn complete_transcript_entry_appends_completion_output_and_end() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "implementer".to_string(),
            round: 1,
            role: "implementer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None,
        };
        let handle = begin_transcript_entry(&project.config, &meta, "user p", None);
        complete_transcript_entry(
            handle.as_ref(),
            TranscriptCompletionStatus::Completed,
            None,
            "the output",
        );

        let path = project.config.state_dir.join("transcript.log");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("--- COMPLETION ---"));
        assert!(content.contains("status: completed"));
        assert!(content.contains("ended_at:"));
        assert!(!content.contains("failure_reason:"), "completed entries must not have failure_reason");
        assert!(content.contains("--- NORMALIZED OUTPUT ---"));
        assert!(content.contains("the output"));
        assert!(content.contains("=== END ==="));
    }

    #[test]
    fn complete_transcript_entry_records_failure_reason() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "implementer".to_string(),
            round: 1,
            role: "implementer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None,
        };
        let handle = begin_transcript_entry(&project.config, &meta, "user p", None);
        complete_transcript_entry(
            handle.as_ref(),
            TranscriptCompletionStatus::Failed,
            Some("agent timed out after 60s"),
            "partial output",
        );

        let path = project.config.state_dir.join("transcript.log");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("--- COMPLETION ---"));
        assert!(content.contains("status: failed"));
        assert!(content.contains("ended_at:"));
        assert!(content.contains("failure_reason: agent timed out after 60s"));
        assert!(content.contains("--- NORMALIZED OUTPUT ---"));
        assert!(content.contains("partial output"), "failed entries must still record captured output");
        assert!(content.contains("=== END ==="));
    }

    #[test]
    fn complete_transcript_entry_noop_when_handle_is_none() {
        // Should not panic or create any files.
        complete_transcript_entry(
            None,
            TranscriptCompletionStatus::Completed,
            None,
            "output",
        );
    }

    #[test]
    fn append_transcript_entry_wrapper_produces_correct_format() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "implementer".to_string(),
            round: 1,
            role: "implementer".to_string(),
            agent_name: "claude".to_string(),
            session_hint: None,
        };
        append_transcript_entry(&project.config, &meta, "user p", None, "output text");

        let path = project.config.state_dir.join("transcript.log");
        let content = fs::read_to_string(&path).unwrap();
        // Verify ordering: in_progress appears before COMPLETION
        let in_progress_pos = content.find("status: in_progress").expect("status: in_progress must be present");
        let completion_pos = content.find("--- COMPLETION ---").expect("--- COMPLETION --- must be present");
        let output_pos = content.find("--- NORMALIZED OUTPUT ---").expect("--- NORMALIZED OUTPUT --- must be present");
        let end_pos = content.find("=== END ===").expect("=== END === must be present");
        assert!(in_progress_pos < completion_pos, "in_progress must come before COMPLETION");
        assert!(completion_pos < output_pos, "COMPLETION must come before NORMALIZED OUTPUT");
        assert!(output_pos < end_pos, "NORMALIZED OUTPUT must come before END");
        assert!(content.contains("status: completed"));
        assert!(content.contains("ended_at:"));
        assert!(content.contains("output text"));
    }

    #[test]
    fn rotation_triggers_after_complete_transcript_entry() {
        let mut project = new_project();
        project.config.transcript_enabled = true;
        fs::create_dir_all(&project.config.state_dir).unwrap();

        let path = project.config.state_dir.join("transcript.log");
        // Pre-seed a large file
        let mut big = String::new();
        for i in 0..TRANSCRIPT_MAX_LINES + 100 {
            big.push_str(&format!("line {i}\n"));
        }
        fs::write(&path, &big).unwrap();

        let meta = AgentCallMeta {
            workflow: FALLBACK_WORKFLOW.to_string(),
            phase: FALLBACK_PHASE.to_string(),
            ..AgentCallMeta::default()
        };
        let handle = begin_transcript_entry(&project.config, &meta, "p", None);
        complete_transcript_entry(handle.as_ref(), TranscriptCompletionStatus::Completed, None, "o");

        let content = fs::read_to_string(&path).unwrap();
        let line_count = content.lines().count();
        assert!(
            line_count <= TRANSCRIPT_MAX_LINES,
            "transcript should be rotated after complete: got {line_count} lines"
        );
        assert!(content.starts_with("[transcript rotated]"));
    }
}
