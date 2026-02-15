# Revised Implementation Plan: Agent-Loop Review Report

## Why revision is required
The previous plan misses several report-priority items and has one critical mismatch:
- It uses `git diff HEAD` instead of diffing from the pre-implementation baseline commit.
- It omits multiple high-impact recommendations from the report's recommended order (`status_label` dedupe, test infra consolidation, auto test/lint checks, adversarial second review for 5/5, structured review format, and `.agent-loop.toml`).

This revised plan keeps the original sequencing model but aligns with the report's impact priorities.

## Phase 1: Quick Wins and Maintainability

### Step 1: Deduplicate status labeling (H3)
**Files:** `src/state.rs`, `src/main.rs`, `src/phases.rs`
- Make `Status` formatting authoritative via `impl Display for Status` (or equivalent single helper).
- Remove any remaining duplicate status-label logic from other modules.
- Add/adjust tests to verify labels are stable.

### Step 2: Remove redundant status parsing helper (M4)
**File:** `src/state.rs`
- Remove `Status::from_serialized`.
- Parse status directly via serde from `Value` with existing enum serde attributes.
- Keep fallback behavior for invalid/missing fields unchanged.

### Step 3: Remove trivial map wrapper (M5)
**File:** `src/state.rs`
- Remove `get_field` and inline `map.get(...)` calls.

### Step 4: Generic env parser for numeric settings (M6)
**File:** `src/config.rs`
- Replace `parse_u32_env` and `parse_u64_env` with generic `parse_env<T: FromStr>`.
- Update call sites in `Config::from_cli`.

### Step 5: Consolidate duplicated test infrastructure (H2)
**Files:** `src/test_support.rs`, `src/agent.rs`, `src/git.rs`, `src/phases.rs`, `src/state.rs`
- Move duplicated test setup patterns (`TestProject` scaffolding, env guards, common config builders) into shared helpers/builders.
- Keep module-local fixtures only where behavior is truly module-specific.

### Step 6: Add direct tests for calendar conversion helper (L3)
**File:** `src/state.rs`
- Add unit tests for `civil_from_days` with fixed known dates (epoch, leap-year boundaries).

### Step 7: Clean prompt redundancy (L4)
**File:** `src/prompts.rs`
- Remove duplicate decomposition instruction sentence.

## Phase 2: Reliability and Correctness

### Step 8: Atomic writes for state files (C2)
**File:** `src/state.rs`
- Implement write-to-temp then replace target for `write_state_file`.
- Ensure temp file is in same directory.
- Keep append-based logging path unchanged.

### Step 9: Process-group-aware timeout cleanup (C1)
**Files:** `src/agent.rs`, `Cargo.toml` (if needed)
- On Unix, spawn agent in its own session/process group.
- Send termination and force-kill to the process group (`-pgid`) instead of only the lead pid.
- Preserve existing non-Unix fallback behavior.
- Extend timeout tests to verify subprocess cleanup.

### Step 10: Monotonic idle timing (M1)
**File:** `src/agent.rs`
- Replace `SystemTime`-based idle math with `Instant`.
- Keep timeout semantics unchanged.

### Step 11: UTF-8-safe stream accumulation (M2)
**File:** `src/agent.rs`
- Avoid per-chunk lossy decoding artifacts by buffering bytes and decoding safely across chunk boundaries.

### Step 12: Stop silently ignoring status-write failures (M3)
**File:** `src/phases.rs`
- Replace `let _ = write_status(...)` with explicit error handling.
- At minimum, log failures; for critical transitions, promote to `ERROR` status.

### Step 13: Validate status timestamp freshness after agent runs (H4)
**File:** `src/phases.rs`
- Compare `read_status().timestamp` to the prompt timestamp used for that agent invocation.
- On mismatch, treat as stale write and force corrective status (`NEEDS_REVISION`/`NEEDS_CHANGES`/`DISPUTED` depending on phase), not silent continue.

### Step 14: Unexpected status fallback policy (Loop #5)
**File:** `src/phases.rs`
- Replace catch-all `Continue` branches with conservative fallback decisions:
  - planning reviewer: `NeedsRevision`
  - implementation reviewer: `NeedsChanges`
  - implementation consensus: `Disputed`
