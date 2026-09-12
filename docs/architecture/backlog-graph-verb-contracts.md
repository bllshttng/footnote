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

## cmd_note

Append a timestamped progress note to a node, and DELIVER it.

A worker reads its node ONCE, at dispatch. So a note appended after that lands in a store no consumer re-reads: the write succeeds, the author believes the finding is delivered, and nothing reports the gap. That pairing is the worst one, because a silent success is indistinguishable from delivery from where the author stands. Measured on 2026-09-08: of seven notes written across four nodes, six were hand-relayed by a separate mail send and one was not, and the missing one's later retraction WAS relayed - so a live worker received the retraction of a finding it had never received.

So the verb mails what it wrote. The worker chain runs for the node and again for its owner (``contained_in`` when set, else ``parent``), and the first arm that yields a live reader wins within a run:

- the live claim holder of ``node:<id>``. ``suspect`` counts as owned (TTL-unexpired, dead pid), because a suspect claim still belongs to its session.
- the node's graph bindings: ``locked_by_harness_session``, ``session_id``, ``locked_by``, each resolved to an ownership-live registry row. A worker can be live and bound to the node in the graph while holding no claim row at all; the claim is the weakest of the bindings, not the only one.
- every ownership-live registry row whose ``node`` field names the node, sorted by name.

The crown walk goes outward and stops at the first scope with a live crown: the node's own id when its ``type`` is ``epic``, then the epic (the owner's ``parent`` for a contained node, else the node's own), then the node's ``project``. Every scope resolves at send time by ``resolve_to_king``.

The author is matched by identity, never by name shape: an address that names a registry row is the author when the row's ``harness_session_id`` matches the sender's under ``session_identity_key``; the bare ``endswith`` match stands only for role-prefixed holders that name no row. The author is named in the receipt but never mailed its own note.

The body is a POINTER, never the note: the node id, the note's opening words, and the command to read it. A full body spends the 80-word rolling pair budget on the first send, and several notes share one 10-minute window.

The delivery lives in the VERB, not in ``append_progress_note``. The status-fanout adapter writes its ``task_done`` / ``run_summary`` stamps through the store function, so machine progress lines never mail: one note per finished task would spend every pair budget on traffic no reader asked for. A fact somebody chose to record is the case that needs a reader.

``--quiet`` is the deliberate silent annotation. Delivery is the default because the two failure modes are not symmetric: a forgotten flag costs a redundant mail, where a forgotten mail costs the finding.

Every outcome prints. A delivery prints ``notified <address> (<why>): <transport> <msg-id>``. When the author is the only bound reader, the note is written and one line names the binding: ``notify: you are the only reader bound to <id> (<why>); nobody else to tell``, exit 0. When nobody is bound, or a fault makes the bindings unreadable, the verb REFUSES BEFORE the append: nothing is written, stderr carries the refusal with every arm reading, and it exits 3 (a node that resolves to nothing stays exit 1). A resolution fault refuses for the same reason a vacant one does: neither can prove anyone would be told, and a note no reader would hear is a silent drop wearing a receipt. ``--quiet`` is the acknowledgment: it skips resolution, writes the note, and mails nobody. After the sends, exit 0 needs at least one ``notified`` receipt; when every receipt is ``notify FAILED`` or ``notify UNCONFIRMED``, the note stays written, one summary line goes to stderr, and the exit code is 4. A failed send - a budget refusal included - prints ``notify FAILED`` on stderr. Each send is bounded at 30 seconds because a live inject waits on the recipient's per-agent flock and one measured run wedged past 150; an unanswered recipient prints ``notify UNCONFIRMED`` on stderr, which says the delivery is unknown rather than done - so an UNCONFIRMED send is not a positive marker, and the exit-4 summary says to check before re-sending.

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

Two receipt additions close the "did the sweep stop?" gap. The receipt ends with a ``last sweep:`` line dated from the archive's newest ``archived_at`` stamp (the marker a sweep writes when it moves rows), because the archive's newest ``completed_at`` always trails today by the age gate and reads as a stall to anyone comparing it to the clock. And ``--apply`` first retires stale postmortem receipts: open ``idea`` rows minted by the retro postmortem pass (the ``retro-triage source_pr=None`` trailer in their details) that no human touched within 30 days are closed as ``done`` with a ``retired: stale-postmortem-receipt`` marker. Queued, claimed, deferred, and any non-idea row is untouched. Retirement composes with the sweep: the closed receipt becomes an ordinary terminal row the age gate removes from the working graph a month later. Without the rule, finalize's per-session completion evals pile up forever - 61 open receipts once produced 750 of the graph's near-duplicate pairs.

## cmd_unarchive

Move one node from graph-archive.json back into the working graph.

``archive`` is a bulk hygiene sweep with no way back, so a node swept early (or swept correctly and then needed again) could only be recovered by hand-editing, which a PreToolUse hook forbids. It is the fourth instance of the same shape as the missing ``reopen``, and the audit that produced this verb found it in ten minutes.

Write order mirrors ``archive`` inverted, for its reason: the working graph is written FIRST, so a crash between the two writes leaves a duplicate that the next sweep dedupes rather than a lost node. Read-through (``entries_with_archive``) already tolerates the window.

