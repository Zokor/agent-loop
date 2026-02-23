use std::collections::HashSet;
use std::path::Path;
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
        decomposition_reviewer_prompt, decomposition_revision_prompt, gather_project_context,
        implementation_adversarial_review_prompt, implementation_consensus_prompt,
        implementation_implementer_prompt, implementation_reviewer_prompt, phase_paths,
        planning_adversarial_review_prompt, planning_implementer_revision_prompt,
        planning_implementer_signoff_prompt, planning_initial_prompt, planning_reviewer_prompt,
        state_manifest, system_prompt_for_role,
    },
    state::{
        FindingEntry, FindingsFile, LoopStatus, Status, StatusPatch, append_decision,
        is_status_stale, log, read_decisions, read_findings, read_findings_with_warnings,
        read_state_file, read_status, summarize_task, timestamp, write_findings, write_state_file,
        write_status,
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
const CHECKPOINT_SUMMARY_MAX_LEN: usize = 80;
const IMPLEMENTATION_CHECKPOINT_FALLBACK: &str = "implementation updates";
const QUALITY_CHECK_TIMEOUT_SECS: u64 = 120;
const QUALITY_CHECK_MAX_LINES: usize = 100;
const FINDINGS_PATH_HINT: &str = ".agent-loop/state/findings.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanningReviewerAction {
    Approved,
    NeedsRevision,
    Error,
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

fn reconcile_findings_after_review(
    round: u32,
    status: Status,
    status_reason: Option<&str>,
    previous_findings: &FindingsFile,
    current_findings: FindingsFile,
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
                Some(
                    "Reviewer requested changes but findings.json was empty; carrying forward previous findings.".to_string(),
                )
            } else if normalized.findings.is_empty() {
                normalized = FindingsFile {
                    round,
                    findings: vec![synthesize_finding(status_reason)],
                };
                Some(
                    "Reviewer requested changes but findings.json was empty; synthesized F-001 from status reason.".to_string(),
                )
            } else {
                None
            };

            let reason = format!(
                "Open findings: {}. See {FINDINGS_PATH_HINT}.",
                findings_id_list(&normalized)
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
                    "Cannot approve with unresolved findings: {}. See {FINDINGS_PATH_HINT}.",
                    findings_id_list(&normalized)
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

// ---------------------------------------------------------------------------
// Planning VERDICT parsing and findings reconciliation (Tasks 13 & 14)
// ---------------------------------------------------------------------------

/// Parse `VERDICT: APPROVED` or `VERDICT: REVISE` from reviewer output.
fn parse_planning_verdict(text: &str) -> Option<&str> {
    let re = regex::Regex::new(r"(?i)VERDICT:\s*(APPROVED|REVISE)").ok()?;
    re.captures(text).map(|caps| {
        let m = caps.get(1).unwrap();
        if m.as_str().eq_ignore_ascii_case("APPROVED") {
            "APPROVED"
        } else {
            "REVISE"
        }
    })
}

/// Parse planning findings JSON block from reviewer output.
/// Looks for ```json\n[...]\n``` blocks containing findings with "id" and "description".
fn parse_planning_findings_from_output(
    text: &str,
    round: u32,
) -> Vec<crate::state::PlanningFindingEntry> {
    use crate::state::{PlanningFindingEntry, PlanningFindingStatus};

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
            // Read optional "status" field; case-insensitive, default to Open for
            // backward compatibility ("resolved", "Resolved", "RESOLVED" all accepted).
            let status = match v
                .get("status")
                .and_then(|s| s.as_str())
                .map(|s| s.to_ascii_lowercase())
                .as_deref()
            {
                Some("resolved") => PlanningFindingStatus::Resolved,
                _ => PlanningFindingStatus::Open,
            };
            Some(PlanningFindingEntry {
                id,
                description,
                status,
                round_introduced: round,
                round_resolved: None,
            })
        })
        .collect()
}

