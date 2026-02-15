# Implementation Tasks

---

## Phase 1: Quick Wins and Maintainability

### Task 1: Deduplicate status labeling across all modules (H3, M4)
**Description:** The report identifies duplicated status-label logic as issue H3 (`status_label` duplicated across `main.rs` and `phases.rs`) and redundant manual deserialization as M4 (`Status::from_serialized`). In the Rust port, `Status` already has a `Display` impl in `state.rs` that serves as the single authoritative formatter, and all modules (`main.rs`, `phases.rs`, `state.rs`) already use it via `{}` formatting. However, `Status::from_serialized` (state.rs:51-66) manually maps string literals to enum variants, duplicating what `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` already provides via serde.

**Cross-module audit:** Verify that no module contains manual status-string formatting or parsing outside of the `Display` impl and serde attributes. Specifically:
- `src/main.rs`: Confirm all status rendering uses `Status`'s `Display` impl (e.g., `println!("status: {}", current_status.status)` at line 347) — no manual label mapping.
- `src/phases.rs`: Confirm `format_summary_block` and all log messages use `Status`'s `Display` impl — no manual label mapping.
- `src/state.rs`: Remove `Status::from_serialized` and replace its usage in `normalize_status_value` with `serde_json::from_value::<Status>()` (leveraging the existing `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` attribute).

**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- `Status::from_serialized` removed from `src/state.rs`
- `normalize_status_value` status parsing uses `serde_json::from_value` instead of manual matching
- Cross-module audit documented: confirmed no other module has manual status-label formatting (H3 is already resolved by the existing `Display` impl)
- All existing tests pass unchanged
- Fallback behavior preserved: unknown status strings still fall back to `fallback.status`
**Testing:** Run existing state.rs and phases/main status-related tests. Add a round-trip test: serialize each `Status` variant to JSON string via serde, deserialize back, assert equality. Add a test confirming that `Display` output matches serde serialization for every variant.

---

### Task 2: Remove trivial `get_field` wrapper (M5)
**Description:** The `get_field` function (state.rs:185-187) is a one-line wrapper around `Map::get()` that adds no value. Inline all call sites to use `map.get("key")` directly. This reduces indirection and improves readability.
**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- `get_field` function removed from `src/state.rs`
- All call sites in `normalize_status_value` updated to direct `map.get()` calls
**Testing:** Run existing state.rs tests — behavior must be identical.

---

### Task 3: Generic env parser for numeric settings (M6)
**Description:** `parse_u32_env` and `parse_u64_env` (config.rs:104-116) are identical except for the type parameter. Replace both with a single generic `parse_env<T: FromStr>(key, default) -> T` function. Update call sites in `Config::from_cli`.
**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- Single `parse_env<T>` function in `src/config.rs`
- `parse_u32_env` and `parse_u64_env` removed
- Call sites updated
**Testing:** Run existing config.rs tests. Ensure the same default-fallback behavior for missing/invalid env vars.

---

### Task 4: Consolidate duplicated test infrastructure (H2)
**Description:** `TestProject`/`TempProject` structs, `test_config()` helpers, `ScopedEnvVar`, `create_executable()`, and `Drop` impls are copy-pasted across state.rs, agent.rs, git.rs, and phases.rs test modules (~200 lines of duplication). Consolidate shared patterns into `src/test_support.rs` using a builder pattern. Keep truly module-specific fixtures local but extract common scaffolding.
**Complexity:** Medium
**Dependencies:** Task 1 (status parsing may affect test helpers), Task 2, Task 3
**Deliverables:**
- `src/test_support.rs` expanded with unified `TestProject` builder, shared `create_executable()`, shared `read_log()` helper
- Each module's test code simplified to use shared infrastructure
- All module tests pass
**Testing:** `cargo test` across all modules. No behavioral change — this is purely structural.

---