- Log the unexpected value explicitly.

## Phase 3: Loop Effectiveness (Highest Impact)

### Step 15: Inject actual diff into reviewer prompt using correct baseline (Loop #1)
**Files:** `src/git.rs`, `src/phases.rs`, `src/prompts.rs`
- Capture a pre-implementation baseline ref per round.
- Build reviewer diff from that baseline to current implementation state (not `git diff HEAD` blindly).
- Truncate to ~500 lines and include truncation note.
- Inject as explicit `DIFF:` section in reviewer prompt.

### Step 16: Inject project context into planning prompt (Loop #2)
**Files:** `src/phases.rs`, `src/prompts.rs`
- Gather bounded project structure (equivalent to `tree -L 2`, capped) and README/CLAUDE docs when present.
- Include as `PROJECT STRUCTURE:` section in planning prompt.

### Step 17: Pass planning dispute reasons forward (Loop #7)
**Files:** `src/phases.rs`, `src/prompts.rs`
- When implementer disputes, persist and include concerns in next reviewer prompt.

### Step 18: Automated test/lint/build checks before reviewer (Loop #4)
**Files:** `src/phases.rs`, `src/prompts.rs`, optionally `src/config.rs`
- Add optional gate controlled by env/config (default off, e.g. `AUTO_TEST=1`).
- Detect primary project command (`cargo test`/`cargo clippy` or package manager equivalents) with bounded output.
- Include pass/fail summary in reviewer prompt.

### Step 19: Structured review output format (Loop #9)
**File:** `src/prompts.rs`
- Require reviewer to structure review in sections: `Correctness`, `Tests`, `Style/Maintainability`, `Security`, `Verdict`.

### Step 20: Shared conversation memory across rounds (Loop #6)
**Files:** `src/state.rs`, `src/phases.rs`, `src/prompts.rs`
- Append concise round summaries to `conversation.md`.
- Include history in subsequent implementation/reviewer prompts.

### Step 21: Adversarial second review for approved 5/5 in dual-agent mode (Loop #8)
**Files:** `src/phases.rs`, `src/prompts.rs`
- If reviewer returns `APPROVED` with rating `5` in dual-agent mode, replace implementer-consensus call with second reviewer pass.
- Second pass prompt must be adversarial but allow genuine approval if no issues found.
- In single-agent mode, auto-consensus on `APPROVED` + `5`.

## Phase 4: Configuration and Observability

### Step 22: Introduce `.agent-loop.toml` project config (Loop #10)
**Files:** `src/config.rs`, `src/main.rs`
- Add optional project config file with env-var fallback.
- Cover settings such as rounds, timeout, test command override, and mode defaults.

### Step 23: Optional verbosity controls (L1)
**Files:** `src/main.rs`, `src/config.rs`, `src/phases.rs`
- Add `--verbose` (and optionally `--debug`) to improve inspectability of prompts/status transitions.

### Step 24: Structured event logging (Loop #12)
**Files:** `src/state.rs`, `src/phases.rs`
- Add `events.jsonl` alongside `log.txt` for machine-readable round metrics and transitions.

## Deferred (explicitly out of immediate scope)
- Split `src/phases.rs` into smaller modules (L2).
- Resume command (`agent-loop resume`, Loop #11).
- Tier-4 experimental features (#13-#18 in report).

## Files to modify
- `Cargo.toml` (if Unix process-group dependency is required)
- `src/agent.rs`
- `src/config.rs`
- `src/git.rs`
- `src/main.rs`
- `src/phases.rs`
- `src/prompts.rs`
- `src/state.rs`
- `src/test_support.rs`

## Verification gates
- Run `cargo fmt`, `cargo clippy`, `cargo test` after each phase.
- Add focused tests per major change:
  - stale timestamp mismatch handling
  - diff injection truncation and baseline correctness
  - process-group termination of child subprocesses
  - auto-test result injection
  - adversarial second-review branching
  - conversation history accumulation and prompt inclusion
  - `.agent-loop.toml` precedence vs env/CLI
