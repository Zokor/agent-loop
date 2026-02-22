use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;

const EXCLUDED_DIRS: &[&str] = &[".git", "target", "node_modules", ".agent-loop"];

/// Build a project structure overview for injection into planning prompts.
///
/// Walks the directory tree up to depth 2, excludes common noise directories,
/// and appends the first `excerpt_max_lines` lines of README.md / CLAUDE.md when present.
/// Total output is capped at `line_cap` lines.
///
/// `line_cap` and `excerpt_max_lines` correspond to the Config fields
/// `context_line_cap` and `planning_context_excerpt_lines`.
pub(crate) fn gather_project_context(
    project_dir: &Path,
    line_cap: usize,
    excerpt_max_lines: usize,
) -> String {
    if !project_dir.is_dir() {
        return String::new();
    }

    let mut lines: Vec<String> = vec!["PROJECT STRUCTURE:".to_string()];

    // Collect the tree (depth 0 = direct children, depth 1 = grandchildren).
    collect_tree(project_dir, 0, &mut lines, line_cap);

    // Append README.md excerpt if budget remains.
    append_file_excerpt(
        project_dir,
        "README.md",
        &mut lines,
        line_cap,
        excerpt_max_lines,
    );
    // Append CLAUDE.md excerpt if budget remains.
    append_file_excerpt(
        project_dir,
        "CLAUDE.md",
        &mut lines,
        line_cap,
        excerpt_max_lines,
    );
    // Append additional optional repository guides if budget remains.
    append_file_excerpt(
        project_dir,
        "ARCHITECTURE.md",
        &mut lines,
        line_cap,
        excerpt_max_lines,
    );
    append_file_excerpt(
        project_dir,
        "CONVENTIONS.md",
        &mut lines,
        line_cap,
        excerpt_max_lines,
    );
    append_file_excerpt(
        project_dir,
        "AGENTS.md",
        &mut lines,
        line_cap,
        excerpt_max_lines,
    );

    // Enforce the global line cap (the header counts as line 1).
    lines.truncate(line_cap);

    lines.join("\n")
}

/// Recursively collect directory entries into `lines` as an indented tree.
/// `depth` starts at 0 for the project root's immediate children and goes up to 1
/// (i.e. two levels below the project root).
fn collect_tree(dir: &Path, depth: usize, lines: &mut Vec<String>, line_cap: usize) {
    if lines.len() >= line_cap {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());

    let indent = "  ".repeat(depth);

    for entry in entries {
        if lines.len() >= line_cap {
            return;
        }

        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        let is_dir = entry.file_type().is_ok_and(|ft| ft.is_dir());

        if is_dir && EXCLUDED_DIRS.contains(&name.as_ref()) {
            continue;
        }

        if is_dir {
            lines.push(format!("{indent}{name}/"));
            if depth < 1 {
                collect_tree(&entry.path(), depth + 1, lines, line_cap);
            }
        } else {
            lines.push(format!("{indent}{name}"));
        }
    }
}

/// Append the first `excerpt_max_lines` lines of a file as a labeled section.
/// Lines count toward the global budget tracked by `lines.len()`.
fn append_file_excerpt(
    project_dir: &Path,
    filename: &str,
    lines: &mut Vec<String>,
    line_cap: usize,
    excerpt_max_lines: usize,
) {
    if lines.len() >= line_cap {
        return;
    }

    let file_path = project_dir.join(filename);
    let Ok(content) = fs::read_to_string(&file_path) else {
        return;
    };

    // Blank separator + header consume 2 lines.
    if lines.len() + 2 >= line_cap {
        return;
    }

    lines.push(String::new());
    lines.push(format!("{filename} (first {excerpt_max_lines} lines):"));

    let remaining = line_cap.saturating_sub(lines.len());
    let max_excerpt = remaining.min(excerpt_max_lines);

    for line in content.lines().take(max_excerpt) {
        lines.push(line.to_string());
    }
}

/// Build a compact state manifest pointing agents to available context files.
/// Used in progressive-context mode instead of front-loading the full project context.
pub(crate) fn state_manifest(config: &Config) -> String {
    let root = &config.project_dir;
    let state = &config.state_dir;

    let mut lines = vec!["AVAILABLE CONTEXT (explore files on-demand as needed):".to_string()];
    lines.push(format!(
        "- Project root: {} -- explore structure and source files",
        root.display()
    ));

    let doc_files: &[(&str, &str)] = &[
        ("README.md", "project documentation"),
        ("CLAUDE.md", "project instructions for AI"),
        ("AGENTS.md", "agent conventions & guidelines"),
        (".agent-loop/decisions.md", "prior decisions & learnings"),
    ];

    for (relative, description) in doc_files {
        let full = root.join(relative);
        if full.exists() {
            lines.push(format!("- {relative}: {} -- {description}", full.display()));
        }
    }

    let state_files: &[(&str, &str)] = &[
        ("conversation.md", "round history"),
        ("plan.md", "agreed development plan"),
        ("tasks.md", "task breakdown"),
    ];

    for (filename, description) in state_files {
        let full = state.join(filename);
        if full.exists() {
            lines.push(format!("- {filename}: {} -- {description}", full.display()));
        }
    }

    lines.join("\n")
}

const SINGLE_AGENT_REVIEWER_PREAMBLE: &str = "⚠️ SINGLE-AGENT REVIEWER MODE ⚠️
You are now switching roles from IMPLEMENTER to REVIEWER. You must adopt a completely independent, critical perspective.

CRITICAL INSTRUCTIONS:
- You MUST evaluate the work as if you did NOT write it.
- Do NOT assume correctness because the code \"looks familiar.\"
- Actively look for bugs, edge cases, missing tests, and design flaws.
- Apply the same scrutiny you would to a junior developer's first PR.
- If something is unclear or under-tested, flag it — do not give benefit of the doubt.
- Your approval carries weight: a false approval means bugs ship to production.

";

const DECISION_CAPTURE_INSTRUCTIONS: &str =
    "DECISION CAPTURE: If you make an important architectural decision, discover a constraint,
choose a reusable pattern, hit a gotcha, or identify a key dependency — append a one-line
entry to `.agent-loop/decisions.md` with format:
- [CATEGORY] description
where CATEGORY is one of: ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY";

