# The decision record

A ruling stated in chat dies with the context. The operator then asks, weeks later, "there was this thing we discussed, what happened with that?" This page owns the answer: what records a decision, where it lands, and how to get it back.

## The verb

```bash
fno decide --subject x-8f44 --decision "recall moves to a machine-wide index" \
  --rationale "the global journal rotates and a union read is 793 MB" \
  --option "global journal" --option "machine-wide index"

fno decide list --subject x-8f44   # newest first, superseded rows marked
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

One `fno decide` call writes the journal, then the index, then the projection. A failed index write is not a success: the exception propagates, the command prints `decide: failed to record` and exits 1. A write the operator cannot read back is worse than a refusal.

## Why the index is separate from the global journal

The event stays project-local because it is durable there. `cli/src/fno/events/gc.py` refuses to compact the global journal but does compact project journals, and it deletes rows classified `retention: ephemeral`. `operator_decision` now carries an explicit `retention: durable` key, so the GC keeps it. It behaved that way before only because it named no retention and the default is durable.

Recall needs one machine-wide file, and two candidates were rejected.

- The global journal rotates. `~/.fno/events.jsonl.1` already holds 13.5 MB here. A rotated-away ruling is the exact failure this record exists to prevent.
- Folding every project journal per read does not scale. The machine-wide graph names 83 project roots, and their journals total 793 MB.

So the index is its own file, it never rotates, and it holds nothing else. It carries the same `operator_decision` envelopes and is written with the same `append_event`, so it inherits validation and the mutex and needs no second parser.

## The subject convention

The subject is any string. When a node exists, use its id. Otherwise use `pr-<n>`, or the area.

The reader takes every subject the writer takes. That is the defect this page was written for. The writer accepted free text while the reader resolved a graph node first, so a ruling about `pr-923` was written, receipted, and lost.

Matching is exact set membership on the recorded string. A subject that resolves to a node also answers to that node's id and slug. A decision about `pr-92` never answers a query for `pr-921`.

A decision with no subject at all is reachable only through `fno decide list` with no `--subject`. When the question names no node, that is what `fno outstanding clear --answer` writes.

## Supersession

`--supersedes <decision-id>` overturns an earlier ruling. The older row stays and is marked `[superseded by ...]`, because a reader of an overturned decision must be able to tell it is not current.

The graph projection stamps that mark at write time under the lock. The index is append-only and cannot, so the reader derives it from the rows it scanned.

## Backfill

`fno decide reindex` makes the index a superset of what already exists. It folds the journals first, then every `decisions` array on the machine-wide graph, and appends anything whose `decision_id` the index does not already hold.

Journals win a tie because a journal holds the event as written. A projection row is derived and can be lossier: the oldest one on this machine dropped `subject`, the one field a recall query reads. A projection row with no subject falls back to the node it sits on.

The command is idempotent by decision id, so a second run adds nothing.
