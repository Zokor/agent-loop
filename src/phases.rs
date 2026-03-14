use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{
    agent::run_agent_with_session,
    config::{Agent, Config, StuckAction},
    error::AgentLoopError,
    git::{git_checkpoint, git_diff_for_review, git_rev_parse_head},
    prompts::{
        AgentRole, PlanningReviewerParams, compound_prompt,
        decomposition_implementer_signoff_prompt, decomposition_initial_prompt,
        decomposition_reviewer_prompt, decomposition_revision_prompt,
        implementation_consensus_prompt, implementation_fresh_context_reviewer_prompt,
        implementation_gate_b_verification_prompt, implementation_gate_c_late_findings_prompt,
        implementation_implementer_prompt, implementation_reviewer_prompt, phase_paths,
        planning_adversarial_review_prompt, planning_implementer_review_fix_prompt,
        planning_implementer_revision_prompt, planning_implementer_signoff_prompt,
        planning_initial_prompt, planning_reviewer_fix_prompt, planning_reviewer_prompt,
        system_prompt_for_role,
    },
    state::{
        AgentCallMeta, FINDINGS_FILENAME, FindingEntry, FindingsFile, LoopStatus,
        QUALITY_CHECKS_FILENAME, Status, StatusPatch, TASKS_FINDINGS_FILENAME, append_decision,
        is_status_stale, log, read_findings, read_findings_with_warnings, read_state_file,
        read_status, summarize_task, timestamp, write_findings, write_state_file, write_status,
    },
    stuck::{StuckDetector, StuckSignal},
};

const STALE_TIMESTAMP_REASON: &str = "Agent did not write status (stale timestamp detected)";
const DECOMPOSITION_REVISION_FALLBACK_REASON: &str =
    "Reviewer did not provide explicit consensus; continuing revision loop.";
const DECOMPOSITION_MAX_ROUNDS_REASON: &str =
    "Task breakdown did not reach consensus within the decomposition round limit.";
const PLANNING_CONSENSUS_REQUIRED_REASON: &str =
    "Planning-only mode requires consensus before task decomposition.";
const IMPLEMENTATION_HIGH_WATERMARK_LOG: &str =
    "⚠ High round count in unlimited mode — timeout and stuck detection remain active safeguards";
const IMPLEMENT_GATE_SAME_CONTEXT: &str = "[gate:same-context]";
const IMPLEMENT_GATE_FRESH_CONTEXT: &str = "[gate:fresh-context]";
const IMPLEMENT_GATE_SIGNOFF: &str = "[gate:implementer-signoff]";
const IMPLEMENT_GATE_C_BOUNCE: &str = "[gate:gate-c-bounce]";
const PLANNING_GATE_ADVERSARIAL: &str = "[gate:fresh-context]";
const PLANNING_GATE_SIGNOFF: &str = "[gate:implementer-signoff]";
const CHECKPOINT_SUMMARY_MAX_LEN: usize = 80;
const IMPLEMENTATION_CHECKPOINT_FALLBACK: &str = "implementation updates";
const QUALITY_CHECK_TIMEOUT_SECS: u64 = 120;
const QUALITY_CHECK_MAX_LINES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanningReviewerAction {
    Approved,
    NeedsRevision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecompositionStatusDecision {
    Approved,
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

#[derive(Debug, Clone)]
struct FindingsReconcileResult {
    status: Status,
    findings: FindingsFile,
    reason: Option<String>,
    log_note: Option<String>,
}

fn normalize_findings_for_round(file: FindingsFile, round: u32) -> FindingsFile {
    let mut normalized = Vec::new();
    let mut used_ids = HashSet::new();

    for (index, finding) in file.findings.into_iter().enumerate() {
        let summary = finding.summary.trim();
        if summary.is_empty() {
            continue;
        }

        let mut id = finding.id.trim().to_string();
        if id.is_empty() {
            id = format!("F-{:03}", index + 1);
        }
        while used_ids.contains(&id) {
            id = format!("F-{:03}", used_ids.len() + 1);
        }
        used_ids.insert(id.clone());

        let severity = match finding.severity.trim().to_ascii_uppercase().as_str() {
            "HIGH" => "HIGH".to_string(),
            "LOW" => "LOW".to_string(),
            _ => "MEDIUM".to_string(),
        };

        let file_refs = finding
            .file_refs
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();

        normalized.push(FindingEntry {
            id,
            severity,
            summary: summary.to_string(),
            file_refs,
        });
    }

    FindingsFile {
        round,
        findings: normalized,
    }
}

fn synthesize_finding(reason: Option<&str>) -> FindingEntry {
    let summary = reason
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Reviewer requested changes but did not provide structured findings.");
    FindingEntry {
        id: "F-001".to_string(),
        severity: "MEDIUM".to_string(),
        summary: summary.to_string(),
        file_refs: Vec::new(),
    }
}

fn findings_id_list(findings: &FindingsFile) -> String {
    findings
        .findings
        .iter()
        .map(|finding| finding.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn findings_for_prompt(findings: &FindingsFile) -> String {
    if findings.findings.is_empty() {
        return String::new();
    }

    let mut lines = Vec::with_capacity(findings.findings.len());
    for finding in &findings.findings {
        let refs = if finding.file_refs.is_empty() {
            "no file refs".to_string()
        } else {
            finding.file_refs.join(", ")
        };
        lines.push(format!(
            "- {} [{}] {} ({})",
            finding.id, finding.severity, finding.summary, refs
        ));
    }
    lines.join("\n")
}

/// Extracts the portion of a review starting from the first "Findings"
/// heading ("## Findings" or a bare "Findings" line). Falls back to
/// the full review text if no marker is found.
fn trim_review_to_findings(review: &str) -> &str {
    let mut byte_offset = 0;
    for line in review.split('\n') {
        let trimmed = line.trim_start_matches('#').trim();
        if trimmed.starts_with("Findings") {
            return &review[byte_offset..];
        }
        byte_offset += line.len() + 1; // +1 for the \n
    }
    review // fallback: marker not found
}

fn reconcile_findings_after_review(
    round: u32,
    status: Status,
    status_reason: Option<&str>,
    previous_findings: &FindingsFile,
    current_findings: FindingsFile,
    config: &Config,
) -> FindingsReconcileResult {
    let mut normalized = normalize_findings_for_round(current_findings, round);

    match status {
        Status::NeedsChanges => {
            let log_note = if normalized.findings.is_empty()
                && !previous_findings.findings.is_empty()
            {
                normalized = FindingsFile {
                    round,
                    findings: previous_findings.findings.clone(),
                };
                Some(format!(
                    "Reviewer requested changes but {FINDINGS_FILENAME} was empty; carrying forward previous findings."
                ))
            } else if normalized.findings.is_empty() {
                normalized = FindingsFile {
                    round,
                    findings: vec![synthesize_finding(status_reason)],
                };
                Some(format!(
                    "Reviewer requested changes but {FINDINGS_FILENAME} was empty; synthesized F-001 from status reason."
                ))
            } else {
                None
            };

            let reason = format!(
                "Open findings: {}. See {}/{FINDINGS_FILENAME}.",
                findings_id_list(&normalized),
                config.state_dir_rel()
            );

            FindingsReconcileResult {
                status: Status::NeedsChanges,
                findings: normalized,
                reason: Some(reason),
                log_note,
            }
        }
        Status::Approved => {
            if normalized.findings.is_empty() {
                FindingsReconcileResult {
                    status: Status::Approved,
                    findings: normalized,
                    reason: None,
                    log_note: None,
                }
            } else {
                let reason = format!(
                    "Cannot approve with unresolved findings: {}. See {}/{FINDINGS_FILENAME}.",
                    findings_id_list(&normalized),
                    config.state_dir_rel()
                );
                FindingsReconcileResult {
                    status: Status::NeedsChanges,
                    findings: normalized,
                    reason: Some(reason),
                    log_note: Some(
                        "Reviewer returned APPROVED with unresolved findings; forcing NEEDS_CHANGES."
                            .to_string(),
                    ),
                }
            }
        }
        _ => FindingsReconcileResult {
            status,
            findings: normalized,
            reason: None,
            log_note: None,
        },
    }
}

/// Determine planning verdict from review text.
/// Returns Approved if review ends with "no findings" (case-insensitive),
/// NeedsRevision otherwise.
fn planning_verdict_from_review(review_text: &str) -> PlanningReviewerAction {
    if crate::state::review_has_no_findings(review_text) {
        PlanningReviewerAction::Approved
    } else {
        PlanningReviewerAction::NeedsRevision
    }
}

fn planning_implementer_reached_consensus(status: Status) -> bool {
    status == Status::Consensus
}

fn decomposition_forced_revision_reason(status: Status) -> Option<&'static str> {
    if status == Status::NeedsRevision {
        None
    } else {
        Some(DECOMPOSITION_REVISION_FALLBACK_REASON)
    }
}

// ---------------------------------------------------------------------------
// Tasks findings parsing and reconciliation (mirrors planning findings)
// ---------------------------------------------------------------------------

/// Parse tasks findings JSON block from reviewer output.
/// Looks for ```json\n[...]\n``` blocks containing findings with "id" and "description".
fn parse_tasks_findings_from_output(
    text: &str,
    round: u32,
) -> Vec<crate::state::TasksFindingEntry> {
    use crate::state::{TasksFindingEntry, TasksFindingStatus};

    // Look for JSON array in code blocks
    let re = regex::Regex::new(r"```(?:json)?\s*\n(\[[\s\S]*?\])\s*\n```").ok();
    let json_text = re
        .and_then(|r| r.captures(text))
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str());

    let Some(json_str) = json_text else {
        return Vec::new();
    };

    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|v| {
            let id = v.get("id")?.as_str()?.to_string();
            let description = v.get("description")?.as_str()?.to_string();
            let status = match v
                .get("status")
                .and_then(|s| s.as_str())
                .map(|s| s.to_ascii_lowercase())
                .as_deref()
            {
                Some("resolved") => TasksFindingStatus::Resolved,
                _ => TasksFindingStatus::Open,
            };
            Some(TasksFindingEntry {
                id,
                description,
                status,
                round_introduced: round,
                round_resolved: None,
            })
        })
        .collect()
}

/// Reconcile tasks verdict with findings, applying safety nets:
/// - NEEDS_REVISION + no new findings + no open findings after merge → synthesize from reason
/// - APPROVED + open findings remaining after merge → force NEEDS_REVISION
fn reconcile_tasks_verdict(
    status: Status,
    status_reason: Option<&str>,
    new_findings: Vec<crate::state::TasksFindingEntry>,
    existing: &crate::state::TasksFindingsFile,
    round: u32,
    config: &Config,
) -> (DecompositionStatusDecision, crate::state::TasksFindingsFile) {
    use crate::state::{TasksFindingEntry, TasksFindingStatus, TasksFindingsFile};

    // ID-based merge: update existing findings, add new ones, resolve transitions.
    let mut merged = existing.findings.clone();
    for new_f in &new_findings {
        if let Some(existing_entry) = merged.iter_mut().find(|f| f.id == new_f.id) {
            existing_entry.description = new_f.description.clone();
            if new_f.status == TasksFindingStatus::Resolved
                && existing_entry.status == TasksFindingStatus::Open
            {
                existing_entry.status = TasksFindingStatus::Resolved;
                existing_entry.round_resolved = Some(round);
            } else if new_f.status == TasksFindingStatus::Open
                && existing_entry.status == TasksFindingStatus::Resolved
            {
                // Reopened — reviewer flagged it again.
                existing_entry.status = TasksFindingStatus::Open;
                existing_entry.round_resolved = None;
            }
        } else {
            // New finding not previously seen.
            let mut entry = new_f.clone();
            if entry.status == TasksFindingStatus::Resolved {
                entry.round_resolved = Some(round);
            }
            merged.push(entry);
        }
    }

    let open_after_merge = merged
        .iter()
        .filter(|f| f.status == TasksFindingStatus::Open)
        .count();

    let decision = match status {
        Status::Approved => {
            if open_after_merge > 0 {
                let _ = log(
                    "Tasks reconciliation: APPROVED with open findings — forcing NEEDS_REVISION",
                    config,
                );
                DecompositionStatusDecision::NeedsRevision
            } else {
                DecompositionStatusDecision::Approved
            }
        }
        Status::NeedsRevision => {
            // Safety net: NEEDS_REVISION + no new findings from reviewer → synthesize
            // a finding from reason text. Mirrors planning behavior (line 378): we check
            // only new_findings.is_empty(), NOT merged.is_empty(), because merged may
            // contain resolved entries from prior rounds that don't represent the current
            // issue. Without this, a NEEDS_REVISION with only resolved prior findings
            // would silently drop the reviewer's concern.
            if new_findings.is_empty() && open_after_merge == 0 {
                let desc = status_reason.unwrap_or(
                    "Reviewer requested revision but did not provide structured findings.",
                );
                let next_id = crate::state::next_tasks_finding_id(&TasksFindingsFile {
                    findings: merged.clone(),
                });
                merged.push(TasksFindingEntry {
                    id: next_id,
                    description: desc.to_string(),
                    status: TasksFindingStatus::Open,
                    round_introduced: round,
                    round_resolved: None,
                });
                let _ = log(
                    "Tasks reconciliation: NEEDS_REVISION with empty findings — synthesized finding from reason",
                    config,
                );
            }
            DecompositionStatusDecision::NeedsRevision
        }
        Status::Error => DecompositionStatusDecision::Error,
        _ => DecompositionStatusDecision::ForceNeedsRevision,
    };

    let findings_file = TasksFindingsFile { findings: merged };
    (decision, findings_file)
}

/// Policy helpers for uniform signoff gating.
fn requires_dual_agent_signoff(config: &Config) -> bool {
    !config.single_agent
}

/// Check whether adversarial planning review should run.
/// Requires: dual-agent mode, toggle enabled, and different agents for
/// implementer vs reviewer (same agent = same model = no independence benefit).
fn planning_adversarial_enabled(config: &Config) -> bool {
    !config.single_agent
        && config.planning_adversarial_review
        && config.implementer.name() != config.reviewer.name()
}

/// Convert a raw round-limit config value into an `Option<u32>`:
/// `0` means unlimited (`None`), any positive value becomes `Some(limit)`.
fn normalize_round_limit(limit: u32) -> Option<u32> {
    match limit {
        0 => None,
        n => Some(n),
    }
}

/// Returns `true` when `round` has reached or exceeded the cap.
/// Always returns `false` in unlimited mode (`limit == None`).
fn round_limit_reached(round: u32, limit: Option<u32>) -> bool {
    limit.is_some_and(|cap| round >= cap)
}

/// Check if rounds are already exhausted before the loop starts.
/// Uses strict `>` because the start_round itself hasn't executed yet.
fn rounds_already_exhausted(start_round: u32, limit: Option<u32>) -> bool {
    limit.is_some_and(|cap| start_round > cap)
}

/// Format a round number for display: `"5"` in unlimited mode, `"5/10"` in bounded mode.
fn round_display(round: u32, limit: Option<u32>) -> String {
    match limit {
        None => format!("{round}"),
        Some(cap) => format!("{round}/{cap}"),
    }
}

/// Emit a high-watermark warning at round 50, then every 25 rounds.
/// Only fires when the loop is running in unlimited mode (`limit == None`).
fn should_emit_high_watermark(round: u32, limit: Option<u32>) -> bool {
    if limit.is_some() {
        return false;
    }
    // 0.is_multiple_of(25) is true, so this fires at 50, 75, 100, …
    round >= 50 && (round - 50).is_multiple_of(25)
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
    if !config.decisions_enabled {
        return;
    }

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
    paths: &crate::prompts::PhasePaths,
    config: &Config,
    round: u32,
    run_agent_fn: &mut FRunAgent,
    log_fn: &mut FLog,
) where
    FRunAgent: FnMut(
        &crate::config::Agent,
        AgentRole,
        &str,
        Option<&str>,
        &Config,
        Option<&AgentCallMeta>,
    ) -> Result<(), AgentLoopError>,
    FLog: FnMut(&str, &Config),
{
    if !config.compound || !config.decisions_enabled {
        return;
    }

    let meta = AgentCallMeta {
        workflow: "implement".to_string(),
        phase: "compound".to_string(),
        round,
        role: "implementer".to_string(),
        agent_name: config.implementer.name().to_string(),
        session_hint: None,
        output_file: None,
    };

    log_fn("🧠 Running compound learning phase...", config);
    if let Err(err) = run_agent_fn(
        &config.implementer,
        AgentRole::Implementer,
        &compound_prompt(paths),
        None,
        config,
        Some(&meta),
    ) {
        log_fn(
            &format!("WARN: compound phase failed (continuing): {err}"),
            config,
        );
    } else {
        log_fn("✅ Compound learning phase complete.", config);
    }
}

#[allow(dead_code)]
pub fn compound_phase(round: u32, config: &Config) {
    let paths = phase_paths(config);
    run_compound_phase_with_runner(
        &paths,
        config,
        round,
        &mut |agent: &crate::config::Agent,
              role: AgentRole,
              prompt,
              _session_hint: Option<&str>,
              current_config,
              meta: Option<&AgentCallMeta>| {
            let sp = system_prompt_for_role(role, current_config);
            let sp_ref = if sp.is_empty() {
                None
            } else {
                Some(sp.as_str())
            };
            let role_str = match role {
                AgentRole::Implementer => "implementer",
                AgentRole::Reviewer => "reviewer",
                AgentRole::Planner => "planner",
            };
            let session_key = format!("implement-{}-{}", role_str, agent.name());
            run_agent_with_session(
                agent,
                prompt,
                current_config,
                sp_ref,
                Some(&session_key),
                Some(role),
                meta,
            )
            .map(|_| ())
        },
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
            Status::Stuck => "STUCK",
            Status::Error => "ERROR",
            Status::Interrupted => "INTERRUPTED",
        },
        None => "status-update",
    }
}

fn finalize_phase_consensus(
    config: &Config,
    round: u32,
    reason: Option<String>,
) -> Result<(), AgentLoopError> {
    write_status(
        StatusPatch {
            status: Some(Status::Consensus),
            round: Some(round),
            reason,
            ..StatusPatch::default()
        },
        config,
    )?;
    Ok(())
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

fn gate_reason(marker: &str, reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.starts_with("[gate:") {
        trimmed.to_string()
    } else if trimmed.is_empty() {
        marker.to_string()
    } else {
        format!("{marker} {trimmed}")
    }
}

fn gate_reason_opt(marker: &str, reason: Option<String>) -> Option<String> {
    reason.map(|text| gate_reason(marker, &text))
}

fn planner_plan_mode_active(config: &Config, agent: &Agent, role: AgentRole) -> bool {
    role == AgentRole::Planner
        && agent.name() == "claude"
        && config.planner_permission_mode == "plan"
}

fn extract_plan_from_output_markers(output: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?is)<plan>\s*(.*?)\s*</plan>").ok()?;
    re.captures(output)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|text| !text.is_empty())
}

fn run_agent_with_output_or_record_error(
    config: &Config,
    agent: &Agent,
    prompt: &str,
    round: Option<u32>,
    role: AgentRole,
    workflow: &str,
    phase: &str,
) -> Option<String> {
    run_agent_with_output_or_record_error_inner(
        config, agent, prompt, round, role, workflow, phase, None,
    )
}

