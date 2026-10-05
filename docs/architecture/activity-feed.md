# The activity feed

A browsable history of what happened across the fleet, in order, with a deep link per row into the session that produced it. Open it in the mux client with `e`.

## The stores, one timeline

Several stores hold one timeline and nothing joined them. The feed is one projection that joins them at read time.

| Store | Holds | Contributes |
|---|---|---|
| `~/.fno/db/events.db` | question and team lifecycle, spawn-gate refusals, stalls, distress, daemon starts, update runs - read typed through the `events_type_ts` index | `question_asked`, `question_closed`, `decision_recorded`, `day_boundary`, `team_granted`, `team_vacated`, `session_spawned`, `session_spawn_refused`, `worker_stalled`, `help_emitted`, `update_started`, `update_finished` |
| `~/.fno/graph.db` | node lifecycle as fields: `created_at`, `sessions[].started_at`, a ship-phase row beside `pr_number`, `merge_status`, `completed_at` | `node_created`, `node_started`, `node_shipped`, `pr_merged`, `node_ended` |
| `~/.fno/agents/events.db` | the agents journal: mux close rows, spawn events, daemon starts | `session_spawned`, `session_spawn_refused`, `pane_closed`, `server_stopped`, `composer_shell_ran`, `composer_shell_refused`, `daemon_restarted` |
| `~/.fno/agents/reap-receipts/` | one durable receipt per removed registry row, each carrying the verbatim resume line | `session_reaped` |

A store qualifies for three reasons. It is durable. It does not rotate. It IS the record, not a restatement of one.

The durable db stores meet that test. `events.jsonl` rotates in about twelve hours, but its durable db copy keeps `retention_class = durable` rows long after the file rotates. The feed never reads the jsonl file: a typed read through the store skips the ticks and reaches rows from weeks back.

The feed reads the question, spawn and close kinds typed. The store's other kinds, 119k+ `attention_delivery` rows alone, never reach the parser. If the query's flags exclude every kind a leg can emit, the leg skips its read entirely.

The lifecycle kinds derive from the graph at query time, so the graph stays the one truth. No writer is added. A merged node reads `pr_merged`, not `node_ended`. The merge IS the end.

## The kinds and their areas

Every row carries an `area`: a pure function of the kind. The column renders in the panel and `--area` filters on it.

| Area | Kinds |
|---|---|
| `mail` | `question_asked`, `question_closed`, `decision_recorded` |
| `backlog` | `node_created`, `node_started`, `node_ended` |
| `ship` | `node_shipped`, `pr_merged` |
| `agents` | `session_spawned`, `session_spawn_refused`, `session_reaped`, `worker_stalled`, `help_emitted`, `team_granted`, `team_vacated` |
| `mux` | `pane_closed`, `server_stopped`, `composer_shell_ran`, `composer_shell_refused` |
| `fleet` | `day_boundary`, `daemon_restarted`, `update_started`, `update_finished` |
| `ci` | `main_ci_changed` (reserved; no writer yet) |

Two kinds have no writer yet: `main_ci_changed` (a durable CI source does not exist) and `mail_to_user` (no durable sink row exists). They are filed, not built here.

`worker_stalled` derives from a `worker_silent` event (the handle and its silent age). `help_emitted` derives from a `blocked` event (the distress kind and its reason). A spawn-gate refusal keeps the `session_spawn_refused` kind and carries the gate axis as its `reason`. The update trio folds into `update_started` and `update_finished`.

## Paging, and the window

The projection orders rows by a total key: parsed time, then kind, node, session, ref and title. Every row carries a `cursor` - that key as one opaque string. The client never parses anything else to page.

`fno agents feed --json --limit 200` reads the newest page. `--before <cursor>` reads the page strictly below a cursor. `--after <cursor>` reads the page strictly above one. Rows sharing a timestamp still page cleanly. Pages concatenate with no row read twice or skipped.

The panel holds a bounded window of at most 600 rows, three pages. Scrolling back near the oldest loaded row arms an Older page. If the cap must hold, the window drops its far end after prepending. Scrolling forward past a detached head arms a Newer page. Rows arrive live at the top. They never jump the view while you are scrolled back. The footer shows `↑ N new`. A click on that marker, or `g`, or the Home key, returns you to the top. Memory stays flat over a long scroll.

A page costs one projection run. The questions leg reads its kinds typed. The owner rollup indexes the graph once. The whole projection runs under a second where it ran over two.

## The verb and the panel

