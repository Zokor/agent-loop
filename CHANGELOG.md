# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added
- Structured phase workflow commands: `plan`, `tasks`, and `implement`.
- Batch implementation mode by default (`implement` runs all tasks from `.agent-loop/state/tasks.md` in one loop).
- Task lifecycle persistence for status and metrics in `.agent-loop/state/task_status.json` and `.agent-loop/state/task_metrics.json`.
- Compound learning and reusable decision capture in `.agent-loop/decisions.md`.
- Automatic decisions reference synchronization into root `AGENTS.md` and `CLAUDE.md` on initialization.
- New integration coverage for command semantics and task lifecycle behavior.

### Changed
- Replaced legacy command paths (`run`, `resume`, `run-tasks`, `init`) with migration guidance and explicit command-specific flows.
- Quality checks now auto-select platform shell (`sh -c` on Unix, `cmd /C` on Windows) for native Windows support.
- Improved stale status/metrics reconciliation and batch metrics aggregation behavior.
- Updated prompts and state handling to include prior decisions and stronger workflow continuity.

### Notes
- `reset` preserves `.agent-loop/decisions.md` while clearing `.agent-loop/state/`.
