---
name: cross-project-pipeline
description: "Orchestrate work across multiple projects. Each project gets its own worktree, atomic commits, sigma-review, and PR. Supports parallel, sequential, and mixed execution modes."
metadata:
  internal: true
---

# Cross-Project Pipeline

Orchestrate multi-project feature development with per-project worktrees, parallel subagents, and automated PR cross-linking.

## When This Runs

This skill is invoked by **target** (directly, via HARD GATE) or by`/do waves` when cross-project scope is detected. Two triggers:

1. `target-state.md` has `cross_project: true` (from `--cross-project` flag) — **target invokes directly**
2. `00-INDEX.md` has `scope: cross-project` —`/do waves` delegates here

## Prerequisites

1. **Work config** — `settings.yaml` must have projects defined (in `work.workspaces.{name}.projects`) with paths for all referenced projects
2. **Git access** — All projects must be git repositories accessible from the current machine
3. **Plan** — 00-INDEX.md with either `scope: cross-project` + `projects:` map, OR enough context to infer projects (see Project Inference below)

## Execution Modes

The pipeline supports three execution modes, set via `execution_mode` in the plan's `00-INDEX.md`:

| Mode | When to Use | Behavior |
|------|-------------|----------|
| `parallel` | Projects are independent (default) | All projects implement + finalize together |
| `sequential` | Each project depends on the previous | Projects run one at a time, each gets PR immediately |
| `mixed` | Some groups depend on others | Groups run in order; projects within a group run in parallel |

### Project Groups

Projects are assigned an `order:` number (default: 1). All order-1 projects complete (implement + PR) before order-2 projects start.

```yaml
# 00-INDEX.md
scope: cross-project
execution_mode: mixed

projects:
  pipeline:
    order: 1              # runs first — schema must exist before frontend
    tasks: [1.1, 2.1]
    type: backend
  webapp:
    order: 2              # starts after order 1 has PRs
    tasks: [2.2, 3.1]
    type: frontend
    depends_on: pipeline   # informational — order value is what enforces this
```

For `sequential` mode: each project is implicitly its own group, ordered by declaration order in the `projects:` map.

For `parallel` mode (default): all projects are group 1 — original behavior, no change.

### Why Immediate PRs Matter

For sequential dependencies (backend schema → frontend consuming it), the backend PR must be created and pushed **before** frontend work begins. This allows:
- Frontend agents to pull the backend branch and build against real schema
- Backend PR to start external review while frontend is still in progress
- Faster overall cycle time vs waiting for everything to finish

## The 4-Step Pipeline

```
parallel mode (default):
  Step 1: SETUP all projects (parallel) → barrier
  Step 2: IMPLEMENT all projects (parallel) → barrier
  Step 3: FINALIZE all projects (parallel) → barrier
  Step 4: LINK all PRs

mixed/sequential mode:
  Step 1: SETUP all projects (parallel) → barrier
  For each order in sequence:
    Step 2: IMPLEMENT order N projects (parallel within order) → barrier
    Step 3: FINALIZE order N projects (parallel within order) → barrier
  Step 4: LINK all PRs
```

## Project Inference (Fallback)

When the plan INDEX does NOT have a `projects:` map (e.g., plan was created before cross-project infrastructure existed):

1. **Read work config** — Load all project names/paths from `settings.yaml` → `work.workspaces.{name}.projects`
2. **Scan plan phase files** — For each phase file, look for:
   - Project names mentioned in task descriptions (matching projects from work config)
   - Repo paths referenced in task content
   - Stack keywords that map to known projects (e.g., "migration" → backend, "workflow" → worker)
   - Phase file names that reference projects (e.g., `04-worker.md`)
3. **Build projects map** — Map each detected project to its tasks:
   ```yaml
   projects:
     api:
       tasks: [1.1, 1.2]        # inferred from phase content
     webapp:
       tasks: [2.1, 2.2, 3.1]   # inferred from phase content
     worker:
       tasks: [4.1, 4.2, 4.3]   # inferred from phase 04
   ```
