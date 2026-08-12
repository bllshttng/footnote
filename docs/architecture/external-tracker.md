# External tracker adoption (bring-your-own-id)

footnote consumes a backlog. It does not own one.
A user who tracks work in Linear, GitHub Issues, Jira, or nothing at all can
still run the delivery pipeline, because footnote adopts an opaque external id
as its work handle and keeps only the state no tracker can express.

This document is the design contract. The load-bearing invariant is a partition,
enforced in CI by `scripts/ci/check-tracker-partition.sh`.

## The partition rule

Today's `~/.fno/graph.json` is two things merged into one record: a tracker
(the fields any backlog stores) and a sidecar (the fields only footnote knows).
Adoption is a partition along that existing line, not a rewrite.

The rule that prevents the two-sources-of-truth bug is structural:

> The sidecar stores only fields the tracker cannot express.
> Zero overlap means zero sync.

A field named on both sides is the only failure mode that matters.
The first convenience mirror of `title` into the sidecar would reintroduce
exactly the two-writer bug that the rejected import/sync option produces.
`scripts/ci/check-tracker-partition.sh` fails on any overlap other than `id`,
which both sides carry as the join key, never as a synced value.

## The interface

Read from the tracker, five fields (`cli/src/fno/tracker/types.py`, `TrackerNode`):

| field | why |
|---|---|
| `id` | opaque, globally unique, claim key |
| `title` | display only |
| `state` | open or closed; footnote derives its own rung |
| `parent` | epic rollup, board ordering |
| `blocked_by` | merge-triggered dispatch (`backlog advance`) |

Write to the tracker, one operation: `close(id)` at node closure.
A PR link is a comment or a native issue-PR link, not a field write.

`status` is not on the interface and never will be.
It is derived (`fno.graph.types._derive_status`) from `completed_at`,
`superseded_by`, `deferred_at`, `pr_number`, `blocked_by`, and the plan rung.
A backend supplies open-or-closed; footnote keeps deriving the rest from the
plan and the PR exactly as it does today.

## The sidecar

`~/.fno/sidecar/<url-encoded-id>.json`, resolved through `fno.paths`, one file
per work item, keyed by the opaque id.
Placement is beside `graph.json` and `ledger.json`, so it inherits the existing
`config.paths` / `state_dir` override and `fno config doctor` with no new
machinery.
One file per item means concurrent workers on different items never contend;
the per-key-file pattern is already proven by `.fno/claims/`.
The filename reuses the claims key encoder (`fno.claims.io.encode_key`) so an
id containing a path separator (`owner/repo#123`) lands as one filename.

Sidecar fields, every one with no external equivalent:
`cwd`, `plan_path`, `pr_number`, `pr_url`, `additional_prs`, `cost_usd`,
`cost_sessions`, `sessions`, `source_*`, `spawned_by_*`, `claimed_at`.

`cwd` is the field most likely to be missed and fatal if dropped.
No tracker models a local checkout, yet it is the authority for multi-repo
dispatch and backs `docs/architecture/node-cwd-authority.md`.

The claim pointer (`locked_by` / `session_id`) is not in the sidecar.
It is live coordination state owned by the claims subsystem
(`fno.claims.io`), which keys on the opaque id and never opens the graph.
Mirroring it here would make the sidecar a second writer for claim state.

Fields that live only in the tracker and are never copied into the sidecar:
`title`, `state`, `priority`, `parent`, `blocked_by`, `size`, `domain`, `details`.

## Backends

1. `graph.json` (default, unchanged).
   A user who wants no tracker gets today's behaviour with no config.
   A stock install with no account works offline.
   `graph.json` is the default forever, never a migration target.
2. GitHub Issues (first external; not yet shipped).
3. Linear (not yet shipped).
4. Jira (last, on demand).

External backends are not in the foundation.
The default `GraphTracker` is a thin projection over `read_graph`; it preserves
today's behaviour exactly and ships as the proof that the seam is honored.

## Routing decisions

`fno backlog next` keeps footnote's ranking applied to fetched items.
Delegating selection to each backend's query language would put ordering inside
every backend and break the board-as-work-order contract that `advance` depends
on.

The fno-vs-external id classification has one shell source
(`scripts/lib/node-id.sh`), sourced by `graph-resolve.sh` and
`parse-claims-arg.sh`.
The Python authority is `fno.graph._constants.is_wellformed_node_id`; the shell
copy exists because the resolvers keep a legacy fallback for environments where
the fno Python package is unavailable, and the two are pinned together by
`cli/tests/unit/test_node_id_sh.py`.

## Failure modes

An external id containing `:` collides with the claim-key grammar, which
partitions on `:`.
Validate at the adapter boundary and refuse such an id rather than mis-routing
a claim.

Every read path that today cannot fail can fail once a network tracker is
behind it.
Never block a hook on a network call; cache the last good read in the sidecar
and degrade to it.

Every adapter field needs a test asserting a read at a named consumer, not a
test asserting the write landed, and not a generic `--field` fetch.
The `artifact_url` reader/writer asymmetry is the standing proof this system
can carry a write-only field for its whole life with passing tests.
