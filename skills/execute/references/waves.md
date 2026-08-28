<!-- style-exception: mechanical verb rename preserves pre-existing prose -->
# Do — Waves Mode (Wave Orchestration)

> **Multi-CLI:** If not on Claude Code, see [cli-tool-mapping.md](cli-tool-mapping.md) for tool equivalents.

Interactive orchestrator for multi-phase implementation plans. Research, waves, verification - stops and asks when things go wrong. This is the `waves` mode of `/execute`; the router has already resolved the mode and is running this body inline.

Provider-aware execution is hook-driven. A parallel wave downgrades to sequential only when tasks share explicit files or hidden shared outputs; the downgrade always carries an explicit reason.

## Usage

```
/execute waves                        # Execute plan in current directory
/execute waves path/to/plan           # Execute specific plan
/execute waves frontend               # Execute with frontend expertise
/execute waves backend path/to/plan   # Execute specific plan with backend expertise
/execute waves adversarial            # Enable adversarial challenge after verification
/execute waves research               # Force research phase even when conditions not fully met
/execute waves adversarial research path/to/plan   # Combinable positional modifiers
/execute waves resume                  # Resume from last checkpoint
/execute waves continue                # Continue from STATE.md
/execute waves retry                   # Retry failed task
```

`operator` is a one-release alias for `waves` (`/execute operator ...` == `/execute waves ...`).

**Available expertise:** frontend, backend, architect, fullstack, devops, qa

## Core Behavior

1. **Validate** plan structure before spending tokens
2. **Load** execution strategy from the plan's Execution Strategy section
3. **Optimize** parallelism from file ownership map (when available)
4. **Research** codebase via read-only workers (when scratchpad available)
5. **Execute** waves according to mode (sequential/parallel)
6. **Verify** changes from a fresh perspective
7. **Report** completion with all results

## Prerequisites

- Single `.md` plan from `/blueprint`
- An `## Execution Strategy` section in the plan body
- Task blocks with detailed tasks (in that Execution Strategy)

## Process

### 0. Plan Validation (MANDATORY)

Validate plan structure before execution. On ERROR: stop. On WARN: log and proceed. On PASS: proceed.

Load [plan-validation.md](plan-validation.md) for the full protocol.

#### Joiner posture (`FNO_WORKER_NAME` starts with `j-`)

A worker spawned by `fno backlog join <node>` (`j-<node>-<k>`) is a VISITOR in the node holder's worktree, not a target. First: `.fno/join-briefs/<node>.md` carries your joiner brief (the lead and mail hub, and your claim discipline) - when that file exists for the bound node, read it before dispatching. The brief's band table names YOUR band (one worker per band, the lead is the highest band): set `FNO_WORKER_BAND` from your row before the dispatch round - the spawn-time env export of the same name cannot reach a daemon-forked worker, the brief file can. Never run `target init` and never take the node claim: the target session keeps it, and a joiner that runs `target init` anyway gets the existing "another session holds the node, not this caller" refusal - on that refusal, stop with `<help>` instead of retrying. Read `graph_node_id` from THIS worktree's manifest with `target_state_field` (step 3e), and let `FNO_WORKER_NAME` be the task-claim holder (the joiner 1 contract): the task verbs bind their holder to the roster name, so each joiner is provable as a distinct owner. Everything else in this flow runs unchanged - a joiner's stop hook exits 0 (`visitor allowed`) because its session id matches no manifest in the worktree.

### 0b. Structural Context (AUTO)

Generate fresh codemap for structural awareness:

```bash
fno doctor codemap --tokens 2048 2>/dev/null || true
```

If `fno` is unavailable or `fno doctor codemap`'s deps are missing, skip silently. Read `.fno/codemap.md` if it exists. Use it to identify god nodes (high-PageRank files) that need careful handling during wave execution, and to understand module dependency flow for wave ordering.

### 1. Load Execution Strategy

Read and parse the execution strategy from the plan's `## Execution Strategy` section.

#### Optional: Read Plan Summary from Scratchpad

Before loading the full execution strategy, check if a plan summary exists in the scratchpad:

```bash
SCRATCHPAD=$(sed -n 's/^scratchpad_path:[[:space:]]*//p' .fno/target-state.md 2>/dev/null)
if [[ -n "$SCRATCHPAD" && -f "$SCRATCHPAD/plan-summary.md" ]]; then
  # Plan summary has condensed constraints and execution notes
  # Use this for context, but always read the plan for the canonical strategy
fi
```

