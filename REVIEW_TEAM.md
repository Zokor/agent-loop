# Agent-Loop Review Report

> Produced by a 4-agent review team on 2026-02-15. Each agent specialized in a different dimension.

---

## Executive Summary

The agent-loop codebase is **well-structured and well-tested** (~900 lines of Rust source, ~1100 lines of tests). The module layering is clean with no circular dependencies, test coverage is strong, and the 3-crate dependency set is lean. However, there are significant opportunities to improve both the **code quality** and, more importantly, **the effectiveness of the agent loop itself**.

The findings below are organized into two categories:
1. **Code-level improvements** — Rust idioms, duplication, reliability fixes
2. **Loop effectiveness improvements** — changes that make agents produce better code

---

## PART 1: Code-Level Improvements

### Critical

| # | Issue | Location | Description |
|---|-------|----------|-------------|
| C1 | **Process group handling** | `agent.rs:77-81` | SIGTERM only targets the lead process PID, not the process group. Sub-processes spawned by `claude`/`codex` become orphans on timeout. Fix: use `setsid()`/`setpgid` + `kill(-pgid, SIGTERM)`. |
| C2 | **Non-atomic file writes** | `state.rs:247` | `fs::write()` is not atomic. A crash mid-write corrupts `status.json`. Fix: write to `.tmp` then `fs::rename()`. The `read_status` fallback (returns Error for bad JSON) mitigates this, but prevention is better. |

### High

| # | Issue | Location | Description |
|---|-------|----------|-------------|
| H1 | **Inconsistent error types** | Project-wide | Three different error conventions: `Result<_, String>` (main.rs, agent.rs), `io::Result` (state.rs), `Result<_, ()>` (git.rs). Unify with a project-level `Error` enum. |
| H2 | **Test infrastructure duplication (~200 lines)** | `state.rs`, `agent.rs`, `git.rs`, `phases.rs` | `TestProject`, `ScopedEnvVar`, and `test_config` are copy-pasted across 4 test modules. Consolidate into `test_support.rs` with a builder pattern. |
| H3 | **`status_label` duplicated** | `main.rs:328-342`, `phases.rs:82-96` | Identical function in two files. Should be a `Display` impl or method on `Status`. |
| H4 | **Stale status detection** | `phases.rs:469,856` | Timestamps are generated and embedded in prompts but never validated after `read_status`. If an agent exits without writing `status.json`, stale state from the previous round is silently used. Fix: compare `status.timestamp` against the prompt timestamp. |

### Medium

| # | Issue | Location | Description |
|---|-------|----------|-------------|
| M1 | **`Instant` vs `SystemTime`** | `agent.rs:40-45` | `SystemTime` used for idle duration measurement. `Instant` is monotonic and immune to NTP clock jumps. |
| M2 | **UTF-8 split across chunks** | `agent.rs:129` | `from_utf8_lossy` per 4096-byte chunk can produce replacement characters for multi-byte chars split across read boundaries. |
| M3 | **Silent `write_status` errors** | `phases.rs:435,583` | `let _ = write_status(...)` silently ignores I/O errors that could leave status stale and cause incorrect loop behavior. |
| M4 | **Redundant `Status::from_serialized`** | `state.rs:30-46` | Manual deserialization that duplicates `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` already on the enum. Use `serde_json::from_value` instead. |
| M5 | **Unnecessary `get_field` wrapper** | `state.rs:164-166` | Trivial wrapper around `Map::get` that adds no value. Inline it. |
| M6 | **`parse_u32_env`/`parse_u64_env` duplication** | `config.rs:101-113` | Two identical functions differing only in type. Replace with a generic `parse_env<T: FromStr>`. |
| M7 | **Clone optimization** | `agent.rs:250` | `output.lock().map(\|value\| value.clone())` clones the entire output string while holding the lock. Use `std::mem::take` instead. |

### Low

| # | Issue | Location | Description |
|---|-------|----------|-------------|
| L1 | **No `--verbose`/`--debug` flag** | N/A | Agent prompts and raw status JSON are not logged, making debugging harder. |
| L2 | **`phases.rs` is large** | `phases.rs` (~2100 lines) | Could split prompt templates into `prompts.rs`, keeping `phases.rs` focused on orchestration. |
| L3 | **No `civil_from_days` unit test** | `state.rs:131-148` | Hand-rolled calendar algorithm tested only indirectly via `timestamp()` shape check. Needs tests with known dates (epoch, leap years). |
| L4 | **Decomposition prompt redundancy** | `phases.rs:260-266` | "Create a task breakdown file at {tasks.md}" followed by "Write the decomposed tasks to {tasks.md}" — says the same thing twice. |

