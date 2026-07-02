---
name: blueprint
description: "Create implementation blueprints (plans). Default: multi-phase BDD folder plan with waves and parallel execution. Use 'quick' for flat single-file plans for bugs and 1-session work. Use when: 'create plan', 'implementation blueprint', 'break this down', 'how should we build'."
argument-hint: "[quick] [group N | no-group] [--plans fragment|separate] [no-adopt] [no-collision-check] <feature-description> [--no-linear] [--no-collision-check]"
---

# Abilities Plan

<HARD-GATE>
NEVER edit ~/.fno/graph.json directly via Edit/Write tools or `jq -i`/`sed -i`.
ALWAYS use `fno backlog` commands or call `locked_mutate_graph()` from Python.
Direct edits are blocked by the PreToolUse hook AND detected via hash sidecar.
</HARD-GATE>

Create implementation plans scaled to the task.

## Plan Claims Ingestion (MANDATORY when input is an ab-id)

Before any other classifier runs, check if the argument is an existing graph
node ID. The pattern `^ab-[0-9a-f]{8}$` is unambiguous and never collides
with file paths or raw descriptions, so this check is cheap and goes first.

When the argument matches, the rendered plan MUST declare `claims: ab-XXX`
in its frontmatter so `fno backlog intake` updates the existing idea-state
node in place rather than creating a duplicate (see
`cli/src/fno/graph/_intake.py::_resolve_claim`). Without the claim,
every adopted spec compounds the dangling-idea-node cleanup debt.

**Classify and resolve.** Run this BEFORE the failure-mode path classifier
below so an ab-id never falls through to "design-doc path":

```bash
ARG="$1"  # First positional arg after mode modifiers are stripped.
PARSER="${SKILL_DIR}/scripts/lib/parse-claims-arg.sh"
# eval'd output sets CLAIMS_ID and (when set) CLAIMS_SEED_ARG.
eval "$(bash "$PARSER" "$ARG")" || exit 1
if [[ -n "${CLAIMS_ID:-}" ]]; then
  ARG="$CLAIMS_SEED_ARG"
  # ARG now carries the seed text; CLAIMS_ID carries the claim target.
fi
```

After resolution, the plan body proceeds as if the user had pasted the
node's title plus details directly. The classifier below sees a raw
description (no slashes, no `.md`) and skips the failure-mode grep.

**Render `claims:` into the plan frontmatter.** When `CLAIMS_ID` is non-
empty, both quick mode and full mode MUST write a literal `claims: $CLAIMS_ID`
line at the top of the rendered frontmatter (above `created:`). The line is
load-bearing for the post-write refusal below; the templates' `claims:`
comment is a doc note, not a substitute.

For quick mode the line lands in the single `.md` file's frontmatter; for
full mode it lands in `00-INDEX.md`'s frontmatter. The plan body otherwise
follows its template as usual.

**Post-write refusal.** After the plan file (or folder) is written but
BEFORE the collision-check + auto-intake steps (3a / 11a), verify the
claim made it into the frontmatter. If not, halt:

```bash
if [[ -n "$CLAIMS_ID" ]]; then
  if [[ -f "$PLAN_PATH" ]]; then
    CLAIMS_FILE="$PLAN_PATH"
  else
    CLAIMS_FILE="$PLAN_PATH/00-INDEX.md"
  fi
  if ! grep -qE "^claims:[[:space:]]+$CLAIMS_ID\$" "$CLAIMS_FILE" 2>/dev/null; then
    echo "Error: input was an ab-id ($CLAIMS_ID) but the rendered plan does not declare 'claims: $CLAIMS_ID' in frontmatter. Refusing to adopt." >&2
    exit 1
  fi
fi
```

The refusal halts before adoption so a malformed claim never lands on the
graph. If you encounter the refusal in practice, hand-edit the frontmatter
to add `claims: $CLAIMS_ID` and rerun, or invoke `/blueprint` with a raw
description if the ab-id was passed by mistake.

**Pass-through to intake.** The auto-intake step (3b / 11b) does not need
to be told about the claim - `fno backlog intake` reads the frontmatter
directly. When you want to override the frontmatter at intake time (e.g.
repairing a past mistake), use `fno backlog intake plan/ --claims ab-XXX`.

## Failure Mode Ingestion (MANDATORY when a design doc is supplied)

`/think` is now contractually obligated to produce a `## Failure Modes`
section in every design doc (see `skills/think/SKILL.md` Step 6b). `/blueprint`
reflects the other side of that contract: when the input points to a design
doc, `/blueprint` MUST read the Failure Modes section before writing any plan
artifact and MUST refuse to proceed when the section is missing.

**Detection.** Classify the argument (after stripping mode modifiers) in
this order so a typo never bypasses the gate by silently degrading to
"raw feature description":

1. If the argument LOOKS like a file path - it contains `/`, or it ends
   in `.md`, or it starts with `~`, `./`, `../`, or `/` - treat it as a
   design-doc path. Resolve it (expand `~`, make absolute). If the file
   does not exist or is not readable, refuse with a missing-file message
   (see below). Do NOT fall through to raw-description mode.
2. Otherwise, treat the argument as a raw feature description and skip
   the grep check (there is nothing to read).

This is deliberately strict: a user who types `path/to/desgin.md` (typo)
gets a loud "file not found" rather than a silently-missed contract.

**Check.** For a path that resolved to a readable file, grep for a literal
level-2 heading `## Failure Modes`. Case-sensitive. A level-3 or deeper
heading does NOT satisfy the check; neither does an in-prose mention. In
the snippet below, `$DESIGN_DOC_PATH` is the resolved design-doc argument:

