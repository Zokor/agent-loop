# agent-loop

A CLI that runs a collaborative development loop between two AI coding agents. One implements, the other reviews. They iterate until both agree the work is done.

By default two different agents are used (Claude implements, Codex reviews). In single-agent mode the same agent handles both roles.

## Install

```bash
# Build and install globally
cargo install --path .

# Verify
agent-loop help
```

Requires at least one agent CLI installed and authenticated:

```bash
claude --version    # Claude Code CLI
codex --version     # OpenAI Codex CLI (only needed for dual-agent mode)
```

## Commands

```
agent-loop run <task>                Run the loop on a task
agent-loop run --file <path>         Read task from a file
agent-loop run --single-agent        Use the same agent for both roles
agent-loop plan <task>               Plan and decompose only, no implementation
agent-loop plan --file <path>        Plan from a file
agent-loop resume                    Continue from existing .agent-loop/state without re-init
agent-loop tasks                     Execute all tasks from .agent-loop/state/tasks.md
agent-loop tasks --file <path>       Execute tasks from a custom file
agent-loop init                      Create .agent-loop/state/ in current directory
agent-loop status                    Show current loop status
agent-loop help                      Print usage
```

`agent-loop "task"` also works (shorthand for `agent-loop run "task"`).

### Deprecated forms (still supported)

The following forms continue to work but emit deprecation warnings:

```
agent-loop run --planning-only       → use 'agent-loop plan' instead
agent-loop run --resume              → use 'agent-loop resume' instead
agent-loop run-tasks                 → use 'agent-loop tasks' instead
```

## How It Works

### Implementation Mode (default)

```
Planning → Implementation → Review → Consensus → Done
```

1. **Planning** — Implementer proposes a plan, reviewer approves or revises (up to `PLANNING_MAX_ROUNDS`)
2. **Implementation** — Implementer writes code based on the agreed plan
3. **Review** — Reviewer checks the code, approves or requests changes
4. **Consensus** — Both agents agree the work is complete
5. **Loop** — If not, iterate until consensus or MAX_ROUNDS

### Planning-Only Mode (`plan`)

```
Review Plan → Refine Plan → Consensus → Decompose Tasks → Stop
```

1. **Planning** — Both agents review and refine the plan
2. **Consensus** — Both must agree before proceeding
3. **Task Decomposition** — Break plan into discrete implementable tasks
4. **Output** — Generates `.agent-loop/state/tasks.md`
5. **Stop** — No code is written; run each task separately later

## Examples

```bash
# Simple task
agent-loop run "Add user authentication with JWT tokens"

# Task from file
agent-loop run --file task.md

# Custom settings
MAX_ROUNDS=3 TIMEOUT=600 agent-loop run "Refactor the API layer"

# Swap roles (Codex implements, Claude reviews)
IMPLEMENTER=codex agent-loop run "Build a REST API"

# Single-agent mode
agent-loop run --single-agent "Fix the pagination bug"

# Planning-only: decompose a large plan into tasks
agent-loop plan --file PLAN.md

# Resume an interrupted loop
agent-loop resume

# Resume with higher round limits
DECOMPOSITION_MAX_ROUNDS=6 agent-loop resume

# Run all generated tasks autonomously (resets rounds per task)
agent-loop tasks

# Run all tasks with more retries for MAX_ROUNDS cases
agent-loop tasks --max-retries 4 --round-step 3

# Run tasks from a custom file
agent-loop tasks --file my-tasks.md

# Then implement each task one by one
agent-loop run "Task 1: Foundation setup"
agent-loop run "Task 2: Database schema"
```

## Autonomous Task Runner

After planning mode generates `.agent-loop/state/tasks.md`, execute all tasks in sequence:

```bash
agent-loop tasks
```

Behavior:

- Parses headings like `### Task 1: ...` from `tasks.md`
- Runs each task with `agent-loop run "Task N: ..."` (fresh run, so rounds reset per task)
- If a task stops with `status: MAX_ROUNDS`, retries with `agent-loop resume`
- If a task stops with timeout `status: ERROR` (for example, "timed out after ..."), retries with `agent-loop resume`
- Increases `MAX_ROUNDS` by `--round-step` on each retry

Options:

- `--file <path>`: use a custom tasks markdown file
- `--max-retries <n>`: retry count for retryable failures (`MAX_ROUNDS` and timeout `ERROR`) (default `2`)
- `--round-step <n>`: amount added to `MAX_ROUNDS` on each retry (default `2`)
- `--single-agent`: run each task in single-agent mode
- `--continue-on-fail`: continue with remaining tasks even if one fails
- `--fail-fast`: stop immediately on first task failure
- `--max-parallel <n>`: limit concurrent task execution

> **Note:** `--tasks-file` is deprecated in favor of `--file`.

## Configuration

Settings are resolved in this order (highest precedence first):

1. **CLI flags** (`--single-agent`, `--planning-only`)
2. **Environment variables**
3. **`.agent-loop.toml`** (per-project config file in the project root)
4. **Built-in defaults**