### Task 5: Add unit tests for calendar conversion (L3)
**Description:** The `civil_from_days` function (state.rs:152-169) implements a hand-rolled calendar algorithm but is only tested indirectly through `timestamp()` shape checks. Add direct unit tests covering known dates: Unix epoch (1970-01-01), a leap year boundary (2000-02-29), a non-leap century (1900-03-01), and a recent date.
**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- New `#[test]` functions in state.rs testing `civil_from_days` with specific inputs and expected (year, month, day) outputs
**Testing:** The tests themselves are the deliverable. Verify against known calendrical values.

---

### Task 6: Clean prompt redundancy (L4)
**Description:** The decomposition prompt (prompts.rs:99-105, `decomposition_initial_prompt`) contains two sentences that say the same thing: "Create a task breakdown file at {tasks.md}" followed by "Write the decomposed tasks to {tasks.md}". Remove the redundant trailing sentence, keeping the first instruction which has more context around the expected structure.
**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- Redundant sentence removed from `src/prompts.rs`
**Testing:** Run phases.rs prompt tests to ensure prompt still contains required path references.

---

### Task 7: Phase 1 verification gate
**Description:** Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` to verify all Phase 1 changes are clean. Fix any issues found.
**Complexity:** Low
**Dependencies:** Tasks 1-6
**Deliverables:**
- Clean `cargo fmt`, `cargo clippy`, and `cargo test` output
- All warnings resolved
**Testing:** The verification commands themselves.

---

## Phase 2: Reliability and Correctness

### Task 8: Atomic writes for state files (C2)
**Description:** `write_state_file` (state.rs) uses `fs::write()` which is not atomic — a crash mid-write corrupts `status.json`. Implement write-to-tempfile-then-rename: write to a `.tmp` file in the same directory, then `fs::rename()` to the target path. This guarantees the file is either fully written or unchanged. Only apply to the JSON state file write path; leave append-based logging unchanged.
**Complexity:** Medium
**Dependencies:** None
**Deliverables:**
- `write_state_file` in `src/state.rs` uses temp-file + rename pattern
- Temp file created in same directory as target (ensures same filesystem for atomic rename)
- Error handling preserves existing behavior (returns `io::Result`)
**Testing:** Existing state write tests. Add a test that verifies the target file is valid JSON after write (simulating the atomicity guarantee).

---

### Task 9: Process-group-aware timeout cleanup (C1)
**Description:** `terminate_for_timeout` (agent.rs:74-97) sends SIGTERM only to the lead process PID. Sub-processes spawned by `claude`/`codex` become orphans. On Unix, use `setsid()` or `pre_exec` with `setpgid(0,0)` to put the agent in its own process group, then send signals to `-pgid` to kill the entire group. Preserve existing non-Unix fallback.
**Complexity:** High
**Dependencies:** None
**Deliverables:**
- Agent process spawned in its own process group on Unix (via `CommandExt::pre_exec`)
- `terminate_for_timeout` sends SIGTERM/SIGKILL to `-pgid` on Unix
- Non-Unix path unchanged
**Testing:** Extend existing agent timeout tests. Add a Unix-only test where the agent script spawns a child process, verify both are terminated on timeout.

---

### Task 10: Monotonic idle timing (M1)
**Description:** `now_millis()` (agent.rs:40-45) uses `SystemTime::now()` for idle duration measurement. `SystemTime` is subject to NTP clock adjustments which can cause false timeouts or missed timeouts. Replace with `Instant` for the idle-tracking loop. Keep `SystemTime` only where wall-clock time is genuinely needed (timestamps in logs).
**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- Idle timeout tracking in `run_agent` uses `Instant` instead of `SystemTime`
- `now_millis` either removed or repurposed
- Timeout behavior unchanged
**Testing:** Existing timeout tests should pass unchanged. The change is behavioral correctness under clock skew, not observable in normal tests.

---

### Task 11: UTF-8-safe stream accumulation (M2)
**Description:** `spawn_reader_thread` (agent.rs:99-137) calls `String::from_utf8_lossy` on each 4096-byte chunk independently. Multi-byte UTF-8 characters split across chunk boundaries produce `U+FFFD` replacement characters. Fix by buffering incomplete trailing bytes and prepending them to the next chunk before decoding.
**Complexity:** Medium
**Dependencies:** None
**Deliverables:**
- Reader thread accumulates a byte buffer and handles incomplete UTF-8 sequences at chunk boundaries
- Final flush handles any remaining bytes
- No replacement characters for valid UTF-8 split across reads
**Testing:** Add a test with a known multi-byte string (e.g., emoji) split across a chunk boundary, verify no replacement characters in output.

---

### Task 12: Surface status-write failures (M3)
**Description:** Multiple `let _ = write_status(...)` calls in phases.rs (lines 134, 237, 335, 385, 427, 517, 540, 824) silently ignore I/O errors. Replace with explicit error handling: at minimum log the error via the existing `log()` function. For critical status transitions (e.g., writing ERROR status), consider propagating the error upward.
**Complexity:** Medium
**Dependencies:** Task 8 (atomic writes should land first so write failures are less likely)
**Deliverables:**
- All `let _ = write_status(...)` replaced with `if let Err(e) = write_status(...) { log(...) }`
- Critical paths propagate errors where appropriate
**Testing:** Existing tests. Verify that a simulated write failure (e.g., read-only directory) produces a log entry rather than silent failure.

---

### Task 13: Validate status timestamp freshness (H4, Loop #3)
**Description:** Each prompt embeds a timestamp, but `read_status` after an agent run never checks if the returned timestamp matches. If an agent exits without writing `status.json`, the previous round's stale status is silently reused. After each `read_status` call, compare the status timestamp against the prompt timestamp. On mismatch, treat as stale and force a corrective status appropriate to the specific phase and sub-step:

- **Planning reviewer path:** `NEEDS_REVISION` — the reviewer failed to write status, so the plan should be re-evaluated.
- **Planning implementer revision path:** `NEEDS_REVISION` — the implementer's revision consensus check failed; fall back to another revision round.
- **Decomposition implementer revision path:** `NEEDS_REVISION` — the implementer's decomposition revision did not produce a valid status.
- **Decomposition reviewer path:** `NEEDS_REVISION` — the reviewer failed to evaluate the task breakdown.
- **Implementation reviewer path:** `NEEDS_CHANGES` — the reviewer did not write status, so assume changes are still needed.
- **Implementation consensus path:** `DISPUTED` — the implementer's consensus response is missing, so treat as if the implementer raised concerns (conservative: prevents auto-approval on stale data).

The reason field for all stale-timestamp fallbacks must include an explanatory message such as "Agent did not write status (stale timestamp detected)".
**Complexity:** Medium
**Dependencies:** None
**Deliverables:**
- Prompt timestamp captured before each agent invocation across all six paths listed above
- Post-run validation compares `status.timestamp` to expected value
- Mismatch triggers the specific corrective status for that path with descriptive reason
- Helper function (e.g., `is_status_stale(expected_ts, actual_status) -> bool`) to centralize the comparison
**Testing:** Add stale-status tests for all six paths:
  1. Planning reviewer → verify fallback is `NEEDS_REVISION`
  2. Planning implementer revision → verify fallback is `NEEDS_REVISION`
  3. Decomposition implementer revision → verify fallback is `NEEDS_REVISION`
  4. Decomposition reviewer → verify fallback is `NEEDS_REVISION`
  5. Implementation reviewer → verify fallback is `NEEDS_CHANGES`
  6. Implementation consensus → verify fallback is `DISPUTED`

---

### Task 14: Unexpected status fallback policy (Loop #5)
**Description:** In `planning_reviewer_action` and `implementation_reviewer_decision`, the catch-all `_` branch maps to `Continue`, silently wasting a round when an agent writes an unexpected status value. Replace with conservative fallback: planning reviewer -> NeedsRevision, implementation reviewer -> NeedsChanges, consensus -> Disputed. Log the unexpected value.
**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- Catch-all branches in decision functions return conservative actions
- Unexpected status value logged with warning
**Testing:** Add tests that pass unexpected status strings and verify they produce the conservative fallback action, not `Continue`.

---

### Task 15: Phase 2 verification gate
**Description:** Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` to verify all Phase 2 changes are clean. Fix any issues found.
**Complexity:** Low
**Dependencies:** Tasks 8-14
**Deliverables:**
- Clean formatting, linting, and test output
**Testing:** The verification commands themselves.

