# Plan: Unified Review Loop Across Plan, Tasks, and Implementation

## Context

`agent-loop` already runs review in all three workflows (`plan`, `tasks`, `implement`), but review rigor and consensus semantics are not uniform across phases. In practice this allows task breakdowns to be accepted in-loop and later flagged by external manual review.

The target behavior is:

1. In dual-agent mode, both agents must participate in review signoff for `plan`, `tasks`, and `implement`.
2. In single-agent mode, exactly one review gate is used (no duplicate self-review pass by the same agent).
3. Findings are durable and safety-checked in every phase before acceptance.

---

## Requirements

### Functional

1. `plan` phase must require reviewer verdict plus implementer signoff in dual-agent mode.
2. `tasks` phase must require reviewer verdict plus implementer signoff in dual-agent mode.
3. `implement` phase keeps reviewer verdict plus implementer signoff in dual-agent mode.
4. Single-agent mode uses one reviewer pass only in all three phases.
5. A phase cannot finalize as approved/consensus while unresolved findings remain.

### Reliability

1. Findings format must be phase-specific and persistent:
   - `planning_findings.json` (already exists)
   - `tasks_findings.json` (new)
   - `findings.json` (implementation, already exists)
2. Safety nets must be symmetric:
   - "needs revision/changes + empty findings" synthesizes a finding from reason text
   - "approved/consensus + open findings" is forced back to revision/changes

### UX/CLI

1. No new top-level command is required for V1.
2. Existing commands (`plan`, `tasks`, `implement`) keep current entrypoints.
3. `status` output should reflect which signoff gate failed (reviewer vs implementer).

---

## Review Protocol Matrix (Target)

| Workflow | Dual-agent | Single-agent |
|---|---|---|
| `plan` | Implementer draft -> Reviewer review -> Implementer signoff (`CONSENSUS` or `DISPUTED`) | Implementer draft -> Reviewer review (`APPROVED`/`NEEDS_REVISION`) |
| `tasks` | Implementer draft -> Reviewer review -> Implementer signoff (`CONSENSUS` or `DISPUTED`) | Implementer draft -> Reviewer review (`CONSENSUS`/`NEEDS_REVISION`) |
| `implement` | Implementer changes -> Reviewer review -> Implementer signoff (`CONSENSUS` or `DISPUTED`) | Implementer changes -> Reviewer review only (remove extra self-review pass) |

Notes:
- Dual-agent "signoff by both" means reviewer and implementer both must explicitly agree before completion.
- Single-agent should not run a second consensus review by the same model; reviewer verdict is the terminal gate.

---

## Gap Analysis vs Current Behavior

1. `plan`:
   - Current: reviewer can directly approve without mandatory final implementer signoff.
   - Needed: dual-agent always requires final implementer signoff.

2. `tasks`:
   - Current: reviewer decision drives consensus/revision directly.
   - Needed: dual-agent implementer signoff step plus persistent findings protocol.

3. `implement`:
   - Current: mostly aligned in dual-agent.
   - Current single-agent path can auto-consensus on 5/5 and still has mixed behavior for other ratings.
   - Needed: single-agent unified to one reviewer gate only.

4. Findings:
   - Current: planning + implementation have structured safeguards, decomposition does not.
   - Needed: decomposition findings parity (`tasks_findings.json` + reconciliation).

---

## Design

### A. Phase-agnostic signoff policy

Add a helper policy in `phases.rs`:

- `requires_dual_agent_signoff(config) -> bool` (`!config.single_agent`)
- `single_agent_one_gate(config) -> bool` (`config.single_agent`)

This policy is applied consistently in planning/decomposition/implementation end-of-review transitions.

### B. Tasks findings parity

Add decomposition findings persistence in `state.rs`:

- `TasksFindingEntry`
- `TasksFindingsFile`
- `read_tasks_findings()`
- `write_tasks_findings()`
- `open_tasks_findings_for_prompt()`
- `next_tasks_finding_id()`

Add reconciliation logic in `phases.rs` similar to planning/implementation safety nets.

### C. New prompts for dual-agent signoff in `tasks`

Add prompt in `prompts.rs`:

- `decomposition_implementer_signoff_prompt(...)`

This mirrors `planning_implementer_revision_prompt` / `implementation_consensus_prompt` semantics, but scoped to task breakdown acceptance.

### D. Plan phase signoff normalization

In `planning_phase()`:

- after reviewer `APPROVED`, if dual-agent, run implementer signoff prompt before marking final consensus.
- if implementer disputes, continue planning rounds with explicit reason and open findings context.

