use std::{
    collections::HashSet,
    ffi::OsStr,
    process::{Command, Output},
};

use crate::{config::Config, error::AgentLoopError, state::log};

#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainEntry {
    path: String,
    rename_source: Option<String>,
}

fn run_git<I, S>(args: I, config: &Config) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .args(args)
        .current_dir(&config.project_dir)
        .output()
}

#[cfg(test)]
fn parse_porcelain_path(line: &str) -> String {
    parse_porcelain_entry(line)
        .map(|entry| entry.path)
        .unwrap_or_default()
}

fn parse_quoted_path(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if !(trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2) {
        return Some(trimmed.to_string());
    }

    let inner = &trimmed.as_bytes()[1..trimmed.len() - 1];
    let mut output = Vec::with_capacity(inner.len());
    let mut idx = 0;

    while idx < inner.len() {
        let byte = inner[idx];
        if byte != b'\\' {
            output.push(byte);
            idx += 1;
            continue;
        }

        idx += 1;
        if idx >= inner.len() {
            return None;
        }

        match inner[idx] {
            b'"' => {
                output.push(b'"');
                idx += 1;
            }
            b'\\' => {
                output.push(b'\\');
                idx += 1;
            }
            b'n' => {
                output.push(b'\n');
                idx += 1;
            }
            b'r' => {
                output.push(b'\r');
                idx += 1;
            }
            b't' => {
                output.push(b'\t');
                idx += 1;
            }
            b'0'..=b'7' => {
                let mut value = (inner[idx] - b'0') as u32;
                let mut digits = 1;
                idx += 1;
                while digits < 3 && idx < inner.len() && matches!(inner[idx], b'0'..=b'7') {
                    value = (value * 8) + (inner[idx] - b'0') as u32;
                    idx += 1;
                    digits += 1;
                }
                output.push(value as u8);
            }
            escaped => {
                output.push(escaped);
                idx += 1;
            }
        }
    }

    Some(String::from_utf8_lossy(&output).into_owned())
}

fn find_rename_delimiter(entry: &str) -> Option<usize> {
    let bytes = entry.as_bytes();
    let mut idx = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    while idx + 3 < bytes.len() {
        let byte = bytes[idx];

        if in_quotes {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_quotes = false;
            }
            idx += 1;
            continue;
        }

        if byte == b'"' {
            in_quotes = true;
            idx += 1;
            continue;
        }

        if bytes[idx] == b' '
            && bytes[idx + 1] == b'-'
            && bytes[idx + 2] == b'>'
            && bytes[idx + 3] == b' '
        {
            return Some(idx);
        }

        idx += 1;
    }

    None
}

fn parse_porcelain_entry(line: &str) -> Option<PorcelainEntry> {
    let status = line.get(0..2).unwrap_or_default();
    let entry = line.get(3..).unwrap_or_default().trim();
    if entry.is_empty() {
        return None;
    }

    let is_rename = status.as_bytes().contains(&b'R');
    if is_rename && let Some(delimiter_idx) = find_rename_delimiter(entry) {
        let source = parse_quoted_path(&entry[..delimiter_idx])?;
        let target = parse_quoted_path(&entry[delimiter_idx + 4..])?;
        return Some(PorcelainEntry {
            path: target,
            rename_source: Some(source),
        });
    }

    let path = parse_quoted_path(entry)?;
    Some(PorcelainEntry {
        path,
        rename_source: None,
    })
}

fn parse_porcelain_entries(status_output: &str) -> Vec<PorcelainEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    for raw_line in status_output.split('\n') {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }

        let Some(entry) = parse_porcelain_entry(line) else {
            continue;
        };

        if seen.insert(entry.path.clone()) {
            entries.push(entry);
        }
    }

    entries
}

#[cfg(test)]
fn parse_porcelain_paths(status_output: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();

    for raw_line in status_output.split('\n') {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }

        let path = parse_porcelain_path(line);
        if path.is_empty() {
            continue;
        }

        if seen.insert(path.clone()) {
            files.push(path);
        }
    }

    files
}

fn parse_name_only_paths(diff_output: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();

    for raw_line in diff_output.split('\n') {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }

        let Some(path) = parse_quoted_path(line) else {
            continue;
        };

        if seen.insert(path.clone()) {
            files.push(path);
        }
    }

    files
}