```bash
# $DESIGN_DOC_PATH is the absolute path resolved from the /blueprint argument.
# $ARG is the raw argument text (before resolution) used for the path-shape
# classifier below.
if [[ "$ARG" == *"/"* || "$ARG" == *.md || "$ARG" == ~* || "$ARG" == ./* || "$ARG" == ../* ]]; then
  if [[ ! -r "$DESIGN_DOC_PATH" ]]; then
    echo "Design doc at $DESIGN_DOC_PATH is missing or unreadable. Check the path and retry." >&2
    exit 1
  fi
  grep -q '^## Failure Modes$' "$DESIGN_DOC_PATH" || {
    echo "Design doc at $DESIGN_DOC_PATH is missing ## Failure Modes section. Run /think first." >&2
    exit 1
  }
fi
```

**Refusal message (verbatim template, where `{path}` stands for the
resolved design-doc path):**

```
Design doc at {path} is missing ## Failure Modes section. Run /think first.
```

Print the message to stderr, halt before any write, and surface the halt as
a non-zero status to the caller (target, operator, or the user shell). Do
NOT attempt to auto-generate the missing section: the whole point of the
gate is to force failure-mode thinking into `/think`, not to paper over a
skipped step here.

**Parse.** On success, extract the bullets under each of the four required
sub-sections (Boundaries, Errors, Invariants, Concurrency). The expected
structure is a bold label on its own line (`**Boundaries**`) followed by a
dash-bullet list; match the label exactly and collect the bullets until the
next bold label or the next `##` heading. Preserve the original bullet
wording so the AC4-EDGE seeds can cite the source by name.

**Seed AC4-EDGE criteria per mode:**

- **Full mode.** For each phase file, emit one `AC4-EDGE` acceptance
  criterion per failure-mode bullet that is relevant to that phase's
  surface area. Each seed MUST cite the source bullet by a short name taken
  from the design doc (e.g. `Cites "Double-submit" from design doc`).
- **Quick mode.** Emit seeds inline under the relevant `### N. [Change]`
  in the Changes section, preserving the same citation format. Do not
  enumerate failure modes that have no touchpoint in the plan.
Irrelevant bullets (no code surface changes this plan) are skipped rather
than padded: AC4-EDGE citations should map to actual implementation
touchpoints, not decorate the plan with unrelated concerns.

## Schema Citation Gate (graduated; DB-touching plans)

A plan that changes the database without ever naming a real table, enum, or
constraint is planning blind. This gate makes a DB-touching plan cite the
schema, the same way Failure Mode Ingestion makes a plan carry failure modes.
It is graduated: full / large plans fail closed, quick / small plans warn.

**When the gate fires (AC3-FR).** Only when the codemap has a
`## Database Schema` section (the schema is known). With no schema section
there is nothing to validate against, so the gate does NOT fire and planning
proceeds. Reuse the schema written at design time: if the design doc already
carries a `## Schema Reconciliation` section, the touched tables it names are
trusted candidate citations; otherwise read `.fno/codemap.md`'s
`## Database Schema` section directly.

**DB-touch detection (lowest-false-positive signal).** A plan is DB-touching
when the schema section exists AND the plan's File Ownership Map / Files-to-
Modify targets a DB path (`migrations/`, `supabase/`, `*.sql`, or a model /
schema file) OR a task body names a known table/enum from the schema section.
A heuristic keyword like the bare word "table" does NOT trip the gate.

**Satisfaction.** A DB-touching task is satisfied when it cites at least one
real identifier present in the schema section. An exact identifier match is
required; a schema-qualified form is accepted (a task naming `user_accounts`
matches a schema table `user_accounts`, and `public.user_accounts` is also
accepted), but a partial token like `account` does not match.

**Enforcement, graduated by size:**

- **Full / L plans -> fail closed.** Refuse, in the same style as the missing
  `## Failure Modes` refusal: print the message to stderr, halt before any
  write, and surface a non-zero status. The refusal MUST name the uncited task
  and list candidate identifiers from the schema section (AC3-ERR / AC3-UI):

  ```
  Plan task {task-id} touches the database but cites no real schema identifier.
  Cite one of: {comma-separated tables/enums/constraints from the schema section}.
  ```

- **Quick / -S plans -> warn, proceed.** Emit a single-line warning on stderr
  (`Warning: task {task-id} touches the DB without a schema citation`) and
  continue (AC3-EDGE). Small blast radius does not justify blocking, mirroring
  how the discovery gate relaxes for `-S`.

Do NOT auto-insert a citation to silence the gate: as with Failure Mode
Ingestion, the point is to force schema thinking into the plan, not to paper
over its absence.

## Executor Lock Transcription (when a design doc supplies a Locked Decision)

When `/think` runs against a frontend or mixed-surface design, it captures
the executor decision as a Locked Decisions entry (see
`skills/think/references/executor-routing-prompt.md`). `/blueprint` transcribes
that lock into the plan's frontmatter so the operator's three-tier resolver
honors it without a runtime surface-inference fallback.

Transcription is purely mechanical: same Locked Decisions input yields the
same frontmatter output. No LLM judgment in this step.

```bash
# The locked-decision parser is the in-package module fno.executor._locked
# (the SINGLE source of truth). In a checkout, point PYTHONPATH at cli/src so
# it imports pre-install; an installed `fno` needs no PYTHONPATH.
_PKG_SRC="$(cd "${CLAUDE_PLUGIN_ROOT:-$SKILL_DIR/../..}" 2>/dev/null && pwd)/cli/src"
[[ -f "${_PKG_SRC}/fno/executor/_locked.py" ]] && export PYTHONPATH="${_PKG_SRC}${PYTHONPATH:+:${PYTHONPATH}}"
LOCKED_VALUE=$(python3 -m fno.executor._locked < "$DESIGN_DOC_PATH")
# LOCKED_VALUE is one of: '' | do | impeccable | mixed
```

Then:

- **`do` or `impeccable`** - write `executor: <value>` to the plan
  frontmatter. For full mode, that is the top of `00-INDEX.md`. For quick
  mode, the top of the single `.md` file. Replace any existing `# executor:`
  comment from the template; never duplicate the key.
