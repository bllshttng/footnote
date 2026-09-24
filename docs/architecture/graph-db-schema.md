# graph.db schema (version 4)

`graph.db` is the backlog store. `crates/fno-agents/src/backlog` holds its only DDL and its only write connection, `backlog::open`, which sets `PRAGMA foreign_keys=ON`. Each table has one owning module, listed in `TABLE_OWNERS` (`backlog/mod.rs`). The `table_ownership` test fails on a write to a table outside its owner's file. It also fails on a new `Connection::open` of the graph.db path outside the allowed read-only probes.

## Tables

| Table | Owner | Key | References |
|---|---|---|---|
| `graph_meta` | `backlog/mod.rs` | `key` | none |
| `harnesses`, `models` | `backlog/entities.rs` | `id` (non-empty) | none |
| `agent_sessions` | `backlog/entities.rs` | `id` (non-empty) | `harness_id` to `harnesses` |
| `nodes` | `backlog/nodes.rs` | `id`, `slug` unique | `session_id` to `agent_sessions` |
| `nodes_raw` | `backlog/nodes.rs` | `id` | none |
| `node_claims` | `backlog/nodes.rs` | `node_id` | `nodes` (cascade), `harness`, `harness_session` |
| `node_dispatch` | `backlog/nodes.rs` | `node_id` | `nodes` (cascade), `model` to `models` |
| `node_provenance` | `backlog/nodes.rs` | `node_id` | `nodes` (cascade), three session ids, two harnesses |
| `supersessions` | `backlog/nodes.rs` | `node_id` | `nodes` (cascade) |
| `sessions` | `backlog/sessions.rs` | `(node_id, seq)` | `nodes` (cascade), `harness`, `session_id` |
| `comments` | `backlog/comments.rs` | `(node_id, seq)` | `nodes` (cascade), `source_harness`, `source_session_id` |
| `encounters` | `backlog/encounters.rs` | `(node_id, seq)` | `nodes` (cascade), `harness`, `session_id`, `model` |
| `pull_requests` | `backlog/pull_requests.rs` | `(node_id, seq)` | `nodes` (cascade) |
| `relations` | `backlog/relations.rs` | `(node_id, related_node_id, type)` | all three node columns to `nodes` |
| `relations_unresolved` | `backlog/relations.rs` | `(node_id, related_node_id, type)` | `listed_on` to `nodes` (cascade) |
| `decisions` | `backlog/decisions.rs` | `seq`, `event_id` unique | none |
| `node_decisions` | `backlog/decisions.rs` | `(node_id, event_id)` | `nodes`, `decisions` (both cascade) |
| `node_costs` | `backlog/costs.rs` | `(node_id, seq)` | `nodes` (cascade), `session_id` |
| `findings` | `backlog/findings.rs` | `finding_id` | `nodes` (cascade), `source_harness`, two session ids |
| `nodes_fts` | `backlog/search.rs` | FTS5 over `nodes` rowids | none |

A harness column points at `harnesses`, a session column at `agent_sessions`, a model column at `models`. Node self-references have no foreign key: `parent_id`, `contained_in`, `superseded_by`, `caused_by` and `supersessions.successor_id`. The live store holds dangling values there. A key forces them out of the node JSON, so the schema declares none. `sessions.observed_model` is a JSON object, not a model id, so it has no key either.

## Timestamps

Every table has `created_at` (row written) and `updated_at` (row last changed). If a write leaves `updated_at` unchanged, an `AFTER UPDATE` trigger per table sets it. `nodes`, `comments`, `findings` and `node_claims` keep a wire `created_at` that the node JSON carries, so their `created_at` can be null. A claim's `created_at` is its lock time.

Every timestamp column carries a named CHECK, `<table>_<column>_iso`. A value must parse with `datetime()` and end in `Z` or `+00:00`. A refused write names the constraint, for example `nodes_completed_at_iso`. The store writes its own stamps as `%Y-%m-%dT%H:%M:%fZ`. It never rewrites a stored value to that spelling, because the node JSON must keep its bytes.

### Session rows

A phase that opens stamps its row's `started_at`, and a phase that closes stamps its `ended_at` (user rulings 2026-09-24). A missing time is never invented: a legacy row with no recorded source keeps its gap.

| Phase | `started_at` | `ended_at` |
|---|---|---|
| think, review | the spawn that opens the row | the gc sweep, when the session retires: its transcript's last event (`ended_by: reap-sweep`) |
| blueprint | the planner's own claim, the spawn claim it joined, or the skill's `--started-at` | `fno backlog session close` |
| do | the node claim | the claim release, the finalize stamp, or the gc settle on a merged node |
| ship | the PR link (`fno do pr bind-created`) | the merge reaper at the merge instant (`ended_by: merge`). The gc sweep backfills a row it missed from the `Merge pull request #N` commit on origin/main (`merge-commit`), or GitHub's `closed_at` for a PR closed unmerged (`pr-closed`). |

