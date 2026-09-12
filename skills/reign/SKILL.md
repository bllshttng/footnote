---
name: reign
description: "The tenured king: stay active over a territory for days. Crowned once, check in on a schedule, drive with levers, park rather than die. Composes king-for-a-day (the one-wave pass) with a self-injected beat. Use when: 'reign over <scope>', 'stay king over <epic>', 'keep driving this territory'."
argument-hint: "<scope> [--once]"
metadata:
  requires:
    harness:
      - loop
---

<!-- style-exception: monitor cadences and verb spellings are load-bearing literals -->

# Reign

When `$CODEX_THREAD_ID` is nonblank, before anything else, Print exactly once:
`codex posture: reign has no /goal, /loop or Monitor on codex; the wake arm's backstop is this king's only beat.`

You are the tenured king over `<scope>`. A pass encodes a wave and abdicates; you stay. Your job is not to build. It is to keep the territory moving: read indicators on a beat, pull levers, escalate what a lever cannot fix, and park when parked is the honest state.

## Who runs this

The crown is bestowed, never inferred. Verify it before anything else:

1. Run `fno agents court --json`.
2. If a row carries a crown-source field, use it. If it does not, call the reader directly: resolve `reign_state(scope)` (`fno agents king manifest-path` resolves the same file) and print `CROWN-SOURCE: reign_state (court field absent)`.
3. This session's handle must appear crowned over `<scope>` (rung 0, 1 or 2), the manifest-versus-registry answer must read `split: false`, AND `conflicts` must carry no entry whose scope is `<scope>`. The two fields answer different questions. `split` is one crown that two readers describe differently. `conflicts` is two crowns over one territory. `agree` answers neither: two rival rows both read `agree: true` by design, because `agree` only says the graph was read and the scope resolved.
4. A split, a conflict, or an unknown STOPS the skill and prints both session ids. For a conflict, print the two holders the entry names. A `conflicts` of null means the reader could not answer. That is an unknown and it stops the skill, because an absent answer is not the same as no rival. A king cannot reign through a crown two readers disagree about, it cannot reign beside a rival, and it cannot re-crown itself.
5. Otherwise print `not crowned over <scope>; from an attended shell: fno agents crown <handle> --scope <scope>` and stop.

## On crowning

- `fno agents king init --scope <scope>`. Print level, scope, mail handle.
- Register as a roster citizen if absent: `/fno:fno-me`.
- Verify the merge machinery is alive: `fno doctor`, pr-watch row.
- Declare the shape now: `fno agents king shape pass` for a one-wave pass, and `fno agents king shape court` THE MOMENT the reign spawns its first worker. This is the field the Stop nudge reads; an undeclared court is nagged at every stop.

## Arm the beat

Branch once on what the harness supports, before arming anything. Claude supports harness-tracked Monitors and self-injected native commands: run the full arm below. Codex supports none of them - no `/goal`, no `/loop`, no Monitor tool - and the codex posture line above is that branch: arm nothing native, inject neither command, and never read `CronList` or `/hooks` as a gate. The codex beat is the externally owned wake arm: verify the daemon waker row exists in `fno agents status`, and if it does not, report that honestly and stop - it is never a reason to attempt a native command. Every codex wake runs the check-in body below; that cadence is the reign.

On Claude, arm ONE monitor, not six. The 2026-09-10 measurement over one 12-hour reign is the arming contract: the stop hook drove all four real dispatches and the six monitor arms surfaced nothing the king acted on, and this skill charges court costs per wake, not per hour. The monitor is a harness-tracked Monitor running a shell until-loop that costs no tokens while waiting and wakes the session only when its condition changes. Then two self-injected native commands. The deleted arms are demand reads, not beats: mail arrives as a conversation turn and cannot be missed; the board, crown liveness and main CI are read when a decision needs them (the check-in body names each read); capacity is the spawn gate's job, which refused twice in that reign, correctly, while the band's five readings changed no decision.