The plan summary supplements but never replaces the plan.

#### Foreign-wave handling (spawn-into-project)

The `scope: cross-project` parallel-worktree pipeline has been removed;
`/execute waves` no longer delegates to it. Instead, resolve each wave's project
(from its `project:` field / node) against the workspace map and, when a wave
belongs to a project OTHER than the session's:

- **Foreign node unblocked (ready):** first deterministically isolate the worker into a dedicated worktree: `wt=$(fno agents workspace worktree ensure --repo <foreign-root> --name "target-<foreign-node>")`. On success, link shared state into it best-effort: `[[ -f <foreign-root>/scripts/setup/setup-worktree.sh ]] && CANONICAL=<foreign-root> WORKTREE=$wt bash <foreign-root>/scripts/setup/setup-worktree.sh` (the verb is package code and cannot run this repo-root script itself; the caller does). Then `fno agents spawn --cwd "${wt:-<foreign-root>}" "/target <foreign-node>"` (the `${wt:-...}` fallback launches in the foreign main checkout when `ensure` fails for any reason, so the dispatch is never blocked). Mark the wave delegated, print a `spawned target-<node> --cwd ${wt:-<foreign-root>}` receipt, and continue this session's own waves.
- **Foreign node still blocked** (e.g. `blocked_by` the current, not-yet-merged node): do NOT spawn (the worker would refuse). Record a `fno backlog carveout add --kind deferred` entry, print a `deferred <node> to <project>; dispatch on <blocker> merge` line, and rely on `fno backlog advance` to dispatch it after merge.
- **Foreign node blocked but `dep=contract`** (the optimistic stub pass, G3): dispatch it **NOW** instead of deferring. The node carries `dep: contract` + `stub_against: <ref>` + `contract_version: N` (set by `fno backlog decompose`, G2). Isolate the worker the same way first (`wt=$(fno agents workspace worktree ensure --repo <foreign-root> --name "target-<foreign-node>")`, then the same best-effort `setup-worktree.sh` link as the ready bullet), then spawn `fno agents spawn --cwd "${wt:-<foreign-root>}" "/target <foreign-node>"` and print a `stubbed <node> vs contract v<N>` receipt. The spawned first pass MUST: build against the pinned `## Interface Contract`, **stub** the surface that needs the blocker landed, open its PR as **draft** (`gh pr create --draft` - it must not merge with mocks), emit its manifest via `fno do pr stub-manifest write --node <node> --contract-version <N> --contract-ref <ref> --contract-test '<cmd>' --stubs-json '[{stub_id,file,symbol,contract_ref,kind}, ...]'`, then **END**. Reconciliation is a later merge-triggered pass (G4); the first pass never waits for the blocker. **`--contract-test` is REQUIRED**: it is the executable command the G4 reconcile pass runs against the now-landed schema to authorize de-stubbing (the blocker ships this suite as part of its `## Interface Contract`; Locked Decision 5). A manifest without it makes G4 refuse auto-de-stub (fail-closed), so the draft PR strands - never omit it. For a non-typed contract, a thin schema-snapshot or type-check command is the minimum viable gate.
- **Never** `cd` into the foreign repo and execute locally.

A pure single-project plan (zero foreign waves) behaves byte-identically to
before: no spawn, no deferral, normal wave execution below.

```yaml
execution_mode: mixed

waves:
  - wave: 1
    mode: sequential
    tasks: [1.1]
    reason: "Foundation - must complete first"

  - wave: 2
    mode: parallel
    tasks: [2.1, 2.2]
    reason: "Independent features, no shared files"
```

If no execution strategy found, fall back to sequential execution of all phases.

### 1b. Dynamic Parallelization (automatic)

If the plan contains a `## File Ownership Map` section, load
[dynamic-parallelization.md](dynamic-parallelization.md)
and run the set intersection algorithm. Sequential waves with provably
disjoint task file sets are upgraded to parallel. Only upgrades, never
downgrades. Skip if no file ownership map found.

### 1c. Research Phase (when available)

Check activation conditions:
1. Scratchpad exists with `research/` directory
2. Plan declares 2 or more waves in its Execution Strategy

If all conditions met (or `research` modifier is set), load
[research-protocol.md](research-protocol.md)
and execute the research phase before the first implementation wave.

