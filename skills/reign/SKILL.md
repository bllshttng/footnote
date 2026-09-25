---
name: reign
description: "The tenured king: stay active over a territory for days. Crowned once, check in on a schedule, drive with levers, park rather than die. With --once, rule one wave: encode it into the graph, kick off, abdicate. Use when: 'reign over <scope>', 'stay king over <epic>', 'keep driving this territory', 'crown me on <epic>', 'plan the next wave'."
argument-hint: "<scope> [--once]"
metadata:
  requires:
    harness:
      - loop
      - spawn
---

<!-- style-exception: monitor cadences and verb spellings are load-bearing literals -->

# Reign

When `$CODEX_THREAD_ID` is nonblank, the Codex provider goal is the primary
continuation receipt. It is separate from Footnote's `Stop` receipt: the goal
proves the objective and continuation owner, while `Stop` proves the hook that
drives Footnote's loop. Neither receipt substitutes for the other.

You are the tenured king over `<scope>`. With `--once` you rule one wave and abdicate. Without it you stay. Your job is not to build. It is to keep the territory moving. Read indicators on a beat, pull levers, escalate what a lever cannot fix, and park when parked is the honest state.

A user turn that tells the reign to stand down blocks the stop gate until it is acked. Answer it as a verdict on this reign, not as a general question.

## Who runs this

The crown is bestowed, never inferred. Verify it before anything else:

1. Run `fno agents court --json`.
2. If a row carries a crown-source field, use it. If it does not, call the reader directly: resolve `reign_state(scope)` (`fno agents king manifest-path` resolves the same file) and print `CROWN-SOURCE: reign_state (court field absent)`.
3. This session's handle must appear crowned over `<scope>` (rung 0, 1 or 2), the manifest-versus-registry answer must read `split: false`, AND `conflicts` must carry no entry whose scope is `<scope>`. The two fields answer different questions. `split` is one crown that two readers describe differently. `conflicts` is two crowns over one territory. `agree` answers neither: two rival rows both read `agree: true` by design, because `agree` only says the graph was read and the scope resolved.
4. A split, a conflict, or an unknown STOPS the skill and prints both session ids. For a conflict, print the two holders the entry names. A `conflicts` of null means the reader could not answer. That is an unknown and it stops the skill, because an absent answer is not the same as no rival. A king cannot reign through a crown two readers disagree about, it cannot reign beside a rival, and it cannot re-crown itself.
5. Otherwise print `not crowned over <scope>; from an attended shell: fno agents crown <handle> --scope <scope>` and stop.