fn prior_decisions_section(decisions: &str) -> String {
    if decisions.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n\nPRIOR DECISIONS & LEARNINGS (from previous sessions — respect these):\n{decisions}"
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhasePaths {
    pub(crate) task_md: PathBuf,
    pub(crate) plan_md: PathBuf,
    pub(crate) tasks_md: PathBuf,
    pub(crate) changes_md: PathBuf,
    pub(crate) review_md: PathBuf,
    pub(crate) findings_json: PathBuf,
    pub(crate) status_json: PathBuf,
}

pub(crate) fn phase_paths(config: &Config) -> PhasePaths {
    PhasePaths {
        task_md: config.state_dir.join("task.md"),
        plan_md: config.state_dir.join("plan.md"),
        tasks_md: config.state_dir.join("tasks.md"),
        changes_md: config.state_dir.join("changes.md"),
        review_md: config.state_dir.join("review.md"),
        findings_json: config.state_dir.join("findings.json"),
        status_json: config.state_dir.join("status.json"),
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(crate) fn single_agent_reviewer_preamble(config: &Config) -> String {
    if !config.single_agent {
        return String::new();
    }

    SINGLE_AGENT_REVIEWER_PREAMBLE.to_string()
}

/// Role-specific system prompt for `--append-system-prompt`.
///
/// Returns `None` when there is nothing role-specific to inject (the default
/// instructions already live in the user prompt for backward compatibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentRole {
    Implementer,
    Reviewer,
    Planner,
}

pub(crate) fn system_prompt_for_role(role: AgentRole, config: &Config) -> String {
    let mut parts: Vec<String> = Vec::new();

    parts.push(DECISION_CAPTURE_INSTRUCTIONS.to_string());

    if role == AgentRole::Reviewer && config.single_agent {
        parts.push(SINGLE_AGENT_REVIEWER_PREAMBLE.to_string());
    }

    // When progressive context is enabled, append the state manifest so the agent
    // knows which files are available and can read them on-demand.
    if config.progressive_context {
        let manifest = state_manifest(config);
        if !manifest.is_empty() {
            parts.push(manifest);
        }
    }

    parts.join("\n\n")
}

pub(crate) fn planning_initial_prompt(
    task: &str,
    project_context: &str,
    decisions: &str,
    paths: &PhasePaths,
    planner_plan_mode: bool,
) -> String {
    let context_section = if project_context.is_empty() {
        String::new()
    } else {
        format!("\n{project_context}\n")
    };
    let decisions_section = prior_decisions_section(decisions);
    let output_instruction = if planner_plan_mode {
        "Output your plan below between <plan> and </plan> markers.".to_string()
    } else {
        format!("Write your plan to the file: {}", path_text(&paths.plan_md))
    };

    format!(
        "You are the IMPLEMENTER in a collaborative development loop.\n{context_section}\nRead the task below and propose a detailed development plan.\n\nTASK:\n{task}{decisions_section}\n\n{output_instruction}\n\nYour plan should include:\n- Overview of approach\n- Step-by-step implementation strategy\n- Files to create/modify\n- Key technical decisions\n- Testing strategy"
    )
}

/// Planning reviewer prompt parameters bundled to satisfy clippy::too_many_arguments.
pub(crate) struct PlanningReviewerParams<'a> {
    pub config: &'a Config,
    pub task: &'a str,
    pub plan: &'a str,
    pub round: u32,
    pub prompt_timestamp: &'a str,
    pub paths: &'a PhasePaths,
    pub dispute_reason: Option<&'a str>,
    pub open_findings: &'a str,
}

pub(crate) fn planning_reviewer_prompt(params: &PlanningReviewerParams<'_>) -> String {
    let PlanningReviewerParams {
        config,
        task,
        plan,
        round,
        prompt_timestamp,
        paths,
        dispute_reason,
        open_findings,
    } = params;

    let concerns_section = match dispute_reason {
        Some(reason) if !reason.trim().is_empty() => {
            format!("\n\nIMPLEMENTER'S CONCERNS:\n{reason}")
        }
        _ => String::new(),
    };

    let findings_section = if open_findings.is_empty() {
        String::new()
    } else {
        format!("\n\nOPEN PLANNING FINDINGS (address or resolve each):\n{open_findings}")
    };

    format!(
        "{preamble}You are the REVIEWER in a collaborative development loop.\n\nReview this development plan against the original task.\n\nTASK:\n{task}\n\nPROPOSED PLAN:\n{plan}{concerns_section}{findings_section}\n\nStructure your review using these sections:\n\n## Completeness\nDoes the plan fully address all requirements in the task?\n\n## Feasibility\nIs the plan technically feasible? Are the proposed approaches sound?\n\n## Risks\nWhat risks, gaps, or potential issues exist?\n\n## Findings\nList specific issues as a JSON block (use IDs like P-001, P-002, ...):\n```json\n[{{\"id\": \"P-001\", \"description\": \"issue description\", \"status\": \"open\"}}]\n```\nUse `\"status\": \"resolved\"` for previously-raised issues that have been fully addressed. Use `\"status\": \"open\"` for issues that still need attention. Omit the block entirely if there are no findings at all.\n\n## Verdict\nEnd your review with exactly one of:\n  VERDICT: APPROVED\n  VERDICT: REVISE\n\nIf you approve the plan, write this exact JSON to {status_path}:\n{{\"status\": \"APPROVED\", \"round\": {round}, \"implementer\": \"{impl_name}\", \"reviewer\": \"{rev_name}\", \"mode\": \"{mode}\", \"timestamp\": \"{prompt_timestamp}\"}}\n\nIf changes are needed, write your revised plan to {plan_path} and write this JSON to {status_path2}:\n{{\"status\": \"NEEDS_REVISION\", \"round\": {round}, \"implementer\": \"{impl_name2}\", \"reviewer\": \"{rev_name2}\", \"mode\": \"{mode2}\", \"reason\": \"your reason here\", \"timestamp\": \"{prompt_timestamp}\"}}",
        preamble = single_agent_reviewer_preamble(config),
        status_path = path_text(&paths.status_json),
        impl_name = config.implementer,
        rev_name = config.reviewer,
        mode = config.run_mode,
        plan_path = path_text(&paths.plan_md),
        status_path2 = path_text(&paths.status_json),
        impl_name2 = config.implementer,
        rev_name2 = config.reviewer,
        mode2 = config.run_mode,
    )
}

pub(crate) fn planning_implementer_revision_prompt(
    config: &Config,
    task: &str,
    revised_plan: &str,
    reviewer_reason: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "The reviewer has revised your plan. Review their changes.\n\nORIGINAL TASK:\n{task}\n\nREVISED PLAN:\n{revised_plan}\n\nREVIEWER'S REASON:\n{reviewer_reason}\n\nIf you agree with the revisions, write this JSON to {}:\n{{\"status\": \"CONSENSUS\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"timestamp\": \"{prompt_timestamp}\"}}\n\nIf you want to make further changes, revise the plan in {} and write:\n{{\"status\": \"DISPUTED\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"reason\": \"your concerns\", \"timestamp\": \"{prompt_timestamp}\"}}",
        path_text(&paths.status_json),
        config.implementer,
        config.reviewer,
        config.run_mode,
        path_text(&paths.plan_md),
        config.implementer,
        config.reviewer,
        config.run_mode,
    )
}

