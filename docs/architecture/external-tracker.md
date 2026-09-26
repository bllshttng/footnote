# External tracker adoption (bring-your-own-id)

footnote consumes a backlog.
It does not own one.
A user who tracks work in Linear, GitHub Issues, Jira, or nothing at all can still run the delivery pipeline.
footnote adopts an opaque external id as its work handle and keeps only the state no tracker can express.

This document is the design contract.
The load-bearing invariant is a partition enforced in CI by `scripts/ci/check-tracker-partition.sh`.

## The partition rule

Today's `~/.fno/graph.json` is two things merged into one record.
It is a tracker of the fields any backlog stores.
It is also a sidecar of the fields only footnote knows.
Adoption is a partition along that existing line, not a rewrite.

The rule that prevents the two-sources-of-truth bug is structural.
The sidecar stores only fields the tracker cannot express.
Zero overlap means zero sync.

A field named on both sides is the only failure mode that matters.
The first convenience mirror of `title` into the sidecar reintroduces exactly the two-writer bug that the rejected import or sync option produces.
`scripts/ci/check-tracker-partition.sh` fails on any overlap other than `id`.
Both sides carry `id` as the join key, never as a synced value.

## The interface

The interface lives in Rust, on `TrackerNode` and `Candidate` in `crates/fno-agents/src/tracker/mod.rs`. The Python `fno.tracker` package is the exec client: it shells `fno-agents backlog get` and parses the pydantic models in `cli/src/fno/tracker/types.py`.

Read from the tracker, five fields plus the display-only reads.

| field | why |
|---|---|
| `id` | opaque, globally unique, claim key |
| `title` | display only |
| `state` | open or closed, footnote derives its own rung |
| `parent` | epic rollup, board ordering |
| `blocked_by` | merge-triggered dispatch via `backlog advance` |
| `details` | display only, the issue body |
| `url` | display only, the issue page |
| `size` | display only, when the backend carries one |

`list_open` widens each open item with the selection-only ordering inputs: `priority`, `rank`, `created_at`.
`list_closed_since(days)` returns the closed window, or `None` when the backend keeps none; the Done column and cycle time empty out without it.
A backend keeps a closed window only if it can answer it bounded.

Write to the tracker, one operation.
`close(id)` runs at node closure.
A PR link is a comment or a native issue-PR link, not a field write.

`status` is not on the interface and never will be.
It is derived from `completed_at`, `superseded_by`, `deferred_at`, `pr_number`, `blocked_by`, and the plan rung.
For an open external item the one derivation is in `tracker/snapshot.rs`: a PR means `in_review`, else a plan means `ready`, else `idea`.
A backend supplies open or closed.

## The door and the cache

Backends answer through `fno-agents backlog get`'s stdin door, the same stdin-JSON shape `gh-budget` uses on `fleet-incident`. The payload is `{"tracker": "read" | "list-open" | "snapshot" | "close", "backend": <name or null>, "id": <id or null>, "stale_ok": <bool>}`. The door prints one JSON object and exits 0 whenever the op ran. Refusals ride in the payload as `{"not_found": true}` or `{"error": "..."}`.

The `snapshot` op builds the joined view once per backend and caches the last good read at `<state_dir>/sidecar/.snapshot/<backend>-<encoded-scope>.json`. The scope is what selects the item set besides the backend name: `FNO_TRACKER_GITHUB_REPO` for github, `FNO_TRACKER_LINEAR_TEAM` for linear, empty for graph.

Only the board passes `stale_ok`; on a failed build it answers the cached snapshot plus `stale_since` and the failure as an errors line, so a backend outage degrades to the last good read instead of blanking the board.
Selection never passes `stale_ok`, so dispatch never picks from stale data.

GitHub parent and blocker links stay degraded (`parent: null`, `blocked_by: []`).
`gh issue list --json` carries neither, and reading sub-issues and dependencies costs two REST calls per open issue per refresh.
A repo with more than 1,000 open issues is truncated, as is the listing cap.

