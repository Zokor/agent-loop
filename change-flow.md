# Change Flow (Next Iteration)

## Goal

Make the CLI workflow explicit and predictable:

1. `plan` only plans.
2. `tasks` only decomposes into tasks.
3. `implement` only implements (single task, task list, or resume).
4. `clean` resets state for a fresh run.

## Command Model

### `plan`

Purpose: generate/refine `.agent-loop/state/plan.md` from task input.

Rules:

1. Does not decompose tasks.
2. Does not run implementation rounds.
3. Produces/updates planning estimate fields in `status.json`.

Examples:

1. `agent-loop plan "Build X"`
2. `agent-loop plan --file docs/plans/feature.md`

### `tasks`

Purpose: decompose an approved `plan.md` into `.agent-loop/state/tasks.md`.

Rules:

1. Reads `plan.md` as primary input.
2. Writes/refines `tasks.md`.
3. No code implementation.
4. Produces/updates decomposition estimate fields in `status.json`.

Examples:

1. `agent-loop tasks`
2. `agent-loop tasks --resume`

### `implement`

Purpose: develop tasks (from `tasks.md`) or a specific task.

Rules:

1. `implement` (no task args): run all tasks from `tasks.md`.
2. `implement --task "Task N: ..."` or `implement --file task.md`: run a single task.
3. `implement --resume`: resume interrupted implementation flow.
4. Writes task outcomes and implementation timing to state.

Examples:

1. `agent-loop implement`
2. `agent-loop implement --task "Task 3: Add waiting list status transitions"`
3. `agent-loop implement --resume`

### `clean`

Purpose: reset `.agent-loop/state` to a clean scaffold (same spirit as current `init`).

Rules:

1. Clears state files used by planning/decomposition/implementation.
2. Recreates required files with empty/default content.
3. Resets `status.json` to `PENDING`.

Examples:

1. `agent-loop clean`

## State Tracking Changes

### `status.json` additions

Add phase-level estimates and outcomes:

1. `estimates`:
   - `planning_ms_estimate`
   - `decomposition_ms_estimate`
   - `implementation_ms_estimate`
   - `total_ms_estimate`
2. `actuals`:
   - `planning_ms_actual`
   - `decomposition_ms_actual`
   - `implementation_ms_actual`
   - `total_ms_actual`
3. `task_outcomes`: array of compact per-task summaries:
   - `task_id`
   - `title`
   - `status` (`done|failed|skipped`)
   - `duration_ms`
   - `retries`
   - `summary`

### File ownership and size guardrail

1. `task_status.json` and `task_metrics.json` remain canonical for full task detail.
2. `status.json` stores a compact outcomes array for quick visibility.
3. If the outcomes array grows large, store only the latest N entries plus totals.

## Deprecated Command Removal

Remove deprecated command paths instead of preserving them.

1. Remove old entrypoints:
   - `run`
   - `run-tasks`
   - legacy `run --planning-only`
   - legacy `run --resume`
2. Keep only:
   - `plan`
   - `tasks`
   - `implement`
   - `clean`
   - `status`
   - `version`
   - `help`
3. For one release window, return explicit migration errors for removed commands:
   - `run` -> "Use `implement`."
   - `run-tasks` -> "Use `implement`."
   - `run --planning-only` -> "Use `plan`."
   - `run --resume` -> "Use `implement --resume` or `tasks --resume`."

## Acceptance Criteria

1. `plan`, `tasks`, `implement`, `clean` each do one job only.
2. `implement --resume` resumes implementation deterministically.
3. `status.json` shows estimates + actuals + compact task outcomes.
4. Existing detailed task state remains persisted in `task_status.json` and `task_metrics.json`.
5. Removed commands fail fast with clear migration guidance.