`fno agents feed [--since-epoch <secs>] [--until-epoch <secs>] [--limit <n>] [--before <cursor>] [--after <cursor>] [--node <ids>] [--session <id>] [--kind <prefix>] [--area <names>] [--agent <name>] [--harness <names>] [--lead <name>] [--json]` is the projection. A missing or unreadable store is not fatal. The rows the other stores yielded still emit, with one stderr line naming the store skipped.

The filter flags AND together, and the values inside one flag are comma-OR. `--kind` matches by prefix, so an exact kind is its own prefix. An empty flag value is ignored. An unknown area returns an empty answer, not an error. Both epoch bounds reach every event-store leg as real store bounds.

`e` in the mux client toggles the full-height panel on the right edge. Rows render newest first as a table: time, area, harness, kind, node, session tail, lead, summary. A narrow panel drops lead, then harness, then area, then session. Time, kind, node and summary never drop. The border drags to a width that persists.

The panel holds a bounded window of at most 600 rows (three pages). Rows arrive live at the top and never jump the view while you are scrolled back. The footer shows `↑ N new`, and a click on that marker (or `g`, or the Home key) returns you to the top and clears the marker.

The bottom row carries the hints and the status. The hints are the `↑ N new` marker, the active query and the keys. The status is the row count, `end of history`, a scan note, or the typed error. The header is a title only.

### Keys

Focus with `E`. An unfocused panel takes no keys at all.

| Key | Does |
|---|---|
| Up, Down | select a row |
| Left, Right | pan the summary in display columns |
| PageUp, PageDown | select a viewport at a time |
| `g` or Home | jump to the top; on a detached head it arms a fresh Head |
| `G` or End | jump to the oldest visible row |
| `o` | toggle grouped/recent order |
| `?` | toggle the keys overlay |
| `/` | open the search bar |
| Enter | open the row's provenance |
| Esc | release the keyboard; the overlay unwinds first if it is open |

`?` toggles the `feed keys` overlay. The panel's keys, as it renders them:

- `up/down row - enter details - o order`
- `g home (newest) - G oldest - arrows pan`
- `/ search - ? keys - esc close`

### Search

`/` opens a query bar on the footer line. The bar speaks the shared search grammar - the one parser the feed, the mux backlog search and the web board share. Space ANDs, comma ORs inside a key, `|` ORs across terms, and a leading `-` negates. A term the grammar cannot read shows the refusal in the footer verbatim and fetches nothing.

The query keys the feed answers, as the `?` overlay renders them:

- `id: n: - value prefix`
- `session: sid: - value prefix`
- `spawner: by: - value prefix`
- `agent: a: - value prefix`
- `actor: - value prefix`
- `pr: - value prefix`
- `project: proj: - value prefix`
- `epic: e: - value prefix`
- `in: - value prefix`
- `lead: l: - value prefix`
- `area: ar: - value prefix`
- `harness: h: - value prefix`
- `model: m: - value prefix`
- `effort: ef: - value prefix`
- `phase: ph: - value prefix`
- `kind: k: - value prefix`
- `reason: - text`
- `ts: at: - date`
- `is: - exact word`
- `has: - exact word`
- `age: - age`
- `title: - text`
- `details: body: - text`

Worked examples: `x-1234` (a bare node id), `h:codex k:node stall`, `sid:00bde302`, `-k:question h:claude`, `h:codex | h:claude k:pr`, `ts:>=2026-10-01`. `s:ready` refuses here: `s:` is node-only.

If the parsed query is plain positive terms on pushable keys, the client sends them as projection flags. The store does the filtering. Anything richer matches client-side over each landed page. Free text, `|` and a negation all land there. The scan caps at five pages per scroll gesture. The footer names how far back it reached.

Tab completes the token before the cursor. A key prefix completes from the shared table's keys. A value after `id:`, `sid:`, `a:`, `h:`, `k:` or `l:` completes from the distinct values in the loaded window. Esc in the bar clears the query and refolds unfiltered. The panel stays open.

## Day boundaries

`fno inbox day start` and `fno inbox day end` fold the existing project journal, question lifecycle, graph completion records, decision retractions, review retractions, and reign check-ins. The native fold returns JSON or a short text readback. The native verb's `--commit` writes one bounded `day_boundary` row to the project journal first. It then writes the same row to `~/.fno/questions.jsonl` for durable cross-rotation recall. The inbox relay only selects the destination, so the operator command lives under the inbox. The row carries ids and counts only. The writer sizes the row against the validated event limit before any write. If the row is over the limit, the writer refuses it and never substitutes.