## The sidecar

The sidecar lives at `~/.fno/sidecar/<url-encoded-id>.json`.
It resolves through `fno.paths`, one file per work item, keyed by the opaque id.
Placement is beside `graph.json` and `ledger.json`.
It inherits the existing `config.paths` and `state_dir` override with no new machinery.

One file per item means concurrent workers on different items never contend.
The per-key-file pattern is already proven by `.fno/claims/`.
The filename encodes the id with stdlib `urllib.parse.quote` so a separator like the slash in `owner/repo#123` lands as one filename.

Sidecar fields each have no external equivalent.
They are `cwd`, `plan_path`, `pr_number`, `pr_url`, `additional_prs`, `cost_usd`, `cost_sessions`, `sessions`, `source_*`, `spawned_by_*`, and `claimed_at`.

If `cwd` is dropped, the loss is fatal.
It is also the field most likely to be missed.
No tracker models a local checkout.
It is the authority for multi-repo dispatch and backs `docs/architecture/node-cwd-authority.md`.

The claim pointer is not in the sidecar.
`locked_by` and `session_id` are live coordination state owned by the claims subsystem at `fno.claims.io`.
That subsystem keys on the opaque id and never opens the graph.
Mirroring it here must not happen, because it makes the sidecar a second writer for claim state.

These fields live only in the tracker and never enter the sidecar.
They are `title`, `state`, `priority`, `parent`, `blocked_by`, `size`, `domain`, and `details`.

## Backends

The first backend is `graph.json`, the default, unchanged.
A user who wants no tracker gets today's behaviour with no config.
A stock install with no account works offline.
`graph.json` is the default forever, never a migration target.

The second backend is GitHub Issues, the first external one.

Linear is third and ships in `crates/fno-agents/src/tracker/linear.rs`.

Jira is last and ships on demand.

The Linear backend reads over Linear's GraphQL API, one `curl` POST per op under the same 30 s subprocess bound the github backend uses.

Auth rides the `FNO_TRACKER_LINEAR_API_KEY` env var, named in the backend and in no config key. `FNO_TRACKER_LINEAR_TEAM` (the team key, e.g. `ENG`) scopes the listings the way `FNO_TRACKER_GITHUB_REPO` scopes github.

The id shape is the Linear identifier, `TEAM-123`, which never carries the `:` claim-key partition character.

Linear parent, blockers, priority, estimate, description and url all read real. State types `completed`/`canceled` read closed. Priority 1-4 maps to p0-p3, with no-priority defaulting to p2. Estimate points map to S (<3), M (<8), L (>=8).

Linear has no footnote rank, so card moves stay disabled: `rank` is always `None` and the trait carries no move operation.

The default `GraphTracker` is a thin projection over `read_graph`.
It preserves today's behaviour exactly and ships as proof the seam is honored.

## Routing decisions

`fno backlog next` keeps footnote's ranking applied to fetched items.
Delegating selection to each backend's query language breaks the board-as-work-order contract that `advance` depends on.

The fno-versus-external id classification has one shell source at `scripts/lib/node-id.sh`.
`graph-resolve.sh` and `parse-claims-arg.sh` both source it.
The Python authority is `fno.graph._constants.is_wellformed_node_id`.
The shell copy exists because the resolvers keep a legacy fallback for environments where the fno Python package is unavailable.
The two are pinned together by `cli/tests/unit/test_node_id_sh.py`.

## Failure modes

An external id containing `:` collides with the claim-key grammar, which partitions on `:`.
Validate at the adapter boundary.
Refuse such an id rather than mis-route a claim.

Every read path that today cannot fail can fail once a network tracker is behind it.
Never block a hook on a network call.
Cache the last good read in the sidecar and degrade to it.

Every adapter field needs a test asserting a read at a named consumer.
A test that asserts the write landed is not enough.
A generic `--field` fetch is not enough either.
The `artifact_url` reader and writer asymmetry is the standing proof this system can carry a write-only field for its whole life.