---

## Phase 3: Loop Effectiveness

### Task 16: Inject actual git diff into reviewer prompt (Loop #1)
**Description:** The reviewer currently sees only a self-reported `changes.md` summary. Before the reviewer runs, compute `git diff` from the pre-implementation baseline commit to current state and inject it as a `DIFF:` section in the reviewer prompt. This transforms the review from trust-based to evidence-based. Truncate at ~500 lines with a notice.

**Baseline capture:** Before each implementation round begins, record the current HEAD commit SHA (or equivalent ref) as the baseline. After the implementer runs and before the reviewer runs, compute diff from that baseline ref to the **current working tree state** (committed + staged + unstaged), e.g. `git diff <baseline>`. Do not use `git diff HEAD` or `git diff <baseline>..HEAD`, because both can miss uncommitted implementation changes when `AUTO_COMMIT=0`.

**Complexity:** High
**Dependencies:** None
**Deliverables:**
- New function in `src/git.rs` to compute diff from a baseline ref to current working tree state (not HEAD-only)
- `src/prompts.rs` updated: `implementation_reviewer_prompt` accepts and includes diff text
- `src/phases.rs` captures baseline ref before implementation, passes diff to prompt
- Truncation at ~500 lines with "[truncated — full diff available via git]" notice
**Testing:** Add git.rs test for diff generation from a known baseline commit. Add git.rs/phases.rs test proving uncommitted changes are included when `AUTO_COMMIT=0`. Add phases.rs test verifying diff appears in reviewer prompt. Test truncation behavior at the 500-line boundary.

