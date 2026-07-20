
# Abilities Think

Enhanced brainstorming workflow that generates testable acceptance criteria alongside design. Includes multi-perspective analysis and mandatory failure-mode coverage to catch silent failures and UI state bugs before implementation.

### Session State Initialization

Initialize session state for cost tracking (replaces the PreToolUse hook for portability):
```bash
mkdir -p .fno
# Only a LIVE target session owns the manifest and should suppress our write.
# A DEAD manifest must NOT (x-4af4: a stale target-state.md once auto-locked
# attended /think for ~10 days). Liveness = the ONE predicate in `fno target
# status` (claim-first); read its machine `manifest-live` field, never file
# existence. Degrade conservatively: if the verb can't answer, assume live.
target_live=0
if [[ -f .fno/target-state.md ]]; then
  ml="$(fno target status --json 2>/dev/null | grep -o '"manifest-live":[^,}]*' || true)"
  if [[ -z "$ml" || "$ml" == *'"live'* ]]; then target_live=1; fi
fi
if [[ "$target_live" != 1 ]]; then
  rm -f .fno/.session-registered
  TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  cat > .fno/session-state.md << STEOF
---
type: think
status: IN_PROGRESS
created_at: ${TIMESTAMP}
---
STEOF
fi
```

<HARD-GATE>
Do NOT invoke any implementation skill, write any code, scaffold any project, or take any implementation action until you have presented a design and the user has approved it. This applies to EVERY project regardless of perceived simplicity.
</HARD-GATE>

## Process

### 1. Understand Context
- Check current project state (files, docs, recent commits)
- Review any existing specs (read plan path: `.claude/settings.local.json` → `plansDirectory`, then `.claude/settings.json` → `plansDirectory`, then `.fno/config.toml` → `plans_dir`)

### 1b. Scope Decomposition Check
Before diving into design, assess scope: if the request describes multiple independent subsystems (e.g., "build a platform with chat, file storage, billing, and analytics"), flag this immediately. Don't spend questions refining details of a project that needs to be decomposed first.

**Epic exemption (do this check first).** Do NOT redirect an **epic** here.
An epic is a node with graph `type: epic`, or a free-text seed carrying epic signals (see Step 1f: "epic", "roadmap", "mission", "consolidate X across Y", an enumeration of multiple independent subsystems).
The epic contract subsumes this check: its `## Gaps / Candidate Children` and `## Decomposition Guidance` sections ARE the decomposition product, produced by the epic design flow rather than by splitting first.
Proceed into the normal flow and let Step 1f resolve `deliverable_type: epic`.
This redirect still fires for every non-epic multi-subsystem request.
Because Step 1b physically precedes Step 1f, resolve the epic question here (graph type, or a quick seed check) before acting on the redirect, so an epic is never split away before it reaches its own contract.

If the project is too large for a single spec, help the user decompose into sub-projects: what are the independent pieces, how do they relate, what order should they be built? Then think through the first sub-project through the normal design flow. Each sub-project gets its own think → plan → do cycle.

### 1c. Discovery Gate (optional)

After reading the project state, surface what the MODEL doesn't know before
asking the user design questions. This catches architectural ambiguities and
scope misunderstandings early.

Load the discovery protocol: `${SKILL_DIR}/references/discovery-gate.md`

- Run in **interactive** mode (present questions, wait for answers)
- 3-5 targeted questions grounded in what you just read from the codebase
- Feed answers into step 2's design exploration

**Skip if:** Pure greenfield with no existing codebase to read (step 2's
existing questions are sufficient for new projects).

The discovery gate asks what the MODEL is uncertain about from the code.
Step 2 below asks what the USER wants from the design. Both are needed.

### 1d. Cross-Project Peer Awareness (fires twice)

If `~/.fno/config.toml` declares a `config.inbox.peers` map, a
single peer-detection check runs at two distinct moments in this flow.
Both moments share the same anti-patterns and disambiguation rule
(below). The check is opt-in: with no `peers` block, no `surfaces` map,
or no surface match, it is a silent no-op.

Resolution mechanic (read once, applies to both moments):

```python
from fno.inbox.settings import read_peer_surfaces
peers = read_peer_surfaces()  # {peer: [surface, ...]} or {}
```

**Sub-condition A — fires NOW, after Step 1c discovery has enumerated
the unknowns and before Step 2 starts answering them.**

For each unknown surfaced in this discovery cycle, ask: does the unknown
name a *specific peer-owned surface* (per `peers[<peer>].surfaces`)? If
yes, send a question to that peer AND append to `messaged_peers:`:

```bash
if fno mail send --to-project <peer> --kind question \
     --body "design Q: <CONCRETE — verbatim unknown text or one-line restatement>"; then
  # Both sub-conditions A and B share the SAME messaged_peers: substrate.
  # /blueprint's 3a-bis check and /target's ship recap dedup against this list,
  # so a question to peer X for surface Y MUST register here or /blueprint
  # will send a redundant heads-up about the same surface.
  append_peer_to_messaged_peers "<peer>"  # in saved design doc's frontmatter
else
  # Send failed (typo'd peer, recipient inbox missing, lock contention).
  # Record under messaged_peers_failed: so a later recap retry treats it
  # as "needs send" rather than "already sent". Do NOT block; continue.
  append_peer_to_messaged_peers_failed "<peer>" "<reason>"
fi
```

If no peer surface matches, keep the unknown solo and let Step 2's
user-question loop work it.

**Sub-condition B — fires later, after Step 8's design doc is saved and
the Locked Decisions section is finalized.**

For each Locked Decisions entry, ask: does the decision affect a
peer-owned surface? If yes, send a heads-up once per affected peer (and
skip peers already in `messaged_peers:` from sub-condition A):

```bash
if [[ "<peer>" not in messaged_peers ]]; then
  if fno mail send --to-project <peer> --kind heads-up \
       --body "locked: <DECISION>; impact: <PEER-FACING DETAIL>; design: <saved-design-path>"; then
    append_peer_to_messaged_peers "<peer>"
  else
    append_peer_to_messaged_peers_failed "<peer>" "<reason>"
  fi
fi
```

