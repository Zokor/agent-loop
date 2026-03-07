use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::state::{FINDINGS_FILENAME, QUALITY_CHECKS_FILENAME};

#[allow(dead_code)]
const EXCLUDED_DIRS: &[&str] = &[".git", "target", "node_modules", ".agent-loop"];

/// Build a project structure overview for injection into planning prompts.
///
/// Walks the directory tree up to depth 2, excludes common noise directories,
/// and appends the first `excerpt_max_lines` lines of README.md / CLAUDE.md when present.
/// Total output is capped at `line_cap` lines.
///
/// `line_cap` and `excerpt_max_lines` correspond to the Config fields
/// `context_line_cap` and `planning_context_excerpt_lines`.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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

    let file_line_count = content.lines().count();
    let remaining = line_cap.saturating_sub(lines.len().saturating_add(2));
    let max_excerpt = remaining.min(excerpt_max_lines).min(file_line_count);

    lines.push(String::new());
    if max_excerpt >= file_line_count {
        lines.push(format!("{filename}:"));
    } else {
        lines.push(format!("{filename} (first {max_excerpt} lines):"));
    }

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

    let mut doc_files: Vec<(&str, &str)> = vec![
        ("README.md", "project documentation"),
        ("CLAUDE.md", "project instructions for AI"),
        ("AGENTS.md", "agent conventions & guidelines"),
    ];
    if config.decisions_enabled {
        doc_files.push((".agent-loop/decisions.md", "prior decisions & learnings"));
    }

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
        (QUALITY_CHECKS_FILENAME, "auto quality-check results"),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhasePaths {
    pub(crate) task_md: PathBuf,
    pub(crate) plan_md: PathBuf,
    pub(crate) tasks_md: PathBuf,
    pub(crate) changes_md: PathBuf,
    pub(crate) quality_checks_md: PathBuf,
    pub(crate) review_md: PathBuf,
    pub(crate) findings_json: PathBuf,
    pub(crate) planning_findings_json: PathBuf,
    pub(crate) tasks_findings_json: PathBuf,
    pub(crate) status_json: PathBuf,
}

/// State directory relative to the project root, used in prompt text.
const STATE_REL: &str = ".agent-loop/state";

pub(crate) fn phase_paths(_config: &Config) -> PhasePaths {
    use crate::state::{PLANNING_FINDINGS_FILENAME, TASKS_FINDINGS_FILENAME};
    let base = Path::new(STATE_REL);
    PhasePaths {
        task_md: base.join("task.md"),
        plan_md: base.join("plan.md"),
        tasks_md: base.join("tasks.md"),
        changes_md: base.join("changes.md"),
        quality_checks_md: base.join(QUALITY_CHECKS_FILENAME),
        review_md: base.join("review.md"),
        findings_json: base.join(FINDINGS_FILENAME),
        planning_findings_json: base.join(PLANNING_FINDINGS_FILENAME),
        tasks_findings_json: base.join(TASKS_FINDINGS_FILENAME),
        status_json: base.join("status.json"),
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Shared output instructions for implementation reviewer prompts (Gate A, Gate B,
/// Gate B verification, Gate C). Generates the findings JSON + status JSON
/// block so it isn't copy-pasted across every reviewer function.
fn implementation_reviewer_output_block(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "Write review to {review_md} and findings to {findings_json}.\n\
        APPROVED: {{\"round\": {round}, \"findings\": []}}\n\
        CHANGES NEEDED: {{\"round\": {round}, \"findings\": [{{\"id\": \"F-001\", \"severity\": \"HIGH\", \"summary\": \"...\", \"file_refs\": [\"file:line\"]}}]}}\n\n\
        Write to {status_json}:\n\
        APPROVED: {{\"status\": \"APPROVED\", \"round\": {round}, \"timestamp\": \"{prompt_timestamp}\"}}\n\
        CHANGES NEEDED: {{\"status\": \"NEEDS_CHANGES\", \"round\": {round}, \"reason\": \"brief summary\", \"timestamp\": \"{prompt_timestamp}\"}}",
        review_md = path_text(&paths.review_md),
        findings_json = path_text(&paths.findings_json),
        status_json = path_text(&paths.status_json),
    )
}

/// Shared signoff block for consensus/disputed prompts.
fn signoff_status_block(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "Write to {status_json}:\n\
        If you agree: {{\"status\": \"CONSENSUS\", \"round\": {round}, \"timestamp\": \"{prompt_timestamp}\"}}\n\
        If you disagree: {{\"status\": \"DISPUTED\", \"round\": {round}, \"reason\": \"your concerns\", \"timestamp\": \"{prompt_timestamp}\"}}",
        status_json = path_text(&paths.status_json),
    )
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

    if config.decisions_enabled {
        parts.push(DECISION_CAPTURE_INSTRUCTIONS.to_string());
    }

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
    paths: &PhasePaths,
    planner_plan_mode: bool,
) -> String {
    let output_instruction = if planner_plan_mode {
        "Output your plan below between <plan> and </plan> markers.".to_string()
    } else {
        format!("Write your plan to the file: {}", path_text(&paths.plan_md))
    };

    format!(
        "Read the task from {task_md} and propose a detailed development plan.\n\n\
        Include: overview, step-by-step strategy, files to modify, key decisions, testing strategy.\n\n\
        {output_instruction}",
        task_md = path_text(&paths.task_md),
    )
}

/// Planning reviewer prompt parameters bundled to satisfy clippy::too_many_arguments.
pub(crate) struct PlanningReviewerParams<'a> {
    pub round: u32,
    pub prompt_timestamp: &'a str,
    pub paths: &'a PhasePaths,
    pub dispute_reason: Option<&'a str>,
    pub open_findings: &'a str,
}