---

## PART 2: Loop Effectiveness Improvements

These changes would make the agents produce **better code** through a better orchestration loop.

### Tier 1 — Critical Impact

#### 1. Include `git diff` in reviewer prompts (instead of self-reported `changes.md`)

**Current**: The reviewer sees a `CHANGES SUMMARY` from `changes.md`, which the *implementer wrote about itself*. The prompt says "Review the ACTUAL code changes" but doesn't provide them.

**Fix**: Before the reviewer runs, compute `git diff` against the pre-implementation commit and inject it as a `DIFF:` section in the reviewer prompt. This transforms the review from trust-based to evidence-based. Cap at ~500 lines with truncation notice.

**Impact**: This is the single highest-impact improvement. A reviewer that sees actual diffs catches real bugs instead of rubber-stamping a self-reported summary.

#### 2. Inject project context in initial prompts

**Current**: The `planning_initial_prompt` gives agents ONLY the task text. No file tree, no language info, no existing code patterns. Agents waste early turns exploring.

**Fix**: Before planning, gather `tree -L 2` (capped at 200 lines) and include as a `PROJECT STRUCTURE:` section. Also include README/CLAUDE.md content if present.

**Impact**: Agents immediately know what exists and can write plans that reference real files and patterns.

#### 3. Validate timestamps to detect stale status

**Current**: Each prompt embeds a timestamp in the JSON template. But `read_status` after an agent run never checks if the returned timestamp matches. If an agent exits without writing `status.json`, the previous round's status is silently reused.

**Fix**: After each `read_status`, compare `status.timestamp` against the prompt timestamp. Mismatch = agent didn't write status = treat as error/force-revision.

**Impact**: Eliminates a class of subtle bugs where stale state causes incorrect loop transitions.

### Tier 2 — High Impact

#### 4. Automated test/lint/build checks between rounds

Before the reviewer runs, optionally execute the project's test suite and linter. Include results in the reviewer prompt. The reviewer who sees "3 tests passing, 2 failing" gives far more targeted feedback.

**Implementation**: Detect test command from `Cargo.toml` / `package.json`. Run after git checkpoint, before review. Control via `AUTO_TEST=1` env var.

#### 5. Treat unexpected status values as NeedsRevision/NeedsChanges

**Current**: If a reviewer writes an unexpected status (e.g., `NEEDS_CHANGES` during planning, which expects `NEEDS_REVISION`), it maps to `Continue` and the round is silently wasted.

**Fix**: In `planning_reviewer_action` and `implementation_reviewer_decision`, treat the catch-all `_` as NeedsRevision/NeedsChanges with a warning log, not a silent `Continue`.

#### 6. Shared conversation memory across rounds

**Current**: Each agent invocation is stateless. The implementer doesn't remember what it tried before; the reviewer doesn't remember what it flagged.

**Fix**: Maintain a `conversation.md` that accumulates one-line summaries per round. Include in subsequent prompts. Prevents "going in circles" where agents re-introduce rejected approaches.

#### 7. Pass dispute reasons forward in planning

**Current**: When the implementer disputes during planning, the next reviewer iteration doesn't see why. The dispute reason from `status.json` is not included in the reviewer's prompt.

**Fix**: In the planning loop, when status is `DISPUTED`, include the reason in the next `planning_reviewer_prompt` as `IMPLEMENTER'S CONCERNS:`.

### Tier 3 — Medium Impact

#### 8. Adversarial second review on 5/5 rating

**Current**: When the reviewer gives `APPROVED` with 5/5, the consensus step asks the *implementer* "do you agree your work is excellent?" — a rubber stamp ~100% of the time. Same cost as a real review, near-zero signal.

