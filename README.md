# agent-loop

`agent-loop` is a CLI for iterative AI-assisted delivery with explicit phases:

1. `plan`: create/refine the implementation plan only
2. `tasks`: decompose the plan into implementable tasks only
3. `implement`: execute implementation work only

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

Experimental agents emit a warning at startup:

```text
Warning: 'gemini' is experimental and may not work correctly.
```

## Commands

### `plan`

Plan only. No decomposition and no implementation rounds.

```bash
agent-loop plan <task>
agent-loop plan --file <path>
```

### `tasks`

Decompose only. Uses `.agent-loop/state/plan.md` by default.

```bash
agent-loop tasks
agent-loop tasks --resume
agent-loop tasks --file <path-to-plan.md>
```

If no plan exists, it errors with:

```text
No plan found. Run 'agent-loop plan' first.
```

### `implement`

Implementation only.

```bash
agent-loop implement
agent-loop implement --per-task
agent-loop implement --wave
agent-loop implement --wave --fail-fast
agent-loop implement --wave --resume
agent-loop implement --task "Task 1: ..."
agent-loop implement --file <task.md>
agent-loop implement --resume
```

`agent-loop implement` (without flags) runs in batch mode:
- Uses `.agent-loop/state/tasks.md` when present and non-empty.
- Falls back to `.agent-loop/state/plan.md` when `tasks.md` is missing or empty.

Use `agent-loop implement --per-task` for legacy one-task-at-a-time execution.
Use `agent-loop implement --wave` for dependency-aware parallel execution (see Wave Mode below).
When `batch_implement = false` (or `--per-task` / `--wave` is used), `tasks.md` is mandatory and plan fallback is disabled.

If both `tasks.md` and `plan.md` are unavailable in plain batch mode, it errors with:

```text
No tasks found and no plan found. Run 'agent-loop plan' first, or generate tasks with 'agent-loop tasks'.
```

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

### Other

```bash
agent-loop status
agent-loop version
agent-loop help
```

## Workflow

```text
plan  ->  tasks  ->  implement
```

Consensus/signoff model:

```text
Plan/tasks single-agent:
  reviewer APPROVED -> system finalizes CONSENSUS

Plan/tasks dual-agent:
  reviewer APPROVED -> implementer signoff (CONSENSUS or DISPUTED)

Implement single-agent:
  reviewer APPROVED -> system finalizes CONSENSUS
  (5/5 remains auto-consensus)

Implement dual-agent:
  reviewer APPROVED -> implementer signoff (CONSENSUS or DISPUTED)
```

## Wave Mode

Wave mode (`--wave`) enables dependency-aware parallel task execution. Tasks declare dependencies in `tasks.md`:

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

Tasks are grouped into waves using topological sort with longest-path levelling. Tasks within the same wave run in parallel (bounded by `MAX_PARALLEL`). Wave barriers ensure all tasks in wave N complete before wave N+1 starts.

Features:
- Per-task state isolation (`.agent-loop/state/task-{index}/`)
- Git checkpoints at wave boundaries only (serialized by main thread)
- Failure propagation: failed tasks cause transitive dependents to be skipped
- `--fail-fast`: stop all execution on first failure
- `--resume`: skip done tasks, re-run pending/running, re-evaluate skipped
- Wave lock prevents concurrent wave runs (stale lock detection via PID liveness)
- Progress journal (`wave-progress.jsonl`) records all wave events

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

By default, the reviewer agent runs with read-only tools: `Read,Grep,Glob,WebFetch`. This prevents reviewers from modifying the codebase during review.

Override via TOML or env:

```toml
reviewer_allowed_tools = "Read,Grep,Glob,WebFetch,Bash"
```

```bash
REVIEWER_ALLOWED_TOOLS="Read,Grep,Glob" agent-loop implement
```

## Progressive Context

When enabled, replaces front-loaded project context (README, decisions, history) with a compact state manifest listing available context files with absolute paths. Agents can read files on-demand.

```bash
PROGRESSIVE_CONTEXT=1 agent-loop implement
```

## Stuck Detection

Detects implementation loops that are not making progress:

```toml
stuck_detection_enabled = true
stuck_no_diff_rounds = 3        # consecutive no-diff rounds before signal
stuck_threshold_minutes = 10    # wall-clock minutes before signal
stuck_action = "warn"           # abort | warn | retry
```

Detection methods:
- **No-diff tracking**: counts consecutive rounds with empty diffs
- **Oscillation detection**: FNV-1a hashing detects when the same diff repeats every 2 rounds
- **Time threshold**: wall-clock elapsed time since loop start

Actions:
- `abort`: record struggle signal, write `Status::Stuck`, terminate loop
- `warn`: record struggle signal, log warning, continue
- `retry`: skip reviewer for this round, continue to next implementer round

## Planning Loop Reliability

The planning phase uses a structured findings protocol:

