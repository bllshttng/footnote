# External tracker adoption (bring-your-own-id)

footnote consumes a backlog.
It does not own one.
A user who tracks work in Linear, GitHub Issues, Jira, or nothing at all can
still run the delivery pipeline.
footnote adopts an opaque external id as its work handle and keeps only the
state no tracker can express.

This document is the design contract.
The load-bearing invariant is a partition.
It is enforced in CI by `scripts/ci/check-tracker-partition.sh`.

## The partition rule

Today's `~/.fno/graph.json` is two things merged into one record.
It is a tracker, the fields any backlog stores.
It is also a sidecar, the fields only footnote knows.
Adoption is a partition along that existing line, not a rewrite.

The rule that prevents the two-sources-of-truth bug is structural.

The sidecar stores only fields the tracker cannot express.
Zero overlap means zero sync.

A field named on both sides is the only failure mode that matters.
The first convenience mirror of `title` into the sidecar reintroduces exactly
the two-writer bug that the rejected import or sync option produces.
`scripts/ci/check-tracker-partition.sh` fails on any overlap other than `id`.
Both sides carry `id` as the join key, never as a synced value.

## The interface

Read from the tracker, five fields.
They live in `cli/src/fno/tracker/types.py` on `TrackerNode`.

| field | why |
|---|---|
| `id` | opaque, globally unique, claim key |
| `title` | display only |
| `state` | open or closed, footnote derives its own rung |
| `parent` | epic rollup, board ordering |
| `blocked_by` | merge-triggered dispatch via `backlog advance` |

Write to the tracker, one operation.
`close(id)` runs at node closure.
A PR link is a comment or a native issue-PR link, not a field write.

`status` is not on the interface and never will be.
It is derived in `fno.graph.types._derive_status` from `completed_at`,
`superseded_by`, `deferred_at`, `pr_number`, `blocked_by`, and the plan rung.
A backend supplies open or closed.
footnote keeps deriving the rest from the plan and the PR exactly as it does
today.

## The sidecar

The sidecar lives at `~/.fno/sidecar/<url-encoded-id>.json`.
It resolves through `fno.paths`, one file per work item, keyed by the opaque id.
Placement is beside `graph.json` and `ledger.json`.
It inherits the existing `config.paths` and `state_dir` override with no new
machinery.

One file per item means concurrent workers on different items never contend.
The per-key-file pattern is already proven by `.fno/claims/`.
The filename reuses the claims key encoder at `fno.claims.io.encode_key`.
An id containing a path separator like `owner/repo#123` lands as one filename.

Sidecar fields each have no external equivalent.
They are `cwd`, `plan_path`, `pr_number`, `pr_url`, `additional_prs`,
`cost_usd`, `cost_sessions`, `sessions`, `source_*`, `spawned_by_*`, and
`claimed_at`.

If `cwd` is dropped, the loss is fatal.
It is also the field most likely to be missed.
No tracker models a local checkout.
It is the authority for multi-repo dispatch and backs
`docs/architecture/node-cwd-authority.md`.

The claim pointer is not in the sidecar.
`locked_by` and `session_id` are live coordination state owned by the claims
subsystem at `fno.claims.io`.
That subsystem keys on the opaque id and never opens the graph.
Mirroring it here must not happen, because it makes the sidecar a second writer
for claim state.

These fields live only in the tracker and never enter the sidecar.
They are `title`, `state`, `priority`, `parent`, `blocked_by`, `size`,
`domain`, and `details`.

## Backends

The first backend is `graph.json`, the default, unchanged.
A user who wants no tracker gets today's behaviour with no config.
A stock install with no account works offline.
`graph.json` is the default forever, never a migration target.

The second backend is GitHub Issues, the first external one.
Linear is third and has the cleanest data model of the three.
Jira is last and ships on demand.

The default `GraphTracker` is a thin projection over `read_graph`.
It preserves today's behaviour exactly and ships as proof the seam is honored.

## Routing decisions

`fno backlog next` keeps footnote's ranking applied to fetched items.
Delegating selection to each backend's query language breaks the board-as-
work-order contract that `advance` depends on.

The fno-versus-external id classification has one shell source at
`scripts/lib/node-id.sh`.
`graph-resolve.sh` and `parse-claims-arg.sh` both source it.
The Python authority is `fno.graph._constants.is_wellformed_node_id`.
The shell copy exists because the resolvers keep a legacy fallback for
environments where the fno Python package is unavailable.
The two are pinned together by `cli/tests/unit/test_node_id_sh.py`.

## Failure modes

An external id containing `:` collides with the claim-key grammar, which
partitions on `:`.
Validate at the adapter boundary.
Refuse such an id rather than mis-routing a claim.

Every read path that today cannot fail can fail once a network tracker is
behind it.
Never block a hook on a network call.
Cache the last good read in the sidecar and degrade to it.

Every adapter field needs a test asserting a read at a named consumer.
A test that asserts the write landed is not enough.
A generic `--field` fetch is not enough either.
The `artifact_url` reader and writer asymmetry is the standing proof this system
can carry a write-only field for its whole life.