Refuses to guess: 0 moved, or a warning that the node is already in the working graph 1 the id is in neither the working graph nor the archive

## cmd_unsupersede

Reverse a supersede on ``node_id``. Idempotent in the safe direction.

Clears ``superseded_by`` and removes ``node_id`` from the replacer's ``supersedes`` list; any ``deferred_at`` park the node carried before the supersession survives, so the status recomputes back to ``deferred`` after the reversal. The plan doc is forced off terminal ``superseded`` (the forward-only projector will not leave a terminal on its own). The verb prints the reason it clears and any plan ruling against the node.

A node that is merely deferred (no ``superseded_by``) is left untouched: reactivating parked work is ``undefer``'s job, and clearing a deferral here would silently make deferred work dispatchable.

Reactivation is a separate verb from ``undefer`` on purpose: reviving a plan that another plan supplanted is a conscious act. ``recompute_statuses`` has always named this verb as the only route back from ``superseded`` - it just did not exist.

Does NOT re-contain or re-parent children released when the node was superseded: re-adoption is decompose's job, the same policy ``cmd_undefer`` applies to un-containment.

## _encounter_provenance

Model/effort provenance for an encounter record, or nothing.

    Read from the harness's own env, and only for the harness that owns those
    names. The variables survive a fork, so a codex session launched from a
    claude shell inherits them; writing them onto a codex vote would attribute
    it to a model that did not cast it, which reads as measured. A harness with
    nothing to report omits the keys rather than writing a plausible default.

    The two names are not equally first-party. CLAUDE_EFFORT lives in the
    claude binary's own namespace, so its presence is claude's doing.
    ANTHROPIC_MODEL is a provider SDK name a shell can export for unrelated
    tooling, so it counts only when ANTHROPIC_BASE_URL is exported beside it:
    the launcher that owns the model string owns the endpoint and sets the
    pair, and a lone model string says nothing about the route this session
    actually ran on.

## _cascade_reopen_parents

Reopen ancestor epics the cascade auto-closed. Returns (reopened, warned).

    The inverse of :func:`_cascade_close_parents`, and not a refusal, because a
    done epic with a live child is not a risky state to correct - it is an
    inconsistent one. The epic's work IS its children; one of them is open again.

    The judgment call is WHICH ancestors. An epic closed by the cascade carries
    the ``auto-closed:`` note :func:`_auto_closed_note` wrote, so reopening it
    just restores what the cascade would compute today. An epic closed WITHOUT
    that note was closed on its own evidence - a real PR, an operator decision -
    and silently reopening it would discard a judgment this verb never made. So
    those are left done and NAMED, which is the refuse-and-say-why rule applied
    to a case where either silent choice is wrong.

    Walks up under the same 64-deep cap the close path uses, for the same reason.

## _strandable_contained_ids

Open nodes whose delivery unit is ALREADY done - closeable right now.

    ``_cascade_close_contained`` only fires while a unit is being closed, and
    ``scan_merge_drift`` never returns an already-closed unit, so a node that
    became contained AFTER its owner shipped is reachable by neither. That is
    not hypothetical: re-running an `adopt` spec back-fills ``contained_in``
    onto a node adopted by an older fno, and if that node's owner has already
    merged, the back-fill removes it from selection (the containment guard) with
    nothing left that would ever complete it - visible, unbuildable, never done.

    Read-only. The same self-heal role ``_strandable_epic_ids`` plays for
    all-done epics, and for the same reason: a state the forward path now
    prevents still has to be swept out of graphs that already carry it. Once
    migrated this returns empty and the sweep is a no-op.

## _starvation_receipts

Classify why each ready-ish in-scope node was NOT selected (G1 receipts).

    Zero-silent-starvation (, epic Success Definition 2): when ``next``
    returns null but buildable-looking nodes exist, name why each was excluded
    so an operator is never left guessing. Reasons: ``plan-less`` | ``container``
    | ``claimed`` | ``design`` | ``quarantined`` | ``dead-ancestor``. A node
    genuinely in review (open PR) or committed to a batch is not starved and
    gets no line.
    Pure over the injected ``claimed`` set + ``now`` so it is unit-testable.

    Mirrors ``_pick_ready``'s SCOPING (project, parent subtree via ``scope_ids``,
    ``--mission``, ``--roadmap-id``) so a scoped request that returns null never
    explains itself with an out-of-scope node (codex P2). Only the exclusion
    filters differ - that is the whole point of the receipt.

## _merge_unconfirmed

True when a done child carries a PR that GitHub never confirmed merged.

    A child reads ``done`` the moment ``/target`` finalizes, not when the PR
    merges, so at a wave gate the graph can say a dependency landed while its
    branch is still open. ``merge_status`` is written only by
    :func:`_apply_completion_fields` when a caller resolved MERGED from gh, so
    its absence is exactly "nobody confirmed this".

    Deliberately NOT called "unmerged". The absence has two explanations - the
    PR is genuinely open, or it merged through a path that never stamped the
    field - and asserting the first from the absence of the second is the
    absence-as-evidence trap. The caller's wording, and ``--verify-merges``,
    keep that distinction. Measured 2026-09-01: 16 of 444 done nodes carrying a
    PR were in this state, spanning 2026-04-28 to 2026-08-26.

## resolve_promise_evidence

