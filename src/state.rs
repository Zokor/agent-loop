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

const LAST_RUN_TASK_MAX_CHARS: usize = 500;
const CONVERSATION_MAX_LINES: usize = 200;
const DECISIONS_REFERENCE_START: &str = "<!-- agent-loop:decisions-reference:start -->";
const DECISIONS_REFERENCE_END: &str = "<!-- agent-loop:decisions-reference:end -->";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<u32>,
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
    pub rating: Option<u32>,
}

fn state_file_path(name: &str, config: &Config) -> PathBuf {
    config.state_dir.join(Path::new(name))
}

pub fn decisions_path(config: &Config) -> PathBuf {
    config.project_dir.join(".agent-loop").join("decisions.md")
}

pub fn read_decisions(config: &Config) -> String {
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
        rating: None,
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

    // --- implementer ---
    let implementer = match map.get("implementer") {
        Some(v) => match v.as_str() {
            Some(s) => s.to_owned(),
            None => {
                warnings.push(format!(
                    "field 'implementer': expected string, got {}; falling back to '{}'",
                    v, fallback.implementer
                ));
                fallback.implementer.clone()
            }
        },
        None => {
            warnings.push(format!(
                "field 'implementer': missing; falling back to '{}'",
                fallback.implementer
            ));
            fallback.implementer.clone()
        }
    };

    // --- reviewer ---
    let reviewer = match map.get("reviewer") {
        Some(v) => match v.as_str() {
            Some(s) => s.to_owned(),
            None => {
                warnings.push(format!(
                    "field 'reviewer': expected string, got {}; falling back to '{}'",
                    v, fallback.reviewer
                ));
                fallback.reviewer.clone()
            }
        },
        None => {
            warnings.push(format!(
                "field 'reviewer': missing; falling back to '{}'",
                fallback.reviewer
            ));
            fallback.reviewer.clone()
        }
    };

    // --- mode ---
    let mode = match map.get("mode") {
        Some(v) => match v.as_str() {
            Some(s) if matches!(s, "single-agent" | "dual-agent") => s.to_owned(),
            Some(s) => {
                warnings.push(format!(
                    "field 'mode': unsupported value '{}'; falling back to '{}'",
                    sanitize_for_display(s),
                    fallback.mode
                ));
                fallback.mode.clone()
            }
            None => {
                warnings.push(format!(
                    "field 'mode': expected string, got {}; falling back to '{}'",
                    v, fallback.mode
                ));
                fallback.mode.clone()
            }
        },
        None => {
            warnings.push(format!(
                "field 'mode': missing; falling back to '{}'",
                fallback.mode
            ));
            fallback.mode.clone()
        }
    };

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

    // --- rating (optional — warn only when present but invalid) ---
    let rating = match map.get("rating") {
        Some(Value::Number(value)) => {
            match value
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| (1..=5).contains(v))
            {
                Some(r) => Some(r),
                None => {
                    warnings.push(format!(
                        "field 'rating': value {} out of range 1..=5; ignoring",
                        value
                    ));
                    None
                }
            }
        }
        Some(v) if !v.is_null() => {
            warnings.push(format!(
                "field 'rating': expected number, got {}; ignoring",
                v
            ));
            None
        }
        _ => None,
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
            rating,
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
    status.timestamp != expected_ts
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
        rating,
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
    let next_rating = match rating {
        Some(value) => Some(value),
        None if clear_stale_diagnostics => None,
        None => current.rating,
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
        rating: next_rating,
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
    let raw = read_state_file("findings.json", config);
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
            let warning = format!("invalid findings.json: {err}; starting fresh");
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
    write_state_file("findings.json", &serialized, config)
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
    ensure_project_guide_decisions_references(config);
    // Accepted for API parity with the TypeScript implementation's checkpoint baseline flow.
    let _baseline_files = baseline_files;

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
            config.max_rounds, config.timeout_seconds
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
            rating: None,
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
        let project = new_project();
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
        assert_eq!(status.rating, None);

        let workflow = read_workflow(&project.config);
        assert_eq!(workflow, Some(WorkflowKind::Implement));

        let log_content = read_state_file("log.txt", &project.config);
        assert!(log_content.contains("Agent loop initialized"));
        assert!(log_content.contains("Task: Build a robust state module"));
    }

    #[test]
    fn init_decisions_reference_blocks_are_idempotent() {
        let project = new_project();
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
        let project = new_project();

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
    fn loop_status_serialization_omits_none_rating_and_includes_present_rating() {
        let without_rating = LoopStatus {
            status: Status::Approved,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "task".to_string(),
            reason: None,
            rating: None,
            timestamp: "2026-02-14T00:00:00.000Z".to_string(),
        };

        let json_without = serde_json::to_value(&without_rating).expect("should serialize");
        assert!(!json_without.as_object().unwrap().contains_key("rating"));

        let with_rating = LoopStatus {
            rating: Some(4),
            ..without_rating
        };

        let json_with = serde_json::to_value(&with_rating).expect("should serialize");
        assert_eq!(json_with["rating"], 4);
    }

    #[test]
    fn normalize_status_value_accepts_valid_ratings_and_rejects_invalid() {
        let project = new_project();

        // Valid rating: 1
        let raw = json!({"rating": 1});
        assert_eq!(
            normalize_status_value(&raw, &project.config).rating,
            Some(1)
        );

        // Valid rating: 5
        let raw = json!({"rating": 5});
        assert_eq!(
            normalize_status_value(&raw, &project.config).rating,
            Some(5)
        );

        // Valid rating: 3
        let raw = json!({"rating": 3});
        assert_eq!(
            normalize_status_value(&raw, &project.config).rating,
            Some(3)
        );

        // Invalid: 0 (out of range)
        let raw = json!({"rating": 0});
        assert_eq!(normalize_status_value(&raw, &project.config).rating, None);

        // Invalid: 6 (out of range)
        let raw = json!({"rating": 6});
        assert_eq!(normalize_status_value(&raw, &project.config).rating, None);

        // Invalid: negative
        let raw = json!({"rating": -1});
        assert_eq!(normalize_status_value(&raw, &project.config).rating, None);

        // Invalid: float
        let raw = json!({"rating": 3.5});
        assert_eq!(normalize_status_value(&raw, &project.config).rating, None);

        // Invalid: string
        let raw = json!({"rating": "4"});
        assert_eq!(normalize_status_value(&raw, &project.config).rating, None);

        // Invalid: null
        let raw = json!({"rating": null});
        assert_eq!(normalize_status_value(&raw, &project.config).rating, None);

        // Missing rating
        let raw = json!({"status": "APPROVED"});
        assert_eq!(normalize_status_value(&raw, &project.config).rating, None);
    }

    #[test]
    fn write_status_clears_stale_rating_on_status_transition_and_allows_explicit_override() {
        let project = new_project();
        write_state_file("task.md", "test task", &project.config)
            .expect("task.md should be writable");

        // Set initial rating
        let first = write_status(
            StatusPatch {
                status: Some(Status::Approved),
                rating: Some(4),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("first write should succeed");
        assert_eq!(first.rating, Some(4));

        // Status transition without explicit rating clears stale rating.
        let second = write_status(
            StatusPatch {
                status: Some(Status::Consensus),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("second write should succeed");
        assert_eq!(second.rating, None);

        // Explicit rating overwrites
        let third = write_status(
            StatusPatch {
                rating: Some(5),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("third write should succeed");
        assert_eq!(third.rating, Some(5));
    }

    #[test]
    fn write_status_preserves_reason_and_rating_when_status_is_unchanged() {
        let project = new_project();
        write_state_file("task.md", "test task", &project.config)
            .expect("task.md should be writable");

        let initial = write_status(
            StatusPatch {
                status: Some(Status::NeedsChanges),
                reason: Some("missing test coverage".to_string()),
                rating: Some(2),
                ..StatusPatch::default()
            },
            &project.config,
        )
        .expect("initial write should succeed");
        assert_eq!(initial.reason.as_deref(), Some("missing test coverage"));
        assert_eq!(initial.rating, Some(2));

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
        assert_eq!(updated_task.rating, Some(2));
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
    fn is_status_stale_returns_true_when_timestamp_differs() {
        let status = LoopStatus {
            status: Status::Approved,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "task".to_string(),
            reason: None,
            rating: None,
            timestamp: "2026-02-14T00:00:00.000Z".to_string(),
        };

        assert!(is_status_stale("2026-02-14T12:00:00.000Z", &status));
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
            rating: None,
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

        // Should produce warnings for: status, round, implementer, reviewer, mode, timestamp
        let required_fields = [
            "status",
            "round",
            "implementer",
            "reviewer",
            "mode",
            "timestamp",
        ];
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

        for field in [
            "status",
            "round",
            "implementer",
            "reviewer",
            "mode",
            "timestamp",
        ] {
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

        // Unsupported mode
        let raw = json!({"status": "PENDING", "round": 1, "implementer": "a", "reviewer": "b", "mode": "triple-agent", "timestamp": "t"});
        let result = normalize_status_value_with_warnings(&raw, &project.config);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("'mode'") && w.contains("unsupported")),
            "expected warning for unsupported mode, got: {:?}",
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
            "rating": 4,
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

        // rating out of range
        let raw = json!({
            "status": "PENDING", "round": 0, "implementer": "a",
            "reviewer": "b", "mode": "dual-agent", "timestamp": "t",
            "rating": 10
        });
        let result = normalize_status_value_with_warnings(&raw, &project.config);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("'rating'") && w.contains("out of range")),
            "expected out-of-range rating warning, got: {:?}",
            result.warnings
        );

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

    #[test]
    fn warnings_mode_with_control_chars_are_sanitized() {
        let project = new_project();
        let raw = json!({
            "status": "PENDING",
            "round": 0,
            "implementer": "a",
            "reviewer": "b",
            "mode": "evil\x1b[31m-mode",
            "timestamp": "t"
        });

        let result = normalize_status_value_with_warnings(&raw, &project.config);
        let mode_warning = result
            .warnings
            .iter()
            .find(|w| w.contains("'mode'"))
            .expect("should have mode warning");

        // The ESC byte should be replaced with U+FFFD
        assert!(
            !mode_warning.contains('\x1b'),
            "warning should not contain raw ESC, got: {mode_warning}"
        );
        assert!(
            mode_warning.contains('\u{FFFD}'),
            "warning should contain replacement char, got: {mode_warning}"
        );
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
                },
                TaskStatusEntry {
                    title: "Task 2: Add retries".to_string(),
                    status: TaskRunStatus::Failed,
                    retries: 2,
                    last_error: Some("MAX_ROUNDS reached".to_string()),
                },
                TaskStatusEntry {
                    title: "Task 3: Cleanup".to_string(),
                    status: TaskRunStatus::Pending,
                    retries: 0,
                    last_error: None,
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
    fn task_status_write_uses_atomic_state_file() {
        let project = new_project();
        let status_file = TaskStatusFile {
            tasks: vec![TaskStatusEntry {
                title: "Task 1".to_string(),
                status: TaskRunStatus::Running,
                retries: 0,
                last_error: None,
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
}
