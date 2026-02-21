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
- Claude now defaults to `--allowedTools` (instead of dangerous skip-permissions) and supports opt-in full access.
- Codex now defaults to `--full-auto --json --color never`, with parsed NDJSON text/usage and opt-in dangerous full access.
- Implementation/review loops now reconcile reviewer findings with status transitions (including forcing `NEEDS_CHANGES` when unresolved findings remain).
- Quality checks now auto-select platform shell (`sh -c` on Unix, `cmd /C` on Windows) for native Windows support.
- Improved stale status/metrics reconciliation and batch metrics aggregation behavior.
- Updated prompts and state handling to include prior decisions and stronger workflow continuity.

### Notes
- `reset` preserves `.agent-loop/decisions.md` while clearing `.agent-loop/state/`.
