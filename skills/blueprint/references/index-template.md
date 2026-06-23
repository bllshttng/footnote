# INDEX.md Template

The INDEX.md is the **only file `/target` reads every iteration**. Keep it focused (~200 lines).

---
created: YYYY-MM-DDTHH:MM
# claims: ab-XXXXXXXX             # Optional. When set, `fno backlog intake`
#                                 # updates this idea-state node in place
#                                 # instead of creating a duplicate. /blueprint
#                                 # writes this automatically when the input
#                                 # was an ab-id; do not hand-author it
#                                 # except to repair a prior dangling node.
#                                 # See skills/blueprint/SKILL.md "Plan Claims
#                                 # Ingestion" for the full contract.
# linear: {TEAM}-XXX              # Only if config.linear.enabled
# linear_url: https://linear.app/{workspace}/issue/{TEAM}-XXX
# Per-plan executor lock - transcribed from /think's Locked Decision when one
# was recorded. Acceptable values: do | impeccable | mixed. Omit (or comment
# out) to let the operator's runtime surface inference choose per task. /blueprint
# writes this automatically; do not hand-author it unless you know the design
# doc disagrees with the runtime fallback. See:
#   skills/think/references/executor-routing-prompt.md (think handoff)
#   skills/do/references/executor-resolution.md  (runtime resolver)
# executor: do
# depends_on:                     # Optional: graph edges wired at auto-adopt time
#   - ../2026-04-19-sibling-slug  # sibling plan folder name (resolved against graph.plan_path)
#   - ab-d359579e                 # or an existing graph node ID
# executor: do                    # Plan-level executor for /do waves dispatch.
#                                 # do (default, archer / TDD) | impeccable (frontend-executor / /impeccable craft+critique).
#                                 # Per-task overrides go on the task block (see phase-template.md).
#                                 # Omit to let surface inference decide per task. Frontend-heavy plans
#                                 # benefit from impeccable's design-system awareness; backend keeps 'do'.
#                                 # See docs/guides/per-task-executors.md for the resolver chain.
# Abort conditions — target/do check these at wave/iteration
# boundaries and emit <aborted reason="{name}"> when any predicate is true.
# Defaults catch "spinning in place"; extend for plan-specific concerns.
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
  # - name: scope_creep
  #   predicate: files_outside(plan_path) > 5
  #   reason: "Touching too many files outside the declared scope"
  # - name: test_file_deleted
  #   predicate: any_test_file_deleted
  #   reason: "Test removal is not a legitimate fix"
# Stamp fields (populated by /target ship gate - do not fill manually):
# status: shipped | done
# shipped_at: <UTC ISO8601>
# urls: []
# session_ids: []
---

# [Feature Name] - Implementation Index

> **For Claude:** Use `/do` to implement phases. Use `/target` for full automation.

