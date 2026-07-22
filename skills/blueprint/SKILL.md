---
name: blueprint
description: "Create implementation blueprints (plans) as a single .md doc. Given a /think design doc, mutate it in place; given an idea, create one. 'quick' scales the sections down for bugs and 1-session work; every plan is one .md == one PR == one node. Use when: 'create plan', 'implementation blueprint', 'break this down', 'how should we build'."
argument-hint: "[quick] [group N | no-group] [no-adopt] [no-collision-check] <design-doc-path | feature-description> [--no-linear]"
---

# Abilities Plan

When `$CODEX_THREAD_ID` is nonblank, before any routing or work, Print exactly once:
`codex posture: blueprint plans natively in this thread; auto-launch is Claude bg only, otherwise the node is visibly parked.`

<HARD-GATE>
NEVER edit ~/.fno/graph.json directly via Edit/Write tools or `jq -i`/`sed -i`.
ALWAYS use `fno backlog` commands or call `locked_mutate_graph()` from Python.
Direct edits are blocked by the PreToolUse hook AND detected via hash sidecar.
</HARD-GATE>

Create implementation plans scaled to the task. The output shape is always one plan `.md` (`plan == PR == node`); the input decides the path (mutate a `/think` doc in place, or create a fresh doc from an idea). Which gates fire is a READ of the input and the plan, not a guess - the dispatch table below names each trigger.

## Gates (read by state)

Each gate loads only when its trigger fires. The bodies (with verbatim scripts) live in [references/blueprint-gates.md](references/blueprint-gates.md); read a gate's section there when the trigger below is true. Do NOT run a gate whose trigger is false - a plan that fires no DB/executor/model/impeccable gate never mentions them.

| Gate | Read its section when |
|------|-----------------------|
| Plan Claims Ingestion | the argument is an existing node id (`x-8af8` / `ab-<hex>`) - runs FIRST, before any classifier |
| Failure Mode Ingestion | the argument resolves to a design-doc path - MANDATORY, refuses if `## Failure Modes` is missing |
| Schema Citation Gate | the codemap has a `## Database Schema` section AND the plan touches the DB |
| Executor Lock Transcription | a design doc supplies a Locked Decision (executor) |
| Model Pin / Model Routing | the plan frontmatter sets `model:` or `model_tier:` |
| Blueprint Provenance Stamp | always, after `$NODE_ID` is minted (tiny, best-effort) |
| PRODUCT.md Prereq Check | the plan locks `executor: impeccable` (plan-level or per-task) |
| impeccable_stages Pin Syntax | a task pins specific `/impeccable` stages |
| done_probes | the deliverable is recurring / operational (a scheduler, watcher, daemon, cadence) - MANDATORY then, omit otherwise |
| Collision check | always, unless `no-collision-check` (step 3a) |
| Cross-project peer heads-up | a `peers` block exists and a Files-to-Modify row matches a peer surface (step 3a-bis) |
| Epic decomposition (`group N`) | see [references/epic-decomposition.md](references/epic-decomposition.md) - `group`/`max_prs:`/`scope: epic` |

**Kill Criteria (MANDATORY, every plan - the one gate that is always inline).** Every plan `/blueprint` writes MUST declare `kill_criteria:` in its `.md` frontmatter (including quick plans; the markdown-heading form is invisible to the parser). Emit these defaults when the plan does not override:

```yaml
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
```