- **`mixed`** - write `executor: do` at plan level (the safe default), then
  emit `executor: impeccable` task blocks for any task whose file list
  matches the operator's locked surface-inference patterns
  (`**/*.tsx`, `**/*.jsx`, `components/**`, `routes/**`, `src/styles/**`).
  This mirrors the operator resolver and keeps cost honest: impeccable runs
  only where it earns its keep.
- **Empty** - write nothing. The runtime surface-inference fallback handles
  the plan correctly. No prompt; no warning.

When the parser fails (malformed YAML, unreadable doc, regex miss) it emits
empty stdout. /blueprint MUST NOT fabricate a lock; treat the empty result as
"no decision recorded" and let surface inference handle it. Log a single
warning line to stderr if the design doc has a Locked Decisions section but
the parser found no recognizable executor entry, so the user knows the lock
wasn't transcribed:

```bash
# Scope the orphan-mention check to the Locked Decisions section ONLY. A
# document-global grep would false-positive on design docs that discuss the
# operator's executor resolver in their Architecture section without ever
# locking it - those are correct empty-parser cases, not orphan mentions.
LOCKED_SECTION=$(awk '
    BEGIN { inside = 0 }
    /^##[[:space:]]/ {
        if (inside) { exit }
        if (tolower($0) ~ /^##[[:space:]]+locked[[:space:]]+decisions/) {
            inside = 1; next
        }
    }
    inside == 1 { print }
' "$DESIGN_DOC_PATH")

if [[ -n "$LOCKED_SECTION" && -z "$LOCKED_VALUE" ]] \
    && printf '%s' "$LOCKED_SECTION" | grep -qi 'executor'; then
    echo "warning: design doc mentions 'executor' inside Locked Decisions but the parser did not extract a canonical value (do|impeccable|mixed). Not transcribed; runtime surface inference will decide." >&2
fi
```

When `/blueprint` re-runs against a design doc whose Locked Decisions changed
since the previous invocation, re-transcribe: the parser is deterministic
and the frontmatter overwrite is idempotent. Stale executor fields are
the worst-case outcome; this guard prevents them.

## Model Pin Transcription (x-571f: when a plan supplies a model)

A plan can pin the model its dispatchers launch the node's worker on. Like the
executor lock, the choice is made once at planning time; `/blueprint`
transcribes it onto the graph node so every dispatcher honors it (US1-US3).
Unset = today's provider default, no behavior change.

Resolve the pin AFTER auto-intake has minted the node id (`$NODE_ID`), from two
sources in precedence order (frontmatter wins):

```bash
# 1. Plan frontmatter `model:` (primary). Scope to the frontmatter block only.
MODEL_PIN="$(awk '/^---[[:space:]]*$/{c++; next} c==1 && /^model:/{sub(/^model:[[:space:]]*/,""); print; exit}' "$PLAN_INDEX")"
# 2. Fallback: a `Model: <token>` Locked Decision (same parser module as the
#    executor lock, selected with --key model; empty when none).
if [[ -z "$MODEL_PIN" ]]; then
  MODEL_PIN="$(python3 -m fno.executor._locked --key model < "$DESIGN_DOC_PATH")"
fi
```

Then, only when non-empty, transcribe onto the node (idempotent; last writer
wins, so a re-run with a changed pin overwrites cleanly):

```bash
[[ -n "$MODEL_PIN" ]] && fno backlog update "$NODE_ID" --model "$MODEL_PIN"
```

The `fno backlog update --model` verb validates the value as a single
non-whitespace token; a malformed pin exits non-zero and leaves the node
unchanged (surface the error, do not fabricate a pin). When `/blueprint`
decomposes a scope:epic into child nodes, apply the same transcription to each
child id it creates. No pin present -> write nothing (no `--model` call).

## PRODUCT.md Prereq Check (when executor: impeccable is locked)

When `/blueprint` generates a plan that locks `executor: impeccable` at the plan
level OR via per-task overrides, it MUST check for a valid PRODUCT.md before
the auto-intake step. This is the spec-time half of the defense-in-depth
prereq strategy (decision 3a); the runtime half lives in the operator
dispatch gate (Phase 03).

Run the check script after writing the plan files but before collision check
and auto-intake:

```bash
SPEC_SCRIPTS="${SKILL_DIR}/scripts"
bash "$SPEC_SCRIPTS/check-product-md.sh" "$PLAN_PATH_OR_FOLDER"
```

The script:
1. Detects whether the plan uses `executor: impeccable` anywhere.
2. Searches for PRODUCT.md in order: `${REPO_ROOT}/PRODUCT.md`,
   `${REPO_ROOT}/.agents/context/PRODUCT.md`,
   `${REPO_ROOT}/docs/PRODUCT.md`.
