# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added
- Structured phase workflow commands: `plan`, `tasks`, and `implement`.
- Batch implementation mode by default (`implement` runs all tasks from `.agent-loop/state/tasks.md` in one loop).
- Task lifecycle persistence for status and metrics in `.agent-loop/state/task_status.json` and `.agent-loop/state/task_metrics.json`.
- Reviewer findings persistence in `.agent-loop/state/findings.json` with structured IDs, severities, summaries, and file references.
- Claude/Codex CLI tuning configuration via env/TOML (`CLAUDE_ALLOWED_TOOLS`, `CLAUDE_SESSION_PERSISTENCE`, effort/token controls, and `CODEX_FULL_ACCESS`).
- Role-aware system prompt injection and Claude session ID persistence per role across rounds.
- Compound learning and reusable decision capture in `.agent-loop/decisions.md`.
- Automatic decisions reference synchronization into root `AGENTS.md` and `CLAUDE.md` on initialization.
- New integration coverage for command semantics and task lifecycle behavior.

### Changed
- Replaced legacy command paths (`run`, `resume`, `run-tasks`, `init`) with migration guidance and explicit command-specific flows.
- **BREAKING**: `max_rounds` renamed to `review_max_rounds` (TOML key) and `MAX_ROUNDS` renamed to `REVIEW_MAX_ROUNDS` (env var). Old names produce explicit rename errors with upgrade guidance.
- **BREAKING**: All round limits (`review_max_rounds`, `planning_max_rounds`, `decomposition_max_rounds`) now default to `0` (unlimited). Timeout and stuck detection remain as active safeguards; high-watermark warnings fire at round 50 then every 25 rounds in unlimited mode.
- **BREAKING**: Claude and Codex now default to full-access mode (`claude_full_access = true`, `codex_full_access = true`). Claude uses `--dangerously-skip-permissions`; Codex uses `--dangerously-bypass-approvals-and-sandbox`. Set `*_full_access = false` in `.agent-loop.toml` to constrain.
- Round-limit environment variables now fail with explicit errors on invalid values (previously silently fell back to defaults).
- Implementation/review loops now reconcile reviewer findings with status transitions (including forcing `NEEDS_CHANGES` when unresolved findings remain).
- Quality checks now auto-select platform shell (`sh -c` on Unix, `cmd /C` on Windows) for native Windows support.
- Improved stale status/metrics reconciliation and batch metrics aggregation behavior.
- Updated prompts and state handling to include prior decisions and stronger workflow continuity.

### Notes
- `reset` preserves `.agent-loop/decisions.md` while clearing `.agent-loop/state/`.