The `messaged_peers:` list lives at the top of the saved design doc's
frontmatter (or, if /think later hands off to /blueprint, in the resulting
plan's frontmatter). /blueprint's 3a-bis and /target's ship recap both read
this field for dedup. Skip Locked Decisions entries with no peer-facing
impact - "internal: refactor X" decisions are not heads-up material.

**Anti-patterns (apply to BOTH sub-conditions):**

- Don't send "FYI considering X" - that is journal mode. Send only
  questions you genuinely need answered or decisions that change a
  peer's surface.
- Don't send to satisfy the prompt - it is conditional, not mandatory.
  Default is silence.
- Don't block on the answer. Sending is fire-and-forget; continue the
  flow.
- If an unknown or decision might apply to multiple peers and you cannot
  determine which is canonical, emit
  `<help reason="cross-project-disambiguation" evidence="<text>">` and
  continue solo. Never multi-peer blast.
- Don't send for internal-only changes. If the touched surface is not
  declared in any peer's `surfaces:` list, the change is internal by
  definition and no message goes out.

### 1d-bis. Backlog dedup (unconditional, all repo types)

Before schema work or approach exploration - and **regardless of whether the
repo is database-backed** - check whether this work already exists. Checking for
a duplicate node/plan/PR has nothing to do with whether the repo has a database,
so this runs on **every** `/think`, including non-DB repos (footnote itself). It
used to live inside the DB-gated Step 1e, so a non-DB repo skipped it wholesale
and duplicates only surfaced retroactively after ship (x-8af8).

```bash
fno backlog find "<feature keywords>"                                    # title tokens + one-line summary
gh pr list --state all --search "<feature keywords>" 2>/dev/null || true  # advisory; skips silently if gh is unauth/missing
```

If a node, plan, or open PR already covers this ground, surface the match to the
operator **before** design begins - consolidate (`/blueprint ... --claims <id>`),
narrow scope, or supersede - rather than filing a duplicate. The graph dedup
(`fno backlog find`) is the hard requirement; the `gh pr list` search is
best-effort and skips silently when `gh` is unauthenticated or absent.

### 1e. Schema Reconciliation (DB-backed repos)

When the repo is database-backed, ground the design against the real schema
and the backlog BEFORE exploring approaches, so the duplicate-or-not decision
in Step 2 is made with schema context instead of in the dark. This is the
phase where schema reconciliation is authored; `/blueprint` only re-does it
when `/think` was skipped.

**Detect a DB-backed repo.** Run this once; the `.env` clause widens the check
to repos whose connection lives only in a dev `.env` file (not the shell):

```bash
REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
db_env_found=""
for ef in .env.local .env.development.local .env.development .env; do
  if [ -f "$REPO_ROOT/$ef" ] && grep -qE '^(export[[:space:]]+)?(DATABASE_URL|POSTGRES_URL|SUPABASE_DB_URL|DIRECT_URL)=' "$REPO_ROOT/$ef"; then
    db_env_found=1; break
  fi
done
if [ -d "$REPO_ROOT/supabase" ] || [ -f "$REPO_ROOT/prisma/schema.prisma" ] || [ -f "$REPO_ROOT/drizzle/schema.ts" ] || [ -n "$DATABASE_URL" ] || [ -n "$db_env_found" ]; then
  fno codemap --tokens 2048 --db-schema 2>/dev/null || true
fi
```

If none of those hold, skip 1e: the repo has no schema surface, and the saved
doc records "no schema surface" (AC1-EDGE). The `--db-schema` companion
discovers the connection from the shell or a dev `.env` file
(`.env.production` / `.env.staging` are never auto-connected), reads the live
database read-only, and falls back to parsing migration files for tables/keys
when no DB is reachable. It never echoes the connection string.

**Reconcile.** Read the `## Database Schema` section from `.fno/codemap.md`:

1. **Backlog dedup already ran** unconditionally in Step 1d-bis; carry its
   verdict into the terminal summary below. Do not re-run it here (this step is
   now purely DB-schema reconciliation).
2. **Decide the touched surface.** List the tables / enums / constraints this
   feature reads or writes. A feature in a DB-backed repo that touches no
   tables records "no schema surface" and proceeds without a forced citation
   (AC1-EDGE).
3. **One-line terminal summary (MANDATORY, AC1-UI).** Print to the session
   output (not only the file), e.g.
   `schema: touches user_accounts, sessions | dedup: no existing capability covers this`
   or `schema: touches billing_events | dedup: overlaps ab-1234 - narrow scope`.

**Record it in the saved doc (Step 8).** The reconciliation becomes a
top-level section whose heading is exactly `## Schema Reconciliation`
(`/blueprint` greps for this literal marker to skip re-running schema
generation, so the spelling is load-bearing). It records the touched
tables/enums, an explicit dedup verdict ("no existing capability covers this"
vs "overlaps with ab-XXXX - narrow scope or supersede"), and any constraint
that shapes the design. If the live DB was unreachable and the schema came
from the migration parser, flag the section "(parsed from migrations, no live
DB)" so the reader knows it may be incomplete (AC1-FR).

### 1f. Deliverable Type Resolution

Classify the work into one of four deliverable types - **feature**, **bug**, **investigation**, or **epic** - and record the result as `deliverable_type:` in the saved doc's frontmatter.
The type drives which sections the contract (Step 8) requires, so resolve it before the Step 2 interview: a free-text seed folds its type confirmation into the first question batch.

**Resolution order (first match wins):**

1. **Graph type wins.** A node-seeded invocation (`/think <node-id>`) carries a
   graph `type` field: `bug -> bug`; `epic -> epic`; `feature | task -> feature`.
   No question asked. The epic contract **subsumes** the Step 1b decomposition
   check: its `## Gaps / Candidate Children` and `## Decomposition Guidance`
   sections ARE the decomposition product, so an epic-typed node proceeds through
   the epic flow instead of being redirected to split first. Step 1b's redirect
   still fires for non-epic types.