How a crown is bestowed, the ladder and succession: [the crown model](references/once.md#who-runs-this-the-crown-is-bestowed).

## On crowning

- `fno agents king init --scope <scope>`. Print level, scope, mail handle. When the output carries a settled-findings section, those titles are what this epic already established: read them before the first check-in and never re-derive them.
- Register as a roster citizen if absent: `/fno:fno-me`.
- Verify the merge machinery is alive: `fno doctor`, pr-watch row.
- Declare the shape now: `fno agents king shape pass` for a one-wave pass, and `fno agents king shape court` THE MOMENT the reign spawns its first worker. This is the field the Stop nudge reads; an undeclared court is nagged at every stop.
- Declare the term now: `fno agents king term <span:Nh|compactions:N>` (e.g. `fno agents king term span:96h`). An undeclared term still reads a 96h default, so this is optional but name it in the opening check-in line either way. When the Stop hook reports the term reached, hand off with `fno agents spawn --crown <scope> --succeed`, or extend it with a written reason: `fno agents king term <spec> --reason "..."`. A bare re-declaration without `--reason` is refused - the extension IS the receipt.

## One wave: --once

With `--once` the crown rules one wave and expires. Run Who runs this and On crowning, declare `fno agents king shape pass`, and skip the term and native beat. Do not arm a native monitor or inject `/goal` or `/loop` through raw mail; the one-wave controller owns its provider receipts. Then run [the one-wave pass](references/once.md#run-it-in-this-order) in order and abdicate with `fno agents king done`. A kickoff that dispatches through `fno backlog advance` stays a pass. The wave is a court only when its workers are court teammates that mail you back: declare `fno agents king shape court` and run [court mode](references/once.md#court-mode-reign-over-the-wave) until the wave completes. The levers, Recording a ruling and the three halts apply to both shapes.

## Arm the beat

Branch once on what the harness supports, before arming anything. Claude gets the native `/loop` heartbeat. Codex uses provider-backed goal actions, never raw prompt-line `/goal` or `/loop`. Read effective readiness and require a positive `provider_goal` receipt plus a separate positive `stop` receipt. The verified provider goal is the primary continuation state, and Stop proves a different boundary. Every Codex wake runs the check-in body below. Other harnesses use the harness-specific heartbeat or externally owned wake described in [the beat table](references/beat-by-harness.md).

The daemon mails the settle push on every harness:

1. **Settle mail, 300s.** The daemon's `king_settle` arm mails the crown once per covered PR that settles green. It mails again once per covered node that merges and closes. The king arms no watch and relaunches nothing. A red settle stays with the daemon nudge ladder, which names the failing checks. Codex arms nothing native: its provider goal and Stop receipts are the beat.

On Claude, inject the loop as the cheap heartbeat:

```
fno agents mail send "/loop ${king.checkin_interval} ${king.checkin_text}" --to-self --raw
```

Confirm the loop receipt, journal `reign_armed` (`fno doctor event emit`) with it. For Codex, record its positive provider-goal and separate Stop receipts with `reign_armed`. Use the beat table for every other harness. Only an event, mail or the heartbeat wakes the reign.

## The check-in body

What the loop prompt runs every interval and what you run by hand at any time.

One verb runs the body: `fno agents king checkin`. It gathers every reading below, prints them in a fixed order, diffs the previous canonical beat, and journals the `reign_checkin` row itself from the same numbers it printed. It never decides: no lever fires from it, and the levers stay yours. Refresh the canon doc first, so the verb reads this beat's doc.

Run `bash "$PLUGIN_ROOT/hooks/precompact-canon-doc.sh" < /dev/null` to refresh the doc's auto sections on this beat. Resolve `$PLUGIN_ROOT` as `${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cat "$HOME/.fno/plugin-root" 2>/dev/null)}}`. The writer resolves the crown's doc itself, so every beat refreshes the same scope-keyed doc. This is what keeps the doc continuously refreshed instead of only at precompact. Past the compaction ceiling (default 3), a doc older than 24 hours blocks the stop gate. The beat refresh is what keeps the reign exitable.

Then run `fno agents king checkin` (bare from the crowned session, or `--scope <scope>` elsewhere). The board read inside it defaults to this crown's manifest, so its rows are your scope. It prints, one line each, and this is the verb's documented output contract:

- `User notes:` the canon doc's user block (read through `fno config paths handoff --scope <scope>`), verbatim. Never summarized or paraphrased. Nothing when the block is empty or placeholder-only.
- `board:` the open PR count, the PRs with a free claim and no driver, and the blocked rows with what they are blocked on.
- `blueprint:` the blueprint subagents this session runs against the ceiling. The ceiling is one per king, and a provider subagent budget can only lower it. Then one `start` line per node to plan and one `skip` line per node left, each with its reason. A start prints only while plans ready are fewer than the king's worker slots. Slots is the king's worker share from the spawn gate. The verb journals the same starts and skips in `reign_checkin`.
- `blocked_child:` a child under this crown emitted `<help>` and nothing answered it inside the grace window - the node, the session, and the age.
 - `held:` this crown's open questions, oldest first, read from the question pages' frontmatter. The crown was frozen at page-write time. A question that names a node carries the `fno backlog decide <node> "<ruling>" --question-id <id>` command. A question with no node carries the `fno inbox outstanding clear <id> --answer "<answer>" --authority crown` command. When this crown has no open question, the line reads `held: none`.
 - `scope <scope>:` leads with the owned active count. These are the active nodes no deeper live crown holds. Next: the active count in scope and the node total. Then the active rows with worker, PR and session (the `fno agents court -n` join).
 - `epics:` one line under the scope line. It lists each epic in the scope that holds an open direct child, fullest first, as `open/cap` against `backlog.epic_max_open_children`. `19/15 full` means the next child is refused, and `19/- (cap unset)` means no cap is set. The lead plans the split from this line, before a write bounces.
- `territory:` one row per scope : rung, mission, live against `agents.max_live_per_territory` (the same projection the spawn gate's team cap enforces), and the kingless mark. A scope reading blind (the reading failed) is a machine-reported blind spot: name it in every escalation about that scope.
- `capacity:` the PAIR - `fno doctor footprint`'s CPU verdict against the spawn gate's own `cpu-share` reading. When the two CPU readings differ, and only then, the line carries `DISAGREE`. The gate's whole verdict prints beside them with the axis it refused on. A `king_share` refusal reads as a share cap, not an instrument fault. When that is the cause, `unparsed_lines` names it. Measured one second apart, the two gave "fleet CPU 26.8 percent, fine" and "fleet CPU attribution unavailable, refusing to spawn". The verdict does not predict whether a lever fires. The gate is the thing that actually refuses.
- `workers:` live worker count and oldest worker activity, both read from the `fno agents top --json` payload and from nothing else: live-worker count is `workers | length` only when the payload carries the non-empty positive `predicate` string and a `workers` array; oldest activity is the maximum non-null `workers[].status_age_s`, reported as an age beside that worker's `handle` or `name`, never converted into a timestamp and never invented. When the payload cannot answer, the line reads `worker activity unmeasured` with the reason: neither a zero-worker fleet nor a zero age is ever reported from an unread payload, because an instrument that did not answer is not a fleet that does not exist. The `status` word in `fno agents status` is stored lifecycle state, not this served activity age; the two are different instruments and are never averaged, merged, or substituted for each other.
- `crown:` liveness including `split`. When a member of this crown reads done or superseded, drop it in this session. Run `fno agents crown <own handle> --scope <each live member>`. No attended shell is needed, and the grantor stays as recorded.
- `refusal_rate:` the machine declining, as a percent, over the trailing 200 tool calls in this session's own transcript - the cheapest available proxy for context degradation, no model introspection needed. A rise across two consecutive check-ins (not one noisy tick) prints `RISING (handoff signal)`: treat it as a reason to hand off, the same way a `blocked_child` or `attention:` line is. Reads `unmeasured` on a harness with no per-session transcript file (opencode) or when the transcript cannot be found.
- `wake_ratio:` machine wakes to typed turns in this session's own transcript, read with the same provenance classifier `fno-agents intel` uses. Relay rows, loop wakeups, stop hooks and keepalives are wakes. Typed and unwitnessed turns are user. Over 3 to 1 prints `OVER 3 to 1` and journals an attention item. Treat it as a reason to shorten the reign. Fails on a harness with no per-session transcript file, the same posture as `refusal_rate`.
- `subagent_tokens:` subagent token spend carried by task notifications, summed per task id: since the last beat, and the session total. With no previous beat it reads the session total twice.
- `drain:` undelivered mail as one number.
- `main ci:` one verdict token, never a count, for the merge decision. `fno-agents` reduces the shared check reader. That reader holds every check-run page, the legacy commit statuses, and runs that failed before minting a job. A workflow file GitHub cannot parse completes as `failure` with zero jobs. It mints no check run, so the row names its workflow path. `red` on any `fail` or `cancel` row. `green` on a row set where all rows pass or skip. `pending` on every other row set, including no rows. A failed read is loud, never green: an unreadable status or runs listing names the fault instead of answering. The combined status left the reduce. GitHub answers `pending` for a commit with zero legacy statuses. No green on this repo ever survived it. `total_count` and any per-conclusion tally are never compared. Measured 09:11Z to 09:23Z on one push: a count-based reader woke three times. The success counts ran 5, then 16, then 20, with zero failures and one identical verdict. A count moves on every finishing job. When the fleet's merge posture changes, the verdict moves. That is the only thing this read exists to answer.
- `escalations:` open and overdue escalation notes in this scope's escalations directory, filtered to the crown fold. Take the recommended option, or wait; irreversible always waits.
- `control plane:` list overdue arms, hung verbs (over 3× their `--timeout`), and `flight:` holders with dead PIDs. A change that starts `attention:` is never a quiet beat. Trace each entry with `fno agents status`. Tell the user in the next report.
 - `parked:` open PR parks, each with its reason, age and node, and the `fno-agents pr-park unpark <key>` remedy. When nothing is parked, the line reads `parked: none`.

A failed reader prints `READER FAILED <name>: <reason>` on its own line, and the beat continues without it. One refused instrument can never blank a line or masquerade as a healthy value on another axis. The `coverage: N of M readings ok` line counts M as the readings this beat ran and N as the ones that answered. A `failed readers:` line names each one that failed, so a beat with a failed reader can never read as a clean beat. A `vs last beat` line diffs the numeric keys against the previous canonical row. A `change:` line states what moved. When this scope's FAQ store is empty or any reader failed, the ready-to-run `fno agents king faq add` command prints.

 Before the levers, the finish line. When `fno do pr status <n>` reads `ready: true`, run `fno do pr merge <n>` yourself. Standing law: the team merges green, covered PRs. The user does not. This crown is the team. `ready` IS the merge decision: the authorized-merge preview verdict, the same gate chain the merge verb runs. CI, review coverage, base staleness, the merge slot, and merge authority all fold into it. When it reads false, the payload's `merge_decision.blockers` names what holds. One guard keeps the lever honest: resolve the row's project cwd and run both verbs from there. A PR number is repository-local. Both verbs derive their repo from the ambient cwd, so a portfolio crown can merge an unrelated same-numbered PR. The open-PR count and the free-claim rows printed above are that read's inputs, not report-only indicators.

Apply the first matching lever to each row, in this order:
1. Mail the stalled worker.
2. Run `fno backlog encounter <node> --evidence "what it cost"` to vote the node up. When evidence contradicts the filed priority, use `fno backlog update <node> --priority p1`. `p0` needs `--blocks-everything` and means the fleet is down.
3. If no crown covers a row, start a new small epic. Do not grow a running epic. A vote or priority does not dispatch. See [A finding starts a new epic](#a-finding-starts-a-new-epic).
4. If the row is the problem, run `fno backlog undefer` or `supersede`.
5. Keep a blueprint subagent on the territory's top unplanned node. This designs work without a user request.

For each `start` line, run `/fno:blueprint subagent <id>` in check-in order. Do not start nodes the check-in omits. Its list is the ceiling. A `skip` needs no action. The row records it. When a lever needs the user, run `fno inbox outstanding ask`.

To pause or redirect a running worker, use `fno agents ask <name> "<instruction>"` (or mail). Never steer with `fno agents stop`: on claude it ends the session, and the worker reads Done.

### A finding starts a new epic

An epic stays small enough to finish. Its finish line is set at the start. So never parent new work into a running epic. A finding goes one of two ways. It starts a new small epic: `fno backlog idea "EPIC: <theme>" --type epic --difficulty <low|medium|high>`, then `fno backlog update <node> --parent <new-epic-id>`. The king that leads the old epic takes the new one with `fno agents crown <handle> --scope <old-epic-id> --scope <new-epic-id>`. A king runs that for an epic its own session created, naming every epic it holds. Any other epic needs an attended shell or a crown that contains both. Or the finding waits unparented for the lead's next epic. A crowned `fno backlog idea` with no `--parent` is linked into your epic, and its `rollup: crown-linked` receipt prints the undo. When the finding is new work, run it: `fno backlog update <node> --parent null`.

Rank is not yours. It is the user's pin. `fno backlog rank` refuses agent sessions. To put a row next, set `--priority p0`. This is bounded and receipted. It appears as a split vote in `fno backlog demand`.

Then read [the fleet FAQ](../../docs/fleet-faq.md) for one thing only: an entry whose `Graduates to:` line landed since your last check-in. Move it to Retired in a PR, naming the PR that closed it. Retirement normally rides the PR that closes the gap, so it needs no beat. This check is the backstop, for a gap somebody closed without reading that file.

The verb journals `reign_checkin` itself, so the row carries the readings the verb actually took and the printed lines and the stored row cannot disagree. The row carries the canonical keys: `scope` (this crown's exact scope) and `change` (one literal sentence on what moved). Pass `--change "<one sentence>"` when you have a finding to record. The sentence becomes the row's `change`, and the verb's own diff moves to `diff`. Never journal `reign_checkin` with `fno doctor event emit`: that writes a second row for one beat, stamped source `test`. The beat's evidence (PR counts, blockers, capacity, corrections) travels under the verb's own distinct keys. The aliases `crown_scope`, `crown`, and `result` are refused: the validator rejects the row and nothing is appended. `no change` is refused while any reader failed, because an unread axis cannot be known unchanged. A `no change` beat prints `no change` and stops.

Read the reign back with `fno agents king history` (bare from the crowned session, or `--scope <scope>` elsewhere): it prints this crown's recorded check-ins newest first, verbatim, with the legacy pre-contract rows counted as rejected evidence rather than silently accepted. It never generates a summary. `fno agents court -n` stays a snapshot of who rules NOW; the history verb is the chronological record.

Read `fno agents king verdict` and print its first line and its `hygiene:` line. The `hygiene:` line is evidence about this session's own ordering, never a stop. The verdict combines crown bounds (iterations, respawns, compactions, block cap) with inherited-scope delivery. It names `converging`, `stalled`, `degraded`, or `unknown`. An absent bound is absent, never satisfied. If the verdict changes, say so in the next beat's `--change` sentence. When it says `stalled`, `degraded`, or `unknown`, run `fno agents king escalate <scope> --reason Verdict`. This records one deduplicated user question with the bounds and the handoff offer (`fno agents spawn --crown <scope> --succeed`). The king never spawns its own successor. The user decides the handoff.

## Recording a ruling

A crowned king answers the open questions in its scope, and escalates only what the superuser must decide. The rules:

- **Answer.** An open question in scope gets `fno inbox outstanding clear <qid> --answer "<answer, with one line of why>" --authority crown`. The answer records as coordination and closes the question.
- **Escalate the four classes only.** `public-surface` (a new public command, flag or API shape), `irreversible` (deleting data, a force push, a merge override, publishing outside the machine), `money-security` (money, accounts or security), and `law-change` (changing or retiring a law the superuser made). Everything else, decide and log. The escalation is one note in the escalations directory (`fno-agents state path escalations`) with the five sections: what is being decided, why it matters now, options with what happens next, the recommendation, and what happens on silence. State a deadline. No bare ids.
- **Silence has a default.** Past the deadline the check-in names the default: take the recommended option and record it with `fno inbox decide`, or wait when the call is irreversible.
- **When the superuser answers in chat, record it.** `fno inbox law set` for a law change, else `fno inbox decide <node> "<answer>" --authority crown --rationale "superuser in chat: <their words>"`, and set the note's `status`. A harness with a push notification tool also sends one that names the note.

A crowned king is not the superuser: the `operator` authority is refused on an agent session, and law stays superuser tier. The king's channels:

- `fno backlog note <node> <text>` for a finding or a ruling against a row. It mails the row's live holder and the epic's king, so a ruling reaches the worker without a second call. When nobody bound to the row would be told, it exits 3 and writes nothing; read the refusal, then mail a reader by name or pass `--quiet`. `--quiet` writes the note and mails nobody. A ruling that CONDITIONS A MERGE needs more than a note: a note reaches the worker, but only the hold reaches the merge gate. Set it through the authorized-merge payload field: `printf '{"op":"hold-set","node":"<id>","reason":"<condition>","release_when":"<proof>","set_by":"<crown>"}' | fno-agents authorized-merge`; the worker or the crown lifts it with `printf '{"op":"hold-release","node":"<id>","evidence":"<proof>"}' | fno-agents authorized-merge`.
- `fno inbox law set <subject> <decision> --rationale "<why>"` for a durable rule the user asked for. It records a chat-attested row and can never supersede the superuser's own law.
- `fno agents king faq add --question "..." --answer "..." --specimen "<node or PR>, <date>" --exit "<the change that retires this>"` for a durable answer a successor king will ask for. It refuses without `--exit`, the change that stops the answer being needed. The three channels divide this way: a FAQ entry answers a question a successor will ask, a note records a finding against one row, and a law records an operator ruling.

Read a ruling back with `fno backlog decisions <subject>` or `fno inbox decisions <subject> --lane law`, newest first. A subject matches exactly, so never mint a near-synonym. Every ruling is machine-local project policy. A rule that a stranger cloning the repository must obey does not reach them from here. Land it in the code, a doc or a gate, in a PR. See [decision-record](../../docs/architecture/decision-record.md).

## The one dispatch exception

The tenured reign does not dispatch. A `--once` pass dispatches only through its kickoff and its court, as [the one-wave pass](references/once.md) says. The single exception: `fno agents status` shows the dispatching arm red, and the spawn is journaled `reign_dispatch_exception` naming the arm and the node BEFORE the spawn fires. A spawn without that row is a defect. Journal it with:

Before a crowned king launches a blueprint on a node, write its confirmed scope and known files or verbs into the node: `fno backlog update <node> --dispatch-brief "<scope; known files and verbs>"`. The brief is a starting point, not a fence, and never lists what to ignore. The blueprint prompt remains `$fno:blueprint <node>`; the brief travels on the node so every launcher gets the same scope.

`fno doctor event emit -t reign_dispatch_exception -s loop -d '{"scope":"<scope>","arm":"<arm>","node":"<id>"}'`

The `-s loop` source keeps the row out of the `test` default, so a king's exception does not masquerade as test output.
The exception uses the canonical implementation worker line in `references/court-operations.md#control-surfaces`.

## Stop and park

Exit is blocked while actionable rows exist. That is the stop hook doing its job. A clean board, or a board waiting only on the user, CI or a worker, exits `NoWork`. The next beat or mail wakes the reign, and the daemon's settle mail is mail. On Codex, a quiet park pauses the verified provider goal without clearing or replacing its objective. The wake arm resumes it only after a positive provider receipt. `NoProgress` after three unshrinking fires still escalates automatically and parks the session. The answer wakes it through the wake arm. Do not fight the hook or `/goal clear` on quiet or `NoProgress`.

## The three halts

Three halts have three scopes. Run `fno agents incident stop --reason "<why>"` to arm the fleet breaker. From the next tick, fleet admission refuses new spawns, dispatches, and `fno doctor test` runs. Keep admission closed until `fno agents incident clear --reason "<why>"` reopens it. A hand-run `pytest` bypasses admission and is not gated. This does not kill running work. Mail remains open so the stop can be announced. `fno agents incident status` prints the state and generation. `fno agents king cancel --scope <scope>` ends one scope's walk. `fno agents king done` ends one crown. Arming the fleet breaker affects others. No standing law grants this authority to an agent. The king names evidence and escalates. Arm only on user order unless a later law grants the authority.

## Abdicate

`fno agents king done` on user order. With `--once`, `fno agents king done` is the last act of pass step 5 or of the court's wave boundary.

The one-wave pass, the crown model, and the minion contract are in [references/](references/): [once.md](references/once.md), [minion-clause.md](references/minion-clause.md), [court-operations.md](references/court-operations.md), [cli-commands.md](references/cli-commands.md), [review.md](references/review.md), [retro-interview.md](references/retro-interview.md), [workflow-routes.md](references/workflow-routes.md), [postcompact-brief.md](references/postcompact-brief.md).

## Known Limitations and Deferred Work

- A Codex reign has no native cron or Monitor beat; its provider goal and Footnote Stop receipts are read independently, and the external wake arm supplies cadence. A `--once` pass does not supervise the workers it spawns. The court crown-source field is not landed yet. See [LIMITATIONS.md](LIMITATIONS.md).
