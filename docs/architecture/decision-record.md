# The decision record

A ruling stated in chat dies with the context. The operator then asks, weeks later, "there was this thing we discussed, what happened with that?" This page owns the answer: what records a decision, where it lands, and how to get it back.

## The verb

```bash
fno decide --subject pr-1234 --decision "recall moves to a machine-wide index" \
  --rationale "the global journal rotates and a union read is 793 MB" \
  --option "global journal" --option "machine-wide index"

fno decide list --subject pr-1234  # newest first, superseded rows marked
fno decide list                    # the recent decisions across every subject
fno decide reindex                 # backfill records written before the index
```

## Two producers, both explicit

`fno decide` records a ruling that has no question on file. `fno outstanding clear --answer` records the answer to an open `operator_question`, because an answered question IS a decision.

Both are explicit on purpose. Automatic classification of "was that a ruling?" is judgment on a truncated view, which `docs/architecture/memory-system.md` records as deprecated for cause.

There is no new recording gate, and none is needed. `crates/fno-agents/src/loopcheck.rs` already holds a session that closed its own question with an answer and emitted no matching decision. Its refusal already names the verb. That gate is starved, not missing: only two `operator_question` events exist across 194,109 events.

## Three stores, one write

| Store | What it is for | Path |
|---|---|---|
| Project journal | Durability and the project audit trail | `<canonical-repo>/.fno/events.jsonl` |
| Decision index | Recall. The reader's only source | `~/.fno/decisions.jsonl` via `paths.decisions_jsonl()` |
| Graph projection | The node view, for anyone reading the node | the subject node's `decisions` array |

One `fno decide` call writes the journal, then the index, then the projection. A failed index write is not a success: the command exits 1, because a write the operator cannot read back is worse than a refusal.

It does NOT ask for a retry. The durable event has already landed by then, so a second run records one ruling twice under two ids. Both producers say so and name `fno decide reindex` as the recovery.

## Why the index is separate from the global journal

The event stays project-local because it is durable there. `cli/src/fno/events/gc.py` refuses to compact the global journal but does compact project journals, and it deletes rows classified `retention: ephemeral`. `operator_decision` carries an explicit `retention: durable` key, so the GC keeps it.

That key changes no behavior today, and it is not decoration. The schema default is already `durable`, so the record survived by inheriting it. A record the whole recall promise rests on was one schema edit away from not surviving, and the key is what makes the guarantee readable at the entry rather than inferred from a default three thousand lines away.

Recall needs one machine-wide file, and two candidates were rejected.

- The global journal rotates. `~/.fno/events.jsonl.1` already holds 13.5 MB here. A rotated-away ruling is the exact failure this record exists to prevent.
- Folding every project journal per read does not scale. The machine-wide graph names 83 project roots, and their journals total 793 MB.

So the index is its own file, it never rotates, and it holds nothing else. It carries the same `operator_decision` envelopes and is written with the same `append_event`, so it inherits validation and the mutex and needs no second parser.

## The subject convention

The subject is any string. When a node exists, use its id. Otherwise use `pr-<n>`, or the area.

The reader takes every subject the writer takes. That is the defect this page was written for. The writer accepted free text while the reader resolved a graph node first, so a ruling about `pr-923` was written, receipted, and lost.

Both sides expand, not just the query. A subject that names a node answers to every spelling of that node: the id, the slug, any case. The operator records under whatever was in front of them. The receipt then prints the canonical id as the way back. A reader that expands only the query sends them to a command that returns nothing.

A subject that names no node matches itself and nothing more. A decision about `pr-92` never answers a query for `pr-921`.

A decision with no subject at all is reachable only through `fno decide list` with no `--subject`. When the question names no node, that is what `fno outstanding clear --answer` writes.

## Supersession

`--supersedes <decision-id>` overturns an earlier ruling. The older row stays and is marked `[superseded by ...]`, because a reader of an overturned decision must be able to tell it is not current.

The graph projection stamps that mark at write time under the lock. The index is append-only and cannot, so the reader derives it from the rows it scanned.

## Backfill

`fno decide reindex` makes the index a superset of what already exists. It folds the journals first, then every `decisions` array on the machine-wide graph, and appends anything whose `decision_id` the index does not already hold.

Journals win a tie because a journal holds the event as written. A projection row is derived and can be lossier: the oldest one on this machine dropped `subject`, the one field a recall query reads. A projection row with no subject falls back to the node it sits on.

The command is idempotent by decision id, so a second run adds nothing.