- Reviewer emits `VERDICT: APPROVED` or `VERDICT: REVISE` with a JSON findings block
- Findings are tracked in `planning_findings.json` with IDs (`P-001`, `P-002`, ...)
- Open findings from prior rounds are fed back into the next planning round
- Safety nets: `REVISE` with empty findings synthesizes `P-001`; `APPROVED` with open findings forces `NEEDS_REVISION`
- Progress tracked in `planning-progress.md`

## Session Resume

Session persistence is capability-based via the agent registry:

- Session files use workflow-scoped naming: `.agent-loop/state/<workflow>-<role>-<agent>_session_id`
- On resume failure, the stale session file is cleared and a fresh session is started (retry-once pattern)
- Codex session resume extracts `session_id` from JSON output and resumes via `codex exec resume <id>`
- Agents without session support (`supports_session_resume=false`) skip session persistence

```toml
claude_session_persistence = true
codex_session_persistence = true
```

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

### Compound phase

After implementation consensus, `agent-loop` can run a best-effort compound reflection phase to extract reusable learnings into `decisions.md`.

Enable/disable:

- TOML: `compound = false`
- Env: `COMPOUND=0`

### Struggle signals

On `MAX_ROUNDS`, `ERROR`, and `STUCK` implementation exits, `agent-loop` appends a struggle signal to `decisions.md`:

```text
- [STRUGGLE] Task: <task_summary> | Issue: <reason> | Round: <n> | Date: <YYYY-MM-DD>
```

## Configuration

Use `.agent-loop.toml` in project root.

```toml
max_rounds = 20
planning_max_rounds = 3
decomposition_max_rounds = 3
timeout = 600

implementer = "claude"   # any registered agent
reviewer = "codex"       # any registered agent
planner = "claude"       # defaults to implementer when not specified
single_agent = false

auto_commit = true
auto_test = false
auto_test_cmd = "cargo test"

compound = true
decisions_max_lines = 50

max_parallel = 1
batch_implement = true
diff_max_lines = 500
context_line_cap = 200
planning_context_excerpt_lines = 100

# Model selection
implementer_model = "claude-sonnet-4-6"
reviewer_model = "o3"
planner_model = "claude-sonnet-4-6"
planner_permission_mode = "default"

# Reviewer sandbox
reviewer_allowed_tools = "Read,Grep,Glob,WebFetch"

# Progressive context
progressive_context = false

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

## Environment Variables

Core:
- `MAX_ROUNDS` (default: 20)
- `PLANNING_MAX_ROUNDS` (default: 3)
- `DECOMPOSITION_MAX_ROUNDS` (default: 3)
- `TIMEOUT` (default: 600)
- `IMPLEMENTER` (default: claude)
- `REVIEWER` (default: opposite of implementer)
- `PLANNER` (default: same as implementer)
- `SINGLE_AGENT` (default: 0)
- `AUTO_COMMIT` (default: 1)
- `AUTO_TEST` (default: 0)
- `AUTO_TEST_CMD`
- `COMPOUND` (default: 1)
- `DECISIONS_MAX_LINES` (default: 50)
- `DIFF_MAX_LINES`
- `CONTEXT_LINE_CAP`
- `PLANNING_CONTEXT_EXCERPT_LINES`
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
- `CLAUDE_FULL_ACCESS` (default: 0)
- `CLAUDE_ALLOWED_TOOLS` (default: Bash,Read,Edit,Write,Grep,Glob,WebFetch)
- `REVIEWER_ALLOWED_TOOLS` (default: Read,Grep,Glob,WebFetch)
- `CLAUDE_SESSION_PERSISTENCE` (default: 1)
- `CLAUDE_EFFORT_LEVEL`
- `CLAUDE_MAX_OUTPUT_TOKENS`
- `CLAUDE_MAX_THINKING_TOKENS`
- `IMPLEMENTER_EFFORT_LEVEL`
- `REVIEWER_EFFORT_LEVEL`

Codex CLI tuning:
- `CODEX_FULL_ACCESS` (default: 0)
- `CODEX_SESSION_PERSISTENCE` (default: 1)

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
  state/
    task.md
    plan.md
    tasks.md
    changes.md
    review.md
    status.json
    workflow.txt
    log.txt
    conversation.md
    task_status.json
    task_metrics.json
    planning_findings.json
    planning-progress.md
    wave.lock
    wave-progress.jsonl
    task-{index}/          # per-task state dirs (wave mode)
```

`reset` only clears `state/`; `decisions.md` is preserved.

## Repository Knowledge Context

When building planning context, `agent-loop` reads these files if present (within line budget):

1. `README.md`
2. `CLAUDE.md`
3. `ARCHITECTURE.md`
4. `CONVENTIONS.md`
5. `AGENTS.md`

In progressive context mode (`PROGRESSIVE_CONTEXT=1`), these are listed as file pointers in a manifest instead of being embedded inline.
