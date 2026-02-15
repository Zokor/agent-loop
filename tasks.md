# Implementation Tasks

---

## Phase 1: Quick Wins + Structural Cleanup

### Task 1: Remove redundant status parsing helpers (M4, M5, H3)
**Description:** Remove `get_field` (state.rs:185) — a trivial wrapper around `map.get()` — by inlining all call sites in `normalize_status_value`. Replace `Status::from_serialized` (state.rs:51-66) with serde-based deserialization using the existing `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` on `Status`. Audit all modules to confirm the `Display` impl on `Status` is the sole source of status formatting (H3 verification).

- **Complexity:** Low
- **Dependencies:** None
- **Key deliverables:**
  - `get_field` removed, all call sites inlined to `map.get("key")`
  - `Status::from_serialized` replaced with `serde_json::from_value::<Status>()` in `normalize_status_value`
  - Cross-module audit confirming no duplicate status formatting logic
  - Fallback behavior preserved for unknown status strings
- **Testing:** All 19 existing state.rs tests pass. Add serde round-trip test for every `Status` variant. Add test confirming `Display` output matches serde serialization.

---

### Task 2: Generify environment variable parsing (M6)
**File:** `src/config.rs`

Replace `parse_u32_env` (config.rs:113-118) and `parse_u64_env` (config.rs:120-125) with a single generic `parse_env<T: FromStr>(key: &str, default: T) -> T`. Update all 4 call sites (lines 60, 62, 63, 65).

- **Complexity:** Low
- **Dependencies:** None
- **Key deliverables:**
  - Single `parse_env<T: FromStr>` function
  - Both `parse_u32_env` and `parse_u64_env` removed
  - All call sites updated
- **Testing:** Existing 9 config.rs tests pass unchanged.

---

### Task 3: Consolidate test infrastructure (H2)
**Files:** `src/test_support.rs`, `src/agent.rs`, `src/git.rs`, `src/phases.rs`, `src/state.rs`

Four test modules each define their own `TestProject` struct with similar `Drop` implementations (state.rs:407-425, agent.rs:311-349, phases.rs:978-1023, git.rs:398-492). Consolidate into a unified `TestProject` in `test_support.rs` using a builder pattern supporting: temp directory creation, mock executable creation, PATH override, git repo init, and cleanup on drop. Keep only truly module-unique helpers local.

- **Complexity:** Medium
- **Dependencies:** Tasks 1-2 (status/config changes should land first to avoid conflicts)
- **Key deliverables:**
  - Unified `TestProject` struct in `test_support.rs` with builder methods
  - `create_executable` helper moved to `test_support.rs`
  - ~150-200 lines of duplicated scaffolding removed
  - All 114+ tests still pass
- **Testing:** Full `cargo test` suite — no behavioral change, purely structural.

---

### Task 4: Prompt polish and calendar unit tests (L3, L4)
**Files:** `src/prompts.rs`, `src/state.rs`

Two fixes: (1) In `decomposition_initial_prompt` (prompts.rs:99-105), remove the redundant second instruction — "Create a task breakdown file at {path}" followed by "Write the decomposed tasks to {path}" says the same thing twice. (2) Add direct unit tests for `civil_from_days` (state.rs:152-169) covering epoch (day 0 = 1970-01-01), leap year (2000-02-29), century boundary (1900-03-01), and a recent date.

- **Complexity:** Low
- **Dependencies:** None
- **Key deliverables:**
  - Redundant decomposition prompt sentence removed
  - 4+ unit tests for `civil_from_days` with known-good dates
- **Testing:** New calendar tests validate correctness. Existing phases.rs prompt tests pass.

---

