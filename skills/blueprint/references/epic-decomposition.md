# Bounded Epic Decomposition (`group N`)

Read this when the invocation carries `group N` / `group`, when the plan frontmatter declares `max_prs:`, or when a bare `/blueprint <doc>` targets a `scope: epic` doc (auto-group). A non-epic doc with no `group`/`max_prs:` keeps the single-doc lean mutation and never loads this.

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

**Resolve the ceiling `N`** as the minimum of every ceiling that applies, with
the config default as the floor of last resort. This mirrors decompose's own
`effective = min(max_children, explicit)` (x-066a) exactly, so blueprint's `N`
and the `--max-prs` it forwards stay in lockstep with the CLI and the two
surfaces never disagree about the cap.

Let `explicit` = the `group N` arg if present, else the per-plan `max_prs:`
frontmatter if present, else unset. Let `mc` = the epic doc's `max_children` if
present and a **positive integer**, else unset (read it from the same epic
frontmatter already loaded for the `scope: epic` / `max_prs:` auto-group
decision above - no new reader). A present-but-malformed `max_children`
(non-integer, `< 1`) is **refused up front**, before grouping: exit non-zero
with `epic max_children must be a positive integer` and create nothing. Do NOT
treat a bad value as unset and defer the check to decompose - procedure step 3
skips decompose entirely when the waves cohere into a single group, so deferring
would let a malformed durable cap pass silently on that path. Blueprint fails
fast so the refusal holds on every path, matching decompose's own
malformed-value refusal (x-066a US5); blueprint never invents a cap from a bad
value, nor silently falls back to the default on one.

