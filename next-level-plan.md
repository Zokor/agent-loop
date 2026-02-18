# Agent-Loop Enhancements: Memory, Article Insights, and Review Flow

## Context

Three improvements to the agent-loop, prompted by the ideas in `new-ideas.md`:
1. Add persistent project memory that agents can write to mid-session and that compounds across sessions
2. Adopt missing concepts from OpenAI's Harness Engineering and Ryan Carson's Compound Engineering
3. Strengthen the review flow so both agents actively review and agree

---

## 1. Compound Engineering: Persistent Project Memory

### Problem
Agents currently have no way to record important decisions, patterns discovered, or lessons learned. The only "memory" is `conversation.md` (capped at 200 lines, session-scoped round summaries). When a new session starts, all prior context is lost. There's no self-improving loop — the agent makes the same mistakes repeatedly.

### Key Insight (from Ryan Carson's Compound Engineering)
Each session should end with an explicit "compound" step where the agent extracts learnings (patterns, gotchas, architectural decisions, constraints) into persistent files that future sessions read. This creates a compounding knowledge loop: every session makes future sessions better.

### Proposal: Two-Layer Memory System

#### Layer 1: Mid-Session Decision Capture (`decisions.md`)

**Location:** `.agent-loop/decisions.md` (persists across sessions, NOT cleared by `clean`)

**How it works during a session:**
1. Add prompt instructions to implementer and reviewer prompts: "If you make an important architectural decision, discover a constraint, choose a pattern, or hit a gotcha — append a one-line entry to `.agent-loop/decisions.md` with format: `- [CATEGORY] description` where CATEGORY is one of: ARCHITECTURE, PATTERN, CONSTRAINT, GOTCHA, DEPENDENCY"
2. On each new run, read `decisions.md` and inject the last N lines into planning/implementation prompts as a `PRIOR DECISIONS & LEARNINGS` section
3. Cap prompt injection at configurable lines (default: 50)

#### Layer 2: Post-Consensus Compound Phase

**After implementation reaches CONSENSUS**, add an optional "compound" phase:

1. Prompt the implementer agent: "Review the full session — the task, plan, implementation rounds, review feedback, and any struggles. Extract learnings that would help future sessions working on this codebase. Append them to `.agent-loop/decisions.md` with categories: PATTERN (reusable approach), GOTCHA (pitfall to avoid), CONSTRAINT (limitation discovered), ARCHITECTURE (structural decision)."
2. This runs once, after consensus, before the final summary. No review loop needed — it's a knowledge extraction step, not code.
3. Controlled by config: `COMPOUND=1` (env) or `compound = true` (TOML). Default: `true`.

#### Struggle Signal Capture (from Harness Engineering)

When implementation hits MAX_ROUNDS or ERROR, automatically append to `decisions.md`:
```
- [STRUGGLE] Task: <summary> | Issue: <last reviewer reason> | Round: <N> | Date: <date>
```
Future sessions see what went wrong and can avoid the same issues.

**Files to modify:**
- `src/prompts.rs`:
  - Add decision-writing instructions to `implementation_implementer_prompt()` and `implementation_reviewer_prompt()`
  - Add `PRIOR DECISIONS & LEARNINGS` section to `planning_initial_prompt()`, `implementation_implementer_prompt()`, and reviewer prompts
  - Add new `compound_prompt()` function for the post-consensus compound phase
- `src/state.rs`:
  - Add `read_decisions(config, max_lines) -> String` helper
  - Add `append_decision(entry, config)` helper
  - Add `decisions_path(config) -> PathBuf` helper (at `.agent-loop/decisions.md`, outside `state/`)
  - `init` creates `decisions.md` if missing during first-time folder setup
  - `clean`/`reset` only clears `.agent-loop/state/` — `decisions.md` lives outside `state/` and is naturally preserved
- `src/phases.rs`:
  - Read decisions before each phase and pass to prompt builders
  - Add `compound_phase()` function called after implementation consensus within the `implement` command
  - Add struggle signal capture in max-rounds/error branches of `implementation_loop()`
- `src/config.rs`:
  - Add `compound: bool` (default: true)
  - Add `decisions_max_lines: u32` (default: 50)

---

## 2. Additional Concepts from Articles

