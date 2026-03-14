# agent-loop

`agent-loop` is a CLI for iterative AI-assisted delivery with explicit phases:

1. `plan`: create/refine the implementation plan only
2. `tasks`: decompose the plan into implementable tasks only
3. `implement`: execute implementation work only

It also supports compound flows for `plan -> tasks -> implement`, `plan -> implement`, and `tasks -> implement`.

It persists session state under `.agent-loop/`, captures reusable decisions in `decisions.md`, and can run an optional post-consensus compound learning phase.

## Supported Agents

`agent-loop` ships with a built-in agent registry. Each agent has a stability tier:

| Agent     | Binary    | Tier         | Default Reviewer |
|-----------|-----------|--------------|------------------|
| claude    | claude    | Stable       | codex            |
| codex     | codex     | Stable       | claude           |
| gemini    | gemini    | Experimental | claude           |
| aider     | aider     | Experimental | claude           |
| qwen      | qwen      | Experimental | claude           |
| vibe      | vibe      | Experimental | claude           |
| deepseek  | deepseek  | Experimental | claude           |
| opencode  | opencode  | Experimental | claude           |

Experimental agents emit a warning at startup:

```text
Warning: 'gemini' is experimental and may not work correctly.
```

Agent capabilities vary by registry entry:

| Capability | claude | codex | opencode | Other Experimental |
|---|---|---|---|---|
| Model flag | Yes (`--model`) | Yes (`-m`) | Yes (`-m`) | No (cleared with warning) |
| Session resume | Yes | Yes | Yes (`--session`) | No |
| Output format | ClaudeStreamJson | JSON | PlainText | PlainText |

## Commands

### `plan`

Plan only. No decomposition and no implementation rounds.

```bash
agent-loop plan <task>
agent-loop plan --file <path>
agent-loop plan --single-agent <task>
```

### `tasks`

Decompose only. Uses `.agent-loop/state/plan.md` by default.

```bash
agent-loop tasks
agent-loop tasks --resume
agent-loop tasks --file <path-to-plan.md>
agent-loop tasks --single-agent
```

`--file` loads a plan from a custom path instead of `.agent-loop/state/plan.md`.

If no plan exists, it errors with:

```text
No plan found. Run 'agent-loop plan' first.
```

### `implement`

Implementation only.

```bash
agent-loop implement
agent-loop implement --per-task
agent-loop implement --per-task --max-retries 3 --round-step 3
agent-loop implement --per-task --continue-on-fail
agent-loop implement --per-task --fail-fast
agent-loop implement --wave
agent-loop implement --wave --max-parallel 4
agent-loop implement --wave --fail-fast
agent-loop implement --wave --resume
agent-loop implement --single-agent
agent-loop implement --task "Task 1: ..."
agent-loop implement --file <task.md>
agent-loop implement --resume
```