The closes that no session writes live in `crates/fno-agents/src/phase_close.rs`.

A legacy row fills its gap only through `fno backlog session backfill`, never through the migration. The verb reads the session's transcript. A start is the first `/fno:<phase>` or `$fno:<phase>` call whose arguments name the node or its plan file. When the session holds that phase for one node only, the first call counts, named or not. A think, blueprint or review row ends at the last event before the next call of another phase or another node. A do row never ends there. The verb never overwrites a stamp. It is a dry run until `--apply`, and it counts the rows it cannot match (`crates/fno-agents/src/session_backfill.rs`).

## Entities

Triggers on each referencing table create the parent rows (`<table>_entities_bi` and `_bu`). Every writer, including an older binary during a rollout, gets a valid parent with no code of its own. A session id first seen with no harness takes the first harness a later row names. A later, different harness never overwrites it. An empty harness, model or session id fails `<table>_id_nonempty` and aborts the write.

## Relations

Both ends of a `relations` row are real nodes. An edge whose far end names no node lives in `relations_unresolved`. The node JSON lists read both tables, so the lists keep their bytes. When the missing node arrives, the `relations_promote` trigger moves its edges into `relations`. When a node is deleted, `relations_park` moves the edges other nodes list against it into `relations_unresolved`, so a deletion never shortens another node's list.

## Row versions

`nodes.version` and `nodes_raw.version` count the writes of each row. The statement that writes the row (the `nodes` upsert in `nodes::save`, the `nodes_raw` upsert in `nodes::save_raw`) bumps it. When the status pass (`nodes::recompute_status`) moves a status or a defect, it bumps the version too. A writer that began before that roll-up then conflicts. Nothing else writes it. The node JSON never carries it. The keeper's `commit_rows` compares it: see [coordination](coordination.md#commit_rows-compares-per-row-versions).

## Costs and provenance

`request_origin` and `origin_evidence` are `node_provenance` columns. If every item names a session and carries a UTC ISO-8601 timestamp or none, a `cost_sessions` list lives in `node_costs`. Any other list stays in `nodes.extras`, so the timestamp CHECK never refuses a node save.

## Wire names

A timestamp name says what it records (user ruling 2026-09-23). A session's `at` and `claimed_at` held its stamp time, so they fold into `started_at`. An encounter's `ts` is now `created_at`. The column `node_claims.locked_at` is now `created_at`, and the node JSON key stays `locked_at`, because the node already has a `created_at`. The column `decisions.ts` is now `created_at`. The flattened decision rows keep `ts`, the event envelope key.

Writers emit only the new names. Readers accept the old ones for one release through `LEGACY_ITEM_KEYS` in `backlog/model.rs`. A legacy `at` that is not UTC ISO-8601 stays in the item's extras, so nothing is lost. A progress note's `ts` does the same, in the model and in the migration. Delete the map, and the folds that read it, one release on.

## The schema-4 migration

`backlog::open` runs `schema_v4::migrate_if_needed` before it ensures the tables. It detects a schema-3 store by shape (`nodes` has no `updated_at`), never by the version string.

1. It turns `foreign_keys` off and takes `BEGIN IMMEDIATE`. It re-reads the shape under the lock, so a second opener that waited returns with no snapshot and no rebuild.
2. It writes a snapshot, `backups/graph-pre-v4.db.<stamp>`, through a second read-only connection. The hourly rotation deletes only `graph.db.*`, so it keeps this one.
3. It renames each table to `<table>_v3` and creates the new tables with no triggers.
4. It fills the entity tables from every referencing column. Legacy session ids get a row with no harness.
5. It copies every table, keeping rowids so the search index stays valid. It unquotes the JSON-quoted session `at` values and folds them into `started_at`. It moves the two provenance keys out of `extras`, and each `cost_sessions` list that `node_costs` can hold. An edge listed on a missing node has no reader and is dropped. An edge whose far end is missing is parked.
6. It drops the `_v3` tables, creates the triggers, and rebuilds the search index.
7. It runs `PRAGMA foreign_key_check`. A violation rolls everything back, and the error names the row and the snapshot. Otherwise it stamps `schema_version = 4`, writes the counts to `graph_meta.schema_v4_report`, and commits.

## Rollback

The migration is one-way in place. The snapshot is the rollback. Stop the keeper. Copy `backups/graph-pre-v4.db.<stamp>` over `graph.db` and delete `graph.db-wal` and `graph.db-shm`. Then install the previous release. Writes made after the migration are lost, so roll back at once or not at all.

Do not run a schema-3 binary against a schema-4 store. The entity triggers keep its writes valid, but it writes dangling relation ends into `relations` and the foreign key refuses them. It also writes the old column names, which no longer exist. The refusal is loud, never silent.
