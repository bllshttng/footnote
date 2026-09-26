# Backlog current state, bounded prose, and the note history journal

Every node carries ONE `current_state` object next to its `details`: `{body, revision, updated_at, source_session_id, source_harness}`. A note REPLACES it. The exact pre-image of every replacement lands in a permanent journal before the replacement publishes. The hot row stays bounded and no history is lost.

Owner: `crates/fno-agents/src/backlog/node_state.rs` (policy), `note_history.rs` (journal), `note_migrate.rs` (inventory, migration, readback). The public writer is `fno backlog note`. The public reader is `fno backlog notes history <id>`. Their bridges and the shipped recipient walk live in `cli/src/fno/graph/note_cli.py` and `cli/src/fno/backlog/note_notify.py`.

## The rules

- Combined `details` + `current_state.body` prose is budgeted at 5,000 Unicode scalars. Count after outer-whitespace and CRLF normalization. The seam refuses any writer that grows a row past the limit. An oversized legacy row can be edited DOWN but never UP.
- Optimistic concurrency: a replacement submits the revision it read. A stale submission refuses with `state conflict: current revision N != submitted M` and writes nothing.
- The journal lives at `<graph>.history/notes.jsonl` (see `docs/state-root-inventory.md`). Records are deduped by node, reason, prior revision, source position, and content hash. Every write is hash-verified. A history failure refuses the state write.
- Machine `task_done` and `run_summary` records and structured wave additions go to history only. A wave sets a `state_needs_refresh` marker. They never overwrite a human or king's current state.
- A row that carries `current_state` has no empty `progress_notes` key. A non-empty one is real pre-cutover prose and is kept.
- Notes on `done` or `superseded` nodes go straight to history and never repopulate hot state.

## The migration runbook

The legacy `progress_notes` feed is retired by an explicit, operator-run migration. Blueprint does not run it. The PR ships the machinery.

1. Inventory: `fno-agents backlog notes inventory --json`. Read-only. It names the backend, per-node note counts, character totals, and each node's `notes_hash`.
2. Digest preparation: for each OPEN node, an agent prepares a manifest entry `{node_id, source_hash, state, details?, author?}` from the node's full original history. Keep obligations, corrections, unresolved uncertainty, and source paths. The combined prose must fit the budget. Notes on OPEN nodes without a manifest entry refuse.
3. Preview: `fno-agents backlog notes migrate --manifest <file> --graph <graph.db>`. Validates every entry against the live rows. A stale hash, a malformed entry, or an over-budget digest names the row `unresolved` and exits nonzero without touching data.
4. Apply: same command plus `--apply`. Re-reads each row under the publication lock. Journals every original note, keyed by node and source position. Replaces the hot row and marks it migrated. A nonzero result names every row it left intact.
5. Readback: `fno backlog notes history <id> --limit 100` (the same reader the migration runs). Explicit and paged. History is never silently loaded into a dispatch prompt.
6. Recovery: restore a selected historical body through the normal revision-checked writer. Never replace the whole graph with an old snapshot over later writes.

Re-running the same manifest is idempotent. Already-migrated rows count as `unchanged`, and logical history counts stay stable.

## Compatibility

The canonical command vocabulary (11 groups) is defined once in `crates/fno-agents/src/backlog/commands.rs` and mirrored in `scripts/ci/verb-collapse-map.tsv`. Legacy spellings keep working during the compatibility window. The `annotate` spellings refuse and name `note --blocking`. The final 11-group-only catalog lands after the note storage merge and compatibility removal.