Full schema + predicate vocabulary: [references/blueprint-gates.md](references/blueprint-gates.md#kill-criteria-declaration-mandatory---full-detail).

## One output shape: a single `.md`

Every `/blueprint` invocation produces **one** plan `.md` - never a folder. The
input decides which path runs; the output shape is always the same (`plan == PR
== node`):

| Input | Path | What happens |
|-------|------|--------------|
| A `/think` design-doc path | **[Single-doc mutation](#single-doc-mutation-design-doc-input)** | Mutate the doc in place (append Execution Strategy + File Ownership + kill_criteria) |
| A raw idea / feature description | **[Single-doc creation](#single-doc-creation-idea-input)** | Write a fresh single `.md` with full frontmatter |

`quick` is a **size knob** on either path (fewer sections, single task), not a
separate mode - a quick plan still carries full frontmatter (`kill_criteria`,
`claims`, `executor`, `status`). Waves live in the doc's `## Execution Strategy`
block; there is no `00-INDEX.md` and no phase files.

---

## Single-doc creation (idea input)

When the argument is a raw idea / feature description (not a design-doc path),
write one flat plan `.md`. Includes lightweight BDD acceptance criteria per
change (1-2 Given/When/Then per change for happy path and primary error case)
and always carries full frontmatter (see the Kill Criteria block under [Gates](#gates-read-by-state)).

### Plan Save Location

Resolve the save path with `fno plan path --slug "<slug>" [--node "<node-id>"]` - it joins the plans dir (`.claude/settings.local.json` → `plansDirectory`, then `.claude/settings.json`, then `plans_dir` in `.fno/config.toml` / `~/.fno/config.toml`) with the `config.plans_filename` template (default `%Y%m%d-{slug}-{node}.md`). Do NOT hand-assemble the filename; the verb is the convention. If `fno` is unavailable, ask the user where to save and suggest running `/setup`.

### Session State Initialization

Initialize session state for cost tracking (replaces the PreToolUse hook for portability):
```bash
mkdir -p .fno
# Don't overwrite if target is running (it has its own state)
if [[ ! -f .fno/target-state.md ]]; then
  rm -f .fno/.session-registered
  TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  cat > .fno/session-state.md << STEOF
---
type: plan
status: IN_PROGRESS
created_at: ${TIMESTAMP}
---
STEOF
fi
```

### Process

1. **Understand** the request (ask if unclear). If the argument resolves to
   a design-doc file, run the **Failure Mode Ingestion** gate ([references/blueprint-gates.md](references/blueprint-gates.md#failure-mode-ingestion-mandatory-when-a-design-doc-is-supplied)) BEFORE
   anything else. A missing `## Failure Modes` section halts the skill with
   the verbatim refusal message; a present section becomes the seed list
   for AC4-EDGE citations inline in the Changes section.
2. **Structural context** — Generate a fresh codemap:
   ```bash
   REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
   # `fno codemap` writes to .fno/codemap.md by default and
   # auto-appends the DB-schema companion when --db-schema is set.
   # The `.env` clause widens detection to repos whose connection lives only
   # in a dev .env file (the case `db-schema.py` now discovers).
   db_env_found=""
   for ef in .env.local .env.development.local .env.development .env; do
     if [ -f "$REPO_ROOT/$ef" ] && grep -qE '^(export[[:space:]]+)?(DATABASE_URL|POSTGRES_URL|SUPABASE_DB_URL|DIRECT_URL)=' "$REPO_ROOT/$ef"; then
       db_env_found=1; break
     fi
   done
   # Reuse-or-regenerate: when /think already wrote a `## Schema Reconciliation`
   # section AND the codemap is fresh (<24h), schema was grounded at design
   # time - skip the --db-schema regeneration (mirrors the `## Discovery` skip
   # below). $DESIGN_DOC_PATH is the resolved design-doc argument, if any.
   schema_reused=""
   if [ -n "${DESIGN_DOC_PATH:-}" ] && grep -q '^## Schema Reconciliation$' "$DESIGN_DOC_PATH" 2>/dev/null \
      && [ -f "$REPO_ROOT/.fno/codemap.md" ] && [ -z "$(find "$REPO_ROOT/.fno/codemap.md" -mmin +1440 2>/dev/null)" ]; then
     schema_reused=1
   fi
   if [ -z "$schema_reused" ] && { [ -d "$REPO_ROOT/supabase" ] || [ -f "$REPO_ROOT/prisma/schema.prisma" ] || [ -f "$REPO_ROOT/drizzle/schema.ts" ] || [ -n "$DATABASE_URL" ] || [ -n "$db_env_found" ]; }; then
     fno codemap --tokens 2048 --db-schema 2>/dev/null || true
   elif [ -z "$schema_reused" ]; then
     fno codemap --tokens 2048 2>/dev/null || true
   fi
   ```
   If `fno` is unavailable or codemap's deps are missing, skip silently. Read `.fno/codemap.md` if it exists - use it to identify god nodes, module boundaries, and dependency flow before Grep/Glob exploration. Top files in the output are highest-importance; changes to these need extra phases.
2c. **Schema citation gate** - When a `## Database Schema` section exists in the
   codemap, run the **Schema Citation Gate** ([references/blueprint-gates.md](references/blueprint-gates.md#schema-citation-gate-graduated-db-touching-plans)) before adopt.
   Quick mode is `-S`-class, so it WARNS on an uncited DB-touching task and
   proceeds; it does not block.
2b. **Discovery gate** - After structural context but before writing the plan,
   surface unknowns. Load `references/discovery-gate.md` for the protocol.
   - With `quick`: 3 questions max (keep it lightweight)
   - Otherwise (default): up to 5 questions
   - **Skip if** /think already ran and produced a design doc with a
     `## Discovery` or `## Assumptions` section (questions were already answered)
   - Detection: check if the user's input references a design doc path, and if
     that doc has a `## Discovery` or `## Assumptions` section

3. **Write** the plan.

   - **A design doc was supplied** (mutate-in-place, the common path): keep its
     existing path unchanged - `mutate_doc.py` writes back to the same file
     (`os.replace` onto the resolved path), so an already-node-bearing name is
     preserved as-is and the `-<node-id>` suffix is never dropped or duplicated
     into `…-x-8af8-x-8af8.md` (US4). Do NOT rename a supplied doc.
   - **Creating fresh** (no design doc): write to the path printed by
     `fno plan path --slug "{slug}"`; when this is **node-seeded** (`$CLAIMS_ID` set,
     e.g. a direct `/blueprint x-8af8` with no prior `/think`), pass the node too:
     `fno plan path --slug "{slug}" --node "$CLAIMS_ID"`. `/blueprint` is the first
     artifact author on the direct path and cannot lean on `/think`'s save rule,
     so it must produce the node-bearing name itself. First **reuse if claimed**:
     if a plans-dir file already carries `$CLAIMS_ID` in its frontmatter or ends
     `-$CLAIMS_ID.md`, finalize into it instead of minting a second file. The
     raw-prose (no `$CLAIMS_ID`) case stays id-less here and is renamed at intake
     (step 3b-bis).

   If a design doc was supplied, run the **Executor Lock Transcription** gate
   ([references/blueprint-gates.md](references/blueprint-gates.md#executor-lock-transcription-when-a-design-doc-supplies-a-locked-decision)) before writing the plan body so the parser's output can be
   inlined into the plan's frontmatter as `executor: <value>`. Empty parser
   output leaves the frontmatter without an `executor:` field, falling through to
   runtime surface inference.

3a. **Collision check + peer heads-up** (conditional). Between writing the plan and auto-intake, run the collision check (skip with `no-collision-check`) and, when a `peers` block exists, the cross-project peer heads-up. Both are gate-shaped, skip-flagged steps - full procedure (the `fno backlog collisions check` read, high-severity AskUserQuestion / beastmode auto-decision, the four options, and the peer-surface match + send) is in [references/blueprint-gates.md](references/blueprint-gates.md#collision-check-step-3a-skip-with-no-collision-check).

3b. **Auto-intake to backlog** (skip if `no-adopt` modifier or `--no-adopt` flag)

   After writing the plan file, register it on the graph so it is visible
   to future `/target` invocations and to the kanban renderer:

   ```bash
   if command -v fno >/dev/null 2>&1; then
     fno backlog intake "$PLAN_PATH" --title "$TITLE" 2>&1 \
       || echo "Warning: auto-intake failed (plan file still saved)" >&2
   else
     echo "Warning: fno CLI not found on PATH; skipping auto-intake. Install the footnote plugin to enable." >&2
   fi
   ```

   If the plan file's frontmatter includes a `depends_on:` list (sibling
   plan slugs or `ab-` IDs), the intake handler resolves those to graph
   node IDs and wires up `blocked_by` edges automatically. Unresolvable
   references emit a warning and are skipped so intake never fails on
   a missing sibling.

   The user-facing `no-adopt` modifier (and `--no-adopt` flag) keep their
   names: that surface is a separate breaking change (see the rename plan's
   Out of Scope). Setting either skips this step entirely. The plan file is
   already durably written, so intake failures never block the handoff
   message.

   After `$NODE_ID` is minted, run the **Model Pin / Routing** and **Blueprint Provenance Stamp** gates ([references/blueprint-gates.md](references/blueprint-gates.md#model-pin-transcription-x-571f-when-a-plan-supplies-a-model)) when their triggers fire.

3b-bis. **Node-bearing filename for raw-prose intake** (US5)

   A node-seeded plan is authored with its id already in the name (step 3, and
   `/think`'s save rule). Only the **raw-prose** path - `/blueprint "some
   feature"` with no node - lands id-less, and auto-intake has just minted its
   node id (`$NODE_ID`, the `intake <id> -> backlog` line). Give the artifact
   its node-bearing name and repoint `plan_path`, so a roadmap base keyed on the
   node id finds it:

   ```bash
   "${SKILL_DIR}/scripts/rename-plan-to-node-id.sh" "$PLAN_PATH" "$NODE_ID"
   ```

   The helper is idempotent and non-fatal: a plan already ending `-$NODE_ID.md`
   (every node-seeded path) is a no-op, a pre-existing target is never
   clobbered, and any failure leaves the id-less file intact and re-runnable -
   it never blocks the handoff. If `$PLAN_PATH` still points at the old name in
   the same session, read the helper's `renamed <new-path>` line and use that
   path downstream.

4. **Present** plan and offer execution

### Template

Load [references/quick-template.md](references/quick-template.md) for the full template. The structure (frontmatter is MANDATORY - every plan carries it, quick or not):

```markdown
---
status: ready
kind: quick-plan
# claims: ab-XXXXXXXX      # only when the input was an ab-id
# executor: do             # transcribed from a Locked Decision, if any
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
---

# [Title]

## Context
[Problem, root cause, what we found]

## Changes

### 1. [Change name]
**Files:** `path/to/file.ts` (lines if known)
[What to do and why. Code snippets when helpful.]

### 2. [Change name]
**Files:** `path/to/other.ts`
[What to do and why.]

## Files to Modify
| File | Action |
|------|--------|

## Patterns to Reuse
| Pattern | Source |
|---------|--------|

## Verification
1. [Concrete runnable check]
```

**Required sections:** Context, Changes, Files to Modify, Verification.
**Optional sections:** Patterns to Reuse (omit if no relevant patterns exist).

### Writing Principles

Write for a fresh-context agent that knows nothing about this conversation:

| Bad (assumes context) | Good (self-contained) |
|----------------------|----------------------|
| "Update the API as discussed" | "Add `GET /api/users/:id` returning `{ id, name, email }`" |
| "Fix the bug from earlier" | "Fix: `calculateTotal()` returns NaN when cart is empty (returns 0 instead)" |
| "Use the approach we agreed on" | "Use server actions (not API routes) because Next.js 15 app router" |
| "Add validation to the API endpoints" | "Add Zod schema to POST /api/facilities in `src/routes/facilities.ts:78`: `{ name: z.string().min(1).max(100), capacity: z.number().int().min(1).max(500) }`. Pattern: `src/routes/users.ts:34`" |

**DB-aware planning** (when codemap.md has a Database Schema section):

| Bad (misses DB) | Good (DB-aware) |
|-----------------|-----------------|
| "Add recording_method: 'biometric' to the insert" | "Add 'biometric' to attendance_recording_method enum (migration), update signature_type_check constraint, THEN add to insert code" |
| "Store the public key as base64" | "Column is bytea - store as hex with \\x prefix, or change column to text (migration)" |

Each change must include:
1. **What** to change (specific files, functions, lines if known)
2. **Why** this approach (the actual reason, not "because we discussed it")
3. **How** to verify (runnable command, not "check that it works")
4. **Enough for synthesis** - The orchestrator will construct worker prompts from this plan. Every task must contain enough detail that the orchestrator can write a specific, actionable prompt WITHOUT re-reading the entire codebase. If you find yourself writing "update the relevant files" or "add appropriate validation," you have not done the research - go back and find the specific files, the specific validation rules, and the specific patterns to follow.

### Handoff

> "Plan saved to `{path}` and adopted to the backlog as `ab-xxxxxxxx`.
> Run `/do {path}` to execute, `/target {path}` for the full pipeline, or
> review first. Use `/blueprint quick no-adopt` next time to skip auto-adopt."

Include the adopted ID only when adopt succeeded. On failure, omit the
"and adopted..." clause and note the adopt warning in its place.

### Opting out of auto-adopt

Every plan calls `fno backlog intake` after being written so it appears on the
graph kanban. To skip that step:

```bash
/blueprint quick no-adopt "feature X"        # positional modifier
/blueprint quick "feature X" --no-adopt      # flag form
/blueprint no-adopt "feature X"              # default (non-quick), positional
```

Use this for throwaway specs, exploratory scratchpads, or when you want
to curate the graph manually.

### Opting out of the collision check

Every plan runs `fno backlog collisions check` between writing the plan and
auto-intake (step 3a). To skip when you know the collision is intentional:

```bash
/blueprint quick no-collision-check "feature X"        # positional modifier
/blueprint quick "feature X" --no-collision-check      # flag form
/blueprint no-collision-check "feature X"              # default (non-quick), positional
```

A skipped check still records `collisions_acknowledged: ["__skipped_check__"]`
on the new node so the audit trail distinguishes "I checked and accepted"
from "I never checked at all."

---

## Session Cost Tracking (AUTO — enforced by stop hook)

Cost is automatically registered by the stop hook when the session exits. The stop hook scans the transcript for `fno:plan` Skill tool invocations, calculates cost via `session-cost.py`, and appends to `ledger.json` via `register-task.py`. No manual action needed.

## Single-doc mutation (design-doc input)

When the input to `/blueprint` is a path to an existing design doc (produced by `/think`), the skill mutates that doc in place rather than creating a folder plan. The doc grows through /think -> /blueprint -> /do -> /review -> /ship; the single file is the canonical artifact.

### How it works

```
1. Read design doc + frontmatter
2. Validate: status must be "design" (or "ready" if `rewrite` passed)
3. Validate: required sections present (## Failure Modes mandatory)
4. Detect codebase state (skip if --mode greenfield|brownfield):
   - Read ## Architecture section, extract file path mentions
   - >= 50% exist -> brownfield; < 50% exist -> greenfield
5. Build ## Execution Strategy (waves YAML block)
6. Brownfield only: ## File Ownership Map, ## Patterns to Reuse
7. Update frontmatter: status -> ready, execution_mode, waves, kill_criteria
8. Write atomically (tempfile + os.replace in same directory)
9. Auto-intake to backlog via `fno backlog intake` (handled by skill body)
```

### Modifiers

| Modifier | Effect |
|---|---|
| `quick` | Skip ## Execution Strategy (single-task; stamp status + kill_criteria) |
| `group N` | Bounded epic decomposition: after intake, partition the waves into at most `N` cohesive delivery groups (one child node + PR each). See [references/epic-decomposition.md](references/epic-decomposition.md). Omit `N` to fall back to the epic's `max_children`, else `config.blueprint.max_prs_per_epic`. Auto-enabled for `scope: epic` docs. |
| `no-group` | Opt OUT of auto-decomposition on a `scope: epic` doc: run the single-doc lean mutation (one epic node, one PR), the pre-auto-group behavior. |
| `greenfield` | Skip File Ownership Map + Patterns to Reuse regardless of codebase state |
| `brownfield` | Force file binding even on empty-codebase detect |
| `rewrite` | Allow re-running on `status: ready` (replaces /blueprint sections only) |
| `verbose` | Inline content instead of cross-references |
| `no-adopt` | Skip auto-intake |
| `no-collision-check` | Skip collision check |

Modifiers are composable in any order: `/blueprint quick greenfield rewrite <doc-path>` works.

### Script invocation

The mutation is implemented in `skills/blueprint/scripts/mutate_doc.py`.
Arguments mirror the modifiers above:

```bash
python3 skills/blueprint/scripts/mutate_doc.py <doc-path> \
  [--mode greenfield|brownfield|auto] \
  [--rewrite] \
  [--no-emit]
```

`--no-emit` is a dry-run: prints the proposed doc to stdout without writing.

Exit codes:
- `0` success
- `1` doc already at status:ready without --rewrite; or path is a nonexistent file / feature description (redirect to /think)
- `2` required section missing (## Failure Modes) or section ownership violation
- `3` frontmatter status missing / invalid

### Section ownership

/blueprint ONLY writes sections in `BLUEPRINT_WRITE_ALLOWLIST`:
- `Execution Strategy`
- `File Ownership Map` (brownfield only)
- `Patterns to Reuse` (brownfield only)
- `kill_criteria` (frontmatter)

Any attempt to write outside this allowlist exits 2. /think-owned sections (Overview, Architecture, User Stories, Failure Modes, Acceptance Criteria, Locked Decisions, etc.) are never touched.

## Ready-gated auto-launch (opt-in, default OFF) — Phase 2 / US6

After a plan is written AND its claimed backlog node is intaked (the final step of both the single-doc creation and mutation paths), run the auto-launch gate as the LAST action:

```bash
bash "${SKILL_DIR}/scripts/autolaunch-on-ready.sh" "<plan-path>"
```

This is a **no-op unless** `config.target.auto_launch_on_blueprint: true` is set in config.toml (DEFAULT OFF; an absent key reads as off, so existing behavior is unchanged for anyone who has not opted in). When enabled, it dispatches the claimed node as a fresh unsupervised `claude --bg` `/target` worker (which keeps an agent-view row and an attachable pane — unsupervised, not headless) IFF the node is `status: ready` and not deferred — exactly the work that is "up-next." This auto-launch lane remains Claude-only: a non-Claude environment must report `parked` or `autolaunch-failed` and leave the node ready for an explicit supported dispatch, never pretend it started a native background worker. A `blocked`/`deferred` node, or one still in `idea`, is **parked** (pre-planned future work), never launched. The dispatched run defaults to `no-merge` (it lands a PR for review, not an auto-merge — Locked Decision 4). On dispatch failure the node stays `ready` and the blueprinted plan is intact for a manual `/target bg <node>` retry.

Relay the single decision line it prints (`auto-launched …` / `parked …` / `autolaunch-failed …`) to the user; it is never silent when the gate is ON. This keeps the planning session free to batch more `/think` + `/blueprint` while the dispatched worker runs (the fresh bg process is the only real context "clear"). The gate reuses the existing backlog state model — no new concept — so the developer's own discipline (marking future work `blocked_by`/`deferred`) IS the "only launch what's up-next" control.

## When to redirect to /think

If the argument to `/blueprint` does NOT look like a file path, redirect immediately:

```
No design doc found. Run `/think "<feature>"` first, then `/blueprint <resulting-doc-path>`.
Or invoke `/target` for the full chain.
```

A string is treated as a feature description (not a path) when it:
- Does not contain `/`
- Does not end in `.md`
- Does not start with `~`, `./`, `../`, or `/`

A path that looks like a path but does not exist on disk also triggers this redirect (exit 1) rather than falling through to raw-description mode. This is deliberate: a typo in a path gets a loud "file not found" rather than silently treating the argument as a description.

## Gotchas

Environment-specific traps that defy reasonable assumptions.

- **A node-id argument must render `claims:` into the plan frontmatter, or intake DUPLICATES the node.** `/blueprint x-8af8` claims that node only if the plan writes a literal `claims: x-8af8` line; the template's commented `# claims:` is a doc note, not a substitute. The post-write refusal (Plan Claims Ingestion gate) halts before adoption when it is missing.
- **A design-doc path with a typo must fail loud, never degrade to raw-description mode.** The path-shape classifier treats anything with `/`, `.md`, `~`, `./`, `../`, `/` as a path; a nonexistent one exits 1 with "file not found" rather than silently planning from the literal string.
- **`## Failure Modes` at level 2 exactly.** The gate greps `^## Failure Modes$` case-sensitively; a level-3 heading or in-prose mention does NOT satisfy it, and the gate refuses rather than auto-generating the section.
- **A malformed epic `max_children` (non-integer, `< 1`) is refused UP FRONT, before grouping** - not deferred to decompose, because a single-group collapse skips decompose entirely and would let the bad cap pass silently.
- **`done_probes` must end in a predicate and assert freshness.** `... | tail -5` masks the real exit status (reads as a pass); `test -f <file>` passes vacuously against launch-day residue. Bound every probe in time.
- **Plans save to the Obsidian vault, not git `docs/`.** Use `fno plan path --slug`; never hand-assemble the filename.

## References

- [references/blueprint-gates.md](references/blueprint-gates.md) - All state-keyed gates (claims, failure-mode, schema, executor, model, provenance, PRODUCT.md, impeccable_stages, done_probes, kill-criteria detail, collision, peer heads-up)
- [references/epic-decomposition.md](references/epic-decomposition.md) - `group N` bounded epic decomposition
- [references/discovery-gate.md](references/discovery-gate.md) - Discovery-gate question protocol
- [references/quick-template.md](references/quick-template.md) - Full plan template
- [references/single-doc-spec.md](references/single-doc-spec.md) - Single-doc mutation spec
- [references/section-headers.md](references/section-headers.md) - Canonical section headers
- [references/dependency-detection.md](references/dependency-detection.md) - depends_on resolution
- [references/kill-criteria-howto.md](references/kill-criteria-howto.md) - Kill-criteria authoring guide
- [references/linear-integration.md](references/linear-integration.md) - Linear sync
