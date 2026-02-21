use std::collections::VecDeque;
use std::fmt;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur when building a wave schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaveError {
    /// References a task index that does not exist.
    InvalidReference(usize),
    /// A task lists itself as a dependency.
    SelfReference(usize),
    /// A circular dependency was detected among the listed tasks.
    CyclicDependency(Vec<usize>),
}

impl fmt::Display for WaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaveError::InvalidReference(idx) => {
                write!(f, "invalid reference: task {} does not exist", idx)
            }
            WaveError::SelfReference(idx) => {
                write!(f, "self reference: task {} depends on itself", idx)
            }
            WaveError::CyclicDependency(tasks) => {
                let ids: Vec<String> = tasks.iter().map(|t| t.to_string()).collect();
                write!(
                    f,
                    "cyclic dependency detected among tasks: [{}]",
                    ids.join(", ")
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Schedule
// ---------------------------------------------------------------------------

/// The result of dependency analysis: tasks grouped into waves of parallelism.
///
/// * `waves[i]`     – the task indices that belong to wave *i*.
/// * `task_wave[j]` – the wave index assigned to task *j*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveSchedule {
    pub waves: Vec<Vec<usize>>,
    pub task_wave: Vec<usize>,
}

// ---------------------------------------------------------------------------
// Dependency parsing
// ---------------------------------------------------------------------------

/// Scan the first 3 non-blank lines of `content` for a line matching
/// `depends: N, M, ...` (case-insensitive).  Returns 0-indexed task indices.
///
/// The input numbers are **1-indexed** (human-facing); the returned vector
/// contains **0-indexed** values (subtract 1).
pub fn parse_dependencies(content: &str) -> Vec<usize> {
    let mut non_blank_seen = 0usize;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        non_blank_seen += 1;
        if non_blank_seen > 3 {
            break;
        }

        // Case-insensitive check for the `depends:` prefix.
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("depends") {
            let rest = rest.trim_start();
            if let Some(after_colon) = rest.strip_prefix(':') {
                // `after_colon` is the lowercase copy; we only need numeric
                // values so case doesn't matter.
                return after_colon
                    .split(',')
                    .filter_map(|tok| tok.trim().parse::<usize>().ok())
                    .filter(|&n| n > 0)
                    .map(|n| n - 1)
                    .collect();
            }
        }
    }

    Vec::new()
}

// ---------------------------------------------------------------------------
// Wave computation (Kahn's algorithm + longest-path levelling)
// ---------------------------------------------------------------------------