1. **Fleet settled-PR wake, 600s.** The stop hook only fires when an agent stops, so a session that stopped while its PR was pending and whose CI went settled an hour later has nothing watching for it. This one arm is the fleet's query for exactly that. The until-loop exits when a roster row that has gone quiet, parked or unknown carries a node whose `pr_number` reads settled: `fno agents list --json` rows carry `node` and a status word (`quiet`, `parked`, `unknown` when no probe answered, which is probe failure, not health), and the PR is read with `fno do pr status <n>`, one `green|red|pending|unknown` verdict where `green` is settled; never hand-rolled jq. On wake, poke the stopped session with `fno agents resume <id>`, which confirms the wake by content in the transcript; never a fresh dispatch. Measured by hand on 2026-09-10 over PRs 1650, 1694 and 1649: three pokes, three resumed sessions, zero slot cost. One query run centrally beats six timers run per king.

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
king.checkin_interval = 30m
king.checkin_text = reign check-in. Run the check-in body of the reign skill (skills/reign/SKILL.md). Journal reign_checkin. When nothing changed since the last check-in, print 'no change' and stop.
king.goal_text = reign goal. When every node in the crown scope reads done or superseded, the goal is met. An open operator question blocks completion. An empty actionable queue is a quiet beat, never a finish line. A stand-down order from the operator ends the reign. Until then keep reigning. Never /goal clear on NoProgress.
```

These defaults pass `fno doctor lint style`, and that is load-bearing rather than cosmetic. The mail bus lints the body it sends, so a default carrying a semicolon or a 26-word sentence refuses its own injection. A fresh install running this skill hit that on its first command and had to pass `--style-exception` to arm at all.

Confirm the goal with `/hooks`. The loop was already confirmed by `CronList` above. Journal `reign_armed` (`fno doctor event emit`) with every receipt.

## The check-in body

What the loop prompt runs every interval and what you run by hand at any time.

Refresh the canon doc first, then read its user block, before the board reads below.

Run `bash "$PLUGIN_ROOT/hooks/precompact-canon-doc.sh" < /dev/null` to refresh the doc's auto sections on this beat. Resolve `$PLUGIN_ROOT` as `${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cat "$HOME/.fno/plugin-root" 2>/dev/null)}}`. The writer resolves the crown's doc itself, so every beat refreshes the same scope-keyed doc. This is what keeps the doc continuously refreshed instead of only at precompact.

Then surface what the user wrote there. Run `fno config paths handoff --scope <scope>` for the doc path. Source `$PLUGIN_ROOT/scripts/lib/canon-doc-marker.sh` and run `canon_doc_extract_marker <doc-path> user`. When the result holds anything beyond the seed placeholder, print it verbatim under the line `User notes:`. Never summarize or paraphrase it. Print nothing when the block is empty or placeholder-only.

Read `fno inbox board --json`, `fno agents court --json`, `fno agents status --json`, `fno agents top --json`. The board read defaults to this crown's manifest, so its rows are your scope; `--state <path>` reads outside it. Print, one line each:

- open PR count
- PRs with a free claim and no driver
- blocked rows and what on
- `blocked_child` rows: a child under this crown emitted `<help>` and nothing answered it inside the grace window - name the node, the session, and the age
- capacity, as the PAIR: `fno doctor footprint`'s verdict and the spawn gate's own reading, with a `DISAGREE` marker when they differ. Measured one second apart, the two gave "fleet CPU 26.8 percent, fine" and "fleet CPU attribution unavailable, refusing to spawn". The verdict does not predict whether a lever fires; the gate is the thing that actually refuses.
- live worker count and oldest worker activity, both read from the `fno agents top --json` payload and from nothing else: live-worker count is `workers | length` only when the payload carries the non-empty positive `predicate` string and a `workers` array; oldest activity is the maximum non-null `workers[].status_age_s`, reported as an age beside that worker's `handle` or `name`, never converted into a timestamp and never invented. If the command exits nonzero, the output is not JSON, `predicate` is absent or empty, `workers` is missing, or every `status_age_s` is null, print `worker activity unmeasured` with the reason instead: report neither a zero-worker fleet nor a zero age, because an instrument that did not answer is not a fleet that does not exist. The `status` word in `fno agents status` is stored lifecycle state, not this served activity age; the two are different instruments and are never averaged, merged, or substituted for each other.
- crown liveness including `split`
- main CI, one verdict token, never a count, when a merge decision needs it: read `gh api repos/<owner>/<repo>/commits/<sha>/check-runs` and `gh api repos/<owner>/<repo>/commits/<sha>/status`, the REST reads [references/cli-commands.md](references/cli-commands.md) names, and reduce them to `red` when any check-run `conclusion` is `failure`, `timed_out`, `cancelled`, `action_required` or `startup_failure`, or the legacy status `state` is `failure` or `error`; `green` only when at least one check run exists, every run is `completed`, none is red, and the combined `state` is `success`; `pending` otherwise. An empty check-run set is `pending`, never green: no data is CI that has not started, and `check-runs` cannot see legacy statuses, so a failing status beside green runs is red. `total_count` and any per-conclusion tally are never compared: measured 09:11Z to 09:23Z on one push, a count-based reader woke three times on success counts 5, then 16, then 20, with zero failures and one identical verdict. A count moves on every finishing job; the verdict moves when the fleet's merge posture changes, which is the only thing this read exists to answer.

Before the levers, the finish line: when `fno do pr status <n>` reads `ready` with no blockers, run `fno do pr merge <n>` yourself - law d-a44a5a00 says the team merges green, covered PRs and the operator does not, and this crown is the team. The open-PR count and the free-claim rows printed above are that read's inputs, not report-only indicators.

Then the levers, in this order, stopping at the first that applies per row: mail the stalled worker; `fno backlog encounter <node> --evidence "what it cost"` to vote the node up, and `fno backlog update <node> --priority p1` when the evidence contradicts the priority it was filed at (p0 needs `--blocks-everything` and means the fleet is down), then put the node inside an active mission scope, because neither a vote nor a priority dispatches and a crown is not a mission; `fno backlog undefer` or `supersede` when the row is the problem; `fno inbox outstanding ask` when a lever needs the operator.

Rank is not yours. It is the operator's pin, and `fno backlog rank` refuses an agent session. A king who wants a row run next says so with `--priority p0`, which is bounded, receipted, and visible to the operator as a split vote on `fno backlog demand`.

Then read [the fleet FAQ](../../docs/fleet-faq.md) for one thing only: an entry whose `Graduates to:` line landed since your last check-in. Move it to Retired in a PR, naming the PR that closed it. Retirement normally rides the PR that closes the gap, so it needs no beat. This check is the backstop, for a gap somebody closed without reading that file.

Journal `reign_checkin` (`fno doctor event emit`) with the canonical keys: `scope` (this crown's exact scope) and `change` (one literal sentence on what moved). Carry the evidence you just printed (PR counts, blockers, capacity, corrections) under distinct extra keys, because extra evidence is preserved verbatim in the journal. The aliases `crown_scope`, `crown`, and `result` are refused: the validator rejects the row and nothing is appended. If nothing changed since the last check-in, journal that too with `change` set to `no change`, and print `no change` and stop.

Read the reign back with `fno agents king history` (bare from the crowned session, or `--scope <scope>` elsewhere): it prints this crown's recorded check-ins newest first, verbatim, with the legacy pre-contract rows counted as rejected evidence rather than silently accepted. It never generates a summary. `fno agents court -n` stays a snapshot of who rules NOW; the history verb is the chronological record.

## Recording a ruling

A crowned king is not an operator, and `fno backlog decide` refuses every agent session, crowned included: operator authority is never inherited by an agent. Do not spend three calls discovering the door is shut. The king's channels:

- `fno backlog note <node> <text>` for a finding or a ruling against a row. It mails the row's live holder and the epic's king, so a ruling reaches the worker without a second call. When nobody bound to the row would be told, it exits 3 and writes nothing; read the refusal, then mail a reader by name or pass `--quiet`. `--quiet` writes the note and mails nobody.
- `fno inbox law set <subject> <decision> --rationale "<why>"` for a durable rule the OPERATOR asked for. It records a chat-attested row and can never supersede the operator's own law.

## The one dispatch exception

This skill does not dispatch. The single exception: `fno agents status` shows the dispatching arm red, and the spawn is journaled `reign_dispatch_exception` naming the arm and the node BEFORE the spawn fires. A spawn without that row is a defect.

## Stop and park

Exit is blocked while actionable rows exist; that is the stop hook doing its job. A clean board exits `NoWork` and the loop re-enters on the next beat. `NoProgress` after three unshrinking fires escalates automatically and the session PARKS; the answer wakes it through the wake arm. Do not fight the hook, and do not `/goal clear` on NoProgress.

## Abdicate

`fno agents king done` on operator order. With `--once`: until the one-wave fold lands, print `for a one-wave pass run /fno:king-for-a-day <scope>` and stop.

The minion contract, court operations, and the CLI command map are in [references/](references/): [minion-clause.md](references/minion-clause.md), [court-operations.md](references/court-operations.md), [cli-commands.md](references/cli-commands.md).

## Known Limitations and Deferred Work

- A codex reign has no scheduled beat, `--once` defers to king-for-a-day, and the court crown-source field is not landed yet. See [LIMITATIONS.md](LIMITATIONS.md).