pub(crate) fn planning_reviewer_prompt(params: &PlanningReviewerParams<'_>) -> String {
    let PlanningReviewerParams {
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
        format!(
            "\n\nOPEN PLANNING FINDINGS (address or resolve each):\n{open_findings}\n\n\
            IMPORTANT: For each open finding above, you MUST include a JSON findings block in your review \
            showing its updated status. Use a fenced code block in your review like this:\n\
            ```json\n\
            [{{\"id\": \"F-001\", \"description\": \"brief description\", \"status\": \"resolved\"}}]\n\
            ```\n\
            Set status to \"resolved\" if the finding has been addressed, or \"open\" if it still needs work. \
            Every open finding listed above MUST appear in your JSON block."
        )
    };

    format!(
        "Critically review the plan in {plan_md} against the actual codebase.\n\
        The original task is in {task_md}.\n\n\
        For every claim, file path, function name, and assumption in the plan, \
        use your tools to verify it against the real code. \
        Check for missing steps, wrong assumptions, breaking changes, dependency issues, \
        and load-order or integration risks.{concerns_section}{findings_section}\n\n\
        Write your review to {review_path}.\n\n\
        End with VERDICT: APPROVED or VERDICT: REVISE.\n\n\
        If APPROVED, write to {status_path}:\n\
        {{\"status\": \"APPROVED\", \"round\": {round}, \"timestamp\": \"{prompt_timestamp}\"}}\n\n\
        If REVISE, write to {status_path2}:\n\
        {{\"status\": \"NEEDS_REVISION\", \"round\": {round}, \"reason\": \"your reason\", \"timestamp\": \"{prompt_timestamp}\"}}",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
        review_path = path_text(&paths.review_md),
        status_path = path_text(&paths.status_json),
        status_path2 = path_text(&paths.status_json),
    )
}

