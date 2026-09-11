# graph/cli.py rationale register

<!-- style-exception: verbatim relocation of code-comment essays under the
file-budget ratchet (law d-4b39ad4c steering). Rewriting the prose would
break the provenance contract between these paragraphs and the review
threads that produced them. -->

Long-form rationale moved out of `cli/src/fno/graph/cli.py` so the code
keeps a one-line pointer and the tree stays under the Python allowance.
Text is verbatim; the anchor names the function the essay belongs to.

## cmd_decompose (formerly at line 2220)

Epic children that no group adopted ( US3). Uses the SAME
predicate as the adoption refusal above, deliberately: keying this on
the base-scoped group_child_slug instead would list a group child
whose plan path no longer matches a renamed epic doc, and the warning
would then tell the operator to adopt a node the refusal blocks.
Collected after the re-parenting above, so
adopted nodes have already left the epic and exclude themselves; no
cross-reference against the adopt lists is needed. Warning, not
refusal: refusing deadlocks, because re-parenting needs the group
nodes a refusal prevents creating, and a parked child is a legitimate
steady state.

## cmd_decompose (formerly at line 2279)

3c. Scaffold per-child quick-plan files (--plans separate). Runs OUTSIDE
the graph lock (mirrors the _set_expected_count doc write below), because
it reads settings (plans_content_dir walks .claude/settings) and settings
reads never happen under the lock. Each child is born at its CANONICAL
`fno do plan path` name, routed into the CHILD project's plans dir - not the
epic's dir, not the legacy `.group-<slug>.md` name ().
US4: transcribe the epic's why (intent + Locked Decisions) once - every child
scaffold is born grounded, AND a fan-out seed carries it so the /think worker
stays grounded even when its origin transcript is unresolved. A missing
Locked-Decisions block degrades to intent-only + a warning; an unreadable doc
yields an empty digest (the scaffold then seeds the validator-rejected stub).

## cmd_decompose (formerly at line 2326)