| Case | `N` (blueprint's grouping ceiling AND `--max-prs` forwarded) |
|------|-------------------------------------------------------------|
| both `explicit` and `mc` set | `min(explicit, mc)` |
| only `explicit` set | `explicit` |
| only `mc` set | `mc` |
| neither set | `config.blueprint.max_prs_per_epic` (default 4) |

Read the config floor with:
```bash
N=$(fno config get config.blueprint.max_prs_per_epic 2>/dev/null || echo 4)
```
If `fno config get` is unavailable, default to 4. **Auto-group MUST degrade to
today's single-doc behavior (not error) if the config read fails** - treat an
unreadable ceiling as 4, never abort the blueprint. Because the epic's own
`max_children` now sits above the config default in this ladder, an author who
declares `max_children: 6` above a default of 4 gets 6 honored on the bare
`/blueprint <epic-doc>` path (x-066a US3), not silently clamped to 4.

`N` is a **ceiling, not a quota** (Locked Decision #3): cohesive work uses
fewer groups; never pad to `N`. This guardrail applies identically to
auto-group - an auto-decompose must never produce more than `N` groups, and a
`scope: epic` doc whose waves cohere into one group still ships ONE PR (record
it in `## Delivery Groups`, never force a split). Reject `group 0` / negative
`N` with a non-zero exit and the message `group N must be >= 1` (AC1-ERR),
creating nothing.

**Procedure** (after the epic node is intaken in step 3b, so `EPIC_ID`
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

   Ceiling: N (source: group-arg | frontmatter max_prs | epic max_children | config default)

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
     {"slug": "2", "title": "Group 2: frontend", "waves": "4-6", "blocked_by_groups": ["1"], "dep": "contract"},
     {"slug": "3", "title": "Group 3: novel index engine", "waves": "7", "blocked_by_groups": ["1"], "needs_think": true}
   ]
   JSON
   fno backlog decompose "$EPIC_ID" --max-prs "$N" --groups "@/tmp/groups-$$.json"
   rm -f /tmp/groups-$$.json
   ```
   The verb creates one child node per group (`parent=$EPIC_ID`, its own
   self-contained quick-plan scaffold at the canonical `fno plan path` name in
   the child's own project plans dir - the path it prints as `scaffolded plan:`
   / reports in the `--json` `scaffolded[]`), `blocked_by` resolved from
   `blocked_by_groups`, prints the epic id and each child id with its wave
   range, and is idempotent: re-running `/blueprint group N` on an
   already-decomposed plan updates the same children in place (keyed on the
   group slug) rather than duplicating (US4). A bad spec leaves the
   graph untouched (AC1-FR) because the whole decomposition lands in one
   locked mutation. If a re-decomposition drops a slug that already
   shipped a PR, the verb refuses (exit 2) unless you pass `--force`; unshipped
   dropped groups are left in place and reported as a warning.

   **Set `needs_think: true` on a group that owns genuine unknowns** - a
   feasibility spike, unresolved epic Open Questions, or a novel subsystem. That
   child gets a dispatched `/think` + `/blueprint` design pass instead of the
   inline-fill below (the decompose invocation is the consent for that spawn).
   Leave it off (the default) for a group whose scope is already clear from the
   epic; that child takes the inline-fill path.

7. **Inline-fill every unflagged child BEFORE linking (MANDATORY).** Decompose
   births each child UNLINKED (`status: stub`, no `plan_path` -> derives `idea`),
   so nothing dispatches against an empty scaffold. You hold the epic in context
   right now - the warmest window there will ever be - so rewrite each unflagged
   child's scaffold (the path decompose reported as `scaffolded plan:` - a
   canonical `YYYYMMDD-<slug>-<id>.md` in the child's own project plans dir) into
   a real quick-plan, then link it:

   - Fill `## Why (from epic)` (the seeded intent + Locked Decisions, narrowed to
     what binds THIS child - transcribed, never a pointer back at the epic).
   - Replace every stub marker: concrete `## Changes`, `## Files to Modify` (from
     the epic's File Ownership Map), `## Verification` (the checks that prove this
     slice), and a `kill_criteria`.
   - Flip the frontmatter `status: stub` -> `status: ready` (`stub` is outside the
     canonical PlanStatus vocabulary, so a linked-but-still-stub plan is later
     archived by `fno plan reconcile-status`; the validator refuses to link one).
   - Validate, THEN link (link LAST, after the content is real):
     ```bash
     bash "${SKILL_DIR}/scripts/validate-plan.sh" <child-plan> \
       && fno backlog update <child-id> --plan-path <child-plan>
     ```
   The validator REFUSES a plan still carrying any stub marker or an empty
   `## Why (from epic)`, so a half-filled child can never be linked. Linking flips
   the child `ready` - that is the design-completion signal (there is no
   `status: ready` to hand-write). A flagged (`needs_think`) child skips this: its
   fan-out design pass produces the real doc and links it.

**Slug stability.** Use stable slugs across re-decomposition so idempotency
holds. Numeric (`1`, `2`, ...) is the simple default; named slugs
(`auth-flow`) are fine as long as they do not change between runs.

**Packaging: `separate` only.** Every child gets its own self-contained
quick-plan file - `plan == PR == node` for children too. Decompose scaffolds a
stub per child (`## Why (from epic)` + Context / Changes / Files to Modify /
Verification, born `status: stub`) at the canonical `fno plan path` name in the
child's own project plans dir (reported as `scaffolded plan:`; existing legacy
`<stem>.group-<slug>.md` stubs are grandfathered in place), and births the
child WITHOUT a `plan_path` - identity is the durable `group_slug` field, so the
unlinked child is still found on re-decompose. Linking the filled plan (inline
step 2, or the fan-out pass) is what makes it `ready`. It is the default (and
only) packaging - `--plans` need not be passed; `--plans fragment` errors.

Scaffolding is idempotent on the slug: re-running upserts the same children, an
existing scaffolded file is never clobbered (a builder's edits survive a
re-decompose), a designed child's `plan_path` is preserved (never unset), and a
child still on the legacy `<doc>#group-<slug>` fragment form (from a pre-removal
decompose) is repointed to its separate file in place.