4. **Confirm with user** — Present: "Detected N projects: [list]. Proceed?" (skip confirmation if running autonomously)

**The `--cross-project` flag overrides INDEX `scope: feature`.** If target-state has `cross_project: true`, treat the plan as cross-project regardless of what the INDEX `scope:` says.

## Process

### 0. Load Context

```markdown
1. Read 00-INDEX.md → extract `scope:`, `execution_mode:`, and `projects:` map (if present)
2. If no `projects:` map: run Project Inference (above)
3. Read settings.yaml → resolve project paths via work config
4. Validate all inferred/declared projects exist on disk
5. Determine feature slug from branch name or plan folder name
6. Set BRANCH_NAME = feature/{feature-slug}
7. Set MEMORY_PATH = main project's memory path (~/.claude/projects/{encoded}/memory/)
8. Parse execution_mode (default: parallel) and build ordered group list:
   - parallel: all projects → order 1
   - sequential: each project → its own order (declaration order)
   - mixed: use explicit `order:` values from projects map
9. Resolve missing `order:` values: if a project has no explicit `order:`,
   look up its `type` in `work.patterns.execution_order` from settings.yaml
   (e.g., `backend: 1`, `frontend: 2`). Fall back to order 1 if not found.
```

### 1. Step 1 — Setup (Parallel Subagents)

For each project in the `projects:` map, dispatch a subagent. Path
resolution and worktree creation flow through
`scripts/lib/worktree-manager.sh` so per-project `worktree_base` from
settings.yaml is honored. See
[skills/_shared/worktree.md](../../_shared/worktree.md) for the
decision matrix.

```markdown
For each project in parallel:
  1. Dispatch subagent with setup prompt (see references/step-prompts.md)
  2. Subagent:
     a. cd to {project.path}
     b. Run scripts/cross-project-setup.sh (which delegates to worktree-manager)
     c. Capture worktree path from JSON output
     d. cd to worktree
     e. Run {project.test_command} for baseline verification
     f. Return: {status: OK|FAILED, worktree_path, test_count}
```

**Direct invocation** (recommended - one-shot, JSON output):
```bash
bash "${CLAUDE_PLUGIN_ROOT}/scripts/cross-project-setup.sh" \
  "{project.path}" "{feature-slug}" "{setup_command}" "{test_command}"
```

**Barrier:** Wait for ALL subagents. If any return FAILED:
- Report which project failed and why
- Ask user: continue with remaining projects, or abort?
- If user says continue: mark failed project as BLOCKED in target-state

### 2–3. Implement + Finalize (Group-Aware)

Steps 2 and 3 execute per **order** when `execution_mode` is `sequential` or `mixed`. For `parallel` mode (default), all projects are order 1, so behavior is unchanged.

```markdown
ORDERS = sorted unique order numbers from projects map  # e.g., [1, 2]

For each ORDER in sequence:
  projects_in_order = projects where project.order == ORDER

  ── Step 2: IMPLEMENT (parallel within order) ──────────────────
  For each project in projects_in_order (parallel):
    1. Determine agent type from project.type (frontend → target, backend → target)
    2. Dispatch agent via Task tool with:
       - subagent_type: {agent_type}
       - prompt: Implementation prompt (see references/step-prompts.md)
       - Context includes:
         - WORKTREE_PATH: {project.path}/.claude/worktrees/{feature-slug}
         - Tasks from INDEX projects map filtered to this project
         - Full task content pasted from plan phase files
         - MEMORY_PATH for centralized memory routing
         - If ORDER > 1: PR URLs from prior orders (for dependency context)
    3. Wait for return: {status: SUCCESS|FAILED|BLOCKED, commits[], concerns[]}

  Barrier: Wait for ALL subagents in this order.
  - If all SUCCESS → proceed to Step 3 for this order
  - If any DONE_WITH_CONCERNS → log concerns, proceed
  - If any FAILED → report failure, mark project BLOCKED
  - If any BLOCKED → report blocker, mark project BLOCKED

  ── Step 3: FINALIZE (parallel within order) ───────────────────
  For each successfully implemented project in this order (parallel):
    1. Dispatch cross-project-finalizer agent with:
       - FEATURE_NAME: {feature name from plan}
       - BRANCH_NAME: {BRANCH_NAME}
       - PROJECT_TYPE: {project.type}
       - RELATED_PROJECTS: [list of other project names]
       - MEMORY_PATH: {MEMORY_PATH}
       - cwd: {worktree_path}
    2. Agent handles: atomic commits → sigma-review → push → gh pr create
    3. Return: {status: SUCCESS|FAILED, pr_url, review_summary}

  Barrier: Wait for ALL subagents in this order. Collect PR URLs.
  Update target-state: set project.pr_url and project.pr_number for each.
  Log: "Order {ORDER} complete: {N} PRs created"

  ── Step 3b: REVIEW (per project in this order) ───────────────
  For each project PR just created:
    1. Run /pr check {pr_number} in the project's worktree cwd
       - Reads reviewer config from settings.yaml (gemini/coderabbit/etc)
       - If reviewer is "none": skip (mark external_review: skipped)
       - Polls for review, implements feedback, pushes fixes
    2. Update target-state: set project.external_review_passed: true

  Note: /pr check runs sequentially per project (not parallel)
  because feedback implementation may require pushes that trigger
  new reviews. Running in parallel would cause push conflicts.

  ── Next order starts only after this order's PRs are reviewed ─
```