### Task 5: Phase 1 verification gate
**Description:** Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` to verify all Phase 1 changes are clean. Fix any issues.

- **Complexity:** Low
- **Dependencies:** Tasks 1-4
- **Key deliverables:** Clean formatting, linting, and test output with all warnings resolved.
- **Testing:** The verification commands themselves.

---

## Phase 2: Reliability Hardening

### Task 6: Atomic state file writes (C2)
**File:** `src/state.rs`

`write_state_file` (state.rs:263-269) uses `fs::write()` which is not atomic — a crash mid-write corrupts `status.json`. Implement write-to-tempfile-then-rename.

**Implementation details:**
1. **Parent directory:** Call `fs::create_dir_all(path.parent())` before writing the temp file. This is required because the state directory may not exist on the first write (e.g., fresh project).
2. **Temp file:** Write contents to `{path}.tmp` in the same directory as the target. Same-directory placement guarantees same-filesystem, which is required for atomic `rename(2)`.
3. **Unix rename:** `fs::rename()` is atomic on POSIX — overwrites the target in a single syscall.
4. **Windows rename:** `fs::rename()` fails if the target exists on some Windows filesystems. Handle explicitly:
   - First attempt `fs::rename()`.
   - On failure, fall back to `fs::remove_file(target)` + `fs::rename(tmp, target)`.
   - If both fail, fall back to non-atomic `fs::write()` as last resort (better to have a non-atomic write than no write at all).
   - Document the non-atomic fallback in a code comment explaining the Windows limitation.
5. **Cleanup:** On any write/rename error, attempt `fs::remove_file(tmp)` to avoid leaving stale `.tmp` files.
6. **Return type:** Continues to return `io::Result<()>`.

- **Complexity:** Medium
- **Dependencies:** None
- **Key deliverables:**
  - `fs::create_dir_all` on parent directory before temp write
  - `write_state_file` uses temp file + rename pattern
  - Temp file in same directory (ensures same-filesystem rename)
  - Windows-specific fallback chain: rename → remove+rename → non-atomic write
  - `.tmp` file cleanup on error paths
  - Code comments documenting Windows replace limitations and fallback rationale
  - Error handling returns `io::Result` as before
- **Testing:** Existing state write tests pass. Add test verifying: (1) file is valid JSON after write, (2) parent directory is created if missing, (3) concurrent reads don't see partial writes (write tmp then rename guarantees this), (4) stale `.tmp` doesn't block subsequent writes.

---

### Task 7: Process group handling for timeout cleanup (C1)
**Files:** `src/agent.rs`, `Cargo.toml`

`terminate_for_timeout` (agent.rs:74-97) sends SIGTERM only to the lead PID. Sub-processes spawned by claude/codex become orphans. On Unix: use `pre_exec` with `setsid()` or `setpgid(0,0)` to put the child in its own process group. On timeout, send signals to `-pgid` to kill the entire group. Add `libc` crate if needed. Keep non-Unix fallback as `child.kill()`.

- **Complexity:** High
- **Dependencies:** None
- **Key deliverables:**
  - Agent child spawned in its own process group (Unix via `CommandExt::pre_exec`)
  - `terminate_for_timeout` sends SIGTERM/SIGKILL to `-pgid` on Unix
  - Non-Unix path unchanged
  - `Cargo.toml` updated if `libc` added
- **Testing:** Extend existing agent timeout tests. Add Unix-only test where agent script spawns a child, verify both are terminated on timeout.

---

### Task 8: Monotonic idle timeout + output collection fixes (M1, M2, M7)
**File:** `src/agent.rs`

Three related fixes: (1) Replace `SystemTime`-based `now_millis()` (lines 40-45) with `Instant`-based elapsed tracking for idle timeout — immune to NTP clock jumps. (2) Buffer raw bytes in reader threads and convert to UTF-8 once after thread join, instead of per-4KB-chunk `from_utf8_lossy` (line 129) which splits multi-byte characters. (3) Use `std::mem::take` instead of `value.clone()` (line 250) when extracting output under the mutex lock.

- **Complexity:** Medium
- **Dependencies:** None
- **Key deliverables:**
  - `Instant`-based idle tracking replaces `SystemTime`
  - Reader threads accumulate `Vec<u8>`, single UTF-8 conversion after join
  - `mem::take` replaces `clone` under lock
- **Testing:** Existing agent tests pass. Add test for multi-byte UTF-8 output correctness.

---

### Task 9: Make status-write failures visible (M3)
**File:** `src/phases.rs`

Multiple `let _ = write_status(...)` calls (lines ~134, ~237, ~335, ~385, ~427, ~517, ~540, ~824) silently ignore I/O errors. Replace with a helper that logs warnings on failure via the existing `log()` function. For critical status transitions, consider propagating the error upward.

- **Complexity:** Low
- **Dependencies:** Task 6 (atomic writes should land first so write failures are rarer)
- **Key deliverables:**
  - All silent `let _ = write_status(...)` replaced with `if let Err(e) = write_status(...) { log(...) }`
  - Warning messages include the error and which transition failed
- **Testing:** Existing tests pass. Verify simulated write failure produces a log entry.

---

### Task 10: Phase 2 verification gate
**Description:** Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`.