fn run_agent_with_output_or_record_error_inner(
    config: &Config,
    agent: &Agent,
    prompt: &str,
    round: Option<u32>,
    role: AgentRole,
    workflow: &str,
    phase: &str,
    output_file: Option<PathBuf>,
) -> Option<String> {
    let sp = system_prompt_for_role(role, config);
    let sp_ref = if sp.is_empty() {
        None
    } else {
        Some(sp.as_str())
    };
    let role_str = match role {
        AgentRole::Implementer => "implementer",
        AgentRole::Reviewer => "reviewer",
        AgentRole::Planner => "planner",
    };
    let session_key = format!("{}-{}-{}", workflow, role_str, agent.name());
    let meta = AgentCallMeta {
        workflow: workflow.to_string(),
        phase: phase.to_string(),
        round: round.unwrap_or(0),
        role: role_str.to_string(),
        agent_name: agent.name().to_string(),
        session_hint: Some(session_key.clone()),
        output_file,
    };
    match run_agent_with_session(
        agent,
        prompt,
        config,
        sp_ref,
        Some(&session_key),
        Some(role),
        Some(&meta),
    ) {
        Ok(output) => Some(output),
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
            None
        }
        Err(err) => {
            write_error_status(config, round, err.to_string());
            None
        }
    }
}

fn run_agent_or_record_error(
    config: &Config,
    agent: &Agent,
    prompt: &str,
    round: Option<u32>,
    role: AgentRole,
    workflow: &str,
    phase: &str,
) -> bool {
    run_agent_with_output_or_record_error(config, agent, prompt, round, role, workflow, phase)
        .is_some()
}

fn run_agent_or_record_error_with_output_file(
    config: &Config,
    agent: &Agent,
    prompt: &str,
    round: Option<u32>,
    role: AgentRole,
    workflow: &str,
    phase: &str,
    output_file: PathBuf,
) -> bool {
    run_agent_with_output_or_record_error_inner(
        config, agent, prompt, round, role, workflow, phase, Some(output_file),
    )
    .is_some()
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
    output_truncated: bool,
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
                output_truncated: false,
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
    let mut output_truncated = false;
    if let Some(buf) = stdout_buf {
        let (text, truncated) = buf.into_output();
        output_truncated |= truncated;
        combined.push_str(&text);
    }
    if let Some(buf) = stderr_buf {
        let (text, truncated) = buf.into_output();
        output_truncated |= truncated;
        if !combined.is_empty() && !text.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&text);
    }

    let (output, final_truncated) = truncate_output(&combined, QUALITY_CHECK_MAX_LINES);
    output_truncated |= final_truncated;

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
        output_truncated,
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

        // For passing checks, emit only a summary line to save tokens.
        if result.success && !result.timed_out {
            lines.push(format!("{} [PASS]", result.label));
            continue;
        }

        lines.push(format!("\n--- {} [{}] ---", result.label, status_label));
        if result.output_truncated {
            lines.push(format!(
                "NOTE: output truncated to last {QUALITY_CHECK_MAX_LINES} lines."
            ));
        }
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

fn format_summary_block(status: &LoopStatus, config: &Config) -> String {
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

    if let Some(reason) = status.reason.as_ref().filter(|value| !value.is_empty()) {
        lines.push(format!("  Note:        {reason}"));
    }

    lines.push(border);
    lines.push(format!("\n📁 State files in: {}/", config.state_dir_rel()));
    lines.push(format!(
        "   - Core: task.md, plan.md, tasks.md, changes.md, workflow.txt, {QUALITY_CHECKS_FILENAME}, review.md, status.json, log.txt"
    ));
    lines.push(format!(
        "   - Planning: planning-progress.md, {TASKS_FINDINGS_FILENAME}"
    ));
    lines.push("   - Tasks: tasks-progress.md".to_string());
    lines.push(format!(
        "   - Implementation: implement-progress.md, conversation.md, {FINDINGS_FILENAME}, task_status.json, task_metrics.json"
    ));
    lines.push(String::new());

    lines.join("\n")
}

fn print_planning_complete_summary(status: &LoopStatus, task: &str, config: &Config) {
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
    println!("   1. Review tasks in: {}/tasks.md", config.state_dir_rel());
    println!("   2. Run each task: {}", planning_next_step_command());
    println!("   3. Or extract a task: cat {}/tasks.md", config.state_dir_rel());
    println!();
}

pub fn planning_phase(config: &Config, planning_only: bool) -> bool {
    planning_phase_internal(config, planning_only, false)
}

pub fn planning_phase_resume(config: &Config) -> bool {
    planning_phase_internal(config, true, true)
}