/// Reconcile planning verdict with findings, applying safety nets:
/// - REVISE + empty findings → synthesize P-001 from reviewer prose
/// - APPROVED + open findings → force NEEDS_REVISION
fn reconcile_planning_verdict(
    verdict: Option<&str>,
    new_findings: Vec<crate::state::PlanningFindingEntry>,
    existing: &crate::state::PlanningFindingsFile,
    review_status: &LoopStatus,
    round: u32,
    config: &Config,
) -> (PlanningReviewerAction, crate::state::PlanningFindingsFile) {
    use crate::state::{PlanningFindingEntry, PlanningFindingStatus, PlanningFindingsFile};

    // ID-based merge: update existing findings, add new ones, resolve transitions.
    let mut merged = existing.findings.clone();
    for new_f in &new_findings {
        if let Some(existing_entry) = merged.iter_mut().find(|f| f.id == new_f.id) {
            // Update description if changed, transition status.
            existing_entry.description = new_f.description.clone();
            if new_f.status == PlanningFindingStatus::Resolved
                && existing_entry.status == PlanningFindingStatus::Open
            {
                existing_entry.status = PlanningFindingStatus::Resolved;
                existing_entry.round_resolved = Some(round);
            } else if new_f.status == PlanningFindingStatus::Open
                && existing_entry.status == PlanningFindingStatus::Resolved
            {
                // Reopened — reviewer flagged it again.
                existing_entry.status = PlanningFindingStatus::Open;
                existing_entry.round_resolved = None;
            }
        } else {
            // New finding not previously seen.
            let mut entry = new_f.clone();
            if entry.status == PlanningFindingStatus::Resolved {
                entry.round_resolved = Some(round);
            }
            merged.push(entry);
        }
    }

    // Count open findings in the merged result (after reconciliation).
    let open_after_merge = merged
        .iter()
        .filter(|f| f.status == PlanningFindingStatus::Open)
        .count();

    let action = match verdict {
        Some("REVISE") => {
            // Safety net: REVISE + empty findings → synthesize P-001
            if new_findings.is_empty() {
                let desc = review_status.reason.as_deref().unwrap_or(
                    "Reviewer requested revision but did not provide structured findings.",
                );
                let next_id = crate::state::next_planning_finding_id(&PlanningFindingsFile {
                    findings: merged.clone(),
                });
                merged.push(PlanningFindingEntry {
                    id: next_id,
                    description: desc.to_string(),
                    status: PlanningFindingStatus::Open,
                    round_introduced: round,
                    round_resolved: None,
                });
                let _ = log(
                    "Planning reconciliation: REVISE with empty findings — synthesized finding from reviewer prose",
                    config,
                );
            }
            PlanningReviewerAction::NeedsRevision
        }
        Some("APPROVED") => {
            // Safety net: APPROVED + open findings remaining after reconciliation → force NEEDS_REVISION
            if open_after_merge > 0 {
                let _ = log(
                    "Planning reconciliation: APPROVED with open findings — forcing NEEDS_REVISION",
                    config,
                );
                PlanningReviewerAction::NeedsRevision
            } else {
                PlanningReviewerAction::Approved
            }
        }
        _ => {
            // Fall back to status-based decision
            planning_reviewer_action(review_status.status)
        }
    };

    let findings_file = PlanningFindingsFile { findings: merged };
    (action, findings_file)
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
    round == 50 || (round > 50 && (round - 50).is_multiple_of(25))
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
    FRunAgent: FnMut(&crate::config::Agent, AgentRole, &str, &Config) -> Result<(), AgentLoopError>,
    FLog: FnMut(&str, &Config),
{
    if !config.compound {
        return;
    }

    log_fn("🧠 Running compound learning phase...", config);
    if let Err(err) = run_agent_fn(
        &config.implementer,
        AgentRole::Implementer,
        &compound_prompt(task, plan),
        config,
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
pub fn compound_phase(task: &str, plan: &str, config: &Config) {
    run_compound_phase_with_runner(
        task,
        plan,
        config,
        &mut |agent: &crate::config::Agent, role: AgentRole, prompt, current_config| {
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
        None => {
            if patch.rating.is_some() {
                return "rating-update";
            }
            "status-update"
        }
    }
}

fn finalize_phase_consensus(
    config: &Config,
    round: u32,
    reason: Option<String>,
    rating: Option<u32>,
) -> Result<(), AgentLoopError> {
    write_status(
        StatusPatch {
            status: Some(Status::Consensus),
            round: Some(round),
            reason,
            rating,
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
    match run_agent_with_session(
        agent,
        prompt,
        config,
        sp_ref,
        Some(&session_key),
        Some(role),
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
) -> bool {
    run_agent_with_output_or_record_error(config, agent, prompt, round, role, workflow).is_some()
}

// ---------------------------------------------------------------------------
// Planning revision helper
// ---------------------------------------------------------------------------

/// Outcome from running the implementer revision step during planning.
enum PlanningRevisionOutcome {
    /// Agent call failed — caller should return false.
    AgentFailed,
    /// Implementer accepted the revised plan.
    Consensus,
    /// Implementer continues to next round (possibly with concerns).
    Continue { dispute_reason: Option<String> },
}

/// Run the implementer revision step: let the planner review a revised plan,
/// handle stale/error statuses, and determine the outcome.
fn run_planning_implementer_revision(
    config: &Config,
    planner_agent: &Agent,
    task: &str,
    reviewer_reason: &str,
    round: u32,
    paths: &crate::prompts::PhasePaths,
) -> PlanningRevisionOutcome {
    let _ = log("🔍 Implementer reviewing revised plan...", config);
    let revised_plan = read_state_file("plan.md", config);
    let implementer_prompt_timestamp = timestamp();
    if !run_agent_or_record_error(
        config,
        planner_agent,
        &planning_implementer_revision_prompt(
            config,
            task,
            &revised_plan,
            reviewer_reason,
            round,
            &implementer_prompt_timestamp,
            paths,
        ),
        Some(round),
        AgentRole::Implementer,
        "plan",
    ) {
        return PlanningRevisionOutcome::AgentFailed;
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
                round: Some(round),
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
            Some(round),
            status_error_reason(&implementer_status),
        );
        return PlanningRevisionOutcome::AgentFailed;
    }

    if planning_implementer_reached_consensus(implementer_status.status) {
        let _ = log("✅ Both agents agreed on the plan!", config);
        return PlanningRevisionOutcome::Consensus;
    }

    let dispute = if implementer_status.status == Status::Disputed {
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

    PlanningRevisionOutcome::Continue {
        dispute_reason: dispute,
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
        "   - task.md, plan.md, tasks.md, changes.md, review.md, findings.json, status.json, log.txt".to_string(),
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
    let (project_context, decisions) = if config.progressive_context {
        let _ = log(
            "📋 Progressive context mode — agents will explore on-demand",
            config,
        );
        (state_manifest(config), String::new())
    } else {
        (
            gather_project_context(
                &config.project_dir,
                config.effective_context_line_cap() as usize,
                config.effective_planning_context_excerpt_lines() as usize,
            ),
            read_decisions(config),
        )
    };

    let planner_agent = config.planner.clone();
    let planner_plan_mode = planner_plan_mode_active(config, &planner_agent, AgentRole::Planner);

    let _ = log("📝 Implementer proposing plan...", config);
    let planner_output = match run_agent_with_output_or_record_error(
        config,
        &planner_agent,
        &planning_initial_prompt(
            &task,
            &project_context,
            &decisions,
            &paths,
            planner_plan_mode,
        ),
        Some(0),
        AgentRole::Planner,
        "plan",
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

    let mut planning_round = 0;
    let mut reached_consensus = false;
    let mut dispute_reason: Option<String> = None;

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

        let plan = read_state_file("plan.md", config);

        // Read open planning findings for the reviewer prompt.
        let planning_findings = crate::state::read_planning_findings(config);
        let open_findings_text =
            crate::state::open_planning_findings_for_prompt(&planning_findings);

        // Clear review.md before each reviewer round to prevent stale content
        // from a previous round bleeding into verdict/findings parsing.
        let _ = write_state_file("review.md", "", config);

        let _ = log("🔍 Reviewer evaluating plan...", config);
        let reviewer_prompt_timestamp = timestamp();
        let reviewer_output = match run_agent_with_output_or_record_error(
            config,
            &config.reviewer,
            &planning_reviewer_prompt(&PlanningReviewerParams {
                config,
                task: &task,
                plan: &plan,
                project_context: &project_context,
                decisions: &decisions,
                round: planning_round,
                prompt_timestamp: &reviewer_prompt_timestamp,
                paths: &paths,
                dispute_reason: dispute_reason.as_deref(),
                open_findings: &open_findings_text,
            }),
            Some(planning_round),
            AgentRole::Reviewer,
            "plan",
        ) {
            Some(output) => output,
            None => return false,
        };

        // Fallback: if agent didn't write to review.md, persist its captured output
        let review_file = read_state_file("review.md", config);
        if review_file.trim().is_empty() && !reviewer_output.trim().is_empty() {
            let _ = write_state_file("review.md", &reviewer_output, config);
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

        // --- VERDICT parsing and findings reconciliation (Tasks 13 & 14) ---
        // Read the review output (reviewer writes to review.md or plan.md).
        let review_output = read_state_file("review.md", config);
        let verdict = parse_planning_verdict(&review_output);
        let new_findings = parse_planning_findings_from_output(&review_output, planning_round);

        let (reconciled_action, updated_findings) = reconcile_planning_verdict(
            verdict,
            new_findings,
            &planning_findings,
            &review_status,
            planning_round,
            config,
        );

        // Persist updated planning findings.
        let _ = crate::state::write_planning_findings(&updated_findings, config);

        // If reconciliation overrode the status (e.g. forced NEEDS_REVISION),
        // update status.json accordingly.
        if reconciled_action == PlanningReviewerAction::NeedsRevision
            && review_status.status == Status::Approved
        {
            warn_on_status_write(
                "NEEDS_REVISION",
                StatusPatch {
                    status: Some(Status::NeedsRevision),
                    round: Some(planning_round),
                    reason: Some(
                        "[gate:reviewer] Forced NEEDS_REVISION: open planning findings remain"
                            .to_string(),
                    ),
                    ..StatusPatch::default()
                },
                config,
            );
            review_status = read_status(config);
        }

        // Append planning progress after each reviewer round.
        let progress_summary = format!(
            "Reviewer: {} — {} (verdict: {})",
            review_status.status,
            review_status.reason.as_deref().unwrap_or("(no reason)"),
            verdict.unwrap_or("none"),
        );
        crate::state::append_planning_progress(planning_round, &progress_summary, config);

        match reconciled_action {
            PlanningReviewerAction::NeedsRevision => {
                let _ = log(
                    &format!(
                        "📝 Reviewer requested changes: {}",
                        review_status.reason.as_deref().unwrap_or("see plan.md")
                    ),
                    config,
                );

                match run_planning_implementer_revision(
                    config,
                    &planner_agent,
                    &task,
                    review_status
                        .reason
                        .as_deref()
                        .unwrap_or("See plan revisions"),
                    planning_round,
                    &paths,
                ) {
                    PlanningRevisionOutcome::AgentFailed => return false,
                    PlanningRevisionOutcome::Consensus => {
                        reached_consensus = true;
                        break;
                    }
                    PlanningRevisionOutcome::Continue { dispute_reason: dr } => {
                        dispute_reason = dr;
                    }
                }
            }
            PlanningReviewerAction::Approved => {
                if requires_dual_agent_signoff(config) {
                    // --- Adversarial planning review (dual-agent only) ---
                    if planning_adversarial_enabled(config) {
                        let first_review = read_state_file("review.md", config);
                        let _ = log(
                            "🔍 Running adversarial second review of plan...",
                            config,
                        );

                        let adversarial_output = match run_agent_with_output_or_record_error(
                            config,
                            &config.implementer,
                            &planning_adversarial_review_prompt(
                                config,
                                &task,
                                &read_state_file("plan.md", config),
                                &first_review,
                                &project_context,
                                &decisions,
                                planning_round,
                                &timestamp(),
                                &paths,
                            ),
                            Some(planning_round),
                            AgentRole::Reviewer,
                            "plan-adversarial",
                        ) {
                            Some(output) => output,
                            None => return false,
                        };

                        // Parse adversarial verdict and findings
                        let adversarial_verdict = parse_planning_verdict(&adversarial_output);
                        let adversarial_findings =
                            parse_planning_findings_from_output(&adversarial_output, planning_round);

                        let (adversarial_action, adversarial_updated) = reconcile_planning_verdict(
                            adversarial_verdict,
                            adversarial_findings,
                            &updated_findings,
                            &read_status(config),
                            planning_round,
                            config,
                        );

                        // Persist merged findings
                        let _ = crate::state::write_planning_findings(&adversarial_updated, config);

                        let adversarial_summary = format!(
                            "Adversarial: {} (verdict: {})",
                            if adversarial_action == PlanningReviewerAction::NeedsRevision {
                                "REVISE"
                            } else {
                                "APPROVED"
                            },
                            adversarial_verdict.unwrap_or("none"),
                        );
                        crate::state::append_planning_progress(
                            planning_round,
                            &adversarial_summary,
                            config,
                        );

                        if adversarial_action == PlanningReviewerAction::NeedsRevision {
                            let _ = log(
                                "📝 Adversarial reviewer found issues — requesting revision",
                                config,
                            );
                            match run_planning_implementer_revision(
                                config,
                                &planner_agent,
                                &task,
                                "Adversarial review found issues the first reviewer missed",
                                planning_round,
                                &paths,
                            ) {
                                PlanningRevisionOutcome::AgentFailed => return false,
                                PlanningRevisionOutcome::Consensus => {
                                    reached_consensus = true;
                                    break;
                                }
                                PlanningRevisionOutcome::Continue { dispute_reason: dr } => {
                                    dispute_reason = dr;
                                    continue;
                                }
                            }
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
                    let approved_plan = read_state_file("plan.md", config);
                    let signoff_timestamp = timestamp();
                    if !run_agent_or_record_error(
                        config,
                        &planner_agent,
                        &planning_implementer_signoff_prompt(
                            config,
                            &task,
                            &approved_plan,
                            planning_round,
                            &signoff_timestamp,
                            &paths,
                        ),
                        Some(planning_round),
                        AgentRole::Implementer,
                        "plan",
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
                    if let Err(err) = finalize_phase_consensus(config, planning_round, None, None) {
                        write_error_status(config, Some(planning_round), err.to_string());
                        return false;
                    }
                    reached_consensus = true;
                    break;
                }
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

fn task_decomposition_phase_internal(config: &Config, resume: bool) -> bool {
    let _ = log("━━━ Task Decomposition Phase ━━━", config);

    let task = read_state_file("task.md", config);
    let plan = read_state_file("plan.md", config);
    let paths = phase_paths(config);

    // Clear tasks_findings.json on fresh runs; preserve on resume.
    if !resume {
        crate::state::clear_tasks_findings(config);
    }

    if resume {
        let current = read_status(config);
        if current.status == Status::Consensus {
            let _ = log("✅ Task decomposition already reached consensus.", config);
            print_planning_complete_summary(&current, &task);
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
            let project_context = if config.progressive_context {
                state_manifest(config)
            } else {
                gather_project_context(
                    &config.project_dir,
                    config.effective_context_line_cap() as usize,
                    config.effective_planning_context_excerpt_lines() as usize,
                )
            };
            if !run_agent_or_record_error(
                config,
                &config.implementer,
                &decomposition_initial_prompt(&task, &plan, &project_context, &paths),
                Some(round),
                AgentRole::Implementer,
                "decompose",
            ) {
                return false;
            }
        } else {
            let current_tasks = read_state_file("tasks.md", config);
            let _ = log(
                &format!(
                    "📝 Implementer revising task breakdown (round {})...",
                    round_display(round, decomp_limit)
                ),
                config,
            );

            let implementer_prompt_timestamp = timestamp();
            if !run_agent_or_record_error(
                config,
                &config.implementer,
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
                AgentRole::Implementer,
                "decompose",
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
                "🔍 Reviewer validating task breakdown (round {})...",
                round_display(round, decomp_limit)
            ),
            config,
        );
        let tasks_text = read_state_file("tasks.md", config);

        // Read open tasks findings for the reviewer prompt.
        let tasks_findings = crate::state::read_tasks_findings(config);
        let open_findings_text = crate::state::open_tasks_findings_for_prompt(&tasks_findings);

        let reviewer_prompt_timestamp = timestamp();

        if !run_agent_or_record_error(
            config,
            &config.reviewer,
            &decomposition_reviewer_prompt(
                config,
                &plan,
                &tasks_text,
                round,
                &reviewer_prompt_timestamp,
                &paths,
                &open_findings_text,
            ),
            Some(round),
            AgentRole::Reviewer,
            "decompose",
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
                &format!("⚠️ Failed to persist tasks_findings.json: {e}"),
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

        match reconciled_decision {
            DecompositionStatusDecision::Approved => {
                if requires_dual_agent_signoff(config) {
                    // Dual-agent: require implementer signoff before finalizing.
                    let _ = log(
                        "🔍 Implementer reviewing approved task breakdown...",
                        config,
                    );
                    let signoff_tasks = read_state_file("tasks.md", config);
                    let signoff_timestamp = timestamp();
                    if !run_agent_or_record_error(
                        config,
                        &config.implementer,
                        &decomposition_implementer_signoff_prompt(
                            config,
                            &task,
                            &plan,
                            &signoff_tasks,
                            round,
                            &signoff_timestamp,
                            &paths,
                        ),
                        Some(round),
                        AgentRole::Implementer,
                        "decompose",
                    ) {
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
                        return false;
                    }

                    if signoff_status.status == Status::Consensus {
                        let _ = log("✅ Both agents agreed on task breakdown!", config);
                        print_planning_complete_summary(&signoff_status, &task);
                        return true;
                    }

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
                    if let Err(err) = finalize_phase_consensus(config, round, None, None) {
                        write_error_status(config, Some(round), err.to_string());
                        return false;
                    }
                    let final_status = read_status(config);
                    print_planning_complete_summary(&final_status, &task);
                    return true;
                }
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
    FRunAgent: FnMut(&crate::config::Agent, AgentRole, &str, &Config) -> Result<(), AgentLoopError>,
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
    // When progressive context is enabled, skip front-loaded decisions/history;
    // the state manifest is injected per-round so agents can fetch context on-demand.
    let phase_decisions = if config.progressive_context {
        String::new()
    } else {
        read_decisions(config)
    };

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
        let plan = read_state_file_fn("plan.md", config);
        let previous_review = read_state_file_fn("review.md", config);
        let previous_findings_result = read_findings_with_warnings(config);
        for warning in &previous_findings_result.warnings {
            log_fn(&format!("⚠ findings.json: {warning}"), config);
        }
        let findings_before_review =
            normalize_findings_for_round(previous_findings_result.findings_file, round);
        let open_findings_for_prompt = findings_for_prompt(&findings_before_review);

        let impl_history = if config.progressive_context {
            String::new()
        } else {
            read_history_fn(config, HISTORY_MAX_LINES)
        };

        log_fn("🔨 Implementer working...", config);
        if let Err(err) = run_agent_fn(
            &config.implementer,
            AgentRole::Implementer,
            &implementation_implementer_prompt(
                round,
                &task,
                &plan,
                &previous_review,
                &open_findings_for_prompt,
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

        let reviewer_history = if config.progressive_context {
            String::new()
        } else {
            read_history_fn(config, HISTORY_MAX_LINES)
        };

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
            &config.reviewer,
            AgentRole::Reviewer,
            &implementation_reviewer_prompt(
                config,
                &task,
                &plan,
                &changes,
                &diff,
                round,
                &reviewer_prompt_timestamp,
                &paths,
                &open_findings_for_prompt,
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

        let reviewer_findings_result = read_findings_with_warnings(config);
        for warning in &reviewer_findings_result.warnings {
            log_fn(&format!("⚠ findings.json: {warning}"), config);
        }
        let reviewer_findings = reconcile_findings_after_review(
            round,
            status.status,
            status.reason.as_deref(),
            &findings_before_review,
            reviewer_findings_result.findings_file,
        );
        if let Some(note) = reviewer_findings.log_note.as_deref() {
            log_fn(&format!("⚠ {note}"), config);
        }
        if matches!(status.status, Status::NeedsChanges | Status::Approved) {
            if let Err(err) = write_findings(&reviewer_findings.findings, config) {
                log_fn(
                    &format!("WARN: failed to write findings.json: {err}"),
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
                        reason: reviewer_findings.reason.clone(),
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
                    let findings_before_adversarial_result = read_findings_with_warnings(config);
                    for warning in &findings_before_adversarial_result.warnings {
                        log_fn(&format!("⚠ findings.json: {warning}"), config);
                    }
                    let findings_before_adversarial = normalize_findings_for_round(
                        findings_before_adversarial_result.findings_file,
                        round,
                    );

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
                        &config.reviewer,
                        AgentRole::Reviewer,
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

                    let adversarial_findings_result = read_findings_with_warnings(config);
                    for warning in &adversarial_findings_result.warnings {
                        log_fn(&format!("⚠ findings.json: {warning}"), config);
                    }
                    let adversarial_findings = reconcile_findings_after_review(
                        round,
                        adversarial_status.status,
                        adversarial_status.reason.as_deref(),
                        &findings_before_adversarial,
                        adversarial_findings_result.findings_file,
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
                                &format!("WARN: failed to write findings.json: {err}"),
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
                                    reason: adversarial_findings.reason.clone(),
                                    ..StatusPatch::default()
                                },
                                config,
                            );
                            adversarial_status = read_status_fn(config);
                        }
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
                            let open_findings = findings_for_prompt(&normalize_findings_for_round(
                                read_findings(config),
                                round,
                            ));
                            let consensus_prompt_timestamp = timestamp_fn();
                            if let Err(err) = run_agent_fn(
                                &config.implementer,
                                AgentRole::Implementer,
                                &implementation_consensus_prompt(
                                    config,
                                    &task,
                                    &plan,
                                    &review,
                                    &open_findings,
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
                } else if config.single_agent {
                    // === BRANCH 3a: Single-agent non-5/5 — auto-consensus ===
                    // Same model self-reviewing adds latency without signal.
                    // After findings reconciliation, system converts APPROVED -> CONSENSUS.
                    log_fn(
                        "🎉 Single-agent approved — auto-consensus (non-5/5)",
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
                        "AUTO-CONSENSUS (single-agent non-5/5)",
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
                } else {
                    // === BRANCH 3b: Dual-agent non-5/5 — implementer consensus flow ===
                    log_fn(
                        "🤝 Reviewer approved — checking implementer consensus...",
                        config,
                    );

                    let review = read_state_file_fn("review.md", config);
                    let open_findings = findings_for_prompt(&normalize_findings_for_round(
                        read_findings(config),
                        round,
                    ));
                    let consensus_prompt_timestamp = timestamp_fn();
                    if let Err(err) = run_agent_fn(
                        &config.implementer,
                        AgentRole::Implementer,
                        &implementation_consensus_prompt(
                            config,
                            &task,
                            &plan,
                            &review,
                            &open_findings,
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
            ..StatusPatch::default()
        },
        config,
    );
    git_checkpoint_fn("max-rounds-reached", config, baseline_files);
    record_struggle_signal(&task, &issue, round, config);
    false
}

pub fn implementation_loop(config: &Config, baseline_files: &HashSet<String>) -> bool {
    implementation_loop_internal(
        config,
        baseline_files,
        |agent: &crate::config::Agent, role: AgentRole, prompt, current_config| {
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
        |agent: &crate::config::Agent, role: AgentRole, prompt, current_config| {
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
            &mut |_agent: &crate::config::Agent, _role: AgentRole, _prompt, _config| {
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
            &mut |_agent: &crate::config::Agent, _role: AgentRole, _prompt, _config| {
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
        let config = test_config();
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
        let previous = FindingsFile::default();
        let current = FindingsFile::default();

        let result = reconcile_findings_after_review(
            2,
            Status::NeedsChanges,
            Some("missing validation"),
            &previous,
            current,
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

        let result = reconcile_findings_after_review(3, Status::Approved, None, &previous, current);

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
    fn parse_planning_verdict_extracts_approved() {
        assert_eq!(
            parse_planning_verdict("Some text\nVERDICT: APPROVED\nmore text"),
            Some("APPROVED")
        );
    }

    #[test]
    fn parse_planning_verdict_extracts_revise() {
        assert_eq!(parse_planning_verdict("VERDICT:  REVISE"), Some("REVISE"));
    }

    #[test]
    fn parse_planning_verdict_returns_none_when_absent() {
        assert_eq!(parse_planning_verdict("no verdict here"), None);
    }

    #[test]
    fn parse_planning_verdict_is_case_insensitive() {
        assert_eq!(
            parse_planning_verdict("verdict: approved"),
            Some("APPROVED")
        );
        assert_eq!(parse_planning_verdict("Verdict: revise"), Some("REVISE"));
    }

    #[test]
    fn parse_planning_findings_extracts_valid_json_block() {
        let text = r#"Here are findings:
```json
[{"id": "P-001", "description": "Missing error handling"}]
```
"#;
        let findings = parse_planning_findings_from_output(text, 2);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, "P-001");
        assert_eq!(findings[0].description, "Missing error handling");
        assert_eq!(findings[0].round_introduced, 2);
    }

    #[test]
    fn parse_planning_findings_returns_empty_for_no_json_block() {
        let text = "No JSON block here, just plain text.";
        let findings = parse_planning_findings_from_output(text, 1);
        assert!(findings.is_empty());
    }

    #[test]
    fn parse_planning_findings_returns_empty_for_invalid_json() {
        let text = "```json\n{not valid json}\n```";
        let findings = parse_planning_findings_from_output(text, 1);
        assert!(findings.is_empty());
    }

    #[test]
    fn parse_planning_findings_status_field_case_insensitive() {
        // "Resolved" (title case) → Resolved
        let text = "```json\n[{\"id\": \"P-001\", \"description\": \"Fixed\", \"status\": \"Resolved\"}]\n```\n";
        let findings = parse_planning_findings_from_output(text, 1);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].status,
            crate::state::PlanningFindingStatus::Resolved
        );

        // "RESOLVED" (upper case) → Resolved
        let text = "```json\n[{\"id\": \"P-002\", \"description\": \"Fixed\", \"status\": \"RESOLVED\"}]\n```\n";
        let findings = parse_planning_findings_from_output(text, 1);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].status,
            crate::state::PlanningFindingStatus::Resolved
        );

        // missing status field → defaults to Open
        let text = "```json\n[{\"id\": \"P-003\", \"description\": \"Still open\"}]\n```\n";
        let findings = parse_planning_findings_from_output(text, 1);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].status,
            crate::state::PlanningFindingStatus::Open
        );
    }

    #[test]
    fn reconcile_planning_verdict_revise_with_empty_findings_synthesizes() {
        let config = test_config();
        let existing = crate::state::PlanningFindingsFile::default();
        let review_status = LoopStatus {
            status: Status::NeedsRevision,
            round: 2,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "plan".to_string(),
            reason: Some("The error handling section needs work".to_string()),
            rating: None,
            timestamp: String::new(),
        };

        let (action, findings_file) = reconcile_planning_verdict(
            Some("REVISE"),
            Vec::new(), // empty findings
            &existing,
            &review_status,
            2,
            &config,
        );

        assert_eq!(action, PlanningReviewerAction::NeedsRevision);
        assert_eq!(findings_file.findings.len(), 1);
        assert_eq!(findings_file.findings[0].id, "P-001");
        assert_eq!(
            findings_file.findings[0].description,
            "The error handling section needs work"
        );
        assert_eq!(
            findings_file.findings[0].status,
            crate::state::PlanningFindingStatus::Open
        );
    }

    #[test]
    fn reconcile_planning_verdict_approved_with_open_findings_forces_needs_revision() {
        let config = test_config();
        let existing = crate::state::PlanningFindingsFile {
            findings: vec![crate::state::PlanningFindingEntry {
                id: "P-001".to_string(),
                description: "Missing error handling".to_string(),
                status: crate::state::PlanningFindingStatus::Open,
                round_introduced: 1,
                round_resolved: None,
            }],
        };
        let review_status = LoopStatus {
            status: Status::Approved,
            round: 3,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "plan".to_string(),
            reason: None,
            rating: None,
            timestamp: String::new(),
        };

        let (action, findings_file) = reconcile_planning_verdict(
            Some("APPROVED"),
            Vec::new(),
            &existing,
            &review_status,
            3,
            &config,
        );

        // Should force NEEDS_REVISION despite APPROVED verdict
        assert_eq!(action, PlanningReviewerAction::NeedsRevision);
        // Existing finding should be preserved
        assert_eq!(findings_file.findings.len(), 1);
        assert_eq!(findings_file.findings[0].id, "P-001");
    }

    #[test]
    fn reconcile_planning_verdict_approved_without_open_findings_approves() {
        let config = test_config();
        let existing = crate::state::PlanningFindingsFile {
            findings: vec![crate::state::PlanningFindingEntry {
                id: "P-001".to_string(),
                description: "Was resolved".to_string(),
                status: crate::state::PlanningFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(2),
            }],
        };
        let review_status = LoopStatus {
            status: Status::Approved,
            round: 3,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "plan".to_string(),
            reason: None,
            rating: None,
            timestamp: String::new(),
        };

        let (action, _) = reconcile_planning_verdict(
            Some("APPROVED"),
            Vec::new(),
            &existing,
            &review_status,
            3,
            &config,
        );

        assert_eq!(action, PlanningReviewerAction::Approved);
    }

    #[test]
    fn reconcile_planning_verdict_revise_with_findings_keeps_them() {
        let config = test_config();
        let existing = crate::state::PlanningFindingsFile::default();
        let new_findings = vec![crate::state::PlanningFindingEntry {
            id: "P-001".to_string(),
            description: "Explicit finding from reviewer".to_string(),
            status: crate::state::PlanningFindingStatus::Open,
            round_introduced: 2,
            round_resolved: None,
        }];
        let review_status = LoopStatus {
            status: Status::NeedsRevision,
            round: 2,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "plan".to_string(),
            reason: Some("Needs work".to_string()),
            rating: None,
            timestamp: String::new(),
        };

        let (action, findings_file) = reconcile_planning_verdict(
            Some("REVISE"),
            new_findings,
            &existing,
            &review_status,
            2,
            &config,
        );

        assert_eq!(action, PlanningReviewerAction::NeedsRevision);
        // Should keep the explicit finding, not synthesize
        assert_eq!(findings_file.findings.len(), 1);
        assert_eq!(
            findings_file.findings[0].description,
            "Explicit finding from reviewer"
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
    // PA-xxx findings reconciliation with existing P-xxx findings
    // -----------------------------------------------------------------------

    #[test]
    fn reconcile_planning_verdict_merges_pa_findings_with_existing_p_findings() {
        use crate::state::{PlanningFindingEntry, PlanningFindingStatus, PlanningFindingsFile};

        let config = test_config();

        // Existing P-xxx findings from primary reviewer
        let existing = PlanningFindingsFile {
            findings: vec![PlanningFindingEntry {
                id: "P-001".to_string(),
                description: "Missing error handling".to_string(),
                status: PlanningFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(1),
            }],
        };

        // New PA-xxx findings from adversarial reviewer
        let adversarial_findings = vec![PlanningFindingEntry {
            id: "PA-001".to_string(),
            description: "Route returns SVG not HTML".to_string(),
            status: PlanningFindingStatus::Open,
            round_introduced: 1,
            round_resolved: None,
        }];

        let review_status = LoopStatus {
            status: Status::NeedsRevision,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "plan".to_string(),
            reason: Some("Adversarial review found issues".to_string()),
            rating: None,
            timestamp: String::new(),
        };

        let (action, merged) = reconcile_planning_verdict(
            Some("REVISE"),
            adversarial_findings,
            &existing,
            &review_status,
            1,
            &config,
        );

        assert_eq!(action, PlanningReviewerAction::NeedsRevision);
        // Both P-001 (resolved) and PA-001 (open) should be in merged findings
        assert_eq!(merged.findings.len(), 2);
        assert!(merged.findings.iter().any(|f| f.id == "P-001"));
        assert!(merged.findings.iter().any(|f| f.id == "PA-001"));
        let pa = merged.findings.iter().find(|f| f.id == "PA-001").unwrap();
        assert_eq!(pa.status, PlanningFindingStatus::Open);
    }

    #[test]
    fn reconcile_planning_verdict_adversarial_approved_with_no_open_findings() {
        use crate::state::{PlanningFindingEntry, PlanningFindingStatus, PlanningFindingsFile};

        let config = test_config();

        // All existing findings resolved
        let existing = PlanningFindingsFile {
            findings: vec![PlanningFindingEntry {
                id: "P-001".to_string(),
                description: "Missing error handling".to_string(),
                status: PlanningFindingStatus::Resolved,
                round_introduced: 1,
                round_resolved: Some(1),
            }],
        };

        let review_status = LoopStatus {
            status: Status::Approved,
            round: 1,
            implementer: "claude".to_string(),
            reviewer: "codex".to_string(),
            mode: "dual-agent".to_string(),
            last_run_task: "plan".to_string(),
            reason: None,
            rating: None,
            timestamp: String::new(),
        };

        // Adversarial reviewer found no issues
        let (action, _) = reconcile_planning_verdict(
            Some("APPROVED"),
            vec![],
            &existing,
            &review_status,
            1,
            &config,
        );

        assert_eq!(action, PlanningReviewerAction::Approved);
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
        let prompt = decomposition_reviewer_prompt(
            &config,
            "plan",
            "tasks",
            1,
            "2026-01-01T00:00:00.000Z",
            &paths,
            "",
        );
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
                rating: Some(2),
                ..crate::state::StatusPatch::default()
            },
            &config,
        )
        .expect("seed status should be written");

        finalize_phase_consensus(&config, 3, Some("finalized".to_string()), Some(4))
            .expect("finalize_phase_consensus should succeed");

        let status = crate::state::read_status(&config);
        assert_eq!(status.status, Status::Consensus);
        assert_eq!(status.round, 3);
        assert_eq!(status.reason.as_deref(), Some("finalized"));
        assert_eq!(status.rating, Some(4));
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
        let prompt = decomposition_reviewer_prompt(
            &config,
            "plan",
            "tasks",
            1,
            "2026-01-01T00:00:00.000Z",
            &paths,
            "",
        );
        // Should request structured findings with T- prefix IDs
        assert!(prompt.contains("T-001"));
        assert!(prompt.contains("\"status\": \"open\""));
        assert!(prompt.contains("\"status\": \"resolved\""));
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

        // Simulate: implementer produces changes, reviewer approves with rating 3 (non-5/5).
        let status_sequence = std::cell::RefCell::new(vec![
            // read_status after reviewer: Approved, rating 3
            LoopStatus {
                status: Status::Approved,
                round: 1,
                implementer: "claude".to_string(),
                reviewer: "claude".to_string(),
                mode: "single-agent".to_string(),
                last_run_task: "test".to_string(),
                reason: None,
                rating: Some(3),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            },
        ]);

        let status_call_count = std::cell::RefCell::new(0usize);

        implementation_loop_internal(
            &config,
            &baseline,
            |_agent, role, _prompt, _cfg| {
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
                        rating: Some(3),
                        timestamp: "now".to_string(),
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
}