/// Build a wave schedule from a set of tasks and their dependencies.
///
/// * `task_count`   – total number of tasks (indexed `0..task_count`).
/// * `dependencies` – `dependencies[i]` lists the tasks that task *i* depends
///   on (i.e. must complete **before** task *i* can start).
///
/// Returns a [`WaveSchedule`] on success, or a [`WaveError`] if the
/// dependency graph is invalid.
pub fn compute_wave_schedule(
    task_count: usize,
    dependencies: &[Vec<usize>],
) -> Result<WaveSchedule, WaveError> {
    // Trivial case: nothing to schedule.
    if task_count == 0 {
        return Ok(WaveSchedule {
            waves: Vec::new(),
            task_wave: Vec::new(),
        });
    }

    // ------------------------------------------------------------------
    // 1. Validate and build the forward adjacency list + in-degree array.
    //    Edge semantics: if task `v` depends on task `u`, we store the
    //    edge `u -> v` (u must finish before v).
    // ------------------------------------------------------------------
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); task_count];
    let mut in_degree: Vec<usize> = vec![0; task_count];

    for (v, deps) in dependencies.iter().enumerate() {
        for &u in deps {
            if u == v {
                return Err(WaveError::SelfReference(v));
            }
            if u >= task_count {
                return Err(WaveError::InvalidReference(u));
            }
            adj[u].push(v);
            in_degree[v] += 1;
        }
    }

    // ------------------------------------------------------------------
    // 2. Kahn's algorithm – topological sort + cycle detection.
    // ------------------------------------------------------------------
    let mut queue: VecDeque<usize> = VecDeque::new();
    for (i, &deg) in in_degree.iter().enumerate() {
        if deg == 0 {
            queue.push_back(i);
        }
    }

    let mut topo_order: Vec<usize> = Vec::with_capacity(task_count);

    while let Some(u) = queue.pop_front() {
        topo_order.push(u);
        for &v in &adj[u] {
            in_degree[v] -= 1;
            if in_degree[v] == 0 {
                queue.push_back(v);
            }
        }
    }

    if topo_order.len() != task_count {
        // Collect every node that was *not* emitted – they form the cycle.
        let in_topo: Vec<bool> = {
            let mut flags = vec![false; task_count];
            for &idx in &topo_order {
                flags[idx] = true;
            }
            flags
        };
        let cycle_members: Vec<usize> = (0..task_count).filter(|&i| !in_topo[i]).collect();
        return Err(WaveError::CyclicDependency(cycle_members));
    }

    // ------------------------------------------------------------------
    // 3. Longest-path levelling to determine wave indices.
    //    wave[v] = max(wave[u] + 1) for every dependency edge u -> v.
    //    Nodes with no dependencies sit in wave 0.
    // ------------------------------------------------------------------
    let mut wave_level: Vec<usize> = vec![0; task_count];

    for &u in &topo_order {
        for &v in &adj[u] {
            let candidate = wave_level[u] + 1;
            if candidate > wave_level[v] {
                wave_level[v] = candidate;
            }
        }
    }

    // ------------------------------------------------------------------
    // 4. Group tasks by their wave level.
    // ------------------------------------------------------------------
    let max_wave = wave_level.iter().copied().max().unwrap_or(0);
    let mut waves: Vec<Vec<usize>> = vec![Vec::new(); max_wave + 1];
    for (task, &w) in wave_level.iter().enumerate() {
        waves[w].push(task);
    }

    Ok(WaveSchedule {
        waves,
        task_wave: wave_level,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_dependencies -------------------------------------------------

    #[test]
    fn parse_depends_basic() {
        let deps = parse_dependencies("depends: 1, 3");
        assert_eq!(deps, vec![0, 2]);
    }

    #[test]
    fn parse_depends_none() {
        let deps = parse_dependencies("title: my task\ndescription: nothing here\n");
        assert!(deps.is_empty());
    }

    #[test]
    fn parse_depends_case_insensitive() {
        let deps = parse_dependencies("Depends: 2");
        assert_eq!(deps, vec![1]);
    }

    #[test]
    fn parse_depends_only_first_3_non_blank() {
        // The depends line is the 4th non-blank line and should be ignored.
        let content = "line one\nline two\nline three\ndepends: 1, 2\n";
        let deps = parse_dependencies(content);
        assert!(deps.is_empty());
    }

    #[test]
    fn parse_depends_skips_blank_lines() {
        // Blank lines should not count towards the 3-line limit.
        let content = "\n\nfirst\n\nsecond\ndepends: 4\n";
        let deps = parse_dependencies(content);
        assert_eq!(deps, vec![3]);
    }

    // -- compute_wave_schedule: success cases --------------------------------

    #[test]
    fn empty_task_list() {
        let schedule = compute_wave_schedule(0, &[]).unwrap();
        assert!(schedule.waves.is_empty());
        assert!(schedule.task_wave.is_empty());
    }

    #[test]
    fn no_deps_all_wave_zero() {
        let deps: Vec<Vec<usize>> = vec![vec![], vec![], vec![]];
        let schedule = compute_wave_schedule(3, &deps).unwrap();
        assert_eq!(schedule.waves.len(), 1);
        assert_eq!(schedule.task_wave, vec![0, 0, 0]);
        // All three tasks in wave 0.
        let mut wave0 = schedule.waves[0].clone();
        wave0.sort();
        assert_eq!(wave0, vec![0, 1, 2]);
    }

    #[test]
    fn linear_chain_three_waves() {
        // 0 -> 1 -> 2  (task 1 depends on 0, task 2 depends on 1)
        let deps = vec![vec![], vec![0], vec![1]];
        let schedule = compute_wave_schedule(3, &deps).unwrap();
        assert_eq!(schedule.waves.len(), 3);
        assert_eq!(schedule.task_wave, vec![0, 1, 2]);
        assert_eq!(schedule.waves[0], vec![0]);
        assert_eq!(schedule.waves[1], vec![1]);
        assert_eq!(schedule.waves[2], vec![2]);
    }

    #[test]
    fn diamond_two_waves() {
        // Tasks 0 and 1 have no deps; task 2 depends on both 0 and 1.
        let deps = vec![vec![], vec![], vec![0, 1]];
        let schedule = compute_wave_schedule(3, &deps).unwrap();
        assert_eq!(schedule.waves.len(), 2);
        assert_eq!(schedule.task_wave[2], 1);
        assert_eq!(schedule.task_wave[0], 0);
        assert_eq!(schedule.task_wave[1], 0);
    }

    // -- compute_wave_schedule: error cases ----------------------------------

    #[test]
    fn self_reference_error() {
        let deps = vec![vec![0]];
        let result = compute_wave_schedule(1, &deps);
        assert_eq!(result, Err(WaveError::SelfReference(0)));
    }

    #[test]
    fn cycle_error() {
        // 0 -> 1 -> 0
        let deps = vec![vec![1], vec![0]];
        let result = compute_wave_schedule(2, &deps);
        match result {
            Err(WaveError::CyclicDependency(members)) => {
                assert!(members.contains(&0));
                assert!(members.contains(&1));
            }
            other => panic!("expected CyclicDependency, got {:?}", other),
        }
    }

    #[test]
    fn invalid_reference_error() {
        let deps = vec![vec![5]];
        let result = compute_wave_schedule(1, &deps);
        assert_eq!(result, Err(WaveError::InvalidReference(5)));
    }

    // -- Display impl -------------------------------------------------------

    #[test]
    fn display_invalid_reference() {
        let err = WaveError::InvalidReference(7);
        assert_eq!(err.to_string(), "invalid reference: task 7 does not exist");
    }

    #[test]
    fn display_self_reference() {
        let err = WaveError::SelfReference(3);
        assert_eq!(
            err.to_string(),
            "self reference: task 3 depends on itself"
        );
    }

    #[test]
    fn display_cyclic_dependency() {
        let err = WaveError::CyclicDependency(vec![0, 1, 2]);
        assert_eq!(
            err.to_string(),
            "cyclic dependency detected among tasks: [0, 1, 2]"
        );
    }
}