2. **Seed inference.** For a free-text seed, classify from the seed text and fold
   a one-line confirmation into the FIRST interview batch (Step 2) - never a
   dedicated extra round. Investigation signals: "audit", "verify",
   "investigate", "why does", "root-cause", "is it true that". Epic signals:
   "epic", "roadmap", "mission", "consolidate X across Y", or an enumeration of
   multiple independent subsystems. Absent any signal, default to **feature** (the
   full contract - the conservative direction).
3. **User override wins.** An explicit user correction at any point outranks a
   graph type and a seed inference alike.

The resolved type parameterizes the per-type contract (Step 8), the AC set
(Step 7), and the reviewer's anti-filler check (Step 8b).

### 2. Explore the Idea (One Round at a Time)

Interview the user to refine understanding - in **batched rounds**, not one
question at a time.

- **Never ask what the repo can answer.** Step 1c's discovery gate already read
  the code and surfaced the model's unknowns; Step 2 inherits that rule instead
  of re-asking generic "who are the users?" boilerplate the codebase (or the
  seeding node) already settles. Recon first, ask second.
- **Batch related questions into one round.** Use a single `AskUserQuestion`
  call with up to 4 questions, each leading with a recommended option. Replaces
  "one question at a time" with **one *round* at a time**. On a CLI without
  `AskUserQuestion`, degrade to one prose prompt listing the round's questions.
- **Ask only user-only questions**: requirements, preferences, tradeoffs,
  edge-case priorities, scope calls. Anything the code or docs can answer is
  recon, not a question.
- **Scale rounds to ambiguity.** A vague feature seed may need several rounds; a
  focused bug with a repro needs one round or none; node-seeded work whose
  details already lock the decisions needs zero. Stop when the open questions
  that remain are the user's to answer and you have their answers.
- **Fold the type confirmation in.** For a free-text seed, ride the Step 1f
  `deliverable_type` confirmation in the FIRST batch (never a dedicated round).

Prefer multiple choice when possible.

### 3. Explore Approaches
- Propose 2-3 different approaches with trade-offs
- Lead with recommended option and explain why
- Get user confirmation before proceeding

**Draft incrementally from here.** Once the approach is confirmed, save the doc
skeleton (the type's required section headings from Step 8's contract table, with
`deliverable_type` in frontmatter) and grow each section as it settles across
Steps 4-7. This survives a mid-design crash and keeps the running design visible.
The Step 8 save becomes a **finalize** of a doc already on disk, not the first
write.

### 4. Present Design (200-300 word sections)
Cover:
- Architecture and components
- Data flow
- Error handling
- Edge cases

**Check after each section**: "Does this look right so far?"

### 5. Multi-Perspective Challenge

**Applies to:** feature (all three perspectives) · bug (Pessimist + Silent-Failure Hunter only) · investigation (skip - the Evidence Chain section replaces it) · epic (optional - strategic lenses only; hand off deep stress-testing to `/think what-if` or `panel`).

Before finalizing the design, stress-test it from three angles. Present findings to the user for each:

**Perspective A — The Pessimist (What breaks?)**
- What happens when the API is down?
- What happens on network timeout mid-action?
- What if the user's session expires during a form submission?
- What if the database write succeeds but the response fails?

**Perspective B — The Impatient User (What gets abused?)**
- What happens if they double-click the submit button?
- What if they navigate away mid-operation?
- What if they open the same form in two tabs?
- What if they paste malformed data?

**Perspective C — The Silent Failure Hunter (What goes unnoticed?)**
- Which actions could fail with NO visible feedback?
- Are there fire-and-forget API calls with no error handling?
- Which state updates could silently not propagate?
- Where could an optimistic update fail to roll back?

Present a summary table:

```markdown
| Scenario | Current Handling | Risk |
|----------|-----------------|------|
| API returns 500 on save | ??? | User thinks save worked |
| Double-click submit | ??? | Duplicate records |
| Session expires mid-form | ??? | Lost work, no feedback |
```

### 6. CRITICAL: UI State Machine Audit (gated on UI surface)

**Applies to:** feature / bug **only when a UI surface is present** · investigation (skip unconditionally) · epic (skip unconditionally - children carry their own UI rigor).

Detect the surface with the same helper Step 6.5 uses, then decide:

```bash
HELPER="${SKILL_DIR}/references/detect-surface.sh"
SURFACE=$(printf '%s' "$DESIGN_TEXT" | bash "$HELPER" 2>/dev/null || true)
# SURFACE is one of: frontend-touching | backend-only | mixed | unknown (empty on detector failure)
```

- `frontend-touching` / `mixed` -> run this section.
- `backend-only` -> skip (no interactive surface).
- `unknown`, empty, or detector missing/errored on non-investigation work -> **KEEP this section** and note "surface unknown - detector failed" in the doc. Fail toward coverage; never silently drop a UI section on an unknown surface.
- investigation type -> skip regardless of the detector.

When the section runs:

**For EVERY interactive element in the design**, enumerate its states. No element gets a pass.

**The rule: every user action MUST produce visible feedback.** If an action can complete with zero visual change, that's a bug in the design.

For each interactive element (button, form, toggle, link, etc.):

```markdown
#### [Element Name] — State Machine

| State | Visual | Trigger In | Trigger Out |
|-------|--------|------------|-------------|
| idle | Default appearance | Page load / action complete | User clicks |
| loading | Spinner + disabled | User clicks | API responds |
| success | Success indicator | API returns 200 | Auto-reset after 2s |
| error | Error message + retry | API returns 4xx/5xx | User clicks retry |
| disabled | Grayed out | Missing prerequisites | Prerequisites met |

**Silent failure check:**
- [ ] Can this element reach a state where nothing visible happens?
- [ ] If the API call fails, does the element recover to a usable state?
- [ ] If the user interrupts (navigates away), is state consistent?
```

Only elements with simple, well-understood behavior (e.g., a navigation link) can skip the full table — but still need the silent failure check.

### 6b. Failure Modes (MANDATORY - becomes a required section in the saved design doc)