For persistent per-project settings, `.agent-loop.toml` is the preferred approach over environment variables.

### `.agent-loop.toml`

Place this file in your project root. All keys are optional — only set what you want to override:

```toml
# Max implementation/review rounds (default: 20)
max_rounds = 20

# Max planning consensus rounds (default: 3)
planning_max_rounds = 3

# Max task decomposition review rounds (default: 3)
decomposition_max_rounds = 3

# Idle timeout in seconds (default: 600)
timeout = 600

# Which agent implements: "claude" or "codex" (default: "claude")
implementer = "claude"

# Which agent reviews: "claude" or "codex" (default: opposite of implementer)
reviewer = "codex"

# Use same agent for both roles (default: false)
single_agent = false

# Auto-commit loop-owned changes after each round (default: true)
auto_commit = true

# Run quality checks before review (default: false)
auto_test = false

# Override auto-detected quality check command
# auto_test_cmd = "cargo test"

# Plan and decompose only, no implementation (default: false)
planning_only = false
```

Unknown keys are rejected to catch typos early. If the file is missing, everything falls back to defaults silently.

### Precedence examples

```bash
# .agent-loop.toml sets max_rounds = 10

# TOML wins over default (10 instead of 20)
agent-loop run "task"

# Env wins over TOML (50 instead of 10)
MAX_ROUNDS=50 agent-loop run "task"

# CLI flag wins over everything
# --single-agent forces single-agent even if SINGLE_AGENT=0 in env
# or single_agent = false in TOML
agent-loop run --single-agent "task"

# Env overrides TOML for planning_only too
# (planning_only = false in .agent-loop.toml, env wins)
PLANNING_ONLY=1 agent-loop run "task"
```

### Environment variables

Environment variables override `.agent-loop.toml` values but are overridden by CLI flags.

| Variable                   | Default  | Description                                                      |
| -------------------------- | -------- | ---------------------------------------------------------------- |
| `MAX_ROUNDS`               | `20`     | Max implementation/review cycles (exits early on consensus)      |
| `PLANNING_MAX_ROUNDS`      | `3`      | Max planning consensus rounds                                    |
| `DECOMPOSITION_MAX_ROUNDS` | `3`      | Max task decomposition review rounds                              |
| `TIMEOUT`                  | `600`    | Idle timeout in seconds (resets on any agent output)             |
| `IMPLEMENTER`              | `claude` | Which agent implements: `claude` or `codex`                      |
| `REVIEWER`                 |          | Which agent reviews: `claude` or `codex` (default: opposite of implementer) |
| `SINGLE_AGENT`             | `0`      | Use same agent for both roles (`1`/`true`/`yes`/`on` to enable)  |
| `AUTO_COMMIT`              | `1`      | Auto-commit loop-owned changes after each round (`0` to disable) |
| `AUTO_TEST`                | `0`      | Run quality checks before review (`1`/`true`/`yes`/`on` to enable) |
| `AUTO_TEST_CMD`            |          | Override auto-detected quality check command                     |
| `PLANNING_ONLY`            | `0`      | Plan and decompose only, no implementation (`1`/`true`/`yes`/`on` to enable) |

## Per-Project State

Running `agent-loop init` or `agent-loop run` creates `.agent-loop/state/` in the current directory:

```
.agent-loop/
└── state/
    ├── task.md        # Original task description
    ├── plan.md        # Agreed development plan
    ├── tasks.md       # Task breakdown (planning-only mode)
    ├── changes.md     # Summary of latest implementation changes
    ├── review.md      # Latest review feedback
    ├── status.json    # Loop state: status, round, actors, timestamp
    ├── workflow.txt   # Workflow marker (plan or run) for resume
    └── log.txt        # Full timestamped session log
```

Add `.agent-loop/state/` to your project's `.gitignore` — state is ephemeral.

The tool binary lives globally. Only state files are created per-project.

## When to Use Which Mode

**Implementation mode** — Task is well-defined and scoped. You know what to build. Ready to write code.

**Planning-only mode** — Starting a new project, need architecture review, plan is too large for a single run (>500 lines or >5 phases). Use `agent-loop plan` first, then implement each task from the generated `tasks.md`.

## Single-Agent vs Dual-Agent

**Dual-agent (default)** — Two different agents (Claude + Codex). Independent review from a different model catches more issues. Use for complex tasks.

**Single-agent (`--single-agent`)** — Same agent for both roles. A hardening preamble is injected into reviewer prompts to counteract self-review bias, instructing the agent to evaluate code as if written by someone else. Use when you only have one CLI or the task is straightforward.

## Exit Codes

- `0` — Consensus reached
- `1` — Max rounds hit, decomposition failed, or error

## Git Integration

- Auto-commits scoped changes after each implementation round with `agent-loop: <message>` prefix
- Excludes `.agent-loop/state/` from commits
- Only commits files created/modified by the loop (not pre-existing changes)
- Disable with `AUTO_COMMIT=0`