For the dependency-aware execution path, see [Wave Mode](#wave-mode) before using `--wave`.

`agent-loop implement` (without flags) runs in batch mode:
- Uses `.agent-loop/state/tasks.md` when present and non-empty.
- Falls back to `.agent-loop/state/plan.md` when `tasks.md` is missing or empty.

Use `agent-loop implement --per-task` for legacy one-task-at-a-time execution.
Use `agent-loop implement --wave` for dependency-aware parallel execution (see [Wave Mode](#wave-mode)).
When `batch_implement = false` (or `--per-task` / `--wave` is used), `tasks.md` is mandatory and plan fallback is disabled.

### Choosing An Implementation Mode

| Mode | Best for | Execution model | Input requirements |
|---|---|---|---|
| `implement` | Small or loosely structured work where one implementation pass is enough | Batch execution over the available task list, with fallback to `plan.md` when `tasks.md` is absent or empty | `tasks.md` preferred; falls back to `plan.md` |
| `implement --per-task` | Sequential task-by-task delivery with explicit retries and failure handling | One task at a time, in listed order | `tasks.md` required |
| `implement --wave` | Task graphs with real dependencies and parallelizable branches | Dependency-aware waves with barriers between waves | `tasks.md` required, with optional `depends:` metadata |

Per-task and wave mode flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--max-retries <N>` | 2 | Maximum retry attempts per task |
| `--round-step <N>` | 2 | Increment `REVIEW_MAX_ROUNDS` per retry |
| `--continue-on-fail` | off | Skip failed tasks and continue to next |
| `--fail-fast` | off | Stop all execution on first failure |
| `--max-parallel <N>` | from config | Max concurrent tasks in wave mode |

`--continue-on-fail` and `--fail-fast` are mutually exclusive.

### Flag Validation Rules

The following combinations are rejected at parse time:

- `--task` and `--file` cannot be used together
- `--resume` cannot be combined with `--task` or `--file`
- `--per-task` cannot be combined with `--task`, `--file`, or `--resume`
- `--wave` and `--per-task` cannot be used together
- `--wave` cannot be combined with `--task` or `--file`
- `--continue-on-fail` and `--fail-fast` cannot be used together
- `--round-step` must be at least 1
- `--max-parallel` must be at least 1

`--wave --resume` is valid and resumes wave execution from `task_status.json`:
- `Done` tasks are preserved and skipped
- Non-`Done` tasks are re-evaluated and re-run as needed

If both `tasks.md` and `plan.md` are unavailable in plain batch mode, it errors with:

```text
No tasks found and no plan found. Run 'agent-loop plan' first, or generate tasks with 'agent-loop tasks'.
```

### `plan-tasks-implement`

Run planning, decomposition, and implementation in one command.

```bash
agent-loop plan-tasks-implement "ship feature"
agent-loop plan-tasks-implement --file task.md
agent-loop plan-tasks-implement --resume
agent-loop plan-tasks-implement "ship feature" --wave --max-parallel 4
```

The resolved implementation mode is persisted before planning starts, so a later `--resume` from `plan` or `tasks` state keeps the original batch / per-task / wave intent.

### `plan-implement`

Run planning and then implementation, skipping decomposition.

```bash
agent-loop plan-implement "ship feature"
agent-loop plan-implement "ship feature" --per-task
agent-loop plan-implement "ship feature" --wave
agent-loop plan-implement --resume
```

`plan-implement` accepts the full implement-mode flag surface. In non-batch mode it synthesizes and persists a single-task `tasks.md` so the existing per-task and wave execution paths can be reused.

### `tasks-implement`

Run decomposition and then implementation from an existing or external `plan.md`.

```bash
agent-loop tasks-implement
agent-loop tasks-implement --file plan.md
agent-loop tasks-implement --resume
```

Fresh runs load `.agent-loop/state/plan.md` unless `--file` is provided. When `--file` is used, persisted `task.md` is ignored and replacement task context is derived from the supplied plan instead. Fresh runs clear the previous state tree after loading the inputs they need to preserve.

### `reset`

Clear `.agent-loop/state/` and recreate it empty.

```bash
agent-loop reset
agent-loop reset --wave-lock   # Force-clear a stale wave lock only
```

Output:

```text
State cleared. decisions.md preserved.
```

### `config init`

Generate a default `.agent-loop.toml` configuration file in the project root.

```bash
agent-loop config init
agent-loop config init --force   # overwrite existing file
```

### Other

```bash
agent-loop status
agent-loop version
agent-loop help
```

### Deprecated Commands

The following subcommands have been removed. Using them produces a parse error:

| Old Command | Replacement |
|---|---|
| `run` | `agent-loop implement` |
| `run-tasks` | `agent-loop implement --per-task` |
| `init` | `agent-loop config init` |
| `resume` | `agent-loop implement --resume` or `agent-loop tasks --resume` |

`tasks --tasks-file` produces a specific migration error directing users to `--file`.

## Workflow

```text
plan  ->  tasks  ->  implement
plan  ->  implement
tasks ->  implement
```

Revision loop (plan, tasks, and implement):

```text
  implementer drafts -> reviewer reviews
    if REVISE/NEEDS_REVISION -> implementer revises -> reviewer re-reviews (loop)
    if APPROVED -> proceed to signoff / next phase
```

The reviewer always gets the last word. When the reviewer requests changes, the implementer revises the artifact (plan, tasks, or code), then the **reviewer re-reviews** — the loop continues until the reviewer approves or the round limit is reached.

Signoff model:

```text
Plan/tasks single-agent:
  reviewer APPROVED -> system finalizes CONSENSUS

Plan/tasks dual-agent:
  reviewer APPROVED -> implementer signoff (CONSENSUS or DISPUTED)

Implement single-agent:
  reviewer APPROVED -> system finalizes CONSENSUS (auto-consensus)

Implement dual-agent:
  reviewer APPROVED (same-context gate)
  -> reviewer APPROVED (fresh-context gate)
  -> implementer signoff (CONSENSUS or DISPUTED)
```

## Wave Mode

Wave mode (`--wave`) is the dependency-aware execution path for `tasks.md`. Instead of running tasks strictly top-to-bottom, `agent-loop` builds a dependency graph, groups independent work into waves, and runs each wave in parallel up to the configured concurrency limit.

Use wave mode when your task list contains real dependencies and you want safe parallelism. If the work is mostly independent but you do not care about dependency ordering, plain batch mode is simpler. If you want one task at a time with retry-focused control, use `--per-task`.

### Declaring dependencies

Tasks declare dependencies in `tasks.md` with a `depends:` line:

```markdown
## Task 1: Foundation
Build the base module.

## Task 2: Extension
depends: 1
Extend the base module.

## Task 3: Documentation
depends: 1, 2
Document everything.
```

Dependency numbers are human-facing and 1-indexed, so `depends: 1, 2` means "do not start this task until Task 1 and Task 2 are done."

The parser looks for `depends:` in the first 3 non-blank lines of the task body. In practice, put dependency declarations at the top of each task so they are unambiguous.

### How scheduling works

`agent-loop` computes a topological schedule, then assigns each task to the earliest possible wave based on the longest dependency chain:

- Tasks with no dependencies go into wave 1.
- A task that depends on wave-1 work lands in wave 2.
- A task only starts after every declared dependency has finished successfully.
- Tasks in the same wave can run concurrently.
- A wave barrier applies between waves: wave N+1 does not start until wave N is finished.

Concurrency is bounded by `--max-parallel`, falling back to `max_parallel` from config/env when the flag is omitted.

Precedence is explicit: `--max-parallel` (CLI) overrides `MAX_PARALLEL` / `.agent-loop.toml`, which overrides the built-in default of `1`.

Example:

```text
Task 1          -> wave 1
Task 2          -> wave 1
Task 3 depends: 1 -> wave 2
Task 4 depends: 1,2 -> wave 2
Task 5 depends: 3,4 -> wave 3
```

### Example: failure propagation in practice

Given this `tasks.md`:

```markdown
## Task 1: Schema groundwork
Create the shared types and persistence layer.

## Task 2: API endpoints
depends: 1
Implement the server endpoints.

## Task 3: Frontend integration
depends: 1
Connect the UI to the shared model.

## Task 4: End-to-end verification
depends: 2, 3
Add integration coverage for the full flow.
```

The schedule is:

```text
wave 1: Task 1
wave 2: Task 2, Task 3
wave 3: Task 4
```

If Task 2 fails but Task 3 succeeds, the resulting state is:

```text
Task 1 -> Done
Task 2 -> Failed
Task 3 -> Done
Task 4 -> Skipped ("dependency failed: API endpoints")
```

On `agent-loop implement --wave --resume`, the completed tasks stay `Done`, Task 2 is retried, and Task 4 becomes runnable again only if its dependencies finish successfully.

### Runtime behavior

Each task keeps its own state, metrics, and conversation history under `.agent-loop/state/.wave-task-{index}/`, while wave-level status is tracked centrally in `.agent-loop/state/task_status.json`, `.agent-loop/state/task_metrics.json`, and `wave-progress.jsonl`.

Features:
- Per-task state isolation: every task runs in its own state directory.
- Dependency failure propagation: if a dependency fails, downstream tasks are marked `Skipped` with a `dependency failed: ...` reason instead of running with invalid context.
- `--fail-fast`: stop the run on the first failed task and mark untouched tasks as skipped.
- `--resume`: keep `Done` tasks, reset non-done tasks, and continue from the persisted `task_status.json` state.
- Wave lock: `.agent-loop/wave.lock` prevents overlapping wave runs and can reclaim stale locks when the owning PID is gone or the lock is too old.
- Progress journal: `wave-progress.jsonl` records `RunStart`, `WaveStart`, `TaskStart`, `TaskEnd`, `WaveEnd`, `RunInterrupted`, and `RunEnd`.
- Git checkpoints happen at wave boundaries only, which keeps commits serialized even when tasks inside a wave run in parallel.

### Failure and resume semantics

Wave mode treats dependency order as authoritative:

- A task only runs if all of its declared dependencies finished `Done`.
- If an upstream task fails, dependents are skipped rather than retried prematurely.
- On `agent-loop implement --wave --resume`, previously completed tasks are preserved and skipped; failed or skipped tasks are reconsidered and can run again.
- Resume uses persisted wave metadata, including stored `wave_index` assignments in `task_status.json`.

This makes wave mode useful for long-running implementation passes where you want to recover from interruption or partial failure without rerunning known-good work.

### Wave Interrupt Handling

On `Ctrl+C` during wave execution:
1. Stop launching new tasks
2. Allow in-flight tasks a grace period (`WAVE_SHUTDOWN_GRACE_MS`)
3. SIGTERM then SIGKILL remaining child processes
4. Write `RunInterrupted` event to journal
5. Mark incomplete tasks as skipped in `task_status.json`
6. Release wave lock

## Model Selection

Override the model used by each agent role:

```toml
implementer_model = "claude-sonnet-4-6"
reviewer_model = "o3"
planner_model = "claude-sonnet-4-6"
planner_permission_mode = "default" # "default" | "plan" (Claude planner only)
```

Or via environment variables: `IMPLEMENTER_MODEL`, `REVIEWER_MODEL`, `PLANNER_MODEL`, `PLANNER_PERMISSION_MODE`. The planner agent itself can be selected with `PLANNER` (defaults to the implementer agent when not specified).

Model flags are agent-specific: Claude uses `--model`, Codex uses `-m`. Agents with `supports_model_flag=false` (experimental agents) will have model overrides cleared with a warning at startup.

## Reviewer Sandbox

When full-access mode is disabled (`claude_full_access = false`), the reviewer agent runs with read-only tools by default: `Read,Grep,Glob,WebFetch`. This prevents reviewers from modifying the codebase during review.

> **Important**: Since `claude_full_access` defaults to `true`, the reviewer sandbox is **not active by default**. To enable it, set `claude_full_access = false` — the reviewer will then use `reviewer_allowed_tools` while the implementer uses `claude_allowed_tools`.

Override the reviewer tool list via TOML or env:

```toml
claude_full_access = false
reviewer_allowed_tools = "Read,Grep,Glob,WebFetch"
```

```bash
CLAUDE_FULL_ACCESS=0 REVIEWER_ALLOWED_TOOLS="Read,Grep,Glob" agent-loop implement
```

## Permission Mode Defaults

By default, `agent-loop` runs agents in full-access mode for maximum autonomy:

- **Claude**: uses `--dangerously-skip-permissions` (bypasses tool allowlists)
- **Codex**: uses `--dangerously-bypass-approvals-and-sandbox` (bypasses approval prompts and sandbox)

> **Security warning**: Full-access mode bypasses permission safeguards. Only use in trusted repositories and environments. Agents can read, write, and execute arbitrary code without confirmation prompts.

### How to constrain permissions

To restrict Claude to specific tools:

```toml
claude_full_access = false
claude_allowed_tools = "Bash,Read,Edit,Write,Grep,Glob,WebFetch"
reviewer_allowed_tools = "Read,Grep,Glob,WebFetch"
```

To restrict Codex to its standard approval flow:

```toml
codex_full_access = false
```

Or via environment variables:

```bash
CLAUDE_FULL_ACCESS=0 CODEX_FULL_ACCESS=0 agent-loop implement
```

## Progressive Context

When enabled, replaces front-loaded project context (README, decisions, history) with a compact state manifest listing available context files with absolute paths. Agents can read files on-demand.

```bash
PROGRESSIVE_CONTEXT=1 agent-loop implement
```

## Git Integration

Git is required when `auto_commit = true` or when a `.git` directory exists. A 5-second preflight check runs `git --version` at startup.

### Auto-Commit Checkpoints

When `auto_commit = true`, git checkpoints are created:
- After each implementation round: `round-{N}-implementation: {summary}` (summary capped at 80 chars)
- After consensus: `consensus-round-{N}`
- After max rounds exhausted: `max-rounds-reached`

Files under `.agent-loop/state/` are excluded from commits.

### Diff Generation

Diffs for reviewer prompts are generated with a fallback chain:
1. Committed diff (`git diff baseline..HEAD`) if baseline ref exists and HEAD advanced
2. Working tree diff (`git diff HEAD` + untracked files)
3. Staging area diff (`git diff --cached` + `git diff` + untracked files)

Output is truncated at `diff_max_lines` (configurable, default 500 lines).

## Stuck Detection

Detects implementation loops that are not making progress:

```toml
stuck_detection_enabled = true
stuck_no_diff_rounds = 3        # consecutive no-diff rounds before signal
stuck_threshold_minutes = 10    # wall-clock minutes before signal
stuck_action = "warn"           # abort | warn | retry
```

Detection methods:
- **No-diff tracking**: counts consecutive rounds with empty diffs; resets on any non-empty diff
- **Oscillation detection**: FNV-1a hashing of diffs detects when the same change pattern repeats every 2 rounds (A -> B -> A cycles)
- **Time threshold**: wall-clock elapsed time since loop start

Actions:
- `abort`: record struggle signal, write `Status::Stuck`, terminate loop
- `warn`: record struggle signal, log warning, continue
- `retry`: record struggle signal, skip the reviewer for this round, and continue directly to the next implementer round

## Planning Loop Reliability

The planning phase uses a lightweight verdict protocol:

- Reviewer ends their review with the exact phrase "no findings" (case-insensitive) to approve, or describes issues otherwise
- The system checks the review text for this phrase — no JSON parsing or structured findings needed
- Role swap: if the reviewer keeps finding issues for `planning_role_swap_after` consecutive rounds (default 3), roles swap — the reviewer fixes the plan directly and the implementer reviews
- Progress tracked in CLI-managed `planning-progress.md`, `tasks-progress.md`, and `implement-progress.md`

## Session Resume

Session persistence is capability-based via the agent registry:

- Session files use workflow-scoped naming: `.agent-loop/state/<workflow>-<role>-<agent>_session_id`
- On resume failure, the stale session file is cleared and a fresh session is started (retry-once pattern)
- Codex session resume extracts `session_id` from JSON output and resumes via `codex exec resume <id>`
- Agents without session support (`supports_session_resume=false`) skip session persistence
- In per-task mode, cached session IDs are cleared between tasks to prevent context leakage

```toml
claude_session_persistence = true
codex_session_persistence = true
```

### Resume Behavior

`agent-loop implement --resume` validates that workflow is `implement`, reads persisted implementation mode when available, and resumes the correct implementation path. Direct user-facing per-task implementation resume remains unsupported once the workflow is already in `implement`.

`agent-loop implement --wave --resume` uses wave resume semantics from `task_status.json` (completed tasks are kept; remaining tasks continue).

For non-wave resume, if the state directory is missing or `status.json` is not found, resume returns an error.

## Review Process

agent-loop uses structured review at every phase. Reviewers are instructed to use
their available tools (Read, Grep, Glob) to verify claims against the actual codebase.

### Plan Review

After the planner creates a plan, the reviewer validates it:

1. **Primary review** — Evaluates completeness, accuracy, feasibility, and risks.
   The reviewer must use codebase tools to spot-check plan claims:
   - Verify referenced file paths exist and contain what the plan assumes
   - Verify routes/endpoints return expected content types (HTML vs JSON vs redirect vs binary)
   - Verify database seeders/migrations have correct call chains and dependency order
   - Verify auth flows match actual validation rules (required fields, middleware)
   - Verify API payloads match controller/request validation requirements
   - Verify waiver/exclusion lists are complete

   If the reviewer finds issues, the implementer revises `plan.md` to address them,
   then the reviewer re-reviews. This loop continues until the reviewer ends with
   "no findings" or the round limit is reached. If findings persist for
   `planning_role_swap_after` rounds, roles swap.

2. **Adversarial review** (dual-agent only) — After the first reviewer approves, the
   implementer agent performs an adversarial pass focused on what the first reviewer
   missed. If the adversarial review finds issues, the implementer revises and the
   loop returns to the primary reviewer.
   Skipped in single-agent mode or when `planning_adversarial_review = false`.

### Task Decomposition Review

After plan consensus, the decomposition reviewer validates the task breakdown:
- Task scope and deliverables clarity
- Task size reasonableness
- Dependencies correctly identified and ordered
- Missing tasks
- Testing/verification steps included

If the reviewer returns NEEDS_REVISION, the implementer revises `tasks.md` to address the findings, then the reviewer re-reviews. The loop continues until approved or the round limit is reached.

Task decomposition findings are persisted in `tasks_findings.json` as structured state (`T-xxx` IDs plus `open`/`resolved` lifecycle fields) so resume and cross-round reconciliation can merge findings deterministically.

### Implementation Review

After each implementation round, the review process uses a multi-gate system.
`REVIEW_MAX_ROUNDS` is shared across the full implementation loop (all gates and signoff), not per gate.

**Gate A — Same-Context Review** (all modes):

The reviewer evaluates the implementation in the same conversation context:
- **Correctness** — code matches plan, no bugs or edge cases
- **Tests** — present, sufficient, covering key scenarios
- **Style** — clean, maintainable, follows project conventions
- **Security** — vulnerabilities, error handling adequacy

If `auto_test = true`, quality check results are included in the review prompt.

In single-agent mode, Gate A approval triggers auto-consensus (no further gates).

**Gate B — Fresh-Context Review** (dual-agent only):

Every Gate A approval triggers a mandatory fresh-context reviewer pass using a new session. This prevents the reviewer from rubber-stamping based on familiarity with prior rounds. Uses `F-xxx` finding IDs with severity and `file_refs` evidence.

If Gate B finds issues, a **verification loop** runs: the same fresh-context reviewer re-examines each finding against the actual code and either confirms or withdraws it. If all findings are withdrawn, the review proceeds to signoff. If any are confirmed, the loop returns to the implementer.

Implementation findings are persisted in `findings.json` as structured state because the loop needs stable IDs, severity, file references, and reconciliation rules. Human-readable progress stays in markdown files such as `planning-progress.md`, `tasks-progress.md`, `implement-progress.md`, `log.txt`, and `conversation.md`.

**Gate C — Late Findings Bounce** (dual-agent only):

If the implementer disputes at signoff (returns DISPUTED with late findings), Gate C re-engages the fresh-context reviewer to verify the implementer's claims:
- If the reviewer rejects the late findings → CONSENSUS (late findings dismissed)
- If the reviewer confirms them → loop continues with NEEDS_CHANGES

**Safety nets**:
- Stale timestamp detection: if an agent doesn't write `status.json`, the system defaults to NEEDS_CHANGES (or DISPUTED for signoff)
- Approved-with-unresolved-findings: if a reviewer returns APPROVED but `findings.json` has open issues, the system forces NEEDS_CHANGES
- Empty findings on NEEDS_CHANGES: carries forward prior round findings or synthesizes `F-001` from the status reason

## Decisions And Compound

`agent-loop` persists reusable learnings in:

```text
.agent-loop/decisions.md
```

This file is:

- Read at planning/implementation phase start
- Injected into prompts as `PRIOR DECISIONS & LEARNINGS` (last `decisions_max_lines` lines)
- Referenced automatically from root `AGENTS.md` and `CLAUDE.md` via a managed block
- Preserved by `agent-loop reset`

Decision categories used in prompts:

- `ARCHITECTURE`
- `PATTERN`
- `CONSTRAINT`
- `GOTCHA`
- `DEPENDENCY`

### Disabling Decisions

The entire decisions subsystem can be turned off:

```toml
decisions_enabled = false
```

When disabled:
- `decisions.md` is not created or read
- Decision capture instructions are omitted from prompts
- Struggle signals are not recorded
- Compound learning phase is skipped
- Managed reference blocks are removed from `AGENTS.md`/`CLAUDE.md`

To keep `decisions.md` but disable the automatic reference syncing into project guide files:

```toml
decisions_auto_reference = false
```

### Compound phase

After implementation consensus, `agent-loop` can run a best-effort compound reflection phase to extract reusable learnings into `decisions.md`. The compound phase is non-blocking: if it fails, a warning is logged but the loop completes successfully.

Requires both `compound = true` and `decisions_enabled = true` to run.

Enable/disable:

- TOML: `compound = false`
- Env: `COMPOUND=0`

### Struggle signals

On round-limit, error, and stuck implementation exits, `agent-loop` appends a struggle signal to `decisions.md`:

```text
- [STRUGGLE] Task: <task_summary> | Issue: <reason> | Round: <n> | Date: <YYYY-MM-DD>
```

## Inspecting Model Input/Output

Enable transcript logging to capture the full prompt and response for every agent call:

```toml
transcript_enabled = true
```

Or via env: `TRANSCRIPT_ENABLED=1`.

Transcripts are written to `.agent-loop/state/transcript.log` in a human-readable format with metadata (workflow, phase, round, role, agent). The file is capped at 10,000 lines and auto-rotates by keeping the last half when the limit is reached.

## Configuration

Use `.agent-loop.toml` in project root.

```toml
# Round limits: 0 = unlimited (timeout and stuck detection remain active).
review_max_rounds = 0
planning_max_rounds = 0
decomposition_max_rounds = 0
timeout = 600

implementer = "claude"   # any registered agent
reviewer = "codex"       # any registered agent
planner = "claude"       # defaults to implementer when not specified
single_agent = false

auto_commit = true
auto_test = false
auto_test_cmd = "cargo test"

compound = true
decisions_enabled = false          # default: disabled; set true to enable decisions subsystem
decisions_auto_reference = true
decisions_max_lines = 50

max_parallel = 1
batch_implement = true
diff_max_lines = 500
context_line_cap = 0                    # 0 = unlimited (default); set to limit prompt size
planning_context_excerpt_lines = 0      # 0 = unlimited (default); set to limit per-file excerpts

# Model selection
implementer_model = "claude-sonnet-4-6"
reviewer_model = "o3"
planner_model = "claude-sonnet-4-6"
planner_permission_mode = "default"

# Reviewer sandbox
reviewer_allowed_tools = "Read,Grep,Glob,WebFetch"

# Progressive context
progressive_context = false
planning_adversarial_review = true      # adversarial second review of plans (dual-agent only)

# Observability
transcript_enabled = false

# Session persistence
claude_session_persistence = true
codex_session_persistence = true

# Stuck detection
stuck_detection_enabled = false
stuck_no_diff_rounds = 3
stuck_threshold_minutes = 10
stuck_action = "warn"

# Wave runtime
wave_lock_stale_seconds = 30
wave_shutdown_grace_ms = 30000

[[quality_commands]]
command = "cargo clippy -- -D warnings"
remediation = "Fix all clippy warnings. Run 'cargo clippy --fix' for auto-fixable issues."

[[quality_commands]]
command = "cargo test"
```

### `quality_commands`

When configured, `[[quality_commands]]` overrides auto-detected quality checks.

- `command`: shell command to run (executed with `sh -c` on Unix and `cmd /C` on Windows)
- `remediation` (optional): hint prepended in quality output as `REMEDIATION: ...`

Priority:

1. `quality_commands`
2. `auto_test_cmd`
3. auto-detection by project type

### Quality Check Auto-Detection

When `auto_test = true` and no explicit `quality_commands` or `auto_test_cmd` is configured, checks are auto-detected by project type:

**Rust** (detected by `Cargo.toml`):
- `cargo build`
- `cargo test`
- `cargo clippy -- -D warnings` (only if clippy is installed)

**JavaScript/TypeScript** (detected by `package.json`):
- Scans `package.json` `scripts` for: `build`, `test`, `lint`
- Filters out npm stubs (empty scripts, "no test specified", echo+exit patterns)
- Generates `npm run <script>` commands

**Other projects**: no auto-detection — configure explicitly.

Each check runs with a 120-second timeout. Output is capped at 100 lines via a ring buffer. Results are written to `.agent-loop/state/quality_checks.md` and included in the reviewer prompt.

## Environment Variables

Core:
- `REVIEW_MAX_ROUNDS` (default: 0, unlimited)
- `PLANNING_MAX_ROUNDS` (default: 0, unlimited)
- `DECOMPOSITION_MAX_ROUNDS` (default: 0, unlimited)
- `TIMEOUT` (default: 600)
- `IMPLEMENTER` (default: claude)
- `REVIEWER` (default: opposite of implementer)
- `PLANNER` (default: same as implementer)
- `SINGLE_AGENT` (default: 0)
- `AUTO_COMMIT` (default: 1)
- `AUTO_TEST` (default: 0)
- `AUTO_TEST_CMD`
- `COMPOUND` (default: 1)
- `DECISIONS_ENABLED` (default: 0)
- `DECISIONS_AUTO_REFERENCE` (default: 1)
- `DECISIONS_MAX_LINES` (default: 50)
- `DIFF_MAX_LINES`
- `CONTEXT_LINE_CAP`
- `PLANNING_CONTEXT_EXCERPT_LINES`
- `PLANNING_ADVERSARIAL_REVIEW` (default: 1)
- `BATCH_IMPLEMENT` (default: 1)
- `MAX_PARALLEL` (default: 1)
- `VERBOSE`

Model selection:
- `IMPLEMENTER_MODEL`
- `REVIEWER_MODEL`
- `PLANNER_MODEL`
- `PLANNER_PERMISSION_MODE` (`default` or `plan`)

Progressive context:
- `PROGRESSIVE_CONTEXT` (default: 0)

Claude CLI tuning:
- `CLAUDE_FULL_ACCESS` (default: 1)
- `CLAUDE_ALLOWED_TOOLS` (default: Bash,Read,Edit,Write,Grep,Glob,WebFetch)
- `REVIEWER_ALLOWED_TOOLS` (default: Read,Grep,Glob,WebFetch)
- `CLAUDE_SESSION_PERSISTENCE` (default: 1)
- `CLAUDE_EFFORT_LEVEL` (low | medium | high)
- `CLAUDE_MAX_OUTPUT_TOKENS` (1-64000)
- `CLAUDE_MAX_THINKING_TOKENS` (extended thinking token budget)
- `IMPLEMENTER_EFFORT_LEVEL` (overrides CLAUDE_EFFORT_LEVEL for implementer role)
- `REVIEWER_EFFORT_LEVEL` (overrides CLAUDE_EFFORT_LEVEL for reviewer role)

Codex CLI tuning:
- `CODEX_FULL_ACCESS` (default: 1)
- `CODEX_SESSION_PERSISTENCE` (default: 1)

Observability:
- `TRANSCRIPT_ENABLED` (default: 0)

Stuck detection:
- `STUCK_DETECTION_ENABLED` (default: 0)
- `STUCK_NO_DIFF_ROUNDS` (default: 3)
- `STUCK_THRESHOLD_MINUTES` (default: 10)
- `STUCK_ACTION` (default: warn)

Wave runtime:
- `WAVE_LOCK_STALE_SECONDS` (default: 30)
- `WAVE_SHUTDOWN_GRACE_MS` (default: 30000)

## State Layout

```text
.agent-loop/
  decisions.md
  wave.lock                              # wave run lock (survives reset)
  wave-progress.jsonl                    # wave event journal (survives reset)
  state/
    task.md
    plan.md
    tasks.md
    changes.md
    review.md
    status.json
    findings.json                        # implementation reviewer findings
    planning-progress.md                 # planning phase progress log (CLI-managed)
    tasks-progress.md                    # task decomposition progress log (CLI-managed)
    implement-progress.md                # canonical implementation progress log
    tasks_findings.json                  # task decomposition findings
    quality_checks.md                    # auto-test / quality check results
    workflow.txt
    log.txt
    conversation.md                      # implementation round summaries / recent history (capped at 200 lines)
    data.json                            # reserved placeholder; not used by the current runtime
    task_status.json                     # per-task lifecycle state for per-task/wave execution
    task_metrics.json                    # per-task timing and usage metrics
    transcript.log                       # agent I/O transcript (when transcript_enabled=true)
    .wave-task-{index}/                   # per-task state dirs (wave mode)
```

`implement-progress.md` is canonical in the active implementation state directory:
- batch and sequential per-task runs use `.agent-loop/state/implement-progress.md`
- wave runs use `.agent-loop/state/.wave-task-{index}/implement-progress.md`

`status` reads `workflow.txt`, prints phase-specific resume guidance, and surfaces task-local wave progress files when they exist.

`reset` only clears `state/`; `decisions.md` is preserved.

## Repository Knowledge Context

When building planning context, `agent-loop` reads these files if present (within line budget):

1. `README.md`
2. `CLAUDE.md`
3. `ARCHITECTURE.md`
4. `CONVENTIONS.md`
5. `AGENTS.md`

In progressive context mode (`PROGRESSIVE_CONTEXT=1`), these are listed as file pointers in a manifest instead of being embedded inline.

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Failure |
| 130 | Interrupted by signal (Ctrl+C) |

## Internal Limits

These constants are not configurable but affect runtime behavior:

| Limit | Value | Description |
|-------|-------|-------------|
| Quality check timeout | 120s | Per-command timeout for auto-test checks |
| Quality check output cap | 100 lines | Ring buffer truncation per check |
| Transcript max lines | 10,000 | Auto-rotates keeping last 5,000 lines |
| Conversation context cap | 200 lines | Accumulated context truncation |
| Response block cap | 500 lines | Agent output truncation with warning |
| Checkpoint summary | 80 chars | Commit message summary length |
| Status task text | 500 chars | Task text truncation in status.json |
| High watermark | round 50 | Warning emitted at round 50, then every 25 rounds (unlimited mode) |

## Planner Permission Mode

When `planner_permission_mode = "plan"` (or `PLANNER_PERMISSION_MODE=plan`), the planner agent runs with Claude's `--permission-mode plan` flag. In this mode the planner's output is expected to contain `<plan>...</plan>` markers, and the plan content is extracted from within those tags.

## Migration Notes

### `max_rounds` renamed to `review_max_rounds`

The `max_rounds` config key and `MAX_ROUNDS` environment variable have been renamed to `review_max_rounds` / `REVIEW_MAX_ROUNDS`. Using the old names produces an actionable error:

```text
`max_rounds` was renamed to `review_max_rounds` in .agent-loop.toml. Please update your config file.
`MAX_ROUNDS` was renamed to `REVIEW_MAX_ROUNDS`. Please update your environment variable.
```

### Round limits default to unlimited

`review_max_rounds`, `planning_max_rounds`, and `decomposition_max_rounds` now default to `0` (unlimited). Timeout and stuck detection remain active as safety guardrails. Set a positive value to cap rounds:

```toml
review_max_rounds = 20
planning_max_rounds = 5
decomposition_max_rounds = 5
```

Or via environment variables:

```bash
REVIEW_MAX_ROUNDS=20 PLANNING_MAX_ROUNDS=5 DECOMPOSITION_MAX_ROUNDS=5 agent-loop implement
```

### Full-access mode is now default

`claude_full_access` and `codex_full_access` now default to `true`. See [Permission Mode Defaults](#permission-mode-defaults) for details and how to constrain.
