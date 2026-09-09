# The court scope fold

`fno agents court -n` folds every crown's scope into its row: counts by
status for the whole scope, the active nodes with their worker, PR and
session ids, and the omitted count stated rather than implied. The same
fold renders as the local board's court section. Design notes that used to
live in docstrings, gathered here.

## Layering: the section crosses layers as a file, not an import

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

## Session ids on a row

A node row's `sessions` is the ordered de-duplicated union of
`sessions[].session_id`, then `session_id`, then `cost_sessions`, then
`locked_by_harness_session`. First occurrence wins. This is local-board
and local-CLI data: a published snapshot carries no holder names and no
session ids, and the `local` gate at the splice site is the only thing
between the fragment and a public document.
