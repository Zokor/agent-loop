# agent-loop

`agent-loop` is a CLI for iterative AI-assisted delivery with explicit phases:

1. `plan`: create/refine the implementation plan only
2. `tasks`: decompose the plan into implementable tasks only
3. `implement`: execute implementation work only

It persists session state under `.agent-loop/`, captures reusable decisions in `decisions.md`, and can run an optional post-consensus compound learning phase.

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
agent-loop implement --task "Task 1: ..."
agent-loop implement --file <task.md>
agent-loop implement --resume
```

`agent-loop implement` (without flags) reads `.agent-loop/state/tasks.md` and executes all tasks in one implementation/review loop.
Use `agent-loop implement --per-task` for legacy one-task-at-a-time execution.

If no tasks file exists, it errors with:

```text
No tasks found. Run 'agent-loop tasks' first.
```

### `reset`

Clear `.agent-loop/state/` and recreate it empty.

```bash
agent-loop reset
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

Implementation rounds use reviewer approval + implementer self-review before consensus:

```text
implement round
  -> reviewer review
  -> if approved: implementer self-review
     -> CONSENSUS or DISPUTED
```

In dual-agent mode, a reviewer 5/5 triggers adversarial review first, then still runs implementer self-review (no dual-agent auto-consensus shortcut).

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

On `MAX_ROUNDS` and `ERROR` implementation exits, `agent-loop` appends a struggle signal to `decisions.md`:

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

implementer = "claude" # or "codex"
reviewer = "codex"     # or "claude"
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

- `MAX_ROUNDS`
- `PLANNING_MAX_ROUNDS`
- `DECOMPOSITION_MAX_ROUNDS`
- `TIMEOUT`
- `IMPLEMENTER`
- `REVIEWER`
- `SINGLE_AGENT`
- `AUTO_COMMIT`
- `AUTO_TEST`
- `AUTO_TEST_CMD`
- `COMPOUND`
- `DECISIONS_MAX_LINES`
- `DIFF_MAX_LINES`
- `CONTEXT_LINE_CAP`
- `PLANNING_CONTEXT_EXCERPT_LINES`
- `BATCH_IMPLEMENT`
- `VERBOSE`

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
```

`reset` only clears `state/`; `decisions.md` is preserved.

## Repository Knowledge Context

When building planning context, `agent-loop` reads these files if present (within line budget):

1. `README.md`
2. `CLAUDE.md`
3. `ARCHITECTURE.md`
4. `CONVENTIONS.md`
5. `AGENTS.md`