- **Complexity:** Low
- **Dependencies:** Tasks 6-9
- **Key deliverables:** Clean formatting, linting, and test output.
- **Testing:** The verification commands themselves.

---

## Phase 3: Core Loop-Effectiveness Changes

### Task 11: Inject git diff into reviewer prompts (Tier 1 #1)
**Files:** `src/git.rs`, `src/phases.rs`, `src/prompts.rs`

The single highest-impact improvement. Currently the reviewer sees only the implementer's self-reported `changes.md`. Add `git_diff_for_review(baseline_ref: Option<&str>, config: &Config) -> String` in `git.rs`: if HEAD advanced past baseline, generate diff for `pre_impl_head..HEAD`; if no commit was created (`AUTO_COMMIT=0` or no new commit), fallback to staged + working-tree diff. Capture `pre_impl_head` via `git rev-parse HEAD` before each implementation round. Pass diff into `implementation_reviewer_prompt` as `ACTUAL CODE DIFF:` section. Truncate at ~500 lines with notice.

- **Complexity:** High
- **Dependencies:** None
- **Key deliverables:**
  - `git_diff_for_review()` function in `git.rs`
  - `pre_impl_head` captured before each implementation round in `phases.rs`
  - Reviewer prompt includes real diff (or "(no diff available)" fallback)
  - Truncation at ~500 lines with notice
  - Works with both `AUTO_COMMIT=1` and `AUTO_COMMIT=0`
- **Testing:** Test diff generation with/without commits. Test uncommitted changes included when `AUTO_COMMIT=0`. Test truncation. Test non-git-repo fallback.

---

### Task 12: Add project context to planning prompts (Tier 1 #2)
**Files:** `src/phases.rs`, `src/prompts.rs`

Planning prompts give agents only the task text — no file tree, no language info. Implement `gather_project_context(project_dir: &Path) -> String` in pure Rust: walk directories up to depth 2, exclude `.git`/`target`/`node_modules`/`.agent-loop`, cap at 200 lines. Include first ~100 lines of `README.md`/`CLAUDE.md` if present. Inject as `PROJECT STRUCTURE:` section in `planning_initial_prompt` and `decomposition_initial_prompt`.

- **Complexity:** Medium
- **Dependencies:** None
- **Key deliverables:**
  - Pure Rust directory traversal (no shell dependency)
  - Depth-2 tree with exclusion list and line cap
  - README/CLAUDE.md excerpt inclusion
  - Both planning and decomposition initial prompts updated
- **Testing:** Test with nested directory structure. Verify exclusions work. Test line cap. Test missing README gracefully handled.

---

### Task 13: Validate status timestamp freshness (Tier 1 #3, H4)
**Files:** `src/phases.rs`, `src/state.rs`

Each prompt embeds a timestamp, but `read_status` never checks if it matches after an agent run. If an agent exits without writing `status.json`, stale state is silently reused. Add `is_status_stale(expected_ts: &str, status: &LoopStatus) -> bool` helper. After each agent run + `read_status`, check freshness. Mismatch triggers phase-appropriate corrective status:
- Planning reviewer → NeedsRevision
- Planning implementer revision → NeedsRevision
- Decomposition paths → NeedsRevision
- Implementation reviewer → NeedsChanges
- Implementation consensus → Disputed

All with descriptive reason: "Agent did not write status (stale timestamp detected)".

- **Complexity:** Medium
- **Dependencies:** None
- **Key deliverables:**
  - `is_status_stale()` validation helper
  - Applied after every agent run in all six paths
  - Phase-appropriate fallback status with explanatory reason
- **Testing:** Add stale-status tests for all six paths verifying correct fallback.

---

### Task 14: Fail-safe unexpected status handling (Tier 2 #5)
**File:** `src/phases.rs`

In `planning_reviewer_action` and `implementation_reviewer_decision`, the catch-all `_` arm maps to `Continue`, silently wasting a round. Change to conservative fallbacks: planning → NeedsRevision, implementation → NeedsChanges. Log the unexpected value as a warning.

