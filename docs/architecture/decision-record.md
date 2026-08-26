# The decision record

A ruling stated in chat dies with the context. Weeks later, the operator asks, "there was this thing we discussed, what happened with that?" This page explains recording, storage, shipped design law, and current-law queries.

## The verb

```bash
fno inbox decide pr-1234 "recall moves to a machine-wide index" \
  --rationale "the global journal rotates and a union read is 793 MB" \
  --option "global journal" --option "machine-wide index"

fno inbox decisions pr-1234  # newest first, superseded rows marked
fno inbox decisions --lane law         # only operator-entitled rulings
fno inbox decisions                     # the recent decisions across every subject
fno backlog decide-reindex                 # backfill records written before the index
```

The old `fno decide` spelling remains as a one-release compatibility shim. It prints the new `fno inbox decide` spelling to stderr and is removed next release.

## Two producers, both explicit

`fno inbox decide` records a ruling that has no question on file. `fno outstanding clear --answer` records the answer to an open `operator_question`, because an answered question IS a decision.

Both are explicit on purpose. Automatic classification of "was that a ruling?" is judgment on a truncated view, which `docs/architecture/memory-system.md` records as deprecated for cause.

## Authority lanes

Every read derives an authority lane in the engine. `operator` authority is `law`. Agent and crown authority are both `coord`. `beastmode` authority is `grant`. A row in the checked-in repository catalog is also `law`, because code review is the authority event that promotes design law into the product. The human list leads with `LAW`, `coord`, or `grant`, and `--lane law|coord|grant|unattributed` filters at that same engine seam.

`--authority` takes exactly four values: `operator`, `crown`, `agent`, `beastmode`. Anything else is refused on the write path, and nothing is recorded. Pass `crown` for a king ruling inside its own crown scope. That value exists because three rows on disk carry invented `crown-l1` and `crown-l2-<node>` spellings. Kings wrote them because no correct value existed. The scope belongs on the crown row, so the value carries no suffix.

The closed set is NOT in `schema.yaml`. The index already holds those invented spellings. A schema enum makes `fno backlog decide-reindex` reject them, which drops recall for real rulings.

## Which field a reader can trust

A row is quoted far more often than it is read in the list, and a quoted row carries no lane column. So provenance has to survive on the row itself. Three fields, and only one of them is a fact about who ran the verb.

| Field | Who writes it | What it means |
|---|---|---|
| `decided_by` | stamped from the ambient session, always, when one resolves | who recorded the ruling. This is the field to trust. It cannot be typed. |
| `attested_by` | only a caller with no session identity AND a terminal on stdin | a name a person stood behind, rather than a handle a process stamped |
| `relayed_by` | filled from `--decided-by` when the caller is not the decider | a name the caller supplied for someone else. A claim, not a stamp. |

Three caller states decide which of those get written, and the third fails closed.

1. A session identity resolves. The handle is stamped into `decided_by`, and the row reads as coordination.
2. No identity, and a terminal on stdin. `attested_by` is written, and `--authority operator` is accepted.
3. No identity, and no terminal. Operator authority is REFUSED. `decided_by` says `unattributed-caller`.

Law is never DEFAULTED, in any state. A caller who wants the operator lane passes `--authority operator`. Omit it at a terminal and the row records with no authority, which reads as `unattributed`.

State 3 exists because state 2 used to be everything that was not state 1. That made attendance an ABSENCE, and `env -u CLAUDE_CODE_SESSION_ID fno backlog decide --authority operator` was enough to forge an attested row in the law lane.

Say the residual limit out loud, because it is narrower than the rule it satisfies. A tty is OBTAINABLE: `script -q /dev/null <cmd>` reports one from a context with no person in it, measured on this box. So the terminal raises the cost of forging law and does not prevent it. Forging law THROUGH THIS VERB now takes two deliberate acts, a wrapped tty and an explicit flag, and neither happens by accident.

### Chat-origin probe

On 2026-08-24, Claude Code 2.1.241 emitted a real `UserPromptSubmit` payload for a disposable sentinel. The positive classifier matched `hook_event_name`, `prompt`, `session_id`, `transcript_path`, `cwd`, `permission_mode`, and the extra `prompt_id`. Its redacted shape digest was `03b48a08fb318830886b5dc8ac822ab7ca881a9c78da147efe3eb4f3ed17f329`. The verdict was `origin_provenance: not_exposed`. The payload carried no harness-authenticated human-origin discriminator. A disposable pane did not confirm live delivery of the separate mail sentinel, so no mail payload is claimed as empirical evidence. The implementation therefore uses the permission-gated fallback and treats an unknown schema as refusal.