fn should_include_for_checkpoint(file: &str, baseline_files: &HashSet<String>) -> bool {
    if file == ".agent-loop/state" || file.starts_with(".agent-loop/state/") {
        return false;
    }

    !baseline_files.contains(file)
}

fn log_checkpoint_failure(config: &Config) {
    let _ = log(
        "⚠ Git checkpoint skipped (commit failed or nothing to commit)",
        config,
    );
}

fn list_changed_entries(config: &Config) -> Result<Vec<PorcelainEntry>, AgentLoopError> {
    let output = run_git(
        ["status", "--porcelain", "--untracked-files=all", "--"],
        config,
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentLoopError::Git(format!(
            "git status failed (exit {}): {}",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            stderr.trim()
        )));
    }

    Ok(parse_porcelain_entries(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn extend_checkpoint_paths(
    entries: &[PorcelainEntry],
    candidates: &[String],
    candidate_set: &HashSet<String>,
) -> Vec<String> {
    let mut scoped_paths = candidates.to_vec();
    let mut seen = candidate_set.clone();

    for entry in entries {
        if !candidate_set.contains(&entry.path) {
            continue;
        }

        if let Some(rename_source) = &entry.rename_source
            && seen.insert(rename_source.clone())
        {
            scoped_paths.push(rename_source.clone());
        }
    }

    scoped_paths
}

const DIFF_MAX_LINES: usize = 500;

fn untracked_files_diff(config: &Config) -> String {
    let output = match run_git(["ls-files", "--others", "--exclude-standard"], config) {
        Ok(o) if o.status.success() => o,
        _ => return String::new(),
    };

    let file_list = String::from_utf8_lossy(&output.stdout);
    let mut patches = String::new();

    for raw_line in file_list.lines() {
        let file = raw_line.trim();
        if file.is_empty() {
            continue;
        }

        if let Ok(diff_output) = run_git(["diff", "--no-index", "--", "/dev/null", file], config) {
            // git diff --no-index exits with 1 when there are differences, which is expected
            let patch = String::from_utf8_lossy(&diff_output.stdout)
                .trim()
                .to_string();
            if !patch.is_empty() {
                if !patches.is_empty() {
                    patches.push('\n');
                }
                patches.push_str(&patch);
            }
        }
    }

    patches
}

fn append_untracked(combined: &mut String, config: &Config) {
    let untracked = untracked_files_diff(config);
    if !untracked.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&untracked);
    }
}

fn truncate_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= DIFF_MAX_LINES {
        return diff.to_string();
    }
    let truncated = lines[..DIFF_MAX_LINES].join("\n");
    format!(
        "{truncated}\n\n... [diff truncated at ~{DIFF_MAX_LINES} lines — {} total] ...",
        lines.len()
    )
}

pub fn git_rev_parse_head(config: &Config) -> Option<String> {
    let output = run_git(["rev-parse", "HEAD"], config).ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

pub fn git_diff_for_review(baseline_ref: Option<&str>, config: &Config) -> String {
    if !is_git_repo(config) {
        return "(no diff available — not a git repo)".to_string();
    }

    // If HEAD advanced past baseline, generate committed diff
    if let Some(baseline) = baseline_ref
        && let Some(current_head) = git_rev_parse_head(config)
        && current_head != baseline
        && let Ok(output) = run_git(
            ["diff", &format!("{baseline}..{current_head}"), "--"],
            config,
        )
        && output.status.success()
    {
        let diff = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !diff.is_empty() {
            return truncate_diff(&diff);
        }
    }

    // Fallback: staged + working-tree diff vs HEAD, plus untracked files
    if let Ok(output) = run_git(["diff", "HEAD", "--"], config)
        && output.status.success()
    {
        let mut combined = String::from_utf8_lossy(&output.stdout).trim().to_string();
        append_untracked(&mut combined, config);
        if !combined.is_empty() {
            return truncate_diff(&combined);
        }
    }

    // Final fallback: try staged + unstaged separately (e.g. no HEAD commit yet)
    let mut combined = String::new();
    if let Ok(output) = run_git(["diff", "--cached", "--"], config)
        && output.status.success()
    {
        let staged = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !staged.is_empty() {
            combined.push_str(&staged);
        }
    }
    if let Ok(output) = run_git(["diff", "--"], config)
        && output.status.success()
    {
        let unstaged = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !unstaged.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&unstaged);
        }
    }
    append_untracked(&mut combined, config);

    if combined.is_empty() {
        "(no diff available)".to_string()
    } else {
        truncate_diff(&combined)
    }
}