- **Complexity:** Low
- **Dependencies:** None
- **Key deliverables:**
  - `planning_reviewer_action` catch-all → NeedsRevision + warning log
  - `implementation_reviewer_decision` catch-all → NeedsChanges + warning log
  - Warning includes the unexpected status string
- **Testing:** Add tests verifying unexpected status values produce conservative fallback, not Continue.

---

### Task 15: Pass planning dispute reasons forward (Tier 2 #7)
**Files:** `src/phases.rs`, `src/prompts.rs`

When the implementer disputes during planning, the next reviewer doesn't see why. In the planning loop, when status is Disputed, extract `status.reason` and pass to the next `planning_reviewer_prompt`. Update `planning_reviewer_prompt` to accept optional `dispute_reason: Option<&str>` and inject as `IMPLEMENTER'S CONCERNS:` section when present.

- **Complexity:** Low
- **Dependencies:** None
- **Key deliverables:**
  - `planning_reviewer_prompt` accepts optional dispute reason parameter
  - Dispute reason extracted from status and forwarded
  - Prompt includes `IMPLEMENTER'S CONCERNS:` section when present
- **Testing:** Test dispute reason appears in prompt. Test absent when no dispute occurred.

---

### Task 16: Automated test/lint/build checks before review (Tier 2 #4)
**Files:** `src/config.rs`, `src/phases.rs`, `src/prompts.rs`

