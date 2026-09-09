# The activity feed

A browsable history of what happened across the fleet, in order, with a deep link per row into the session that produced it. Open it in the mux client with `e`.

## The stores, one timeline

Several stores hold one timeline and nothing joined them. The feed is one projection that joins them at read time.

| Store | Holds | Contributes |
|---|---|---|
| `~/.fno/questions.jsonl` | `operator_question`, `operator_question_closed`, `operator_decision` rows | `question_asked`, `question_closed`, `decision_recorded` |
| `~/.fno/graph.json` | node lifecycle as fields: `created_at`, `sessions[].started_at`, a ship-phase row beside `pr_number`, `completed_at` | `node_created`, `node_started`, `pr_created`, `node_ended` |
| `~/.fno/agents/reap-receipts/` | one durable receipt per removed registry row, each carrying the verbatim resume line | `session_reaped` |
| `~/.fno/events.jsonl` | telemetry (72% ticks) | nothing - deliberately not read |

A store qualifies when it is durable, does not rotate, and IS the record rather than a restatement of one. The reap-receipts store meets that test: one file per removal, written before the row drops, holding the recovery path itself. `events.jsonl` fails it. It rotates in about twelve hours and carries a hundred noisy rows for every operator-facing one, and a feed that mines it re-derives the filter in every consumer. The lifecycle kinds derive from the graph at query time, so the graph stays the one truth. No writer is added, and no second statement of "a PR was opened" exists to drift.

## The kinds

| Kind | Derived from |
|---|---|
| `question_asked` | an `operator_question` row (ref: question id) |
| `question_closed` | an `operator_question_closed` row (ref: question id) |
| `decision_recorded` | an `operator_decision` row (ref: decision id) |
| `node_created` | a node's `created_at`, which every entry carries |
| `node_started` | a do-phase `sessions[]` row with `started_at` |
| `pr_created` | a ship-phase row with `started_at` on a node carrying `pr_number` (ref: PR number) |
| `node_ended` | a node's `completed_at`, session id from its newest do/ship row |
| `session_reaped` | a reap receipt, `detail` carrying its verbatim resume line |

## An actor is not a session

The questions store puts a MECHANISM in the field a session goes in. `operator_decision.decided_by` is the literal string `fno agents stale-escalate` on every row, and `operator_question_closed.closed_by` is `stale-escalate` on most rows. Projecting those into `session_id` made the panel offer an attach the server answers with `no such agent`.

So the SHAPE decides. A value that reads as an fno session id (`20260904T151442Z-cl54345-58af0c`) or a harness uuid becomes `session_id`; every other value becomes `actor`, which is provenance and never an attach target.

A closure and a decision carry only their `question_id`, so the asking row is the one place their node association exists. The projection indexes the asking rows and fills `node` from them. It does NOT borrow the asking row's session: that session asked the question, it did not close it, and putting it in `session_id` would be a guess wearing provenance's clothes.

## The verb and the panel

`fno agents feed [--since-epoch <secs>] [--limit <n>] [--node <id>] [--session <id>] [--kind <k>] [--json]` is the projection. A missing or unreadable store is not fatal: the rows the other stores yielded still emit, with one stderr line naming the store skipped.

`e` in the mux client toggles the full-height panel on the right edge. Rows render newest first, and the border drags to a width that persists.

### Two input states, and the header says which

The panel is chrome by default: it consumes no keys at all, so typing reaches the focused pane. That is deliberate, not a missing binding.

`E` focuses it explicitly. While focused it takes Up and Down (row select), Left and Right (pan the title in display columns, with the stamp, kind and node anchored), PageUp and PageDown, Enter (open the row's provenance), and Esc (release the keyboard, leaving the panel open). Closing the panel releases it too, so a reopen never starts holding the keyboard.

The header names the state it is in, and degrades to a shorter spelling on a narrow panel rather than clipping the focus key away. It is the only place that key is advertised.

### A click opens provenance, not a deep link

A click on a row opens that row's PROVENANCE view: the nine facts the operator asked for, in their order, each saying how it is known.

Three of those nine are measured to be mostly unrecorded, so the view distinguishes three silences and never leaves a cell blank. `NOT RECORDED` means the source lacks the fact. `NOT APPLICABLE` needs positive evidence the concept does not apply, as when a graph-derived row was never run by any session. A named live state (`not in the live roster`, `no seat · retarget portal 0`, the session was removed) is a reading, not an absence.

Pane, parent and king are a live lookup against the roster, joined on the exact `harness_session_id`. Never on the row NAME: a later worker can reuse a name, and the view would then answer about a different session.

The deep link is this view's footer ACTION, not the gesture that opened it. Inspecting attaches and resumes nothing on its own. Enter resolves from the same evidence the footer named: `FocusPane` for a row seated in a pane (pane ids allocate from zero, so pane 0 is a real seat and the check is an equality against the `Option`), `AttachAgent` on portal 0 for a live paneless row, and for a `session_reaped` row the receipt's verbatim resume line, because a removal is a normal outcome with a recovery path rather than an error.

## Deploy rule

The feed reads the graph for lifecycle - no writer is added. To surface a new operator-facing event, add it to a store that passes the test above: durable, non-rotating, and the record itself rather than a restatement of one. A questions.jsonl row, a stamped graph field beside the row that produced it, or a durable receipt store all qualify. Never emit it into events.jsonl expecting the feed to mine it.