Decide whether a node's plan promised work that has not all shipped.

    Fires ONLY on an explicit declaration - a plan that declares neither
    ``close_probes`` nor ``expected_url_count`` closes exactly as it does today.
    Inferring "multi-wave" from ``## Wave N`` headings was rejected: the common
    case (one .md == one PR == one node) uses waves as internal structure and
    would false-positive identically to a half-ship, parking every such node on
    autonomous /target. Coverage grows as /blueprint stamps ``expected_url_count``
    going forward, so nothing retroactively parks.

    Three conditions, first refusal wins. D reads the carve-out ledger and is
    independent of the plan; B and C read the plan at ``node["plan_path"]``:

      D. Unharvested deferred carve-outs. The project ledger
         (``.fno/carveouts.jsonl``) still carries a ``deferred`` carve-out -
         declared scope that did not ship. It must become a node (the retro
         harvest files and consumes it) or be force-overridden. ``oos-bug`` and
         ``backfill`` carve-outs do NOT block (genuine discovery, filed by the
         later harvest); both land in the same ledger and look identical, which
         is why a deferred item can merge away unnoticed. Checked first and independent of the plan so a close is held
         even when the plan is absent or unreadable. Scoped to the closing
         node's OWN rows via the ``node`` field stamped at capture time
         (``find_held_node`` proven ownership): a row filed by another node's
         session must not hold this close open (one unrelated
         carve-out was blocking every close in the repo). Unattributed rows
         (legacy, ambient shell, harness without a session id) block nothing
         at close time; they stay visible via ``fno backlog carveout list`` and the
         retro sweep, which are the repo-wide backstop.
      B. Outcome probes. Any ``close_probes`` entry exits non-zero. Probes are
         delegated to ``fno-agents probe-run`` (the same runner the loop uses for
         ``done_probes``); a declared gate that cannot be evaluated fails closed.
      C. Ship count. ``expected_url_count: N`` (N >= 2) and fewer than N of the
         node's PR refs are MERGED. The right check for multi-repo / split
         deliveries. Refs are de-duplicated by PR number, so one PR listed
         twice is one ship and can never satisfy two.

    A gate that only fires on an explicit promise cannot false-positive, which
    is the reason the count is written at blueprint time rather than inferred
    afterward. Fails open on an absent/unreadable plan or unparseable
    frontmatter: a stale ``plan_path`` must not wedge a close, but the warning
    names the unreadable path so the gap is visible, not silent.

    Three outcomes, not two. ``ok`` is POSITIVE evidence and is the only one
    that closes; every close boundary reads ``verdict.satisfied``, never a
    negative test against one refusal name. ``promise_unmet`` is a policy
    refusal the operator resolves (exit 6). ``promise_unknown`` is a retryable
    read outage under condition C: a declared count with some refs unreadable
    is unconfirmed in BOTH directions, so the node stays open and the next
    sweep retries (exit 4, the merge gate's own outage code). Reading that
    outage as ``ok`` is what closed declared multi-ship nodes on the strength
    of a gh timeout. A NON-retryable read failure stays ``promise_unmet``: it
    is a policy problem (bad credentials, a stale ref) that retrying will not
    fix. A plan with no declaration never reaches condition C, so legacy
    behavior is unchanged.

## emit_session_satisfied_for_record

Emit a ``session_satisfied{source:"pr_merge"}`` event for the target
    session that owns a merged-and-now-closed node (Group 1).

    Today only an in-gate merge through ``scripts/lib/pr-merge.sh`` emits this
    signal, so an out-of-band merge (web button, bare ``gh pr merge``) leaves the
    owning session hot and the stop hook hard re-blocks it. After reconcile
    closes the drifted node, this hands the same auto-complete signal to the
    owning session.

    The event binds to that session via ``session_id`` + ``gate_state_hash`` (the
    md5 of the owning target-state.md at emit time), matching the stop hook's
    staleness check (``check_session_satisfied``). The defensive stop-hook probe
    (Task 1.2) is the backstop for when this emit is stale or never lands.

    Best-effort and non-fatal: returns the events.jsonl path on a successful
    emit, or None when there is nothing to satisfy (no cwd, no live state file,
    already-COMPLETE session, missing session_id) or any failure. A failure here
    must never abort the reconcile close.

## emit_gate_escape_for_record

Tier-1 auto-emit: a ``gate_escape{reason:dead-bot}`` when
    reconcile closes an out-of-band-merged node whose required review bot never
    reviewed.

    Boundary (the #222 rule - the load-bearing correctness surface):
      - required_bots empty          -> NOT an escape: a no-required-bots repo
                                        self-merging a green PR is normal (AC2).
      - every required bot reviewed  -> NOT an escape: the gate was met; only
                                        the merge happened out of band (AC2b).
      - some required bot never reviewed -> escape: the loop should have waited
                                        for / resolved that review (AC1).

    Lands in the CANONICAL events log (``events_path`` overrides for tests) so a
    closed node's telemetry outlives its worktree and retro aggregates one
    coherent log. Dedup on (pr, reason) (AC4). Telemetry fails OPEN: any failure
    logs a durable emit-failure line beside the events log (AC7) and returns
    None, never raising - the emit must never abort the reconcile close (AC5).
    Returns the events.jsonl path on a successful emit.

## scan_merge_drift

Find open nodes whose PR has merged outside the ship gate.

    Returns one record per open node that resolves to a MERGED PR, plus
    records flagged with ``error`` for nodes whose PR state could not be
    resolved. Nodes whose PRs are all still OPEN (or closed-unmerged) yield no
    record - they are not drift. ``node_id`` restricts the scan to a single
    node (a str) or a set of nodes (an iterable of str) - e.g. every node one
    specific PR's exact trailer names, so a ``--pr-number`` call scans only
    what that PR could possibly touch instead of the whole graph.
    Tests inject a ``query`` stub to avoid shelling out to gh.

    A second pass (``reverse_map_unstamped``) covers open nodes with NO PR ref
    at all - a session that died before the node<->PR stamp - by matching the
    node id against merged branch names. ``list_merged`` is injected in tests.

    Cost bound: both listing scans group candidates by resolved git common dir, so the worktrees of one repo share one ``gh pr list`` call, and the run's shared ``_ListingCache`` makes it one open and one merged listing per repo per sweep; merge drift resolves a stamped number from those listings and pays the per-node query only for a number in neither.

    Worst-case graph staleness is therefore the 900s reconcile throttle (``scripts/lib/reconcile-throttle.sh``), and it is a bound only while neither scan's 60s ``REVERSE_MAP_BUDGET_S`` fires: a firing budget defers the remaining repo groups to a later sweep, and nothing carries them forward until then.

    A listing row only answers a ref whose pr_url parses to the listing's own repo, or a ref with no parseable url (the cwd's listing scopes it); a number collision in a foreign repo never closes a node from this listing.

## pr_url_for_repo

The canonical PR url for the checkout at ``cwd``, or None.

    A writer must resolve the url at least as capably as the reader resolves
    the slug, so this shares ``resolve_current_repo_slug``'s origin-then-gh
    chain: a url-less ``pr_number`` names no repo, and PR numbers collide
    across repos.

    An absent ``cwd`` resolves against the invocation checkout - the caller is
    standing in the repo it is stamping. A ``cwd`` that IS recorded but no
    longer exists returns None instead: that is positive evidence the node
    belongs to another repo, and `backlog done`/`backlog update` can name any node
    in the cross-project graph, so falling back would stamp the running repo's
    slug onto a foreign node.

## _unharvested_deferred_carveouts

Unharvested ``deferred`` carve-outs on the node's project ledger.

    ``deferred`` blocks a close (declared scope did not ship); ``oos-bug`` and
    ``backfill`` do not (genuine discovery, filed by the later harvest). Both
    land in the same ``.fno/carveouts.jsonl``, which is why a deferred item can
    merge away unnoticed. The ledger is resolved from
    the NODE's project (``cwd``), not the ambient command repo: a cross-project
    close names a foreign node from this session, and reading the ambient
    ledger would both miss the foreign carve-out and let an unrelated local one
    block it. Falls back to the ambient canonical ledger when ``cwd`` is absent
    or unresolvable (the same-project close, the common case). Fails open on an
    unreadable ledger: a corrupt ledger must not wedge a close, and the retro
    harvest is the durable resolution path either way.

## reverse_map_unstamped

Close open nodes with NO PR refs by matching the id in a merged branch.

    A /target session that dies between ``gh pr create`` and the node<->PR
    stamp leaves an open node with no ``pr_number`` - invisible to the forward
    ``scan_merge_drift`` (which needs a ref to query). The branch convention
    (``branch_name()``) still carries the full node id, so one
    ``gh pr list --state merged`` per repo reverse-maps it. A unique headRef hit
    synthesizes the same MergeDriftRecord the stamped path emits (so the
    existing close path applies unchanged); an ambiguous hit (two merged PRs
    for one id) emits an ``error`` record naming both, never a guess.

    ``list_merged`` is injected in tests to avoid shelling to gh.

## node_cwd_in_repo

Does ``entry``'s own ``cwd`` sit inside ``our_root`` (or is it missing)?

    A missing/empty/non-string cwd can't be proven NOT this repo's, and
    there is no cost to treating it as in-scope here: ``reverse_map_unstamped``
    (the only caller of this scope) independently skips a missing cwd
    unconditionally before it would ever fire a gh call, so this branch
    changes candidate-set membership only - never the observable close/skip
    outcome for a no-cwd node.

    Module-level (not nested inside a caller) precisely so this predicate is
    unit-testable on its own, independent of the reverse-map machinery that
    happens to make its no-cwd branch behaviorally inert today.

## detect_reverted_nodes

(node_id, revert_pr_number) pairs to stamp ``reverted: true``.

    A merged PR whose title starts with ``Revert`` and whose body references
    a PR number carried by a not-yet-reverted graph node names that node's
    ship as reverted. Pure (no I/O) so tests need no gh.

    Matching is REPO-SCOPED: the graph is global across projects, so bare PR
    numbers collide. A candidate node must carry a ``pr_url`` in the SAME
    repo as the revert PR's own ``url``, a body qualifier
    (``Reverts other/repo#N``) must match that repo, and an ambiguous match
    (two same-repo nodes on one number) stamps nothing - the same
    ambiguity-resolves-to-nothing rule as the W1 backfill.

## verify_pending_supersessions

Verify predecessor cause surfaces against one merged PR's file set.

    Returns positive receipts for predecessors that remain pending. Surfaces
    govern the EVIDENCE stamp only: a predecessor's status went
    terminal from the superseded_by edge alone, so a receipt here never
    changes whether the row reads as live work - it records which declared
    paths a merged PR did or did not touch.

    ``evidence_complete=False`` says the file list is known to be short of the
    PR's real one. A surface missing from a truncated list is an absence with
    two explanations, so neither verdict is available: nothing verifies and the
    receipt names the truncation rather than blaming the surface.

## successors_owing_verification

Successor id -> successor node, for pending predecessors already owed proof.

    ``verify_pending_supersessions`` only ever runs while reconcile is CLOSING a
    successor. A successor that closed at any other moment - an earlier sweep, a
    hand-run ``fno backlog done``, or a supersede recorded against a node that
    had already shipped - never passes through that path. Its predecessors keep
    an unverified record, but the superseded_by edge already terminals their
    status, so the row never reads as live work while the evidence
    stays open.

    This finds those rows so the sweep can settle them against the evidence the
    successor already carries.

## select_lane_fill

Select up to ``max_lanes`` ready nodes, each collision-clean to dispatch.

    The parallel-mode (group 2) lane-fill selector. With
    ``claim=True`` each pick atomically acquires a dispatch-time lane slot (the
    group-1 primitive ``acquire_lane_slot``), so the concurrency cap is enforced
    by claim atomicity, never a counted snapshot (Locked Decision #7). Each
    returned node already holds a slot keyed ``parallel-lane:<id>``; the caller
    spawns one worker per node and the worker's ``target init`` reconciles that
    same slot (Locked Decision #8) rather than acquiring a fresh one.

    Collision-cleanliness is recomputed AFTER each claim from a FRESH ready-list,
    never a pre-claim snapshot: between two picks a peer may claim a node or a
    lane may finish, and re-querying reflects that. This is the "stops at
    a claimed head" hazard - selection must skip claimed heads across every
    domain. A node a live peer lane already holds is skipped so a
    not-yet-node-claimed lane is never double-dispatched. (Two dispatchers
    racing the SAME node are prevented upstream by the singleton
    ``walker:<root>`` claim, so this stays a single-dispatcher selector, not a
    distributed lock.)

    Domain is NOT a selection rule: the file-collision gate decides,
    so two same-domain nodes with disjoint surfaces co-schedule. What remains
    of domain is the annotation on an unevaluated candidate - see
    :func:`_classify_lane_candidate`.

    ``max_lanes == 1`` selects a single ready node: this is the retargeted
    active_backlog daemon's sequential
    fire-and-forget dispatch. ``max_lanes < 1`` returns ``[]`` with no
    side effects.

    ``claim=False`` previews the selection (which nodes WOULD dispatch) without
    holding any slot - the read-only mode, mirroring ``fno backlog next`` sans
    ``--claim``.

    ``claim=True`` assumes the caller runs under the singleton ``walker:<root>``
    claim (the dispatch context does): that serialization is what prevents two
    concurrent callers from both selecting the SAME node and each grabbing a
    distinct slot for it (which would inflate the cap - the group-1 primitive is
    idempotent only for a single caller's retries). It is NOT a standalone
    distributed lock; do not run two ``--claim`` selectors concurrently outside
    the walker.

## cmd_ready (the selection, served natively)

Which backlog nodes may be dispatched right now, and in what order. The decision lives in `crates/fno-agents/src/backlog_ready.rs` (`backlog_ready::select`), served by the keeper's `ready` verb; `fno backlog ready` and `fno backlog next` are clients. The verb accepts the filter flags (`project`, `all`, `roadmap_id`, `parent`, `mission`, `include_ideas`, `include_deferred`, `repo_root`), an optional `staleness_days` override, and an optional `entries` array - rows ride IN, the one decision answers both backends (the external-tracker branch feeds `_joined_open_candidates` through it). Without the override the keeper reads `config.backlog.staleness_days` from the `config.toml` beside the graph, falling back to the 21-day default. With no `claimed` array the verb resolves live claims itself and FAILS CLOSED: an unreadable claims root refuses the whole selection (the same contract `live_claimed_node_ids(strict=True)` gave the Python leg), never an empty set read as "nothing is claimed". The reply carries survivors plus per-node drops, first-filter attribution, with guard drops naming `dead-ancestor:<id>`, `design-stage`, `idea-stage`, `stale-quarantine`, `contained:<id>`, `no-difficulty`, or the hold verdict's guard reason; `advance --explain` renders from them (AC4). A missing `--parent` node refuses (exit 1, `ReadyParentMissingError` client-side). An unreachable keeper refuses selection: `fno backlog ready` exits non-zero naming the keeper, never a locally recomputed fallback (AC6). The `next` observer merge (`_with_observer`) still re-verifies observer rows through the Python `selection_guards`: a divergence detector over the reply, not a second selection leg. Under `next --claim`, the lock fields land on the graph entry the winner id resolves to - the selection rows are serialized summaries, not references into the commit snapshot. The reply is a JSON array on stdout. A line-prefix parser reads it as zero rows forever, indistinguishable from a quiet board.

## selection_guards

Return a skip-reason for a would-be-selected node, or None to select it.

    The single narrowing choke point shared by ``next`` selection (_pick_ready)
    and the converge readiness filter (_direct_dependents), so the two paths
    can never disagree about what is dispatchable. Guards, in order:

      contained: the node carries ``contained_in`` - its work ships inside
        another node's PR, so it is not a delivery unit and dispatching it
        would open a second PR for one plan. Returns ``contained:<owner-id>``,
        which names where the work actually went (``dead-ancestor`` would only
        say the subtree is dead). First, because containment is a fact about
        THIS node while every guard below reads its ancestors or its plan.
        Belt-and-braces: the write-site refusal already stops new
        double-bindings, so this is a read of a state that should not exist.
        It is also only HALF the coverage - selection_guards is autonomous-only
        (see the design-stage note below), so `fno do target init` carries the
        named-dispatch half. A guard on one of two reachable paths is
        decorative.

      dead-ancestor: any transitive ``parent`` in {superseded, deferred} - the
        subtree is abandoned, so building a leaf under a killed epic is wasted
        work. Returns ``dead-ancestor:<ancestor-id>``. A missing parent id ends
        the walk with no verdict (select normally). Depth-bounded + cycle-safe.

      stale-quarantine: a ready node with no movement signal older than
        ``staleness_days`` -> ``stale-quarantine``. The guard only EXCLUDES
        here; the reversible defer is owned by ``maintain --apply`` (guards
        never mutate the graph as a selection side effect - epic LD1/LD2).

      design-stage: the linked plan is still a design doc (frontmatter
        ``status: design``), so the node is planned but not blueprinted ->
        ``design-stage``. Only AUTONOMOUS selection routes through this
        function; an explicitly-named node dispatches from any rung, naming
        being the consent (epic LD8). This is what retires the
        keep-plans-unlinked workaround: linking a design doc now lands a
        visible-but-unarmed node instead of arming dispatch.

    Hold reads fail closed before the compatibility guard: an unreadable plan
    cannot prove a hold is absent. Every remaining guard stays fail-open (epic
    Errors): a read failure returns None and emits one loud stderr line.

## _join_node

Spawn width-bounded joiners into a held node's worktree.

    Resolves the holder's worktree from the LIVE ``node:<id>`` claim (never a
    manifest snapshot), computes the bound plan's ready-graph width, and
    spawns ``/fno:execute waves <plan>`` workers there via ``fno agents spawn
    --substrate thread`` - one per distinct wave band when the plan carries
    bands (highest band first, the lead; each lane resolved per band by
    ``_grid_lane_for``), else ``min(workers, width - 1)`` shapeless workers
    (joiner 2). Either way the width rule caps the count: the node holder is
    one of the width workers. ``workers`` is the requested joiner count;
    ``None`` (the CLI default) derives the ask from the sizing table - the
    node's priority against the plan's highest wave band - instead of
    defaulting to one joiner, which is the default that kept bare joins
    from ever being worth running. The brief rides TWO channels: the file
    ``<worktree>/.fno/join-briefs/<node>.md`` (reaches daemon-forked workers,
    which the waves.md joiner posture reads) and TARGET_BRIEF (reaches lanes
    that inherit the spawner's env, e.g. panes); a banded brief also carries
    the per-worker band table, the band's durable channel beside the
    best-effort ``FNO_WORKER_BAND`` env export. The spawned process exports
    FNO_WORKER_NAME from ``--name``, so each joiner's task-claim holder is
    its own roster name (the joiner 1 contract; where the env export cannot
    reach, resolve_task_holder reads the roster binding). ``model`` rides as
    an explicit ``--model``: a typed model with no vendor implication
    overrides a config-injected default whose lane would refuse.

    Returns the receipt ``{"node", "worktree", "width", "priority", "band",
    "workers", "workers_source", "spawned", "lead", "lanes"}`` - the three
    sizing inputs ride beside the requested count and where it came from
    (``derived`` | ``explicit``), so the receipt answers "why this many".
    ``lanes`` maps each spawned name to its ``band``/``harness``/
    ``model``/``sandbox`` (``enforced`` | ``overlapping`` | ``unevaluated`` |
    ``off``, from ``config.join.sandbox`` and the plan's band partition; plus
    ``"grid": "declined"`` when the grid declined that band
    and the joiner rides the caller's default lane). Raises JoinRefuse (exit
    2/3/4/5) on a precondition failure and SpawnError when the lead spawn
    itself fails; a non-lead spawn failure warns to stderr and shrinks
    ``spawned`` instead of aborting the join.

## _grid_lane_for

``(harness, model, decline_reason)`` the capacity grid picks for an UNPINNED spawn.

    On a pick the reason is ``None``; on a decline the harness and model are
    ``None`` and the reason names WHY.

    ``resolve_grid`` already returns ``(candidate, chain)`` whose last element
    is its terminal reason, and this function used to throw that away as
    ``_chain``. A bare ``(None, None)`` collapsed three different outcomes into
    one value - the caller pinned a model, capacity is unknown, or the
    inventory is empty - so a spawn site could not tell a deliberate pin from a
    config gap and fell through to the ambient default in silence.

    That is not hypothetical. With ``config.routing.models`` empty no band can
    ever match: ``resolve_grid`` appends ``grid=no-inventory-declared`` and
    returns no candidate, so EVERY banded plan buys the ambient fleet forever
    rather than momentarily. Observed on this node's own joiners, which both
    spawned on the most expensive lane while the receipt said only "grid
    declined, harness null, model null".

    The reason is for RECEIPTS, never for refusing. Routing degrades and never
    blocks a spawn (Locked 10), and an empty inventory is a config gap, not a
    capacity failure. Naming it is what makes it fixable.

    Deliberately ONE function rather than a two-tuple wrapper around a
    three-tuple worker. Tests monkeypatch this name, and an internal caller
    that reached past the wrapper would silently bypass every such patch - the
    first cut of this change did exactly that and two tests caught it.

    The receiving end of the difficulty deferral: dispatch callers resolve
    difficulty to nothing precisely so it picks the lane HERE, where live
    capacity is readable - the spawned argv's explicit --harness can never
    trigger the spawn-CLI grid. Only a fully unpinned spawn defers (an explicit
    model or provider stays operator authority); unknown capacity falls back to
    the caller's defaults (Locked 10: routing degrades, never blocks a spawn).
    Dispatch sites that make HARNESS-KEYED decisions before spawning (lane
    worktree placement) must call this first and thread the result through
    both decisions, so placement and spawn always agree.

## candidates

Union FTS5 and relatedness recall, ranked by relatedness score.

    ``entries`` is an optional narrowed pool for callers such as the filing
    gate.  The FTS cache still searches the graph bytes, then ids are filtered
    to that pool.  Relatedness is allowed below the filing floor so an FTS-only
    vocabulary hit remains visible with its measured score.  ``domain`` is the
    incoming node's own domain: relatedness grants a same-domain bonus, so a
    caller that knows it must pass it rather than let every query read as
    ``code``.

## assess

Assess one node without changing it or making an external mutation.

    ``pr_state`` is the caller's gh-verified answer for whether a PR number is
    merged.  Deferral clears every node-side completion field, so a shipped
    node that later expired is only provable through evidence that survives
    the defer: a verified merged PR, or files recorded in its text that still
    exist on disk.

## _spawn_worker

Dispatch a fire-and-forget autonomous ``/target`` (or ``dispatch_verb``) worker.

    Routes the substrate + the per-harness-normalized command through the shared
    resolver (``fno.agents.harness_map.resolve_dispatch``) instead of hardcoding
    ``--substrate bg`` + a ``/target`` f-string. ``harness`` (the selected
    provider record's ``cli``; ``None`` = config/``claude``) picks the substrate:
    ``bg`` for claude (the detached ``claude --bg`` thread that self-isolates into a
    worktree, never the pane default that would STALL a fire-and-forget dispatch),
    ``headless`` for codex/others. The workflow verb is DERIVED, not read:
    ``harness_map.resolve_effective_verb`` runs one
    conditional over the node's plan rung and difficulty. At INTAKE (plan rung
    ``none``) difficulty decides - ``low`` dispatches straight to ``/target``
    with no plan, ``medium``/``high`` blueprint on a frontier lane first and a
    separate ``/target`` builds the resulting plan. At RE-DISPATCH the linked
    plan's rung decides - ``idea``/``design`` keep ``/blueprint``,
    ``ready``/``in_progress``/``in_review`` advance to ``/target``. The node's
    stored ``dispatch_verb`` is audit input only: a target/blueprint-family
    value reconciles through the same table and the decision trail names both
    spellings, so correctness never depends on the blueprint session close
    having rewritten the graph field. An out-of-family verb (``/think``) keeps
    declared precedence; ``unreadable``/``done``/``superseded`` plan rungs and
    a planless node without a valid difficulty REFUSE. Selection drops such a
    node as ``no-difficulty`` on the UNSCOPED drain head, before any dispatcher
    reserves it, so one underivable row never spends a drain tick. A scoped
    call (``--parent``, ``--mission``, ``--roadmap-id``) is an enumeration:
    it still surfaces the row, so the epic fan-out decides per row, where one
    refusal costs a failed spawn and never the tick. ``verb_source`` keeps
    the RAW declaration state (declared / none-declared) beside the resolved
    verb, and a node dict missing the ``dispatch_verb`` key at all is a lossy
    selection projection: the spawn refuses before anything is spent. The stage
    table reads the DERIVED verb, so ``agents.profiles.blueprint`` reaches
    medium/high planless nodes and ``agents.profiles.target`` no longer
    acquires planning eligibility from plan absence (the ``_grid_lane_for``
    role floor and the ``spawn_defaults.grid_role`` split are gone).

    Merge posture stays a launcher decision, never baked into a node verb:
    a derived ``/target`` renders through the same rungs as the builtin (the
    default bakes ``no-merge``; ``config.auto_merge.grant`` omits the flag);
    reconcile stays an explicit
    ``/target [--no-merge] --reconcile <manifest> {id}`` template and bypasses
    the conditional. The agent is named
    ``target-<full-node-id>-<slug>`` (a ``-blueprint`` qualifier states a
    derived non-target phase; ``reconcile`` prefix when G4), and the cwd
    resolves to the node's recorded root (``--cwd``) or canonical main (``--fresh``).

    ``dispatch_account`` is a quota cutover's destination provider RECORD id, and
    it rides argv. The credentials never do: the spawn front door resolves the
    record and applies its overlay where the harness is exec'd. That matters
    because a non-claude record's overlay is a HOME override and footnote reads
    HOME to find its own state root, so an overlay merged into THIS wrapper's env
    would file the worker's registry row and claim under the account's home. ``extra_env`` is refused outright for any such key.

    Returns the spawn receipt's LAUNCH IDENTITY: the claude short_id, or for a
    codex thread the FULL harness_session_id (codex has no short id; a head-8
    slice is refused by shape - ruling d-513d9d22). Raises SpawnAlreadyRunning
    on a name-collision (a peer beat us in the boot window),
    DispatchResolveError on an unresolvable harness/substrate/verb (caught
    non-fatally by the caller), and SpawnError otherwise.

## _classify_lane_candidate

Classify one ready node for lane-fill. ``None`` = selectable, else a typed
    exclusion reason. Read-only (acquires no slot): the SINGLE per-candidate
    truth shared by :func:`select_lane_fill` (live) and :func:`schedule_shadow`
    (the read-only report), so the two can never disagree about why a node is
    held back. Duplicating this sequence into a second copy is the drift the
    codebase's path-uniqueness rule exists to prevent.

    Guard order: peer-lane, then collision, then domain. Domain was a proxy for
    "these will not collide"; the collision gate is the real measurement, so it
    runs first and domain NEVER excludes an evaluated candidate - two
    same-domain nodes with disjoint file surfaces co-schedule.

    Reason tokens (all stable, machine-readable):

      ``peer-lane``              a live peer lane already holds this exact node.
      ``high-collision:<id>``    a high-severity file overlap with in-flight work.
      ``unevaluated:no-surface`` the plan states no comparable file surface, so
        collision safety is UNKNOWN. This is a distinct class, not an exclusion:
        live dispatch fails open on it (dispatches anyway); the shadow report is
        conservative and serializes it with this diagnostic (plan Change 1).
        When the node's domain is already held (by a live lane or an earlier
        pick) the token carries ``+same-domain:<domain>`` - the domain tiebreak
        survives only as that annotation, so the report can still explain a
        serialized unknown. That subclass is the one behavior change inside the
        class: a held domain excluded such a candidate before the reorder and
        now it dispatches, loudly (select_lane_fill warns on the annotation),
        with the mandatory-surface intake refusal as the standing control.
      ``unevaluated:collision-error`` the collision gate raised, so safety is
        unknown for the same reason and gets the same fail-open treatment. It is
        a stated verdict rather than a swallowed error precisely so it cannot
        reach the frontier looking like a clean comparison. Carries the same
        ``+same-domain:<domain>`` annotation when the domain is held.

## _strandable_epic_ids

Open epics (parents) whose children are ALL done - closeable right now.

Read-only. The cascade (_cascade_close_parents) only fires on a child-CLOSE
event, so an epic whose children were all completed BEFORE this code shipped
(or whose last child closed via a path that did not cascade) is stranded:
open, all children done, and - now that containers are hidden from
next/ready - unreachable for closure. This identifies them so reconcile can
self-heal.

## _sweep_close_done_epics

Close every open epic whose children are all done (self-heal/migration).

Idempotent, mutating, run inside a close mutator. Repeats to a fixpoint so a
freshly-closed epic heals ITS parent too (grandparent chains). Returns the
ids it closed so the caller can auto-continue their dependents. Reconcile
runs this so pre-existing stranded all-done epics heal on the next reconcile
pass - going forward the cascade prevents new ones, so this is a no-op once
migrated.

## Reopen holds against automatic closes

A deliberate reopen (``fno backlog reopen --reason``) is human judgment; a
close is machine evidence. An automatic sweep that discards the judgment
without a word is the defect the reopen guards exist to end. Measured twice:
a reopen carrying operator words was re-closed within two minutes, and no
retry could hold it.

Four close paths read a reopen; the deliberate verb (``cmd_done``, with its
own ``--force`` plus ``--reason`` ladder) does not:

- ``_cascade_close_parents`` and ``_sweep_close_done_epics`` key on the
  CHILDREN's closes (``_reopen_outranks_child_closes``).
- reconcile's PR-merged close leg keys on the MERGE
  (``_reopen_outranks_merge``): it reads no children, so the child-keyed
  guard never reached it.
- the contained merge cascade keys on the merge when the closing PR's
  ``merged_at`` is handed to it, and on the owner's historical close stamp
  otherwise - the mutator stamps that field with reconcile's wall clock
  moments before the call, so the merge is the evidence on that path.

Expiry and ambiguity, in both directions. A reopen PREDATING its close
evidence is stale and expires by itself: the node genuinely completed after
the judgment was formed, so a later merge closes it again with no operator
action. Because the record stamps the FIRST merged ref, a hold is expired
only after the node's OTHER refs are checked for a merge postdating the
reopen (``_merge_postdates_reopen``). An unreadable or unread stamp protects
the human: the node stays held one more sweep and the next pass retries.
Inside the locked close mutation the guard is re-checked against the live
node, because ``closeable`` is a pre-lock snapshot and ``completed_at``
reads None again the moment a reopen lands; a reopen the snapshot already
considered and expired skips that recheck.