pub fn is_git_repo(config: &Config) -> bool {
    match run_git(["rev-parse", "--is-inside-work-tree"], config) {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

pub fn list_changed_files(config: &Config) -> Result<Vec<String>, AgentLoopError> {
    list_changed_entries(config)
        .map(|entries| entries.into_iter().map(|entry| entry.path).collect())
}

pub fn git_checkpoint(message: &str, config: &Config, baseline_files: &HashSet<String>) {
    if !config.auto_commit {
        let _ = log("⏭️ Git checkpoint skipped (AUTO_COMMIT=0)", config);
        return;
    }

    if !is_git_repo(config) {
        let _ = log("⚠ Git checkpoint skipped (not a git repo)", config);
        return;
    }

    let changed_entries = match list_changed_entries(config) {
        Ok(entries) => entries,
        Err(err) => {
            let _ = log(&format!("⚠ Git checkpoint: {err}"), config);
            log_checkpoint_failure(config);
            return;
        }
    };

    let files_to_commit = changed_entries
        .iter()
        .map(|entry| entry.path.clone())
        .filter(|file| should_include_for_checkpoint(file, baseline_files))
        .collect::<Vec<_>>();

    if files_to_commit.is_empty() {
        let _ = log("⏭️ Git checkpoint skipped (no loop-owned changes)", config);
        return;
    }

    let candidate_set = files_to_commit.iter().cloned().collect::<HashSet<_>>();
    let scoped_paths = extend_checkpoint_paths(&changed_entries, &files_to_commit, &candidate_set);

    let mut add_args = vec!["add".to_string(), "-A".to_string(), "--".to_string()];
    add_args.extend(files_to_commit.iter().cloned());
    match run_git(add_args, config) {
        Ok(output) if output.status.success() => {}
        _ => {
            log_checkpoint_failure(config);
            return;
        }
    }

    let staged_output = match run_git(["diff", "--cached", "--name-only", "--"], config) {
        Ok(output) if output.status.success() => output,
        _ => {
            log_checkpoint_failure(config);
            return;
        }
    };

    let staged_paths = parse_name_only_paths(&String::from_utf8_lossy(&staged_output.stdout));
    let staged_for_commit = staged_paths
        .into_iter()
        .filter(|line| candidate_set.contains(line))
        .collect::<Vec<_>>();

    if staged_for_commit.is_empty() {
        let _ = log("⏭️ Git checkpoint skipped (no scoped staged files)", config);
        return;
    }

    let mut commit_args = vec![
        "commit".to_string(),
        "-m".to_string(),
        format!("agent-loop: {message}"),
        "--only".to_string(),
        "--".to_string(),
    ];
    commit_args.extend(scoped_paths);

    match run_git(commit_args, config) {
        Ok(output) if output.status.success() => {
            let _ = log(
                &format!(
                    "📦 Git checkpoint: {message} ({} file(s))",
                    staged_for_commit.len()
                ),
                config,
            );
        }
        _ => log_checkpoint_failure(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestProject, env_lock, run_git_ok};
    use std::fs;

    fn new_project(auto_commit: bool) -> TestProject {
        TestProject::builder("agent_loop_git_test")
            .auto_commit(auto_commit)
            .build()
    }

    fn new_git_project(auto_commit: bool) -> TestProject {
        TestProject::builder("agent_loop_git_test")
            .auto_commit(auto_commit)
            .with_git()
            .build()
    }

    #[test]
    fn is_git_repo_returns_false_for_non_git_directory() {
        let _env_guard = env_lock();
        let project = new_project(true);
        assert!(!is_git_repo(&project.config));
    }

    #[test]
    fn is_git_repo_returns_true_for_initialized_repo() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        assert!(is_git_repo(&project.config));
    }

    #[test]
    fn list_changed_files_includes_modified_and_untracked_paths() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("tracked.txt", "one");
        project.commit_all("initial");

        project.write_file("tracked.txt", "two");
        project.write_file("new.txt", "new");

        let changed_files = list_changed_files(&project.config).expect("should list files");
        assert!(changed_files.contains(&"tracked.txt".to_string()));
        assert!(changed_files.contains(&"new.txt".to_string()));
    }

    #[test]
    fn list_changed_files_uses_rename_target_path() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("old-name.txt", "content");
        project.commit_all("initial");

        fs::rename(
            project.root.join("old-name.txt"),
            project.root.join("new-name.txt"),
        )
        .expect("rename should succeed");
        run_git_ok(&project.root, &["add", "-A", "--"]);

        let changed_files = list_changed_files(&project.config).expect("should list files");
        assert_eq!(changed_files, vec!["new-name.txt".to_string()]);
    }

    #[test]
    fn parse_porcelain_paths_dedupes_and_preserves_first_order() {
        let status_output =
            " M src/one.rs\n?? src/two.rs\nR  old.rs -> src/one.rs\n?? src/two.rs\n";

        let parsed = parse_porcelain_paths(status_output);
        assert_eq!(
            parsed,
            vec!["src/one.rs".to_string(), "src/two.rs".to_string()]
        );
    }

    #[test]
    fn parse_porcelain_path_unquotes_special_characters() {
        assert_eq!(
            parse_porcelain_path("?? \"a b.txt\""),
            "a b.txt".to_string()
        );
        assert_eq!(
            parse_porcelain_path("?? \"quote\\\"name.txt\""),
            "quote\"name.txt".to_string()
        );
    }

    #[test]
    fn parse_name_only_paths_unquotes_escaped_entries() {
        let diff_output = "plain.txt\n\"quote\\\"name.txt\"\n\"na\\303\\257ve.txt\"\n";

        let parsed = parse_name_only_paths(diff_output);
        assert_eq!(
            parsed,
            vec![
                "plain.txt".to_string(),
                "quote\"name.txt".to_string(),
                "naïve.txt".to_string(),
            ]
        );
    }

    #[test]
    fn checkpoint_filter_excludes_state_paths_and_baseline_files() {
        let baseline = HashSet::from(["baseline.txt".to_string()]);

        assert!(!should_include_for_checkpoint(
            ".agent-loop/state",
            &baseline
        ));
        assert!(!should_include_for_checkpoint(
            ".agent-loop/state/status.json",
            &baseline
        ));
        assert!(!should_include_for_checkpoint("baseline.txt", &baseline));
        assert!(should_include_for_checkpoint("src/main.rs", &baseline));
    }

    #[test]
    fn git_checkpoint_skips_when_no_loop_owned_files_remain() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("baseline.txt", "baseline");
        project.commit_all("initial");

        project.write_file("baseline.txt", "changed baseline");
        project.write_file(".agent-loop/state/status.json", "{\"status\":\"PENDING\"}");

        let baseline = HashSet::from(["baseline.txt".to_string()]);
        let before = project.commit_count();

        git_checkpoint("round-1", &project.config, &baseline);

        assert_eq!(project.commit_count(), before);
        assert!(
            project
                .read_log()
                .contains("⏭️ Git checkpoint skipped (no loop-owned changes)")
        );
    }

    #[test]
    fn git_checkpoint_commits_only_scoped_files_with_prefixed_message() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("baseline.txt", "baseline");
        project.write_file("owned.txt", "before");
        project.commit_all("initial");

        project.write_file("baseline.txt", "changed baseline");
        project.write_file(".agent-loop/state/changes.md", "internal state");
        project.write_file("owned.txt", "after");

        let baseline = HashSet::from(["baseline.txt".to_string()]);
        let before = project.commit_count();

        git_checkpoint("round-1-implementation", &project.config, &baseline);

        assert_eq!(project.commit_count(), before + 1);
        assert!(project.head_subject().starts_with("agent-loop: "));

        let committed_files = project.head_files();
        assert!(committed_files.contains(&"owned.txt".to_string()));
        assert!(!committed_files.contains(&"baseline.txt".to_string()));
        assert!(
            !committed_files
                .iter()
                .any(|path| path == ".agent-loop/state" || path.starts_with(".agent-loop/state/"))
        );
    }

    #[test]
    fn git_checkpoint_commits_paths_with_spaces() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("seed.txt", "seed");
        project.commit_all("initial");
        project.write_file("file with spaces.txt", "loop-owned");

        let before = project.commit_count();
        git_checkpoint("round-spaces", &project.config, &HashSet::new());

        assert_eq!(project.commit_count(), before + 1);
        let committed_files = project.head_files();
        assert!(committed_files.contains(&"file with spaces.txt".to_string()));
    }

    #[test]
    fn git_checkpoint_commits_paths_with_quotes() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("seed.txt", "seed");
        project.commit_all("initial");
        project.write_file("quote\"name.txt", "loop-owned");

        let before = project.commit_count();
        git_checkpoint("round-quotes", &project.config, &HashSet::new());

        assert_eq!(project.commit_count(), before + 1);
        let committed_files = project.head_files();
        assert!(committed_files.contains(&"quote\"name.txt".to_string()));
    }

    #[test]
    fn git_checkpoint_commits_rename_without_leaving_old_path_tracked() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("old-name.txt", "content");
        project.commit_all("initial");

        fs::rename(
            project.root.join("old-name.txt"),
            project.root.join("new-name.txt"),
        )
        .expect("rename should succeed");

        let before = project.commit_count();
        git_checkpoint("round-rename", &project.config, &HashSet::new());

        assert_eq!(project.commit_count(), before + 1);
        let tracked_files = project.tracked_files();
        assert!(tracked_files.contains(&"new-name.txt".to_string()));
        assert!(!tracked_files.contains(&"old-name.txt".to_string()));
    }

    #[test]
    fn git_checkpoint_skips_when_auto_commit_is_disabled() {
        let _env_guard = env_lock();
        let project = new_git_project(false);
        project.write_file("tracked.txt", "initial");
        project.commit_all("initial");
        project.write_file("tracked.txt", "updated");

        let before = project.commit_count();
        git_checkpoint("round-1", &project.config, &HashSet::new());

        assert_eq!(project.commit_count(), before);
        assert!(
            project
                .read_log()
                .contains("⏭️ Git checkpoint skipped (AUTO_COMMIT=0)")
        );
    }

    #[test]
    fn git_checkpoint_logs_and_skips_when_git_command_fails() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("tracked.txt", "initial");
        project.commit_all("initial");
        project.write_file("tracked.txt", "updated");
        project.write_file(".git/index.lock", "lock");

        let before = project.commit_count();
        git_checkpoint("round-1", &project.config, &HashSet::new());

        assert_eq!(project.commit_count(), before);
        assert!(
            project
                .read_log()
                .contains("⚠ Git checkpoint skipped (commit failed or nothing to commit)")
        );
    }

    #[test]
    fn git_checkpoint_logs_failure_when_git_status_fails() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("tracked.txt", "initial");
        project.commit_all("initial");
        project.write_file("tracked.txt", "updated");
        project.write_file(".git/index", "not-a-valid-index");

        let before = project.commit_count();
        git_checkpoint("round-status-failure", &project.config, &HashSet::new());

        assert_eq!(project.commit_count(), before);
        let log_output = project.read_log();
        assert!(
            log_output.contains("⚠ Git checkpoint skipped (commit failed or nothing to commit)")
        );
        assert!(!log_output.contains("⏭️ Git checkpoint skipped (no loop-owned changes)"));
    }

    // --- git_rev_parse_head tests ---

    #[test]
    fn git_rev_parse_head_returns_sha_for_initialized_repo() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("seed.txt", "seed");
        project.commit_all("initial");

        let sha = git_rev_parse_head(&project.config);
        assert!(sha.is_some());
        let sha = sha.unwrap();
        assert_eq!(sha.len(), 40);
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn git_rev_parse_head_returns_none_for_non_git_dir() {
        let _env_guard = env_lock();
        let project = new_project(true);

        assert_eq!(git_rev_parse_head(&project.config), None);
    }

    #[test]
    fn git_rev_parse_head_returns_none_for_empty_repo() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        // No commits yet
        assert_eq!(git_rev_parse_head(&project.config), None);
    }

    // --- git_diff_for_review tests ---

    #[test]
    fn git_diff_for_review_non_git_repo() {
        let _env_guard = env_lock();
        let project = new_project(true);

        let diff = git_diff_for_review(None, &project.config);
        assert_eq!(diff, "(no diff available — not a git repo)");
    }

    #[test]
    fn git_diff_for_review_with_no_changes() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("seed.txt", "seed");
        project.commit_all("initial");

        let baseline = git_rev_parse_head(&project.config);
        let diff = git_diff_for_review(baseline.as_deref(), &project.config);
        assert_eq!(diff, "(no diff available)");
    }

    #[test]
    fn git_diff_for_review_with_committed_changes() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("file.txt", "original");
        project.commit_all("initial");

        let baseline = git_rev_parse_head(&project.config).unwrap();

        project.write_file("file.txt", "modified");
        project.commit_all("update");

        let diff = git_diff_for_review(Some(&baseline), &project.config);
        assert!(diff.contains("file.txt"), "diff should reference file.txt");
        assert!(
            diff.contains("-original"),
            "diff should show removed original line"
        );
        assert!(
            diff.contains("+modified"),
            "diff should show added modified line"
        );
    }

    #[test]
    fn git_diff_for_review_with_uncommitted_changes() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("file.txt", "original");
        project.commit_all("initial");

        let baseline = git_rev_parse_head(&project.config).unwrap();
        // HEAD is the same as baseline (simulates AUTO_COMMIT=0)
        project.write_file("file.txt", "uncommitted change");

        let diff = git_diff_for_review(Some(&baseline), &project.config);
        assert!(diff.contains("file.txt"), "diff should reference file.txt");
        assert!(
            diff.contains("+uncommitted change"),
            "diff should show uncommitted changes"
        );
    }

    #[test]
    fn git_diff_for_review_falls_back_to_working_tree_when_head_unchanged() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("file.txt", "original");
        project.commit_all("initial");

        let baseline = git_rev_parse_head(&project.config).unwrap();
        project.write_file("file.txt", "working-tree change");

        // HEAD hasn't advanced, so it should fall back to working-tree diff
        let current_head = git_rev_parse_head(&project.config).unwrap();
        assert_eq!(baseline, current_head, "HEAD should not have advanced");

        let diff = git_diff_for_review(Some(&baseline), &project.config);
        assert!(
            diff.contains("+working-tree change"),
            "diff should show working-tree changes via fallback"
        );
    }

    #[test]
    fn git_diff_for_review_includes_untracked_files_in_no_commit_mode() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("tracked.txt", "original");
        project.commit_all("initial");

        let baseline = git_rev_parse_head(&project.config).unwrap();
        // Modify a tracked file and create a new untracked file
        project.write_file("tracked.txt", "modified");
        project.write_file("newfile.txt", "brand new content");

        // HEAD hasn't advanced (simulates AUTO_COMMIT=0)
        let current_head = git_rev_parse_head(&project.config).unwrap();
        assert_eq!(baseline, current_head, "HEAD should not have advanced");

        let diff = git_diff_for_review(Some(&baseline), &project.config);
        assert!(
            diff.contains("tracked.txt"),
            "diff should include tracked file changes"
        );
        assert!(
            diff.contains("newfile.txt"),
            "diff should include untracked new file"
        );
        assert!(
            diff.contains("brand new content"),
            "diff should show content of untracked file"
        );
    }

    #[test]
    fn git_diff_for_review_includes_only_untracked_files_when_no_tracked_changes() {
        let _env_guard = env_lock();
        let project = new_git_project(true);
        project.write_file("seed.txt", "seed");
        project.commit_all("initial");

        let baseline = git_rev_parse_head(&project.config).unwrap();
        // Only create untracked files, no tracked modifications
        project.write_file("brand_new.txt", "untracked content");

        let current_head = git_rev_parse_head(&project.config).unwrap();
        assert_eq!(baseline, current_head);

        let diff = git_diff_for_review(Some(&baseline), &project.config);
        assert!(
            diff.contains("brand_new.txt"),
            "diff should include untracked file: {diff}"
        );
        assert!(
            diff.contains("untracked content"),
            "diff should show untracked file content: {diff}"
        );
    }

    #[test]
    fn truncate_diff_leaves_short_diffs_unchanged() {
        let short_diff = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(truncate_diff(&short_diff), short_diff);
    }

    #[test]
    fn truncate_diff_truncates_long_diffs_with_notice() {
        let long_diff = (0..600)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let truncated = truncate_diff(&long_diff);

        assert!(truncated.contains("line 0"));
        assert!(truncated.contains("line 499"));
        assert!(!truncated.contains("line 500\n"));
        assert!(truncated.contains("... [diff truncated at ~500 lines — 600 total] ..."));
    }
}