**CRITICAL for sequential/mixed:** Each order's PRs are created, reviewed, and have feedback addressed **before** the next order starts. This ensures:
- Backend PRs have external review feedback implemented before frontend builds on them
- Quality gates apply equally to all projects, not just the main thread's project

**For parallel mode:** There is only one order, so Steps 3 + 3b execute once — identical to running these gates on the main thread.

### 4. Step 4 — Link (Main Session)

This step runs in the main session (not subagent):

```markdown
1. Collect all PR URLs from Step 3 results
2. For each PR:
   a. Build "Related PRs" table with all OTHER project PRs
   b. Run: gh pr edit {pr_url} --add-label "cross-project"
   c. Add comment: "Related PRs: {table of other PRs}"
3. Update target-state.md:
   - Set each project's status to COMPLETE
   - Set each project's pr_url and pr_number
4. If ALL projects have PRs:
   - Set overall target-state status ready for promise
   - Report: "All {N} project PRs created and linked"
```

### Related PRs Table Format

```markdown
## Related PRs

This PR is part of a cross-project feature: **{FEATURE_NAME}**

| Project | PR | Status |
|---------|-----|--------|
| pipeline | #123 | Ready for review |
| webapp | #456 | Ready for review |

> Cross-linked by footnote cross-project-pipeline
```

## Error Handling

### Partial Project Failure

If some projects succeed and others fail:
1. Successful projects get PRs created and linked
2. Failed projects are marked BLOCKED in target-state
3. The pipeline reports partial completion
4. Promise tag is NOT emitted until all projects complete
5. On `--resume`, only failed projects are retried

### Worktree Already Exists

If a worktree already exists at the target path:
- Verify it's on the correct branch
- Reuse it (don't recreate)
- Report branch mismatch as WARNING (not error)

### Single-Project Degradation

If `scope: cross-project` is set but only 1 project is listed:
- Log: "Cross-project scope with single project — using standard pipeline"
- Delegate back to normal operator flow (no worktree overhead)

## Configuration

Read from `work` section of settings.yaml:

| Setting | Default | Purpose |
|---------|---------|---------|
| `worktree.base` | `.claude/worktrees` | Worktree location per project |
| `worktree.shared_branch_name` | `true` | Same branch across all repos |
| `cross_project.memory_target` | `main` | Route memory to main project |
| `cross_project.pr_linking` | `true` | Auto-link PRs |
| `cross_project.finalize_model` | `sonnet` | Model for finalizer agent |
| `cross_project.execution_mode` | `parallel` | Override default execution mode (plan INDEX takes precedence) |

## References

- [references/step-prompts.md](references/step-prompts.md) — Subagent prompt templates
- [references/example-workflow.md](references/example-workflow.md) — Complete example scenario