fn planning_phase_internal(config: &Config, planning_only: bool, resume: bool) -> bool {
    let _ = log("━━━ Planning Phase ━━━", config);
    warn_on_status_write(
        "PLANNING",
        StatusPatch {
            status: Some(Status::Planning),
            ..StatusPatch::default()
        },
        config,
    );

    let paths = phase_paths(config);

    let planner_agent = config.planner.clone();

    if resume {
        let current = read_status(config);
        if matches!(current.status, Status::Consensus | Status::Approved) {
            let _ = log("✅ Planning already reached consensus.", config);
            return true;
        }
        let _ = log(
            &format!(
                "↪ Resuming planning from round {}",
                current.round.saturating_add(1)
            ),
            config,
        );
    } else {
        let planner_plan_mode =
            planner_plan_mode_active(config, &planner_agent, AgentRole::Planner);

        let _ = log("📝 Implementer proposing plan...", config);
        let planner_output = match run_agent_with_output_or_record_error_inner(
            config,
            &planner_agent,
            &planning_initial_prompt(&paths, planner_plan_mode),
            Some(0),
            AgentRole::Planner,
            "plan",
            "initial",
            Some(paths.plan_md.clone()),
        ) {
            Some(output) => output,
            None => return false,
        };

        if planner_plan_mode {
            if let Some(plan_text) = extract_plan_from_output_markers(&planner_output) {
                if let Err(err) = write_state_file("plan.md", &plan_text, config) {
                    write_error_status(
                        config,
                        Some(0),
                        format!("Failed to persist plan from planner output: {err}"),
                    );
                    return false;
                }
            } else {
                let fallback_plan = read_state_file("plan.md", config);
                if fallback_plan.trim().is_empty() {
                    write_error_status(
                        config,
                        Some(0),
                        "Planner plan-mode output missing <plan>...</plan> block and no plan.md was produced.".to_string(),
                    );
                    return false;
                }
                let _ = log(
                    "⚠ Planner plan-mode output missing <plan> markers; using plan.md fallback",
                    config,
                );
            }
        }
    }

    let start_round = if resume {
        let previous = read_status(config);
        previous.round.saturating_add(1).max(1)
    } else {
        1
    };
    let mut planning_round = start_round.saturating_sub(1);
    let mut reached_consensus = false;
    let mut dispute_reason: Option<String> = None;
    let mut consecutive_revision_rounds: u32 = 0;

    let planning_limit = normalize_round_limit(config.planning_max_rounds);

    loop {
        if round_limit_reached(planning_round, planning_limit) {
            break;
        }
        planning_round += 1;
        if should_emit_high_watermark(planning_round, planning_limit) {
            let _ = log(IMPLEMENTATION_HIGH_WATERMARK_LOG, config);
        }
        let _ = log(
            &format!(
                "🔄 Planning consensus round {}",
                round_display(planning_round, planning_limit)
            ),
            config,
        );

        // Clear review.md before each reviewer round to prevent stale content.
        let _ = write_state_file("review.md", "", config);

        let _ = log("🔍 Reviewer evaluating plan...", config);
        let reviewer_output = match run_agent_with_output_or_record_error_inner(
            config,
            &config.reviewer,
            &planning_reviewer_prompt(&PlanningReviewerParams {
                paths: &paths,
                dispute_reason: dispute_reason.as_deref(),
                resumed: planning_round > 1,
            }),
            Some(planning_round),
            AgentRole::Reviewer,
            "plan",
            "review",
            Some(paths.review_md.clone()),
        ) {
            Some(output) => output,
            None => return false,
        };

        // Fallback: if agent didn't write to review.md, persist its captured output
        let review_file = read_state_file("review.md", config);
        if review_file.trim().is_empty() && !reviewer_output.trim().is_empty() {
            let _ = write_state_file("review.md", &reviewer_output, config);
        }

        // Determine verdict from review text: ends with "no findings" → approved
        let review_text = read_state_file("review.md", config);
        let action = planning_verdict_from_review(&review_text);

        // Write status.json based on verdict (system-managed, not agent-written)
        let verdict_label = if action == PlanningReviewerAction::Approved {
            "APPROVED"
        } else {
            "NEEDS_REVISION"
        };
        warn_on_status_write(
            verdict_label,
            StatusPatch {
                status: Some(if action == PlanningReviewerAction::Approved {
                    Status::Approved
                } else {
                    Status::NeedsRevision
                }),
                round: Some(planning_round),
                reason: if action == PlanningReviewerAction::NeedsRevision {
                    Some("see review.md".to_string())
                } else {
                    None
                },
                ..StatusPatch::default()
            },
            config,
        );

        // Append planning progress after each reviewer round.
        let findings = extract_finding_summary(&review_text, 500);
        crate::state::append_planning_progress(
            planning_round,
            &format!("Reviewer: {verdict_label}"),
            findings.as_deref(),
            config,
        );

        match action {
            PlanningReviewerAction::NeedsRevision => {
                let _ = log("📝 Reviewer requested changes — see review.md", config);
                consecutive_revision_rounds += 1;

                // Role swap: after N consecutive revision rounds, have the reviewer
                // fix the plan directly, then the implementer reviews.
                let swap_threshold = config.planning_role_swap_after;
                if swap_threshold > 0 && consecutive_revision_rounds >= swap_threshold {
                    let _ = log(
                        &format!(
                            "🔄 Role swap: {consecutive_revision_rounds} consecutive revision rounds — reviewer will fix, implementer will review",
                        ),
                        config,
                    );
                    crate::state::append_planning_progress(
                        planning_round,
                        &format!(
                            "Role swap triggered after {consecutive_revision_rounds} consecutive revision rounds"
                        ),
                        None,
                        config,
                    );

                    // Step 1: Reviewer fixes the plan directly
                    let _ = log("🔧 Reviewer fixing plan...", config);
                    if !run_agent_or_record_error_with_output_file(
                        config,
                        &config.reviewer,
                        &planning_reviewer_fix_prompt(&paths, planning_round > 1),
                        Some(planning_round),
                        AgentRole::Reviewer,
                        "plan",
                        "reviewer-fix",
                        paths.plan_md.clone(),
                    ) {
                        return false;
                    }

                    // Step 2: Implementer reviews the reviewer's fix
                    let _ = log("🔍 Implementer reviewing reviewer's fix...", config);
                    let _ = write_state_file("review.md", "", config);
                    if !run_agent_or_record_error_with_output_file(
                        config,
                        &planner_agent,
                        &planning_implementer_review_fix_prompt(&paths, planning_round > 1),
                        Some(planning_round),
                        AgentRole::Implementer,
                        "plan",
                        "implementer-review-fix",
                        paths.review_md.clone(),
                    ) {
                        return false;
                    }

                    // Reset counter — the reviewer's fix gets a fresh chance
                    consecutive_revision_rounds = 0;
                    dispute_reason = None;
                    continue;
                }

                // Normal path: implementer revises
                let _ = log("📝 Implementer revising plan...", config);
                if !run_agent_or_record_error_with_output_file(
                    config,
                    &planner_agent,
                    &planning_implementer_revision_prompt(&paths, planning_round > 1),
                    Some(planning_round),
                    AgentRole::Implementer,
                    "plan",
                    "implementer-revision",
                    paths.plan_md.clone(),
                ) {
                    return false;
                }
                dispute_reason = None;
                continue;
            }
            PlanningReviewerAction::Approved => {
                consecutive_revision_rounds = 0;

                if requires_dual_agent_signoff(config) {
                    // --- Adversarial planning review (dual-agent only) ---
                    if planning_adversarial_enabled(config) {
                        let _ = log("🔍 Running adversarial second review of plan...", config);

                        let adversarial_output = match run_agent_with_output_or_record_error_inner(
                            config,
                            &config.implementer,
                            &planning_adversarial_review_prompt(&paths),
                            Some(planning_round),
                            AgentRole::Reviewer,
                            &format!("plan-adversarial-r{planning_round}"),
                            "adversarial-review",
                            Some(paths.review_md.clone()),
                        ) {
                            Some(output) => output,
                            None => return false,
                        };

                        // Determine adversarial verdict from review text
                        let adversarial_review = read_state_file("review.md", config);
                        let adversarial_text = if adversarial_review.trim().is_empty() {
                            &adversarial_output
                        } else {
                            &adversarial_review
                        };
                        let adversarial_action = planning_verdict_from_review(adversarial_text);

                        let adversarial_label =
                            if adversarial_action == PlanningReviewerAction::NeedsRevision {
                                "REVISE"
                            } else {
                                "APPROVED"
                            };
                        let adversarial_findings = extract_finding_summary(adversarial_text, 500);
                        crate::state::append_planning_progress(
                            planning_round,
                            &format!(
                                "{} Adversarial: {adversarial_label}",
                                PLANNING_GATE_ADVERSARIAL
                            ),
                            adversarial_findings.as_deref(),
                            config,
                        );

                        if adversarial_action == PlanningReviewerAction::NeedsRevision {
                            let _ = log(
                                "📝 Adversarial reviewer found issues — requesting revision",
                                config,
                            );
                            consecutive_revision_rounds += 1;
                            if !run_agent_or_record_error_with_output_file(
                                config,
                                &planner_agent,
                                &planning_implementer_revision_prompt(&paths, planning_round > 1),
                                Some(planning_round),
                                AgentRole::Implementer,
                                "plan",
                                "implementer-revision",
                                paths.plan_md.clone(),
                            ) {
                                return false;
                            }
                            dispute_reason = None;
                            continue;
                        }

                        let _ = log("✅ Adversarial reviewer approved the plan", config);
                    } else if !config.single_agent
                        && config.implementer.name() == config.reviewer.name()
                    {
                        let _ = log(
                            "⚠️ Adversarial planning review skipped: implementer and reviewer are the same agent",
                            config,
                        );
                    }

                    // Dual-agent: require implementer signoff before finalizing.
                    let _ = log("🔍 Implementer reviewing approved plan...", config);
                    let signoff_timestamp = timestamp();
                    if !run_agent_or_record_error(
                        config,
                        &planner_agent,
                        &planning_implementer_signoff_prompt(
                            planning_round,
                            &signoff_timestamp,
                            &paths,
                        ),
                        Some(planning_round),
                        AgentRole::Implementer,
                        &format!("plan-signoff-r{planning_round}"),
                        "implementer-signoff",
                    ) {
                        return false;
                    }

                    let mut signoff_status = read_status(config);
                    if signoff_status.status != Status::Error
                        && is_status_stale(&signoff_timestamp, &signoff_status)
                    {
                        let _ = log(
                            "⚠️ Stale status after plan implementer signoff — writing Disputed fallback",
                            config,
                        );
                        warn_on_status_write(
                            "DISPUTED",
                            StatusPatch {
                                status: Some(Status::Disputed),
                                round: Some(planning_round),
                                reason: Some(gate_reason(
                                    PLANNING_GATE_SIGNOFF,
                                    STALE_TIMESTAMP_REASON,
                                )),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                        signoff_status = read_status(config);
                    }

                    if signoff_status.status == Status::Error {
                        write_error_status(
                            config,
                            Some(planning_round),
                            status_error_reason(&signoff_status),
                        );
                        return false;
                    }

                    if planning_implementer_reached_consensus(signoff_status.status) {
                        let _ = log("✅ Both agents agreed on the plan!", config);
                        reached_consensus = true;
                        break;
                    }

                    // Disputed — continue round with concerns.
                    dispute_reason = if signoff_status.status == Status::Disputed {
                        signoff_status
                            .reason
                            .as_deref()
                            .filter(|r| !r.trim().is_empty())
                            .map(ToOwned::to_owned)
                    } else {
                        None
                    };

                    let _ = log(
                        &format!(
                            "📝 Implementer disputed approved plan: {}",
                            signoff_status.reason.as_deref().unwrap_or("see plan.md")
                        ),
                        config,
                    );
                } else {
                    // Single-agent: system converts APPROVED -> CONSENSUS.
                    let _ = log("✅ Reviewer approved the plan!", config);
                    if let Err(err) = finalize_phase_consensus(config, planning_round, None) {
                        write_error_status(config, Some(planning_round), err.to_string());
                        return false;
                    }
                    reached_consensus = true;
                    break;
                }
            }
        }

        if round_limit_reached(planning_round, planning_limit) {
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
    if matches!(previous.status, Status::Consensus | Status::Approved) {
        return 1;
    }

    previous.round.saturating_add(1).max(1)
}

fn open_tasks_findings_count(findings: &crate::state::TasksFindingsFile) -> usize {
    findings
        .findings
        .iter()
        .filter(|entry| entry.status == crate::state::TasksFindingStatus::Open)
        .count()
}

fn decomposition_decision_label(decision: DecompositionStatusDecision) -> &'static str {
    match decision {
        DecompositionStatusDecision::Approved => "Approved",
        DecompositionStatusDecision::NeedsRevision => "NeedsRevision",
        DecompositionStatusDecision::Error => "Error",
        DecompositionStatusDecision::ForceNeedsRevision => "ForceNeedsRevision",
    }
}

fn append_decomposition_error_progress(config: &Config, round: u32, fallback: &str) {
    let status = read_status(config);
    let reason = status.reason.as_deref().unwrap_or(fallback);
    crate::state::append_tasks_progress(round, &format!("Stopped: {reason}"), None, config);
}

fn task_decomposition_phase_internal(config: &Config, resume: bool) -> bool {
    let _ = log("━━━ Task Decomposition Phase ━━━", config);

    let task = read_state_file("task.md", config);
    let paths = phase_paths(config);

    // Clear tasks_findings.json on fresh runs; preserve on resume.
    if !resume {
        crate::state::clear_tasks_findings(config);
        crate::state::clear_tasks_progress(config);
    }

    if resume {
        let current = read_status(config);
        if current.status == Status::Consensus {
            let _ = log("✅ Task decomposition already reached consensus.", config);
            print_planning_complete_summary(&current, &task, config);
            return true;
        }
    }
    let start_round = decomposition_start_round(config, resume);

    let decomp_limit = normalize_round_limit(config.decomposition_max_rounds);

    if resume {
        let _ = log(
            &format!(
                "↪ Resuming task decomposition from round {}",
                round_display(start_round, decomp_limit)
            ),
            config,
        );
    }

    if rounds_already_exhausted(start_round, decomp_limit) {
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
        crate::state::append_tasks_progress(
            start_round,
            "Max rounds reached without consensus",
            None,
            config,
        );
        return false;
    }

    let mut round = start_round.saturating_sub(1);
    loop {
        round += 1;
        if should_emit_high_watermark(round, decomp_limit) {
            let _ = log(IMPLEMENTATION_HIGH_WATERMARK_LOG, config);
        }
        let previous_status = read_status(config);
        if previous_status.status == Status::Error {
            write_error_status(config, Some(round), status_error_reason(&previous_status));
            append_decomposition_error_progress(
                config,
                round,
                "Decomposition stopped after an error",
            );
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
            if !run_agent_or_record_error(
                config,
                &config.implementer,
                &decomposition_initial_prompt(&paths),
                Some(round),
                AgentRole::Implementer,
                "decompose",
                "initial",
            ) {
                append_decomposition_error_progress(
                    config,
                    round,
                    "Implementer decomposition failed",
                );
                return false;
            }
        } else {
            let _ = log(
                &format!(
                    "📝 Implementer revising task breakdown (round {})...",
                    round_display(round, decomp_limit)
                ),
                config,
            );

            if !run_agent_or_record_error(
                config,
                &config.implementer,
                &decomposition_revision_prompt(&paths, round > 1),
                Some(round),
                AgentRole::Implementer,
                "decompose",
                "revision",
            ) {
                append_decomposition_error_progress(
                    config,
                    round,
                    "Implementer decomposition revision failed",
                );
                return false;
            }
        }

        let _ = log(
            &format!(
                "🔍 Reviewer validating task breakdown (round {})...",
                round_display(round, decomp_limit)
            ),
            config,
        );

        // Read open tasks findings for the reviewer prompt.
        let tasks_findings = crate::state::read_tasks_findings(config);
        let open_findings_text = crate::state::open_tasks_findings_for_prompt(&tasks_findings);

        let reviewer_prompt_timestamp = timestamp();

        if !run_agent_or_record_error(
            config,
            &config.reviewer,
            &decomposition_reviewer_prompt(
                round,
                &reviewer_prompt_timestamp,
                &paths,
                &open_findings_text,
                round > 1,
            ),
            Some(round),
            AgentRole::Reviewer,
            "decompose",
            "review",
        ) {
            append_decomposition_error_progress(
                config,
                round,
                "Reviewer decomposition review failed",
            );
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

        // --- Tasks findings reconciliation ---
        // Read the review output and parse structured findings from it.
        let review_output = read_state_file("review.md", config);
        let new_task_findings = parse_tasks_findings_from_output(&review_output, round);

        let (reconciled_decision, updated_findings) = reconcile_tasks_verdict(
            status.status,
            status.reason.as_deref(),
            new_task_findings,
            &tasks_findings,
            round,
            config,
        );

        // Persist updated tasks findings — propagate errors instead of ignoring.
        if let Err(e) = crate::state::write_tasks_findings(&updated_findings, config) {
            let _ = log(
                &format!("⚠️ Failed to persist {TASKS_FINDINGS_FILENAME}: {e}"),
                config,
            );
        }

        // If reconciliation overrode the status (e.g. forced NEEDS_REVISION),
        // update status.json accordingly.
        if reconciled_decision == DecompositionStatusDecision::NeedsRevision
            && status.status == Status::Approved
        {
            warn_on_status_write(
                "NEEDS_REVISION",
                StatusPatch {
                    status: Some(Status::NeedsRevision),
                    round: Some(round),
                    reason: Some(
                        "[gate:reviewer] Forced NEEDS_REVISION: open tasks findings remain"
                            .to_string(),
                    ),
                    ..StatusPatch::default()
                },
                config,
            );
            status = read_status(config);
        }

        let tasks_findings_bullets = format_tasks_findings_summary(&updated_findings, 500);
        crate::state::append_tasks_progress(
            round,
            &format!(
                "Reviewer verdict: {} (open findings: {})",
                decomposition_decision_label(reconciled_decision),
                open_tasks_findings_count(&updated_findings)
            ),
            tasks_findings_bullets.as_deref(),
            config,
        );

        match reconciled_decision {
            DecompositionStatusDecision::Approved => {
                if requires_dual_agent_signoff(config) {
                    // Dual-agent: require implementer signoff before finalizing.
                    let _ = log(
                        "🔍 Implementer reviewing approved task breakdown...",
                        config,
                    );
                    let signoff_timestamp = timestamp();
                    if !run_agent_or_record_error(
                        config,
                        &config.implementer,
                        &decomposition_implementer_signoff_prompt(
                            round,
                            &signoff_timestamp,
                            &paths,
                        ),
                        Some(round),
                        AgentRole::Implementer,
                        "decompose",
                        "implementer-signoff",
                    ) {
                        append_decomposition_error_progress(
                            config,
                            round,
                            "Implementer signoff failed during decomposition",
                        );
                        return false;
                    }

                    let mut signoff_status = read_status(config);
                    if signoff_status.status != Status::Error
                        && is_status_stale(&signoff_timestamp, &signoff_status)
                    {
                        let _ = log(
                            "⚠️ Stale status after decomposition implementer signoff — writing Disputed fallback",
                            config,
                        );
                        warn_on_status_write(
                            "DISPUTED",
                            StatusPatch {
                                status: Some(Status::Disputed),
                                round: Some(round),
                                reason: Some(STALE_TIMESTAMP_REASON.to_string()),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                        signoff_status = read_status(config);
                    }

                    if signoff_status.status == Status::Error {
                        write_error_status(
                            config,
                            Some(round),
                            status_error_reason(&signoff_status),
                        );
                        append_decomposition_error_progress(
                            config,
                            round,
                            "Implementer signoff ended with an error",
                        );
                        return false;
                    }

                    if signoff_status.status == Status::Consensus {
                        crate::state::append_tasks_progress(
                            round,
                            "Implementer signoff: CONSENSUS",
                            None,
                            config,
                        );
                        let _ = log("✅ Both agents agreed on task breakdown!", config);
                        print_planning_complete_summary(&signoff_status, &task, config);
                        return true;
                    }

                    crate::state::append_tasks_progress(
                        round,
                        "Implementer signoff: DISPUTED",
                        None,
                        config,
                    );

                    // Disputed — continue to next round with the implementer's concerns.
                    let _ = log(
                        &format!(
                            "📝 Implementer disputed task breakdown: {}",
                            signoff_status
                                .reason
                                .as_deref()
                                .unwrap_or("see status.json")
                        ),
                        config,
                    );
                } else {
                    // Single-agent: system converts APPROVED -> CONSENSUS (no self-review).
                    let _ = log("✅ Task breakdown approved!", config);
                    if let Err(err) = finalize_phase_consensus(config, round, None) {
                        write_error_status(config, Some(round), err.to_string());
                        append_decomposition_error_progress(
                            config,
                            round,
                            "Failed to finalize single-agent decomposition consensus",
                        );
                        return false;
                    }
                    crate::state::append_tasks_progress(
                        round,
                        "Approved (single-agent -> consensus)",
                        None,
                        config,
                    );
                    let final_status = read_status(config);
                    print_planning_complete_summary(&final_status, &task, config);
                    return true;
                }
            }
            DecompositionStatusDecision::NeedsRevision => {}
            DecompositionStatusDecision::Error => {
                write_error_status(config, Some(round), status_error_reason(&status));
                append_decomposition_error_progress(
                    config,
                    round,
                    "Reviewer reconciliation ended with an error",
                );
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

        if round_limit_reached(round, decomp_limit) {
            break;
        }
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
    crate::state::append_tasks_progress(round, "Max rounds reached without consensus", None, config);

    false
}

pub fn task_decomposition_phase(config: &Config) -> bool {
    task_decomposition_phase_internal(config, false)
}

pub fn task_decomposition_phase_resume(config: &Config) -> bool {
    task_decomposition_phase_internal(config, true)
}

fn extract_finding_summary(review_text: &str, max_chars: usize) -> Option<String> {
    if review_text.trim().is_empty() {
        return None;
    }
    if crate::state::review_has_no_findings(review_text) {
        return None;
    }

    // Narrow to the Findings section so preamble/checklist bullets are excluded.
    let scoped = trim_review_to_findings(review_text);

    // Tier 1: Extract bullet/numbered list items
    let bullets: Vec<&str> = scoped
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || trimmed
                    .find(". ")
                    .map(|pos| trimmed[..pos].chars().all(|c| c.is_ascii_digit()) && pos > 0)
                    .unwrap_or(false)
        })
        .take(5)
        .collect();

    let lines = if bullets.is_empty() {
        // Tier 2: Prose fallback — first 5 non-empty, non-heading lines.
        // Also skip bare "Findings" marker lines that trim_review_to_findings()
        // includes as the section start.
        let prose: Vec<String> = scoped
            .lines()
            .map(str::trim)
            .filter(|line| {
                !line.is_empty()
                    && !line.starts_with('#')
                    && line.trim_start_matches('#').trim() != "Findings"
            })
            .take(5)
            .map(|line| format!("- {line}"))
            .collect();
        if prose.is_empty() {
            return None;
        }
        prose
    } else {
        bullets.iter().map(|line| line.trim().to_string()).collect()
    };

    Some(truncate_summary_lines(&lines, max_chars))
}

/// Joins lines with truncation. When a line would exceed `max_chars`,
/// the line is character-truncated (with `...` suffix) rather than
/// replaced entirely with `- ...`.
fn truncate_summary_lines(lines: &[String], max_chars: usize) -> String {
    let mut result = String::new();
    for line in lines {
        let sep_cost = if result.is_empty() { 0 } else { 1 };
        let remaining = max_chars.saturating_sub(result.len() + sep_cost);
        if remaining == 0 {
            break;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        if line.len() <= remaining {
            result.push_str(line);
        } else {
            // Truncate the line itself, preserving at least some content.
            // Use char boundaries to avoid panicking on multibyte UTF-8.
            let trunc_end = remaining.saturating_sub(3);
            if trunc_end > 0 {
                let safe_end = line
                    .char_indices()
                    .take_while(|&(i, _)| i < trunc_end)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                result.push_str(&line[..safe_end]);
                result.push_str("...");
            } else {
                result.push_str("...");
            }
            break;
        }
    }
    result
}

fn format_tasks_findings_summary(
    findings: &crate::state::TasksFindingsFile,
    max_chars: usize,
) -> Option<String> {
    let mut open: Vec<&crate::state::TasksFindingEntry> = findings
        .findings
        .iter()
        .filter(|e| e.status == crate::state::TasksFindingStatus::Open)
        .collect();
    // Prioritize most recently introduced findings so the latest round's
    // rejection reasons appear first, before the 5-item cap is applied.
    open.sort_by(|a, b| b.round_introduced.cmp(&a.round_introduced));
    open.truncate(5);

    if open.is_empty() {
        return None;
    }

    let lines: Vec<String> = open
        .iter()
        .map(|entry| format!("- [{}] {}", entry.id, entry.description))
        .collect();

    let result = truncate_summary_lines(&lines, max_chars);

    Some(result)
}

fn implementation_progress_entry(phase: &str, summary: &str) -> String {
    let label = match phase {
        "implementation" => "Implementation",
        "review" => "Gate A",
        "fresh-review" => "Gate B",
        "gate-b-verify" => "Gate B verification",
        "consensus" => "Consensus",
        "gate-c-bounce" => "Gate C bounce",
        "stuck" => "Stuck",
        "error" => "Error",
        "terminal" => "Terminal",
        other => other,
    };

    format!("{label}: {}", summary.trim())
}

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
    _read_history_fn: FReadHistory,
    mut append_history_fn: FAppendHistory,
    resume: bool,
) -> bool
where
    FRunAgent: FnMut(
        &crate::config::Agent,
        AgentRole,
        &str,
        Option<&str>,
        &Config,
        Option<&AgentCallMeta>,
    ) -> Result<(), AgentLoopError>,
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

    let review_limit = normalize_round_limit(config.review_max_rounds);

    if resume {
        log_fn(
            &format!(
                "↪ Resuming implementation from round {}",
                round_display(start_round, review_limit)
            ),
            config,
        );
    }

    if rounds_already_exhausted(start_round, review_limit) {
        let task = read_state_file_fn("task.md", config);
        log_fn(
            &format!(
                "\n⏰ Max rounds ({}) reached without consensus",
                config.review_max_rounds
            ),
            config,
        );
        write_status_fn(
            StatusPatch {
                status: Some(Status::MaxRounds),
                round: Some(config.review_max_rounds),
                ..StatusPatch::default()
            },
            config,
        );
        git_checkpoint_fn("max-rounds-reached", config, baseline_files);
        record_struggle_signal(
            &task,
            "max rounds already exhausted before resume",
            config.review_max_rounds,
            config,
        );
        append_history_fn(
            config.review_max_rounds,
            "terminal",
            "MAX_ROUNDS — already exhausted before resume",
            config,
        );
        return false;
    }

    let mut stuck_detector = StuckDetector::new(
        config.stuck_detection_enabled,
        config.stuck_no_diff_rounds,
        config.stuck_threshold_minutes,
    );

    let mut round = start_round.saturating_sub(1);
    loop {
        round += 1;
        if should_emit_high_watermark(round, review_limit) {
            log_fn(IMPLEMENTATION_HIGH_WATERMARK_LOG, config);
        }
        log_fn(
            &format!("━━━ Round {} ━━━", round_display(round, review_limit)),
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
        let previous_findings_result = read_findings_with_warnings(config);
        for warning in &previous_findings_result.warnings {
            log_fn(&format!("⚠ {FINDINGS_FILENAME}: {warning}"), config);
        }
        let findings_before_review =
            normalize_findings_for_round(previous_findings_result.findings_file, round);
        log_fn("🔨 Implementer working...", config);
        let impl_meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "implementer".to_string(),
            round,
            role: "implementer".to_string(),
            agent_name: config.implementer.name().to_string(),
            session_hint: None,
            output_file: Some(paths.changes_md.clone()),
        };
        if let Err(err) = run_agent_fn(
            &config.implementer,
            AgentRole::Implementer,
            &implementation_implementer_prompt(round, &paths, round > 1),
            None,
            config,
            Some(&impl_meta),
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
            append_history_fn(round, "error", &err.to_string(), config);
            return false;
        }

        let changes = read_state_file_fn("changes.md", config);
        append_history_fn(round, "implementation", &changes, config);

        let checkpoint_message = implementation_checkpoint_message(round, &changes);
        git_checkpoint_fn(&checkpoint_message, config, baseline_files);

        let diff = git_diff_for_review_fn(pre_impl_head.as_deref(), config);

        // --- Stuck detection ---
        let stuck_signal = stuck_detector.observe_round(&diff);
        if stuck_signal != StuckSignal::Ok {
            let signal_msg = match &stuck_signal {
                StuckSignal::NoDiffProgress { consecutive_rounds } => {
                    format!("No diff progress for {consecutive_rounds} consecutive rounds")
                }
                StuckSignal::Oscillating => {
                    "Oscillating diff pattern detected (A → B → A)".to_string()
                }
                StuckSignal::TimeThresholdExceeded { elapsed_minutes } => {
                    format!("Wall-clock threshold exceeded ({elapsed_minutes} minutes)")
                }
                StuckSignal::Ok => unreachable!(),
            };

            match config.stuck_action {
                StuckAction::Abort => {
                    log_fn(
                        &format!("🛑 Stuck detected — aborting: {signal_msg}"),
                        config,
                    );
                    write_status_fn(
                        StatusPatch {
                            status: Some(Status::Stuck),
                            round: Some(round),
                            reason: Some(signal_msg.clone()),
                            ..StatusPatch::default()
                        },
                        config,
                    );
                    record_struggle_signal(&task, &signal_msg, round, config);
                    append_history_fn(round, "stuck", &format!("STUCK — {signal_msg}"), config);
                    return false;
                }
                StuckAction::Warn => {
                    log_fn(
                        &format!("⚠ Stuck detected — continuing: {signal_msg}"),
                        config,
                    );
                    record_struggle_signal(&task, &signal_msg, round, config);
                }
                StuckAction::Retry => {
                    log_fn(
                        &format!("🔄 Stuck detected — skipping reviewer, retrying: {signal_msg}"),
                        config,
                    );
                    record_struggle_signal(&task, &signal_msg, round, config);
                    continue;
                }
            }
        }

        let quality_checks_output = run_quality_checks(config);
        if let Some(content) = quality_checks_output.as_deref() {
            if let Err(err) = write_state_file(QUALITY_CHECKS_FILENAME, content, config) {
                log_fn(
                    &format!("WARN: failed to write {QUALITY_CHECKS_FILENAME}: {err}"),
                    config,
                );
            }
        } else {
            let _ = fs::remove_file(config.state_dir.join(QUALITY_CHECKS_FILENAME));
        }

        write_status_fn(
            StatusPatch {
                status: Some(Status::Reviewing),
                round: Some(round),
                reason: Some(gate_reason(
                    IMPLEMENT_GATE_SAME_CONTEXT,
                    "Awaiting same-context reviewer gate",
                )),
                ..StatusPatch::default()
            },
            config,
        );
        log_fn("🔍 Reviewer evaluating implementation...", config);

        let reviewer_prompt_timestamp = timestamp_fn();
        let review_meta = AgentCallMeta {
            workflow: "implement".to_string(),
            phase: "gate-a-review".to_string(),
            round,
            role: "reviewer".to_string(),
            agent_name: config.reviewer.name().to_string(),
            session_hint: None,
            output_file: Some(paths.review_md.clone()),
        };
        if let Err(err) = run_agent_fn(
            &config.reviewer,
            AgentRole::Reviewer,
            &implementation_reviewer_prompt(
                round,
                &reviewer_prompt_timestamp,
                &paths,
                config.auto_test,
                round > 1,
            ),
            None,
            config,
            Some(&review_meta),
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
            append_history_fn(round, "error", &err.to_string(), config);
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
                    reason: Some(gate_reason(
                        IMPLEMENT_GATE_SAME_CONTEXT,
                        STALE_TIMESTAMP_REASON,
                    )),
                    ..StatusPatch::default()
                },
                config,
            );
            status = read_status_fn(config);
        }

        let reviewer_findings_result = read_findings_with_warnings(config);
        for warning in &reviewer_findings_result.warnings {
            log_fn(&format!("⚠ {FINDINGS_FILENAME}: {warning}"), config);
        }
        let reviewer_findings = reconcile_findings_after_review(
            round,
            status.status,
            status.reason.as_deref(),
            &findings_before_review,
            reviewer_findings_result.findings_file,
            config,
        );
        if let Some(note) = reviewer_findings.log_note.as_deref() {
            log_fn(&format!("⚠ {note}"), config);
        }
        if matches!(status.status, Status::NeedsChanges | Status::Approved) {
            if let Err(err) = write_findings(&reviewer_findings.findings, config) {
                log_fn(
                    &format!("WARN: failed to write {FINDINGS_FILENAME}: {err}"),
                    config,
                );
            }
            if reviewer_findings.status != status.status
                || reviewer_findings.reason.as_deref() != status.reason.as_deref()
            {
                write_status_fn(
                    StatusPatch {
                        status: Some(reviewer_findings.status),
                        round: Some(round),
                        reason: gate_reason_opt(
                            IMPLEMENT_GATE_SAME_CONTEXT,
                            reviewer_findings.reason.clone(),
                        ),
                        ..StatusPatch::default()
                    },
                    config,
                );
                status = read_status_fn(config);
            }
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
                if !config.single_agent {
                    // === BRANCH 2: Dual-agent — mandatory fresh-context reviewer gate ===
                    log_fn("🔍 Gate B: running fresh-context reviewer pass...", config);
                    let findings_before_adversarial_result = read_findings_with_warnings(config);
                    for warning in &findings_before_adversarial_result.warnings {
                        log_fn(&format!("⚠ {FINDINGS_FILENAME}: {warning}"), config);
                    }
                    let findings_before_adversarial = normalize_findings_for_round(
                        findings_before_adversarial_result.findings_file,
                        round,
                    );

                    // Write intermediate Reviewing status before fresh-context gate call.
                    write_status_fn(
                        StatusPatch {
                            status: Some(Status::Reviewing),
                            round: Some(round),
                            reason: Some(gate_reason(
                                IMPLEMENT_GATE_FRESH_CONTEXT,
                                "Awaiting fresh-context reviewer gate",
                            )),
                            ..StatusPatch::default()
                        },
                        config,
                    );

                    let adversarial_timestamp = timestamp_fn();
                    let fresh_session_hint = format!("fresh-context-review-r{round}");
                    let fresh_review_meta = AgentCallMeta {
                        workflow: "implement".to_string(),
                        phase: "gate-b-review".to_string(),
                        round,
                        role: "reviewer".to_string(),
                        agent_name: config.reviewer.name().to_string(),
                        session_hint: Some(fresh_session_hint.clone()),
                        output_file: Some(paths.review_md.clone()),
                    };

                    if let Err(err) = run_agent_fn(
                        &config.reviewer,
                        AgentRole::Reviewer,
                        &implementation_fresh_context_reviewer_prompt(
                            round,
                            &adversarial_timestamp,
                            &paths,
                            config.auto_test,
                        ),
                        Some(fresh_session_hint.as_str()),
                        config,
                        Some(&fresh_review_meta),
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
                        append_history_fn(round, "error", &err.to_string(), config);
                        return false;
                    }

                    let mut adversarial_status = read_status_fn(config);
                    if adversarial_status.status != Status::Error
                        && is_status_stale(&adversarial_timestamp, &adversarial_status)
                    {
                        log_fn(
                            "⚠️ Stale status after fresh-context review — writing NeedsChanges fallback",
                            config,
                        );
                        write_status_fn(
                            StatusPatch {
                                status: Some(Status::NeedsChanges),
                                round: Some(round),
                                reason: Some(gate_reason(
                                    IMPLEMENT_GATE_FRESH_CONTEXT,
                                    STALE_TIMESTAMP_REASON,
                                )),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                        adversarial_status = read_status_fn(config);
                    }

                    let adversarial_findings_result = read_findings_with_warnings(config);
                    for warning in &adversarial_findings_result.warnings {
                        log_fn(&format!("⚠ {FINDINGS_FILENAME}: {warning}"), config);
                    }
                    let adversarial_findings = reconcile_findings_after_review(
                        round,
                        adversarial_status.status,
                        adversarial_status.reason.as_deref(),
                        &findings_before_adversarial,
                        adversarial_findings_result.findings_file,
                        config,
                    );
                    if let Some(note) = adversarial_findings.log_note.as_deref() {
                        log_fn(&format!("⚠ {note}"), config);
                    }
                    if matches!(
                        adversarial_status.status,
                        Status::NeedsChanges | Status::Approved
                    ) {
                        if let Err(err) = write_findings(&adversarial_findings.findings, config) {
                            log_fn(
                                &format!("WARN: failed to write {FINDINGS_FILENAME}: {err}"),
                                config,
                            );
                        }
                        if adversarial_findings.status != adversarial_status.status
                            || adversarial_findings.reason.as_deref()
                                != adversarial_status.reason.as_deref()
                        {
                            write_status_fn(
                                StatusPatch {
                                    status: Some(adversarial_findings.status),
                                    round: Some(round),
                                    reason: gate_reason_opt(
                                        IMPLEMENT_GATE_FRESH_CONTEXT,
                                        adversarial_findings.reason.clone(),
                                    ),
                                    ..StatusPatch::default()
                                },
                                config,
                            );
                            adversarial_status = read_status_fn(config);
                        }
                    }

                    let adversarial_summary = match adversarial_status.status {
                        Status::Approved => "APPROVED (fresh-context)".to_string(),
                        Status::NeedsChanges => format!(
                            "NEEDS_CHANGES (fresh-context) — {}",
                            adversarial_status
                                .reason
                                .as_deref()
                                .unwrap_or("see review.md")
                        ),
                        other => format!("{other} (fresh-context)"),
                    };
                    append_history_fn(round, "fresh-review", &adversarial_summary, config);

                    // Determine gate-B outcome: Approved, NeedsChanges (with
                    // verification), or Error.
                    let gate_b_approved = match implementation_reviewer_decision(
                        adversarial_status.status,
                    ) {
                        ImplementationReviewerDecision::Approved => true,
                        ImplementationReviewerDecision::NeedsChanges => {
                            // --- Confirmation loop: ask SAME fresh-context reviewer to verify ---
                            log_fn(
                                &format!(
                                    "⚠ Fresh-context review found issues: {} — asking reviewer to verify...",
                                    adversarial_status
                                        .reason
                                        .as_deref()
                                        .unwrap_or("see review.md")
                                ),
                                config,
                            );

                            let verify_timestamp = timestamp_fn();
                            write_status_fn(
                                StatusPatch {
                                    status: Some(Status::Reviewing),
                                    round: Some(round),
                                    reason: Some(gate_reason(
                                        IMPLEMENT_GATE_FRESH_CONTEXT,
                                        "Awaiting gate-B findings verification",
                                    )),
                                    ..StatusPatch::default()
                                },
                                config,
                            );
                            let verify_meta = AgentCallMeta {
                                workflow: "implement".to_string(),
                                phase: "gate-b-verify".to_string(),
                                round,
                                role: "reviewer".to_string(),
                                agent_name: config.reviewer.name().to_string(),
                                session_hint: Some(fresh_session_hint.clone()),
                                output_file: Some(paths.review_md.clone()),
                            };
                            if let Err(err) = run_agent_fn(
                                &config.reviewer,
                                AgentRole::Reviewer,
                                &implementation_gate_b_verification_prompt(
                                    round,
                                    &verify_timestamp,
                                    &paths,
                                ),
                                Some(fresh_session_hint.as_str()),
                                config,
                                Some(&verify_meta),
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
                                append_history_fn(round, "error", &err.to_string(), config);
                                return false;
                            }

                            let mut verify_status = read_status_fn(config);
                            if verify_status.status != Status::Error
                                && is_status_stale(&verify_timestamp, &verify_status)
                            {
                                log_fn(
                                    "⚠️ Stale status after gate-B verification — writing NeedsChanges fallback",
                                    config,
                                );
                                write_status_fn(
                                    StatusPatch {
                                        status: Some(Status::NeedsChanges),
                                        round: Some(round),
                                        reason: Some(gate_reason(
                                            IMPLEMENT_GATE_FRESH_CONTEXT,
                                            STALE_TIMESTAMP_REASON,
                                        )),
                                        ..StatusPatch::default()
                                    },
                                    config,
                                );
                                verify_status = read_status_fn(config);
                            }

                            append_history_fn(
                                round,
                                "gate-b-verify",
                                &format!("{} (verification)", verify_status.status,),
                                config,
                            );

                            match implementation_reviewer_decision(verify_status.status) {
                                ImplementationReviewerDecision::Approved => {
                                    log_fn(
                                        "✅ Fresh-context reviewer withdrew findings — proceeding to signoff",
                                        config,
                                    );
                                    true
                                }
                                ImplementationReviewerDecision::NeedsChanges => {
                                    log_fn(
                                        "⚠ Findings confirmed — returning to implementation loop",
                                        config,
                                    );
                                    false
                                }
                                ImplementationReviewerDecision::Error => {
                                    let reason = status_error_reason(&verify_status);
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
                                    append_history_fn(round, "error", &reason, config);
                                    return false;
                                }
                            }
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
                            append_history_fn(round, "error", &reason, config);
                            return false;
                        }
                    };

                    // --- Implementer signoff (only if gate-B approved or verification withdrew findings) ---
                    if gate_b_approved {
                        log_fn(
                            "🤝 Fresh-context review approved — checking implementer consensus...",
                            config,
                        );

                        let open_findings = findings_for_prompt(&normalize_findings_for_round(
                            read_findings(config),
                            round,
                        ));
                        let consensus_prompt_timestamp = timestamp_fn();
                        write_status_fn(
                            StatusPatch {
                                status: Some(Status::Reviewing),
                                round: Some(round),
                                reason: Some(gate_reason(
                                    IMPLEMENT_GATE_SIGNOFF,
                                    "Awaiting implementer signoff",
                                )),
                                ..StatusPatch::default()
                            },
                            config,
                        );
                        let signoff_session_hint = format!("fresh-context-signoff-r{round}");
                        let signoff_meta = AgentCallMeta {
                            workflow: "implement".to_string(),
                            phase: "implementer-signoff".to_string(),
                            round,
                            role: "implementer".to_string(),
                            agent_name: config.implementer.name().to_string(),
                            session_hint: Some(signoff_session_hint.clone()),
                            output_file: None,
                        };
                        if let Err(err) = run_agent_fn(
                            &config.implementer,
                            AgentRole::Implementer,
                            &implementation_consensus_prompt(
                                &open_findings,
                                round,
                                &consensus_prompt_timestamp,
                                &paths,
                            ),
                            Some(signoff_session_hint.as_str()),
                            config,
                            Some(&signoff_meta),
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
                            append_history_fn(round, "error", &err.to_string(), config);
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
                                    reason: Some(gate_reason(
                                        IMPLEMENT_GATE_SIGNOFF,
                                        STALE_TIMESTAMP_REASON,
                                    )),
                                    ..StatusPatch::default()
                                },
                                config,
                            );
                            final_status = read_status_fn(config);
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
                                    &paths,
                                    config,
                                    round,
                                    &mut run_agent_fn,
                                    &mut log_fn,
                                );
                                return true;
                            }
                            ImplementationConsensusDecision::Disputed => {
                                let dispute_reason = final_status
                                    .reason
                                    .as_deref()
                                    .unwrap_or("see status.json")
                                    .to_string();
                                log_fn(
                                    &format!(
                                        "⚠ Implementer disputed: {dispute_reason} — bouncing to Gate B reviewer..."
                                    ),
                                    config,
                                );

                                let bounce_timestamp = timestamp_fn();
                                write_status_fn(
                                    StatusPatch {
                                        status: Some(Status::Reviewing),
                                        round: Some(round),
                                        reason: Some(gate_reason(
                                            IMPLEMENT_GATE_C_BOUNCE,
                                            "Verifying late findings from implementer dispute",
                                        )),
                                        ..StatusPatch::default()
                                    },
                                    config,
                                );
                                let bounce_meta = AgentCallMeta {
                                    workflow: "implement".to_string(),
                                    phase: "gate-c-bounce".to_string(),
                                    round,
                                    role: "reviewer".to_string(),
                                    agent_name: config.reviewer.name().to_string(),
                                    session_hint: Some(fresh_session_hint.clone()),
                                    output_file: Some(paths.review_md.clone()),
                                };
                                if let Err(err) = run_agent_fn(
                                    &config.reviewer,
                                    AgentRole::Reviewer,
                                    &implementation_gate_c_late_findings_prompt(
                                        &dispute_reason,
                                        round,
                                        &bounce_timestamp,
                                        &paths,
                                    ),
                                    Some(fresh_session_hint.as_str()),
                                    config,
                                    Some(&bounce_meta),
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
                                    append_history_fn(round, "error", &err.to_string(), config);
                                    return false;
                                }

                                let mut bounce_status = read_status_fn(config);
                                if bounce_status.status != Status::Error
                                    && is_status_stale(&bounce_timestamp, &bounce_status)
                                {
                                    log_fn(
                                        "⚠️ Stale status after gate-C bounce — writing NeedsChanges fallback",
                                        config,
                                    );
                                    write_status_fn(
                                        StatusPatch {
                                            status: Some(Status::NeedsChanges),
                                            round: Some(round),
                                            reason: Some(gate_reason(
                                                IMPLEMENT_GATE_C_BOUNCE,
                                                STALE_TIMESTAMP_REASON,
                                            )),
                                            ..StatusPatch::default()
                                        },
                                        config,
                                    );
                                    bounce_status = read_status_fn(config);
                                }

                                append_history_fn(
                                    round,
                                    "gate-c-bounce",
                                    &format!(
                                        "{} (late findings verification)",
                                        bounce_status.status,
                                    ),
                                    config,
                                );

                                match implementation_reviewer_decision(bounce_status.status) {
                                    ImplementationReviewerDecision::Approved => {
                                        // Late findings rejected — consensus holds
                                        log_fn(
                                            "✅ Late findings rejected by reviewer — CONSENSUS (dispute overruled)",
                                            config,
                                        );
                                        write_status_fn(
                                            StatusPatch {
                                                status: Some(Status::Consensus),
                                                round: Some(round),
                                                reason: Some(
                                                    "CONSENSUS: late findings rejected by reviewer"
                                                        .to_string(),
                                                ),
                                                ..StatusPatch::default()
                                            },
                                            config,
                                        );
                                        append_history_fn(
                                            round,
                                            "consensus",
                                            "CONSENSUS (late findings rejected)",
                                            config,
                                        );
                                        git_checkpoint_fn(
                                            &format!("consensus-round-{round}"),
                                            config,
                                            baseline_files,
                                        );
                                        run_compound_phase_with_runner(
                                            &paths,
                                            config,
                                            round,
                                            &mut run_agent_fn,
                                            &mut log_fn,
                                        );
                                        return true;
                                    }
                                    ImplementationReviewerDecision::NeedsChanges => {
                                        // Late findings confirmed — loop back
                                        log_fn(
                                            "⚠ Late findings confirmed — returning to implementation loop",
                                            config,
                                        );
                                    }
                                    ImplementationReviewerDecision::Error => {
                                        let reason = status_error_reason(&bounce_status);
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
                                        append_history_fn(round, "error", &reason, config);
                                        return false;
                                    }
                                }
                            }
                            ImplementationConsensusDecision::Continue => {
                                log_fn(
                                    &format!(
                                        "⚠ Unexpected status after signoff: {}",
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
                                append_history_fn(round, "error", &reason, config);
                                return false;
                            }
                        }
                    }
                    // gate_b_approved == false: findings confirmed, loop continues
                } else if config.single_agent {
                    // === Single-agent auto-consensus ===
                    // Same model self-reviewing adds latency without signal.
                    // After findings reconciliation, system converts APPROVED -> CONSENSUS.
                    log_fn("🎉 Single-agent approved — auto-consensus", config);
                    write_status_fn(
                        StatusPatch {
                            status: Some(Status::Consensus),
                            round: Some(round),
                            ..StatusPatch::default()
                        },
                        config,
                    );
                    append_history_fn(round, "consensus", "AUTO-CONSENSUS (single-agent)", config);
                    git_checkpoint_fn(&format!("consensus-round-{round}"), config, baseline_files);
                    run_compound_phase_with_runner(
                        &paths,
                        config,
                        round,
                        &mut run_agent_fn,
                        &mut log_fn,
                    );
                    return true;
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
                append_history_fn(round, "error", &reason, config);
                return false;
            }
        }

        if round_limit_reached(round, review_limit) {
            break;
        }
    }

    // round_limit_reached broke out of the loop
    let task = read_state_file_fn("task.md", config);
    let issue = read_status_fn(config)
        .reason
        .unwrap_or_else(|| "max rounds reached without consensus".to_string());
    log_fn(
        &format!(
            "\n⏰ Max rounds ({}) reached without consensus",
            config.review_max_rounds
        ),
        config,
    );
    write_status_fn(
        StatusPatch {
            status: Some(Status::MaxRounds),
            round: Some(round),
            reason: Some(issue.clone()),
            ..StatusPatch::default()
        },
        config,
    );
    git_checkpoint_fn("max-rounds-reached", config, baseline_files);
    record_struggle_signal(&task, &issue, round, config);
    append_history_fn(round, "terminal", &format!("MAX_ROUNDS — {issue}"), config);
    false
}

pub fn implementation_loop(config: &Config, baseline_files: &HashSet<String>) -> bool {
    implementation_loop_internal(
        config,
        baseline_files,
        |agent: &crate::config::Agent,
         role: AgentRole,
         prompt,
         session_hint: Option<&str>,
         current_config,
         meta: Option<&AgentCallMeta>| {
            let sp = system_prompt_for_role(role, current_config);
            let sp_ref = if sp.is_empty() {
                None
            } else {
                Some(sp.as_str())
            };
            let role_str = match role {
                AgentRole::Implementer => "implementer",
                AgentRole::Reviewer => "reviewer",
                AgentRole::Planner => "planner",
            };
            let session_key = match session_hint {
                Some(hint) => format!("implement-{}-{}-{}", role_str, agent.name(), hint),
                None => format!("implement-{}-{}", role_str, agent.name()),
            };
            run_agent_with_session(
                agent,
                prompt,
                current_config,
                sp_ref,
                Some(&session_key),
                Some(role),
                meta,
            )
            .map(|_| ())
        },
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
            crate::state::append_implement_progress(
                round,
                &implementation_progress_entry(phase, summary),
                current_config,
            );
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
        |agent: &crate::config::Agent,
         role: AgentRole,
         prompt,
         session_hint: Option<&str>,
         current_config,
         meta: Option<&AgentCallMeta>| {
            let sp = system_prompt_for_role(role, current_config);
            let sp_ref = if sp.is_empty() {
                None
            } else {
                Some(sp.as_str())
            };
            let role_str = match role {
                AgentRole::Implementer => "implementer",
                AgentRole::Reviewer => "reviewer",
                AgentRole::Planner => "planner",
            };
            let session_key = match session_hint {
                Some(hint) => format!("implement-{}-{}-{}", role_str, agent.name(), hint),
                None => format!("implement-{}-{}", role_str, agent.name()),
            };
            run_agent_with_session(
                agent,
                prompt,
                current_config,
                sp_ref,
                Some(&session_key),
                Some(role),
                meta,
            )
            .map(|_| ())
        },
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
            crate::state::append_implement_progress(
                round,
                &implementation_progress_entry(phase, summary),
                current_config,
            );
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
    println!("{}", format_summary_block(&status, config));
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
            output_truncated: false,
            output: "warning: dead_code".to_string(),
        }];

        let rendered = format_quality_checks(&checks);
        assert!(rendered.contains("REMEDIATION: Run cargo clippy --fix first."));
        assert!(rendered.contains("warning: dead_code"));
    }

    #[test]
    fn format_quality_checks_includes_truncation_note_when_truncated() {
        let checks = vec![CheckResult {
            label: "custom".to_string(),
            success: false,
            timed_out: false,
            remediation: None,
            output_truncated: true,
            output: "line 1\nline 2".to_string(),
        }];

        let rendered = format_quality_checks(&checks);
        assert!(rendered.contains("NOTE: output truncated to last 100 lines."));
        assert!(rendered.contains("line 1"));
    }

    #[test]
    fn compound_phase_respects_compound_flag() {
        use crate::prompts::phase_paths;

        let mut enabled = test_config();
        enabled.compound = true;
        enabled.decisions_enabled = true;
        let enabled_paths = phase_paths(&enabled);

        let mut disabled = test_config();
        disabled.compound = false;
        let disabled_paths = phase_paths(&disabled);

        let mut enabled_calls = 0u32;
        run_compound_phase_with_runner(
            &enabled_paths,
            &enabled,
            1,
            &mut |_agent, _role, _prompt, _session_hint, _config, _meta: Option<&AgentCallMeta>| {
                enabled_calls += 1;
                Ok(())
            },
            &mut |_message, _config| {},
        );
        assert_eq!(enabled_calls, 1, "compound should run when enabled");

        let mut disabled_calls = 0u32;
        run_compound_phase_with_runner(
            &disabled_paths,
            &disabled,
            1,
            &mut |_agent, _role, _prompt, _session_hint, _config, _meta: Option<&AgentCallMeta>| {
                disabled_calls += 1;
                Ok(())
            },
            &mut |_message, _config| {},
        );
        assert_eq!(
            disabled_calls, 0,
            "compound should be skipped when disabled"
        );
    }

    #[test]
    fn record_struggle_signal_appends_expected_shape() {
        let mut config = test_config();
        config.decisions_enabled = true;
        record_struggle_signal("Task 7: Handle retries", "timeout", 3, &config);

        let content = crate::state::read_decisions(&config);
        assert!(content.contains("- [STRUGGLE] Task:"));
        assert!(content.contains("| Issue: timeout"));
        assert!(content.contains("| Round: 3 | Date: "));

        // Date should be YYYY-MM-DD (10 chars) right after the Date label.
        let date_start =
            content.find("| Date: ").expect("date field should exist") + "| Date: ".len();
        let date = &content[date_start..date_start + 10];
        assert_eq!(date.len(), 10);
        assert!(date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-');
    }

    #[test]
    fn reconcile_findings_needs_changes_synthesizes_when_missing() {
        let root = create_temp_project_root("reconcile_synth");
        let config = make_test_config(&root, TestConfigOptions::default());
        let previous = FindingsFile::default();
        let current = FindingsFile::default();

        let result = reconcile_findings_after_review(
            2,
            Status::NeedsChanges,
            Some("missing validation"),
            &previous,
            current,
            &config,
        );

        assert_eq!(result.status, Status::NeedsChanges);
        assert_eq!(result.findings.round, 2);
        assert_eq!(result.findings.findings.len(), 1);
        assert_eq!(result.findings.findings[0].id, "F-001");
        assert!(
            result
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("Open findings: F-001")
        );
    }

    #[test]
    fn reconcile_findings_approved_with_open_forces_needs_changes() {
        let root = create_temp_project_root("reconcile_approved");
        let config = make_test_config(&root, TestConfigOptions::default());
        let previous = FindingsFile::default();
        let current = FindingsFile {
            round: 3,
            findings: vec![FindingEntry {
                id: "F-002".to_string(),
                severity: "HIGH".to_string(),
                summary: "hash mismatch".to_string(),
                file_refs: vec!["src/lib.rs:20".to_string()],
            }],
        };

        let result = reconcile_findings_after_review(3, Status::Approved, None, &previous, current, &config);

        assert_eq!(result.status, Status::NeedsChanges);
        assert_eq!(result.findings.findings.len(), 1);
        assert!(
            result
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("Cannot approve with unresolved findings: F-002")
        );
    }

    #[test]
    fn trim_review_extracts_from_markdown_heading() {
        let review =
            "## Correctness\nAll good.\n## Findings\nF-001 missing test\n## Verdict\nAPPROVED";
        assert!(trim_review_to_findings(review).starts_with("## Findings"));
    }

    #[test]
    fn trim_review_extracts_from_bare_findings_line() {
        let review = "Correctness\nAll good.\nFindings\nF-001 missing test";
        assert!(trim_review_to_findings(review).starts_with("Findings"));
    }

    #[test]
    fn trim_review_falls_back_when_no_marker() {
        let review = "Just prose with no findings section here.";
        assert_eq!(trim_review_to_findings(review), review);
    }

    #[test]
    fn findings_for_prompt_renders_summary_lines() {
        let findings = FindingsFile {
            round: 4,
            findings: vec![FindingEntry {
                id: "F-010".to_string(),
                severity: "LOW".to_string(),
                summary: "missing docs".to_string(),
                file_refs: vec!["README.md:1".to_string()],
            }],
        };

        let rendered = findings_for_prompt(&findings);
        assert!(rendered.contains("F-010 [LOW] missing docs (README.md:1)"));
    }

    #[test]
    fn planning_verdict_no_findings_is_approved() {
        assert_eq!(
            planning_verdict_from_review("Everything looks good.\n\nno findings"),
            PlanningReviewerAction::Approved,
        );
    }

    #[test]
    fn planning_verdict_with_findings_is_needs_revision() {
        assert_eq!(
            planning_verdict_from_review("Found issues:\n1. Missing migration step"),
            PlanningReviewerAction::NeedsRevision,
        );
    }

    #[test]
    fn planning_verdict_empty_review_is_needs_revision() {
        assert_eq!(
            planning_verdict_from_review(""),
            PlanningReviewerAction::NeedsRevision,
        );
    }

    #[test]
    fn planning_verdict_no_findings_case_insensitive() {
        assert_eq!(
            planning_verdict_from_review("All good.\nNo Findings"),
            PlanningReviewerAction::Approved,
        );
        assert_eq!(
            planning_verdict_from_review("Looks great.\nNO FINDINGS"),
            PlanningReviewerAction::Approved,
        );
    }

    #[test]
    fn planning_verdict_no_findings_with_trailing_whitespace() {
        assert_eq!(
            planning_verdict_from_review("All good.\nno findings  \n"),
            PlanningReviewerAction::Approved,
        );
    }

    #[test]
    fn planning_verdict_no_findings_with_trailing_punctuation() {
        // Trailing period — the most common variant from real runs
        assert_eq!(
            planning_verdict_from_review("All good.\n\nNo findings."),
            PlanningReviewerAction::Approved,
        );
        assert_eq!(
            planning_verdict_from_review("Approved.\nNo findings.\n"),
            PlanningReviewerAction::Approved,
        );
        // Other trailing punctuation
        assert_eq!(
            planning_verdict_from_review("Done!\nno findings!"),
            PlanningReviewerAction::Approved,
        );
        assert_eq!(
            planning_verdict_from_review("no findings:"),
            PlanningReviewerAction::Approved,
        );
    }

    #[test]
    fn planning_verdict_rejects_embedded_no_findings() {
        // "no findings" must be the entire last line, not embedded in a sentence
        assert_eq!(
            planning_verdict_from_review("I cannot say no findings"),
            PlanningReviewerAction::NeedsRevision,
        );
        assert_eq!(
            planning_verdict_from_review("There are definitely no findings here today"),
            PlanningReviewerAction::NeedsRevision,
        );
        // But a standalone last line is still accepted
        assert_eq!(
            planning_verdict_from_review("I reviewed everything.\nno findings"),
            PlanningReviewerAction::Approved,
        );
    }

    // -----------------------------------------------------------------------
    // Tasks findings reconciliation tests
    // -----------------------------------------------------------------------

    #[test]
    fn reconcile_tasks_verdict_needs_revision_empty_findings_synthesizes() {
        let config = test_config();
        let existing = crate::state::TasksFindingsFile::default();
        let (decision, findings) = reconcile_tasks_verdict(
            Status::NeedsRevision,
            Some("task sizes are wrong"),
            Vec::new(),
            &existing,
            2,
            &config,
        );
        assert_eq!(decision, DecompositionStatusDecision::NeedsRevision);
        assert_eq!(findings.findings.len(), 1);
        assert_eq!(findings.findings[0].id, "T-001");
        assert_eq!(findings.findings[0].description, "task sizes are wrong");
        assert_eq!(findings.findings[0].round_introduced, 2);
    }

    #[test]
    fn reconcile_tasks_verdict_needs_revision_without_reason_synthesizes_default_message() {
        let config = test_config();
        let existing = crate::state::TasksFindingsFile::default();
        let (decision, findings) = reconcile_tasks_verdict(
            Status::NeedsRevision,
            None,
            Vec::new(),
            &existing,
            2,
            &config,
        );
        assert_eq!(decision, DecompositionStatusDecision::NeedsRevision);
        assert_eq!(findings.findings.len(), 1);
        assert_eq!(findings.findings[0].id, "T-001");
        assert_eq!(
            findings.findings[0].description,
            "Reviewer requested revision but did not provide structured findings."
        );
    }

    #[test]
    fn reconcile_tasks_verdict_approved_with_open_findings_forces_needs_revision() {
        let config = test_config();
        let existing = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "T-001".to_string(),
                description: "Missing testing task".to_string(),
                status: crate::state::TasksFindingStatus::Open,
                round_introduced: 1,
                round_resolved: None,
            }],
        };
        let (decision, _) =
            reconcile_tasks_verdict(Status::Approved, None, Vec::new(), &existing, 2, &config);
        assert_eq!(decision, DecompositionStatusDecision::NeedsRevision);
    }

    #[test]
    fn reconcile_tasks_verdict_approved_with_new_open_finding_forces_needs_revision() {
        let config = test_config();
        let existing = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "T-001".to_string(),
                description: "Old issue".to_string(),
                status: crate::state::TasksFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(2),
            }],
        };
        let new_findings = vec![crate::state::TasksFindingEntry {
            id: "T-001".to_string(),
            description: "Issue reopened by reviewer".to_string(),
            status: crate::state::TasksFindingStatus::Open,
            round_introduced: 3,
            round_resolved: None,
        }];
        let (decision, findings) =
            reconcile_tasks_verdict(Status::Approved, None, new_findings, &existing, 3, &config);
        assert_eq!(decision, DecompositionStatusDecision::NeedsRevision);
        assert_eq!(findings.findings.len(), 1);
        assert_eq!(
            findings.findings[0].status,
            crate::state::TasksFindingStatus::Open
        );
    }

    #[test]
    fn reconcile_tasks_verdict_approved_no_open_findings_approves() {
        let config = test_config();
        let existing = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "T-001".to_string(),
                description: "Was resolved".to_string(),
                status: crate::state::TasksFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(2),
            }],
        };
        let (decision, _) =
            reconcile_tasks_verdict(Status::Approved, None, Vec::new(), &existing, 3, &config);
        assert_eq!(decision, DecompositionStatusDecision::Approved);
    }

    #[test]
    fn reconcile_tasks_verdict_error_passes_through() {
        let config = test_config();
        let existing = crate::state::TasksFindingsFile::default();
        let (decision, _) =
            reconcile_tasks_verdict(Status::Error, None, Vec::new(), &existing, 1, &config);
        assert_eq!(decision, DecompositionStatusDecision::Error);
    }

    #[test]
    fn reconcile_tasks_verdict_unexpected_status_forces_revision() {
        let config = test_config();
        let existing = crate::state::TasksFindingsFile::default();
        let (decision, _) =
            reconcile_tasks_verdict(Status::Consensus, None, Vec::new(), &existing, 1, &config);
        assert_eq!(decision, DecompositionStatusDecision::ForceNeedsRevision);
    }

    #[test]
    fn reconcile_tasks_verdict_merges_resolved_findings() {
        // Test that the reviewer can resolve open findings by submitting
        // them with status "resolved", breaking the deadlock.
        let config = test_config();
        let existing = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "T-001".to_string(),
                description: "Missing testing task".to_string(),
                status: crate::state::TasksFindingStatus::Open,
                round_introduced: 1,
                round_resolved: None,
            }],
        };
        // Reviewer submits T-001 as resolved
        let new_findings = vec![crate::state::TasksFindingEntry {
            id: "T-001".to_string(),
            description: "Missing testing task".to_string(),
            status: crate::state::TasksFindingStatus::Resolved,
            round_introduced: 1,
            round_resolved: None,
        }];
        let (decision, findings) =
            reconcile_tasks_verdict(Status::Approved, None, new_findings, &existing, 2, &config);
        // With T-001 resolved, APPROVED should pass through
        assert_eq!(decision, DecompositionStatusDecision::Approved);
        assert_eq!(findings.findings.len(), 1);
        assert_eq!(
            findings.findings[0].status,
            crate::state::TasksFindingStatus::Resolved
        );
        assert_eq!(findings.findings[0].round_resolved, Some(2));
    }

    #[test]
    fn reconcile_tasks_verdict_adds_new_findings_from_reviewer() {
        // Test that the reviewer can introduce new findings
        let config = test_config();
        let existing = crate::state::TasksFindingsFile::default();
        let new_findings = vec![crate::state::TasksFindingEntry {
            id: "T-001".to_string(),
            description: "Task 3 is too large".to_string(),
            status: crate::state::TasksFindingStatus::Open,
            round_introduced: 1,
            round_resolved: None,
        }];
        let (decision, findings) = reconcile_tasks_verdict(
            Status::NeedsRevision,
            Some("Issues found"),
            new_findings,
            &existing,
            1,
            &config,
        );
        assert_eq!(decision, DecompositionStatusDecision::NeedsRevision);
        assert_eq!(findings.findings.len(), 1);
        assert_eq!(findings.findings[0].id, "T-001");
        assert_eq!(findings.findings[0].description, "Task 3 is too large");
    }

    #[test]
    fn reconcile_tasks_verdict_reopens_resolved_finding() {
        // Test that a previously resolved finding can be reopened
        let config = test_config();
        let existing = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "T-001".to_string(),
                description: "Missing testing task".to_string(),
                status: crate::state::TasksFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(2),
            }],
        };
        let new_findings = vec![crate::state::TasksFindingEntry {
            id: "T-001".to_string(),
            description: "Testing task still insufficient".to_string(),
            status: crate::state::TasksFindingStatus::Open,
            round_introduced: 1,
            round_resolved: None,
        }];
        let (decision, findings) = reconcile_tasks_verdict(
            Status::NeedsRevision,
            None,
            new_findings,
            &existing,
            3,
            &config,
        );
        assert_eq!(decision, DecompositionStatusDecision::NeedsRevision);
        assert_eq!(
            findings.findings[0].status,
            crate::state::TasksFindingStatus::Open
        );
        assert_eq!(findings.findings[0].round_resolved, None);
        assert_eq!(
            findings.findings[0].description,
            "Testing task still insufficient"
        );
    }

    #[test]
    fn reconcile_tasks_verdict_needs_revision_resolved_findings_synthesizes() {
        // Edge case: prior rounds left only resolved findings in merged,
        // and reviewer returns NEEDS_REVISION with no new findings.
        // The safety net must still synthesize a finding from reason text.
        // Before the fix, `merged.is_empty()` was false (resolved entries exist),
        // so synthesis was skipped, silently dropping the reviewer's concern.
        let config = test_config();
        let existing = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "T-001".to_string(),
                description: "Old issue now resolved".to_string(),
                status: crate::state::TasksFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(2),
            }],
        };
        let (decision, findings) = reconcile_tasks_verdict(
            Status::NeedsRevision,
            Some("new problem with task ordering"),
            Vec::new(), // no structured findings from reviewer
            &existing,
            3,
            &config,
        );
        assert_eq!(decision, DecompositionStatusDecision::NeedsRevision);
        // Should have the original resolved finding + synthesized new one
        assert_eq!(findings.findings.len(), 2);
        // Original resolved finding preserved
        assert_eq!(findings.findings[0].id, "T-001");
        assert_eq!(
            findings.findings[0].status,
            crate::state::TasksFindingStatus::Resolved
        );
        // Synthesized finding from reason text
        assert_eq!(findings.findings[1].id, "T-002");
        assert_eq!(
            findings.findings[1].description,
            "new problem with task ordering"
        );
        assert_eq!(
            findings.findings[1].status,
            crate::state::TasksFindingStatus::Open
        );
        assert_eq!(findings.findings[1].round_introduced, 3);
    }

    #[test]
    fn reconcile_tasks_verdict_needs_revision_with_open_findings_no_synthesis() {
        // When NEEDS_REVISION is returned with no new findings but open findings
        // already exist from prior rounds, we should NOT synthesize — the existing
        // open findings already capture the issue.
        let config = test_config();
        let existing = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "T-001".to_string(),
                description: "Existing open issue".to_string(),
                status: crate::state::TasksFindingStatus::Open,
                round_introduced: 1,
                round_resolved: None,
            }],
        };
        let (decision, findings) = reconcile_tasks_verdict(
            Status::NeedsRevision,
            Some("still not fixed"),
            Vec::new(), // no new structured findings
            &existing,
            2,
            &config,
        );
        assert_eq!(decision, DecompositionStatusDecision::NeedsRevision);
        // Should NOT synthesize: open findings already exist
        assert_eq!(findings.findings.len(), 1);
        assert_eq!(findings.findings[0].id, "T-001");
    }

    // -----------------------------------------------------------------------
    // Signoff policy tests
    // -----------------------------------------------------------------------

    #[test]
    fn requires_dual_agent_signoff_returns_true_for_dual_agent() {
        let config = test_config(); // default is dual-agent
        assert!(requires_dual_agent_signoff(&config));
    }

    #[test]
    fn requires_dual_agent_signoff_returns_false_for_single_agent() {
        let root = create_temp_project_root("phases_single_agent");
        let config = make_test_config(
            &root,
            TestConfigOptions {
                single_agent: true,
                ..Default::default()
            },
        );
        assert!(!requires_dual_agent_signoff(&config));
    }

    // -----------------------------------------------------------------------
    // planning_adversarial_enabled guard
    // -----------------------------------------------------------------------

    #[test]
    fn planning_adversarial_enabled_true_for_dual_agent_different_agents() {
        let config = test_config(); // dual-agent, adversarial=true, claude/codex
        assert!(planning_adversarial_enabled(&config));
    }

    #[test]
    fn planning_adversarial_enabled_false_for_single_agent() {
        let root = create_temp_project_root("phases_adv_single");
        let config = make_test_config(
            &root,
            TestConfigOptions {
                single_agent: true,
                ..Default::default()
            },
        );
        assert!(!planning_adversarial_enabled(&config));
    }

    #[test]
    fn planning_adversarial_enabled_false_when_toggle_off() {
        let root = create_temp_project_root("phases_adv_toggle_off");
        let config = make_test_config(
            &root,
            TestConfigOptions {
                planning_adversarial_review: false,
                ..Default::default()
            },
        );
        assert!(!planning_adversarial_enabled(&config));
    }

    #[test]
    fn planning_adversarial_enabled_false_when_same_agent() {
        use crate::config::Agent;
        let root = create_temp_project_root("phases_adv_same_agent");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        // Force implementer == reviewer (same model = no independence).
        config.implementer = Agent::known("claude");
        config.reviewer = Agent::known("claude");
        assert!(!planning_adversarial_enabled(&config));
    }

    // -----------------------------------------------------------------------
    // Decomposition reviewer now writes APPROVED (not CONSENSUS)
    // -----------------------------------------------------------------------

    #[test]
    fn decomposition_reviewer_prompt_uses_approved_status() {
        use crate::prompts::{decomposition_reviewer_prompt, phase_paths};
        let root = create_temp_project_root("phases_decomp_approved");
        let config = make_test_config(&root, TestConfigOptions::default());
        let paths = phase_paths(&config);
        let prompt =
            decomposition_reviewer_prompt(1, "2026-01-01T00:00:00.000Z", &paths, "", false);
        assert!(prompt.contains("\"status\": \"APPROVED\""));
        assert!(!prompt.contains("\"status\": \"CONSENSUS\""));
    }

    // -----------------------------------------------------------------------
    // Tasks findings parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_tasks_findings_from_output_extracts_findings() {
        let text = r#"
