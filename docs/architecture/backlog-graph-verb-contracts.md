<!-- style-exception: prose moved verbatim from cli.py docstrings to satisfy the shrink-only file budget; rewriting it here would edit history, not clarify it -->
# Backlog graph verb contracts

The long design prose behind the backlog graph's close, reopen, supersede,
archive, encounter, join, and selection helpers, moved out of the module so
the source file stays under its line budget. Each section is the full
contract of one helper; the code carries a one-line summary and points
here. Internal tracker ids from the original comments are dropped.

## _has_unmerged_open_pr

True when a node already carries a PR but is not yet closed (done) - i.e. work is in flight / in review, so it must NOT be re-selected for dispatch .

A node only leaves the ready pool at merge-and-close (completed_at set -> recompute_statuses derives status "done"). During the whole PR window pr_number is set but completed_at is None, so the status derivation still yields "ready"; this predicate is the missing selection-time guard, mirroring _live_claimed_node_ids - the PID-based node claim dies when the builder session exits, leaving no in-flight signal behind.

Excludes every not-done node that carries a pr_number, regardless of merge_status: an open unmerged PR (the originally-observed case), a merged-but-reconcile-pending PR (do not re-dispatch already-merged work in the close gap), and a closed-without-merge PR (un-dispatchable until pr_number is cleared - see the plan's known edges) all mean "not fresh ready work".

## _container_ids

Ids of nodes that are some other node's ``parent`` - i.e. epics/containers.

A container is never directly buildable: its work lives in its decomposed children, and it carries no PR of its own. So every work-SELECTION surface drops it from the candidate pool - `next`/`ready`/`--all-ready` build the leaves, never the box . Shared by `next` (_pick_ready) and `ready` (cmd_ready) so the two surfaces cannot drift; advance_dependents applies the same rule on the merge edge-following path.

A child whose ``contained_in`` is the parent ships INSIDE the parent's PR, so the parent is the delivery unit, not a box: an owner whose only children are its own contained subtasks still drains. Free children make the parent a box exactly as before.

No "keep the all-done epic selectable" exception is needed: an epic closes automatically via ``_cascade_close_parents`` on the merge that finishes its last child (uniform across projects), so it is already ``done`` - never a lingering ``ready`` container that selection would have to surface for closure. That replaces the old "walker closes the epic via next" path, which conflicted with never building a container.

## cmd_encounter

Record ONE encounter with this node, from this session, with evidence.

An encounter is a thing that happened, not an opinion. A poll count of N agents who like a node is unfalsifiable; N distinct sessions that each name what the node cost them is falsifiable against a transcript. That is why evidence is required, and why an agent vote needs a provable identity: a vote no transcript can vouch for is a poll. The one exception is the operator lane: on this single-operator machine the human may vote from a plain terminal by passing --operator, which votes under the stable ``operator`` voter key. That lane is a declaration, not proof - any session can pass the flag - so the record keeps the casting session's id whenever one is provable, and the demand read displays the agent/operator split instead of folding the two kinds together.

Read the signal with ``fno backlog demand``. This verb never writes rank and never touches a kanban column: the board stays the work order, and demand is a column the operator ranks FROM.

There is no correction verb: an encounter is a thing that happened and cannot be edited or withdrawn. An edit path would make the record deniable, which is the property this signal exists to prevent. A later correction is a ``fno backlog note``.

Exit codes are distinct so a probe can tell the refusals apart. 1 evidence (or an unknown node), 3 duplicate, 4 style, 5 unprovable identity. 2 is typer's usage error and is deliberately unused here: an identity refusal sharing a code with a mistyped flag is a refusal nothing can verify.

## cmd_demand

Show where agent encounters and operator priority DISAGREE.

Sorted by divergence, not by volume. A p0 with many encounters tells you nothing, because you already ranked it. A p3 or a never-dispatched node with many encounters is the whole point of the signal.

Operator votes count in ``enc`` and remain visible in the split: ``enc 5 (4a/1o)``. Provenance is displayed rather than excluded, so the reader can distinguish agent demand from the operator's own vote.

`dispatched` is how many of the encountering sessions were also sent to this node. Read it beside the count: `enc 12, dispatched 12` is one king that fanned out, while `enc 3, dispatched 0` is three sessions that hit the node while doing something else.

A read, and only a read. It never writes rank and never moves a column; the board stays the work order and this is a column you rank FROM.

## _joined_open_candidates

The transient joined selection model: ``list_open`` exactly once, one sidecar load per OPEN id, never the closed history (AC4's bound: 48 live rows, not the ~2,000 inactive archive).

A transient render for selection filters and ranking only - never persisted, never a shared convenience record (locked decision 3). The rung is DERIVED at read time from seam-carried evidence (open + PR = in review; open + linked plan = ready; plan-less = idea, the cold-dispatch admission): no stored status flag crosses the seam, and footnote's mutation-stamped rungs (deferred/superseded) cannot exist on an externally-owned item. Footnote-minted scoping pins (project, roadmap_id, mission_*) carry no external equivalent and stay absent: a scoped request over them is honestly empty, and `--project`/-A detection degrades to "all open candidates" exactly as a join with no project column must. Priority/rank/created_at ride the selection projection, so footnote's ranking (applied by _pick_ready AFTER this join) is tracker-owned on both backends - the same sort key picks the same winner (AC5).

## cmd_join

Spawn execute-waves joiners into a held node's worktree .

The node must hold a LIVE node claim with a bound plan: join resolves the holder's worktree from the claim, computes the plan's ready-graph width, and spawns ``/fno:execute waves <plan>`` workers INTO that worktree as visitors - they take task claims under their own roster names and never the node claim. One worker per wave band when the plan carries bands (highest band first, the lead; each lane resolved per band), else ``min(workers, width - 1)`` shapeless workers; the width rule caps the count either way. An omitted --workers derives the ask from the node priority and the plan's highest wave band instead of asking for one joiner. Prints one JSON receipt: ``{"node", "worktree", "width", "priority", "band", "workers", "workers_source", "spawned", "lead", "lanes"}``.

Refusals: exit 2 nothing to join (no live claim), exit 3 width 1, exit 4 no usable bound plan, exit 5 already joined (live j-<node>-* workers).

## cmd_session_add

Stamp a node with a lifecycle phase record (idempotent, append-only).

Identify the node by NODE (id/slug/hex) or by ``--pr-number <n>`` (the unique PR-linked node) -- exactly one of the two. With ``--pr-number`` and no ``--repo`` the verb resolves the current checkout's slug itself, so a caller needs no conditional flag . Harness + session id default to the ambient session identity; with neither an env marker nor an explicit flag the stamp is skipped and a warning names the node/PR and phase (provenance is never invented, AC2-ERR). Exit 0 on append or duplicate; exit 2 on missing identity, an unresolvable NODE, unknown phase, or bad input. A ``--pr-number`` that maps to zero or several nodes is a best-effort SKIP, not an error: it warns (naming the candidates) and exits 0, because refusing to guess is the designed outcome and a caller must not log it as a failure (AC3-ERR).

``--require-session`` and ``--guard-plan`` are the honesty guards an unattended caller (finalize's do-provenance backstop,) needs: a stale manifest in a reused worktree or a plan claiming another node must not mis-attribute work on an append-only record. Both skip with exit 0 and one named reason, because a guard skip is a designed outcome the caller must not log as a failure.

## cmd_remove

Delete a node from the graph permanently. This verb exists and works.

It had no docstring until 2026-08-11, which is why `fno help backlog --all` printed its name against an empty description and read as a stub. An agent consequently ruled that no delete verb existed and made that the load-bearing reason for a decision, and another project kept 23 nodes it believed un-file-able. Hence this paragraph: the verb's own help is the one place a caller asking "can this node go away" will actually look.

A HARD delete, unlike ``archive``, which moves the node to ``graph-archive.json`` and keeps it readable. Prefer ``archive`` for shipped work, ``supersede`` when something replaced it, and ``defer`` when it is merely not now. Reach for ``remove`` on a duplicate, a test artifact, or a node filed by mistake - the cases where the record itself is the noise.

Repairs every edge that pointed at the node, because nothing else can once the node is gone: drops it from every ``blocked_by``, from the symmetric ``related`` lists, nulls a dependent's ``source_node_id`` rather than leaving a dangling string, and releases contained children (the reconcile heal deliberately skips a MISSING owner, so an orphan there is permanent).

Refuses when other nodes name it as a blocker, listing them, since removing it silently unblocks work whose real dependency never landed. ``--force`` confirms that trade.

## cmd_pick

Interactively manage the backlog queue via fzf with live marker updates.

Pressing keys updates the marker in real time via fzf's reload action. So pressing ``q`` on a ``[ ]`` row flips it to ``[Q]`` in-place; pressing ``u`` on a ``[Q]`` row flips it back to ``[ ]``.

q queue this row -> marker becomes [Q] u unqueue this row -> marker becomes [ ] space toggle this row -> marker flips o open plan in Obsidian (idea rows: no-op) Enter commit all pending marker changes atomically Ctrl-C cancel; no marks land on the graph type fuzzy-filter the visible rows

Markers reflect the effective state INCLUDING pending changes. Latest mark per row wins, so you can change your mind by pressing the opposite key. Idempotent: re-queuing an already-queued row is a no-op on commit.

## _clear_completion_fields

Undo :func:`_apply_completion_fields`. Shared by ``reopen`` and its cascade.

It lives beside its forward counterpart for that function's own stated reason: the close paths share one helper so they cannot drift, and an open path that drifts from the close path is the same defect pointed the other way.

Clearing ``completed_at`` IS the status change - ``recompute_statuses`` derives the node's underlying state from its absence, exactly as it derives ``done`` from its presence.

``completion_note`` is cleared rather than overwritten with the reopen trail, and this is load-bearing rather than tidy: ``_cascade_close_parents`` only writes its ``auto-closed:`` note when that field is EMPTY, so an epic carrying reopen prose would never be recognizable as cascade-closed again, and a later reopen would leave it done under a live child. The trail goes in dedicated ``reopened_at`` / ``reopened_reason`` fields instead.

Four things are deliberately NOT restored, because reopening a node is not

rewinding time:

- ``merge_status`` stays. It records that GitHub confirmed a merge, which is

still true after a reopen; clearing it would erase a fact to express an opinion.

- ``cost_usd`` / ``cost_sessions`` stay. The spend happened.

- ``locked_by`` / ``locked_at`` stay null. ``done`` cleared them, and

inventing a holder here would give the node a claim no lockfile backs; claims are acquired by ``fno do target init``.

- ``deferred_at`` / ``queued_at`` stay null. ``done`` cleared those too, and

re-parking is ``defer``'s job - the same policy ``cmd_unsupersede`` applies to un-containment.

## _cascade_close_parents

Close ancestor epics whose children are now all complete .

Called inside the close mutator right after a node's completion fields are set. An epic is a container with no PR of its own - its work IS its decomposed children - so it is "done" exactly when all of them are. Walking UP the ``parent`` chain, each ancestor whose children all carry ``completed_at`` is closed too (and tagged with a completion_note so the PR-less close is self-explaining), continuing to the grandparent.

This is the closure path that lets epics be excluded from build-SELECTION everywhere (`next`/`ready`/advance_dependents never dispatch the box): the box closes itself off the merge event that finishes its last child. It fires on every close path (done + reconcile) since each calls this after ``_apply_completion_fields``, and it is uniform across projects because it follows the parent EDGE, not a project filter - so a cross-project parent closes on the same merge that completes its last child.

Idempotent: an already-done or missing ancestor stops that branch. The walk is depth-capped against a malformed parent cycle.

## _release_parented_children

Clear ``parent`` on the owner's non-done children; return the ids freed.

The membership-axis sibling of ``_release_contained_children``. That helper covers ``contained_in`` (delivery: ships inside this PR); this one covers ``parent`` (epic membership). A permanently dead unit's children would otherwise stay parented to it: any one later revived (undeferred or unsuperseded) then hits the dead-ancestor selection guard and strands - the exact state this release exists to prevent.

NON-DONE children only. ``completed_at`` is the one truly terminal marker (done never reactivates), so a shipped child keeps ``parent`` as history. Live, deferred, and superseded children can all return to dispatch, so all get cleared: leaving a deferred child parented to a dead unit would strand it the moment it is undeferred. This is the gap a release keyed on liveness alone misses - the supersede guard refuses only over currently-dispatchable children, so deferred/superseded children pass it and must be released here.

Lives in ``cmd_supersede``, not the shared release helper, because ``cmd_defer`` also calls that helper and defer is a pause (undefer exists): a deferred epic's children must keep their membership through the pause. Supersede is permanent, so only it orphans.

Nothing re-parents on ``unsupersede``: re-adoption is decompose's job, the same policy the contained release states for un-containment.

## _cascade_close_contained

Close every node that shipped inside ``node_id``'s PR (task 1.5).

Called inside the close mutator right after a delivery unit's completion fields are set. A node carrying ``contained_in`` was folded into that unit by ``decompose ... adopt:``; its work rides the unit's PR, so it has no PR of its own and ``scan_merge_drift`` - which only ever returns nodes carrying a PR - can never see it. Before this, dispatch and cost had each learned to read containment and completion had no inference at all, so a contained node stayed open forever behind a merged PR.

Deliberately NOT the inverse of ``_cascade_close_parents``. That one closes a parent when its last child lands (bottom-up, conditional on the siblings); this closes children off their owner's merge (top-down, unconditional). One level only: containment is a direct relation to the node that owns the PR, not a chain, so a node contained in a contained node is a shape decompose cannot produce.

Three deliberate omissions, each load-bearing:

- No ledger rollup. Reusing the done root's path would hand each contained

node the same plan's cost and re-introduce the very triple count task 1.4 just removed. ``cost_usd`` stays None; the note is what makes that null read as located rather than missing.

- No ``merge_status``. The field means "GitHub confirmed THIS node's PR

merged" and a contained node has no PR, matching how the PR-less epic cascade leaves it unset.

- No auto-continue dispatch. See the AC6 note in

``tests/unit/test_reconcile_cascade.py``: a contained node is not a legal ``blocked_by`` target, and fanning one merge into N dispatches would scale with how finely an epic happened to be decomposed. The close still re-arms any dependent for the next selection pass.

Idempotent: an already-closed node keeps its own completion and note, so a child that shipped its own PR is never relabelled as contained cargo. Reconcile runs on every SessionStart, so this matters more than once.

The note is built BEFORE any mutation and nothing fallible runs between a node's ``_apply_completion_fields`` and its note. The caller treats a raised cascade as a warning and keeps the delivery unit's close, so anything that can throw mid-loop leaves nodes closed with no note - done, with the reason they are done missing. Cheap to arrange, and the alternative is a state no reader can interpret.

## _stamp_and_graduate_plan

Best-effort: stamp a plan ``shipped`` (when a ship URL is known) then graduate.

The completion path (``done``/``reconcile``) closes a node because its PR landed. ``graduate`` ALONE is a no-op on a plan that never went through target's ship gate: ``cmd_graduate`` returns early unless ``status`` is already ``shipped``, so a never-stamped plan's frontmatter would never record the ship . When a concrete PR ``url`` is available we first ``stamp`` the plan (sets ``shipped_at`` + ``status: in_review`` + records the URL and session id) and THEN ``graduate`` (flips ``shipped -> done`` once the URL count is met). Without a URL we fall back to graduate-only - the prior behavior - rather than assert a ship we cannot evidence (e.g. a forced close on an advisory node with no PR).

Returns True when a stamp/graduate actually ran successfully (the relevant verb exited 0); False when the run failed. Non-fatal: every failure warns and returns False, never raising, so a node close is never aborted by a stamp problem.

Shared by ``done`` and ``reconcile``. The stamper is the in-package ``fno.plan._stamp`` module, run under the same interpreter as fno, so it resolves whether the package runs from the repo (editable install) or a uv-installed venv.

## _set_expected_count

Authoritatively write expected_url_count=count onto a plan's frontmatter.

Used by ``decompose`` so a shared epic-decomposition doc graduates only after all N group PRs ship, not after the first. Runs the in-package ``fno.plan._stamp`` ``set-expected`` verb (the same sys.executable pattern as ``_graduate_plan``) to keep plan-frontmatter I/O in its single owner and this graph CLI graph-agnostic about frontmatter format.

Returns ``(status, detail)`` where status is a ``SetExpectedStatus``:

- ``"ok"`` - the count was written.

- ``"skipped"`` - benign: the count could not be written for a reason that

PROVABLY does NOT create the early-graduation risk: the base doc does not exist (set-expected exit 3). target also cannot stamp the doc at ship time, so it never graduates early. Mirrors ``_graduate_plan``'s best-effort, non-fatal philosophy. The caller proceeds silently.

- ``"failed"`` - a real risk that must be surfaced: either the module RAN

and reported a write failure on a doc it could read (e.g. malformed frontmatter; set-expected exit 1/2), OR the spawn itself raised. A spawn failure is INDETERMINATE - unlike an absent doc it does not prove the doc is unstampable at ship, so it could mask early graduation; the caller surfaces it as a loud, actionable stderr warning.

The caller never rolls back the graph and never exits non-zero on any of these outcomes (group nodes are the source of truth, and a non-zero exit would break pipelines that call decompose for a best-effort stamp).

## cmd_done

Mark a node complete.

Sets ``completed_at`` to an ISO timestamp; ``recompute_statuses`` derives ``status: done`` from that field and unblocks any dependents.

Before mutation, a gh cross-check verifies that at least one referenced PR is MERGED (: graph done = merged, uniformly). An OPEN PR is NOT closing evidence - the node is awaiting merge and closes on the actual merge via reconcile / merge-triggered advance. CI state is irrelevant to the close decision.

The rich completion surface (--backfill, --force-overwrite, --pr-number, --pr-url, --link, --note, title/branch query) ported here from the retired root `fno done` spelling : those paths delegate to the implementation that already owned them, so the old spelling forwards argv-verbatim onto this one.

Exit codes: 0 success (node closed) 1 validation error (bad id, node not found) 2 usage error (--force without --reason) 3 gh cross-check refused: CLOSED-unmerged / UNKNOWN, no merge evidence (retryable when the PR merges; walker treats this as Parked) 4 gh outage: subprocess failure / timeout / parse error; retryable 5 awaiting merge: PR OPEN, not merged; node stays in_review (success-shaped; close lands via reconcile/advance at merge) 6 promise unmet: plan promised work that has not all shipped (multi-wave with no assertion, a failed close_probe, or fewer merged ships than expected_url_count). Use --force --reason to record a deliberate half-ship.

## cmd_reopen

Clear a node's completion, returning it to its underlying state.

Every other lifecycle transition had an inverse (``defer``/``undefer``, ``supersede``/``unsupersede``, ``queue``/``unqueue``); ``done`` was terminal with none, so a node closed in error was corrected by hand-editing ``graph.json``, which a PreToolUse hook forbids for good reason.

Refuses when a referenced PR is MERGED. That is ``done``'s gate inverted: the work is in main, and clearing the completion would make the graph assert that shipped work did not ship. The remedy is almost always to file the remaining work as its own node (``fno backlog idea``) rather than to reopen the record of the part that landed. ``--force`` records a deliberate reopen of shipped work, and is journaled as such.

Ancestor epics the cascade auto-closed when this node closed are reopened alongside it, since an epic is done exactly when its children are. An epic closed on its own evidence is left done and named on stderr, because reopening it would discard a judgment this verb never made.

Reopening does not reclaim, un-defer, or un-queue the node; see :func:`_clear_completion_fields` for what it deliberately leaves alone.

Exit codes: 0 success (node reopened), or a no-op warning on a node that is not done 1 validation error (bad id, node not found in the graph or the archive) 2 usage error (blank --reason) 3 refused: a referenced PR is MERGED. Use --force --reason to override 4 archived, or a gh outage. Both are retryable: unarchive first, or retry once gh is reachable. The node stays done either way

## _pr_touch_ids

Every node id ``_pr_number`` could possibly close: the trailer's own claims, any node already carrying ``_pr_number`` as a ref (stamped at creation, before this feature existed), plus every OPEN, ref-less node whose own ``cwd`` matches THIS repo's root.

The last group exists because ``scan_merge_drift`` passes this exact scope straight through to ``reverse_map_unstamped`` (its reverse branch-name-map pass), which is what closes a node whose session died before the pr_number stamp landed. Omitting ref-less nodes here would silently zero out that entire pass on every ``--pr-number`` call - the node's own branch names it, but a scope with nothing in it matches nothing. It is NOT free to include every such node graph-wide, though: the graph is a single store shared across every project on the machine, and ``reverse_map_unstamped`` fires one gh call per DISTINCT node cwd in its scope (bounded by ``REVERSE_MAP_BUDGET_S``) - an unscoped sweep pays a gh call for every other project's ref-less nodes too, on every ordinary merge in this repo. Scoped below by matching each candidate's own ``cwd`` against ``repo_root`` (this checkout's own root, cheap and local - no gh call) - never by comparing a resolved project-NAME string against the node's stored ``project`` field: that field is written by a completely different resolver (settings-based project detection at intake) that can drift from an independently-derived name, and a node created via ``fno backlog new`` before being claimed by a plan legitimately carries ``project: null``, which a name comparison would misread as "not this repo" even when its cwd matches exactly.

The graph is CROSS-PROJECT: a bare number match, scoped by nothing, would pull in a same-numbered PR belonging to a different repo's node. Scoped by repo like every other PR-matching path in this module - a ref with no parseable url is still accepted as best-effort, but an unresolvable OUR OWN repo refuses every number-based match rather than wildcarding it in, matching ``_find_pr_node_id``'s actual stance (it returns ``None`` outright when its own slug is unresolvable, rather than treating that as a pass for every candidate).

## cmd_archive

Sweep old terminal (done/superseded) nodes into graph-archive.json.

Dry-run by default: prints how many would move and why some are held back. ``--apply`` mutates under the graph lock (archive written first, then the working graph, so a crash duplicates rather than loses). Never archives a node an OPEN node still references through a hard edge (blocker, parent, supersede target). A SOFT edge (the open node's ``related`` peer or ``source_node_id`` origin) does not hold the target: the reference is stripped from the open side at apply time and the node leaves; the read-through fallback keeps its id resolvable.

Every run's receipt names all four held-back buckets plus the soft-edge strip count, and every run emits a ``graph_archive_swept`` event, dry-run included: a leg that runs daily and reports bare "ok" is indistinguishable from one that never ran - the count that matters is often the held-back one, not the moved one.

## cmd_unarchive

Move one node from graph-archive.json back into the working graph.

``archive`` is a bulk hygiene sweep with no way back, so a node swept early (or swept correctly and then needed again) could only be recovered by hand-editing, which a PreToolUse hook forbids. It is the fourth instance of the same shape as the missing ``reopen``, and the audit that produced this verb found it in ten minutes.

Write order mirrors ``archive`` inverted, for its reason: the working graph is written FIRST, so a crash between the two writes leaves a duplicate that the next sweep dedupes rather than a lost node. Read-through (``entries_with_archive``) already tolerates the window.

Refuses to guess: 0 moved, or a warning that the node is already in the working graph 1 the id is in neither the working graph nor the archive

## cmd_unsupersede

Reverse a supersede on ``node_id``. Idempotent in the safe direction.

Clears ``superseded_by`` and removes ``node_id`` from the replacer's ``supersedes`` list; any ``deferred_at`` park the node carried before the supersession survives, so the status recomputes back to ``deferred`` after the reversal. The plan doc is forced off terminal ``superseded`` (the forward-only projector will not leave a terminal on its own).

A node that is merely deferred (no ``superseded_by``) is left untouched: reactivating parked work is ``undefer``'s job, and clearing a deferral here would silently make deferred work dispatchable.

Reactivation is a separate verb from ``undefer`` on purpose: reviving a plan that another plan supplanted is a conscious act. ``recompute_statuses`` has always named this verb as the only route back from ``superseded`` - it just did not exist.

Does NOT re-contain or re-parent children released when the node was superseded: re-adoption is decompose's job, the same policy ``cmd_undefer`` applies to un-containment.