---

### Task 17: Inject project context into planning prompt (Loop #2)
**Description:** The planning prompt gives agents only the task text. Before planning, gather a bounded project structure (like `tree -L 2`, capped at 200 lines) and include README/CLAUDE.md content if present. Inject as a `PROJECT STRUCTURE:` section in the planning initial prompt.
**Complexity:** Medium
**Dependencies:** None
**Deliverables:**
- New function to gather project structure (file listing, bounded)
- README.md and CLAUDE.md content included if they exist
- `planning_initial_prompt` in `src/prompts.rs` accepts and includes project context
- `src/phases.rs` gathers context before planning and passes it
**Testing:** Test with a project directory containing known files. Verify prompt includes structure. Test truncation at 200 lines.

---

### Task 18: Pass planning dispute reasons forward (Loop #7)
**Description:** When the implementer disputes during planning, the next reviewer iteration doesn't see why. The dispute reason from `status.json` is available but not included in the reviewer's prompt. Include it as an `IMPLEMENTER'S CONCERNS:` section in the next `planning_reviewer_prompt`.
**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- `planning_reviewer_prompt` in `src/prompts.rs` accepts optional dispute reason
- `src/phases.rs` reads dispute reason from status and passes it to next reviewer prompt
**Testing:** Test that dispute reason appears in reviewer prompt. Test that it's absent when no dispute occurred.

---

### Task 19: Automated test, lint, and build checks before reviewer (Loop #4)
**Description:** Before the reviewer runs, optionally execute project quality checks — **tests, linting, AND build verification** — and include all results in the reviewer prompt. This gives the reviewer concrete evidence of code correctness beyond the self-reported changes summary.

**Detection logic by project type:**
- **Rust** (detected via `Cargo.toml` in project root):
  - **Build:** `cargo build` (always run when feature is enabled — ensures compilation succeeds)
  - **Test:** `cargo test` (always run when feature is enabled)
  - **Lint:** `cargo clippy` (run only if `cargo clippy --version` succeeds, i.e., clippy is installed)
- **JavaScript/TypeScript** (detected via `package.json` in project root):
  - **Build:** `npm run build` (run only if `scripts.build` exists in `package.json`)
  - **Test:** `npm test` (run only if `scripts.test` exists in `package.json` and is not the default `echo "Error: no test specified" && exit 1`)
  - **Lint:** `npm run lint` (run only if `scripts.lint` exists in `package.json`)