Every design doc MUST include a level-2 heading `## Failure Modes` with four
required sub-bullets. `/blueprint` grep-scans for this heading and refuses to run
without it, so the section is not optional even on trivial features.

**Required sub-sections (keep these exact bold labels so /blueprint can parse them).**
Each sub-section is a bold label followed by a bullet list, not an inline
list item. Use this structure verbatim:

1. **Boundaries** - limits and edge values. Zero, negative, max, overflow,
   empty input, input larger than the buffer, pagination cursor at the last
   page.
2. **Errors** - failure paths from dependencies. API 500 / 4xx, DB deadlock,
   disk full, permission denied, malformed response, partial writes.
3. **Invariants** - rules that must hold. Referential integrity, monotonic
   counters, ordering guarantees, "at most one active session," balance never
   negative, hash matches payload.
4. **Concurrency** - ordering and race hazards. Double-submit, stale-read
   writes, out-of-order events, interleaved retries, split-brain between
   nodes, the same operation landing via two code paths.

**Format:** one sentence per bullet in imperative form (**"must handle"**,
**"must reject"**, or **"must preserve"**) so the language carries a
testable obligation rather than a vague worry. The example below shows the
exact structure `/blueprint` will parse: `**Label**` on its own line, then a
dash-bullet list underneath.

```markdown
## Failure Modes

**Boundaries**
- The system must handle a cart with 0 items (render empty state, do not POST)
- The system must reject line-item quantities above 10,000 with a field error

**Errors**
- The system must preserve the user's form state when /checkout returns 500
- The system must reject payment responses whose signature does not verify

**Invariants**
- The system must preserve the invariant that an order has exactly one primary address
- The system must reject a submit that would leave the total below $0

**Concurrency**
- The system must handle two submit clicks within 100ms as a single order
- The system must preserve ordering when webhook retries arrive out of order
```

**Trivial features are NOT exempt from the structure.** If failure modes
truly do not apply (e.g., a single-file pure function with no I/O or
state), keep all four sub-sections so `/blueprint`'s parser, the reviewer
subagent, and the imperative-form rule still have content to validate.
State the "none" case per sub-section in one short imperative bullet:

```markdown
## Failure Modes

**Boundaries**
- The system must handle language-level integer range only (no domain bounds).

**Errors**
- The system must preserve behavior with no external dependencies to fail.

**Invariants**
- The system must preserve statelessness (no shared mutable state).

**Concurrency**
- The system must handle concurrent calls safely (pure function, no shared state).
```