That claim is scoped to the verb on purpose. The index is an append-only file that nothing authenticates. One raw line written into `~/.fno/decisions.jsonl` reads as law, and `fno backlog decide-reindex` re-emits a forged journal row verbatim. No local signal proves a human is present, because a caller that owns the process owns every local signal. Proof needs an out-of-band attestation this verb cannot mint for itself.

On 2026-08-19 an agent passed `--decided-by "J.N. Choi"`, and that name landed in `decided_by`. Five workers had been told to verify their orders by reading that field. Each did it correctly and got a fabricated yes.

The split closes that without deleting the honest case. An agent relaying a real operator answer still records the name, in `relayed_by`, where a reader sees who typed it.

## Looking one up

A decision id is a lookup key. `fno backlog decisions d-1a2b3c4d` returns that decision whatever subject it was filed under, and `--json` says `matched_by: "decision_id"` so a machine reader is never confused about which key answered.

Repository design law declares a canonical subject and explicit aliases in `docs/architecture/decisions.yaml`. The reader canonicalizes both the query and recorded local subjects through that table. An alias is exact and one-to-one: `handoff` reaches `target-self-handoff` only because the catalog declares it, never because a fuzzy matcher guessed.

Every read of a subject also scans for near misses and names them with their counts. Without that scan, a ruling filed under the free-text subject `force-push policy` stays invisible to `--subject force-push`.

Even where the exact subject DID answer, the scan still runs. The case that cost a reader four rulings returned one row, not zero. A scan gated on an empty answer stays silent on exactly that case.

A miss never claims the store is empty. An argument shaped like a decision id that matches nothing is told it is shaped like one. Every miss is a statement about the query, never about the world.

The fixed cutover is `2026-08-21T00:00:00Z`, chosen to safely postdate every row produced by the old deployed writer. Before that point the writer stamped agent coordination as `operator`, so every pre-cutover operator-shaped row renders as `unattributed`, including rows that answered a question. This deliberately creates a narrow transition window in which a genuine operator ruling is not called law. That under-claim is safer than fabricating authority, and the append-only index is never rewritten. After the cutover, the engine guard makes `operator` an earned value, so new operator rows are law even without a question.

There is no new recording gate, and none is needed. `crates/fno-agents/src/loopcheck.rs` already holds a session that closed its own question with an answer and emitted no matching decision. Its refusal already names the verb. That gate is starved, not missing: only two `operator_question` events exist across 194,109 events.

## Three local stores plus shipped design law

| Store | What it is for | Path |
|---|---|---|
| Project journal | Durability and the project audit trail | `<canonical-repo>/.fno/events.jsonl` |
| Decision index | Local project-policy recall | `~/.fno/decisions.jsonl` via `paths.decisions_jsonl()` |
| Graph projection | The node view, for anyone reading the node | the subject node's `decisions` array |
| Repository catalog | Reviewed design law that every clone inherits | `docs/architecture/decisions.yaml` |

One `fno backlog decide` call writes the journal, then the index, then the projection. A failed index write is not a success: the command exits 1, because a write the operator cannot read back is worse than a refusal.

It does NOT ask for a retry. The durable event has already landed by then, so a second run records one ruling twice under two ids. Both producers say so and name `fno backlog decide-reindex` as the recovery.

A failed PROJECTION does not fail the command at all. Both durable stores already hold the decision by then, so the ruling is recorded and recoverable. Only the node view is missing, and the command says which decision id it is.

The repository catalog is read-only to `fno backlog decide`. A repository change adds or revises design law through normal code review. The reader composes catalog rows with the local index by decision id. It uses reviewed content and keeps machine-local audit fields. Project policy can structurally supersede a repository default. The existing operator-law guard still applies.

## Why the index is separate from the global journal

The event stays project-local because it is durable there. `cli/src/fno/events/gc.py` refuses to compact the global journal but does compact project journals, and it deletes rows classified `retention: ephemeral`. `operator_decision` carries an explicit `retention: durable` key, so the GC keeps it.

That key changes no behavior today, and it is not decoration. The schema default is already `durable`, so the record survived by inheriting it. The whole recall promise rested on a default three thousand lines away, one edit from not holding. The key states the guarantee at the entry instead.

Recall needs one machine-wide file, and two candidates were rejected.

- The global journal rotates. `~/.fno/events.jsonl.1` already holds 13.5 MB here. A rotated-away ruling is the exact failure this record exists to prevent.
- Folding every project journal per read does not scale. The machine-wide graph names 83 project roots, and their journals total 793 MB.