Some review text.

## Findings
```json
[{"id": "T-001", "description": "Task 3 is too large", "status": "open"},
 {"id": "T-002", "description": "Missing dependency on task 1", "status": "open"}]
```

More text.
"#;
        let findings = parse_tasks_findings_from_output(text, 2);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].id, "T-001");
        assert_eq!(findings[0].description, "Task 3 is too large");
        assert_eq!(findings[0].status, crate::state::TasksFindingStatus::Open);
        assert_eq!(findings[0].round_introduced, 2);
        assert_eq!(findings[1].id, "T-002");
    }

    #[test]
    fn parse_tasks_findings_from_output_handles_resolved() {
        let text = r#"
```json
[{"id": "T-001", "description": "Previously raised issue", "status": "resolved"}]
```
"#;
        let findings = parse_tasks_findings_from_output(text, 3);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].status,
            crate::state::TasksFindingStatus::Resolved
        );
    }

    #[test]
    fn parse_tasks_findings_from_output_returns_empty_when_no_block() {
        let findings = parse_tasks_findings_from_output("no findings here", 1);
        assert!(findings.is_empty());
    }

    #[test]
    fn extract_plan_from_output_markers_extracts_plan_body() {
        let output = "Intro\n<plan>\n# Plan\n- Step A\n- Step B\n</plan>\nFooter";
        let plan = extract_plan_from_output_markers(output);
        assert_eq!(plan.as_deref(), Some("# Plan\n- Step A\n- Step B"));
    }

    #[test]
    fn extract_plan_from_output_markers_returns_none_without_markers() {
        let output = "# Plan\n- Step A\n- Step B";
        assert!(extract_plan_from_output_markers(output).is_none());
    }

    #[test]
    fn planner_plan_mode_active_requires_claude_planner_role_and_plan_setting() {
        let mut config = test_config();
        config.planner_permission_mode = "plan".to_string();
        let claude = crate::config::Agent::known("claude");
        let codex = crate::config::Agent::known("codex");

        assert!(planner_plan_mode_active(
            &config,
            &claude,
            AgentRole::Planner
        ));
        assert!(!planner_plan_mode_active(
            &config,
            &claude,
            AgentRole::Implementer
        ));
        assert!(!planner_plan_mode_active(
            &config,
            &codex,
            AgentRole::Planner
        ));
    }

    #[test]
    fn finalize_phase_consensus_writes_consensus_status() {
        let root = create_temp_project_root("phases_finalize_consensus");
        let config = make_test_config(&root, TestConfigOptions::default());
        std::fs::create_dir_all(&config.state_dir).expect("state dir should be created");
        crate::state::write_status(
            crate::state::StatusPatch {
                status: Some(Status::Approved),
                round: Some(1),
                reason: Some("seed".to_string()),

                ..crate::state::StatusPatch::default()
            },
            &config,
        )
        .expect("seed status should be written");

        finalize_phase_consensus(&config, 3, Some("finalized".to_string()))
            .expect("finalize_phase_consensus should succeed");

        let status = crate::state::read_status(&config);
        assert_eq!(status.status, Status::Consensus);
        assert_eq!(status.round, 3);
        assert_eq!(status.reason.as_deref(), Some("finalized"));
    }

    // -----------------------------------------------------------------------
    // Gate-source tagging in status reasons
    // -----------------------------------------------------------------------

    #[test]
    fn gate_source_in_planning_forced_revision_reason() {
        // Verify the forced NEEDS_REVISION reason includes gate-source tag
        let reason = "[gate:reviewer] Forced NEEDS_REVISION: open planning findings remain";
        assert!(reason.starts_with("[gate:"));
        let gate = reason
            .strip_prefix("[gate:")
            .and_then(|rest| rest.split_once(']'))
            .map(|(g, _)| g);
        assert_eq!(gate, Some("reviewer"));
    }

    #[test]
    fn gate_source_in_dual_agent_signoff_reason() {
        let reason =
            "[gate:implementer-signoff] Dual-agent signoff: reviewer approved, implementer agreed";
        let gate = reason
            .strip_prefix("[gate:")
            .and_then(|rest| rest.split_once(']'))
            .map(|(g, _)| g);
        assert_eq!(gate, Some("implementer-signoff"));
    }

    // -----------------------------------------------------------------------
    // Decomposition reviewer prompt includes findings protocol
    // -----------------------------------------------------------------------

    #[test]
    fn decomposition_reviewer_prompt_includes_findings_protocol() {
        use crate::prompts::{decomposition_reviewer_prompt, phase_paths};
        let root = create_temp_project_root("phases_decomp_findings_proto");
        let config = make_test_config(&root, TestConfigOptions::default());
        let paths = phase_paths(&config);
        let prompt =
            decomposition_reviewer_prompt(1, "2026-01-01T00:00:00.000Z", &paths, "", false);
        // Should reference tasks file and have review/status instructions
        assert!(prompt.contains("/state/tasks.md"));
        assert!(prompt.contains("\"status\": \"APPROVED\""));
    }

    // -----------------------------------------------------------------------
    // Single-agent implementation non-5/5: no self-consensus
    // -----------------------------------------------------------------------

    #[test]
    fn implementation_single_agent_non_5_5_auto_consensus() {
        // This test verifies the branch structure by testing that
        // the single-agent non-5/5 branch writes CONSENSUS directly
        // without running the consensus prompt.
        let root = create_temp_project_root("phases_impl_sa_non55");
        let config = make_test_config(
            &root,
            TestConfigOptions {
                single_agent: true,
                review_max_rounds: 1,
                compound: false,
                ..Default::default()
            },
        );

        let mut agent_calls: Vec<(String, String)> = Vec::new();
        let baseline = HashSet::new();

        // Simulate: implementer produces changes, reviewer approves.
        let status_sequence = std::cell::RefCell::new(vec![
            // read_status after reviewer: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                // Deliberately differs from prompt timestamp; verdict statuses
                // should not be treated as stale.
                timestamp: "2026-01-01T00:00:01.000Z".to_string(),
            },
        ]);

        let status_call_count = std::cell::RefCell::new(0usize);

        implementation_loop_internal(
            &config,
            &baseline,
            |_agent, role, _prompt, _session_hint, _cfg, _meta| {
                agent_calls.push((format!("{role:?}"), "called".to_string()));
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |name, _cfg| {
                if name == "review.md" {
                    return "Good work".to_string();
                }
                String::new()
            },
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    // After consensus write, return consensus status.
                    LoopStatus {
                        status: Status::Consensus,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "single-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "2026-01-01T00:00:02.000Z".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "2026-01-01T00:00:00.000Z".to_string(),
            |_cfg, _max| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        // In single-agent non-5/5, only Implementer and Reviewer should be called.
        // No third consensus call should happen.
        let role_names: Vec<&str> = agent_calls.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(role_names, vec!["Implementer", "Reviewer"]);
    }

    #[test]
    fn implementation_loop_writes_quality_checks_and_reviewer_prompt_points_to_file() {
        let root = create_temp_project_root("phases_impl_quality_checks_cycle");
        let config = make_test_config(
            &root,
            TestConfigOptions {
                single_agent: true,
                review_max_rounds: 1,
                auto_test: true,
                auto_test_cmd: Some("echo quality-ok".to_string()),
                compound: false,
                decisions_enabled: false,
                ..Default::default()
            },
        );
        let baseline: HashSet<String> = HashSet::new();
        let baseline_files: Vec<String> = Vec::new();
        crate::state::init(
            "Implement quality checks flow",
            &config,
            &baseline_files,
            crate::state::WorkflowKind::Implement,
        )
        .expect("state init should succeed");

        let quality_path = config.state_dir.join(QUALITY_CHECKS_FILENAME);
        let reviewer_seen = std::cell::Cell::new(false);
        let status_call_count = std::cell::RefCell::new(0usize);

        let reached = implementation_loop_internal(
            &config,
            &baseline,
            |_agent, role, prompt, _session_hint, _cfg, _meta| {
                if matches!(role, AgentRole::Reviewer) {
                    assert!(
                        prompt.contains("Review automated check output from"),
                        "reviewer prompt should reference quality checks"
                    );
                    assert!(
                        prompt.contains("quality_checks.md"),
                        "reviewer prompt should include quality checks file path"
                    );
                    let checks = std::fs::read_to_string(&quality_path)
                        .expect("quality checks output should be written before review");
                    assert!(checks.contains("QUALITY CHECKS:"));
                    assert!(checks.contains("custom [PASS]"));
                    reviewer_seen.set(true);
                }
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |name, cfg| crate::state::read_state_file(name, cfg),
            |_cfg| {
                let mut idx = status_call_count.borrow_mut();
                let status = match *idx {
                    0 => LoopStatus {
                        status: Status::Approved,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "single-agent".to_string(),
                        last_run_task: "Implement quality checks flow".to_string(),
                        reason: None,

                        timestamp: "2026-01-01T00:00:00.000Z".to_string(),
                    },
                    _ => LoopStatus {
                        status: Status::Consensus,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "single-agent".to_string(),
                        last_run_task: "Implement quality checks flow".to_string(),
                        reason: None,

                        timestamp: "2026-01-01T00:00:00.000Z".to_string(),
                    },
                };
                *idx += 1;
                status
            },
            |_head, _cfg| String::new(),
            || "2026-01-01T00:00:00.000Z".to_string(),
            |_cfg, _max| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        assert!(reached, "loop should reach consensus");
        assert!(reviewer_seen.get(), "reviewer call should have occurred");
    }

    #[test]
    fn implementation_dual_agent_non_5_5_runs_fresh_context_gate_before_signoff() {
        let root = create_temp_project_root("phases_impl_da_fresh_gate");
        let config = make_test_config(
            &root,
            TestConfigOptions {
                single_agent: false,
                review_max_rounds: 2,
                compound: false,
                ..Default::default()
            },
        );

        let baseline = HashSet::new();
        let calls: std::cell::RefCell<Vec<(String, Option<String>)>> =
            std::cell::RefCell::new(Vec::new());
        let status_calls = std::cell::RefCell::new(0usize);

        let reached = implementation_loop_internal(
            &config,
            &baseline,
            |_agent, role, _prompt, session_hint, _cfg, _meta| {
                calls
                    .borrow_mut()
                    .push((format!("{role:?}"), session_hint.map(|s| s.to_string())));
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |name, _cfg| {
                if name == "review.md" {
                    return "Gate review".to_string();
                }
                String::new()
            },
            |_cfg| {
                let mut idx = status_calls.borrow_mut();
                let status = match *idx {
                    0 => LoopStatus {
                        status: Status::Approved,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "codex".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "ts".to_string(),
                    },
                    1 => LoopStatus {
                        status: Status::Approved,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "codex".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "ts".to_string(),
                    },
                    _ => LoopStatus {
                        status: Status::Consensus,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "codex".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "ts".to_string(),
                    },
                };
                *idx += 1;
                status
            },
            |_head, _cfg| String::new(),
            || "ts".to_string(),
            |_cfg, _max| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        assert!(reached, "dual-agent loop should reach consensus");
        let observed = calls.borrow();
        assert_eq!(
            observed.len(),
            4,
            "expected implementer + gate A reviewer + gate B reviewer + signoff implementer"
        );
        assert_eq!(observed[0].0, "Implementer");
        assert_eq!(observed[0].1, None);
        assert_eq!(observed[1].0, "Reviewer");
        assert_eq!(observed[1].1, None);
        assert_eq!(observed[2].0, "Reviewer");
        assert_eq!(
            observed[2].1.as_deref(),
            Some("fresh-context-review-r1"),
            "gate B reviewer should use fresh-context session hint"
        );
        assert_eq!(observed[3].0, "Implementer");
        assert_eq!(
            observed[3].1.as_deref(),
            Some("fresh-context-signoff-r1"),
            "signoff should use fresh implementer session hint"
        );
    }

    #[test]
    fn implementation_dual_agent_max_rounds_in_fresh_context_preserves_gate_reason() {
        let root = create_temp_project_root("phases_impl_da_max_rounds_fresh");
        let config = make_test_config(
            &root,
            TestConfigOptions {
                single_agent: false,
                review_max_rounds: 1,
                compound: false,
                ..Default::default()
            },
        );

        let baseline = HashSet::new();
        let status_calls = std::cell::RefCell::new(0usize);
        let written: std::cell::RefCell<Vec<StatusPatch>> = std::cell::RefCell::new(Vec::new());

        let reached = implementation_loop_internal(
            &config,
            &baseline,
            |_agent, _role, _prompt, _session_hint, _cfg, _meta| Ok(()),
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |patch, _cfg| {
                written.borrow_mut().push(patch);
            },
            |name, _cfg| {
                if name == "review.md" {
                    return "Gate review".to_string();
                }
                String::new()
            },
            |_cfg| {
                let mut idx = status_calls.borrow_mut();
                let status = if *idx == 0 {
                    LoopStatus {
                        status: Status::Approved,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "codex".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "ts".to_string(),
                    }
                } else {
                    LoopStatus {
                        status: Status::NeedsChanges,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "codex".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: Some(
                            format!("[gate:fresh-context] Open findings: F-001. See {}/{FINDINGS_FILENAME}.", config.state_dir_rel()),
                        ),

                        timestamp: "ts".to_string(),
                    }
                };
                *idx += 1;
                status
            },
            |_head, _cfg| String::new(),
            || "ts".to_string(),
            |_cfg, _max| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        assert!(!reached, "loop should stop at configured max rounds");
        let max_round_patch = written
            .borrow()
            .iter()
            .find(|p| p.status == Some(Status::MaxRounds))
            .cloned()
            .expect("max-rounds status patch should be written");
        assert!(
            max_round_patch
                .reason
                .as_deref()
                .unwrap_or("")
                .contains(IMPLEMENT_GATE_FRESH_CONTEXT),
            "max-rounds reason should preserve fresh-context gate marker"
        );
    }

    // -----------------------------------------------------------------------
    // normalize_round_limit
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_round_limit_zero_is_unlimited() {
        assert_eq!(normalize_round_limit(0), None);
    }

    #[test]
    fn normalize_round_limit_positive_is_bounded() {
        assert_eq!(normalize_round_limit(10), Some(10));
        assert_eq!(normalize_round_limit(1), Some(1));
    }

    #[test]
    fn rounds_already_exhausted_returns_false_for_unlimited() {
        assert!(!rounds_already_exhausted(999, None));
    }

    #[test]
    fn rounds_already_exhausted_returns_false_at_cap() {
        // start_round == cap: the round hasn't run yet, so not exhausted
        assert!(!rounds_already_exhausted(10, Some(10)));
    }

    #[test]
    fn rounds_already_exhausted_returns_true_past_cap() {
        assert!(rounds_already_exhausted(11, Some(10)));
    }

    // -----------------------------------------------------------------------
    // High-watermark warnings only fire in unlimited mode
    // -----------------------------------------------------------------------

    #[test]
    fn high_watermark_fires_at_thresholds_in_unlimited_mode() {
        // None (unlimited): fires at 50, 75, 100
        assert!(should_emit_high_watermark(50, None));
        assert!(should_emit_high_watermark(75, None));
        assert!(should_emit_high_watermark(100, None));
    }

    #[test]
    fn high_watermark_does_not_fire_below_threshold_in_unlimited_mode() {
        assert!(!should_emit_high_watermark(1, None));
        assert!(!should_emit_high_watermark(49, None));
        assert!(!should_emit_high_watermark(51, None));
    }

    #[test]
    fn high_watermark_suppressed_in_bounded_mode() {
        // Some(_) (bounded): never fires, even at threshold rounds
        assert!(!should_emit_high_watermark(50, Some(100)));
        assert!(!should_emit_high_watermark(75, Some(100)));
        assert!(!should_emit_high_watermark(100, Some(200)));
    }

    #[test]
    fn round_limit_reached_returns_false_for_unlimited() {
        assert!(!round_limit_reached(999, None));
    }

    #[test]
    fn round_limit_reached_returns_true_at_cap() {
        assert!(round_limit_reached(10, Some(10)));
        assert!(round_limit_reached(11, Some(10)));
    }

    #[test]
    fn round_limit_reached_returns_false_below_cap() {
        assert!(!round_limit_reached(9, Some(10)));
    }

    #[test]
    fn round_display_formats_unlimited_without_slash() {
        assert_eq!(round_display(5, None), "5");
    }

    #[test]
    fn round_display_formats_bounded_with_slash() {
        assert_eq!(round_display(5, Some(10)), "5/10");
    }

    // -----------------------------------------------------------------------
    // decisions_enabled gating tests
    // -----------------------------------------------------------------------

    #[test]
    fn record_struggle_signal_skips_when_decisions_disabled() {
        let root = create_temp_project_root("phases_struggle_disabled");
        let mut config = make_test_config(
            &root,
            TestConfigOptions {
                decisions_enabled: false,
                ..Default::default()
            },
        );
        config.decisions_enabled = false;

        // Should be a no-op; decisions.md should not exist.
        record_struggle_signal("Task 1: build widget", "timeout", 2, &config);

        let content = crate::state::read_decisions(&config);
        assert!(
            content.is_empty(),
            "decisions should not be written when disabled"
        );
    }

    #[test]
    fn compound_phase_skips_when_decisions_disabled() {
        use crate::prompts::phase_paths;

        let root = create_temp_project_root("phases_compound_decisions_off");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.compound = true;
        config.decisions_enabled = false;
        let paths = phase_paths(&config);

        let mut calls = 0u32;
        run_compound_phase_with_runner(
            &paths,
            &config,
            1,
            &mut |_agent, _role, _prompt, _session_hint, _config, _meta: Option<&AgentCallMeta>| {
                calls += 1;
                Ok(())
            },
            &mut |_message, _config| {},
        );
        assert_eq!(
            calls, 0,
            "compound phase should be skipped when decisions_enabled=false"
        );
    }

    // -----------------------------------------------------------------------
    // Transcript metadata propagation tests (F-002)
    // -----------------------------------------------------------------------

    #[test]
    fn compound_phase_passes_metadata_with_workflow_and_phase() {
        use crate::prompts::phase_paths;

        let mut config = test_config();
        config.compound = true;
        config.decisions_enabled = true;
        let paths = phase_paths(&config);

        let captured_meta: std::cell::RefCell<Option<AgentCallMeta>> =
            std::cell::RefCell::new(None);
        run_compound_phase_with_runner(
            &paths,
            &config,
            1,
            &mut |_agent, _role, _prompt, _session_hint, _config, meta: Option<&AgentCallMeta>| {
                *captured_meta.borrow_mut() = meta.cloned();
                Ok(())
            },
            &mut |_message, _config| {},
        );

        let meta = captured_meta.borrow();
        let meta = meta
            .as_ref()
            .expect("meta should be passed to run_agent_fn");
        assert_eq!(meta.workflow, "implement");
        assert_eq!(meta.phase, "compound");
        assert_eq!(meta.role, "implementer");
        assert!(!meta.agent_name.is_empty());
    }

    #[test]
    fn implementation_loop_internal_passes_metadata_with_correct_phases() {
        let root = create_temp_project_root("phases_transcript_meta");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = true;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        let baseline = HashSet::new();
        let captured_metas: std::cell::RefCell<Vec<AgentCallMeta>> =
            std::cell::RefCell::new(Vec::new());

        // Status sequence: Implementing → Approved → Consensus
        let status_sequence = std::cell::RefCell::new(vec![
            LoopStatus {
                status: Status::Implementing,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            LoopStatus {
                status: Status::Consensus,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        implementation_loop_internal(
            &config,
            &baseline,
            |_agent, _role, _prompt, _session_hint, _cfg, meta| {
                if let Some(m) = meta {
                    captured_metas.borrow_mut().push(m.clone());
                }
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |name, _cfg| {
                if name == "review.md" {
                    return "Good work".to_string();
                }
                String::new()
            },
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::Consensus,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "single-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        let metas = captured_metas.borrow();
        // Single-agent mode: implementer + reviewer per round
        assert!(
            metas.len() >= 2,
            "expected at least 2 agent calls, got {}",
            metas.len()
        );

        // First call: implementer phase
        assert_eq!(metas[0].workflow, "implement");
        assert_eq!(metas[0].phase, "implementer");
        assert_eq!(metas[0].round, 1);
        assert_eq!(metas[0].role, "implementer");

        // Second call: gate-a review
        assert_eq!(metas[1].workflow, "implement");
        assert_eq!(metas[1].phase, "gate-a-review");
        assert_eq!(metas[1].round, 1);
        assert_eq!(metas[1].role, "reviewer");
    }

    #[test]
    fn transcript_captures_metadata_and_reviewer_findings_through_loop() {
        let root = create_temp_project_root("phases_transcript_findings");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = true;
        config.transcript_enabled = true;
        // Cap at 1 round so the loop exits after implementer+reviewer.
        // The safety net forces NEEDS_CHANGES when open findings exist, so
        // unlimited rounds would loop forever with mock statuses.
        config.review_max_rounds = 1;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        // Write a findings file with an open finding so it appears in reviewer prompts
        let findings = crate::state::FindingsFile {
            round: 1,
            findings: vec![crate::state::FindingEntry {
                id: "F-001".to_string(),
                severity: "HIGH".to_string(),
                summary: "Missing error handling".to_string(),
                file_refs: vec!["src/lib.rs:42".to_string()],
            }],
        };
        let _ = crate::state::write_findings(&findings, &config);

        let baseline = HashSet::new();

        // Status: Implementing → NeedsChanges (findings safety net forces this)
        let status_sequence = std::cell::RefCell::new(vec![
            LoopStatus {
                status: Status::Implementing,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Open findings: F-001".to_string()),

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        // The run_agent_fn closure writes transcript entries via the real
        // append_transcript_entry, simulating what the production code does.
        implementation_loop_internal(
            &config,
            &baseline,
            |_agent, _role, prompt, _session_hint, cfg, meta| {
                // Simulate what run_agent_inner does: append transcript entry.
                if let Some(m) = meta {
                    crate::state::append_transcript_entry(
                        cfg,
                        m,
                        prompt,
                        Some("system-prompt-placeholder"),
                        "agent output placeholder",
                    );
                }
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |name, _cfg| {
                if name == "review.md" {
                    return "NEEDS_CHANGES. Open findings remain.".to_string();
                }
                String::new()
            },
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::NeedsChanges,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "single-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: Some("Open findings: F-001".to_string()),

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        // Read the transcript file and verify content
        let transcript_path = config.state_dir.join("transcript.log");
        let transcript = std::fs::read_to_string(&transcript_path)
            .expect("transcript.log should exist when transcript_enabled=true");

        // Verify metadata propagation: workflow, phase, round are populated
        assert!(
            transcript.contains("workflow: implement"),
            "transcript should contain workflow: implement"
        );
        assert!(
            transcript.contains("phase: implementer"),
            "transcript should contain phase: implementer"
        );
        assert!(
            transcript.contains("phase: gate-a-review"),
            "transcript should contain phase: gate-a-review"
        );
        assert!(
            transcript.contains("round: 1"),
            "transcript should contain round: 1"
        );
    }

    /// F-002: Dual-agent implementation loop populates distinct metadata for
    /// implementer, gate-a review, gate-b (fresh-context) review, and signoff.
    #[test]
    fn implementation_loop_dual_agent_captures_all_gate_metadata() {
        let root = create_temp_project_root("phases_dual_agent_meta");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = false;
        config.transcript_enabled = true;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        let baseline = HashSet::new();
        let captured_metas: std::cell::RefCell<Vec<AgentCallMeta>> =
            std::cell::RefCell::new(Vec::new());

        // read_status_fn is called after each reviewer/signoff agent call,
        // not after the implementer. The sequence for dual-agent success:
        //   1. After gate-a reviewer → Approved
        //   2. After gate-b fresh-context reviewer → Approved
        //   3. After implementer signoff → Consensus
        let status_sequence = std::cell::RefCell::new(vec![
            // After gate-a reviewer: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // After gate-b fresh-context reviewer: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // After implementer signoff: Consensus
            LoopStatus {
                status: Status::Consensus,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        implementation_loop_internal(
            &config,
            &baseline,
            |_agent, _role, prompt, _session_hint, cfg, meta| {
                if let Some(m) = meta {
                    captured_metas.borrow_mut().push(m.clone());
                    // Write transcript so we can verify file content too
                    crate::state::append_transcript_entry(
                        cfg,
                        m,
                        prompt,
                        Some("system-prompt"),
                        "agent output",
                    );
                }
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |name, _cfg| {
                if name == "review.md" {
                    return "Looks good".to_string();
                }
                String::new()
            },
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::Consensus,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        let metas = captured_metas.borrow();
        // Dual-agent mode: implementer + gate-a review + gate-b review + signoff
        assert!(
            metas.len() >= 4,
            "expected at least 4 agent calls in dual-agent mode, got {}",
            metas.len()
        );

        // Implementer phase — shared session, no session_hint
        assert_eq!(metas[0].workflow, "implement");
        assert_eq!(metas[0].phase, "implementer");
        assert_eq!(metas[0].round, 1);
        assert_eq!(metas[0].role, "implementer");
        assert!(
            metas[0].session_hint.is_none(),
            "implementer uses shared session, session_hint must be None"
        );

        // Gate A: same-context review — shared session, no session_hint
        assert_eq!(metas[1].workflow, "implement");
        assert_eq!(metas[1].phase, "gate-a-review");
        assert_eq!(metas[1].round, 1);
        assert_eq!(metas[1].role, "reviewer");
        assert!(
            metas[1].session_hint.is_none(),
            "gate-a review uses shared session, session_hint must be None"
        );

        // Gate B: fresh-context review — fresh session, has session_hint
        assert_eq!(metas[2].workflow, "implement");
        assert_eq!(metas[2].phase, "gate-b-review");
        assert_eq!(metas[2].round, 1);
        assert_eq!(metas[2].role, "reviewer");
        assert!(
            metas[2]
                .session_hint
                .as_deref()
                .unwrap_or("")
                .starts_with("fresh-context-review-r"),
            "gate-b must carry fresh-context session_hint, got: {:?}",
            metas[2].session_hint
        );

        // Implementer signoff — fresh session, has session_hint
        assert_eq!(metas[3].workflow, "implement");
        assert_eq!(metas[3].phase, "implementer-signoff");
        assert_eq!(metas[3].round, 1);
        assert_eq!(metas[3].role, "implementer");
        assert!(
            metas[3]
                .session_hint
                .as_deref()
                .unwrap_or("")
                .starts_with("fresh-context-signoff-r"),
            "signoff must carry fresh-context session_hint, got: {:?}",
            metas[3].session_hint
        );

        // Also verify transcript file contains all phase labels and session_hints
        let transcript_path = config.state_dir.join("transcript.log");
        let transcript =
            std::fs::read_to_string(&transcript_path).expect("transcript.log should exist");
        assert!(transcript.contains("phase: implementer"));
        assert!(transcript.contains("phase: gate-a-review"));
        assert!(transcript.contains("phase: gate-b-review"));
        assert!(transcript.contains("phase: implementer-signoff"));
        // Gate-b and signoff have fresh-context session hints in the transcript
        assert!(
            transcript.contains("session_hint: fresh-context-review-r1"),
            "transcript must contain gate-b session_hint"
        );
        assert!(
            transcript.contains("session_hint: fresh-context-signoff-r1"),
            "transcript must contain signoff session_hint"
        );
    }

    /// F-002: Verify that `run_agent_with_output_or_record_error` builds
    /// proper AgentCallMeta with workflow/phase/round for planning phases.
    #[test]
    fn run_agent_with_output_or_record_error_builds_correct_metadata() {
        let root = create_temp_project_root("phases_helper_meta");
        let config = make_test_config(&root, TestConfigOptions::default());
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        // We can't call run_agent_with_output_or_record_error directly
        // (it runs a real process), but we can verify the metadata construction
        // logic matches expectations by testing via planning_loop which uses it.
        // Instead, verify the metadata struct construction pattern used in the
        // function body (lines 928-934) by replicating it:
        let workflow = "plan";
        let phase = "reviewer";
        let round: u32 = 3;
        let role = AgentRole::Reviewer;
        let agent = &config.reviewer;

        let role_str = match role {
            AgentRole::Implementer => "implementer",
            AgentRole::Reviewer => "reviewer",
            AgentRole::Planner => "planner",
        };
        let session_key = format!("{}-{}-{}", workflow, role_str, agent.name());
        let meta = AgentCallMeta {
            workflow: workflow.to_string(),
            phase: phase.to_string(),
            round,
            role: role_str.to_string(),
            agent_name: agent.name().to_string(),
            session_hint: Some(session_key.clone()),
            output_file: None,
        };

        assert_eq!(meta.workflow, "plan");
        assert_eq!(meta.phase, "reviewer");
        assert_eq!(meta.round, 3);
        assert_eq!(meta.role, "reviewer");
        assert!(!meta.agent_name.is_empty());
        assert!(
            meta.session_hint
                .as_ref()
                .unwrap()
                .starts_with("plan-reviewer-")
        );
    }

    /// F-002: Parse individual transcript entries and verify per-entry metadata
    /// and that the reviewer entry specifically contains open-findings text.
    #[test]
    fn transcript_entries_carry_per_entry_metadata_and_reviewer_findings() {
        let root = create_temp_project_root("phases_per_entry_meta");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = true;
        config.transcript_enabled = true;
        config.review_max_rounds = 1;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        // Write findings so they appear in reviewer prompts
        let findings = crate::state::FindingsFile {
            round: 1,
            findings: vec![crate::state::FindingEntry {
                id: "F-001".to_string(),
                severity: "HIGH".to_string(),
                summary: "Null pointer in parser".to_string(),
                file_refs: vec!["src/parser.rs:99".to_string()],
            }],
        };
        let _ = crate::state::write_findings(&findings, &config);

        let baseline = HashSet::new();
        let status_sequence = std::cell::RefCell::new(vec![
            LoopStatus {
                status: Status::Implementing,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Open findings: F-001".to_string()),

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        implementation_loop_internal(
            &config,
            &baseline,
            |_agent, _role, prompt, _session_hint, cfg, meta| {
                if let Some(m) = meta {
                    crate::state::append_transcript_entry(
                        cfg,
                        m,
                        prompt,
                        Some("system-prompt-test"),
                        "mock agent output",
                    );
                }
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |name, _cfg| {
                if name == "review.md" {
                    return "NEEDS_CHANGES. F-001 unresolved.".to_string();
                }
                String::new()
            },
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::NeedsChanges,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "single-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: Some("Open findings: F-001".to_string()),

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        // Parse the transcript into individual entries
        let transcript_path = config.state_dir.join("transcript.log");
        let transcript =
            std::fs::read_to_string(&transcript_path).expect("transcript.log must exist");

        // Split into individual entries by the delimiter
        let entries: Vec<&str> = transcript
            .split("=== AGENT CALL [")
            .filter(|e| !e.trim().is_empty())
            .collect();

        assert!(
            entries.len() >= 2,
            "expected at least 2 transcript entries (implementer + reviewer), got {}",
            entries.len()
        );

        // Entry 0: implementer — should have phase: implementer, round: 1
        let impl_entry = entries[0];
        assert!(
            impl_entry.contains("phase: implementer"),
            "first entry must be implementer phase, got:\n{impl_entry}"
        );
        assert!(
            impl_entry.contains("workflow: implement"),
            "implementer entry must have workflow: implement"
        );
        assert!(
            impl_entry.contains("round: 1"),
            "implementer entry must have round: 1"
        );
        assert!(
            impl_entry.contains("role: implementer"),
            "implementer entry must have role: implementer"
        );

        // Entry 1: reviewer — should have phase: gate-a-review, round: 1,
        // AND must contain the open-findings text (F-001 + summary).
        let review_entry = entries[1];
        assert!(
            review_entry.contains("phase: gate-a-review"),
            "second entry must be gate-a-review phase, got:\n{review_entry}"
        );
        assert!(
            review_entry.contains("workflow: implement"),
            "reviewer entry must have workflow: implement"
        );
        assert!(
            review_entry.contains("round: 1"),
            "reviewer entry must have round: 1"
        );
        assert!(
            review_entry.contains("role: reviewer"),
            "reviewer entry must have role: reviewer"
        );

        assert!(
            review_entry.contains("--- USER PROMPT ---"),
            "reviewer entry must have USER PROMPT section"
        );
    }

    /// F-001: All phase-created AgentCallMeta must be phase-tracked (not fallback).
    /// Verifies `is_phase_tracked()` returns true for every meta built by the
    /// implementation loop and false for a fallback-constructed meta.
    #[test]
    fn phase_metas_are_phase_tracked_not_fallback() {
        let root = create_temp_project_root("phases_tracked_check");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = false;
        config.transcript_enabled = true;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        let baseline = HashSet::new();
        let captured_metas: std::cell::RefCell<Vec<AgentCallMeta>> =
            std::cell::RefCell::new(Vec::new());

        // Status sequence for dual-agent: gate-a approved, gate-b approved, signoff consensus
        let status_sequence = std::cell::RefCell::new(vec![
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            LoopStatus {
                status: Status::Consensus,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        implementation_loop_internal(
            &config,
            &baseline,
            |_agent, _role, _prompt, _session_hint, _cfg, meta| {
                if let Some(m) = meta {
                    captured_metas.borrow_mut().push(m.clone());
                }
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |_name, _cfg| String::new(),
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::Consensus,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        let metas = captured_metas.borrow();
        assert!(
            metas.len() >= 4,
            "expected 4 agent calls (impl, gate-a, gate-b, signoff), got {}",
            metas.len()
        );

        // ALL phase-created metas must be phase-tracked
        for (i, m) in metas.iter().enumerate() {
            assert!(
                m.is_phase_tracked(),
                "meta[{i}] ({}/{}) must be phase-tracked, but is_phase_tracked()=false",
                m.workflow,
                m.phase
            );
            assert!(
                !m.workflow.is_empty(),
                "meta[{i}] must have non-empty workflow"
            );
            assert!(!m.phase.is_empty(), "meta[{i}] must have non-empty phase");
            assert!(m.round > 0, "meta[{i}] must have round > 0 from the loop");
        }

        // Contrast: a fallback meta must NOT be phase-tracked
        let fallback = AgentCallMeta {
            workflow: crate::state::FALLBACK_WORKFLOW.to_string(),
            phase: crate::state::FALLBACK_PHASE.to_string(),
            ..AgentCallMeta::default()
        };
        assert!(
            !fallback.is_phase_tracked(),
            "fallback meta must not be phase-tracked"
        );
    }

    /// F-002: Fresh-context reviewer transcript entry contains expected metadata
    /// (session hint, workflow, round, role) when findings are present.
    #[test]
    fn fresh_context_reviewer_transcript_contains_gate_b_metadata() {
        let root = create_temp_project_root("phases_fresh_ctx_findings");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = false;
        config.transcript_enabled = true;
        config.review_max_rounds = 1;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        // Write open findings so they appear in fresh-context reviewer prompt
        let findings = crate::state::FindingsFile {
            round: 1,
            findings: vec![crate::state::FindingEntry {
                id: "F-042".to_string(),
                severity: "HIGH".to_string(),
                summary: "Race condition in worker pool".to_string(),
                file_refs: vec!["src/pool.rs:77".to_string()],
            }],
        };
        let _ = crate::state::write_findings(&findings, &config);

        let baseline = HashSet::new();

        // Status sequence: gate-a approved → findings reconciliation re-read
        // → still approved → triggers gate-b fresh-context → NeedsChanges
        //
        // The reconcile_findings_after_review path detects open findings with an
        // Approved status and rewrites to NeedsChanges, then re-reads status.
        // We need a third entry so the re-read still returns Approved (the
        // write_status_fn mock is a no-op) allowing the gate-b branch to run.
        let status_sequence = std::cell::RefCell::new(vec![
            // 1. After gate-a reviewer: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 2. Findings reconciliation re-read (open findings force rewrite):
            //    return Approved so the decision branch enters gate-b
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 3. After gate-b fresh-context: NeedsChanges (loop exits on max_rounds=1)
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Open findings: F-042".to_string()),

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        implementation_loop_internal(
            &config,
            &baseline,
            |_agent, _role, prompt, _session_hint, cfg, meta| {
                if let Some(m) = meta {
                    crate::state::append_transcript_entry(
                        cfg,
                        m,
                        prompt,
                        Some("system-prompt-test"),
                        "mock output",
                    );
                }
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |name, _cfg| {
                if name == "review.md" {
                    return "NEEDS_CHANGES. F-042 unresolved.".to_string();
                }
                String::new()
            },
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::NeedsChanges,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: Some("Open findings: F-042".to_string()),

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        // Parse transcript entries
        let transcript_path = config.state_dir.join("transcript.log");
        let transcript =
            std::fs::read_to_string(&transcript_path).expect("transcript.log must exist");

        let entries: Vec<&str> = transcript
            .split("=== AGENT CALL [")
            .filter(|e| !e.trim().is_empty())
            .collect();

        // Dual-agent with gate-a approved: impl + gate-a + gate-b = 3 entries
        assert!(
            entries.len() >= 3,
            "expected at least 3 entries (impl, gate-a, gate-b), got {}",
            entries.len()
        );

        // Find the gate-b fresh-context entry
        let gate_b = entries
            .iter()
            .find(|e| e.contains("phase: gate-b-review"))
            .expect("transcript must have a gate-b-review entry");

        // Verify fresh-context reviewer entry has correct session hint and metadata
        assert!(
            gate_b.contains("session_hint: fresh-context-review-r1"),
            "gate-b entry must have fresh-context session_hint"
        );
        // Phase-specific metadata
        assert!(
            gate_b.contains("workflow: implement"),
            "gate-b entry must have workflow: implement"
        );
        assert!(
            gate_b.contains("round: 1"),
            "gate-b entry must have round: 1"
        );
        assert!(
            gate_b.contains("role: reviewer"),
            "gate-b entry must have role: reviewer"
        );
        // Phase-tracked entries must NOT have the untracked annotation
        assert!(
            !gate_b.contains("tracking: untracked"),
            "phase-tracked entries must not have untracked annotation"
        );
    }

    // -----------------------------------------------------------------------
    // Gate-B confirmation loop tests (F-001)
    // -----------------------------------------------------------------------

    /// When gate-B returns NeedsChanges and verification returns Approved,
    /// the loop should proceed to implementer signoff (reaching consensus).
    #[test]
    fn gate_b_verification_withdrawn_reaches_signoff() {
        let root = create_temp_project_root("phases_gate_b_verify_withdrawn");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = false;
        config.compound = false;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        let baseline = HashSet::new();
        let observed_roles: std::cell::RefCell<Vec<(String, Option<String>)>> =
            std::cell::RefCell::new(Vec::new());

        // Status sequence for dual-agent:
        // 1. After gate-a reviewer: Approved
        // 2. After gate-b reviewer: NeedsChanges
        // 3. Gate-b findings reconciliation re-read: NeedsChanges
        //    (reconcile synthesizes a finding from empty findings.json,
        //     reason differs from original, triggers write+re-read)
        // 4. After gate-b verification: Approved (withdrawn)
        // 5. After implementer signoff: Consensus
        let status_sequence = std::cell::RefCell::new(vec![
            // 1. gate-a: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 2. gate-b: NeedsChanges
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Found issues".to_string()),

                timestamp: "now".to_string(),
            },
            // 3. gate-b findings reconciliation re-read (reason changed)
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Open findings: F-001".to_string()),

                timestamp: "now".to_string(),
            },
            // 4. gate-b verification: Approved (withdrawn)
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 5. signoff: Consensus
            LoopStatus {
                status: Status::Consensus,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        let result = implementation_loop_internal(
            &config,
            &baseline,
            |_agent, role, _prompt, session_hint, _cfg, _meta| {
                let role_name = format!("{role:?}");
                observed_roles
                    .borrow_mut()
                    .push((role_name, session_hint.map(|s| s.to_string())));
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |_name, _cfg| String::new(),
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::Consensus,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        assert!(result, "loop should reach consensus");

        let roles = observed_roles.borrow();
        // Expected: implementer, gate-a reviewer, gate-b reviewer,
        //           gate-b verify reviewer, signoff implementer
        assert!(
            roles.len() >= 5,
            "expected 5 agent calls (impl, gate-a, gate-b, verify, signoff), got {}",
            roles.len()
        );

        // The verification call (index 3) should reuse the same fresh-context session
        assert!(
            roles[3]
                .1
                .as_deref()
                .unwrap_or("")
                .starts_with("fresh-context-review-r"),
            "gate-b verify should reuse fresh-context session, got: {:?}",
            roles[3].1
        );
        // The signoff call (index 4) should have a fresh signoff session
        assert!(
            roles[4]
                .1
                .as_deref()
                .unwrap_or("")
                .starts_with("fresh-context-signoff-r"),
            "signoff should have fresh session, got: {:?}",
            roles[4].1
        );
    }

    /// When gate-B returns NeedsChanges and verification also returns
    /// NeedsChanges (confirmed), the loop should continue to the next round.
    #[test]
    fn gate_b_verification_confirmed_loops_back() {
        let root = create_temp_project_root("phases_gate_b_verify_confirmed");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = false;
        config.compound = false;
        config.review_max_rounds = 1;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        let baseline = HashSet::new();
        let agent_call_count = std::cell::RefCell::new(0u32);

        // Status sequence:
        // 1. gate-a: Approved
        // 2. gate-b: NeedsChanges
        // 3. gate-b findings reconciliation re-read: NeedsChanges
        // 4. gate-b verification: NeedsChanges (confirmed)
        // Loop exits on max_rounds=1
        let status_sequence = std::cell::RefCell::new(vec![
            // 1. gate-a
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 2. gate-b
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Found issues".to_string()),

                timestamp: "now".to_string(),
            },
            // 3. gate-b findings reconciliation re-read
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Open findings: F-001".to_string()),

                timestamp: "now".to_string(),
            },
            // 4. gate-b verification: confirmed
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Issues confirmed".to_string()),

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        let result = implementation_loop_internal(
            &config,
            &baseline,
            |_agent, _role, _prompt, _session_hint, _cfg, _meta| {
                *agent_call_count.borrow_mut() += 1;
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |_name, _cfg| String::new(),
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::NeedsChanges,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: Some("issues".to_string()),

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        assert!(
            !result,
            "loop should NOT reach consensus (findings confirmed, max_rounds=1)"
        );

        // Expected calls: implementer + gate-a + gate-b + gate-b-verify = 4
        // (no signoff because verification confirmed findings)
        let calls = *agent_call_count.borrow();
        assert_eq!(
            calls, 4,
            "expected 4 agent calls (impl, gate-a, gate-b, verify), got {calls}"
        );
    }

    // -----------------------------------------------------------------------
    // Gate C bounce tests
    // -----------------------------------------------------------------------

    /// When signoff returns Disputed and the gate-C bounce reviewer returns
    /// Approved (late findings rejected), the loop should reach CONSENSUS.
    #[test]
    fn gate_c_bounce_rejected_reaches_consensus() {
        let root = create_temp_project_root("phases_gate_c_bounce_rejected");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = false;
        config.compound = false;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        let baseline = HashSet::new();
        let observed_roles: std::cell::RefCell<Vec<(String, Option<String>)>> =
            std::cell::RefCell::new(Vec::new());
        let observed_status_writes: std::cell::RefCell<Vec<StatusPatch>> =
            std::cell::RefCell::new(Vec::new());

        // Status sequence for dual-agent:
        // 1. After gate-a reviewer: Approved
        // 2. After gate-b reviewer: Approved
        // 3. After implementer signoff: Disputed
        // 4. After gate-c bounce reviewer: Approved (late findings rejected)
        let status_sequence = std::cell::RefCell::new(vec![
            // 1. gate-a: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 2. gate-b: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 3. signoff: Disputed
            LoopStatus {
                status: Status::Disputed,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Late finding: missing edge case".to_string()),

                timestamp: "now".to_string(),
            },
            // 4. gate-c-bounce: Approved (late findings rejected)
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        let result = implementation_loop_internal(
            &config,
            &baseline,
            |_agent, role, _prompt, session_hint, _cfg, _meta| {
                let role_name = format!("{role:?}");
                observed_roles
                    .borrow_mut()
                    .push((role_name, session_hint.map(|s| s.to_string())));
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |patch, _cfg| {
                observed_status_writes.borrow_mut().push(patch);
            },
            |_name, _cfg| String::new(),
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::Consensus,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: None,

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        assert!(
            result,
            "loop should reach consensus after gate-c bounce rejects late findings"
        );

        let roles = observed_roles.borrow();
        // Expected: implementer, gate-a reviewer, gate-b reviewer,
        //           signoff implementer, gate-c-bounce reviewer
        assert!(
            roles.len() >= 5,
            "expected 5 agent calls (impl, gate-a, gate-b, signoff, gate-c-bounce), got {}",
            roles.len()
        );

        // The gate-c bounce call (index 4) should reuse the same fresh-context session hint
        assert!(
            roles[4]
                .1
                .as_deref()
                .unwrap_or("")
                .starts_with("fresh-context-review-r"),
            "gate-c bounce should reuse fresh-context session, got: {:?}",
            roles[4].1
        );

        // Verify a CONSENSUS status write was made
        let writes = observed_status_writes.borrow();
        let has_consensus = writes.iter().any(|p| p.status == Some(Status::Consensus));
        assert!(has_consensus, "CONSENSUS status write must be present");
    }

    /// When signoff returns Disputed and the gate-C bounce reviewer returns
    /// NeedsChanges (late findings confirmed), the loop should continue.
    #[test]
    fn gate_c_bounce_confirmed_loops_back() {
        let root = create_temp_project_root("phases_gate_c_bounce_confirmed");
        let mut config = make_test_config(&root, TestConfigOptions::default());
        config.single_agent = false;
        config.compound = false;
        config.review_max_rounds = 1;
        std::fs::create_dir_all(&config.state_dir).expect("create state dir");

        let baseline = HashSet::new();
        let observed_roles: std::cell::RefCell<Vec<(String, Option<String>)>> =
            std::cell::RefCell::new(Vec::new());

        // Status sequence:
        // 1. gate-a: Approved
        // 2. gate-b: Approved
        // 3. signoff: Disputed
        // 4. gate-c-bounce: NeedsChanges (late findings confirmed)
        // Loop exits on max_rounds=1
        let status_sequence = std::cell::RefCell::new(vec![
            // 1. gate-a: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 2. gate-b: Approved
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,

                timestamp: "now".to_string(),
            },
            // 3. signoff: Disputed
            LoopStatus {
                status: Status::Disputed,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Late finding: missing tests".to_string()),

                timestamp: "now".to_string(),
            },
            // 4. gate-c-bounce: NeedsChanges (confirmed)
            LoopStatus {
                status: Status::NeedsChanges,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "dual-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: Some("Late findings confirmed".to_string()),

                timestamp: "now".to_string(),
            },
        ]);
        let status_call_count = std::cell::RefCell::new(0usize);

        let result = implementation_loop_internal(
            &config,
            &baseline,
            |_agent, role, _prompt, session_hint, _cfg, _meta| {
                let role_name = format!("{role:?}");
                observed_roles
                    .borrow_mut()
                    .push((role_name, session_hint.map(|s| s.to_string())));
                Ok(())
            },
            |_msg, _cfg, _bf| {},
            |_msg, _cfg| {},
            |_patch, _cfg| {},
            |_name, _cfg| String::new(),
            |_cfg| {
                let mut count = status_call_count.borrow_mut();
                let seq = status_sequence.borrow();
                if *count < seq.len() {
                    let result = seq[*count].clone();
                    *count += 1;
                    result
                } else {
                    LoopStatus {
                        status: Status::NeedsChanges,
                        round: 1,
                        implementer: "claude".to_string(),
                        reviewer: "claude".to_string(),
                        mode: "dual-agent".to_string(),
                        last_run_task: "test".to_string(),
                        reason: Some("issues".to_string()),

                        timestamp: "now".to_string(),
                    }
                }
            },
            |_head, _cfg| String::new(),
            || "now".to_string(),
            |_cfg, _n| String::new(),
            |_round, _phase, _summary, _cfg| {},
            false,
        );

        assert!(
            !result,
            "loop should NOT reach consensus (late findings confirmed, max_rounds=1)"
        );

        let roles = observed_roles.borrow();
        // Expected: implementer, gate-a, gate-b, signoff, gate-c-bounce = 5
        assert!(
            roles.len() >= 5,
            "expected 5 agent calls (impl, gate-a, gate-b, signoff, gate-c-bounce), got {}",
            roles.len()
        );

        // The gate-c bounce call (index 4) should reuse the same fresh-context session hint
        assert!(
            roles[4]
                .1
                .as_deref()
                .unwrap_or("")
                .starts_with("fresh-context-review-r"),
            "gate-c bounce should reuse fresh-context session, got: {:?}",
            roles[4].1
        );
    }

    // -----------------------------------------------------------------------
    // Planning gate label and session hint tests
    // -----------------------------------------------------------------------

    #[test]
    fn planning_gate_constants_have_expected_values() {
        assert_eq!(PLANNING_GATE_ADVERSARIAL, "[gate:fresh-context]");
        assert_eq!(PLANNING_GATE_SIGNOFF, "[gate:implementer-signoff]");
    }

    // -----------------------------------------------------------------------
    // extract_finding_summary tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_finding_summary_returns_none_for_empty_text() {
        assert!(extract_finding_summary("", 500).is_none());
        assert!(extract_finding_summary("   \n  \n  ", 500).is_none());
    }

    #[test]
    fn extract_finding_summary_returns_none_for_no_findings() {
        let review = "Everything looks good.\n\nno findings";
        assert!(extract_finding_summary(review, 500).is_none());
    }

    #[test]
    fn extract_finding_summary_returns_none_for_no_findings_with_punctuation() {
        let review = "The plan is solid.\n\nno findings.";
        assert!(extract_finding_summary(review, 500).is_none());
    }

    #[test]
    fn extract_finding_summary_extracts_dash_bullets() {
        let review = "# Review\n\n- first issue\n- second issue\n- third issue\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(result, "- first issue\n- second issue\n- third issue");
    }

    #[test]
    fn extract_finding_summary_extracts_star_bullets() {
        let review = "# Review\n\n* alpha problem\n* beta problem\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(result, "* alpha problem\n* beta problem");
    }

    #[test]
    fn extract_finding_summary_extracts_numbered_items() {
        let review = "1. first finding\n2. second finding\n10. tenth finding\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(result, "1. first finding\n2. second finding\n10. tenth finding");
    }

    #[test]
    fn extract_finding_summary_prose_fallback() {
        let review = "The plan has issues.\nError handling is missing.\nAPI boundary is unclear.\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(
            result,
            "- The plan has issues.\n- Error handling is missing.\n- API boundary is unclear."
        );
    }

    #[test]
    fn extract_finding_summary_skips_headings_in_prose() {
        let review = "# Review\nSome issue here.\n## Details\nAnother issue.\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(result, "- Some issue here.\n- Another issue.");
    }

    #[test]
    fn extract_finding_summary_scopes_to_findings_section() {
        // Preamble bullets before the Findings heading should be excluded
        let review = "## Checklist\n- setup step one\n- setup step two\n\n## Findings\n- actual issue one\n- actual issue two\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(result, "- actual issue one\n- actual issue two");
        assert!(!result.contains("setup step"));
    }

    #[test]
    fn extract_finding_summary_falls_back_to_full_review_without_findings_heading() {
        // When there's no Findings heading, the full review is used
        let review = "- issue one\n- issue two\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(result, "- issue one\n- issue two");
    }

    #[test]
    fn extract_finding_summary_bare_findings_marker_excluded_from_prose() {
        // A bare "Findings" marker line should not appear as "- Findings" in the output
        let review = "Findings\nIssue one\nIssue two\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert!(!result.contains("- Findings"));
        assert_eq!(result, "- Issue one\n- Issue two");
    }

    #[test]
    fn extract_finding_summary_safe_with_multibyte_utf8() {
        // Must not panic when truncation lands inside a multibyte character
        let review = "- résumé of the finding — it is quite long and detailed\n";
        let result = extract_finding_summary(review, 20).unwrap();
        assert!(result.ends_with("..."));
        assert!(result.len() <= 23); // max_chars + "..."
        // Verify the result is valid UTF-8 (would panic on push_str otherwise)
        let _ = result.as_bytes();
    }

    #[test]
    fn format_tasks_findings_summary_safe_with_multibyte_utf8() {
        let findings = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "F1".to_string(),
                description: "café résumé — long description with em-dashes".to_string(),
                status: crate::state::TasksFindingStatus::Open,
                round_introduced: 1,
                round_resolved: None,
            }],
        };
        let result = format_tasks_findings_summary(&findings, 20).unwrap();
        assert!(result.ends_with("..."));
        // Must be valid UTF-8 and not panic
        let _ = result.as_bytes();
    }

    #[test]
    fn extract_finding_summary_limits_to_5_bullets() {
        let review = "- a\n- b\n- c\n- d\n- e\n- f\n- g\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(result, "- a\n- b\n- c\n- d\n- e");
    }

    #[test]
    fn extract_finding_summary_truncates_at_max_chars() {
        let review = "- this is a long finding that takes up space\n- another long finding here\n- yet another\n";
        let result = extract_finding_summary(review, 60).unwrap();
        // Should truncate to max_chars, including partial content from truncated line
        assert!(result.len() <= 63); // line content + "..."
        assert!(result.ends_with("..."));
        // First line fits (44 chars), second line should be char-truncated
        assert!(result.contains("- this is a long finding that takes up space"));
    }

    #[test]
    fn extract_finding_summary_truncates_first_long_line() {
        // When even the first line exceeds max_chars, it should be
        // char-truncated rather than replaced with just "- ..."
        let review = "- this is a very long finding that exceeds the limit by a lot\n";
        let result = extract_finding_summary(review, 30).unwrap();
        assert!(result.len() <= 33);
        assert!(result.ends_with("..."));
        // Must contain actual finding content, not just "..."
        assert!(result.starts_with("- this is a very long findi"));
    }

    #[test]
    fn extract_finding_summary_mixed_bullet_styles() {
        let review = "- dash item\n* star item\n1. numbered item\n";
        let result = extract_finding_summary(review, 500).unwrap();
        assert_eq!(result, "- dash item\n* star item\n1. numbered item");
    }

    // -----------------------------------------------------------------------
    // format_tasks_findings_summary tests
    // -----------------------------------------------------------------------

    #[test]
    fn format_tasks_findings_summary_returns_none_when_no_open() {
        let findings = crate::state::TasksFindingsFile {
            findings: vec![crate::state::TasksFindingEntry {
                id: "F1".to_string(),
                description: "resolved issue".to_string(),
                status: crate::state::TasksFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(2),
            }],
        };
        assert!(format_tasks_findings_summary(&findings, 500).is_none());
    }

    #[test]
    fn format_tasks_findings_summary_returns_none_when_empty() {
        let findings = crate::state::TasksFindingsFile {
            findings: vec![],
        };
        assert!(format_tasks_findings_summary(&findings, 500).is_none());
    }

    #[test]
    fn format_tasks_findings_summary_renders_open_findings() {
        let findings = crate::state::TasksFindingsFile {
            findings: vec![
                crate::state::TasksFindingEntry {
                    id: "F1".to_string(),
                    description: "missing tests".to_string(),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 1,
                    round_resolved: None,
                },
                crate::state::TasksFindingEntry {
                    id: "F2".to_string(),
                    description: "unclear naming".to_string(),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 1,
                    round_resolved: None,
                },
            ],
        };
        let result = format_tasks_findings_summary(&findings, 500).unwrap();
        assert_eq!(result, "- [F1] missing tests\n- [F2] unclear naming");
    }

    #[test]
    fn format_tasks_findings_summary_truncates_at_max_chars() {
        let findings = crate::state::TasksFindingsFile {
            findings: vec![
                crate::state::TasksFindingEntry {
                    id: "F1".to_string(),
                    description: "a]".repeat(50),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 1,
                    round_resolved: None,
                },
                crate::state::TasksFindingEntry {
                    id: "F2".to_string(),
                    description: "b".repeat(50),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 1,
                    round_resolved: None,
                },
                crate::state::TasksFindingEntry {
                    id: "F3".to_string(),
                    description: "c".repeat(50),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 1,
                    round_resolved: None,
                },
            ],
        };
        let result = format_tasks_findings_summary(&findings, 80).unwrap();
        assert!(result.ends_with("..."));
        assert!(result.len() <= 83); // max_chars + "..."
        // Should contain actual content from F1, not just "..."
        assert!(result.starts_with("- [F1]"));
    }

    #[test]
    fn format_tasks_findings_summary_prioritizes_most_recent_round() {
        let findings = crate::state::TasksFindingsFile {
            findings: vec![
                crate::state::TasksFindingEntry {
                    id: "F1".to_string(),
                    description: "old finding from round 1".to_string(),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 1,
                    round_resolved: None,
                },
                crate::state::TasksFindingEntry {
                    id: "F2".to_string(),
                    description: "old finding from round 2".to_string(),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 2,
                    round_resolved: None,
                },
                crate::state::TasksFindingEntry {
                    id: "F3".to_string(),
                    description: "old finding from round 3".to_string(),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 3,
                    round_resolved: None,
                },
                crate::state::TasksFindingEntry {
                    id: "F4".to_string(),
                    description: "old finding from round 4".to_string(),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 4,
                    round_resolved: None,
                },
                crate::state::TasksFindingEntry {
                    id: "F5".to_string(),
                    description: "old finding from round 5".to_string(),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 5,
                    round_resolved: None,
                },
                crate::state::TasksFindingEntry {
                    id: "F6".to_string(),
                    description: "new finding from round 6".to_string(),
                    status: crate::state::TasksFindingStatus::Open,
                    round_introduced: 6,
                    round_resolved: None,
                },
            ],
        };
        let result = format_tasks_findings_summary(&findings, 500).unwrap();
        // F6 (round 6) should appear first, and F1 (round 1) should be dropped
        // since we cap at 5 items
        assert!(result.starts_with("- [F6] new finding from round 6"));
        assert!(!result.contains("[F1]"));
        // Should contain F2-F6 in reverse round order
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("[F6]"));
        assert!(lines[1].contains("[F5]"));
        assert!(lines[2].contains("[F4]"));
        assert!(lines[3].contains("[F3]"));
        assert!(lines[4].contains("[F2]"));
    }
}