Before the reviewer runs, optionally execute project quality checks. Detection by project type:
- **Rust** (`Cargo.toml`): `cargo build`, `cargo test`, `cargo clippy` (if installed)
- **JS/TS** (`package.json`): `npm run build`/`test`/`lint` (if scripts exist and aren't stubs)

Controlled by `AUTO_TEST=1` env var (default off) and optional `AUTO_TEST_CMD` override. Each check has 120s timeout and 100-line output cap. Include results in reviewer prompt as `QUALITY CHECKS:` section.

- **Complexity:** High
- **Dependencies:** Task 11 (reviewer prompt injection pattern established)
- **Key deliverables:**
  - `AUTO_TEST` env var and `config.auto_test` field
  - `AUTO_TEST_CMD` override support
  - Project type detection for Rust and JS/TS
  - Bounded execution with timeout and output truncation
  - Reviewer prompt includes `QUALITY CHECKS:` section
- **Testing:** Test command detection for Cargo and npm projects. Test feature off by default. Test override. Test truncation and timeout.

---

### Task 17: Phase 3 verification gate
**Description:** Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`.

- **Complexity:** Low
- **Dependencies:** Tasks 11-16
- **Key deliverables:** Clean formatting, linting, and test output.
- **Testing:** The verification commands themselves.

---

## Phase 4: Advanced Review Quality + Config UX

### Task 18: Shared conversation memory across rounds (Tier 2 #6)
**Files:** `src/state.rs`, `src/phases.rs`, `src/prompts.rs`

Maintain `conversation.md` in state directory accumulating one-line summaries per round (e.g., "Round 1: Implemented basic auth. Round 2 review: NeedsChanges — missing validation."). After each agent run, append a summary. Include last ~20 lines in subsequent implementer/reviewer prompts as `ROUND HISTORY:` section. Prevents agents from re-introducing rejected approaches.

- **Complexity:** Medium
- **Dependencies:** None
- **Key deliverables:**
  - `append_round_summary(round, phase, summary, config)` helper
  - `read_recent_history(config, max_lines) -> String` helper
  - History section in implementer and reviewer prompts
  - `conversation.md` managed in state directory
- **Testing:** Test summary accumulation and history reading. Test bounding at 20 lines.

---

### Task 19: Structured reviewer output template (Tier 3 #9)
**File:** `src/prompts.rs`

Update reviewer prompts to request structured template output: Correctness, Tests, Style, Security, Verdict for implementation reviews; Completeness, Feasibility, Risks, Verdict for planning reviews. Structure is part of prompt instructions (not enforced by parsing). Existing JSON status format preserved.

- **Complexity:** Low
- **Dependencies:** None
- **Key deliverables:**
  - Planning reviewer prompt requests structured sections
  - Implementation reviewer prompt requests structured sections
  - JSON status output format unchanged
- **Testing:** Verify prompt content includes all sections via content tests.

---

### Task 20: Adversarial second review for APPROVED + 5/5 (Tier 3 #8)
**Files:** `src/phases.rs`, `src/prompts.rs`

When reviewer gives APPROVED + 5/5, the consensus step is a rubber stamp. Fix:
- **Dual-agent 5/5:** Replace implementer consensus with second adversarial review using git diff + test results + first review. Framed as "find what the first reviewer missed." Include guardrail: "If no meaningful issues, APPROVED is correct." If also APPROVED → finish. Otherwise → NeedsChanges.
- **Single-agent 5/5:** Auto-consensus (skip extra call).
- **All other APPROVED:** Keep current consensus behavior.

- **Complexity:** High
- **Dependencies:** Task 11 (git diff), Task 16 (test results, optional)
- **Key deliverables:**
  - Consensus branching logic for 5/5 dual/single-agent
  - Adversarial review prompt with guardrail
  - Auto-consensus for single-agent 5/5
  - Current consensus preserved for non-5/5
- **Testing:** Test all three branches. Verify adversarial prompt includes diff. Verify auto-consensus in single-agent mode.

---

### Task 21: Project config file support (Tier 3 #10)
**Files:** `src/config.rs`, `src/main.rs`, `Cargo.toml`, `README.md`

Add `.agent-loop.toml` parsing. Precedence: CLI args > env vars > `.agent-loop.toml` > defaults. Support: `max_rounds`, `planning_max_rounds`, `decomposition_max_rounds`, `timeout`, `implementer`, `reviewer`, `single_agent`, `auto_commit`, `auto_test`, `auto_test_cmd`, `planning_only`. Add `toml` crate. Parse in `Config::from_cli` before env var resolution.

- **Complexity:** Medium
- **Dependencies:** Task 16 (auto_test fields exist in Config)
- **Key deliverables:**
  - `toml` crate added to Cargo.toml
  - `.agent-loop.toml` parsed if present in project root
  - Correct precedence: CLI > env > file > defaults
  - All env-var-based config still works
  - **Documentation updates:**
    - `README.md`: Add "Configuration" section documenting `.agent-loop.toml` format with a complete example file showing all supported keys, their types, and defaults
    - `README.md`: Document precedence rules (CLI > env > file > defaults) with concrete examples showing override behavior
    - `README.md`: Update existing env var documentation to note that `.agent-loop.toml` is the preferred approach for per-project config
    - Add inline doc comments on the TOML deserialization struct explaining each field
- **Testing:** Test with/without TOML file. Test precedence (each layer overrides correctly). Test invalid TOML handling (malformed file, unknown keys, wrong types). Test missing file is silent (not an error).

---

### Task 22: Unify error model (H1)
**Files:** `src/error.rs` (new), `src/main.rs`, `src/agent.rs`, `src/git.rs`, `src/state.rs`, `src/config.rs`, `src/phases.rs`

Introduce `AgentLoopError` enum: `Io(io::Error)`, `Git(String)`, `Agent(String)`, `Config(String)`, `State(String)`. Implement `From<io::Error>`, `Display`, `std::error::Error`. Replace `Result<_, String>` in `agent.rs`, `Result<_, ()>` in `git.rs`, and mixed styles at module boundaries. Phases can continue returning `bool` internally.

This is a cross-cutting refactor that touches every module's public API signatures. It must run after all other tasks to avoid merge conflicts and signature churn.

- **Complexity:** High
- **Dependencies:**
  - **Hard dependencies (signature-changing tasks that must complete first):**
    - Task 1 (changes `state.rs` public API — `from_serialized` removal affects return types)
    - Task 6 (changes `write_state_file` error handling in `state.rs`)
    - Task 8 (changes `agent.rs` output collection — affects `run_agent` return paths)
    - Task 9 (changes `write_status` error handling patterns in `phases.rs`)
    - Task 13 (adds new `state.rs` helpers with their own return types)
    - Task 16 (adds new functions in `config.rs` and `phases.rs` with return types to unify)
    - Task 21 (adds TOML parsing to `config.rs` with new error paths)
  - **Soft dependencies (should complete first to reduce churn but not blocking):**
    - Tasks 2, 3, 7, 11, 12, 14, 15, 18 (add or change functions but don't alter error conventions)
  - **No dependency:** Tasks 4, 5, 10, 17, 19, 20, 23 (prompt-only, verification, or flow-logic changes)
  - **Practical rule:** Schedule as the last implementation task in Phase 4. If parallelizing within Phase 4, all hard-dependency tasks must be merged before starting Task 22.
- **Key deliverables:**
  - `src/error.rs` with `AgentLoopError` enum and `From` conversions for `io::Error`, `serde_json::Error`, `toml::de::Error`
  - Module public APIs use `Result<T, AgentLoopError>` — specifically:
    - `agent.rs`: `run_agent()` returns `Result<..., AgentLoopError>` instead of `Result<..., String>`
    - `git.rs`: public functions return `Result<..., AgentLoopError>` instead of `Result<..., ()>`
    - `state.rs`: public functions continue `io::Result` internally but callers convert via `From<io::Error>`
    - `config.rs`: `Config::from_cli` returns `Result<Config, AgentLoopError>` instead of `Result<Config, String>`
  - `mod error` declared in `main.rs`
  - `main()` uses `AgentLoopError` for top-level error reporting
- **Testing:** All existing tests pass with updated types. Add error conversion tests (each `From` impl). Add test that `Display` output is informative for each variant. Run `cargo test` and `cargo clippy`.

---

### Task 23: Phase 4 verification gate
**Description:** Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`.

- **Complexity:** Low
- **Dependencies:** Tasks 18-22
- **Key deliverables:** Clean formatting, linting, and test output.
- **Testing:** The verification commands themselves.

---

## Summary

| Task | Phase | Title | Complexity | Dependencies |
|------|-------|-------|-----------|--------------|
| 1 | 1 | Remove redundant status helpers (M4, M5, H3) | Low | None |
| 2 | 1 | Generify env parsing (M6) | Low | None |
| 3 | 1 | Consolidate test infrastructure (H2) | Medium | 1, 2 |
| 4 | 1 | Prompt polish + calendar tests (L3, L4) | Low | None |
| 5 | 1 | Phase 1 verification gate | Low | 1-4 |
| 6 | 2 | Atomic state writes (C2) | Medium | None |
| 7 | 2 | Process group handling (C1) | High | None |
| 8 | 2 | Monotonic idle + output fixes (M1, M2, M7) | Medium | None |
| 9 | 2 | Status-write failure visibility (M3) | Low | 6 |
| 10 | 2 | Phase 2 verification gate | Low | 6-9 |
| 11 | 3 | Git diff in reviewer prompts (Tier 1 #1) | High | None |
| 12 | 3 | Project context in planning (Tier 1 #2) | Medium | None |
| 13 | 3 | Timestamp freshness validation (Tier 1 #3, H4) | Medium | None |
| 14 | 3 | Fail-safe unexpected status (Tier 2 #5) | Low | None |
| 15 | 3 | Planning dispute forwarding (Tier 2 #7) | Low | None |
| 16 | 3 | Auto test/lint/build before review (Tier 2 #4) | High | 11 |
| 17 | 3 | Phase 3 verification gate | Low | 11-16 |
| 18 | 4 | Shared conversation memory (Tier 2 #6) | Medium | None |
| 19 | 4 | Structured reviewer template (Tier 3 #9) | Low | None |
| 20 | 4 | Adversarial 5/5 review (Tier 3 #8) | High | 11, 16 |
| 21 | 4 | Project config file (Tier 3 #10) | Medium | 16 |
| 22 | 4 | Unified error model (H1) | High | Hard: 1,6,8,9,13,16,21; Soft: 2,3,7,11,12,14,15,18 |
| 23 | 4 | Phase 4 verification gate | Low | 18-22 |

**Total: 23 tasks across 4 phases**

Each task is completable in a single agent-loop run. Within each phase, tasks without mutual dependencies can be parallelized. Verification gates at phase boundaries ensure cumulative correctness.

### Optional Future Backlog (deferred)
- `--verbose`/`--debug` logging flag (L1)
- `events.jsonl` structured telemetry (Tier 3 #12)
- Resume interrupted loops (Tier 3 #11)
- Custom agent support, hooks, interactive mode, role rotation (Tier 4)