pub(crate) fn planning_adversarial_review_prompt(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "A first reviewer has already approved the plan in {plan_md}. Find what they missed.\n\n\
        The original task is in {task_md}. The first review is in {review_md}.\n\
        For every claim, file path, and assumption in the plan, use your tools to verify it against the real code. \
        Look for breaking changes, missing migration paths, dependency gaps, and integration risks the first review overlooked.\n\
        If you find no meaningful issues, APPROVED is the correct verdict.\n\n\
        Write your review to {review_path}.\n\n\
        End with VERDICT: APPROVED or VERDICT: REVISE.\n\n\
        If APPROVED, write to {status_path}:\n\
        {{\"status\": \"APPROVED\", \"round\": {round}, \"timestamp\": \"{prompt_timestamp}\"}}\n\n\
        If REVISE, write to {status_path2}:\n\
        {{\"status\": \"NEEDS_REVISION\", \"round\": {round}, \"reason\": \"brief summary\", \"timestamp\": \"{prompt_timestamp}\"}}",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
        review_md = path_text(&paths.review_md),
        review_path = path_text(&paths.review_md),
        status_path = path_text(&paths.status_json),
        status_path2 = path_text(&paths.status_json),
    )
}

pub(crate) fn planning_implementer_revision_prompt(
    paths: &PhasePaths,
) -> String {
    format!(
        "Read the review in {review_md}. Revise the plan in {plan_md} to address the findings.",
        review_md = path_text(&paths.review_md),
        plan_md = path_text(&paths.plan_md),
    )
}

pub(crate) fn decomposition_initial_prompt(paths: &PhasePaths) -> String {
    format!(
        "Read the task from {task_md} and the agreed plan from {plan_md}.\n\n\
        Break down the plan into discrete, implementable tasks.\n\
        Write the task breakdown to {tasks_md}.\n\n\
        For each task include: number, title, description, complexity (Low/Medium/High), \
        dependencies, deliverables, and testing requirements.\n\n\
        IMPORTANT: Do NOT include revision history, changelogs, or round-by-round notes in {tasks_md2}. \
        Keep the file clean — only task definitions. Revision tracking is handled externally.",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
        tasks_md = path_text(&paths.tasks_md),
        tasks_md2 = path_text(&paths.tasks_md),
    )
}

pub(crate) fn decomposition_revision_prompt(
    paths: &PhasePaths,
) -> String {
    format!(
        "Read the review in {review_md}. Revise {tasks_md} to address the findings.\n\
        Do NOT add revision history or changelogs — only task definitions.",
        review_md = path_text(&paths.review_md),
        tasks_md = path_text(&paths.tasks_md),
    )
}

pub(crate) fn decomposition_reviewer_prompt(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
    open_findings: &str,
) -> String {
    let findings_section = if open_findings.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nOPEN TASKS FINDINGS (address or resolve each):\n{open_findings}\n\n\
            IMPORTANT: For each open finding above, you MUST include a JSON findings block in your review \
            showing its updated status. Use a fenced code block in your review like this:\n\
            ```json\n\
            [{{\"id\": \"T-001\", \"description\": \"brief description\", \"status\": \"resolved\"}}]\n\
            ```\n\
            Set status to \"resolved\" if the finding has been addressed, or \"open\" if it still needs work. \
            Every open finding listed above MUST appear in your JSON block."
        )
    };

    format!(
        "Read the plan from {plan_md} and the proposed tasks from {tasks_md}.{findings_section}\n\n\
        Review task scope, sizing, dependencies, ordering, completeness, and testing.\n\
        Write your review to {review_path}.\n\n\
        If APPROVED, write to {status_path}:\n\
        {{\"status\": \"APPROVED\", \"round\": {round}, \"timestamp\": \"{prompt_timestamp}\"}}\n\n\
        If changes needed, write to {status_path2}:\n\
        {{\"status\": \"NEEDS_REVISION\", \"round\": {round}, \"reason\": \"brief explanation\", \"timestamp\": \"{prompt_timestamp}\"}}",
        plan_md = path_text(&paths.plan_md),
        tasks_md = path_text(&paths.tasks_md),
        review_path = path_text(&paths.review_md),
        status_path = path_text(&paths.status_json),
        status_path2 = path_text(&paths.status_json),
    )
}

pub(crate) fn decomposition_implementer_signoff_prompt(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "The reviewer has APPROVED the task breakdown.\n\
        Read the task from {task_md}, the plan from {plan_md}, and the tasks from {tasks_md}.\n\
        Review the tasks against the original plan and task.\n\n\
        {signoff_block}",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
        tasks_md = path_text(&paths.tasks_md),
        signoff_block = signoff_status_block(round, prompt_timestamp, paths),
    )
}

