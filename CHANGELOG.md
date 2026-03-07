# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

## [0.1.9] - 2026-03-07

### Changed
- Planning and task-decomposition revision loops now keep artifact ownership with the implementer: reviewers record findings in `review.md`, then implementers revise `plan.md` or `tasks.md` from that review instead of reviewers rewriting those files directly.
- Planning and decomposition revision prompts were simplified to remove stale signoff/status handoff from implementer revision steps.
- README workflow documentation now explicitly describes the reviewer re-review loop for plan, tasks, and implementation phases.

## [0.1.8] - 2026-03-06

### Fixed
- `agent-loop implement --wave --resume` now routes to wave-mode resume semantics (`task_status.json`) instead of the generic implementation resume preconditions.
- Added regression coverage to ensure wave resume remains functional even when generic resume state is absent.

### Changed
- README documentation now matches actual `--wave --resume` behavior and current `implement` flag-validation semantics.

## [0.1.7] - 2026-03-05

### Added
- `decisions_enabled` toggle (default: `false`): master switch for the decisions subsystem. When `false`, `decisions.md` is not created or read, decision capture instructions are omitted from prompts, struggle signals are not recorded, compound learning is skipped, and managed reference blocks are removed from project guides.
- `decisions_auto_reference` toggle (default: `true`): controls automatic syncing of managed decisions-reference blocks into `AGENTS.md`/`CLAUDE.md`. Set to `false` to keep `decisions.md` but skip reference syncing.
- `transcript_enabled` toggle (default: `false`): writes a human-readable per-agent-call transcript to `.agent-loop/state/transcript.log` with metadata (workflow, phase, round, role, agent). Auto-rotates at 10,000 lines.
- Structured phase workflow commands: `plan`, `tasks`, and `implement`.
- Dependency-aware wave execution mode (`implement --wave`) with topological scheduling, wave barriers, and bounded parallelism.
- Batch implementation mode by default (`implement` runs all tasks from `.agent-loop/state/tasks.md` in one loop).
- Task lifecycle persistence for status and metrics in `.agent-loop/state/task_status.json` and `.agent-loop/state/task_metrics.json`.
- Reviewer findings persistence in `.agent-loop/state/findings.json` with structured IDs, severities, summaries, and file references.
- Claude/Codex CLI tuning configuration via env/TOML (`CLAUDE_ALLOWED_TOOLS`, `CLAUDE_SESSION_PERSISTENCE`, effort/token controls, and `CODEX_FULL_ACCESS`).
- Role-aware system prompt injection and Claude session ID persistence per role across rounds.
- Phase-level transcript metadata propagation (`workflow`, `phase`, `round`, `session_hint`) for implementer/reviewer/planner calls.
- Gate-B reviewer verification prompts/flow for confirming or withdrawing fresh-context findings before signoff.
- Compound learning and reusable decision capture in `.agent-loop/decisions.md`.
- Automatic decisions reference synchronization into root `AGENTS.md` and `CLAUDE.md` on initialization.
- New integration coverage for command semantics and task lifecycle behavior.

### Changed
- Replaced legacy command paths (`run`, `resume`, `run-tasks`, `init`) with migration guidance and explicit command-specific flows.
- **BREAKING**: `max_rounds` renamed to `review_max_rounds` (TOML key) and `MAX_ROUNDS` renamed to `REVIEW_MAX_ROUNDS` (env var). Old names produce explicit rename errors with upgrade guidance.
- **BREAKING**: All round limits (`review_max_rounds`, `planning_max_rounds`, `decomposition_max_rounds`) now default to `0` (unlimited). Timeout and stuck detection remain as active safeguards; high-watermark warnings fire at round 50 then every 25 rounds in unlimited mode.
- **BREAKING**: Claude and Codex now default to full-access mode (`claude_full_access = true`, `codex_full_access = true`). Claude uses `--dangerously-skip-permissions`; Codex uses `--dangerously-bypass-approvals-and-sandbox`. Set `*_full_access = false` in `.agent-loop.toml` to constrain.
- **BREAKING**: Dual-agent implementation review now uses a mandatory three-gate flow: same-context reviewer approval, fresh-context reviewer approval, then implementer signoff. The old 5/5-only second-review trigger was removed.
- **BREAKING**: `status.json` no longer includes the `rating` field; status payloads are now verdict/reason/timestamp focused.
- Round-limit environment variables now fail with explicit errors on invalid values (previously silently fell back to defaults).
- Implementation/review loops now reconcile reviewer findings with status transitions (including forcing `NEEDS_CHANGES` when unresolved findings remain).
- Quality checks now auto-select platform shell (`sh -c` on Unix, `cmd /C` on Windows) for native Windows support.
- Improved stale status/metrics reconciliation and batch metrics aggregation behavior.
- Updated prompts and state handling to include prior decisions and stronger workflow continuity.

### Fixed
- Per-task execution now clears persisted implementation session IDs between tasks to prevent resume-context leakage into a new task’s first round.
- Stale timestamp fallback now targets active in-progress statuses, reducing false stale handling on terminal verdict statuses.

### Notes
- `reset` preserves `.agent-loop/decisions.md` while clearing `.agent-loop/state/`.
