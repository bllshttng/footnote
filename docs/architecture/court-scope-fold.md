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

The fold resolves its own claims directory. Every key it asks after is a `node:` key. Those route to the global claims root on both the Rust and the Python side. One resolver answers, and no caller passes a path. `--claims-dir` stays as an override for tests.

Until 2026-09-12 the one Python caller passed no directory. The Rust side then returned an empty map on its `None` arm. So the worker column read null on every row of every surface, while the help string already documented the flag. That is the false-zero shape AGENTS.md names: the instrument ran, it reported clean, and it had read nothing.

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

The fold pays one whole-graph read and one claim sweep per call. Over 2037 nodes it measured 8.5 s of CPU, and a busy fleet stretches that past 30 s of wall clock. The caller waits 120 s. A court that reads blind whenever the fleet is busy is blind at the one moment anybody asks it.

## What a node row carries

Beside `id`, `slug`, `status`, `worker`, `pr_number` and `sessions`, an active row states its claim and its age.

`claim_state` is the sweep's own verdict (`live`, `suspect`, `free`, `stale`, `corrupted`) plus two the fold itself answers. `no-record` means the sweep ran and found no claim file for that node. `unreadable` means the sweep never reached the store, so nothing was measured. Those two must never print the same string: an absence and a broken instrument are different answers, and `claim_state` is the field that separates them. `claims::list_in_result` names the directories whose scan succeeded, which is the same distinction one layer down.

When `claim_state` is `live` or `suspect`, `worker` names the holder. On any other verdict `worker` is null, because a holder on an unheld record is history, not an owner. `claim_basis` carries the sweep's own basis string.

`age_hours` comes from the entry's `created_at`, to one decimal. When no stamp parses, it is null. A reader must not read that null as "brand new". `blocked_by` and `blocked_reason` come straight off the graph entry, so a blocked row says what it waits on instead of only counting.

The board's HTML section renders `claim` and `age` as their own columns, because the section and the JSON come from one fold.

## The stuck verdict

The counts say how much. `stuck` says whether anything needs a hand, which is the only part of the read worth a glance. It is computed in `court_fold.rs`, beside the rows it judges, so no second reader can disagree about what a row means. The fold returns it as `stuck`, plus a rendered `stuck_line` for the node half of the one-line answer.

A node counts as stuck under exactly these rules, with the threshold at 60 minutes:

- `ready` or `in_progress`, not held, and older than the threshold.
- `blocked`. The line names the ids in `blocked_by`, because a count that never says what on is the gap this closes.
- An unproven claim (`corrupted` or `unreadable`). An unproven claim blocks a dispatch as hard as a held one does.
- `in_review` with a `pr_number`, older than the threshold.

A node is counted ONCE. An L1 crown folds the nodes its L2 epics also fold. An overlapping node therefore reaches the verdict once per crown covering it. Counting it twice reports more stuck work than exists. Several crowns failing the same way is one fault and prints one line.

A clause names five ids and then counts the rest, because a live court put 40 ids in one clause. The full list stays in the JSON.

The caller adds only what the fold cannot see. `fno agents court` appends the spawn gate's refusal. An unknown gate is itself a blind spot, so it lands in `blind` rather than being dropped. When nothing is stuck and the gate accepts, the line reads `stuck: nothing`. When the fold, the sweep or the gate cannot answer, the line says which one cannot answer. A clean line and a blind line must never look the same.

Live PR state is deliberately out of scope. `merge_status` on a graph entry is a closure stamp that only ever reads `merged` or null. It cannot say CONFLICTING or red. The honest verdict needs a network read, and `fno do pr status <n>` already performs it. The row carries `pr_number` and an age, so an `in_review` node past the threshold surfaces without one.

## Session ids on a row

A node row's `sessions` is the ordered de-duplicated union of
`sessions[].session_id`, then `session_id`, then `cost_sessions`, then
`locked_by_harness_session`. First occurrence wins. This is local-board
and local-CLI data: a published snapshot carries no holder names and no
session ids, and the `local` gate at the splice site is the only thing
between the fragment and a public document.