pub(crate) fn planning_implementer_signoff_prompt(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "The reviewer has APPROVED your plan. Read the task from {task_md} and the plan from {plan_md}.\n\
        Perform a final review before confirming.\n\n\
        {signoff_block}",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
        signoff_block = signoff_status_block(round, prompt_timestamp, paths),
    )
}

pub(crate) fn implementation_implementer_prompt(
    round: u32,
    paths: &PhasePaths,
) -> String {
    let review_instruction = if round <= 1 {
        "This is the first implementation round.".to_string()
    } else {
        format!(
            "Address the reviewer's feedback in {}.",
            path_text(&paths.review_md)
        )
    };

    format!(
        "Read the task from {} and the plan from {}.\n\
        {review_instruction}\n\n\
        Implement ONLY the task in {}.\n\
        Use {} strictly as supporting context; do not implement unrelated plan items.\n\
        Write a summary of changes to {}.",
        path_text(&paths.task_md),
        path_text(&paths.plan_md),
        path_text(&paths.task_md),
        path_text(&paths.plan_md),
        path_text(&paths.changes_md),
    )
}

pub(crate) fn implementation_reviewer_prompt(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
    auto_test: bool,
) -> String {
    let quality_line = if auto_test {
        format!(
            "\nReview automated check output from {}.",
            path_text(&paths.quality_checks_md)
        )
    } else {
        String::new()
    };

    format!(
        "Read the task from {task_md}, the plan from {plan_md}, and changes from {changes_md}.\n\
        Read the changed files directly and review the implementation.{quality_line}\n\n\
        {output_block}",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
        changes_md = path_text(&paths.changes_md),
        output_block = implementation_reviewer_output_block(round, prompt_timestamp, paths),
    )
}

pub(crate) fn implementation_consensus_prompt(
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
        "Read the task from {task_md}, the plan from {plan_md}, and the review from {review_md}. \
        Review the implementation.\n\n\
        {findings_section}\n\n\
        {signoff_block}",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
        review_md = path_text(&paths.review_md),
        signoff_block = signoff_status_block(round, prompt_timestamp, paths),
    )
}

pub(crate) fn compound_prompt(paths: &PhasePaths) -> String {
    format!(
        "Post-consensus compound reflection phase.\n\n\
        Read the task from {task_md} and the plan from {plan_md}.\n\
        Review the full session: task goals, plan, implementation rounds, review feedback, \
        and any struggles encountered. Extract only reusable, cross-session learnings.\n\
        Do not record one-off or session-specific details.\n\n\
        Append each learning to `.agent-loop/decisions.md` using one line per entry:\n\
        - [CATEGORY] description\n\n\
        Allowed categories: ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY.",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
    )
}

pub(crate) fn implementation_fresh_context_reviewer_prompt(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
    auto_test: bool,
) -> String {
    let quality_line = if auto_test {
        format!(
            "\nReview check output from {}.",
            path_text(&paths.quality_checks_md)
        )
    } else {
        String::new()
    };

    format!(
        "Read the task from {task_md}, the plan from {plan_md}, changes from {changes_md}. \
        Review the implementation.{quality_line}\n\n\
        {output_block}",
        task_md = path_text(&paths.task_md),
        plan_md = path_text(&paths.plan_md),
        changes_md = path_text(&paths.changes_md),
        output_block = implementation_reviewer_output_block(round, prompt_timestamp, paths),
    )
}

pub(crate) fn implementation_gate_b_verification_prompt(
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "You are the SAME fresh-context reviewer from Gate B (round {round}).\n\n\
        You previously found issues. Re-examine each finding against the actual code.\n\
        Read the task from {task_md}, your findings from {findings_json}, and your review from {review_md}.\n\
        For each finding, CONFIRM it is real or WITHDRAW it if mistaken.\n\n\
        If ALL withdrawn: write APPROVED. If ANY confirmed: write NEEDS_CHANGES.\n\n\
        {output_block}",
        task_md = path_text(&paths.task_md),
        findings_json = path_text(&paths.findings_json),
        review_md = path_text(&paths.review_md),
        output_block = implementation_reviewer_output_block(round, prompt_timestamp, paths),
    )
}