3. Validates the found file is non-empty AND >= 200 chars (filters `[TODO]`
   stubs per /impeccable's loader contract).
4. If missing or stale:
   - Writes a `prerequisites:` block to the plan's `00-INDEX.md` frontmatter
     (quick mode: the single `.md` file's frontmatter):
     ```yaml
     prerequisites:
       - kind: file
         path: PRODUCT.md
         missing_reason: "required by /impeccable's setup gate; runtime will hard-block at dispatch"
     ```
   - Prints a warning to stderr (never blocks; plan ships regardless):
     ```
     warning: this plan locks executor: impeccable but no PRODUCT.md was found.
     Run /impeccable teach before /target dispatch, or /do waves will hard-block.
     ```

DESIGN.md gets softer treatment: if DESIGN.md is missing, add to
`prerequisites_optional:` (different key, NOT gated at dispatch). The
check script does not currently handle DESIGN.md - that is out of scope.

The script always exits 0. Plan creation is not blocked.

## impeccable_stages Pin Syntax

When a task needs specific `/impeccable` subcommands (beyond the agent's
default rule), `/blueprint` can write a per-task `impeccable_stages: [...]` YAML
field:

```yaml
### Task 2.1: Build Hero Component

executor: impeccable
impeccable_stages: [craft, critique, harden]
```

Known stages baseline (validated by `validate-plan.sh`):

```
craft, critique, polish, harden, audit, layout,
animate, bolder, colorize, delight, overdrive, quieter, typeset,
distill, extract, adapt, shape, teach
```

Pin-only treatments (animate, bolder, colorize, delight, overdrive, quieter,
typeset) are reachable ONLY via explicit `impeccable_stages` pins - the agent
never picks them autonomously (decision 4). Pinning them is exactly what this
field is for.

Rules for valid pins:
- List must be non-empty. `impeccable_stages: []` is an error (intent
  unclear; the validator rejects it).
- Every entry must be a known stage name. Unknown entries are errors.
- Pin-only treatments listed here are valid; they are not "wrong" for being
  pin-only.

The `validate-plan.sh` script enforces these rules at `/blueprint` validation time
(step 11 / full mode), so errors surface before auto-intake.

## Kill Criteria Declaration (MANDATORY)

Every plan `/blueprint` writes MUST declare `kill_criteria:` - abort conditions
that target/do evaluate at wave and iteration boundaries. When any
predicate holds, the engine emits `<aborted reason="{name}">`, the stop
hook exits clean (symmetric to `<promise>`), and the ledger records the
abort reason. This is how we prevent spinning-in-place burn loops.

**Default entries (emit these when the plan doesn't override):**

```yaml
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
```

**Placement.** Full mode: add the block to the top-level frontmatter of
`00-INDEX.md` (alongside `created:`, `depends_on:`, etc.). Quick mode: add
a `## Kill Criteria` heading with a fenced YAML block right after the
title. Keep the syntax identical across modes so tooling is consistent.

**Schema per entry:** `name` (string, identifier-style), `predicate`
(string, from the known vocabulary below), `reason` (string, shown to the
user at abort time).

**Known predicate vocabulary** (validated by
`scripts/validate-plan.sh`; unrecognized predicates produce a WARN and are
skipped at runtime, so new predicates degrade gracefully):

- `iteration > N` - iteration counter ceiling
- `same_test_failing_for >= N` - stuck-test detector (reads
  `verification.consecutive_failures`)
- `files_outside(plan_path) > N` - scope-creep guard
- `any_test_file_deleted` - detects deleted test files in the session

**Optional entries.** Plans are free to add or swap entries - for example,
a plan with a narrow file surface can enable `scope_creep`, or a plan with
well-understood recovery loops can lift `iteration_ceiling` to 30.
Malformed or missing fields are rejected by `validate-plan.sh` (errors),
and unknown predicates are warnings.

**Backward compatibility.** A plan without `kill_criteria:` behaves
identically to today's engine - the evaluator returns exit 0 when the
block is absent, and only the engine-wide defaults (if any) apply.

---

## Modes

| Mode | Subcommand | Output | Best For | Executor |
|------|------------|--------|----------|----------|
| **Full** (default) | _(none)_ | Folder with INDEX + phases | Multi-day features, parallel waves | `/do waves` or `/target` |
| **Focused** | `quick` | Single `.md` file | Bugs, quick features, 1-session work | `/do` |

---

## Focused Mode (`quick`)

Single flat plan file. No folder structure, no YAML. Includes lightweight BDD
acceptance criteria per change (1-2 Given/When/Then per change for happy path
and primary error case).

### Plan Save Location

Read plan path (first match wins):
1. `.claude/settings.local.json` → `"plansDirectory"`
2. `.claude/settings.json` → `"plansDirectory"`
3. `.fno/settings.yaml` → `config.plans.quick_path`
4. `~/.fno/settings.yaml` → `config.plans.quick_path`

**No default.** If none of these are set, ask the user where to save. Suggest running `/setup` to configure (if setup skill is installed).

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
   a design-doc file, run the **Failure Mode Ingestion** check above BEFORE
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
   codemap, run the **Schema Citation Gate** (top of this file) before adopt.
   Quick mode is `-S`-class, so it WARNS on an uncited DB-touching task and
   proceeds; it does not block.
2b. **Discovery gate** - After structural context but before writing the plan,
   surface unknowns. Load `references/discovery-gate.md` for the protocol.
   - In quick mode: 3 questions max (keep it lightweight)
   - In full mode: up to 5 questions
   - **Skip if** /think already ran and produced a design doc with a
     `## Discovery` or `## Assumptions` section (questions were already answered)
   - Detection: check if the user's input references a design doc path, and if
     that doc has a `## Discovery` or `## Assumptions` section

3. **Write** plan to `{quick_path}/YYYY-MM-DD-{slug}.md`. If a design doc
   was supplied, run the **Executor Lock Transcription** step (top of this
   file) before writing the plan body so the parser's output can be inlined
   into the plan's frontmatter as `executor: <value>`. Empty parser output
   leaves the frontmatter without an `executor:` field, falling through to
   runtime surface inference.

3a. **Collision check** (skip if `no-collision-check` modifier or `--no-collision-check` flag)

   After writing the plan but before auto-intake, scan pending plans on the
   graph for file overlap so two parallel specs do not silently target the
   same surface:

   ```bash
   fno backlog collisions check "$PLAN_PATH" --json > /tmp/collisions.json
   ```

   Read `/tmp/collisions.json`. If any entry has `severity: "high"`, present
   them via AskUserQuestion before adopting:

   > Your plan touches files also touched by these in-flight plans:
   >
   > - **{with_node_id}** ({with_node_title}): {len(shared_files)} shared files [recommended: {recommended_action}]
   >   *Rationale: {rationale}*
   >
   > Options:
   > 1. **Proceed anyway** - adopt this plan as a new node; both plans land separately and may conflict at merge time.
   > 2. **Modify {with_node_id} to absorb my changes** - print the existing plan path; cancel this new plan.
   > 3. **Supersede {with_node_id}** - adopt this new plan and mark the old one as superseded.
   > 4. **Cancel this new plan** - delete the file; do not adopt.

   Apply the user's choice:

   - **Option 1 (proceed):** continue to step 3b. Once the new node ID is
     known, run `fno backlog update <new-id> --acknowledge-collisions
     ab-XYZ,ab-ABC,...` so the new node carries an audit trail of the
     collisions deliberately ignored.
   - **Option 2 (modify):** print the existing plan path and exit cleanly.
     The user edits the older plan; the new file is left on disk for
     reference but not adopted.
   - **Option 3 (supersede):** continue to step 3b. After adoption, run
     `fno backlog supersede <new-id> --replaces ab-XYZ --reason "..."`.
     Use the colliding plan's rationale or ask the user for a reason.
   - **Option 4 (cancel):** delete the plan file and exit cleanly.

   Medium and low-severity collisions are reported as single-line warnings on
   stderr (`Warning: M shared files with ab-XYZ`) and never block auto-intake.

   The `no-collision-check` positional modifier (and `--no-collision-check`
   flag) skip this step entirely. Use case: "I know this collides; I'm doing
   it on purpose." When this modifier is used, after auto-intake run
   `fno backlog update <new-id> --acknowledge-collisions __skipped_check__`
   so the audit trail distinguishes "I checked and accepted" from "I never
   checked at all."

3a-bis. **Cross-project peer heads-up** (Files-to-Modify intersection)

   After the plan file is written and any collision check has resolved, but
   BEFORE auto-intake, scan the plan's Files-to-Modify table for paths that
   match a peer-owned surface. Mechanical resolution, not LLM judgment:

   ```python
   from fno.inbox.settings import read_peer_surfaces, read_surface_patterns
   peers = read_peer_surfaces()           # {peer: [surface_name, ...]}
   patterns = read_surface_patterns()     # {surface_name: [glob, ...]}
   ```

   For each peer, union the glob patterns of every surface that peer owns,
   then test the plan's Files-to-Modify rows against that union. The
   patterns intentionally use `**` for recursive matches (e.g. `src/api/**`),
   so use a matcher that supports recursive globs across the project's
   target Python (3.11+). `fnmatch.fnmatch` does NOT recurse on `**`, and
   `pathlib.PurePath.match` only treats `**` as recursive starting in 3.13.
   The portable choice is to translate the glob to a regex and match:

   ```python
   import re, fnmatch
   def match_recursive(file_path: str, pat: str) -> bool:
       # Translate `**` to `.*` (cross-segment) and `*` to `[^/]*` (single segment).
       regex = (
           re.escape(pat)
           .replace(r"\*\*", ".*")
           .replace(r"\*", "[^/]*")
           .replace(r"\?", ".")
       )
       return re.fullmatch(regex, file_path) is not None
   ```

   For each peer with at least one match, check the rc on send and update
   the dedup list:

   ```bash
   if [[ "<peer>" not in messaged_peers ]]; then
     if fno mail send --to-project <peer> --kind heads-up \
          --body "spec'd: <PLAN-TITLE>; touches surface <SURFACE-NAME>; ETA: <PLAN-TIMESTAMP>; plan: <PLAN-PATH>"; then
       append_peer_to_messaged_peers "<peer>"  # in plan frontmatter
     else
       # Send failed (typo, recipient missing, lock contention). Record
       # under messaged_peers_failed: so /target's ship recap retries
       # rather than treating it as already-sent.
       append_peer_to_messaged_peers_failed "<peer>" "<reason>"
     fi
   fi
   ```

   The dedup variable is named `messaged_peers` consistently across /think,
   /blueprint, and /target - it is the same plan-frontmatter list, just read at
   different phases.

   Skip the step silently when:

   - No `peers` block exists (opt-in by design)
   - No surface pattern matches any Files-to-Modify entry (the change is
     internal by definition)
   - The same peer was already notified at /think time (entry already in
     `messaged_peers:`)

   Do NOT block on responses - sends are fire-and-forget. If a single file
   could be claimed by surfaces owned by multiple peers and the canonical
   owner is ambiguous, emit `<help reason="cross-project-disambiguation"
   evidence="<file path>">` and skip those sends rather than multi-peer
   blasting.

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

4. **Present** plan and offer execution

### Template

Load [references/quick-template.md](references/quick-template.md) for the full template. The structure:

```markdown
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

Both modes call `roadmap-tasks.py intake` after writing the plan so it
appears on the graph kanban. To skip that step:

```bash
/blueprint quick no-adopt "feature X"        # positional modifier
/blueprint quick "feature X" --no-adopt      # flag form
/blueprint no-adopt "feature X"              # full mode, positional
```

Use this for throwaway specs, exploratory scratchpads, or when you want
to curate the graph manually.

### Opting out of the collision check

Both modes run `fno backlog collisions check` between writing the plan and
auto-intake (steps 3a / 11a). To skip when you know the collision is
intentional:

```bash
/blueprint quick no-collision-check "feature X"        # positional modifier
/blueprint quick "feature X" --no-collision-check      # flag form
/blueprint no-collision-check "feature X"              # full mode, positional
```

A skipped check still records `collisions_acknowledged: ["__skipped_check__"]`
on the new node so the audit trail distinguishes "I checked and accepted"
from "I never checked at all."

---

## Full Mode (Default)

Multi-phase folder plan for`/do waves` and `/target` execution.

### Plan Save Location

Same lookup as quick mode (see above). Uses `plansDirectory` from `.claude/settings.json` or `config.plans.full_path` from settings.yaml.

### Output Format

```
{full_path}/YYYY-MM-DD-feature-name/
├── 00-INDEX.md              # Overview, execution strategy (~200 lines)
├── 01-database.md           # Phase 1 tasks
├── 02-core-api.md           # Phase 2 tasks
├── 02b-webhooks.md          # Phase 2b (parallel with 02)
├── 03-ui-components.md      # Phase 3 (depends on 01, 02)
└── 04-tests-docs.md         # Phase 4 (depends on all)
```

### Reference Materials

| Reference | Load When |
|-----------|-----------|
| [references/index-template.md](references/index-template.md) | Creating INDEX.md |
| [references/phase-template.md](references/phase-template.md) | Writing phase files |
| [references/dependency-detection.md](references/dependency-detection.md) | Determining parallel vs sequential |
| [references/linear-integration.md](references/linear-integration.md) | Creating Linear tickets |

### Prerequisites

- Design document from `/think` OR clear feature description
- If no design doc exists, run `/think` first

### Process

1. **Load Design Context.** Read design doc or extract user stories from
   description. When a design-doc file is supplied, run the **Failure Mode
   Ingestion** check (top of this file) FIRST: refuse with the verbatim
   message when `## Failure Modes` is missing; otherwise parse its four
   sub-sections (Boundaries / Errors / Invariants / Concurrency) so phase
   files can emit AC4-EDGE citations per source bullet.
1b. **Structural Context (AUTO)** — Generate fresh codemap for module awareness:
   ```bash
   REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
   # The `.env` clause widens detection to repos whose connection lives only
   # in a dev .env file (the case `db-schema.py` now discovers).
   db_env_found=""
   for ef in .env.local .env.development.local .env.development .env; do
     if [ -f "$REPO_ROOT/$ef" ] && grep -qE '^(export[[:space:]]+)?(DATABASE_URL|POSTGRES_URL|SUPABASE_DB_URL|DIRECT_URL)=' "$REPO_ROOT/$ef"; then
       db_env_found=1; break
     fi
   done
   # Reuse-or-regenerate: a fresh (<24h) codemap plus a `## Schema Reconciliation`
   # section in the design doc means /think already grounded the schema - skip
   # regeneration (mirrors the `## Discovery` skip in step 1c).
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
   If `fno` is unavailable or codemap's deps are missing, skip silently. Read `.fno/codemap.md` if present. Use god nodes to identify phases needing extra care, module boundaries to inform phase splits, and dependency flow to determine wave ordering.
1c. **Discovery gate** - Surface unknowns before planning. Load
   `references/discovery-gate.md` for the protocol (up to 5 questions in full mode).
   Skip if /think already produced a `## Discovery` or `## Assumptions` section.
2. **Identify Phases** — Break into logical phases with dependency patterns
3. **Cross-Project Decomposition** — If phases touch >1 project from work config in settings.yaml, do NOT set `scope: cross-project` (removed). A plan is single-project from the executing session's view. Decompose into one backlog node per project, linked by `blocked_by`, each carrying its own `project`/`cwd` and its own `plan_path` (or a `#fragment` of this shared design doc per `references/section-headers.md`):
   - `fno backlog decompose <epic> --groups <json>` mints the per-group child nodes (`parent`/`blocked_by`/per-node `#fragment`). Give each group that belongs to a *different* repo a `project` (cwd is resolved from the settings work-map) or an explicit `cwd`; a group with neither inherits the epic's repo. An unmapped `project` is refused, so foreign work is never silently recorded under the current repo.
   - Each node then ships its own PR in its own repo; spawn-into-project carries the cross-repo handoff.
4. **Execution Strategy** — Load [dependency-detection.md](references/dependency-detection.md) for the algorithm
5. **Create INDEX.md** — Load [index-template.md](references/index-template.md). Keep ~200 lines.

5b. **Transcribe executor lock** (when a design doc was supplied). Run the
   **Executor Lock Transcription** step (top of this file) after writing the
   INDEX so the transcribed `executor:` frontmatter line lands without
   disturbing any other frontmatter field. For `mixed` results, walk each
   phase file and emit `executor: impeccable` task blocks on tasks whose
   file lists match the locked surface-inference patterns. Phase files
   without matching tasks inherit the plan default (`executor: do`).
6. **Goal Alignment** (if goals exist in settings.yaml) — Map tasks → goals
7. **Critical Path Trace** (MANDATORY) — Trace every user journey through the system
8. **Scope Classification** — `feature` | `scaffolding` | `poc` (a multi-repo feature is decomposed into per-project nodes in step 3, not classified `cross-project`)
9. **Create Phase Files** — Load [phase-template.md](references/phase-template.md) for task format
10. **Linear Ticket** — Only if `config.linear.enabled: true` in settings.yaml. Read team prefix from `config.linear.team`. If Linear is not configured, skip this step entirely.
11. **Validate** — Run `bash ${SKILL_DIR}/scripts/validate-plan.sh <plan-folder>`

11-schema. **Schema citation gate** — When the codemap has a `## Database
   Schema` section, run the **Schema Citation Gate** (top of this file) against
   the phase files' File Ownership Maps and task bodies. Full / large plans
   FAIL CLOSED: refuse with the named-task-plus-candidates message and halt
   before adopt if any DB-touching task cites no real schema identifier. With
   no schema section the gate does not fire (AC3-FR).

11a. **Collision check** (skip if `no-collision-check` modifier or `--no-collision-check` flag)

    Mirrors quick-mode step 3a, but checks the folder plan instead of a single
    file. The collision primitive walks `00-INDEX.md` plus every numbered phase
    file and unions their Files-to-Modify tables:

    ```bash
    fno backlog collisions check "$PLAN_FOLDER" --json > /tmp/collisions.json
    ```

    On any high-severity entry, run the same AskUserQuestion flow described
    in step 3a (proceed / modify / supersede / cancel). Apply the user's
    choice via `fno backlog update --acknowledge-collisions` or
    `fno backlog supersede` after adopt.

    `no-collision-check` (or `--no-collision-check`) skips the step;
    audit-trail handling matches quick mode.

11a-bis. **Cross-project peer heads-up** (Files-to-Modify intersection)

    Mirrors quick-mode step 3a-bis, but unions Files-to-Modify across
    `00-INDEX.md` and every numbered phase file before doing the surface
    intersection. Same readers, same anti-patterns, same fire-and-forget
    sends. After sending, append each notified peer to `messaged_peers:`
    in `00-INDEX.md`'s frontmatter so /target's ship recap dedups against
    folder-mode plans the same way it does quick-mode plans.

    Skip silently when no `peers` block is configured, when no surface
    pattern matches any Files-to-Modify row across the folder, or when the
    same peer is already in `messaged_peers:` (e.g. /think already sent
    for a Locked Decision affecting the same surface).

11b. **Auto-intake to backlog** (skip if `no-adopt` modifier or `--no-adopt` flag)

    Same invocation as quick mode, passing the plan folder path rather than a
    single file:

    ```bash
    if command -v fno >/dev/null 2>&1; then
      fno backlog intake "$PLAN_FOLDER" --title "$TITLE" 2>&1 \
        || echo "Warning: auto-intake failed (plan folder still saved)" >&2
    else
      echo "Warning: fno CLI not found on PATH; skipping auto-intake. Install the footnote plugin to enable." >&2
    fi
    ```

    Non-blocking: the plan folder is already on disk before this runs.

12. **Handoff** — Present execution options

### Required Skills (Full Mode Only)

- **`/bdd-acceptance-criteria`** — Use patterns from [references/common-criteria.md](references/common-criteria.md)
- **`/linear`** — Create Linear ticket (only if `config.linear.enabled`)

### Key Principles (Full Mode)

- INDEX.md stays small (~200 lines)
- Phase files have all implementation detail
- Dependency graph in INDEX (visual + table)
- Naming encodes parallelism (same prefix = parallel)
- Every phase gets frontmatter
- Every story gets 4 AC types (HP, ERR, UI, EDGE)
- Tests verify DATABASE outcomes, not just UI
- File ownership map MANDATORY in INDEX.md
- Bite-sized tasks (2-5 min each)

### Handoff (Full Mode)

> "Plan complete with N phases, adopted to the backlog as `ab-xxxxxxxx`.
> Linear ticket ID is in the plan's INDEX.md if created.
>
> **Execution options:**
> 1. `/target {plan-folder}/` — full pipeline
> 2.`/do waves {plan-folder}/` — wave orchestration only
> 3. `/target {plan-folder}/02-core-api.md` — single phase
>
> Parallel phases 02 and 02b can run simultaneously in separate terminals.
> Pass `no-adopt` on next invocation to skip auto-adopt."

Include the adopted ID only when adopt succeeded. On failure, omit that
clause and note the adopt warning.

## Session Cost Tracking (AUTO — enforced by stop hook)

Cost is automatically registered by the stop hook when the session exits. The stop hook scans the transcript for `fno:plan` Skill tool invocations, calculates cost via `session-cost.py`, and appends to `ledger.json` via `register-task.py`. No manual action needed.

## Single-doc mutation behavior (lean default)

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
| `group N` | Bounded epic decomposition: after intake, partition the waves into at most `N` cohesive delivery groups (one child node + PR each). See [Bounded Epic Decomposition](#bounded-epic-decomposition-group-n). Omit `N` to fall back to `config.blueprint.max_prs_per_epic`. Auto-enabled for `scope: epic` docs. |
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

## Bounded Epic Decomposition (`group N`)

Large epics (multi-wave design docs) ship better as a **bounded** number of
focused PRs than as one giant PR or 12 tiny ones. The `group N` modifier
partitions the epic's waves into at most `N` cohesive **delivery groups**,
each becoming one child backlog node and one PR. Waves stay the internal
execution unit; a group bundles 1+ waves. See
`internal/fno/plans/2026-05-24-epic-scoped-execution.md` (C1).

**When it runs.** Decomposition fires in either of two ways:

- **Auto (epic inputs).** A bare `/blueprint <doc>` whose frontmatter declares
  `scope: epic` (or whose `## Execution Strategy` has >1 wave) decomposes at the
  resolved ceiling by default - so an epic never silently collapses into one
  giant PR, and you never have to remember the `group` keyword. To opt OUT, pass
  **`no-group`** (`/blueprint no-group <epic-doc>`): that preserves the exact old
  single-PR behavior (the single-doc lean mutation) for an epic you have decided
  really is one cohesive PR.
- **Explicit (any doc).** The invocation carries the `group` keyword, OR the
  plan frontmatter declares `max_prs:`. Use this to force a split on a doc that
  is not flagged `scope: epic`.

A **non-epic** doc with no `group`/`max_prs:` keeps the single-doc lean mutation
unchanged - auto-group only changes the default for `scope: epic` inputs.

**Resolve the ceiling `N`** (first match wins):

1. Explicit `group N` (e.g. `/blueprint group 5 <doc>`) -> `N`.
2. `group`/auto-group with per-plan `max_prs:` in frontmatter -> that value.
3. `group`/auto-group with neither -> `config.blueprint.max_prs_per_epic`
   (default 4). Read it with:
   ```bash
   N=$(fno config get config.blueprint.max_prs_per_epic 2>/dev/null || echo 4)
   ```
   If `fno config get` is unavailable, default to 4. **Auto-group MUST degrade to
   today's single-doc behavior (not error) if the config read fails** - treat an
   unreadable ceiling as 4, never abort the blueprint.

`N` is a **ceiling, not a quota** (Locked Decision #3): cohesive work uses
fewer groups; never pad to `N`. This guardrail applies identically to
auto-group - an auto-decompose must never produce more than `N` groups, and a
`scope: epic` doc whose waves cohere into one group still ships ONE PR (record
it in `## Delivery Groups`, never force a split). Reject `group 0` / negative
`N` with a non-zero exit and the message `group N must be >= 1` (AC1-ERR),
creating nothing.

**Procedure** (after the epic node is intaken in step 9 / 11b, so `EPIC_ID`
is known):

1. Read the `## Execution Strategy` waves from the doc.
2. **Group by cohesion, surface, and dependency** (Locked Decision #8 - LLM
   judgment, not a blind contiguous split). Keep a cohesive change (e.g. one
   frontend surface) inside a single group. Order groups so a later group
   `blocked_by` an earlier one only when there is a real dependency.
3. If the grouping collapses to a single group, **skip decompose** - the epic
   node is the one PR. Still record this in `## Delivery Groups`
   (AC1-EDGE: never force a split).
4. Write a `## Delivery Groups` section to the doc (this section is owned by
   the decomposition step, NOT by `mutate_doc.py`; it is preserved across
   `rewrite` re-runs). Format:
   ```markdown
   ## Delivery Groups

   Ceiling: N (source: group-arg | frontmatter max_prs | config default)

   | Group | Waves | PR scope | Depends on |
   |-------|-------|----------|------------|
   | 1 | 1-3 | foundation + schema | - |
   | 2 | 4-5 | API surface | 1 |
   | 3 | 6 | UI | 2 |
   ```
5. **Classify each cross-repo dependency `hard` or `contract`** (only for a
   group that `blocked_by` a group in a *different* repo; same-repo edges are
   always `hard`). Default is `hard` (the blocker must land before the dependent
   builds). Propose `contract` (the dependent builds **now** against a
   pinned interface, stubbing the unlanded parts) ONLY when **both** gates hold,
   else keep `hard`:
   - **Pin gate:** the design doc has a `## Interface Contract` section with a
     `contract_version` (a G1 `/think` output). No pin ⇒ `hard`. The CLI
     re-checks this and downgrades a `contract` request to `hard` **loudly** (a
     warning on stderr and in the JSON `downgrades`) if the doc pins nothing, so
     an honest mistake never ships a mocked PR, but propose `contract` only when
     the pin is really there.
   - **Independent-work gate:** the dependent has ≥ 1 wave of work that does NOT
     need the blocker landed (real parallelism to win). A dependent that only
     stubs ⇒ `hard`.

   `contract` is **model-proposed, human-confirmed** (Locked Decision 6): show
   the author which edge you propose to mark `contract` and why, and proceed only
   on confirmation. To mark a group, add `"dep": "contract"` to it; the CLI
   stamps `contract_version` (read from the doc) and `stub_against` on the child.

6. Build the groups JSON and call the CLI (atomic + idempotent upsert). A
   `contract` group just carries `"dep": "contract"` (it must already
   `blocked_by` its blocker); everything else is unchanged:
   ```bash
   cat > /tmp/groups-$$.json <<'JSON'
   [
     {"slug": "1", "title": "Group 1: backend API", "waves": "1-3", "blocked_by_groups": []},
     {"slug": "2", "title": "Group 2: frontend", "waves": "4-6", "blocked_by_groups": ["1"], "dep": "contract"}
   ]
   JSON
   fno backlog decompose "$EPIC_ID" --max-prs "$N" --groups "@/tmp/groups-$$.json"
   rm -f /tmp/groups-$$.json
   ```
   The verb creates one child node per group (`parent=$EPIC_ID`,
   `plan_path=<doc>#group-<slug>`, `blocked_by` resolved from
   `blocked_by_groups`), prints the epic id and each child id with its wave
   range, and is idempotent: re-running `/blueprint group N` on an
   already-decomposed plan updates the same children in place (keyed on
   `#group-<slug>`) rather than duplicating (US4). A bad spec leaves the
   graph untouched (AC1-FR) because the whole decomposition lands in one
   locked mutation. If a re-decomposition drops a `#group-` slug that already
   shipped a PR, the verb refuses (exit 2) unless you pass `--force`; unshipped
   dropped groups are left in place and reported as a warning.

**Slug stability.** Use stable slugs across re-decomposition so idempotency
holds. Numeric (`1`, `2`, ...) is the simple default; named slugs
(`auth-flow`) are fine as long as they do not change between runs.

**Packaging choice: `fragment` (default) vs `separate`.** Decompose can attach
two kinds of per-child plan; pass `--plans` to pick (the skill surfaces the
tradeoff so the operator chooses deliberately instead of hand-rolling it):

- **`fragment` (default, lean):** `plan_path = <doc>#group-<slug>`, a fragment of
  the shared epic doc. One source of truth, thinner per-child detail. Best when
  the children are built in the same context that holds the epic doc.
- **`separate`:** after decompose, scaffold a self-contained quick-plan stub per
  child (Context / Changes / Files to Modify / Verification, seeded from the
  group's waves + a pointer to the epic's File Ownership Map) and repoint each
  child's `plan_path` to its own `<stem>.group-<slug>.md` file. Best for a
  fresh-context bg `/target` builder, which reads a self-contained plan better
  than a fragment of a larger doc.

  ```bash
  fno backlog decompose "$EPIC_ID" --max-prs "$N" --plans separate --groups "@/tmp/groups-$$.json"
  ```

  `separate` is idempotent on the slug just like `fragment` (re-running upserts
  the same children, and an existing scaffolded file is never clobbered, so a
  builder's edits survive a re-decompose). Switching modes on an
  already-decomposed epic repoints the same children rather than duplicating.
  When in doubt, default to `fragment`; reach for `separate` when you are about
  to hand the children to fresh-context builders.

## Ready-gated auto-launch (opt-in, default OFF) — Phase 2 / US6

After a plan is written AND its claimed backlog node is intaked (the final step of every mode — Focused, Full, and single-doc lean), run the auto-launch gate as the LAST action:

```bash
bash "${SKILL_DIR}/scripts/autolaunch-on-ready.sh" "<plan-path>"
```

This is a **no-op unless** `config.target.auto_launch_on_blueprint: true` is set in settings.yaml (DEFAULT OFF; an absent key reads as off, so existing behavior is unchanged for anyone who has not opted in). When enabled, it fire-and-forget dispatches the claimed node as a fresh `claude --bg` `/target` worker IFF the node is `_status: ready` and not deferred — exactly the work that is "up-next." A `blocked`/`deferred` node, or one still in `idea`, is **parked** (pre-planned future work), never launched. The dispatched run defaults to `no-merge` (it lands a PR for review, not an auto-merge — Locked Decision 4). On dispatch failure the node stays `ready` and the blueprinted plan is intact for a manual `/target bg <node>` retry.

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
