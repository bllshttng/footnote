# The activity feed

A browsable history of what happened across the fleet, in order, with a
deep link per row into the session that produced it. Open it in the mux
client with `prefix+e`.

## The three stores, one timeline

Three stores hold one timeline and nothing joined them. The feed is one
projection that joins them at read time.

| Store | Holds | Contributes |
|---|---|---|
| `~/.fno/questions.jsonl` | `operator_question`, `operator_question_closed`, `operator_decision` rows | `question_asked`, `question_closed`, `decision_recorded` |
| `~/.fno/graph.json` | node lifecycle as fields: `sessions[].started_at`, a ship-phase row beside `pr_number`, `completed_at` | `node_started`, `pr_created`, `node_ended` |
| `~/.fno/events.jsonl` | telemetry (72% ticks) | nothing - deliberately not read |

`events.jsonl` is excluded on purpose: it rotates in about twelve hours and
carries a hundred noisy rows for every operator-facing one. A feed that
mines it re-derives the filter in every consumer. The lifecycle kinds derive
from the graph at query time, so the graph stays the one truth - no writer
is added, no second statement of "a PR was opened" exists to drift.

## The six kinds

| Kind | Derived from |
|---|---|
| `question_asked` | an `operator_question` row (ref: question id) |
| `question_closed` | an `operator_question_closed` row (ref: question id) |
| `decision_recorded` | an `operator_decision` row (ref: decision id) |
| `node_started` | a do-phase `sessions[]` row with `started_at` |
| `pr_created` | a ship-phase row with `started_at` on a node carrying `pr_number` (ref: PR number) |
| `node_ended` | a node's `completed_at`, session id from its newest do/ship row |

Every row carries the node id and session id the deep link needs.

## The verb and the overlay

- `fno agents feed [--since-epoch <secs>] [--limit <n>] [--node <id>] [--session <id>] [--json]` -
  the projection. A missing or unreadable store is not fatal: the rows the
  other store yielded still emit, with one stderr line naming the store skipped.
- `prefix+e` in the mux client - the overlay. Rows render newest first;
  j/k move the cursor; Enter deep-links the row: a row that joins a live
  sideline row resolves through `agent_hit` exactly as a sideline click does
  (FocusPane, or AttachAgent on portal 0); an unjoined row with a session id
  attaches that id, and the server's existing `no such agent` notice is what
  a dead session answers.

## Deploy rule

The feed reads the graph for lifecycle - no writer is added. To surface a
new operator-facing event, add it to one of the two stores the feed already
reads (a questions.jsonl row, or a stamped graph field beside the row that
produced it). Never emit it into events.jsonl expecting the feed to mine it.