- **Other project types:** No auto-detection; rely on custom command override.

**Controls:**
- `AUTO_TEST=1` env var enables the feature (default: off)
- Optional `AUTO_TEST_CMD` env var overrides all auto-detection with a single custom command string
- Each check has a bounded execution timeout (default: 120 seconds per command) and output capture (truncate at 100 lines per command)

**Prompt injection format:**
```
QUALITY CHECKS:
  cargo build: PASSED
  cargo test: PASSED (42 tests)
  cargo clippy: FAILED (3 warnings)
  [truncated output...]
```

Run checks after git checkpoint, before review. Build is listed first because a compilation failure makes test/lint results moot.
**Complexity:** High
**Dependencies:** Task 16 (diff injection provides the pattern for injecting external data into prompts)
**Deliverables:**
- Project type detection logic (Rust via `Cargo.toml`, JS/TS via `package.json`)
- **Build**, test, and lint command selection with per-project-type rules described above
- Bounded execution with per-command timeout and output truncation
- `AUTO_TEST` env var control (default off), `AUTO_TEST_CMD` override
- Reviewer prompt includes quality-check summary section with build, test, and lint results
- Config struct updated with `auto_test` and `auto_test_cmd` fields
**Testing:**
- Test command detection for Cargo project: verify `cargo build`, `cargo test`, and `cargo clippy` are all selected (with clippy gated on availability)
- Test command detection for npm project (with/without `scripts.test`, with/without `scripts.lint`, with/without `scripts.build`, with default npm test stub)
- Test that feature is off by default
- Test `AUTO_TEST_CMD` override bypasses detection
- Test output truncation at 100 lines per command
- Test timeout handling (command exceeds time limit)
- Test that build failure is clearly reported even when test/lint pass

---

### Task 20: Structured review output format (Loop #9)
**Description:** Request reviewers to structure reviews in a template with sections: Correctness, Tests, Style/Maintainability, Security, Verdict. This produces more actionable and consistent reviews compared to free-form text.
**Complexity:** Low
**Dependencies:** None
**Deliverables:**
- `implementation_reviewer_prompt` in `src/prompts.rs` updated with structured review template
- Template includes section headers and brief guidance for each
**Testing:** Verify prompt contains all required sections. Existing reviewer prompt tests updated.

---

### Task 21: Shared conversation memory across rounds (Loop #6)
**Description:** Each agent invocation is stateless — the implementer doesn't remember what it tried before, and the reviewer doesn't remember previous flags. Maintain a `conversation.md` file that accumulates one-line summaries per round. Include this history in subsequent implementation and reviewer prompts to prevent "going in circles."
**Complexity:** Medium
**Dependencies:** None
**Deliverables:**
- New `conversation.md` state file, appended after each round
- Round summary format: `Round N: [phase] [status] - [brief reason]`
- Subsequent prompts include conversation history
- History bounded (last 10 rounds or ~50 lines)
**Testing:** Test that conversation.md accumulates entries. Test that history appears in prompts. Test bounding behavior.

---

### Task 22: Adversarial second review for 5/5 approval (Loop #8)
**Description:** When the reviewer gives APPROVED with 5/5 rating, the current consensus step asks the implementer "do you agree your work is excellent?" — a rubber stamp. In dual-agent mode, replace this with a second adversarial reviewer pass that receives actual git diff + test results and is framed as "find what the first reviewer missed." If second review is also APPROVED -> finish. Otherwise -> NEEDS_CHANGES. In single-agent mode, auto-consensus on APPROVED + 5. Include prompt guardrail: "If you find no meaningful issues, APPROVED is the correct result."
**Complexity:** High
**Dependencies:** Task 16 (git diff injection), Task 19 (test/build results, optional but valuable)
**Deliverables:**
- Consensus phase branching: 5/5 dual-agent -> adversarial review, 5/5 single-agent -> auto-approve, other -> existing behavior
- Adversarial review prompt in `src/prompts.rs`
- Phase logic in `src/phases.rs` for the new branch
**Testing:** Test all three branches. Verify adversarial prompt includes diff. Verify auto-consensus in single-agent mode. Verify normal consensus for non-5/5 ratings.

