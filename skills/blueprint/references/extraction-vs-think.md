# Extraction vs `/think`, and where they reconcile

Read this when turning a commitments list (a transcript, a review's action items, a decision log) into backlog nodes, or when deciding whether a question wants `/think` or extraction.
The 2026-07-28 mislinking put three nodes on one plan file because the two were treated as the same step.

## Extract commitments; do not `/think` them

A commitments list is a record of promises already made.
Its only correct processing is extraction: one node per commitment, so nothing is dropped.

`fno backlog idea --parent <epic>` mints a node per finding at discovery granularity, which is the right granularity here, because a finding exists at the moment it is recorded, not later.
Dropping an item to keep the backlog clean is a defect: the item was a commitment.

## `/think` scopes unknowns; it is not a commitments processor

A design question (what should this do, what could break, is X in scope) wants `/fno:think`.
Its job is to narrow: it carries a `## Non-Goals` section on purpose, and narrowing is scope authority.

Running `/think` over a commitments list hands scope authority over promises to the step whose purpose is narrowing.
Commitments get reframed, merged, or dropped to fit a cleaner design, and a commitment that does not fit the shape the design wants disappears.
That is how three commitments ended up folded onto one plan with no record that they were one delivery unit.

## `adopt` reconciles the two granularities

Discovery mints at finding granularity (one node per commitment).
Delivery ships at PR granularity (one node per shippable plan).
The two disagree by nature whenever a plan folds two findings into one PR.

`adopt` is the reconciliation.
In `fno backlog decompose`, a group's `adopt: [<node-id>...]` re-parents the discovery nodes into the group child that ships their PR.
The finding is preserved (extraction did its job) and the delivery boundary is drawn (one plan, one PR, one node).
See `epic-decomposition.md` step 5b.

The ordering is fixed: extract first (nothing dropped), then `adopt` (boundary drawn).
`/think` belongs before decomposition, scoping the epic; never after it, re-scoping the promises.
