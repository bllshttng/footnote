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

When `$CODEX_THREAD_ID` is nonblank, before anything else, Print exactly once:
`codex posture: reign has no /goal, /loop or Monitor on codex; the wake arm's backstop is this king's only beat.`

You are the tenured king over `<scope>`. With `--once` you rule one wave and abdicate. Without it you stay. Your job is not to build. It is to keep the territory moving. Read indicators on a beat, pull levers, escalate what a lever cannot fix, and park when parked is the honest state.

An operator turn that tells the reign to stand down blocks the stop gate until it is acked. Answer it as a verdict on this reign, not as a general question.

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

With `--once` the crown rules one wave and expires. Run Who runs this and On crowning, declare `fno agents king shape pass`, and skip the term. Skip Arm the beat: no monitor, no `/loop`, no `/goal`. Then run [the one-wave pass](references/once.md#run-it-in-this-order) in order and abdicate with `fno agents king done`. A kickoff that dispatches through `fno backlog advance` stays a pass. The wave is a court only when its workers are court teammates that mail you back: declare `fno agents king shape court` and run [court mode](references/once.md#court-mode-reign-over-the-wave) until the wave completes. The levers, Recording a ruling and the three halts apply to both shapes.

## Arm the beat

Branch once on what the harness supports, before arming anything. Claude supports harness-tracked Monitors and self-injected native commands: run the full arm below. Codex supports none of them - no `/goal`, no `/loop`, no Monitor tool - and the codex posture line above is that branch: arm nothing native, inject neither command, and never read `CronList` or `/hooks` as a gate. The codex beat is the externally owned wake arm: verify the daemon waker row exists in `fno agents status`, and if it does not, report that honestly and stop - it is never a reason to attempt a native command. Every codex wake runs the check-in body below; that cadence is the reign.

On Claude, arm ONE monitor, not six. The 2026-09-10 measurement over one 12-hour reign is the arming contract: the stop hook drove all four real dispatches and the six monitor arms surfaced nothing the king acted on, and this skill charges court costs per wake, not per hour. The monitor is a harness-tracked Monitor running a shell until-loop that costs no tokens while waiting and wakes the session only when its condition changes. Then two self-injected native commands. The deleted arms are demand reads, not beats: mail arrives as a conversation turn and cannot be missed; the board, crown liveness and main CI are read when a decision needs them (the check-in body names each read); capacity is the spawn gate's job, which refused twice in that reign, correctly, while the band's five readings changed no decision.

1. **Nudge-escalation wake, 600s.** The daemon nudge ladder owns every poke of a quiet session on an open PR. It mails a live session. It resumes one whose process is gone. After three nudges with no activity it files one operator question and emits `pr_nudge_escalated` (docs/reaping-faq.md). This arm never pokes a worker, so no crown adds a second writer. When `fno doctor event find pr_nudge_escalated --since 15m --json` returns a match whose `node` is one of this crown's nodes, the until-loop exits. Take the one `fno agents court --nodes` row whose `manifest_session` equals the `harness_session_id` that `fno whoami --json` prints. Keep its `scope_nodes.nodes` rows marked `owned: true`, so one crown wakes per node. No court row matching your session is a probe failure, never a quiet board. The same holds for a find payload whose `unreadable_files` is above zero. On wake, read the PR with `fno do pr status <n>`: a `ready` PR with no blockers takes the merge lever below. Anything else is the ladder's operator question, which the check-in's `held:` line carries, so name it in the next report. Never `fno agents resume` it: the ladder already tried three times. Measured 2026-09-19 to 2026-09-22: 341 ladder nudges and 10 escalations.

A red settle does not belong to this arm. The daemon nudge ladder wakes a quiet worker once per red head and names the failing checks (docs/reaping-faq.md).

Every arm emits on **probe failure** as well as on the watched condition. A monitor that is silent when its instrument breaks reports "nothing happened" and "the reader is dead" with the same silence. Gate on a positive marker in the output, never on the exit code alone: `fno backlog show` does not exist and the failure exits 0, so an exit-code caller reads a missing verb as a healthy empty node.

Not monitored, because each has an owner: individual worker transcripts (court-mode watching, the machinery's job), per-PR CI (the merge arm and the heal driver), and the raw load average (the spawn gate refuses on its own reading, which is the owner a band monitor would only duplicate).

Then, still on the Claude branch only, inject the two native commands, typing them as the operator would. **Send them in two separate turns, never in one breath.** `/goal` is a one-way door: the moment it lands, the stop hook holds the session open and it never idles again, so anything still queued behind it is never delivered. Sending both together leaves the loop waiting forever and the operator has to interrupt the session by hand to get it in. Writing `/loop` first in the same turn does NOT avoid this, because both land in the same input queue and the goal closes the door on whatever has not been read yet.

Inject the loop, end the turn so the harness actually delivers it, then confirm a cron exists before going on:

```
fno agents mail send "/loop ${king.checkin_interval} ${king.checkin_text}" --to-self --raw
```

`CronList` must now name the job. An empty list means the loop never landed, so re-send it and stop: a reign with a goal and no loop has no beat, and only the operator can restart one. Once the cron is there, inject the goal:

```
fno agents mail send "/goal ${king.goal_text}" --to-self --raw
```

Read both texts with `fno config get`. The defaults, verbatim, so a fresh install runs with no config:

```
king.checkin_interval = 4h
king.checkin_text = reign check-in. Run fno agents king checkin: it gathers the check-in readings, prints them, diffs the last beat, and journals reign_checkin. Then act on the printout per the reign skill. When nothing changed and coverage is full, print 'no change' and stop. This beat is a heartbeat. 
king.goal_text = reign goal. When every node in the crown scope reads done or superseded, the goal is met. An open operator question blocks completion. An empty actionable queue is a quiet beat, never a finish line. A stand-down order from the operator ends the reign. Until then keep reigning. Never /goal clear on NoProgress.
```

These defaults pass `fno doctor lint style`, and that is load-bearing rather than cosmetic. The mail bus lints the body it sends, so a default carrying a semicolon or a 26-word sentence refuses its own injection. A fresh install running this skill hit that on its first command and had to pass `--style-exception` to arm at all.

The monitor and stop hook are the beat. The cron is a 4-hour heartbeat whose job is to prove the reign and its monitor are alive.

Confirm the goal with `/hooks`. The loop was already confirmed by `CronList` above. Journal `reign_armed` (`fno doctor event emit`) with every receipt.

## The check-in body

What the loop prompt runs every interval and what you run by hand at any time.

One verb runs the body: `fno agents king checkin`. It gathers every reading below, prints them in a fixed order, diffs the previous canonical beat, and journals the `reign_checkin` row itself from the same numbers it printed. It never decides: no lever fires from it, and the levers stay yours. Refresh the canon doc first, so the verb reads this beat's doc.

Run `bash "$PLUGIN_ROOT/hooks/precompact-canon-doc.sh" < /dev/null` to refresh the doc's auto sections on this beat. Resolve `$PLUGIN_ROOT` as `${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cat "$HOME/.fno/plugin-root" 2>/dev/null)}}`. The writer resolves the crown's doc itself, so every beat refreshes the same scope-keyed doc. This is what keeps the doc continuously refreshed instead of only at precompact. Past the compaction ceiling (default 3), a doc older than 24 hours blocks the stop gate. The beat refresh is what keeps the reign exitable.

Then run `fno agents king checkin` (bare from the crowned session, or `--scope <scope>` elsewhere). The board read inside it defaults to this crown's manifest, so its rows are your scope. It prints, one line each, and this is the verb's documented output contract:

- `User notes:` the canon doc's user block (read through `fno config paths handoff --scope <scope>`), verbatim. Never summarized or paraphrased. Nothing when the block is empty or placeholder-only.
- `board:` the open PR count, the PRs with a free claim and no driver, and the blocked rows with what they are blocked on.
- `blocked_child:` a child under this crown emitted `<help>` and nothing answered it inside the grace window - the node, the session, and the age.
- `held:` nodes an open operator question blocks, oldest question first. Each row carries the question id, its age, and the `fno backlog decide <node> "<ruling>" --question-id <id>` command that clears it. When no question holds a node, the line reads `held: none`.
- `scope <scope>:` the scope node counts, then the active rows with worker, PR and session (the `fno agents court -n` join).
- `epics:` one line under the scope line. It lists each epic in the scope that holds an open direct child, fullest first, as `open/cap` against `backlog.epic_max_open_children`. `19/15 full` means the next child is refused, and `19/- (cap unset)` means no cap is set. The lead plans the split from this line, before a write bounces.
- `territory:` one row per scope : rung, mission, live against `agents.max_live_per_territory` (the same projection the spawn gate's team cap enforces), the blueprinter handle with its liveness, and the kingless mark. A scope reading blind (the reading failed) is a machine-reported blind spot: name it in every escalation about that scope. The blueprinter is machinery-owned; you feed it nothing and reset nothing.
- `capacity:` the PAIR - `fno doctor footprint`'s verdict and the spawn gate's own reading, with a `DISAGREE` marker when they differ and `unparsed_lines` named when that is the cause. Measured one second apart, the two gave "fleet CPU 26.8 percent, fine" and "fleet CPU attribution unavailable, refusing to spawn". The verdict does not predict whether a lever fires; the gate is the thing that actually refuses.
- `workers:` live worker count and oldest worker activity, both read from the `fno agents top --json` payload and from nothing else: live-worker count is `workers | length` only when the payload carries the non-empty positive `predicate` string and a `workers` array; oldest activity is the maximum non-null `workers[].status_age_s`, reported as an age beside that worker's `handle` or `name`, never converted into a timestamp and never invented. When the payload cannot answer, the line reads `worker activity unmeasured` with the reason: neither a zero-worker fleet nor a zero age is ever reported from an unread payload, because an instrument that did not answer is not a fleet that does not exist. The `status` word in `fno agents status` is stored lifecycle state, not this served activity age; the two are different instruments and are never averaged, merged, or substituted for each other.
- `crown:` liveness including `split`.
- `refusal_rate:` the machine declining, as a percent, over the trailing 200 tool calls in this session's own transcript - the cheapest available proxy for context degradation, no model introspection needed. A rise across two consecutive check-ins (not one noisy tick) prints `RISING (handoff signal)`: treat it as a reason to hand off, the same way a `blocked_child` or `attention:` line is. Reads `unmeasured` on a harness with no per-session transcript file (opencode) or when the transcript cannot be found.
- `drain:` undelivered mail as one number.
- `main ci:` one verdict token, never a count, for the merge decision. `fno-agents` reduces the shared check reader. That reader holds every check-run page, the legacy commit statuses, and runs that failed before minting a job. A workflow file GitHub cannot parse completes as `failure` with zero jobs. It mints no check run, so the row names its workflow path. `red` on any `fail` or `cancel` row. `green` on a row set where all rows pass or skip. `pending` on every other row set, including no rows. A failed read is loud, never green: an unreadable status or runs listing names the fault instead of answering. The combined status left the reduce. GitHub answers `pending` for a commit with zero legacy statuses. No green on this repo ever survived it. `total_count` and any per-conclusion tally are never compared. Measured 09:11Z to 09:23Z on one push: a count-based reader woke three times. The success counts ran 5, then 16, then 20, with zero failures and one identical verdict. A count moves on every finishing job. When the fleet's merge posture changes, the verdict moves. That is the only thing this read exists to answer.
- `escalations:` open and overdue escalation notes in this scope's escalations directory, filtered to the crown fold. Take the recommended option, or wait; irreversible always waits.
- `control plane:` every arm failing past `[notify] arm_failing_after_s`, every hung verb (a process running far past three times its own `--timeout`), and every `flight:` holder whose pid is gone. A change that starts `attention:` is never a quiet beat. Trace each entry with `fno agents status` and tell the operator in the next report.
- `parked:` open PR parks, each with its reason, age and node, and the `fno-agents pr-park unpark <key>` remedy. When nothing is parked, the line reads `parked: none`.

A failed reader prints `READER FAILED <name>: <reason>` on its own line, and the beat continues without it. One refused instrument can never blank a line or masquerade as a healthy value on another axis. The `coverage: N of M readings ok` line counts M as the readings this beat ran and N as the ones that answered. A `failed readers:` line names each one that failed, so a beat with a failed reader can never read as a clean beat. A `vs last beat` line diffs the numeric keys against the previous canonical row. A `change:` line states what moved. When this scope's FAQ store is empty or any reader failed, the ready-to-run `fno agents king faq add` command prints.

Before the levers, the finish line: when `fno do pr status <n>` reads `ready: true`, run `fno do pr merge <n>` yourself - standing law: the team merges green, covered PRs and the operator does not, and this crown is the team. `ready` IS the merge decision (x-53c5): it is the authorized-merge preview verdict, the same gate chain the merge verb runs, so CI, review coverage, base staleness, the merge slot, and merge authority all fold into it, and when it reads false the payload's `merge_decision.blockers` names what holds. One guard keeps the lever honest: resolve the row's project cwd and run both verbs from there, because a PR number is repository-local and both verbs derive their repo from the ambient cwd, so a portfolio crown can inspect and merge an unrelated same-numbered PR. The open-PR count and the free-claim rows printed above are that read's inputs, not report-only indicators.

Then the levers, in this order, stopping at the first that applies per row: mail the stalled worker; `fno backlog encounter <node> --evidence "what it cost"` to vote the node up, and `fno backlog update <node> --priority p1` when the evidence contradicts the priority it was filed at (p0 needs `--blocks-everything` and means the fleet is down), then, for a row no crown covers, start a new small epic rather than growing a running one, because neither a vote nor a priority dispatches (see [A finding starts a new epic](#a-finding-starts-a-new-epic)); `fno backlog undefer` or `supersede` when the row is the problem; `fno inbox outstanding ask` when a lever needs the operator.

### A finding starts a new epic

An epic stays small enough to finish. Its finish line is set at the start. So never parent new work into a running epic. A finding goes one of two ways. It starts a new small epic: `fno backlog idea "EPIC: <theme>" --type epic --difficulty <low|medium|high>`, then `fno backlog update <node> --parent <new-epic-id>`. The king that leads the old epic takes the new one with `fno agents crown <handle> --scope <old-epic-id> --scope <new-epic-id>`. A king runs that for an epic its own session created, naming every epic it holds. Any other epic needs an attended shell or a crown that contains both. Or the finding waits unparented for the lead's next epic. A crowned `fno backlog idea` with no `--parent` is linked into your epic, and its `rollup: crown-linked` receipt prints the undo. When the finding is new work, run it: `fno backlog update <node> --parent null`.

Rank is not yours. It is the operator's pin, and `fno backlog rank` refuses an agent session. A king who wants a row run next says so with `--priority p0`, which is bounded, receipted, and visible to the operator as a split vote on `fno backlog demand`.

Then read [the fleet FAQ](../../docs/fleet-faq.md) for one thing only: an entry whose `Graduates to:` line landed since your last check-in. Move it to Retired in a PR, naming the PR that closed it. Retirement normally rides the PR that closes the gap, so it needs no beat. This check is the backstop, for a gap somebody closed without reading that file.

The verb journals `reign_checkin` itself, so the row carries the readings the verb actually took and the printed lines and the stored row cannot disagree. The row carries the canonical keys: `scope` (this crown's exact scope) and `change` (one literal sentence on what moved). Pass `--change "<one sentence>"` when you have a finding to record. The sentence becomes the row's `change`, and the verb's own diff moves to `diff`. Never journal `reign_checkin` with `fno doctor event emit`: that writes a second row for one beat, stamped source `test`. The beat's evidence (PR counts, blockers, capacity, corrections) travels under the verb's own distinct keys. The aliases `crown_scope`, `crown`, and `result` are refused: the validator rejects the row and nothing is appended. `no change` is refused while any reader failed, because an unread axis cannot be known unchanged. A `no change` beat prints `no change` and stops.

Read the reign back with `fno agents king history` (bare from the crowned session, or `--scope <scope>` elsewhere): it prints this crown's recorded check-ins newest first, verbatim, with the legacy pre-contract rows counted as rejected evidence rather than silently accepted. It never generates a summary. `fno agents court -n` stays a snapshot of who rules NOW; the history verb is the chronological record.

Then read the tenure verdict with `fno agents king verdict` and print its first line: it judges the crown's bounds (iterations, respawns, compactions, block cap) and the inherited-scope delivery trend as one set, naming `converging`, `stalled`, `degraded`, or `unknown` (an absent bound is named absent, never satisfied). When the verdict word moved, say so in the next beat's `--change` sentence. On `stalled`, `degraded`, or `unknown`, run `fno agents king escalate <scope> --reason Verdict`: it records one deduplicated operator question naming the bounds and the handoff offer (`fno agents spawn --crown <scope> --succeed`). The king never spawns its own successor; the handoff is the operator's call.

## Recording a ruling

A crowned king answers the open questions in its scope, and escalates only what the superuser must decide. The rules:

- **Answer.** An open question in scope gets `fno inbox outstanding clear <qid> --answer "<answer, with one line of why>" --authority crown`. The answer records as coordination and closes the question.
- **Escalate the four classes only.** `public-surface` (a new public command, flag or API shape), `irreversible` (deleting data, a force push, a merge override, publishing outside the machine), `money-security` (money, accounts or security), and `law-change` (changing or retiring a law the superuser made). Everything else, decide and log. The escalation is one note in the escalations directory (`fno-agents state path escalations`) with the five sections: what is being decided, why it matters now, options with what happens next, the recommendation, and what happens on silence. State a deadline. No bare ids.
- **Silence has a default.** Past the deadline the check-in names the default: take the recommended option and record it with `fno inbox decide`, or wait when the call is irreversible.
- **When the superuser answers in chat, record it.** `fno inbox law set` for a law change, else `fno inbox decide <node> "<answer>" --authority crown --rationale "superuser in chat: <their words>"`, and set the note's `status`. A harness with a push notification tool also sends one that names the note.

A crowned king is not an operator: the `operator` authority is refused on an agent session, and law stays superuser tier. The king's channels:

- `fno backlog note <node> <text>` for a finding or a ruling against a row. It mails the row's live holder and the epic's king, so a ruling reaches the worker without a second call. When nobody bound to the row would be told, it exits 3 and writes nothing; read the refusal, then mail a reader by name or pass `--quiet`. `--quiet` writes the note and mails nobody. A ruling that CONDITIONS A MERGE needs more than a note: a note reaches the worker, but only the hold reaches the merge gate. Set it through the authorized-merge payload field: `printf '{"op":"hold-set","node":"<id>","reason":"<condition>","release_when":"<proof>","set_by":"<crown>"}' | fno-agents authorized-merge`; the worker or the crown lifts it with `printf '{"op":"hold-release","node":"<id>","evidence":"<proof>"}' | fno-agents authorized-merge`.
- `fno inbox law set <subject> <decision> --rationale "<why>"` for a durable rule the OPERATOR asked for. It records a chat-attested row and can never supersede the operator's own law.
- `fno agents king faq add --question "..." --answer "..." --specimen "<node or PR>, <date>" --exit "<the change that retires this>"` for a durable answer a successor king will ask for. It refuses without `--exit`, the change that stops the answer being needed. The three channels divide this way: a FAQ entry answers a question a successor will ask, a note records a finding against one row, and a law records an operator ruling.

Read a ruling back with `fno backlog decisions <subject>` or `fno inbox decisions <subject> --lane law`, newest first. A subject matches exactly, so never mint a near-synonym. Every ruling is machine-local project policy. A rule that a stranger cloning the repository must obey does not reach them from here. Land it in the code, a doc or a gate, in a PR. See [decision-record](../../docs/architecture/decision-record.md).

## The one dispatch exception

The tenured reign does not dispatch. A `--once` pass dispatches only through its kickoff and its court, as [the one-wave pass](references/once.md) says. The single exception: `fno agents status` shows the dispatching arm red, and the spawn is journaled `reign_dispatch_exception` naming the arm and the node BEFORE the spawn fires. A spawn without that row is a defect. Journal it with:

`fno doctor event emit -t reign_dispatch_exception -s loop -d '{"scope":"<scope>","arm":"<arm>","node":"<id>"}'`

The `-s loop` source keeps the row out of the `test` default, so a king's exception does not masquerade as test output.
The exception uses the canonical implementation worker line in `references/court-operations.md#control-surfaces`.

## Stop and park

Exit is blocked while actionable rows exist; that is the stop hook doing its job. A clean board exits `NoWork` and the loop re-enters on the next beat. `NoProgress` after three unshrinking fires escalates automatically and the session PARKS; the answer wakes it through the wake arm. Do not fight the hook, and do not `/goal clear` on NoProgress.

## The three halts

Three halts, three scopes. `fno agents incident stop --reason "<why>"` arms the fleet breaker: from the next tick, new spawns, new dispatch, and `fno doctor test` admissions refuse fleet-wide until `fno agents incident clear --reason "<why>"` reopens admission. A suite started outside that admission path, a hand-run `pytest`, is not gated. It kills nothing that is already running, and mail stays open so the stop can be announced; `fno agents incident status` prints the state and its generation. `fno agents king cancel --scope <scope>` cancels one scope's walk. `fno agents king done` ends one crown. Arming the fleet breaker is outward-facing, and no standing law grants an agent that authority: a king names the evidence and escalates, and arms only on operator order unless a later law grants it.

## Abdicate

`fno agents king done` on operator order. With `--once`, `fno agents king done` is the last act of pass step 5 or of the court's wave boundary.

The one-wave pass, the crown model, and the minion contract are in [references/](references/): [once.md](references/once.md), [minion-clause.md](references/minion-clause.md), [court-operations.md](references/court-operations.md), [cli-commands.md](references/cli-commands.md), [review.md](references/review.md), [retro-interview.md](references/retro-interview.md), [workflow-routes.md](references/workflow-routes.md), [postcompact-brief.md](references/postcompact-brief.md).

## Known Limitations and Deferred Work

- A codex reign has no scheduled beat. A `--once` pass does not supervise the workers it spawns. The court crown-source field is not landed yet. See [LIMITATIONS.md](LIMITATIONS.md).