If conditions not met: skip. Implementation proceeds with existing
synthesis protocol only.

### 2. Execute Waves

**Before every wave starts**, evaluate the plan's `kill_criteria:` block.
If a predicate fires, stop wave dispatch, record the abort in STATE.md,
and return an abort signal to the caller (target) by emitting
`<aborted reason="{name}">MISSION ABORTED: {reason}</aborted>` in the
user-facing output. In-flight parallel tasks within the PREVIOUS wave
are allowed to complete atomically (no half-state) before the abort fires
- that is, the check happens at wave boundaries, not mid-wave.

```bash
PLAN_PATH="${PLAN_DIR:-}"  # resolved in Step 1 "Load Execution Strategy"
# kill-criteria.sh was folded into the fno-agents binary (US1, ab-58645f63).
# `fno do phase kill-check` prints `KILL_CRITERIA_FIRED <name>|<reason>` and exits
# 1 when a predicate fires, exits 0 (empty) when none fire, and exits 2 when the
# fno-agents binary is unavailable. Branch on the exit code: only rc 1 WITH the
# marker aborts; rc 2 (or any other non-zero) is an infra failure that warns and
# skips, never aborting the wave with an empty kill reason (codex PR #515 P2).
if [[ -n "$PLAN_PATH" ]] && command -v fno >/dev/null 2>&1; then
    KC_OUT=$(fno do phase kill-check "$PLAN_PATH" 2>/dev/null); KC_RC=$?
    if [[ $KC_RC -eq 1 && "$KC_OUT" == KILL_CRITERIA_FIRED* ]]; then
        KC_NAME="${KC_OUT#KILL_CRITERIA_FIRED }"; KC_NAME="${KC_NAME%%|*}"
        KC_REASON="${KC_OUT##*|}"
        echo "waves: kill_criteria fired at wave boundary - $KC_NAME: $KC_REASON" >&2
        # Record in STATE.md so target / the user can see which wave aborted.
        # Emit <aborted> tag in next user-facing turn to signal the stop hook.
    elif [[ $KC_RC -ne 0 ]]; then
        echo "waves: kill-check unavailable (rc=$KC_RC); skipping kill-criteria gate" >&2
    fi
fi
```

Backward compat: plans without `kill_criteria:` return exit 0 (no abort).
Malformed predicates log WARN and are skipped (never abort on an
unparseable criterion).

#### Session-Project Invariant (per wave, MANDATORY)