---

### Task 23: Phase 3 verification gate
**Description:** Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` to verify all Phase 3 changes are clean. Fix any issues found.
**Complexity:** Low
**Dependencies:** Tasks 16-22
**Deliverables:**
- Clean formatting, linting, and test output
**Testing:** The verification commands themselves.

---

## Phase 4: Configuration and Observability

### Task 24: Introduce `.agent-loop.toml` project config (Loop #10)
**Description:** Replace reliance on environment variables with an optional project-level `.agent-loop.toml` config file. Support settings: max_rounds, timeout_seconds, implementer, reviewer, single_agent, auto_commit, planning_only, auto_test, auto_test_cmd, and custom test command. Env vars and CLI flags override config file values (precedence: CLI > env > toml > defaults).
**Complexity:** High
**Dependencies:** Task 19 (auto_test and auto_test_cmd settings)
**Deliverables:**
- TOML parsing added to `src/config.rs` (add `toml` crate to Cargo.toml)
- `Config::from_cli` reads `.agent-loop.toml` from project root
- Precedence chain: CLI flags > env vars > TOML file > hardcoded defaults
- Example `.agent-loop.toml` documented
**Testing:** Test config loading with TOML file present/absent. Test precedence (env overrides TOML). Test invalid TOML handling.

---

### Task 25: Optional verbosity controls (L1)
**Description:** Add `--verbose` flag to the CLI. When enabled, log agent prompts, raw status JSON after each read, and decision logic to the log file. Add `--debug` for even more detail (full agent output echoed to stderr). This aids debugging without cluttering normal output.
**Complexity:** Medium
**Dependencies:** Task 24 (verbosity could be a TOML setting too)
**Deliverables:**
- `--verbose` and `--debug` CLI flags in `src/main.rs`
- `Config` struct extended with verbosity level
- Conditional logging at key points in `src/phases.rs`
**Testing:** Test that verbose mode produces additional log output. Test that non-verbose mode is unchanged.

---

### Task 26: Structured event logging (Loop #12)
**Description:** Write structured JSON events to `events.jsonl` alongside `log.txt` for post-hoc analysis. Events include: round start/end, status transitions, agent invocations (duration, exit code), ratings, and phase transitions. Each event has a timestamp, event type, and relevant payload.
**Complexity:** Medium
**Dependencies:** None
**Deliverables:**
- `events.jsonl` file created in state directory
- Event types: `round_start`, `round_end`, `agent_invoked`, `agent_completed`, `status_transition`, `phase_transition`
- Events appended at appropriate points in `src/phases.rs` and `src/agent.rs`
- Helper function in `src/state.rs` for event writing
**Testing:** Run a mock loop and verify `events.jsonl` contains expected event sequence. Verify each event is valid JSON.

---

### Task 27: Phase 4 verification gate
**Description:** Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` to verify all Phase 4 changes are clean. Fix any issues found.
**Complexity:** Low
**Dependencies:** Tasks 24-26
**Deliverables:**
- Clean formatting, linting, and test output
**Testing:** The verification commands themselves.

---

## Summary

| Phase | Tasks | Focus |
|-------|-------|-------|
| Phase 1 | 1-7 | Quick wins: cross-module status-label audit, deduplication, cleanup, test consolidation |
| Phase 2 | 8-15 | Reliability: atomic writes, process groups, error handling, timestamp validation |
| Phase 3 | 16-23 | Loop effectiveness: diff injection, project context, memory, quality gates (build+test+lint), adversarial review |
| Phase 4 | 24-27 | Configuration and observability: TOML config, verbosity, structured logging |

**Total: 27 tasks across 4 phases**

Each task is designed to be completable in a single agent-loop run. Tasks within each phase can generally be parallelized except where noted in dependencies. Verification gates at the end of each phase ensure cumulative correctness.
