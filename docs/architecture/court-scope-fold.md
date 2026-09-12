# The court scope fold

`fno agents court -n` folds every crown's scope into its row: counts by
status for the whole scope, the active nodes with their worker, PR and
session ids, and the omitted count stated rather than implied. Design
notes gathered here.

## Layering: the section crosses layers as a file, not an import

The board's court section (a follow-up node; the readout below ships
first) renders the same fold as HTML. Its data is the agents runtime's
own (registry, claims, crown verdicts), while the board renderer is L1
core and must not import `fno/agents/*` (L5 runtime); the
company-boundary gate prohibits new edges. The contract between the
layers is therefore a file: the runtime writes a section fragment and
the renderer splices that fragment between markers on the local board.
The section's CSS ships inside the fragment, so the board never needs to
know the section exists.

The court's data is the agents runtime's own (registry, claims, crown
verdicts). `fno/graph/render_html.py` is L1 core and must not import
`fno/agents/*` (L5 runtime); the company-boundary gate prohibits new
edges. The contract between the layers is therefore a file: the runtime
writes `~/.fno/court-section.html` (`fno agents court --update-board`)
and the renderer splices that fragment between the local board's
`<!-- court:begin --><!-- court:end -->` markers. The section's CSS ships
inside the fragment, so the board never needs to know the section exists.

The fragment is one court read behind the board that splices it: a graph
mutation re-renders the board in-process with the previous fragment, and
the next `--update-board` closes the gap. Refresh cadence belongs to the
caller (hooks, or the operator).

## The fold lives in the native binary

`fno-agents court-fold` reads graph.json and the claims dir itself,
compiles each crown's scope with the rules `king_board/scope.rs` applies,
and names workers through the same native claim verdicts `claim sweep`
uses, so a fold and the claims surface cannot disagree about who holds a
node. Python passes the crowns `gather_court` already adjudicated and
reads the answer back. A fold that cannot run - stale binary, unreadable
graph, timeout - marks the crown `unresolved` with the reason rather than
rendering an empty table.

The fold resolves its own claims directory. Every key it asks after is a
`node:` key, and those route to the global claims root on both the Rust
and the Python side, so one resolver answers and no caller passes a path.
`--claims-dir` stays as an override for tests. Until 2026-09-12 the one
Python caller passed no directory and the Rust side returned an empty map
on its `None` arm, so the worker column read null on every row of every
surface while the help string already documented the flag. That is the
false-zero shape AGENTS.md names: the instrument ran, it reported clean,
and it had read nothing.

The scope compile is a FORCED-level arm of the board's compiler: the
level comes from the crown row the court already adjudicated, never
re-resolved from config. A row reading level=2 over a project folds as
epics and fails; it does not silently re-resolve into the project's
nodes.

## The vocabulary

ACTIVE_STATUSES (in_progress, in_review, ready, blocked, design) is the
statuses a reader means by "what is being worked on": neither closed
(done, superseded) nor unstarted (idea, deferred). It lives in
`court_fold.rs`; the Python tree holds no second literal. Counts cover
every status present in the whole scope and render in lifecycle order;
`omitted` is always stated, so a crown whose active list is empty reads
as "N nodes, none active", never as "nothing here".

## What a node row carries

Beside `id`, `slug`, `status`, `worker`, `pr_number` and `sessions`, an
active row states its claim and its age.

`claim_state` is the sweep's own verdict (`live`, `suspect`, `free`,
`stale`, `corrupted`) plus two the fold itself answers. `no-record` means
the sweep ran and found no claim file for that node. `unreadable` means
the sweep never reached the store, so nothing was measured. Those two must
never print the same string: an absence and a broken instrument are
different answers, and `claim_state` is the field that separates them.
`claims::list_in_result` names the directories whose scan succeeded, which
is the same distinction one layer down. `worker` is filled only when
`claim_state` is `live` or `suspect`; a holder on an unheld record is
history, not an owner. `claim_basis` carries the sweep's own basis string.

`age_hours` comes from the entry's `created_at`, to one decimal, and is
null when no stamp parses. A reader must not read that null as "brand
new". `blocked_by` and `blocked_reason` come straight off the graph entry,
so a blocked row says what it waits on instead of only counting.

The board's HTML section renders `claim` and `age` as their own columns,
because the section and the JSON come from one fold.

## Session ids on a row

A node row's `sessions` is the ordered de-duplicated union of
`sessions[].session_id`, then `session_id`, then `cost_sessions`, then
`locked_by_harness_session`. First occurrence wins. This is local-board
and local-CLI data: a published snapshot carries no holder names and no
session ids, and the `local` gate at the splice site is the only thing
between the fragment and a public document.
