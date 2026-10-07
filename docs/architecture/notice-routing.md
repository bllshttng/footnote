# Notice routing

Every session-start and hook notice carries a role audience: `worker`,
`lead`, or `user`. Each notice reaches the session that owns it, and a
warning no session should see is owned instead of broadcast. Designed in
x-f455 (grill 2026-10-06); the measured evidence lives on the node.

## Role audience

| Notice | Audience |
|---|---|
| Fleet-incident holds | every role (grill ruling: audience stays all) |
| Own questions, own claim, own lead's mail | every role, own scope only |
| `Waiting on you` projection (UserPromptSubmit) | lead and user |
| Outstanding fleet total line | lead and user |
| Operator-capture orders | lead and user |
| Reconcile warnings (promise gate, canonical sync, orphan plans) | the owning lead, by mail |
| Waiting-on-you questions | the user's session only |

The session role reads from one resolver: a live team's `holder_session`
makes a session a lead; a registry row with a node set makes it a worker;
anything else is the user. `fno-agents context-run --role <session_id>`
prints the role word, so hooks outside context-run read the same answer.
Every context-run producer runs with `FNO_SESSION_ROLE` set, and a role
read that fails reads as `user`, which is the pre-change behavior.

The producer declaration carries two optional keys. `audience` lists the
roles a producer runs for. `warning` marks the reconcile banner the fold
counts. Both are ignored by the Python census expander.

## Owner ladder

One module answers "who owns this" (`owner_ladder.rs`):

1. **NodeLead**: the deepest live team whose compiled territory holds the
   node (`territory::node_owners`).
2. **HigherUp**: the live teams one level above that lead (the same
   level-minus-one rule `lead_wake` uses, now one shared `rung_up`).
3. **LeastLoaded**: among live leads with a beat inside 7,200 s, the fewest
   held nodes plus open notices; a tie goes to the newest beat. Each pick
   records an `authority-router` decision.
4. **User**: no live lead qualifies; the caller files the question page.

A rung whose holder session equals the asker is skipped, so a worker never
routes to itself. An unreadable registry is `Err`, never an empty team
list, so the ladder never resolves out of a half-readable world.

## The notice_route arm

The daemon arm beats every 300 s over each workspace project root's
`.fno/.reconcile-result.json` and `.fno/.orphan-plans-result.json`. Each
warning resolves through the ladder; one deduped mail per owner goes
through `fno/notice-router` (the system-sender lane, with the resume
fallback). The mail carries the same cause text the hook printed. The
dedupe store is `~/.fno/notice-route/sent.json`: a sha256 over the sorted
`kind:node` pairs, kept 24 h. The arm renames each consumed result file to
`.shown` - the rename the hook did before. A user-rung group files one
question page through `fno inbox outstanding ask --question-file`.

The session-start hook keeps only its trigger duty: the retro-pending
advisory, the pr-watch self-heal, and the throttled reconcile fire.

## The repeat-failure and banner fold

An hourly pass reads the trailing 7 days of every `~/.fno/spaces/*/events.jsonl`.
The allowlist is event types ending `_failed` or `_dropped`. The key is
type plus the normalized error (digits, hex runs, uuids and absolute
paths collapse to one token; first 160 chars). Rules:

- 3 rows of one key in 24 h file one bug node tagged `failure-key:<sha8>`
  (first ts, last ts, count in the details), or one encounter on the open
  node the fold ledger maps to that key. Further copies count once per
  pass.
- A done node whose key recurs after `completed_at` reopens to triage.
- A 24 h count doubling over the prior 24 h, or a 3-day-old node still
  growing, mails the owner a question once per day.
- Steady-noise types (`transition_rejected`, `graph_status_drift`) file
  only past three times their 7-day daily average.
- A warning producer whose one content_hash reached 3 distinct sessions
  inside 24 h folds as `banner_repeated:<producer id>`.

The fold ledger is `~/.fno/notice-route/fold.json`; the node still carries
the tag a human can grep. `merge resets` the count window to
`completed_at` (the reopen rule reads it), and recurrence reopens.
