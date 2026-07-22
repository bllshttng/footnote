# Blueprint gates (state-keyed)

Read a gate only when its trigger fires. The dispatch table in SKILL.md ("## Gates (read by state)") names each trigger; this file holds the bodies. Every gate is mechanical - same input yields the same output, no LLM judgment - unless noted.

## Plan Claims Ingestion (MANDATORY when input is a node id)

Before any other classifier runs, check if the argument is an existing graph
node ID. `parse-claims-arg.sh` recognizes the config-agnostic node-id shape
`^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$` - the configured `id_prefix`/`id_hex_width`
(e.g. `x-8af8`) and the legacy `ab-<8hex>` alike, never a hard-coded `ab-` only.
A node id never collides with file paths or raw descriptions, so this check is
cheap and goes first.

When the argument matches, the rendered plan MUST declare `claims: <node-id>`
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
empty, the rendered plan MUST write a literal `claims: $CLAIMS_ID` line at the
top of the frontmatter (above `created:`) - the single `.md` is the only plan
shape. The line is load-bearing for the post-write refusal below; the
template's `claims:` comment is a doc note, not a substitute.

**Post-write refusal.** After the plan file is written but BEFORE the
collision-check + auto-intake steps (3a / 3b), verify the claim made it into
the frontmatter. If not, halt:

```bash
if [[ -n "$CLAIMS_ID" ]]; then
  if ! grep -qE "^claims:[[:space:]]+$CLAIMS_ID\$" "$PLAN_PATH" 2>/dev/null; then
    echo "Error: input was an ab-id ($CLAIMS_ID) but the rendered plan does not declare 'claims: $CLAIMS_ID' in frontmatter. Refusing to adopt." >&2
    exit 1
  fi
fi
```

The refusal halts before adoption so a malformed claim never lands on the
graph. If you encounter the refusal in practice, hand-edit the frontmatter
to add `claims: $CLAIMS_ID` and rerun, or invoke `/blueprint` with a raw
description if the ab-id was passed by mistake.

**Pass-through to intake.** The auto-intake step (3b) does not need to be told
about the claim - `fno backlog intake` reads the frontmatter directly. When you
want to override the frontmatter at intake time (e.g. repairing a past
mistake), use `fno backlog intake <plan>.md --claims ab-XXX`.

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

**Seed AC4-EDGE criteria.** Emit seeds inline under the relevant
`### N. [Change]` in the Changes section (or against the design doc's Execution
Strategy waves on the mutation path), one `AC4-EDGE` per failure-mode bullet
with a code touchpoint. Each seed MUST cite the source bullet by a short name
taken from the design doc (e.g. `Cites "Double-submit" from design doc`).
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

- **`do` or `impeccable`** - write `executor: <value>` to the plan `.md`
  frontmatter (the single doc is the only plan shape). Replace any existing
  `# executor:` comment from the template; never duplicate the key.
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

### Model Routing (tier assignment, parallel to executor routing)

Beyond an exact pin, a plan may express a **minimum quality tier** and let
dispatch pick the cheapest reachable model that clears it (pareto routing). This
mirrors executor routing: judged once at planning time, honored at every
dispatch, and unset = today's provider default (no behavior change).

Assign a tier per the task's nature, one line of rationale each:

- **`low`** - mechanical work: a rename, a codemod, a doc tweak, boilerplate.
- **`medium`** - a standard feature or fix that needs real reasoning but no
  load-bearing judgment.
- **`high`** - gate semantics, security, concurrency, migrations, or an
  architecture decision where a weaker model's error is expensive.

Precedence is `model:` (exact) over `model_tier:` (tier) over the provider
default; an exact pin on the same task wins. Transcribe a plan-wide default from
frontmatter onto the node (AFTER `$NODE_ID` is minted), idempotently:

```bash
# Plan frontmatter `model_tier:` (scope to the frontmatter block).
MODEL_TIER="$(awk '/^---[[:space:]]*$/{c++; next} c==1 && /^model_tier:/{sub(/^model_tier:[[:space:]]*/,""); print; exit}' "$PLAN_INDEX")"
[[ -n "$MODEL_TIER" ]] && fno backlog update "$NODE_ID" --model-tier "$MODEL_TIER"
```

`fno backlog update --model-tier` validates the band (`high|medium|low`); an
invalid value exits non-zero and leaves the node unchanged (surface it, do not
fabricate a tier). Per-task tiers ride in task blocks as `model_tier:` lines
(the do-phase reads them the same way it reads `executor:` task blocks). No tier
and no pin present -> write nothing.

## Blueprint Provenance Stamp (x-b6e4)

After auto-intake mints `$NODE_ID` and the plan's Execution Strategy is
finalized, stamp the lifecycle provenance so the graph records which
session/harness planned the node (idempotent, append-only, best-effort):

```bash
[[ -n "$NODE_ID" ]] && { fno backlog session add "$NODE_ID" --phase blueprint \
  || echo "blueprint: session add failed for $NODE_ID (non-fatal, provenance not stamped)" >&2; }
```