The permanent question index stores the boundary reference because the project journal rotates at 8 MiB and keeps only one rotated file. The index is recall and provenance, not a second source of question truth. Open questions still come from the existing lifecycle fold, and a failed index append names the boundary id after the project append. The boundary id is stable per kind and local day. A retry after that failure recognizes its own journal row and appends only the index leg, so no orphan row accumulates.

Each boundary uses a half-open interval `[from, to)`. The first boundary starts at local midnight. Later boundaries start at the newest saved cutoff. Repeated `start` or `end` on the same local day returns the saved boundary and does not append a duplicate or advance attention. Up to five question ids are featured. Three come from the shared queue order. The rest are questions that have not appeared in an earlier saved boundary.

An unreadable question store exits 1 and prints no zero-open line. A missing question store is a successful, explicitly incomplete read. The first line says `open questions: unknown` and the receipt names the missing path. A saved boundary is historical evidence. It does not replace current queue or merge authority.

## An actor is not a session

The questions store puts a MECHANISM in the field a session goes in. `operator_decision.decided_by` is the literal string `fno agents stale-escalate` on every row. `operator_question_closed.closed_by` is `stale-escalate` on most rows. Projecting those into `session_id` made the panel offer an attach the server answers with `no such agent`.

So the SHAPE decides. A value that reads as an fno session id (`20260904T151442Z-cl54345-58af0c`) becomes `session_id`. So does a harness uuid. Every other value becomes `actor`, which is provenance and never an attach target.

A closure and a decision carry only their `question_id`. The asking row is the one place their node association exists, so the projection indexes the asking rows and fills `node` from them.

It does NOT borrow the asking row's session. That session asked the question. It did not close it. A borrowed session in `session_id` is a guess wearing the clothes of provenance.

## The harness and the lead

A row with a session and no harness reads the lane its session ran. The projection builds one session-to-harness map from the graph's `sessions[]` rows and the spawn events. It joins on the session id. A session in neither source stays absent. No guess.

A row that rolls up to a held team scope carries `lead`: the crown holder's name, beside the `owner` spelling the panel groups on. A row with no node whose parent session IS a held crown holder's session rolls up to that holder.

## A click opens provenance, not a deep link

A click on a row opens that row's PROVENANCE view. It shows the nine facts the operator asked for, in their order, each saying how it is known.

Three of those nine are measured to be mostly unrecorded. So the view separates three silences and never leaves a cell blank:

| Marker | Means |
|---|---|
| `NOT RECORDED` | the source lacks the fact |
| `NOT APPLICABLE` | positive evidence the concept does not apply, as for a row that records no session at all |
| a named live state | a reading, not an absence: `not in the live roster`, `no seat · retarget portal 0`, the session was removed |

Which silence a cell shows is read from the ROW, never from its kind. A `node_ended` on a node that ran carries the last do or ship session, so its blank lane is `NOT RECORDED`. A `node_ended` on a node nothing ever ran carries no session, and only then is the lane inapplicable.

Pane, parent and king are a live lookup against the roster, joined on the exact `harness_session_id`. Never on the row NAME. A later worker can reuse a name, and the view then answers about a different session.

The deep link is this view's footer ACTION, not the gesture that opened it. Inspecting attaches and resumes nothing on its own.

Enter resolves from the same evidence the footer named. For a row seated in a pane it sends `FocusPane`. Pane ids allocate from zero, so pane 0 is a real seat and the check is an equality against the `Option`. For a live paneless row it sends `AttachAgent` on portal 0. For a `session_reaped` row it hands over the receipt's verbatim resume line. A removal is a normal outcome with a recovery path, not an error.

For a `node_created` row with a node id, the footer offers `b: blueprint`. Pressing `b` opens the node-bound launch composer with `/fno:blueprint <id>`. When the row has a recorded cwd, the composer selects that project. If it is absent, choose the project in the composer. The harness, model and placement choices remain available, and the composer shows the normal launch receipt. Other row kinds and id-less creation rows do not offer this action.

## Open questions in the sideline

Open questions also show as a block in the sideline, pinned above the court block. The block reads `fno-agents needs --items`, not the feed: the feed shows a question's history, and the block shows only what is open. An answer picked in the block's overlay records `sink: mux`, and the row shows the delivery rung for 15 minutes. See [attention-items](attention-items.md) for the delivery ladder.

## Deploy rule

The feed reads the durable stores and the graph for lifecycle. No writer is added.

When you want to surface a new operator-facing event, add it to a store that passes the test above. A typed row in db/events.db or the agents journal qualifies - give the type a required data shape and the projection a derived kind. So does a stamped graph field beside the row that produced it, and so does a durable receipt store. Never emit it into events.jsonl expecting the feed to mine the rotating file.