Route the stub into the CHILD project's plans dir. The child node's
own cwd is the authoritative per-child root: the mutator set it to
the routed cwd (or inherited the epic's) at mint, so it already
reflects a route made WITHOUT an explicit re-route - a routed child
re-decomposed with no route keeps its own repo, not the epic's.
created_at sources the filename date, so a later-day re-decompose
recomputes the SAME path (idempotent). Fall back to the epic cwd
then repo_root() only if the re-read lost the node.

## cmd_decompose (formerly at line 2375)

4a. Per-child design pass ( US3) + born-with-why (v2 A1). Runs BEFORE
the report so a flagged fan-out's outcome rides in the --json payload
(a machine caller must see when a child was left an unlinked idea, not a
silent success). Two lanes, one shared RunState bounding the batch's blast
radius (AC1-EDGE):
- `needs_think` group -> FORCE a fan-out /think+/blueprint design pass.
The decompose invocation IS the operator consent (Locked Decision 3),
so the gate + attended-offer are overridden (mirrors the
dispatch_conversational env-forcing); the caps still bound it. A spawn
that does not fire leaves the child `idea` with its stub on disk.
- unflagged group -> nothing. `needs_think` is the SOLE consent for a
decompose-time /think: the born-with-why lane that used to
run here spawned unconditionally on any autonomous decompose, because
its OFFER branch needs presence == attended and an autonomous session
always classifies `away`. The epic doc is a group child's design
authority; scaffold_separate_plan + why_digest already carry it.
Only UNLINKED children are candidates (a re-decompose never re-designs a
child that already has a plan). Strictly non-fatal: never wedge decompose.

## cmd_decompose (formerly at line 2451)

`owned` resolves the inline-fill handoff (Open Question 3)
and is the field step 7 reads. Ownership is keyed on the
OBSERVED spawn receipt, never on predicted wave
membership: if the spawn did not fire, the child is still
inline-fill's, so a spawn failure degrades to today's
behavior instead of leaving an orphan nobody fills.
That is also what makes double-writing impossible (AC9-CON)
- exactly one lane can see `owned: true` for a child.

## cmd_decompose (formerly at line 2571)

5. Record the group count N on the shared epic doc so it graduates only
after all N group PRs ship (not after the first). The graph mutation
above is the source of truth and is NEVER rolled back; decompose also
never exits non-zero on a stamp problem, because that would break
pipelines (e.g. /blueprint group) for a best-effort stamp. A genuine
write failure (the doc exists but could not be written) is surfaced as
a loud, actionable stderr warning so it is not silent; environment
skips (absent doc/script - which also can't be stamped at ship, so no
early graduation) stay quiet.

## cmd_update (formerly at line 3397)

Model pin (): a validated-shape pass-through, not an allowlist. The
value must be a single shell-safe token so it survives unquoted use in the
dispatchers (dispatch-node.sh $model_arg, the loop-driver MODEL_FLAG
word-split) without word-splitting OR globbing. The charset [A-Za-z0-9._:/-]
covers every real model id (fable, claude-opus-4-8, openai/gpt-4,
us.anthropic.claude-...) while forbidding whitespace and shell/glob
metacharacters (* ? [ ] etc.); the CLI (not fno) resolves the alias.
'null' clears.

## cmd_update (formerly at line 3722)

plan == PR == node (): a plan file is the delivery unit
of exactly one node. Refuse to arm a second node against a plan
another node already owns; the ambiguous state is what armed
two concurrent dispatches against one sequential-mode plan on
2026-07-28. Routed through the shared plan_path_owner_conflict
so every write site (this one + intake's lanes) checks one way;
the decompose repoint is scoped to a slug it owns and never
lands here. --force escapes a deliberate repoint and still
names the other holder (AC2).

## cmd_update (formerly at line 3928)

Re-parenting AWAY from the delivery unit un-adopts the node
(). Without this there is no supported way out of a mistyped
`adopt` id: only decompose writes `contained_in`, re-running the
spec without the entry leaves the stale value, and graph.json is a
hook-blocked forbidden surface - so one typo permanently unarmed a
real delivery unit AND had the cascade later stamp it "shipped
inside <owner>", a false completion note on work that never
shipped. It also keeps `parent` and `contained_in` from
disagreeing, which is what produced that false note.
Deliberately keyed on moving away from THE OWNER, not on any
re-parent: a contained node moved between two nodes that both sit
under its delivery unit is still contained.

## _starvation_receipts (formerly at line 4190)

Read off the persisted rung, not the guard: the guard only fires
for the stale window (graph still says `ready`, doc since edited
down a rung), so a node already ON the rung would fall through to
`selection_guards`, get None (it is gated on a persisted `ready`),
and be dropped by the `continue` - reporting nothing at all.
`idea` is the COMMON case for a linked decompose scaffold, since
recomputation persists that rung directly; without this arm a
backlog of nothing but undesigned children prints a bare `null`
instead of naming what each one is waiting on.

## _joined_open_candidates (formerly at line 4307)

: How long an external selector's `node:<id>` claim protects the node it just
: handed out. It is a SELECTION window, not a work lease: the winner is handed
: to a caller that has yet to launch anything, and the worker replaces this
: hold with its own the moment `fno target init` runs. Sized to match the spawn
: handover window for the same reason - both cover launch-to-init, and the
: selector's caller has strictly less to do before init than a spawn does. Short
: on purpose: a selector that never dispatches must not wedge the node, and an
: expired claim is provably dead so the node self-heals.

## cmd_next (formerly at line 4424)

Epic-scope filter (C2, ): restrict candidates to the
transitive children of --parent. Resolve the parent id up-front so a
missing node is a hard error (AC2-ERR) and a childless node prints a
clear note while still returning null so the walker can fall back
(AC2-EDGE). The actual descendant SET is computed inside the keeper's
ready verb from the entries it receives so that under --claim it reflects the
locked graph state, not a pre-read snapshot (avoids a TOCTOU where a
concurrent reparent could claim a node no longer in the subtree).

## cmd_next (formerly at line 4593)

TWO things have to be true for this lock to protect anything,
and routing alone gave only the first.
ROUTE the root, or the lock lands in the cwd-default tree
while every reader of a `node:` key resolves the global root
through `claims_root_for`, so the node still reads `free`.
`_read_node_claim` names the same trap from the other side.
TTL, or the lock is visible and still not honored. Selection
runs in a process that exits as soon as it prints the node,
so a pid-liveness claim is dead on arrival: it reads `stale`,
which does not block dispatch, and a worker launches onto the
node this selector just handed out. The TTL makes the dead pid
read `suspect` instead, which does block, and it bounds the
hold so an external selector that never dispatches cannot wedge
the node past the window.

## cmd_task_update (formerly at line 6154)

The pid is the discriminator, not the name. A claim whose holder name
equals OURS but whose pid is a different live process proves the
identity is shared (an opencode worker's daemon-anchored pid, a
manifest-inherited session id) - refusing with the fix beats attributing
a stranger's work. release_claim is non-strict, so honoring a takeover
against a LIVE claim would delete a running worker's claim and let the
next in_progress succeed on an empty key: the double-dispatch these verbs
exist to close. A live claim is never takeable, whoever it names; this
guard runs for every transition, so done/pending cannot settle a live
worker's row either.

## cmd_remove (formerly at line 6902)

-- defer / undefer --
``defer`` records a first-class pause on a backlog node via dedicated
``deferred_at`` + ``deferred_reason`` fields. The cascade derives
``status: deferred`` from those fields so the node disappears from the
default ``ready`` / ``next`` candidate sets and from triage proposals,
but resurfaces with ``--include-deferred``. Reversal is via ``undefer``
(idempotent: clearing already-clear state warns but exits 0).
Predates the ``completed_at: "deferred:<ts>"`` workaround; ``recompute_statuses``
auto-migrates the prefix to the new schema, so callers should never see
the old shape after one mutation.

## cmd_defer (formerly at line 7012)

-- queue / unqueue / queued --
``queue`` is the user-facing triage marker for "I'm pulling this off
the backlog and intend to work on it next" (e.g. "tomorrow I'm going
to queue x, y, z"). Orthogonal to ``status``: a queued node still has
``status: ready`` so ``fno backlog ready`` keeps surfacing it. The
kanban renderer reads ``queued_at`` separately and promotes the card
into the Now column (between ``claimed`` and the priority-driven
promotion rule).
Cleared automatically by ``cmd_done``; reversible via ``unqueue``.

## cmd_done (formerly at line 8491)

A query that is not a bare node id (title substring, bare hex, slug,
`next`) resolves through the same fuzzy resolver the rich surface uses
and then closes CANONICALLY on the resolved id: the close depth must
not fork on which resolvable form named the node (PR 1200 review -
a bare-hex close of an epic child skipped the cascade and stamp that
the prefixed form ran). The rich surface still delegates wholesale
(and resolves inside the delegate, so no resolve happens here);
an unresolvable query delegates so ITS error rendering answers.

## cmd_reopen (formerly at line 8893)

-- Step 2: the merged-PR gate (outside the lock, like cmd_done's) --
Through `resolve_merge_evidence`, the SAME resolver `cmd_done` uses, and
over ALL refs rather than the primary. That is what makes this the same
gate inverted rather than a similar-looking one: a node can close on a
merged `additional_prs` entry while its primary `pr_number` sits closed
and unmerged, and a reopen that only queried the primary would permit
exactly the case the refusal exists to catch. It also carries the node's
`cwd`, which is how a foreign-repo ref reaches the right repository
instead of resolving PR #N in whatever checkout happens to be current.

## _reconcile_once (formerly at line 9604)

A binding is a MERGE-time close signal, never a promise on
an OPEN or CLOSED-unmerged PR - the caller could later
abandon or close it unmerged, leaving a claimed node
holding a dead ref. The three real callers (the ritual,
fno do pr merge, the bare sweep) only ever reach this with an
already-merged PR; this guards a direct manual
`--pr-number` invocation against the same premature bind.
Only worth refusing (and reporting) when the body actually
names a trailer: a state-read blip on a PR with NO trailer
at all has nothing to bind either way, and the node this
run is closing almost always closes fine via the ordinary
ref-based scan below - flagging that as a refusal read a
fully successful run as failed (round-8 review fix).

## _reconcile_once (formerly at line 9733)

_pr_touch_ids refuses to repo-scope a bare-number ref match
when it cannot resolve OUR OWN repo (avoiding a false cross-repo
collision), so the scan below can silently see only trailer
claims and open ref-less nodes - the ordinary ref-stamped,
no-trailer close can go dark with nothing on stderr to say so.
Same condition leg_stamp already warns about before it calls
this same command with an explicit --repo; surface it here too
for a caller (a bare `fno backlog reconcile --pr-number`) that
never resolves one itself (round-11 review fix, ).

## _reconcile_once (formerly at line 9753)

Auto-bind closure claims for every OTHER merged PR this sweep just
discovered on its own (). --pr-number above covers the two paths
that already KNOW the PR number (the post-merge ritual, `fno do pr
merge`); a THIRD path merges with no caller ever naming a number at
all - an operator merging in the GitHub UI, or a king's automation -
and is caught only later by this bare sweep's own forward/reverse scan.
Full sweep only: a --pr-number call is scoped to its own PR above, and
discovering totally unrelated merged PRs is this bare sweep's job (it
auto-fires often), not a side effect of every single merge event.

## _reconcile_once (formerly at line 9778)

Bounded two ways, not because more discovery is wrong, but because
each entry costs one sequential `gh pr view` (up to
GH_QUERY_TIMEOUT_S each) - an unbounded loop on a backlog with many
stale closeable records turns every SessionStart sweep into a
serial gh-latency tax. A COUNT cap alone still lets a degraded gh
(network blip, auth stall) burn up to count*GH_QUERY_TIMEOUT_S -
20*30s = 10 minutes - turning a hook that used to fail fast into
one that hangs. A wall-clock budget bounds that regardless of the
count cap. A later sweep (SessionStart auto-fires often) picks up
whatever this run dropped either way.

## cmd_unarchive (formerly at line 11491)

TWO locked passes, and the split is the whole safety argument.
`archive` writes the archive inside its mutator because archive-FIRST is
safe for it: a crash leaves a duplicate. Inverting the verb inverts the
safe order, and the mutator cannot express it - `locked_mutate_graph`
persists the returned entries only AFTER the mutator returns, so an
archive shrink written inside the mutator lands BEFORE the working graph
and a crash between them loses the node from both files. That is the one
outcome neither verb may produce, and doing it there quietly guaranteed
the ordering the comment claimed to prevent.
So: pass 1 adds the row to the working graph and persists it. Pass 2 takes
the lock again, re-reads the archive fresh (never a list read before the
first write, which a concurrent `archive --apply` could have grown), and
drops the row only after confirming the node is really live. A crash
between the passes leaves a duplicate, which read-through resolves working
-first and the next sweep dedupes.

## _do_intake_multi (formerly at line 11781)

Unlike the per-file refusals inside the mutator (which skip one file and
land the rest), a surface-less plan refuses the WHOLE batch here, before
any write: the rule is knowable up front and a half-landed batch is what
the caller would otherwise have to unwind by hand. Plans already bound to
a node are exempt on every roadmap: the mutator resolves them per file
with the accurate verdicts ('already intaked' same-roadmap, the
owner-conflict error cross-roadmap), and blaming one for the batch would
name a remedy - add file rows - that cannot fix the real blocker.

## cmd_supersede (formerly at line 12402)

Guard the death transition: a unit with live children cannot
be killed without orphaning them, and orphaning is a deliberate act.
Refuse unless forced, naming the children so the operator sees what
would strand under a dead unit. Gates on liveness, not type - the
`type` field is not maintained reliably enough to guard on (the epic
that prompted this was itself typed `feature`).
Use the canonical resolved id, not the raw `replaces` argument: an
abbreviated id (ab-9728) resolves via _find_node but would never
equal a child's full canonical `parent`, silently bypassing the guard.

## cmd_supersede (formerly at line 12450)

A pre-existing deferral stays: status precedence reads superseded
above deferred, so the park resurfaces if the supersession is
reversed.
Release anything that was shipping inside it (, sigma). Same
trap `cmd_remove` was fixed for, one step short of deletion: a
superseded unit will never merge, so `_strandable_contained_ids`
(which keys on completed_at) never heals its children, while
selection_guards keeps refusing them and the redirect keeps pointing
at a node that is not going to ship. Unbuildable, uncloseable, and
invisible to every sweep. Un-contained rather than closed: superseding
the unit is not a claim that its children shipped.