pub(crate) fn implementation_gate_c_late_findings_prompt(
    dispute_reason: &str,
    round: u32,
    prompt_timestamp: &str,
    paths: &PhasePaths,
) -> String {
    format!(
        "You are the SAME fresh-context reviewer from Gate B (round {round}).\n\n\
        The implementer has DISPUTED the consensus with late findings.\n\
        Read the task from {task_md}, findings from {findings_json}, and review from {review_md}.\n\n\
        IMPLEMENTER'S DISPUTE REASON:\n{dispute_reason}\n\n\
        Verify each late finding against the code. If REJECTED: write APPROVED. \
        If CONFIRMED: write NEEDS_CHANGES.\n\n\
        {output_block}",
        task_md = path_text(&paths.task_md),
        findings_json = path_text(&paths.findings_json),
        review_md = path_text(&paths.review_md),
        output_block = implementation_reviewer_output_block(round, prompt_timestamp, paths),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_path;
    use std::fs;

    /// Generous defaults for tests — mirrors the 0-means-unlimited semantics.
    const LINE_CAP: usize = usize::MAX;
    const EXCERPT_MAX_LINES: usize = usize::MAX;

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
    fn gather_project_context_caps_at_explicit_line_cap() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        // Create 250 files at the root level to exceed an explicit 200 line budget.
        for i in 0..250 {
            fs::write(dir.join(format!("file_{i:04}.txt")), "").unwrap();
        }

        let cap = 200;
        let output = gather_project_context(&dir, cap, EXCERPT_MAX_LINES);
        let line_count = output.lines().count();

        assert!(
            line_count <= cap,
            "output should be capped at {cap} lines, got {line_count}"
        );
    }

    #[test]
    fn gather_project_context_includes_readme_excerpt() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        // Create a README.md with 150 lines — unlimited excerpt includes all.
        let readme_lines: Vec<String> = (1..=150).map(|i| format!("readme line {i}")).collect();
        fs::write(dir.join("README.md"), readme_lines.join("\n")).unwrap();

        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);

        assert!(output.contains("README.md:"));
        assert!(output.contains("readme line 1"));
        assert!(output.contains("readme line 150"));
    }

    #[test]
    fn gather_project_context_includes_readme_excerpt_with_explicit_cap() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        // Create a README.md with 150 lines (only first 100 should appear with explicit cap).
        let readme_lines: Vec<String> = (1..=150).map(|i| format!("readme line {i}")).collect();
        fs::write(dir.join("README.md"), readme_lines.join("\n")).unwrap();

        let output = gather_project_context(&dir, LINE_CAP, 100);

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

        assert!(output.contains("CLAUDE.md:"));
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
        assert!(output.contains("README.md:"));
        assert!(output.contains("CLAUDE.md:"));
        assert!(output.contains("ARCHITECTURE.md:"));
        assert!(output.contains("CONVENTIONS.md:"));
        assert!(output.contains("AGENTS.md:"));
    }

    #[test]
    fn gather_project_context_skips_missing_additional_repository_guides() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        fs::write(dir.join("README.md"), "readme line").unwrap();
        let output = gather_project_context(&dir, LINE_CAP, EXCERPT_MAX_LINES);

        assert!(output.contains("README.md:"));
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
    fn planning_initial_prompt_references_task_file() {
        let paths = test_phase_paths();
        let prompt = planning_initial_prompt(&paths, false);
        assert!(prompt.contains("/state/task.md"));
        assert!(prompt.contains("Write your plan to the file:"));
    }

    #[test]
    fn planning_initial_prompt_plan_mode_uses_plan_markers() {
        let paths = test_phase_paths();
        let prompt = planning_initial_prompt(&paths, true);
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
            quality_checks_md: PathBuf::from("/state/quality_checks.md"),
            review_md: PathBuf::from("/state/review.md"),
            findings_json: PathBuf::from("/state/findings.json"),
            planning_findings_json: PathBuf::from("/state/planning_findings.json"),
            tasks_findings_json: PathBuf::from("/state/tasks_findings.json"),
            status_json: PathBuf::from("/state/status.json"),
        }
    }

    #[test]
    fn planning_reviewer_prompt_includes_concerns_when_dispute_reason_present() {
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
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
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
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
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
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
    fn planning_reviewer_prompt_references_task_and_plan_files() {
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
            round: 1,
            prompt_timestamp: "2026-02-15T00:00:00.000Z",
            paths: &paths,
            dispute_reason: None,
            open_findings: "",
        });

        assert!(prompt.contains("/state/task.md"), "should reference task file");
        assert!(prompt.contains("/state/plan.md"), "should reference plan file");
    }

    #[test]
    fn planning_reviewer_prompt_includes_review_md_write_instruction() {
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
            round: 1,
            prompt_timestamp: "2026-02-15T00:00:00.000Z",
            paths: &paths,
            dispute_reason: None,
            open_findings: "",
        });

        assert!(
            prompt.contains("Write your review"),
            "reviewer prompt should instruct writing to review.md"
        );
        assert!(
            prompt.contains("review.md"),
            "reviewer prompt should reference review.md path"
        );
    }


    #[test]
    fn decomposition_initial_prompt_references_files() {
        let paths = test_phase_paths();
        let prompt = decomposition_initial_prompt(&paths);
        assert!(prompt.contains("/state/task.md"));
        assert!(prompt.contains("/state/plan.md"));
        assert!(prompt.contains("/state/tasks.md"));
    }

    #[test]
    fn implementation_consensus_prompt_references_files_and_statuses() {
        let paths = test_phase_paths();
        let prompt = implementation_consensus_prompt(
            "",
            2,
            "2026-02-15T00:00:00.000Z",
            &paths,
        );

        assert!(prompt.contains("Review the implementation"));
        assert!(prompt.contains("/state/task.md"));
        assert!(prompt.contains("/state/plan.md"));
        assert!(prompt.contains("CONSENSUS"));
        assert!(prompt.contains("DISPUTED"));
    }

    #[test]
    fn compound_prompt_references_files_and_decisions_path() {
        let paths = test_phase_paths();
        let prompt = compound_prompt(&paths);
        assert!(prompt.contains("/state/task.md"));
        assert!(prompt.contains("/state/plan.md"));
        assert!(prompt.contains(".agent-loop/decisions.md"));
        assert!(prompt.contains("ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY"));
    }

    #[test]
    fn planning_reviewer_prompt_references_files_and_verdict() {
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
            round: 1,
            prompt_timestamp: "2026-02-15T00:00:00.000Z",
            paths: &paths,
            dispute_reason: None,
            open_findings: "",
        });

        assert!(prompt.contains("/state/task.md"), "should reference task file");
        assert!(prompt.contains("/state/plan.md"), "should reference plan file");
        assert!(prompt.contains("VERDICT"), "should contain verdict instruction");
    }

    #[test]
    fn implementation_reviewer_prompt_references_files() {
        let paths = test_phase_paths();
        let prompt = implementation_reviewer_prompt(
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            false,
        );

        assert!(prompt.contains("/state/task.md"), "should reference task file");
        assert!(prompt.contains("/state/plan.md"), "should reference plan file");
        assert!(prompt.contains("/state/changes.md"), "should reference changes file");
        assert!(prompt.contains("/state/review.md"), "should reference review file");
    }

    #[test]
    fn implementation_implementer_prompt_enforces_task_scope() {
        let paths = test_phase_paths();
        let prompt = implementation_implementer_prompt(1, &paths);

        assert!(
            prompt.contains("Implement ONLY the task"),
            "implementer prompt should enforce task-only scope"
        );
        assert!(
            prompt.contains("do not implement unrelated plan items"),
            "implementer prompt should explicitly forbid unrelated plan work"
        );
        assert!(prompt.contains("/state/task.md"), "should reference task file");
        assert!(prompt.contains("/state/plan.md"), "should reference plan file");
    }

    #[test]
    fn planning_reviewer_prompt_preserves_json_status_format() {
        let paths = test_phase_paths();
        let prompt = planning_reviewer_prompt(&PlanningReviewerParams {
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
        let paths = test_phase_paths();
        let prompt = implementation_reviewer_prompt(
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            false,
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
            prompt.contains("\"findings\":"),
            "missing findings JSON template"
        );
    }

    #[test]
    fn fresh_context_review_prompt_references_files_and_framing() {
        let paths = test_phase_paths();
        let prompt = implementation_fresh_context_reviewer_prompt(
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
            false,
        );

        assert!(
            prompt.contains("/state/task.md"),
            "fresh-context prompt should reference task file"
        );
        assert!(
            prompt.contains("/state/plan.md"),
            "fresh-context prompt should reference plan file"
        );
        assert!(
            prompt.contains("Review the implementation"),
            "fresh-context prompt should contain review instruction"
        );
    }

    #[test]
    fn fresh_context_review_prompt_includes_json_status_templates() {
        let paths = test_phase_paths();
        let prompt = implementation_fresh_context_reviewer_prompt(
            2,
            "2026-02-15T12:00:00.000Z",
            &paths,
            false,
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
        assert!(
            !prompt.contains("\"status\": \"CONSENSUS\""),
            "fresh-context reviewer must not write consensus statuses"
        );
        assert!(
            prompt.contains("\"file_refs\":"),
            "fresh-context findings template should require file_refs evidence"
        );
    }

    // -----------------------------------------------------------------------
    // planning_adversarial_review_prompt
    // -----------------------------------------------------------------------

    #[test]
    fn planning_adversarial_prompt_references_files_and_framing() {
        let paths = test_phase_paths();
        let prompt = planning_adversarial_review_prompt(
            1,
            "2026-02-15T00:00:00.000Z",
            &paths,
        );

        assert!(
            prompt.contains("/state/task.md"),
            "planning adversarial prompt should reference task file"
        );
        assert!(
            prompt.contains("/state/plan.md"),
            "planning adversarial prompt should reference plan file"
        );
        assert!(
            prompt.contains("/state/review.md"),
            "planning adversarial prompt should reference review file"
        );
        assert!(
            prompt.contains("Find what they missed"),
            "planning adversarial prompt should contain adversarial framing"
        );
        assert!(
            prompt.contains("no meaningful issues"),
            "planning adversarial prompt should contain guardrail text"
        );
    }

    #[test]
    fn planning_adversarial_prompt_includes_json_status_templates() {
        let paths = test_phase_paths();
        let prompt = planning_adversarial_review_prompt(
            2,
            "2026-02-15T12:00:00.000Z",
            &paths,
        );

        assert!(
            prompt.contains("\"status\": \"APPROVED\""),
            "missing APPROVED JSON status"
        );
        assert!(
            prompt.contains("\"status\": \"NEEDS_REVISION\""),
            "missing NEEDS_REVISION JSON status"
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

        let mut config = Config {
            project_dir: dir.clone(),
            state_dir: state_dir.clone(),
            ..crate::test_support::make_test_config(
                &dir,
                crate::test_support::TestConfigOptions::default(),
            )
        };
        config.decisions_enabled = true;

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

    // -----------------------------------------------------------------------
    // decisions_enabled gating in prompts
    // -----------------------------------------------------------------------

    #[test]
    fn system_prompt_omits_decision_capture_when_disabled() {
        let mut config = test_config_for_prompts();
        config.decisions_enabled = false;
        let prompt = system_prompt_for_role(AgentRole::Implementer, &config);
        assert!(
            !prompt.contains("DECISION CAPTURE"),
            "decision capture should be absent when decisions disabled"
        );
    }

    #[test]
    fn system_prompt_includes_decision_capture_when_enabled() {
        let mut config = test_config_for_prompts();
        config.decisions_enabled = true;
        let prompt = system_prompt_for_role(AgentRole::Implementer, &config);
        assert!(
            prompt.contains("DECISION CAPTURE"),
            "decision capture should be present when decisions enabled"
        );
    }

    #[test]
    fn state_manifest_omits_decisions_file_when_disabled() {
        let dir = make_temp_dir();
        let _guard = TempDir(dir.clone());

        let state_dir = dir.join(".agent-loop").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let decisions_dir = dir.join(".agent-loop");
        fs::write(decisions_dir.join("decisions.md"), "decisions").unwrap();

        let mut config = Config {
            project_dir: dir.clone(),
            state_dir: state_dir.clone(),
            ..crate::test_support::make_test_config(
                &dir,
                crate::test_support::TestConfigOptions::default(),
            )
        };
        config.decisions_enabled = false;

        let manifest = state_manifest(&config);
        assert!(
            !manifest.contains("decisions.md"),
            "decisions.md should be omitted from manifest when disabled"
        );
    }

    #[test]
    fn gate_b_verification_prompt_references_files_and_status_templates() {
        let paths = test_phase_paths();

        let prompt = implementation_gate_b_verification_prompt(
            3,
            "2025-01-01T00:00:00Z",
            &paths,
        );

        assert!(
            prompt.contains("Re-examine each finding"),
            "prompt must ask for re-examination"
        );
        assert!(
            prompt.contains("/state/review.md"),
            "prompt must reference review file"
        );
        assert!(
            prompt.contains("APPROVED"),
            "prompt must have APPROVED template"
        );
        assert!(
            prompt.contains("\"round\": 3"),
            "prompt must include round number in JSON templates"
        );
    }

    #[test]
    fn phase_paths_includes_all_findings_files() {
        let config = test_config_for_prompts();
        let paths = phase_paths(&config);
        assert_eq!(
            paths.findings_json.parent(),
            paths.planning_findings_json.parent(),
            "findings and planning_findings should share parent dir"
        );
        assert_eq!(
            paths.findings_json.parent(),
            paths.tasks_findings_json.parent(),
            "findings and tasks_findings should share parent dir"
        );
        assert!(
            paths
                .findings_json
                .to_string_lossy()
                .contains("findings.json"),
            "findings_json path must be set"
        );
        assert!(
            paths
                .planning_findings_json
                .to_string_lossy()
                .contains("planning_findings.json"),
            "planning_findings_json path must be set"
        );
        assert!(
            paths
                .tasks_findings_json
                .to_string_lossy()
                .contains("tasks_findings.json"),
            "tasks_findings_json path must be set"
        );
        assert!(
            paths
                .quality_checks_md
                .to_string_lossy()
                .contains("quality_checks.md"),
            "quality_checks_md path must be set"
        );
    }

    #[test]
    fn fresh_context_reviewer_prompt_includes_independent_verification() {
        let paths = test_phase_paths();
        let prompt = implementation_fresh_context_reviewer_prompt(
            1,
            "2025-01-01T00:00:00Z",
            &paths,
            false,
        );
        assert!(
            prompt.contains("Review the implementation"),
            "fresh context reviewer prompt must include review instruction"
        );
    }

    #[test]
    fn gate_c_late_findings_prompt_contains_dispute_and_status_templates() {
        let paths = test_phase_paths();
        let prompt = implementation_gate_c_late_findings_prompt(
            "Missing error handling in auth.rs",
            2,
            "2025-01-01T00:00:00Z",
            &paths,
        );

        assert!(
            prompt.contains("Missing error handling in auth.rs"),
            "prompt must include the dispute reason"
        );
        assert!(
            prompt.contains("NEEDS_CHANGES"),
            "prompt must have NEEDS_CHANGES template"
        );
        assert!(
            prompt.contains("APPROVED"),
            "prompt must have APPROVED template"
        );
        assert!(
            prompt.contains("\"round\": 2"),
            "prompt must include round number"
        );
    }
}