Harness + session id default from the ambient identity. A missing-identity
warning is non-fatal; the stamp never blocks intake. When `/blueprint`
decomposes a scope:epic into child nodes, this stamps the parent node only (the
session planned the epic; child do-entries stamp themselves at execution).

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
bash "$SPEC_SCRIPTS/check-product-md.sh" "$PLAN_PATH"
```

The script:
1. Detects whether the plan uses `executor: impeccable` anywhere.
2. Searches for PRODUCT.md in order: `${REPO_ROOT}/PRODUCT.md`,
   `${REPO_ROOT}/.agents/context/PRODUCT.md`,
   `${REPO_ROOT}/docs/PRODUCT.md`.
3. Validates the found file is non-empty AND >= 200 chars (filters `[TODO]`
   stubs per /impeccable's loader contract).
4. If missing or stale:
   - Writes a `prerequisites:` block to the plan `.md`'s frontmatter:
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

The `validate-plan.sh` script enforces these rules at `/blueprint` validation
time (the validate step), so errors surface before auto-intake.

## Kill Criteria Declaration (MANDATORY - full detail)

The default block and placement rule are inline in SKILL.md; this is the full schema and vocabulary.

Every plan `/blueprint` writes MUST declare `kill_criteria:` - abort conditions
that target/do evaluate at wave and iteration boundaries. When any
predicate holds, the engine emits `<aborted reason="{name}">`, the stop
hook exits clean (symmetric to `<promise>`), and the ledger records the
abort reason. This is how we prevent spinning-in-place burn loops.

**Placement.** ALWAYS in the plan `.md`'s frontmatter (alongside `created:`,
`claims:`, `executor:`, etc.) - including quick plans. The markdown-heading form
(`## Kill Criteria`) is invisible to the stamp/validate parser, so it is not
used; frontmatter is the single source of truth across every plan.

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

## done_probes: operational evidence (MANDATORY for recurring deliverables)

A plan whose deliverable is **recurring or operational** - a scheduler, a
watcher, a daemon, a cadence, a LaunchAgent, anything whose value is that it
keeps running - MUST declare 1-2 `done_probes` in its frontmatter.
Everything else omits the field entirely.

`fno-agents loop-check` runs each probe as the final `DonePRGreen` conjunct and
refuses done until every one exits 0.
The gate otherwise measures artifacts (PR + CI + review), which operational
silence cannot falsify: grooming shipped three times without ever running,
because the last mile (installing the agent, firing the first run) lives
outside the repo where the worker's authority ends.
A probe is what forces that last mile before the session can claim done.

```yaml
done_probes:
  - "fno mail list --kind report --since 24h | grep -q groom"
```

**Assert freshness, never bare existence.**
A probe like `test -f ~/.fno/groom-report.json` passes vacuously against
launch-day dry-run residue, which is exactly the failure this field exists to
catch.
Bound every probe in time (`--since 24h`, an mtime check, a date-stamped
lookup) so it can only pass if the thing ran recently.

**End in a predicate, not a pipeline tail.**
`... | grep -q x` exits on the grep; `... | tail -5` exits on the tail and
masks the real status, so a broken command reads as a pass.

**Use the block form above, or a single-line inline list** (`done_probes: ["cmd"]`).
A declaration in any other shape (a multi-line inline list, say) refuses done as
"probes undeterminable" rather than passing as if you had declared nothing -
declaring a gate the loop cannot read must never read as no gate.

**Constraints.** At most 3 probes (a gate, not a test suite); each gets a 60s
native timeout and its whole process group is killed past it; a missing binary
(127), a non-zero exit, and a timeout all fail closed with the command and code
named in the block reason.
Probes must be read-only and idempotent - two near-simultaneous fires may run
them twice.
There is no env override: a probe that cannot pass in this environment is
resolved by editing the plan, which is visible in git.

## Collision check (step 3a; skip with `no-collision-check`)

After writing the plan but before auto-intake, scan pending plans on the
graph for file overlap so two parallel specs do not silently target the
same surface:

```bash
fno backlog collisions check "$PLAN_PATH" --json > /tmp/collisions.json
```

Read `/tmp/collisions.json`: `{"status": "ok"|"unevaluated", "collisions": [...]}`.
A `status` of `unevaluated` means the plan states no file surface, so nothing
was compared - that is NOT a clean result. Fill in the plan's
`## File Ownership Map` (or `## Files to Modify`) table and re-run before
adopting.

If any entry in `collisions` has `severity: "high"`, present
them via AskUserQuestion before adopting - unless `fno target status` shows
`authority: full` on the `attended` line (a live `/target beastmode` session), in
which case take the `recommended_action` for each entry, append one
`## Autonomous Decisions` entry naming the collision and the action, and
continue without prompting:

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

## Cross-project peer heads-up (step 3a-bis; Files-to-Modify intersection)

After the plan file is written and any collision check has resolved, but
BEFORE auto-intake, scan the plan's Files-to-Modify table for paths that
match a peer-owned surface. Mechanical resolution, not LLM judgment. Skip the
whole step silently when no `peers` block exists (opt-in by design).

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