pub(crate) fn decomposition_initial_prompt(
    task: &str,
    plan: &str,
    project_context: &str,
    paths: &PhasePaths,
) -> String {
    let context_section = if project_context.is_empty() {
        String::new()
    } else {
        format!("\n{project_context}\n")
    };

    format!(
        "You are the IMPLEMENTER in a collaborative development loop.\n{context_section}\nBoth agents have agreed on the following development plan. Your job now is to break it down into discrete, implementable tasks.\n\nORIGINAL TASK:\n{task}\n\nAGREED PLAN:\n{plan}\n\nCreate a task breakdown file at {} with the following structure:\n\n# Implementation Tasks\n\nFor each task, include:\n- Task number and title\n- Brief description (2-3 sentences)\n- Estimated complexity (Low/Medium/High)\n- Dependencies (which tasks must complete first)\n- Key deliverables\n- Testing requirements\n\nGuidelines:\n- Each task should be completable in a single implementation session (4-8 hours of work)\n- Break large features into smaller incremental tasks\n- Ensure tasks have clear success criteria\n- Order tasks by dependencies (foundational work first)\n- Include verification/testing as separate tasks if needed",
        path_text(&paths.tasks_md)
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn decomposition_revision_prompt(
    config: &Config,
    task: &str,
    plan: &str,
    current_tasks: &str,
    reviewer_feedback: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "You are the IMPLEMENTER revising a task breakdown.\n\nORIGINAL TASK:\n{task}\n\nAGREED PLAN:\n{plan}\n\nCURRENT TASKS:\n{current_tasks}\n\nREVIEWER FEEDBACK:\n{reviewer_feedback}\n\nRevise {} to address all reviewer feedback while preserving clear dependencies and testing requirements.\n\nThen write this JSON to {}:\n{{\"status\": \"DISPUTED\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"timestamp\": \"{prompt_timestamp}\"}}",
        path_text(&paths.tasks_md),
        path_text(&paths.status_json),
        config.implementer,
        config.reviewer,
        config.run_mode,
    )
}

pub(crate) fn decomposition_reviewer_prompt(
    config: &Config,
    plan: &str,
    tasks: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
    open_findings: &str,
) -> String {
    let findings_section = if open_findings.is_empty() {
        String::new()
    } else {
        format!("\n\nOPEN TASKS FINDINGS (address or resolve each):\n{open_findings}")
    };

    format!(
        "{preamble}You are the REVIEWER in a collaborative development loop.\n\nThe implementer has broken down the agreed plan into discrete tasks. Review the task breakdown for:\n\nAGREED PLAN:\n{plan}\n\nPROPOSED TASKS:\n{tasks}{findings_section}\n\nReview criteria:\n1. Does each task have clear scope and deliverables?\n2. Are task sizes reasonable (not too large, not too small)?\n3. Are dependencies correctly identified?\n4. Is the task order logical?\n5. Are there any missing tasks?\n6. Are testing/verification steps included?\n\n## Findings\nList specific issues as a JSON block in {review_path} (use IDs like T-001, T-002, ...):\n```json\n[{{\"id\": \"T-001\", \"description\": \"issue description\", \"status\": \"open\"}}]\n```\nUse `\"status\": \"resolved\"` for previously-raised issues that have been fully addressed. Use `\"status\": \"open\"` for issues that still need attention. Omit the block entirely if there are no findings at all.\n\nIf you approve the breakdown, write this JSON to {status_path}:\n{{\"status\": \"APPROVED\", \"round\": {round}, \"implementer\": \"{impl_name}\", \"reviewer\": \"{rev_name}\", \"mode\": \"{mode}\", \"timestamp\": \"{prompt_timestamp}\"}}\n\nIf changes are needed, revise the task list in {tasks_path} and write:\n{{\"status\": \"NEEDS_REVISION\", \"round\": {round}, \"implementer\": \"{impl_name2}\", \"reviewer\": \"{rev_name2}\", \"mode\": \"{mode2}\", \"reason\": \"brief explanation\", \"timestamp\": \"{prompt_timestamp}\"}}",
        preamble = single_agent_reviewer_preamble(config),
        review_path = path_text(&paths.review_md),
        status_path = path_text(&paths.status_json),
        impl_name = config.implementer,
        rev_name = config.reviewer,
        mode = config.run_mode,
        tasks_path = path_text(&paths.tasks_md),
        impl_name2 = config.implementer,
        rev_name2 = config.reviewer,
        mode2 = config.run_mode,
    )
}

pub(crate) fn decomposition_implementer_signoff_prompt(
    config: &Config,
    task: &str,
    plan: &str,
    tasks: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "The reviewer has APPROVED the task breakdown. Review the tasks against the original plan and task.\n\nORIGINAL TASK:\n{task}\n\nAGREED PLAN:\n{plan}\n\nAPPROVED TASKS:\n{tasks}\n\nIf you agree with the task breakdown, write this JSON to {}:\n{{\"status\": \"CONSENSUS\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"timestamp\": \"{prompt_timestamp}\"}}\n\nIf you want to request changes, write:\n{{\"status\": \"DISPUTED\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"reason\": \"your concerns\", \"timestamp\": \"{prompt_timestamp}\"}}",
        path_text(&paths.status_json),
        config.implementer,
        config.reviewer,
        config.run_mode,
        config.implementer,
        config.reviewer,
        config.run_mode,
    )
}

