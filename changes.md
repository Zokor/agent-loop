# Changes — Round 5 Implementation

All 18 tasks completed. 351 tests pass, 0 failures.

## New Modules

| Module | Lines | Purpose |
|--------|-------|---------|
| `src/agent_registry.rs` | 336 | Static registry of agent specs (`LazyLock<BTreeMap>`), command builders, `AgentSpec` struct with `supports_session_resume`, `supports_model_flag`, `OutputFormat`, `Tier` |
| `src/stuck.rs` | 188 | `StuckDetector` with FNV-1a hash-based oscillation detection, configurable thresholds and actions |
| `src/wave.rs` | 350 | Dependency parsing from markdown, topological wave scheduling via Kahn's algorithm + longest-path levelling |
| `src/wave_runtime.rs` | 483 | Wave lock file (`WaveRunLock`), PID liveness check, progress journal (`WaveProgressEvent` JSONL), ISO-8601 timestamps |

## Modified Modules

### `src/config.rs` (1851 lines)
- **Agent struct**: Refactored from enum to `Agent { name, model }` with `known()`, `with_model()`, `clear_model()`, `spec()` methods
- **StuckAction enum**: `Abort`, `Warn`, `Retry` with `from_str_opt`
- **New config fields**: `stuck_detection_enabled`, `stuck_no_diff_rounds`, `stuck_threshold_minutes`, `stuck_action`, `wave_lock_stale_seconds`, `wave_shutdown_grace_ms`, `progressive_context`, `planner_model`, `reviewer_allowed_tools`
- **Model selection**: `implementer_model` and `reviewer_model` flow into `Agent::with_model()` at config construction
- Config now derives `Clone` (needed for wave per-task isolation)

### `src/agent.rs` (1532 lines)
- **`resolve_command()`**: Two-layer design — registry command builder for base args, then config-driven overrides (permissions, session, system prompt)
- **Session resume generalized**: Uses `agent.spec().supports_session_resume` instead of hardcoded `agent.name() == "claude"` checks
- **Codex session resume**: `extract_codex_session_id()`, `codex exec resume <id>` subcommand injection in `resolve_command()`
- **Agent-aware session keys**: Session files now named `{role}-{agent}_session_id` (e.g., `implementer-claude_session_id`) for disambiguation
- **Reviewer sandbox**: Role-based tool selection (`reviewer_allowed_tools` vs `claude_allowed_tools`)

### `src/phases.rs` (2801 lines)
- **Stuck detection integration**: `StuckDetector` created before implementation loop, observed after each diff computation, handles `Abort`/`Warn`/`Retry` signals
- **Progressive context**: `state_manifest(config)` wired into planning and decomposition phases
- **Planning findings**: Reads open findings before each reviewer call, passes to `planning_reviewer_prompt()`, appends progress summary after each round
- **Session key format**: Updated all 4 session key sites to include agent name

### `src/prompts.rs` (1527 lines)
- **Planning reviewer prompt**: Added `open_findings` parameter, findings section in prompt, VERDICT protocol instructions

### `src/state.rs` (3034 lines)
- **`Status::Stuck`** variant with Display impl
- **Planning findings types**: `PlanningFindingStatus`, `PlanningFindingEntry`, `PlanningFindingsFile`
- **Helpers**: `read_planning_findings()`, `write_planning_findings()`, `open_planning_findings_for_prompt()`, `next_planning_finding_id()`, `append_planning_progress()`

### `src/main.rs` (2125 lines)
- **`--wave` flag**: Mutually exclusive with `--per-task`, `--task`, `--file`, `--resume`
- **Wave orchestrator** (`implement_all_tasks_wave`): Wave schedule computation, mpsc channel-based concurrency, dependency failure propagation, fail-fast, git checkpoints, resume via persisted task statuses, lock acquisition/release, journal events, interrupt checking
- **`reset --wave-lock`**: Force-delete stale wave lock
- **Status command**: Shows wave lock info (PID, start time, dead PID warning) and recent wave progress events
- **Stuck detection + wave env vars**: Added to `environment_help()`

### `src/preflight.rs` (438 lines)
- **Model validation**: Warns and clears model for agents that don't support `--model`
- **Session resume validation**: Warns when session persistence enabled for agents without `supports_session_resume`

### `src/error.rs` (167 lines)
- **`Wave(String)`** variant for wave-specific errors

### `src/test_support.rs` (446 lines)
- All new config fields added to `TestConfigOptions` and `make_test_config`

## New CLI Flags & Env Vars

| Flag / Env Var | Default | Description |
|----------------|---------|-------------|
| `--wave` | — | Enable wave-based parallel task execution |
| `--wave-lock` (reset) | — | Force-clear stale wave lock |
| `IMPLEMENTER_MODEL` | — | Model override for implementer |
| `REVIEWER_MODEL` | — | Model override for reviewer |
| `PLANNER_MODEL` | — | Model override for planning phase |
| `REVIEWER_ALLOWED_TOOLS` | `Read,Grep,Glob,WebFetch` | Reviewer read-only sandbox |
| `STUCK_DETECTION_ENABLED` | `0` | Enable stuck detection |
| `STUCK_NO_DIFF_ROUNDS` | `3` | Consecutive no-diff rounds threshold |
| `STUCK_THRESHOLD_MINUTES` | `10` | Wall-clock minutes threshold |
| `STUCK_ACTION` | `warn` | Action: `abort`/`warn`/`retry` |
| `WAVE_LOCK_STALE_SECONDS` | `300` | Lock staleness threshold |
| `WAVE_SHUTDOWN_GRACE_MS` | `30000` | In-flight task grace period on interrupt |
| `PROGRESSIVE_CONTEXT` | `0` | Enable progressive context discovery |

## State File Changes

```
.agent-loop/state/
  wave.lock                              # Wave run lock (PID, timestamp, mode)
  wave-progress.jsonl                    # Append-only wave lifecycle journal
  planning-findings.json                 # Open/resolved planning findings
  planning-progress.jsonl                # Planning round summaries
  implementer-claude_session_id          # Agent-aware session persistence
  implementer-codex_session_id           # (was: implementer_session_id)
  reviewer-codex_session_id
```

## Architecture Decisions

- Registry-based agent resolution: `AgentSpec` provides base command via `command_builder`, `resolve_command()` applies runtime policy
- Wave scheduling uses Kahn's algorithm with longest-path levelling for optimal parallelism
- Stuck detection uses FNV-1a hash of diff content to detect oscillation patterns
- Planning findings create a structured feedback loop between planning rounds
- Session resume is agent-agnostic via `supports_session_resume` capability flag