> **Linear:** [{TEAM}-XXX](https://linear.app/{workspace}/issue/{TEAM}-XXX)
> _(Only if Linear configured. Omit this line if `config.linear.enabled` is absent.)_

**Goal:** [One sentence describing what this builds]

**Architecture:** [2-3 sentences about approach]

**Tech Stack:** [Key technologies/libraries]

**Total Points:** NN | **Phases:** N

---

## Execution Strategy

```yaml
execution_mode: mixed  # sequential | parallel | mixed

# scope: cross-project has been removed. A plan is single-project from the
# executing session's view. For a multi-repo feature, decompose into one backlog
# node per project (linked by blocked_by) via `fno backlog decompose`, then
# `fno backlog update <child> --project <p> --cwd <root>` for any child in a
# different repo (decompose copies the epic's project/cwd to every child). Each
# node ships its own PR and spawn-into-project carries the cross-repo handoff.

waves:
  - wave: 1
    mode: sequential
    tasks: [1.1, 1.2]
    reason: "Foundation - database must complete first"

  - wave: 2
    mode: parallel
    tasks: [2.1, 2.2, 2.3]
    reason: "Independent features, no shared files"

  - wave: 3
    mode: sequential
    tasks: [3.1]
    reason: "UI depends on both API endpoints"

  - wave: 4
    mode: sequential
    tasks: [4.1, 4.2]
    reason: "Tests require all implementation complete"
```

---

## Wave 1: Foundation

**Phases:** 01
**Mode:** sequential
**Rationale:** Foundation - database must complete first

| Phase | File | Points | Depends on |
|-------|------|--------|------------|
| 01 | [01-database.md](./01-database.md) | 13 | - |

## Wave 2: Independent Features

**Phases:** 02, 02b
**Mode:** parallel
**Rationale:** Independent features, no shared files

| Phase | File | Points | Depends on |
|-------|------|--------|------------|
| 02 | [02-core-api.md](./02-core-api.md) | 21 | 01 |
| 02b | [02b-webhooks.md](./02b-webhooks.md) | 8 | 01 |

## Wave 3: UI

**Phases:** 03
**Mode:** sequential
**Rationale:** UI depends on both API endpoints

| Phase | File | Points | Depends on |
|-------|------|--------|------------|
| 03 | [03-ui.md](./03-ui.md) | 18 | 01, 02, 02b |

## Wave 4: Tests

**Phases:** 04
**Mode:** sequential
**Rationale:** Tests require all implementation complete

| Phase | File | Points | Depends on |
|-------|------|--------|------------|
| 04 | [04-tests.md](./04-tests.md) | 16 | All |

_The `## Wave N: <name>` headers above mirror the YAML wave manifest in `## Execution Strategy`. The YAML is consumed by `/do waves` for scheduling; the headers are consumed by humans (browsing the plan) and by backlog wikilinks (`plan_path: 00-INDEX.md#wave-3-ui`). Both must agree — `scripts/validate-plan.sh` enforces this via `validate_wave_section_headers()`. See [section-headers.md](../references/section-headers.md) for the slug rules and worked examples._

_For plans with zero waves (single-phase trivial work), omit the `## Wave N:` sections entirely; `## Execution Strategy` alone suffices._

---

## Phase Dependencies

```
01-database ──┬──→ 02-core-api ──┬──→ 03-ui ──→ 04-tests
              └──→ 02b-webhooks ─┘
```

| Phase | Document | Points | Depends On | Can Parallel With | Project |
|-------|----------|--------|------------|-------------------|---------|
| 1 | [01-database.md](./01-database.md) | 13 | - | - | _(all or key)_ |
| 2 | [02-core-api.md](./02-core-api.md) | 21 | 01 | 02b | _(project key)_ |
| 2b | [02b-webhooks.md](./02b-webhooks.md) | 8 | 01 | 02 | _(project key)_ |
| 3 | [03-ui.md](./03-ui.md) | 18 | 01, 02, 02b | - | _(project key)_ |
| 4 | [04-tests.md](./04-tests.md) | 16 | All | - | _(all)_ |

_The **Project** column maps phases to workspace project keys from settings.yaml (informational; a multi-repo feature is decomposed into per-project backlog nodes rather than executed as one `scope: cross-project` plan)._

---

## User Stories Summary

### Epic 1: [Name] (Phase 2)
| Story | Points | Priority |
|-------|--------|----------|
| As a [user], I want [goal] so that [value] | N | P0 |

### Epic 2: [Name] (Phase 3)
...

---

## Technical Architecture Overview

[High-level architecture: database tables, component structure, key decisions]

---

## Success Metrics

| Metric | Target | Measurement |
|--------|--------|-------------|
| [Metric] | [Target] | [How to measure] |

---

## Goal Alignment

_Read `project.goals` from settings.yaml. For each task, identify which goal it serves._

| Task | Goal | Rationale |
|------|------|-----------|
| 1.1 [name] | G2 | [Why this task serves this goal] |
| 2.1 [name] | ⚠️ None | No goal alignment — justify or remove |

_If no goals are defined in settings.yaml, omit this entire section._

_If >50% of tasks have no alignment, add: "Consider whether this plan serves project objectives."_

---

## Critical Path Trace

Map every user journey through the system. Mark each component's status.

**Status markers:**
- `✅` EXISTS — already built and working
- `🔨` THIS PLAN — task in this plan builds it
- `⚠️` STUB — exists but is a placeholder/mock (must have task to resolve OR scope != feature)
- `❌` NOT BUILT — doesn't exist yet (must have task to resolve OR scope != feature)
- `🔗` EXTERNAL — third-party service/API (note dependency)

**Example:**
```
Journey: User creates a new task via mobile app
User taps "New Task" → ✅ CharacterPickerModal → ✅ API POST /tasks →
🔨 AgentManager.createTask() [Task 2.1] → ⚠️ ContainerRunner._spawnProcess() [Task 3.1] →
🔗 Claude API (external) → ✅ WebSocket update → ✅ Mobile UI refresh
```

If ANY link is ⚠️ or ❌ without a corresponding task AND scope is `feature`:
**This plan is incomplete. Add tasks or change scope.**

---

## Scope Classification

```yaml
scope: feature  # feature | scaffolding | poc
```

| Scope | Meaning | Stub Policy |
|-------|---------|-------------|
| `feature` | Works end-to-end for users | NO unresolved stubs in critical path |
| `scaffolding` | Foundation for future plans | Stubs OK if documented |
| `poc` | Proof of concept / demo | Stubs expected |

---

*Plan created using /blueprint skill with BDD acceptance criteria.*