If even one sub-section would force a lie (e.g., "must handle concurrent
calls" on a function that explicitly cannot be called concurrently), write
the bullet as "Not applicable: <one-sentence reason>" rather than deleting
the sub-section. The four bold labels are structural and must always be
present.

**When to delegate to `/think what-if`:** if the feature has >=3 external
dependencies OR touches auth / payments / concurrency / distributed state,
prompt the user to run `/think what-if` before finalizing. Emit a single,
specific hand-off line so the user can copy-paste it:

```
Run `/think what-if <domain> <depth> failure-modes "<scope>"` to stress-test: <categories>
```

Pick `<domain>` (software, product, business, security) from the feature's
dominant risk surface; `<depth>` is `standard` by default, `deep` for
high-risk features; the literal `failure-modes` positional modifier tells
`/think what-if` to emit a top-level `## Failure Modes` section this skill can
consume; `<scope>` is a one-sentence description of what to stress-test;
`<categories>` lists the dimensions to explore (e.g. `error_path,
concurrent, recovery`). Do NOT recommend `/think what-if` on trivial features
(inline enumeration here is sufficient). When `/think what-if` output is already
present in the design context, fold its findings into this section without
duplicating items.

> Red flag: an output that skips the `## Failure Modes` heading is a broken
> design doc. The saved doc, the reviewer subagent (Step 8b), and `/blueprint`
> must all treat the missing heading as a hard failure, not a style nit.

### 6c. Interface Contract (cross-repo features only - becomes a versioned, Locked section)

**Conditional, not mandatory.** Unlike Failure Modes, this section appears ONLY
when the feature spans more than one repo/project: a frontend in one repo coding
against a backend in another, two services sharing a wire format, a library and
its consumer shipping in lockstep. A single-project feature skips this step
entirely - no section, no version, no ceremony.

When the feature IS cross-repo, pin the exact interface both sides will code to
as a level-2 `## Interface Contract` section and capture it as a Locked Decision.
This is the artifact downstream `contract`-tier execution stubs against, so it
must be concrete enough that one side can build against it while the other
implements it.

**Required shape:**

````markdown
## Interface Contract

**contract_version: 1**

The schema / API / type surface both sides code to. Pin it concretely - the
exact request/response shapes, field names, types, status codes, error bodies.

- `POST /api/widgets` -> `{ id: string, name: string, createdAt: ISO8601 }`
- `WidgetCreated` event: `{ widgetId: string, actorId: string }`
- error: `409 { code: "duplicate", field: "name" }`
````

**Versioning rule.** `contract_version` starts at `1`. If a later `/think`
iteration amends the pinned surface (a field renamed, a status code added), bump
the integer and note what changed - never edit a shipped version in place. The
version is the token the reconciliation pass validates the landed schema against,
so a silent edit would let a drifted implementation de-stub against the wrong
shape. When more than one version is live at once, keep each under its own
`### Contract vN` subheading (newest first) so a parser can extract any
still-active version without walking git history.

**Lock it.** Add a Locked Decisions entry naming the contract and its version,
e.g. `Interface Contract v1 is the single source of truth for the X<->Y surface;
both the implementation and any stubs reference it.`

**No pinnable contract => not `contract`-eligible.** If the interface genuinely
cannot be pinned yet (still being discovered, depends on an unbuilt subsystem),
do NOT fabricate one. Omit the section and record why in Open Questions. A
cross-repo feature with no pinned contract falls back to `hard` serialization at
`/blueprint` decompose time: the dependent waits for its blocker to land rather
than building against stubs.

### 6.5 Executor Routing (capture as Locked Decision)

By this point the architecture, user stories, and (for non-trivial designs)
the implied file list are concrete enough to detect surface mix. Capture the
executor decision now so `/blueprint` can transcribe it into plan frontmatter
without re-asking. Skipping this step means the runtime resolver falls back
to surface inference at task time, which is correct but cannot express
plan-level intent.

Run the surface detector against the design text gathered so far (user
stories + architecture sections + any files-likely-touched list):

```bash
HELPER="${SKILL_DIR}/references/detect-surface.sh"
SURFACE=$(printf '%s' "$DESIGN_TEXT" | bash "$HELPER")
# SURFACE is one of: frontend-touching | backend-only | mixed | unknown
```

Resolve the call mode in this priority order. Load
[executor-routing-prompt.md](executor-routing-prompt.md)
for the full rule set, prompt template, and decision-capture format.

1. **CLI flag wins.** If the env var `FNO_EXECUTOR_OVERRIDE` is set
   (the contract `/target M --executor <value>` uses to plumb intent into
   /think), write that value to Locked Decisions with provenance
   `(cli-flag)` and skip detection entirely.
2. **Target autonomous auto-locks (LIVE manifest only).** Consult
   `fno target status --json`: if `attended` is `false` on a **live**
   manifest (`manifest-live` starts with `live`), /think is running inside
   an autonomous target session that cannot block on user input - apply the
   detection result and lock without prompting. Provenance: `(auto-detected)`.
   Key on the liveness verdict, NOT on `.fno/target-state.md` existing: a
   **dead** manifest (`manifest-live` starts with `dead`) is a defunct prior
   session (x-4af4 - one auto-locked attended /think for ~10 days), so do NOT
   lock; run fully attended and print one line naming it
   (`note: dead target manifest (.fno/target-state.md) ignored; running attended`)
   so the posture is visible, never silent. Pure-backend sessions (and
   `unknown`) never lock; the absence of a lock IS the signal, and surface
   inference handles backend correctly at runtime.
3. **Standalone interactive prompts.** No CLI flag, no target context. If
   the detection result is `frontend-touching` or `mixed`, fire the prompt
   from the reference doc and capture the user's choice with provenance
   `(user-confirmed)`. If the detection result is `backend-only` or
   `unknown`, skip the prompt entirely.

Write the chosen decision as a single Locked Decisions entry using the
format documented in
[executor-routing-prompt.md](executor-routing-prompt.md).
For `mixed`, the entry must list the surface-inference patterns
(`**/*.tsx`, `**/*.jsx`, `components/**`, `routes/**`, `src/styles/**`) so
`/blueprint` can emit per-task `executor: impeccable` overrides on matching
tasks while the plan default stays `executor: do`.

> **For trivial designs** with no surface signal at all (a refactor of a
> config loader, a one-line bug fix, a prose-only doc): detection returns
> `unknown` and this step writes nothing. The runtime resolver picks `do`
> via surface inference. No ceremony required.

### 7. Generate BDD Acceptance Criteria

**Applies to:** feature (all 5 types) · bug (AC-HP + AC-ERR + AC-FR + AC-EDGE; AC-UI only when the Step 6 gate found a UI surface) · investigation (skip - a verdict has no ACs; the Evidence Chain and Re-open Conditions sections carry its rigor) · epic (skip - `## Success Definition` replaces ACs; children carry their own AC rigor in their own design passes).

**Load the `/bdd-acceptance-criteria` skill** for comprehensive patterns.

For each testable behavior, write Given/When/Then using patterns from `bdd-acceptance-criteria/references/common-criteria.md`.

**The required AC types per deliverable type:**

| Type | Code | Tests | feature | bug | investigation | epic |
|------|------|-------|---------|-----|---------------|------|
| Happy path | AC-HP | Expected behavior works (bug: the repro now passes) | yes | yes | none | none |
| Error/validation | AC-ERR | Invalid input, API errors | yes | yes | none | none |
| UI state changes | AC-UI | Loading, disabled, feedback | yes | only if UI surface | none | none |
| Edge cases | AC-EDGE | Boundaries, empty state, concurrency | yes | yes | none | none |
| **Failure recovery** | **AC-FR** | **Silent failures, state recovery, interrupted operations** | yes | yes | none | none |

The AC-FR type is new and catches the bugs that slip through:

```markdown
#### AC1-FR: Failure Recovery - [Description]
**Given** I click [action button]
**When** the server returns a 500 error
**Then** I see an error message describing the failure
**And** the [button] returns to its idle state (not stuck in loading)
**And** my form data is preserved (not cleared)
**And** I can retry the action

#### AC2-FR: Interrupted Operation - [Description]
**Given** I start [async action]
**When** I navigate away before it completes
**Then** either the action completes in the background
**Or** the action is cancelled cleanly
**And** no orphaned state remains

#### AC3-FR: Double Action Prevention - [Description]
**Given** I click [submit button]
**When** I click it again before the first request completes
**Then** only one request is sent
**And** the button is disabled during processing
```

### 7b. Domain Pitfalls

Before handoff, ask: "What are the known pitfalls for [technology/domain]?"

Document any pitfalls that could affect the implementation plan. Examples:
- Next.js App Router: server/client boundary, hydration mismatches
- Supabase RLS: policies don't apply to service_role key
- React state: stale closures in effects, batching behavior

### 8. Finalize the Design Document

Finalize the doc drafted incrementally from Step 3 (or write it now if the flow
was short enough to skip the skeleton).

Save to the path printed by `fno plan path --slug "<feature-slug>" [--node "<node-id>"]` - it joins the resolved plans dir (`.claude/settings.local.json` → `plansDirectory`, then `.claude/settings.json` → `plansDirectory`, then `.fno/config.toml` → `plans_dir`) with the `config.plans_filename` template (default `%Y%m%d-{slug}-{node}.md`, e.g. `20260711-dark-mode-x-8af8.md`). Do NOT hand-assemble the filename and do NOT anchor on legacy loose files in the plans dir (`think-<node>.md`, dashed dates); the verb is the convention. Per invocation shape:

- **Node-seeded** (`/think <node-id>`): pass `--node` - the canonical node id is the filename suffix so a roadmap base keyed on the node id can find the doc (`/think x-8af8` → `…-x-8af8.md`; the configured prefix/width is preserved verbatim, never re-prefixed). First **reuse if claimed**: if a plans-dir file already carries this node in its frontmatter (`claims:`/`graph_node_id:`) or already ends `-<node-id>.md`, finalize INTO that file instead of minting a second one (a pre-created roadmap stub is the doc's home; a re-dispatch after a slug edit reuses the same doc). An empty slug degrades cleanly (the verb collapses the dangling separator), never a `--<node-id>.md`.
- **Raw prose** (no node): omit `--node` for the id-less name. If `/blueprint` later intakes it and assigns a node id, its step 3b-bis renames the artifact to carry the id and repoints `plan_path` - so the final invariant (id in both filename and `plan_path`) is reached either way.

**Link the doc to its node (node-seeded only).** After the file is written, point the backlog node at it so the design is visible from the board and `/blueprint <node>` can find it without being handed a path:

```bash
# Node-seeded runs only. Raw prose has no node yet - /blueprint intakes it later.
[[ -n "$NODE_ID" ]] && fno backlog update "$NODE_ID" --plan-path "$DOC_PATH"
```

This is safe, and only became safe with the `design` rung (x-5d91). The node now derives `design` - visible but NOT autonomously dispatchable - because this doc's frontmatter says `status: design`. Before that rung existed, linking a design doc flipped the node to `ready` and the dispatcher claimed it within ~a minute, which is why the old advice was to leave plans unlinked until blueprint. That workaround is retired: link freely.

Two invariants make it hold, so do not "simplify" either away:
- **The doc must carry `status: design`** in its frontmatter (it does - stamped with the other keys below). The probe demotes only on that positive evidence, so a doc with no status key, or one stamped `ready`, would ARM the node instead of parking it.
- **Autonomous selection is gated on the rung, not on `plan_path`.** `next` / `--all-ready` / converge / lane-fill / the daemon drain all skip `design`. An explicitly-named node still dispatches from any rung (naming is the consent), and `/target` runs `/blueprint` first when it lands on `design`.

Stamp the resolved type into the doc's frontmatter as `deliverable_type: feature | bug | investigation | epic` (Step 1f).
An **epic** additionally stamps `scope: epic` - that second key is `/blueprint`'s auto-group trigger; without it an epic silently collapses into the single-PR lean mutation, shipping a multi-wave epic as one PR.
Stamp both keys on every epic doc.

**The required sections scale to `deliverable_type`.** The uniform 12-section
contract manufactured filler on non-feature work (an investigation verdict was
forced to fabricate AC-UI and UI-state sections). Include a section only where
this table marks it for the resolved type:

| Section | feature | bug | investigation | epic |
|---|---|---|---|---|
| Overview | yes | yes | yes | yes |
| Schema Reconciliation (1e, DB-backed) | yes | yes | yes | yes |
| **Vision** (new) | no | no | no | **yes** - the end state in prose (no implementation detail) |
| **Success Definition** (new) | no | no | no | **yes** - measurable mission-level outcomes (the epic-altitude replacement for BDD ACs) |
| Architecture | yes | as "Fix approach" | optional | recommended for brownfield - the traced as-is landscape, under the literal `## Architecture` heading (`/blueprint` reads it) |
| **Gaps / Candidate Children** (new) | no | no | no | **yes** - enumerated gaps, each a candidate child (explicitly NOT a final node list) |
| **Decomposition Guidance** (new) | no | no | no | **yes** - grouping / sequencing instructions to the decomposing session |
| **Operator Intent** (new) | no | no | no | **yes** - decision principles and context over structure |
| User Stories | yes | yes - the fix's discrete work items (see note) | no | yes - one story per candidate child or delivery group |
| Multi-Perspective Findings (5) | full | Pessimist + Silent-Failure only | no (Evidence Chain replaces it) | optional - strategic lenses; hand off deep stress-testing to `/think what-if` or `panel` |
| UI State Machines (6) | only if UI surface | only if UI surface | never | never |
| **Failure Modes (6b)** | **yes** | **yes** | **yes** | **yes** |
| Interface Contract (6c) | if cross-repo | if cross-repo | never | optional (cross-repo epic) |
| Repro (new) | no | **yes** - commands/steps that reproduce the bug | no | no |
| Acceptance Criteria (7) | all 5 types | AC-HP/ERR/FR/EDGE; AC-UI only if UI surface | none | **no - excluded** (anti-filler target; Success Definition carries verification) |
| Evidence Chain (new) | no | no | **yes** - each claim pinned to a source | no |
| Re-open Conditions (new) | no | no | **yes** - the observation that would invalidate the verdict | no |
| Domain Pitfalls (7b) | yes | yes | optional | optional |
| **Non-goals** (new) | no | no | no | recommended - scope creep is the canonical epic failure mode |
| Locked Decisions + Claude's Discretion | yes | yes | yes | yes - written child-consumable (decompose transcribes the Locked block verbatim) |
| Open Questions | yes | yes | yes | yes |

**Why a bug keeps `## User Stories`.** `/blueprint` synthesizes its
`## Execution Strategy` task list *solely* from `## User Stories`
(`mutate_doc.py:_build_execution_strategy`); a doc with no stories degrades to a
single empty "implement feature" task. A bug's discrete fix steps therefore live
under `## User Stories` (framed as work items, not "As a user I can..."
narratives) so the bug's real work survives into the plan. Dropping the section
would silently gut blueprint's task synthesis - and the parser seam is frozen, so
this is a think.md-side obligation, not a blueprint change. An `investigation`
omits it because it is no-build (it never reaches `/blueprint`).

**The three type-specific new sections:**

- `## Repro` (bug) - the exact commands or steps that surface the bug today, so
  AC-HP can assert "the repro now passes" against a concrete starting point.
- `## Evidence Chain` (investigation) - each claim in the verdict on its own
  bullet, pinned to a source (file:line, command output, PR, doc). An
  investigation's rigor lives here in place of BDD ACs.
- `## Re-open Conditions` (investigation) - the concrete observation(s) that
  would invalidate the verdict and warrant re-opening the question.

**The epic-specific new sections** (mission framing at epic altitude, in place of
feature ACs and UI rigor):

- `## Vision` (epic) - the end state in prose: what is true when the epic is done
  and why it matters. No implementation detail; this is the direction the children
  serve.
- `## Success Definition` (epic) - measurable mission-level outcomes, each
  checkable after all children land. This is the epic-altitude replacement for BDD
  acceptance criteria; write each outcome so a wrong-but-passing world would fail
  it, never a bare "the system works."
- `## Architecture` (epic, recommended for brownfield) - the traced as-is
  landscape the epic changes (its "current state"), documented under the literal
  `## Architecture` heading. Keep that exact heading even at epic altitude:
  `/blueprint` reads `get_section("Architecture")` to detect brownfield and build
  the File Ownership Map, so a renamed heading (e.g. `## Current State`) silently
  loses those downstream sections. Greenfield epics may omit it.
- `## Gaps / Candidate Children` (epic) - the enumerated gaps between current state
  and vision, each a candidate child node. Explicitly NOT a final node list - the
  decomposing session calibrates the real count from Decomposition Guidance.
- `## Decomposition Guidance` (epic) - instructions to the decomposing session:
  grouping principles, what must not be split, sequencing, "do not mint one node
  per gap." Size in PRs - one node is one PR; coordination, acceptance, and
  research gates are epic-level work, never PR nodes, and trigger-based follow-ons
  stay OUT of the initial decomposition with their trigger named.
- `## Operator Intent` (epic) - decision principles and context over structure:
  what the operator would decide when a child hits ambiguity, so a child's own
  design pass inherits the intent rather than re-litigating it.
- `## Non-goals` (epic, recommended) - what this epic explicitly does not cover.
  Scope creep is the canonical epic failure mode; naming the out-of-scope surfaces
  is the cheapest guard against it.

**`## Failure Modes` stays mandatory for all four types** with all four bold
labels (Boundaries / Errors / Invariants / Concurrency), using the "Not
applicable: <reason>" bullet rule where a label genuinely does not apply. The
`/blueprint` parser seam is frozen: it greps for exactly `^## Failure Modes$` and
the four labels, so this section never scales away.

**The UI-surface gate (Step 6) decides the UI rows.** Reuse
`references/detect-surface.sh`; a detector failure or `unknown` surface on
non-investigation work KEEPS the UI sections (fail toward coverage).

`## Schema Reconciliation` (all types, when DB-backed) and `## Interface Contract`
(cross-repo only, feature/bug) keep their exact literal headings - `/blueprint`
greps for both. A cross-repo feature that cannot pin a contract yet omits the
section and records why in Open Questions (no placeholder `contract_version`).

### Locked Decisions Section (MANDATORY)

Every design output must include:

```markdown
## Locked Decisions (DO NOT revisit)

These decisions are settled. The planner and executor must not revisit them:

1. [Decision]: [Rationale]
2. [Decision]: [Rationale]

## Claude's Discretion

These areas are open for the implementing agent to decide:

1. [Area]: [Constraints, if any]
```

#### Decisions worth locking explicitly

Beyond domain-specific choices, surface these routing/orchestration
decisions when they apply - the planner needs them as plan frontmatter,
not as "we'll figure it out at implementation time":

- **Executor routing** (frontend-heavy or mixed-surface work): which
  executor drives the implementation? `do` (default, archer / TDD) or
  `impeccable` (frontend-executor + /impeccable craft+critique loop)?
  Lock plan-level if the whole feature is one surface; lock per-task
  if mixed. Surface inference is a fallback, not a substitute for the
  decision. See `docs/guides/per-task-executors.md`.
- **Cross-project scope**: single-project (default) or `cross-project`
  with worktrees per repo?
- **Interface Contract version** (cross-repo features, from Step 6c): when the
  design pins a `## Interface Contract`, lock its `contract_version` so the
  dependent stubs against a frozen surface and the reconciliation pass can
  detect drift against a known shape.

### 8b. Spec Review Loop

After saving the design document, spawn a Haiku reviewer subagent to critique it:

1. Dispatch reviewer with the full design doc text **plus `deliverable_type` and
   the type's required-section list from the Step 8 contract table.** The
   reviewer judges the doc against ITS type's contract, not the uniform one.
2. Reviewer checks:
   - **Hard-fail (all types):** a missing `## Failure Modes` heading or any
     missing bold label (Boundaries / Errors / Invariants / Concurrency) - the
     frozen `/blueprint` parser seam. Unchanged.
   - **Hard-fail (per type):** a type-required section is absent - a `bug`
     without `## Repro`; an `investigation` without `## Evidence Chain` or
     `## Re-open Conditions`; a cross-repo feature/bug without a
     `## Interface Contract` carrying `contract_version` and a Locked Decision
     referencing it (unless the omission is explained in Open Questions); an
     `epic` missing ANY of `## Vision`, `## Success Definition`,
     `## Gaps / Candidate Children`, `## Decomposition Guidance`, or
     `## Operator Intent` - **or carrying one as a bare heading with no content
     under it** (the empty-section rule, same substrate as the empty-stories check
     below) - or missing the `scope: epic` frontmatter key, or with `## User
     Stories` absent or empty.
   - **Anti-filler check (new):** ANY section the resolved type EXCLUDES (marked
     `no` for that type in the Step 8 required-section table) that is present
     anyway is flagged **for removal, not approved** - AC blocks or UI-state
     tables on an `investigation` **or an `epic`**, an `## Evidence Chain` on a
     `feature` or `bug`, or an epic-only mission section (`## Vision`,
     `## Success Definition`, `## Gaps / Candidate Children`,
     `## Decomposition Guidance`, `## Operator Intent`, `## Non-goals`) on a
     `feature`, `bug`, or `investigation`. This is the
     x-2bf7 failure inverted: the reviewer once approved a no-build verdict's
     fabricated AC-UI sections; a type-excluded (type-excluded == filler) section
     is now a finding.
   - **Empty-stories check (new, all buildable types):** a `## User Stories`
     heading with no story content under it is a finding on `feature`, `bug`, AND
     `epic` - `/blueprint` silently degrades to one empty "implement feature" task
     otherwise. This is a reviewer-prompt line, not a parser change.
   - **AC adequacy attack (new):** for each AC in the doc, try to name one
     concrete implementation or input that satisfies the AC as written while
     violating the design's intent - a wrong-but-passing implementation, a
     degenerate input, a silent no-op. If you can name one, emit an `ac-bypass`
     finding carrying the AC id, the named bug in one sentence, and a suggested
     sharpened Then-clause that the named bug would fail. One `ac-bypass` finding
     per AC maximum; none-case ("Not applicable") ACs and Failure Mode bullets
     are out of scope, do not invent bugs for them. A rated critique ("this AC is
     weak") is not a finding; only a named bug is - vague criteria, vague
     critiques. On an `epic` this attack is a natural no-op: the epic contract
     excludes ACs, and the anti-filler check already flags any AC block that
     sneaks in.
   - **Ground-truth join (new, eval-type designs):** an eval / metric / scoreboard
     design that scores process shape without naming its **ground-truth join** -
     the downstream observation that would falsify a good score (e.g. a PR that
     scored clean yet bounced) - is a finding, unless it explicitly declares
     itself process-only. A good process score with no outcome to falsify it is a
     number that cannot be wrong, which is not a measurement. The fix is to name
     the join (what real-world outcome the score is checked against) or the
     process-only declaration; a rated critique is not a finding, a named missing
     join is.
   - General quality: missing error states, contradictions between sections,
     vague implementation details.
3. If issues found: fix them, re-dispatch reviewer (max 3 iterations). Resolve an
   `ac-bypass` finding by sharpening the named AC's Then-clause so the bug no
   longer passes - never by deleting the AC below the type contract's required AC
   set.
4. If approved (or 3 iterations reached): present to user. Any `ac-bypass`
   finding still unresolved at the 3-iteration cap is recorded under the doc's
   `## Open Questions` (never silently dropped), and the presentation summary
   lists one line per sharpened AC (id + named bug).

> "Design doc written and reviewed (N iteration(s)). Please review and let me know if you want changes before we create the implementation plan."

### 8c. Stamp think provenance (node-seeded only)

Once the design doc is saved and reviewed AND this invocation is node-seeded
(`/think <node-id>`, so exactly one node is bound), stamp the lifecycle
provenance so the graph records which session/harness thought about the node:

```bash
fno backlog session add <node-id> --phase think
```

Harness + session id default from the ambient identity; the primitive is
idempotent (a re-run is a no-op) and append-only. Skip silently for raw-prose
`/think` (no node to attribute) or if the stamp warns about missing identity -
provenance is best-effort and never blocks the design output.

### 9. Output for Target Pipeline

When invoked as part of a pipeline (a **live** target manifest - `fno target status --json` reports `manifest-live` starting with `live`; this works standalone without one, and a dead manifest counts as standalone),
structure your output with clear sections:
- **Design Decisions** - with rationale
- **Constraints Discovered** - technical limits found during exploration
- **Rejected Alternatives** - what was considered and why not
- **Failure Modes** - verbatim copy of the `## Failure Modes` section from Step 6b, so `/blueprint` can consume it via the scratchpad as well as the saved design doc
- **Open Questions** - unresolved items for the plan phase

Target will capture this into the scratchpad for downstream phases.

### 10. Handoff

Ask: "Ready to create the implementation plan with `/blueprint`?"

## Key Principles

- **One round at a time** - batch related questions into a single round; never overwhelm, but never drip one question at a time either
- **YAGNI ruthlessly** - Remove unnecessary features
- **Test-first thinking** - Always ask "how would we verify this?"
- **An AC a bug can pass is not an AC** - write every Then-clause so a wrong-but-passing implementation would fail it; pin an observable output and bound, never just "it succeeds"
- **Every action needs feedback** - If a user does something and nothing visible happens, that's a design bug
- **Multi-perspective challenge** - Stress-test from pessimist, impatient user, and silent failure angles
- **State machines over checklists** - Enumerate states for interactive elements, don't just list happy paths

## NEVER (Design Thinking Anti-Patterns)

**NEVER skip the "What could go wrong?" perspective:**
- Optimistic design is incomplete design
- For every feature: "What happens when this fails?"
- For every UI state: "What does the user see when data is missing/stale/wrong?"

**NEVER ship a design doc without a `## Failure Modes` section:**
- The heading is a required output artifact. `/blueprint` refuses to proceed without it.
- Even trivial features get the heading, with a one-line justification for "none".
- Omitting the sub-sections (Boundaries / Errors / Invariants / Concurrency) is the same as omitting the section: the downstream parser will not find the content it needs to seed `AC4-EDGE` criteria.

**NEVER design only the happy path:**
- Empty states, error states, loading states, partial-data states
- Offline behavior, timeout behavior, race conditions
- First-time user vs power user vs admin

**NEVER assume the user's mental model matches yours:**
- "Intuitive" is subjective — what's obvious to you may confuse users
- Name things from the USER's perspective, not the developer's
- When in doubt, use the terminology from the domain, not the codebase

**NEVER propose architecture changes without considering migration:**
- "We should use X instead of Y" requires: how do we get from Y to X?
- Breaking changes need migration plans, not just target state
- Existing data, existing users, existing integrations — all must survive

**NEVER conflate "I like this pattern" with "this is better":**
- Personal preference ≠ technical improvement
- "Modern" ≠ "better for this project"
- Justify with concrete benefits: performance, maintainability, safety — not aesthetics

## Session Cost Tracking (AUTO — enforced by stop hook)

Cost is automatically registered by the stop hook when the session exits. The stop hook scans the transcript for `fno:think` Skill tool invocations, calculates cost via `session-cost.py`, and appends to `ledger.json` via `register-task.py`. No manual action needed.