So the index is its own file, it never rotates, and it holds nothing else. It carries the same `operator_decision` envelopes and is written with the same `append_event`, so it inherits validation and the mutex and needs no second parser.

## The subject convention

When a node exists, use its id. Otherwise use `pr-<n>` or a catalog canonical subject. A checked-in alias is accepted. Durable design work must use the canonical spelling so a new synonym does not split history.

The reader takes every subject the writer takes. That is the defect this page was written for. The writer accepted free text while the reader resolved a graph node first, so a ruling about `pr-923` was written, receipted, and lost.

Both sides expand, not just the query. A subject that names a node answers to every spelling of that node: the id, the slug, any case. The operator records under whatever was in front of them. The receipt then prints the canonical id as the way back. A reader that expands only the query sends them to a command that returns nothing.

A subject that names no node and no catalog alias matches itself and nothing more. A decision about `pr-92` never answers a query for `pr-921`.

A decision with no subject at all is reachable only through `fno backlog decisions` with no `--subject`. When the question names no node, that is what `fno outstanding clear --answer` writes.

## Supersession

`--supersedes <decision-id>` overturns an earlier ruling. The older row stays and is marked `[superseded by ...]`, because a reader of an overturned decision must be able to tell it is not current.

The graph projection stamps that mark at write time under the lock. The index and repository catalog are append-only histories from the reader's perspective, so the reader derives `superseded_by` from the combined rows it scanned. Catalog validation refuses a missing target or supersession cycle.

## Backfill

`fno backlog decide-reindex` makes the index a superset of what already exists. It folds the journals first, then every `decisions` array on the machine-wide graph, and appends anything whose `decision_id` the index does not already hold.

Journals win a tie because a journal holds the event as written. A projection row is derived and can be lossier: the oldest one on this machine dropped `subject`, the one field a recall query reads. A projection row with no subject falls back to the node it sits on.

The command is idempotent by decision id, so a second run adds nothing.

## Lifecycle and standing law

The index remains append-only. A retraction is a new `decision_retracted` event, never an edit or delete: `fno backlog decide-retract d-1a2b3c4d --reason "the decision no longer applies"`. The command resolves the target first, refuses a blank reason or unknown id, and requires the operator lane to retract law. If the project journal is durable but the recall-index append fails, the command names `fno backlog decide-reindex` and never recommends retrying the retraction.

Every row carries a derived lifecycle: `live`, `expired`, `superseded`, `retracted`, or `unscoped`. Human output leads with that marker, and JSON includes `lifecycle`, reason, and any positive closure evidence. `--state live|expired|superseded|retracted|unscoped|all` filters the same projection. If a coord row is stamped to a node, it expires only after that node's graph entry has positive closure evidence. If a repository-scoped PR row exists, it expires only after its exact graph binding is marked merged. Missing, ambiguous, or unreadable closure evidence is `unscoped`, never live. Law does not expire because a node or PR closes.

The standing query is law-only and lifecycle-filtered: `fno backlog decisions <topic> --lane law --state live`. Peer-mail trailers use that exact query. JSON adds the canonical subject and a `current_law.status` of `single`, `conflict`, or `none`. Human output prints `CURRENT LAW`, `LAW CONFLICT`, or `NO CURRENT LAW`. Only `single` is an actionable current answer. Conflict never chooses the newest, and catalog damage is a nonzero read failure rather than `none`. A live coord row, an expired coord row, superseded or retracted law, and an unattributed row cannot authorize an outward or irreversible action.

## Review and export

`fno backlog decisions --review-list` is a non-mutating operator report. It groups canonical subjects with more than one unrelated live decision after alias resolution, supersession, retraction, and coord expiry have been applied. It names every candidate id, lane, timestamp, and decision, and never chooses a winner or writes the index. Its conflict membership uses the same combined projection as the direct standing query. Legacy subjectless rows remain visible under `(unscoped)` and are counted as data-quality findings. New answers to subjectless outstanding questions use `question:<question-id>` as their recovery subject.

`--output PATH` writes the complete requested report, ignoring the display limit. `.json`, `.md`, and `.markdown` infer the format. `--format json|markdown` is explicit, and conflicting or unknown formats are refused. The command prints a positive receipt with the exact path and byte count only after the file is written.

The upstream provenance seam owns mail-origin classification, stamped carriage, and the law chokepoint. The law command owns human-origin proof, law recording, and coord-to-law promotion. This lifecycle feature consumes those seams and owns expiry, retraction, review, export, and law-only reads. It does not add another origin classifier, law command, promotion path, or decision store.