pub(crate) fn planning_implementer_signoff_prompt(
    config: &Config,
    task: &str,
    plan: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "The reviewer has APPROVED your plan. Perform a final review before confirming.\n\nORIGINAL TASK:\n{task}\n\nAPPROVED PLAN:\n{plan}\n\nIf you agree with the plan, write this JSON to {}:\n{{\"status\": \"CONSENSUS\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"timestamp\": \"{prompt_timestamp}\"}}\n\nIf you want to request changes, write:\n{{\"status\": \"DISPUTED\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"reason\": \"your concerns\", \"timestamp\": \"{prompt_timestamp}\"}}",
        path_text(&paths.status_json),
        config.implementer,
        config.reviewer,
        config.run_mode,
        config.implementer,
        config.reviewer,
        config.run_mode,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn implementation_implementer_prompt(
    round: u32,
    task: &str,
    plan: &str,
    previous_review: &str,
    open_findings: &str,
    decisions: &str,
    paths: &PhasePaths,
    round_history: &str,
) -> String {
    let review_section = if previous_review.trim().is_empty() {
        "This is the first implementation round.".to_string()
    } else {
        format!("PREVIOUS REVIEW FEEDBACK (address all issues):\n{previous_review}")
    };

    let history_section = if round_history.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nROUND HISTORY:\n{round_history}")
    };
    let findings_section = if open_findings.trim().is_empty() {
        "OPEN FINDINGS:\nNone.".to_string()
    } else {
        format!("OPEN FINDINGS (resolve every item before approval):\n{open_findings}")
    };
    let decisions_section = prior_decisions_section(decisions);

    format!(
        "You are the IMPLEMENTER in round {round} of a collaborative development loop.\n\nTASK:\n{task}\n\nPLAN:\n{plan}{history_section}{decisions_section}\n\n{review_section}\n\n{findings_section}\n\nImplement the plan. When done, write a summary of all changes to: {}\n\nFocus on:\n- Writing clean, well-structured code\n- Following the plan closely\n- Addressing all review feedback from previous rounds\n- Resolving every open finding ID\n- Adding tests where appropriate\n\n{DECISION_CAPTURE_INSTRUCTIONS}",
        path_text(&paths.changes_md)
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn implementation_reviewer_prompt(
    config: &Config,
    task: &str,
    plan: &str,
    changes: &str,
    diff: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
    open_findings: &str,
    quality_checks: Option<&str>,
    decisions: &str,
    round_history: &str,
) -> String {
    let quality_section = match quality_checks {
        Some(checks) if !checks.trim().is_empty() => format!("\n\n{checks}"),
        _ => String::new(),
    };

    let history_section = if round_history.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nROUND HISTORY:\n{round_history}")
    };
    let open_findings_section = if open_findings.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n\nOPEN FINDINGS FROM PREVIOUS ROUND (keep IDs for still-open issues):\n{open_findings}"
        )
    };
    let decisions_section = prior_decisions_section(decisions);

    format!(
        "{}You are the REVIEWER in round {round} of a collaborative development loop.\n\nTASK:\n{task}\n\nPLAN:\n{plan}{decisions_section}\n\nCHANGES SUMMARY:\n{changes}\n\nACTUAL CODE DIFF:\n{diff}{quality_section}{history_section}{open_findings_section}\n\nReview the ACTUAL code changes shown in the diff above (not just the summary).\n\nStructure your review using these sections:\n\n## Correctness\nDoes the code match the plan? Are there bugs or edge cases?\n\n## Tests\nAre tests present, sufficient, and covering key scenarios?\n\n## Style\nIs the code clean, maintainable, and following project conventions?\n\n## Security\nAre there security concerns? Is error handling adequate?\n\n## Findings\nList unresolved issues using IDs like F-001, F-002. Include severity and file refs.\n\n## Verdict\nAPPROVE or REQUEST CHANGES — with a quality rating (1-5) and brief justification.\n\nWrite your detailed review to: {}\n\nAlso write structured findings JSON to {}:\n\nIf APPROVED (no unresolved issues):\n{{\"round\": {round}, \"findings\": []}}\n\nIf CHANGES NEEDED:\n{{\"round\": {round}, \"findings\": [{{\"id\": \"F-001\", \"severity\": \"HIGH\", \"summary\": \"what is wrong\", \"file_refs\": [\"src/file.rs:42\"]}}]}}\n\nRules for findings.json:\n- Include every unresolved issue in the findings array.\n- Keep IDs stable for issues that remain unresolved across rounds.\n- Do not mark APPROVED when findings is non-empty.\n\nInclude a quality rating from 1-5 in your status JSON:\n  1 = poor (major bugs, missing tests, does not follow plan)\n  2 = below average (significant issues or gaps)\n  3 = acceptable (works but has notable issues)\n  4 = good (solid implementation, minor issues only)\n  5 = excellent (clean, well-tested, follows plan precisely)\n\nThen write one of these to {}:\n\nIf APPROVED:\n{{\"status\": \"APPROVED\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"rating\": 4, \"timestamp\": \"{prompt_timestamp}\"}}\n\nIf CHANGES NEEDED:\n{{\"status\": \"NEEDS_CHANGES\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"rating\": 2, \"reason\": \"brief summary\", \"timestamp\": \"{prompt_timestamp}\"}}\n\n{DECISION_CAPTURE_INSTRUCTIONS}",
        single_agent_reviewer_preamble(config),
        path_text(&paths.review_md),
        path_text(&paths.findings_json),
        path_text(&paths.status_json),
        config.implementer,
        config.reviewer,
        config.run_mode,
        config.implementer,
        config.reviewer,
        config.run_mode,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn implementation_consensus_prompt(
    config: &Config,
    task: &str,
    plan: &str,
    review: &str,
    open_findings: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    let findings_section = if open_findings.trim().is_empty() {
        "OPEN FINDINGS:\nNone.".to_string()
    } else {
        format!("OPEN FINDINGS:\n{open_findings}")
    };

    format!(
        "The reviewer has APPROVED your implementation. Before confirming consensus,
perform your own final review:

1. Re-read the TASK requirements below — verify every requirement is met
2. Check for edge cases, error handling gaps, or missing tests the reviewer may have overlooked
3. Verify the code follows project conventions and the agreed plan
4. Look for any regressions or unintended side effects

TASK:
{task}

PLAN:
{plan}

REVIEW:
{review}

{findings_section}

Write a brief summary of your self-review findings.

If everything checks out, write CONSENSUS [JSON].
If you find issues the reviewer missed, write DISPUTED [JSON] with specific
details of what was missed.

CONSENSUS [JSON] to {}:
{{\"status\": \"CONSENSUS\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"timestamp\": \"{prompt_timestamp}\"}}

DISPUTED [JSON] to {}:
{{\"status\": \"DISPUTED\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"reason\": \"what was missed\", \"timestamp\": \"{prompt_timestamp}\"}}",
        path_text(&paths.status_json),
        config.implementer,
        config.reviewer,
        config.run_mode,
        path_text(&paths.status_json),
        config.implementer,
        config.reviewer,
        config.run_mode,
    )
}

pub(crate) fn compound_prompt(task: &str, plan: &str) -> String {
    format!(
        "You are the IMPLEMENTER running a post-consensus compound reflection phase.

Review the full session: task goals, plan, implementation rounds, review feedback,
and any struggles encountered. Extract only reusable, cross-session learnings.
Do not record one-off or session-specific details.

TASK:
{task}

PLAN:
{plan}

Append each learning to `.agent-loop/decisions.md` using one line per entry:
- [CATEGORY] description

Allowed categories: ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY."
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn implementation_adversarial_review_prompt(
    config: &Config,
    task: &str,
    plan: &str,
    changes: &str,
    diff: &str,
    first_review: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
    quality_checks: Option<&str>,
) -> String {
    let quality_section = match quality_checks {
        Some(checks) if !checks.trim().is_empty() => format!("\n\n{checks}"),
        _ => String::new(),
    };

    format!(
        "{}You are a SECOND REVIEWER performing an adversarial review in round {round}.\n\n\
        A first reviewer has already approved this implementation with a perfect 5/5 rating. \
        Your job is to find what the first reviewer missed. Look for subtle bugs, edge cases, \
        missing error handling, security issues, insufficient tests, or design flaws that a \
        favorable first review might overlook.\n\n\
        IMPORTANT GUARDRAIL: If you find no meaningful issues after thorough analysis, then \
        APPROVED is the correct verdict. Do not manufacture issues or nitpick for the sake of \
        finding problems.\n\n\
        TASK:\n{task}\n\n\
        PLAN:\n{plan}\n\n\
        CHANGES SUMMARY:\n{changes}\n\n\
        ACTUAL CODE DIFF:\n{diff}\n\n\
        FIRST REVIEWER'S ASSESSMENT:\n{first_review}{quality_section}\n\n\
        Focus your adversarial review on:\n\
        1. Bugs or edge cases the first reviewer may have overlooked\n\
        2. Missing or insufficient test coverage\n\
        3. Security vulnerabilities or unsafe patterns\n\
        4. Deviations from the plan that weren't flagged\n\
        5. Error handling gaps or silent failures\n\n\
        Write your detailed review to: {}\n\n\
        Also write structured findings JSON to {}:\n\
        - If APPROVED: {{\"round\": {round}, \"findings\": []}}\n\
        - If CHANGES NEEDED: {{\"round\": {round}, \"findings\": [{{\"id\": \"F-001\", \"severity\": \"HIGH\", \"summary\": \"what was missed\", \"file_refs\": [\"src/file.rs:42\"]}}]}}\n\
        Keep finding IDs stable when issues remain unresolved.\n\n\
        Include a quality rating from 1-5 in your status JSON:\n  \
        1 = poor (major bugs, missing tests, does not follow plan)\n  \
        2 = below average (significant issues or gaps)\n  \
        3 = acceptable (works but has notable issues)\n  \
        4 = good (solid implementation, minor issues only)\n  \
        5 = excellent (clean, well-tested, follows plan precisely)\n\n\
        Then write one of these to {}:\n\n\
        If APPROVED (no meaningful issues found):\n\
        {{\"status\": \"APPROVED\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"rating\": 5, \"timestamp\": \"{prompt_timestamp}\"}}\n\n\
        If CHANGES NEEDED (found issues the first reviewer missed):\n\
        {{\"status\": \"NEEDS_CHANGES\", \"round\": {round}, \"implementer\": \"{}\", \"reviewer\": \"{}\", \"mode\": \"{}\", \"rating\": 3, \"reason\": \"brief summary of missed issues\", \"timestamp\": \"{prompt_timestamp}\"}}",
        single_agent_reviewer_preamble(config),
        path_text(&paths.review_md),
        path_text(&paths.findings_json),
        path_text(&paths.status_json),
        config.implementer,
        config.reviewer,
        config.run_mode,
        config.implementer,
        config.reviewer,
        config.run_mode,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_CONTEXT_LINE_CAP, DEFAULT_PLANNING_CONTEXT_EXCERPT_LINES};
    use crate::test_support::unique_temp_path;
    use std::fs;

    const LINE_CAP: usize = DEFAULT_CONTEXT_LINE_CAP as usize;
    const EXCERPT_MAX_LINES: usize = DEFAULT_PLANNING_CONTEXT_EXCERPT_LINES as usize;

    fn make_temp_dir() -> PathBuf {
        let dir = unique_temp_path("prompts_test");
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    /// Helper to clean up temp dirs after test (best-effort).
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn gather_project_context_builds_depth_two_tree() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        // Depth 0 files
        fs::write(dir.join("Cargo.toml"), "").unwrap();
        fs::write(dir.join("main.rs"), "").unwrap();

        // Depth 0 directory with depth 1 contents
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();
        fs::write(dir.join("src/main.rs"), "").unwrap();

        // Depth 1 directory with depth 2 contents (should NOT appear)
        fs::create_dir_all(dir.join("src/utils")).unwrap();
        fs::write(dir.join("src/utils/helper.rs"), "").unwrap();

        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);

        assert!(output.starts_with("PROJECT STRUCTURE:"));
        assert!(output.contains("Cargo.toml"));
        assert!(output.contains("main.rs"));
        assert!(output.contains("src/"));
        assert!(output.contains("  lib.rs"));
        assert!(output.contains("  main.rs"));
        // src/utils/ should appear (it's at depth 1), but its contents should not
        assert!(output.contains("  utils/"));
        assert!(
            !output.contains("    helper.rs"),
            "depth-2 file contents should not appear"
        );
    }

    #[test]
    fn gather_project_context_excludes_known_directories() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::create_dir_all(dir.join(".agent-loop")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();

        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);

        assert!(!output.contains(".git"));
        assert!(!output.contains("target/"));
        assert!(!output.contains("node_modules/"));
        assert!(!output.contains(".agent-loop"));
        assert!(output.contains("src/"));
    }

    #[test]
    fn gather_project_context_caps_at_200_lines() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        // Create 250 files at the root level to exceed the 200 line budget.
        for i in 0..250 {
            fs::write(dir.join(format!("file_{i:04}.txt")), "").unwrap();
        }

        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);
        let line_count = output.lines().count();

        assert!(
            line_count <= LINE_CAP,
            "output should be capped at {LINE_CAP} lines, got {line_count}"
        );
    }

    #[test]
    fn gather_project_context_includes_readme_excerpt() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        // Create a README.md with 150 lines (only first 100 should appear).
        let readme_lines: Vec<String> = (1..=150).map(|i| format!("readme line {i}")).collect();
        fs::write(dir.join("README.md"), readme_lines.join("\n")).unwrap();

        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);

        assert!(output.contains("README.md (first 100 lines):"));
        assert!(output.contains("readme line 1"));
        assert!(output.contains("readme line 100"));
        assert!(!output.contains("readme line 101"));
    }

    #[test]
    fn gather_project_context_includes_claude_md_excerpt() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        fs::write(dir.join("CLAUDE.md"), "agent conventions here\nsecond line").unwrap();

        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);

        assert!(output.contains("CLAUDE.md (first 100 lines):"));
        assert!(output.contains("agent conventions here"));
        assert!(output.contains("second line"));
    }

    #[test]
    fn gather_project_context_includes_additional_repository_guides() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        fs::write(dir.join("README.md"), "readme line").unwrap();
        fs::write(dir.join("CLAUDE.md"), "claude line").unwrap();
        fs::write(dir.join("ARCHITECTURE.md"), "architecture line").unwrap();
        fs::write(dir.join("CONVENTIONS.md"), "conventions line").unwrap();
        fs::write(dir.join("AGENTS.md"), "agents line").unwrap();

        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);
        assert!(output.contains("README.md (first 100 lines):"));
        assert!(output.contains("CLAUDE.md (first 100 lines):"));
        assert!(output.contains("ARCHITECTURE.md (first 100 lines):"));
        assert!(output.contains("CONVENTIONS.md (first 100 lines):"));
        assert!(output.contains("AGENTS.md (first 100 lines):"));
    }

    #[test]
    fn gather_project_context_skips_missing_additional_repository_guides() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        fs::write(dir.join("README.md"), "readme line").unwrap();
        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);

        assert!(output.contains("README.md (first 100 lines):"));
        assert!(!output.contains("ARCHITECTURE.md"));
        assert!(!output.contains("CONVENTIONS.md"));
        assert!(!output.contains("AGENTS.md"));
    }

    #[test]
    fn gather_project_context_handles_missing_readme_gracefully() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        fs::write(dir.join("Cargo.toml"), "").unwrap();

        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);

        assert!(output.starts_with("PROJECT STRUCTURE:"));
        assert!(output.contains("Cargo.toml"));
        assert!(!output.contains("README.md"));
        assert!(!output.contains("CLAUDE.md"));
    }

    #[test]
    fn gather_project_context_returns_empty_for_nonexistent_dir() {
        let dir = PathBuf::from("/tmp/definitely_does_not_exist_xyzzy_42");
        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);
        assert!(output.is_empty());
    }

    #[test]
    fn gather_project_context_respects_custom_line_cap() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        // Create 50 files — with a line cap of 10, only 9 entries fit (header takes line 1).
        for i in 0..50 {
            fs::write(dir.join(format!("file_{i:04}.txt")), "").unwrap();
        }

        let output = gather_project_context(&dir, 10, EXCERPT_MAX_LINES);
        let line_count = output.lines().count();
        assert!(
            line_count <= 10,
            "custom line cap of 10 should be respected, got {line_count}"
        );
    }

    #[test]
    fn gather_project_context_respects_custom_excerpt_lines() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        // Create a README.md with 50 lines; limit excerpt to 5.
        let readme_lines: Vec<String> = (1..=50).map(|i| format!("line {i}")).collect();
        fs::write(dir.join("README.md"), readme_lines.join("\n")).unwrap();

        let output = gather_project_context(&dir, LINE_CAP, 5);

        assert!(output.contains("README.md (first 5 lines):"));
        assert!(output.contains("line 1"));
        assert!(output.contains("line 5"));
        assert!(!output.contains("line 6"));
    }

    #[test]
    fn planning_initial_prompt_includes_context_when_provided() {
        let paths = PhasePaths {
            task_md: PathBuf::from("/state/task.md"),
            plan_md: PathBuf::from("/state/plan.md"),
            tasks_md: PathBuf::from("/state/tasks.md"),
            changes_md: PathBuf::from("/state/changes.md"),
            review_md: PathBuf::from("/state/review.md"),
            findings_json: PathBuf::from("/state/findings.json"),
            status_json: PathBuf::from("/state/status.json"),
        };

        let with_context = planning_initial_prompt(
            "My task",
            "PROJECT STRUCTURE:\nsrc/\n  main.rs",
            "",
            &paths,
            false,
        );
        let without_context = planning_initial_prompt("My task", "", "", &paths, false);

        assert!(with_context.contains("PROJECT STRUCTURE:\nsrc/\n  main.rs"));
        assert!(!without_context.contains("PROJECT STRUCTURE:"));
        // Both should contain the task
        assert!(with_context.contains("TASK:\nMy task"));
        assert!(without_context.contains("TASK:\nMy task"));
    }

    #[test]
    fn planning_initial_prompt_injects_prior_decisions_when_present() {
        let paths = test_phase_paths();
        let prompt =
            planning_initial_prompt("Task", "", "- [PATTERN] Reuse parser logic", &paths, false);
        assert!(prompt.contains("PRIOR DECISIONS & LEARNINGS"));
        assert!(prompt.contains("- [PATTERN] Reuse parser logic"));
    }

    #[test]
    fn planning_initial_prompt_plan_mode_uses_plan_markers() {
        let paths = test_phase_paths();
        let prompt = planning_initial_prompt("Task", "", "", &paths, true);
        assert!(prompt.contains("<plan>"));
        assert!(prompt.contains("</plan>"));
        assert!(!prompt.contains("Write your plan to the file:"));
    }

    fn test_config_for_prompts() -> Config {
        use crate::test_support::{TestConfigOptions, make_test_config};
        make_test_config(
            &PathBuf::from("/tmp/prompts-test"),
            TestConfigOptions::default(),
        )
    }

    fn test_phase_paths() -> PhasePaths {
        PhasePaths {
            task_md: PathBuf::from("/state/task.md"),
            plan_md: PathBuf::from("/state/plan.md"),
            tasks_md: PathBuf::from("/state/tasks.md"),
            changes_md: PathBuf::from("/state/changes.md"),
            review_md: PathBuf::from("/state/review.md"),
            findings_json: PathBuf::from("/state/findings.json"),
            status_json: PathBuf::from("/state/status.json"),
        }
    }

    #[test]
    fn planning_reviewer_prompt_includes_concerns_when_dispute_reason_present() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
            config: &config,
            task: "Build feature X",
            plan: "1. Implement it",
            round: 2,
            prompt_timestamp: "2026-02-14T10:00:00.000Z",
            paths: &paths,
            dispute_reason: Some("I disagree with the rollback plan"),
            open_findings: "",
        });

        assert!(
            prompt.contains("IMPLEMENTER'S CONCERNS:\nI disagree with the rollback plan"),
            "prompt should include the concerns section with the dispute reason"
        );
    }

    #[test]
    fn planning_reviewer_prompt_omits_concerns_when_dispute_reason_absent() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
            config: &config,
            task: "Build feature X",
            plan: "1. Implement it",
            round: 2,
            prompt_timestamp: "2026-02-14T10:00:00.000Z",
            paths: &paths,
            dispute_reason: None,
            open_findings: "",
        });

        assert!(
            !prompt.contains("IMPLEMENTER'S CONCERNS:"),
            "prompt should not include the concerns section when no dispute reason"
        );
    }

    #[test]
    fn planning_reviewer_prompt_omits_concerns_when_dispute_reason_is_empty() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
            config: &config,
            task: "Build feature X",
            plan: "1. Implement it",
            round: 2,
            prompt_timestamp: "2026-02-14T10:00:00.000Z",
            paths: &paths,
            dispute_reason: Some("   "),
            open_findings: "",
        });

        assert!(
            !prompt.contains("IMPLEMENTER'S CONCERNS:"),
            "prompt should not include the concerns section when dispute reason is whitespace-only"
        );
    }

    #[test]
    fn decomposition_initial_prompt_includes_context_when_provided() {
        let paths = PhasePaths {
            task_md: PathBuf::from("/state/task.md"),
            plan_md: PathBuf::from("/state/plan.md"),
            tasks_md: PathBuf::from("/state/tasks.md"),
            changes_md: PathBuf::from("/state/changes.md"),
            review_md: PathBuf::from("/state/review.md"),
            findings_json: PathBuf::from("/state/findings.json"),
            status_json: PathBuf::from("/state/status.json"),
        };

        let with_context =
            decomposition_initial_prompt("My task", "The plan", "PROJECT STRUCTURE:\nsrc/", &paths);
        let without_context = decomposition_initial_prompt("My task", "The plan", "", &paths);

        assert!(with_context.contains("PROJECT STRUCTURE:\nsrc/"));
        assert!(!without_context.contains("PROJECT STRUCTURE:"));
        assert!(with_context.contains("ORIGINAL TASK:\nMy task"));
        assert!(without_context.contains("ORIGINAL TASK:\nMy task"));
    }

    #[test]
    fn implementation_reviewer_prompt_excludes_quality_checks_when_none() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            "",
            None,
            "",
            "",
        );

        assert!(!prompt.contains("QUALITY CHECKS:"));
        assert!(prompt.contains("ACTUAL CODE DIFF:\ndiff content"));
    }

    #[test]
    fn implementation_reviewer_prompt_includes_quality_checks_when_provided() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let checks = "QUALITY CHECKS:\n\n--- cargo test [PASS] ---\nAll 42 tests passed.";
        let prompt = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            "",
            Some(checks),
            "",
            "",
        );

        assert!(prompt.contains("QUALITY CHECKS:"));
        assert!(prompt.contains("All 42 tests passed."));
        // Quality checks should appear after the diff
        let diff_pos = prompt.find("ACTUAL CODE DIFF:").unwrap();
        let checks_pos = prompt.find("QUALITY CHECKS:").unwrap();
        assert!(checks_pos > diff_pos);
    }

    #[test]
    fn implementation_reviewer_prompt_ignores_empty_quality_checks() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            "",
            Some("   "),
            "",
            "",
        );

        assert!(!prompt.contains("QUALITY CHECKS:"));
    }

    #[test]
    fn implementation_implementer_prompt_includes_round_history_when_provided() {
        let paths = test_phase_paths();
        let history = "Round 1 implementation: Added auth\nRound 1 review: NEEDS_CHANGES — missing validation";
        let prompt = implementation_implementer_prompt(
            2,
            "Task",
            "Plan",
            "Fix validation",
            "",
            "",
            &paths,
            history,
        );

        assert!(prompt.contains("ROUND HISTORY:\nRound 1 implementation: Added auth\nRound 1 review: NEEDS_CHANGES — missing validation"));
        // History should appear after PLAN and before review feedback
        let plan_pos = prompt.find("PLAN:\nPlan").unwrap();
        let history_pos = prompt.find("ROUND HISTORY:").unwrap();
        let review_pos = prompt.find("PREVIOUS REVIEW FEEDBACK").unwrap();
        assert!(history_pos > plan_pos);
        assert!(history_pos < review_pos);
    }

    #[test]
    fn implementation_implementer_prompt_omits_round_history_when_empty() {
        let paths = test_phase_paths();
        let prompt = implementation_implementer_prompt(1, "Task", "Plan", "", "", "", &paths, "");

        assert!(!prompt.contains("ROUND HISTORY:"));
    }

    #[test]
    fn implementation_implementer_prompt_omits_round_history_when_whitespace_only() {
        let paths = test_phase_paths();
        let prompt =
            implementation_implementer_prompt(1, "Task", "Plan", "", "", "", &paths, "   \n  ");

        assert!(!prompt.contains("ROUND HISTORY:"));
    }

    #[test]
    fn implementation_implementer_prompt_includes_open_findings_when_present() {
        let paths = test_phase_paths();
        let prompt = implementation_implementer_prompt(
            2,
            "Task",
            "Plan",
            "Fix the previous issues",
            "- F-001 [HIGH] Missing validation (src/lib.rs:10)",
            "",
            &paths,
            "",
        );

        assert!(prompt.contains("OPEN FINDINGS (resolve every item before approval):"));
        assert!(prompt.contains("F-001 [HIGH] Missing validation"));
    }

    #[test]
    fn implementation_prompts_include_decision_capture_categories() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();

        let implementer = implementation_implementer_prompt(
            1,
            "Task",
            "Plan",
            "",
            "",
            "- [CONSTRAINT] Keep compatibility",
            &paths,
            "",
        );
        assert!(implementer.contains("DECISION CAPTURE"));
        assert!(implementer.contains("ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY"));
        assert!(implementer.contains("PRIOR DECISIONS & LEARNINGS"));

        let reviewer = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            "",
            None,
            "- [GOTCHA] Handle stale status writes",
            "",
        );
        assert!(reviewer.contains("DECISION CAPTURE"));
        assert!(reviewer.contains("ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY"));
        assert!(reviewer.contains("PRIOR DECISIONS & LEARNINGS"));
    }

    #[test]
    fn implementation_consensus_prompt_includes_self_review_checklist_and_context() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_consensus_prompt(
            &config,
            "Task body",
            "Plan body",
            "Reviewer approved",
            "",
            2,
            "2026-02-15T00:00:00.000Z",
            &paths,
        );

        assert!(prompt.contains("perform your own final review"));
        assert!(prompt.contains("TASK:\nTask body"));
        assert!(prompt.contains("PLAN:\nPlan body"));
        assert!(prompt.contains("If everything checks out, write CONSENSUS [JSON]."));
        assert!(prompt.contains("If you find issues the reviewer missed, write DISPUTED [JSON]"));
    }

    #[test]
    fn compound_prompt_includes_categories_and_decisions_path() {
        let prompt = compound_prompt("Task", "Plan");
        assert!(prompt.contains(".agent-loop/decisions.md"));
        assert!(prompt.contains("ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY"));
    }

    #[test]
    fn implementation_reviewer_prompt_includes_round_history_when_provided() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let history = "Round 1 implementation: Added auth\nRound 1 review: APPROVED";
        let prompt = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            2,
            "2026-02-15T00:00:00.000Z",
            &paths,
            "",
            None,
            "",
            history,
        );

        assert!(prompt.contains(
            "ROUND HISTORY:\nRound 1 implementation: Added auth\nRound 1 review: APPROVED"
        ));
        // History should appear after diff/quality-checks and before review criteria
        let diff_pos = prompt.find("ACTUAL CODE DIFF:").unwrap();
        let history_pos = prompt.find("ROUND HISTORY:").unwrap();
        let review_pos = prompt.find("Review the ACTUAL code changes").unwrap();
        assert!(history_pos > diff_pos);
        assert!(history_pos < review_pos);
    }

    #[test]
    fn implementation_reviewer_prompt_omits_round_history_when_empty() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            "",
            None,
            "",
            "",
        );

        assert!(!prompt.contains("ROUND HISTORY:"));
    }

    #[test]
    fn planning_reviewer_prompt_includes_structured_sections() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
            config: &config,
            task: "Build feature X",
            plan: "1. Implement it",
            round: 1,
            prompt_timestamp: "2026-02-15T00:00:00.000Z",
            paths: &paths,
            dispute_reason: None,
            open_findings: "",
        });

        assert!(
            prompt.contains("## Completeness"),
            "missing Completeness section"
        );
        assert!(
            prompt.contains("## Feasibility"),
            "missing Feasibility section"
        );
        assert!(prompt.contains("## Risks"), "missing Risks section");
        assert!(prompt.contains("## Verdict"), "missing Verdict section");
    }

    #[test]
    fn implementation_reviewer_prompt_includes_structured_sections() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            "",
            None,
            "",
            "",
        );

        assert!(
            prompt.contains("## Correctness"),
            "missing Correctness section"
        );
        assert!(prompt.contains("## Tests"), "missing Tests section");
        assert!(prompt.contains("## Style"), "missing Style section");
        assert!(prompt.contains("## Security"), "missing Security section");
        assert!(prompt.contains("## Findings"), "missing Findings section");
        assert!(prompt.contains("## Verdict"), "missing Verdict section");
    }

    #[test]
    fn planning_reviewer_prompt_preserves_json_status_format() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
            config: &config,
            task: "Build feature X",
            plan: "1. Implement it",
            round: 1,
            prompt_timestamp: "2026-02-15T00:00:00.000Z",
            paths: &paths,
            dispute_reason: None,
            open_findings: "",
        });

        assert!(
            prompt.contains("\"status\": \"APPROVED\""),
            "missing APPROVED JSON status"
        );
        assert!(
            prompt.contains("\"status\": \"NEEDS_REVISION\""),
            "missing NEEDS_REVISION JSON status"
        );
    }

    #[test]
    fn implementation_reviewer_prompt_preserves_json_status_format() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_reviewer_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            "",
            None,
            "",
            "",
        );

        assert!(
            prompt.contains("\"status\": \"APPROVED\""),
            "missing APPROVED JSON status"
        );
        assert!(
            prompt.contains("\"status\": \"NEEDS_CHANGES\""),
            "missing NEEDS_CHANGES JSON status"
        );
        assert!(
            prompt.contains("\"rating\":"),
            "missing rating field in JSON status"
        );
        assert!(
            prompt.contains("\"findings\":"),
            "missing findings JSON template"
        );
    }

    #[test]
    fn adversarial_review_prompt_contains_diff_and_first_review() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_adversarial_review_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff --git a/file.rs\n+new code",
            "First review: looks great!",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            None,
        );

        assert!(
            prompt.contains("diff --git a/file.rs\n+new code"),
            "adversarial prompt should contain the diff"
        );
        assert!(
            prompt.contains("First review: looks great!"),
            "adversarial prompt should contain the first review"
        );
    }

    #[test]
    fn adversarial_review_prompt_contains_guardrail_and_framing() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_adversarial_review_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            "First review text",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            None,
        );

        assert!(
            prompt.contains("find what the first reviewer missed"),
            "adversarial prompt should contain adversarial framing"
        );
        assert!(
            prompt.contains("no meaningful issues"),
            "adversarial prompt should contain guardrail text"
        );
        assert!(
            prompt.contains("Do not manufacture issues"),
            "adversarial prompt should discourage false negatives"
        );
    }

    #[test]
    fn adversarial_review_prompt_includes_quality_checks_when_provided() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let checks = "QUALITY CHECKS:\n\n--- cargo test [PASS] ---\nAll 42 tests passed.";
        let prompt = implementation_adversarial_review_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            "First review text",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            Some(checks),
        );

        assert!(prompt.contains("QUALITY CHECKS:"));
        assert!(prompt.contains("All 42 tests passed."));
    }

    #[test]
    fn adversarial_review_prompt_omits_quality_checks_when_none() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_adversarial_review_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            "First review text",
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            None,
        );

        assert!(!prompt.contains("QUALITY CHECKS:"));
    }

    #[test]
    fn adversarial_review_prompt_includes_json_status_templates() {
        let config = test_config_for_prompts();
        let paths = test_phase_paths();
        let prompt = implementation_adversarial_review_prompt(
            &config,
            "Task",
            "Plan",
            "Changes",
            "diff content",
            "First review text",
            2,
            "2026-02-15T12:00:00.000Z",
            &paths,
            None,
        );

        assert!(
            prompt.contains("\"status\": \"APPROVED\""),
            "missing APPROVED JSON status"
        );
        assert!(
            prompt.contains("\"status\": \"NEEDS_CHANGES\""),
            "missing NEEDS_CHANGES JSON status"
        );
        assert!(
            prompt.contains("\"rating\":"),
            "missing rating field in JSON status"
        );
        assert!(
            prompt.contains("\"findings\":"),
            "missing findings JSON template"
        );
        assert!(
            prompt.contains("\"round\": 2"),
            "missing round in JSON status"
        );
        assert!(
            prompt.contains("\"timestamp\": \"2026-02-15T12:00:00.000Z\""),
            "missing timestamp in JSON status"
        );
    }

    #[test]
    fn state_manifest_includes_plan_tasks_and_agents_md() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        let state_dir = dir.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();

        // Create the project-level files.
        fs::write(dir.join("README.md"), "readme").unwrap();
        fs::write(dir.join("CLAUDE.md"), "claude").unwrap();
        fs::write(dir.join("AGENTS.md"), "agents guide").unwrap();

        // Create the state files.
        fs::write(state_dir.join("conversation.md"), "convo").unwrap();
        fs::write(state_dir.join("plan.md"), "the plan").unwrap();
        fs::write(state_dir.join("tasks.md"), "task list").unwrap();

        // Create decisions.md.
        let decisions_dir = dir.join(".agent-loop");
        fs::write(decisions_dir.join("decisions.md"), "decisions").unwrap();

        let config = Config {
            project_dir: dir.clone(),
            state_dir: state_dir.clone(),
            ..crate::test_support::make_test_config(
                &dir,
                crate::test_support::TestConfigOptions::default(),
            )
        };

        let manifest = state_manifest(&config);

        assert!(
            manifest.contains("AGENTS.md"),
            "manifest should include AGENTS.md"
        );
        assert!(
            manifest.contains("plan.md"),
            "manifest should include plan.md"
        );
        assert!(
            manifest.contains("tasks.md"),
            "manifest should include tasks.md"
        );
        assert!(
            manifest.contains("conversation.md"),
            "manifest should include conversation.md"
        );
        assert!(
            manifest.contains("decisions.md"),
            "manifest should include decisions.md"
        );
    }

    #[test]
    fn state_manifest_omits_missing_files() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        let state_dir = dir.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let config = Config {
            project_dir: dir.clone(),
            state_dir: state_dir.clone(),
            ..crate::test_support::make_test_config(
                &dir,
                crate::test_support::TestConfigOptions::default(),
            )
        };

        let manifest = state_manifest(&config);

        assert!(
            !manifest.contains("AGENTS.md"),
            "absent AGENTS.md should be omitted"
        );
        assert!(
            !manifest.contains("plan.md"),
            "absent plan.md should be omitted"
        );
        assert!(
            !manifest.contains("tasks.md"),
            "absent tasks.md should be omitted"
        );
        assert!(
            !manifest.contains("conversation.md"),
            "absent conversation.md should be omitted"
        );
    }
}