### E. Implementation single-agent simplification

In implementation loop:

- when `config.single_agent == true`, approved reviewer verdict is terminal consensus gate (after findings safety checks).
- skip additional implementer consensus prompt in single-agent mode.
- keep adversarial path dual-agent only.

---

## Files to Modify

| File | Changes |
|---|---|
| `src/phases.rs` | Unify signoff policy across phases; add dual-agent signoff step to `tasks` and `plan`; add tasks findings reconciliation; simplify single-agent implementation terminal path |
| `src/prompts.rs` | Add decomposition implementer signoff prompt; update decomposition reviewer prompt to reference open findings list |
| `src/state.rs` | Add `tasks_findings.json` schema + read/write/helpers |
| `src/main.rs` | No command changes expected; optional status/help wording updates |
| `README.md` | Update workflow documentation matrix and single-agent behavior notes |
| `tests/*` | Add integration and unit coverage for new phase transitions and findings safety nets |

---

## Implementation Plan

## Phase 1: Tasks findings protocol

1. Add `tasks_findings.json` state model + helpers in `state.rs`.
2. Extend decomposition prompts to include open findings in reviewer context.
3. Implement reconciliation in `phases.rs`:
   - `NEEDS_REVISION` + empty findings -> synthesize `T-001`
   - `CONSENSUS/APPROVED` + open findings -> force `NEEDS_REVISION`

## Phase 2: Dual-agent signoff for tasks

1. Add implementer signoff prompt for decomposition.
2. After reviewer approves in decomposition:
   - dual-agent: run implementer signoff
   - single-agent: finalize directly
3. Persist round history summary for reviewer and implementer signoff outcomes.

## Phase 3: Dual-agent signoff for plan

1. In planning flow, normalize end condition:
   - reviewer approval is not terminal in dual-agent mode
   - implementer signoff required
2. Preserve existing planning findings behavior and dispute propagation.

## Phase 4: Single-agent one-gate normalization for implementation

1. Remove extra single-agent self-review step for approved reviews.
2. Make reviewer gate terminal in single-agent after findings reconciliation.
3. Keep dual-agent adversarial + consensus flow unchanged.

## Phase 5: Docs and status clarity

1. Update `README.md` workflow section with exact dual vs single behavior.
2. Ensure `status.json.reason` identifies gate source (`reviewer` or `implementer-signoff`).

---

## Verification

## Unit tests

1. `tasks` findings reconciliation:
   - needs revision + empty findings -> synthesized `T-001`
   - approved/consensus + open findings -> forced `NEEDS_REVISION`
2. planning dual-agent approval path requires implementer signoff.
3. decomposition dual-agent approval path requires implementer signoff.
4. single-agent implementation approval finalizes without second self-review.

## Integration tests

1. Dual-agent plan:
   - reviewer approves
   - implementer disputes
   - phase remains open and continues round.
2. Dual-agent tasks:
   - reviewer approves
   - implementer consensus required before success exit.
3. Single-agent tasks/plan/implement:
   - exactly one review gate is exercised per round.
4. Regression:
   - existing resume behavior (`plan/tasks/implement --resume`) remains valid.

## Manual smoke

```bash
cd agent-loop
cargo build
cargo test

# Dual-agent path
SINGLE_AGENT=0 agent-loop plan "..."
SINGLE_AGENT=0 agent-loop tasks
SINGLE_AGENT=0 agent-loop implement --task "Task 1: ..."

# Single-agent path
SINGLE_AGENT=1 agent-loop plan "..."
SINGLE_AGENT=1 agent-loop tasks
SINGLE_AGENT=1 agent-loop implement --task "Task 1: ..."
```

Acceptance criteria:
- Dual-agent phases do not finish without both-agent signoff.
- Single-agent phases use one review gate only.
- No phase can finalize with unresolved findings.

---

## Risks and Mitigations

1. Increased round count in dual-agent mode:
   - Mitigation: keep bounded by existing max-round configs; improve prompts for tighter signoff.

2. Prompt complexity drift:
   - Mitigation: keep prompt templates phase-specific but share common status/write contract language.

3. Resume/state migration risk with new `tasks_findings.json`:
   - Mitigation: default empty file behavior; tolerant reads; compatibility tests for missing file.

---

## Out of Scope (This Plan)

1. New standalone `agent-loop review` subcommands.
2. Model arbitration/voting across more than two agents.
3. Hard deterministic static analysis of business correctness in generated `tasks.md`.