**Policy**:
- **APPROVED + rating == 5, dual-agent mode**: Replace implementer consensus with a second reviewer pass (adversarial, diff-based, test-aware). The second pass is framed as "find what the first reviewer missed" and receives actual `git diff` + test results. If second review is also `APPROVED` → finish. Otherwise → `NEEDS_CHANGES` and continue the loop.
- **APPROVED + rating == 5, single-agent mode**: Auto-consensus (skip the extra call). A second pass from the same model adds little independence and wastes tokens.
- **All other APPROVED outcomes** (rating < 5 or no rating): Keep current consensus behavior — the implementer confirmation still has value at lower confidence ratings.

**Prompt guardrail**: The adversarial second review prompt must include: *"If you find no meaningful issues, APPROVED is the correct result."* — this counteracts the bias toward inventing problems when the framing asks "find what was missed."

**Impact**: Same token cost as the current consensus step in dual-agent mode, but swaps a low-value self-approval for a high-value evidence-based adversarial review. Saves one agent call entirely in single-agent mode. A 5/5 can mean "genuinely excellent" or "reviewer didn't look carefully" — this disambiguates.

#### 9. Structured review format

Request reviews in a structured template (Correctness, Tests, Style, Security, Verdict) instead of free-form. Produces more actionable, consistent reviews.

#### 10. `.agent-loop.toml` configuration file

Replace env vars with a project-level config file. Better UX, supports per-project conventions, enables features like custom test commands and coding standards.

#### 11. Resume interrupted loops

Add `agent-loop resume` subcommand. Read `status.json` to determine phase/round and resume from that point. Prevents wasted work on long-running tasks.

#### 12. Structured logging (events.jsonl)

Write structured JSON events alongside `log.txt` for post-hoc analysis: round timings, status transitions, ratings. Essential for understanding and improving the system over time.

### Tier 4 — Lower Impact / Future

| # | Improvement | Notes |
|---|------------|-------|
| 13 | Custom agent support | Replace hardcoded Claude/Codex with configurable agent commands |
| 14 | Hook system | `.agent-loop/hooks/` for pre-implementation, post-review scripts |
| 15 | Interactive mode | `--interactive` flag for human oversight between rounds |
| 16 | Role rotation | Alternate implementer/reviewer between rounds for diverse perspectives |
| 17 | Progressive context loading | Start minimal, add context (file tree, test results) if implementation fails review |
| 18 | Metrics collection | Track wall time, tokens per round, approval rate |

---

## Reliability Assessment

| Area | Verdict | Notes |
|------|---------|-------|
| Process management | Good, with caveats | Idle timeout + SIGTERM + SIGKILL escalation works. Process group issue (C1) needs fixing. |
| File I/O | Acceptable | Non-atomic writes (C2) are the main risk. Graceful fallback on corruption exists. |
| Git operations | Strong | Checkpoint scoping via baseline set + `--only` flag is well-designed. Quoted path parsing is robust. |
| Error handling | Adequate | Messages are informative. Silent `let _ =` on status writes (M3) is the main concern. |
| Platform compat | Good | `#[cfg(unix)]` blocks correct. `PathBuf::join` used throughout. Windows SIGTERM fallback to `kill()` is reasonable. |
| Security | Acceptable | No command injection (all `Command::new().args()`). Dangerous permission flags are by design. `CLAUDECODE` stripping prevents recursion. |
| Test coverage | Strong | Comprehensive tests across all modules. Unix integration tests with shell script stubs are creative. |

---

## Recommended Implementation Order

**Phase 1 — Quick wins (code quality)**
1. Deduplicate `status_label` → method on `Status`
2. Remove `get_field` wrapper, `Status::from_serialized`
3. Generify `parse_u32_env`/`parse_u64_env`
4. Consolidate test infrastructure into `test_support.rs`

**Phase 2 — Reliability fixes**
5. Atomic file writes (write-then-rename)
6. Process group handling for timeout cleanup
7. Use `Instant` instead of `SystemTime` for idle timeout

**Phase 3 — Loop effectiveness (highest impact)**
8. Git diff injection in reviewer prompts
9. Project context (file tree) in planning prompts
10. Timestamp validation for stale status detection
11. Treat unexpected status values as NeedsRevision/NeedsChanges

**Phase 4 — Enhanced loop features**
12. Automated test/lint checks between rounds
13. Shared conversation memory
14. Adversarial second review on 5/5 rating
15. Structured review format
16. `.agent-loop.toml` configuration file