**Before executing any wave, check whether it belongs to this session's
project.** A `/execute` session operates only in its own project; a wave whose
`project:` differs from the session project is **foreign** and must NOT be
executed here. Resolve the session project (plan frontmatter `project:`, or the
target-state node's `project`) and compare it to each `wave.project`:

- **local** (`wave.project` empty or == session project): execute normally.
- **foreign + unblocked node**: spawn `fno agents spawn --harness claude --cwd
  <root> "target-<node>" "/target <node>"`, mark the wave DELEGATED in STATE.md,
  continue this session's own waves. Never `cd` into the foreign repo.
- **foreign + blocked node**: do NOT spawn (a worker would refuse on the blocked
  node); record `fno backlog carveout add --kind deferred ...` and rely on G1's
  merge-triggered dispatch. Log the deferral. **Exception - `dep=contract`:**
  dispatch NOW (the optimistic stub pass); see the `dep=contract` bullet under
  "Foreign-wave handling" above for the first-pass contract (build vs the pinned
  contract, draft PR, `fno do pr stub-manifest write`, then end).
- **foreign + unmapped project** (`fno backlog project-root` exits 1) or
  **node-less foreign wave**: REFUSE by name; do not guess a cwd.

Every foreign wave prints a one-line receipt or deferral - none is silently
skipped. Load
[session-project-invariant.md](session-project-invariant.md)
for the full decision table, exact commands, and receipt formats.

For each DISPATCH ROUND, drive off the ready set, not wave order:

```bash
# One JSON object on stdout: {"ready": [...], "completed": [...], "claimed": [...],
# "blocked": [...], "blocked_on": {task: [blockers]}, "bands": {task: band}}.
# Pass --node "$NODE_ID" when graph_node_id is bound (the same field step 3e
# reads): cross-session done/in_progress task rows then join this session's
# STATE.md [x] as completions and claims. A failed node read degrades to
# STATE.md only - proceed, never stall on the richer read.
READY_ARGS=(--ready --state .fno/STATE.md)
if [[ -n "${NODE_ID:-}" ]]; then
    READY_ARGS+=(--node "$NODE_ID")
fi
# A joined worker filters the pull to its own band. Neither env export
# (FNO_WORKER_BAND, FNO_WORKER_NAME) reaches a daemon-forked worker, so the
# fallback resolves the roster name from the registry binding (whoami's
# worker_name - the same rung resolve_task_holder reads) and looks up the
# band table the join brief carries. The brief exists only in a joined
# worktree, which gates the whole path; a lone target matches no row and
# stays unfiltered.
WORKER_BAND="${FNO_WORKER_BAND:-}"
if [[ -z "$WORKER_BAND" && -n "$NODE_ID" && -f ".fno/join-briefs/${NODE_ID}.md" ]]; then
    WORKER=$(fno whoami --json 2>/dev/null | python3 -c \
        "import json,sys; print(json.load(sys.stdin).get('worker_name') or '')" 2>/dev/null)
    WORKER_BAND=$(awk -F'|' -v w="$WORKER" \
        'tolower($2) ~ tolower(w) {gsub(/ /, "", $3); print $3; exit}' \
        ".fno/join-briefs/${NODE_ID}.md" 2>/dev/null)
fi
if [[ -n "$WORKER_BAND" ]]; then
    READY_ARGS+=(--band "$WORKER_BAND")
fi
READY_JSON=$(python3 skills/execute/orchestrator.py "$PLAN_PATH" "${READY_ARGS[@]}")
```

A task appears under `ready` when its effective blockers are all complete and no peer holds it. A declared `blocked_by` list wins, including an explicit empty list. A task without that key derives every task in the previous wave. Inside a parallel wave, `collision.partition` adds derived edges: tasks sharing a file (or a hidden shared output root) serialize in wave order, and a task with no parseable file list waits for every evaluated sibling. Live foreign task claims appear under `claimed`. Stale or self-owned claims are re-offered. Tasks held back by unfinished blockers appear under `blocked_on` with those blockers and re-enter `ready` on a later round, so they need no action now. Unknown task-row statuses appear under `blocked` and never dispatch. Dispatch every entry under `ready` before running the query again:

**If mode: sequential**
- Execute each ready task in order using fno:archer
- Wait for completion before next task
- Update STATE.md after each task

**If mode: parallel**
- Resolve provider capabilities first (a sequential-fallback provider still downgrades the wave)
- Spawn the whole `ready` set concurrently - Task tool, multiple concurrent calls
- With `--node` bound: update STATE.md as each task completes, then rerun the `--ready` query; do not wait for the round - the peers' live task rows hold their in-flight claims back
- Node-less: wait for the whole round to complete, update STATE.md, then re-query - with no node rows an in-flight sibling still reads `ready`, and re-dispatching it double-spawns the work
- Tasks under `blocked_on` need no action; their derived edges hold them until a later round

`bands` maps each task to its wave's `difficulty` band, with the plan frontmatter band as the fallback. A pulling worker takes only tasks at or below its own band. The band-to-harness-and-model mapping lives in config, never in the plan.

**Joined workers pull one task, never the set.** When `--band` was passed (a joined worker, `FNO_WORKER_NAME` starts with `j-`), the response carries `above_band` (tasks above your band, shown not offered) and `ready` sorted highest band first. Do NOT dispatch the whole `ready` set: take the FIRST entry, claim it (step 3e), then rerun the query - your claim holds the task out of every sibling's next `ready`. When every `ready` entry is above your band, dispatch nothing this round and re-query after the peers release their claims. A lone target (no `--band`) keeps whole-set dispatch exactly as below.

An empty `ready` list does not prove execution is complete. If `claimed` is
non-empty, wait for peer claims to leave `in_progress` and rerun `--ready`. If
`blocked` is non-empty, stop with a refusal naming those task ids until their
statuses are repaired. Proceed to verification only when every plan task is
in `completed` and both `claimed` and `blocked` are empty. A held (`[~]`) task
reappears after its peer's row leaves `in_progress`. A `done` row never does.

**Gemini note:** Gemini always runs main-thread sequential; a parallel wave downgrades and records the reason. There is no project-agent upgrade path (the experimental mode was removed when Google deprecated the Gemini CLI).

See [wave-patterns.md](wave-patterns.md) for the decision tree on sequential vs parallel.

### 3. Spawn Task Executors

Use Task tool to spawn fresh executors for each task. Before dispatch, resolve
the per-task executor with the three-tier chain (task → plan → surface
inference → `do`):

```bash
# Resolve task.executor with documented precedence:
#   1. Explicit `executor:` on the task block (highest)
#   2. Explicit `executor:` on the plan frontmatter
#   3. Surface inference from the task's file list
#      (fno.executor._surface)
#   4. 'do' (default)
TASK_EXECUTOR=$(TASK_EXEC="$task_executor_field" \
                PLAN_EXEC="$plan_executor_field" \
                TASK_FILES="$task_file_list" \
                bash skills/execute/scripts/resolve-executor.sh)

case "$TASK_EXECUTOR" in
    impeccable) SUBAGENT_TYPE="frontend-executor" ;;
    *)          SUBAGENT_TYPE="archer" ;;  # tdd, do, anything else
esac
```

The resolver fail-closes to `tdd` (archer) on any unknown executor name,
so a typo in plan frontmatter cannot silently route to the wrong subagent.
See [executor-resolution.md](executor-resolution.md)
for the full chain, the locked surface inference list, override paths,
and failure modes.

### 3d. PRODUCT.md dispatch gate (impeccable tasks only)

Before dispatching `frontend-executor` for any task whose resolved executor is
`impeccable`, re-check PRODUCT.md presence and freshness at dispatch time:

```python
from orchestrator import check_product_md_for_dispatch

gate_passed = check_product_md_for_dispatch(
    repo_root=Path(repo_root),
    plan_path=plan_path,
    stages=task_stages,  # e.g. ["craft", "critique", "harden"]
)
if not gate_passed:
    # <help> has already been emitted to stdout.
    # Return early - do NOT dispatch frontend-executor.
    return
```

The function searches in order: `${REPO_ROOT}/PRODUCT.md`,
`${REPO_ROOT}/.agents/context/PRODUCT.md`, `${REPO_ROOT}/docs/PRODUCT.md`.
It treats the file as stale if it is shorter than 200 chars or if `[TODO]`
markers make up more than 25% of its content (mirrors /impeccable's loader
contract).

If MISSING or STALE, it emits:

```
<help reason="missing-product-md" evidence="path/to/plan, stages: [craft, critique, harden]">
PRODUCT.md required by /impeccable but missing or stale.
Run /impeccable teach, then resume target.
</help>
```

The loop pauses. The user runs `/impeccable teach`, then resumes target.

**This is the actual hard gate.** /blueprint's warning at plan-creation time is
heads-up only; a stale /blueprint from yesterday cannot grant permission for a
missing-at-dispatch PRODUCT.md because waves mode always re-checks.

DESIGN.md is NOT gated here. The `prerequisites_optional:` block from /blueprint
passes through informationally to the agent dispatch envelope; waves mode does
not re-check or block on it.

**The archer agent enforces TDD discipline:**
- Pre-flight checks (dev server, test runner)
- Write test first, verify it fails (red)
- Implement minimal code to pass (green)
- Database verification (not just UI tests)
- Structured return contract (SUCCESS/FAILED/BLOCKED)

**The frontend-executor agent enforces design-aware iteration:**
- Drives the full /impeccable stage pipeline with a single shared iteration budget
- Budget: `config.executors.impeccable.max_iterations_per_task` (default 8),
  applied to the **entire stage loop** (craft -> critique -> harden -> ...),
  not per-stage. See [executor-resolution.md](executor-resolution.md)
  for the single-budget contract.
- Exit verdict when ceiling trips is two-tier (SUCCESS / DONE_WITH_CONCERNS /
  FAILED), NOT a hard FAILED reflex. Score determines which tier applies.
- Critique findings are advisory input to `/review`, NEVER gate-passing
  evidence on their own (locked decision #3)
- waves mode writes the wave-end scratchpad summary; frontend-executor writes
  per-task scratchpad notes

See `agents/archer.md` and `agents/frontend-executor.md` for full
protocols. Domain-specific checklists are in
`skills/execute/references/domain-checklists.md` - the orchestrator injects
the appropriate one into `.fno/CONTEXT.md` before spawning the agent.

If expertise was specified (e.g., `/execute waves frontend`), inject expertise content into each agent prompt. Load [expertise-injection.md](expertise-injection.md) for the injection protocol.

### 3a. Synthesis Protocol (MANDATORY - Before Every Dispatch)

**The orchestrator must prove understanding before dispatching any worker.**

When constructing a Task tool prompt for archer, the orchestrator MUST:

1. **Read the plan task** - not just the task title, but the full task body including Files, AC, and Steps
2. **Read the target files** - actually open and read the files listed in the task's Files section
3. **Synthesize into a specific prompt** that includes:
   - Exact file paths the worker should modify (verified to exist)
   - Line numbers or function names where changes go
   - What the current code does (from reading it, not from the plan description)
   - What the code should do instead (the specific change)
   - Why this approach works (from understanding the codebase, not parroting the plan)

**NEVER delegate understanding. Synthesize it.**

Load [synthesis-checklist.md](synthesis-checklist.md) for the full anti-delegation rules, pre-dispatch checklist, and constraint injection protocol.

### 3e. Claim the task before dispatch (node-bound runs)

Before spawning the executor for a task, take the task claim THROUGH the status transition, never a separate claim call a worker can forget. When the run has a bound node (`.fno/target-state.md` `graph_node_id`, read with `target_state_field` from `scripts/lib/target-guard.sh`):

```bash
# The plugin root first (installed plugin: cwd is the consumer's project),
# the repo root second (development checkout).
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-}}"
GUARD="$PLUGIN_ROOT/scripts/lib/target-guard.sh"
[[ -f "$GUARD" ]] || GUARD="$(git rev-parse --show-toplevel 2>/dev/null)/scripts/lib/target-guard.sh"
[[ -f "$GUARD" ]] && source "$GUARD"
NODE_ID=$(target_state_field graph_node_id)
if [[ -n "$NODE_ID" ]]; then
  fno backlog task update "$NODE_ID" "$TASK_ID" --status in_progress
  TASK_CLAIM_RC=$?
  [[ $TASK_CLAIM_RC -eq 0 ]] || echo "task $TASK_ID not claimed (rc=$TASK_CLAIM_RC)"
fi
```

For a joiner (`FNO_WORKER_NAME` starts with `j-`, step 0 posture) this block is the whole claim path: the manifest read still resolves `graph_node_id` (that field names the node, not the session), and the task verbs bind the claim's holder to the joiner's roster name, so a joined worker's owner is provable without any session identity of the target.

Branch on `TASK_CLAIM_RC`. Every nonzero code means you did NOT take the claim, so dispatching anyway is the double-worker bug this step exists to close.

- **Exit 0**: the claim is yours (idempotent re-acquire if you already hold it). Dispatch as planned.
- **Exit 1**: the plan bound to the node is unreadable, or the row vanished between the read and the write. A stop, not a skip: the rows derive from the plan, so no task of this node can be claimed until it is readable.
- **Exit 2**: the task id is not in the plan, or `--status` was misspelled. A stop: the wave and the plan disagree about what a task is.
- **Exit 6**: this node has no task grain. Either no plan is bound to it, or the project runs a non-graph tracker backend that has no task rows at all. Dispatch unguarded, exactly as the no-bound-node case below. It is its own code because it is common rather than exceptional: about one live node in five carries no `plan_path`, and a tracker-backend project carries none anywhere, so reading it as a stop would halt waves that ran fine before task rows existed.
- **Exit 3**: a peer session holds the task, or the task is already `done`. The message says which. For a peer holder, log `task <id> held by <holder>, skipping this round`, write `- [~] <id>: held by <holder>` to STATE.md, and move to the next task in the wave, the same shape as lane-fill leaving a peer-lane node ready. A `[~]` task needs no re-offer bookkeeping of its own: the next `--ready` read omits every row still `in_progress`, so once the peer releases it (the row leaves `in_progress`) the task simply returns under `ready`. For a `done` row there is no holder and no later pass that changes the answer: write `- [x] <id>` and never re-offer it, or the wave spins on shipped work forever.
- **Exit 4**: no provable session id or session pid for the claim. A stop, not a skip: emit `<help reason="task claim needs a session id" evidence="task <id> of node <node>">`.
- **Exit 5**: the graph itself is unreadable. A stop for the whole wave, never a skip. This code is distinct from 3 on purpose: reading a corrupt graph as "a peer holds it" would make every task in the wave skip silently and log a holder that does not exist.
- **No bound node** (plan-only sessions with no `graph_node_id`): skip the call and dispatch as today. There is no task row to guard.

The release rides the boundary emit you already run after each task (3b): `task_done` with outcome `SUCCESS` or `DONE_WITH_CONCERNS` releases through `--status done`; `blocked` or outcome `FAILED` gives the task back with `--status pending` so the next ready worker can pick it up. Pass `--outcome` explicitly on every `task_done`. The outcome vocabulary is closed and an omitted outcome settles nothing.

A flat run that carries a `graph_node_id` settles at the boundary too. Only the absence of a bound node skips it, never the flat surface itself.

**Sequential Wave:**
```
For each task in wave.tasks:
  1. Announce: "## Executing Task {task}"
  2. Spawn archer with task details
  3. Wait for completion
  4. Update STATE.md
  5. Continue to next task
```

**Parallel Wave:**
```
1. Announce: "## Executing Wave {n} (parallel: {count} tasks)"
2. Spawn ALL targets concurrently
3. Wait for ALL to complete
4. Collect results (success/failure for each)
5. Update STATE.md with all results
6. If any failed, report and pause
```

### 3b. Atomic Commits Per Task (MANDATORY)

After each task completes successfully (return contract: SUCCESS or DONE_WITH_CONCERNS),
create an atomic commit scoped to that task's changed files before proceeding to the next task.

```bash
# Stage only files changed by this task (from return contract or git diff)
git add <specific-files>
git commit -m "feat(scope): what changed

Task {N.M}: {task title}"
```

**Rules:**
- One commit per completed task, not one commit per wave
- Never `git add .` or `git add -A` - only stage task-relevant files
- If a task touches multiple files, they go in one commit (that's atomic)
- If a task fails, do NOT commit partial work - fix first, then commit
- Subagent (archer) tasks: the agent creates the commit as part of its work

**Why:** Large undifferentiated diffs at the end of execution make review
harder, break `git bisect`, and prevent granular reverts. The review phase
and PR are more effective when each task has its own commit.

**Status-breakpoint emit (x-dbaf, best-effort).** Right after each task's return
is parsed, emit its boundary event so observers can track the run. `task_started`
already fired from `resolve-executor.sh` at dispatch; `run_summary` fires from
`finalize` at the terminal. Emit the middle boundary (one non-fatal line):

```bash
# task_done with the parsed outcome (SUCCESS | DONE_WITH_CONCERNS | FAILED):
python3 "${SKILL_DIR:-skills/execute}/orchestrator.py" --emit-boundary task_done \
  --task "{N.M}" --outcome "{status}" --data '{"commit":"{sha}","concerns":{k}}'
# blocked on a RESULT: BLOCKED return (or a <help> distress emission):
python3 "${SKILL_DIR:-skills/execute}/orchestrator.py" --emit-boundary blocked \
  --task "{N.M}" --data '{"reason":"{why}"}'
```

`--run`/`--node` fall back to the manifest. Never gate on the emit; a failure
prints one stderr note and the wave continues.

The same emit releases the task claim taken in 3e. It fires on `--task` alone,
with `--node` falling back to the manifest the same way the emit envelope
does, so omitting `--node` does not suppress the settle. It runs
`fno backlog task update <node> <task> --status done`
after a `task_done` with outcome `SUCCESS` or `DONE_WITH_CONCERNS`, and
`--status pending` (the holder-only give-back) after `blocked` or a `FAILED`
outcome, so the next ready worker can pick the task up. Non-fatal like the
emit itself: the release prints one stderr note on failure and never fails the
task.

### 3c. Wave-End Scratchpad Note

After all tasks in a wave return SUCCESS, write a wave summary note to the
scratchpad (if one is configured). When any task ran via `frontend-executor`,
aggregate the per-task impeccable scores from each `task-${TASK_ID}-impeccable.md`
scratchpad note:

```yaml
# wave-N-summary.md (in scratchpad/execution/)
wave: N
tasks_completed: [1.1, 1.2]
agents_used: [archer, frontend-executor]
# Impeccable aggregation (only when frontend-executor ran)
per_task_scores: [38, 36]
iterations_total: 7
deferred_findings:
  - task_id: 1.4
    finding: "border-radius mismatch with design token"
```

Tasks that ran via archer are not included in `per_task_scores:` (omit the
field when no impeccable tasks ran). There is no gate artifact to write and
no `fno gate` call to make - the commits from completed tasks are the proof.

### 4. Auto-Persist State

After EVERY wave completion, write to .fno/STATE.md. The state file is the single source of truth for progress.

Load [state-formats.md](state-formats.md) for the STATE.md schema, task return contract, and scratchpad wave results format.

### 5a. Auto-Verify Before Done

Before reporting completion:

1. Spawn fno:verifier agent
2. Pass plan path and STATE.md
3. Wait for verification result
4. Only report "done" if verification PASSES

If verification FAILS, report issues and do not claim done.

### 5b. Fresh Verification (automatic)

After the existing verifier passes, dispatch a fresh worker to check
plan-vs-implementation alignment. Load
[verification-protocol.md](verification-protocol.md)
for the protocol.

This checks "did we build what was planned" rather than "did tests pass."

### 5c. Adversarial Challenge (`adversarial` only)

Only when `adversarial` modifier is present. Load
[adversarial-challenge.md](adversarial-challenge.md).
Challenges the implementation from three adversarial angles. If critical
findings: fix loop (max 3 iterations). Costs extra tokens - opt-in only.

### 6. Handle Failures

Load [error-recovery.md](error-recovery.md) for the full failure recovery protocol, including task attempt tracking, partial wave failure handling, and retry commands.

## NEVER (Silent Failure Prevention)

**NEVER silently drop agent failures:**
- If a archer returns FAILURE, it MUST appear in the wave results
- If a archer times out or crashes (no result returned), treat as FAILURE, not success
- If a archer returns SUCCESS but with warnings, report the warnings

**NEVER claim wave completion without checking ALL results:**
- Count results received vs tasks dispatched
- If count doesn't match, something failed silently - investigate
- Missing result = FAILURE until proven otherwise

**NEVER skip verification because "agents already tested":**
- Subagent tests run in isolated context - they may use different config
- The fno:verifier MUST run independently after all waves
- "Agent said tests pass" is not the same as "I ran tests and they pass"

**NEVER continue to next wave with unresolved failures:**
- A parallel wave with 2/3 successes is NOT "mostly done"
- Fix the failure or get explicit user approval to skip
- "Proceed anyway" is a user decision, not an agent decision

**NEVER trust STATE.md without cross-checking:**
- STATE.md could be stale from a crashed previous run
- On resume, verify that "completed" tasks actually have commits
- `git log --oneline` should show commits matching completed tasks

### When Verification Itself Fails

If the verification command fails (not "tests fail" but "command errors"), this is a meta-failure. Report it explicitly, do NOT claim completion, do NOT retry silently. The user needs to know.

## Context Management

**Target:** Keep main context under 40%

**Strategy:**
- Offload ALL substantial work to subagents
- Only orchestration logic in main context
- Subagents have fresh context per wave
- STATE.md is the handoff mechanism

## Resume

Load [resume-protocol.md](resume-protocol.md) for the full resume protocol.

## Deviation Handling

Load [deviation-handling.md](deviation-handling.md) for the full deviation handling protocol.

## Linear Integration (optional - requires linear plugin)

If the linear plugin is installed and the plan has a Linear ticket (check the plan's frontmatter for `linear: RR-XXX`), sync status at wave transitions. If no linear plugin or no ticket field, skip all Linear sync steps.

| Event | Linear Action |
|-------|---------------|
| Execution starts | Status -> "In Progress" |
| Wave completed | Sync progress to ticket description |
| All waves complete | Status remains "In Progress" (PR not created yet) |
| Failure/blocked | Add comment with error details |

## Key Principles

- **Orchestrate, don't execute** - Spawn subagents for actual work
- **State is truth** - STATE.md is the single source of progress
- **Fail fast** - Stop on first failure in sequential, report all in parallel
- **Verify before done** - Never claim completion without verification
- **Fresh contexts** - Each subagent gets fresh context
- **Resume-friendly** - Always be resumable from STATE.md

## Quick Reference

| Command | Behavior |
|---------|----------|
| `/execute waves` | Start fresh execution from wave 1 |
| `/execute waves resume` | Continue from STATE.md |
| `/execute waves retry <task>` | Re-run specific failed task |
| `/execute waves continue` | Skip failures, continue next wave |
| `/execute waves adversarial` | Enable adversarial challenge after verification |
| `/execute waves research` | Force research phase before execution |