### What We Already Have
- Agent-first development (the whole system)
- Task decomposition (similar to Carson's PRD-to-tasks)
- Quality checks via `AUTO_TEST`
- Repository context via README.md/CLAUDE.md excerpts
- Adversarial second review on perfect scores
- Round history injection

### Gaps to Address (Ordered by Impact)

#### 2a. Expanded Repository Knowledge — "Golden Principles" (from Harness Engineering)

**Gap:** We only read README.md and CLAUDE.md. OpenAI encodes "golden principles" in the repo and reads them. Carson's system updates AGENTS.md as the living knowledge base.

**Proposal:** Extend `gather_project_context()` to also read these files if present:
- `ARCHITECTURE.md` — system design overview
- `CONVENTIONS.md` — coding conventions
- `AGENTS.md` — agent-specific instructions (OpenAI's standard)

This is a trivial change — just add more `append_file_excerpt()` calls.

**Files to modify:**
- `src/prompts.rs` — Add `append_file_excerpt()` calls for ARCHITECTURE.md, CONVENTIONS.md, AGENTS.md in `gather_project_context()`

#### 2b. Agent-Friendly Error Messages in Quality Checks (from Harness Engineering)

**Gap:** OpenAI designs linter error messages to "inject remediation instructions into agent context." Our `run_quality_checks()` dumps raw test output. Agents see errors but not how to fix them.

**Proposal:** Add support for custom quality commands with remediation hints in config:

```toml
[[quality_commands]]
command = "cargo clippy -- -D warnings"
remediation = "Fix all clippy warnings. Run 'cargo clippy --fix' for auto-fixable issues."

[[quality_commands]]
command = "npm run lint"
remediation = "Fix lint errors following the project's ESLint config."
```

Remediation text gets prepended to quality check output in prompts.

**Files to modify:**
- `src/config.rs` — Add `quality_commands` config field with `command` + `remediation` structs
- `src/phases.rs` — Modify `run_quality_checks()` to use custom commands when configured, prepend remediation hints to output

#### 2c. Concepts We Don't Need to Implement

| Concept | Source | Why Skip |
|---|---|---|
| Nightly autonomous loop | Carson | `implement` already runs tasks autonomously. Scheduling (launchd/cron) is outside scope — users can wrap `agent-loop implement` in their own scheduler. |
| Doc-gardening agent | Harness Eng. | Nice-to-have but low priority. The compound phase partially addresses this by capturing what changed. |
| "Don't outsource thinking" | Carson | Already addressed by the planning phase — human provides the task, agents plan and implement. |
| Priority-driven backlog | Carson | Outside scope — agent-loop processes one task/task-list at a time. Backlog management belongs in external tools. |

---

## 3. Review Flow: Both Agents Actively Reviewing and Agreeing

### Current Flow
```
Implementer writes code
    → Reviewer reviews (APPROVED / NEEDS_CHANGES)
        → If APPROVED:
            - Single-agent 5/5: auto-consensus (no implementer input)
            - Dual-agent 5/5: adversarial second review → auto-consensus (no implementer input)
            - Non-5/5 APPROVED: Implementer consensus check (can DISPUTE)
        → If NEEDS_CHANGES: back to implementer
```

**Problems:**
1. The consensus prompt is passive: "If you agree the implementation is complete and the review is fair, write CONSENSUS." — encourages rubber-stamping.
2. On the dual-agent 5/5 path, the implementer never gets a say at all. Two reviewer passes agree, but the code author never confirms. (`phases.rs` line 1619-1646 auto-writes CONSENSUS after adversarial approval.)

### Proposal: Two changes

#### 3a. Active Self-Review in Consensus Prompt

Rewrite `implementation_consensus_prompt()` so the implementer performs an active review instead of rubber-stamping:

**Current:**
> "The reviewer has APPROVED your implementation. If you agree the implementation is complete and the review is fair, write CONSENSUS..."

**Proposed:**
> "The reviewer has APPROVED your implementation. Before confirming consensus, perform your own final review:
>
> 1. Re-read the original TASK requirements — verify every requirement is met
> 2. Check for edge cases, error handling gaps, or missing tests the reviewer may have overlooked
> 3. Verify the code follows project conventions and the agreed plan
> 4. Look for any regressions or unintended side effects
>
> Write a brief summary of your self-review findings.
>
> If everything checks out, write CONSENSUS. If you find issues the reviewer missed, write DISPUTED with specific details of what was missed."

**Signature change:** `implementation_consensus_prompt()` needs `task` and `plan` parameters added (currently only receives `review`) so the implementer can verify against requirements.

#### 3b. Always Run Implementer Self-Review (Including After Adversarial Approval)

Remove the auto-consensus shortcut on the dual-agent 5/5 path. After adversarial approval, fall through to the implementer active self-review instead of immediately writing CONSENSUS.

**New unified flow:**
```
Implementer writes code
    → Reviewer reviews (APPROVED / NEEDS_CHANGES)
        → If APPROVED:
            - Single-agent 5/5: auto-consensus (unchanged — same agent, self-review adds no value)
            - Dual-agent 5/5: adversarial review → implementer active self-review → CONSENSUS or DISPUTED
            - Non-5/5 APPROVED: implementer active self-review → CONSENSUS or DISPUTED
        → If NEEDS_CHANGES: back to implementer
```

Every dual-agent path now requires both agents to actively agree. The adversarial review catches what the reviewer missed; the implementer self-review catches what both reviewers missed (the implementer has the deepest context on intent).

**Code change in `phases.rs`:** In the adversarial-approved branch (lines 1619-1646), instead of auto-writing CONSENSUS and returning, fall through to the same consensus flow used in Branch 3 (lines 1677+). This is a structural change — the adversarial `Approved` arm should not `return true` but instead continue to the implementer consensus prompt.

**Files to modify:**
- `src/prompts.rs` — Rewrite `implementation_consensus_prompt()` with active self-review instructions. Add `task` and `plan` parameters.
- `src/phases.rs` — Remove auto-consensus after adversarial approval (lines 1619-1646). After adversarial `Approved`, fall through to implementer self-review. Update the call site to pass `task` and `plan` to the consensus prompt.

---

## 4. Remove Deprecated Commands and Update CLI

As defined in `change-flow.md`, remove all old command forms entirely (no backward compatibility).

### Commands to remove

| Old command | Replacement | Migration error message |
|---|---|---|
| `run` | `implement` | "Use `implement`." |
| `run-tasks` | `implement` | "Use `implement`." |
| `run --planning-only` | `plan` | "Use `plan`." |
| `run --resume` | `implement --resume` or `tasks --resume` | "Use `implement --resume` or `tasks --resume`." |
| `init` | (auto-created on first `plan`/`implement`) | "Use `plan` or `implement` — state is created automatically." |
| `--tasks-file` flag | `--file` | "Use `--file` instead." |

For one release, return explicit migration errors for removed commands — then drop the error handlers too.

### New command set (only these exist)

```
agent-loop plan <task>                      Plan only
agent-loop plan --file <path>               Plan from file
agent-loop tasks                            Decompose plan into tasks
agent-loop tasks --resume                   Resume interrupted decomposition
agent-loop implement                        Run all tasks from tasks.md
agent-loop implement --task "Task N: ..."   Run a single task
agent-loop implement --file task.md         Run task from file
agent-loop implement --resume               Resume interrupted implementation
agent-loop reset                            Clear .agent-loop/state/ (preserves decisions.md)
agent-loop status                           Show current loop status
agent-loop version                          Print version
agent-loop help                             Print usage
```

### Config cleanup

Remove from `.agent-loop.toml` and env vars:
- `planning_only` config/env — replaced by the `plan` subcommand

### Files to modify

- `src/main.rs` — Remove `run`, `run-tasks`, `init` command variants. Remove `--planning-only`, `--resume` flags from `RunArgs`. Add `plan`, `tasks`, `implement`, `reset` subcommands. Add migration error handlers for removed commands.
- `src/config.rs` — Remove `planning_only` field. Add `compound`, `decisions_max_lines`, `quality_commands` fields.
- `src/state.rs` — Rename `init` logic to `reset`. Ensure `reset` preserves `.agent-loop/decisions.md`.

---

## 5. Update README.md

Rewrite `README.md` to reflect the new command model. Key changes:

1. **Commands section** — Replace `run`/`run-tasks`/`init`/`resume` with `plan`/`tasks`/`implement`/`reset`
2. **Remove "Deprecated forms" section** entirely — no backward compatibility
3. **Examples** — Update all `agent-loop run` examples to `agent-loop implement`
4. **Configuration** — Remove `planning_only` from TOML and env var tables. Add `compound`, `decisions_max_lines`, `quality_commands` entries.
5. **Per-Project State** — Update directory tree to show `.agent-loop/decisions.md` alongside `state/`. Note that `reset` preserves `decisions.md`.
6. **How It Works** — Update flow diagrams to reflect active self-review in consensus and compound phase.
7. **"When to Use Which Mode"** — Update references from `run` to `implement`.

---

## Implementation Order

1. **Deprecated command removal + new CLI** (foundational — everything else depends on the new command model)
   - Remove `run`, `run-tasks`, `init` commands
   - Add `plan`, `tasks`, `implement`, `reset` subcommands
   - Remove `planning_only` config
   - Add migration error handlers
2. **Compound Engineering / Decision Memory** (highest impact — creates the self-improving loop)
   - `decisions.md` persistence, reading, and prompt injection
   - Mid-session decision capture via prompt instructions
   - Compound phase after consensus
   - Struggle signal capture on MAX_ROUNDS/ERROR
3. **Active Self-Review Consensus** (quick win, immediate quality improvement)
   - Rewrite consensus prompt with active self-review checklist
   - Add `task`/`plan` to consensus prompt signature
   - Remove auto-consensus on dual-agent 5/5 adversarial path — fall through to implementer self-review
4. **Expanded Repository Knowledge** (trivial change, immediate value)
   - Read ARCHITECTURE.md, CONVENTIONS.md, AGENTS.md in `gather_project_context()`
5. **Custom Quality Commands** (medium effort, high value for projects with linters)
   - Config support for `[[quality_commands]]` with remediation hints
6. **Update README.md** (last — reflects all changes above)

---

## Files Summary

| File | Changes |
|---|---|
| `src/main.rs` | Remove `run`, `run-tasks`, `init` commands. Add `plan`, `tasks`, `implement`, `reset` subcommands. Migration error handlers for removed commands. |
| `src/prompts.rs` | Decision-writing instructions in implementer/reviewer prompts. `PRIOR DECISIONS` section injection. New `compound_prompt()`. Rewritten `implementation_consensus_prompt()` with active self-review + `task`/`plan` params added. Additional `append_file_excerpt()` calls in `gather_project_context()`. |
| `src/state.rs` | `read_decisions()`, `append_decision()`, `decisions_path()` helpers. `init` creates `decisions.md` at `.agent-loop/decisions.md` (outside `state/`). `reset` only touches `state/`, so decisions persist. |
| `src/phases.rs` | `compound_phase()` after consensus. Decisions read + passed to prompts. Struggle signal capture. Custom quality commands support. Dual-agent 5/5 adversarial path: remove auto-consensus, fall through to implementer self-review. Pass `task`/`plan` to consensus prompt call site. |
| `src/config.rs` | Remove `planning_only`. Add `compound: bool`, `decisions_max_lines: u32`, `quality_commands: Vec<QualityCommand>` fields. |
| `README.md` | Full rewrite: new command model, remove deprecated sections, add decisions.md docs, update config tables, update examples. |

---

## Verification

1. `cargo build` — compiles without errors
2. `cargo test` — all existing + new tests pass
3. Manual smoke tests:
   - `agent-loop plan "test"` works
   - `agent-loop implement` works
   - `agent-loop reset` works, preserves `decisions.md`
   - `agent-loop run "test"` returns migration error
   - `agent-loop run-tasks` returns migration error
   - `agent-loop init` returns migration error
   - Run a full implementation loop — verify agents write decisions mid-session
   - Verify compound phase runs after consensus and extracts learnings
   - Run again — verify `PRIOR DECISIONS & LEARNINGS` section appears in prompts with entries from previous run
   - Trigger MAX_ROUNDS — verify struggle signal is appended to `decisions.md`
   - Verify consensus prompt now requires active self-review with task/plan context
   - Dual-agent 5/5: verify implementer self-review runs after adversarial approval (no auto-consensus)
   - Create an ARCHITECTURE.md in project — verify it appears in project context
   - Configure `[[quality_commands]]` — verify remediation hints appear in reviewer prompts
   - README.md reflects all new commands and features accurately
