---
name: reign
description: "The tenured king: stay active over a territory for days. Crowned once, check in on a schedule, drive with levers, park rather than die. Composes king-for-a-day (the one-wave pass) with a self-injected beat. Use when: 'reign over <scope>', 'stay king over <epic>', 'keep driving this territory'."
argument-hint: "<scope> [--once]"
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
3. This session's handle must appear crowned over `<scope>` (rung 0, 1 or 2) AND the manifest-versus-registry answer must read `split: false`.
4. A split, or an unknown, STOPS the skill and prints both session ids. A king cannot reign through a crown two readers disagree about, and it cannot re-crown itself.
5. Otherwise print `not crowned over <scope>; from an attended shell: fno agents crown <handle> --scope <scope>` and stop.

## On crowning

- `fno agents king init --scope <scope>`. Print level, scope, mail handle.
- Register as a roster citizen if absent: `/fno:fno-me`.
- Verify the merge machinery is alive: `fno doctor`, pr-watch row.
- Declare the shape now: `fno agents king shape pass` for a one-wave pass, and `fno agents king shape court` THE MOMENT the reign spawns its first worker. This is the field the Stop nudge reads; an undeclared court is nagged at every stop.

## Arm the beat

Six monitors, each a harness-tracked Monitor running a shell until-loop that costs no tokens while waiting and wakes the session only when its condition changes. Then two self-injected native commands.

1. **Unread mail, 60s.** Everything routes through it: worker reports, peer facts, the operator's answers. Read with `fno agents mail unread -n <handle>`. The handle is `-n`, never a positional; the positional form fails loudly rather than returning empty.
2. **Board change, 120s.** Do NOT poll `fno inbox board --json` (about two minutes at load). Watch the cheap proxy until the board port lands: tail the project events journal for `pr_opened`, `loop_terminated`, `claim_released`, plus the `fno backlog ready` count. `fno backlog ready` emits JSON on stdout; `-J` exists only for parity, so a line-prefix parser reads it as zero rows forever, which is indistinguishable from a quiet board. Assert a non-zero count as a positive control before trusting a zero. When you do read the board, a bare call is already scoped to this crown (the caller's own manifest is the default); pass `--state <path>` only to read outside it.
3. **Crown liveness, 300s.** `reign_state(scope)` for this handle. Report an unreadable registry or graph as `CROWN-UNKNOWN`, never as a missing crown. Report `split` and court `conflicts` separately from the agree counts: two live rows over one scope each read `agree=true`, so a caller gating on the counts alone sees a healthy court while the fleet has two kings.
4. **Main branch CI, 300s.** A red main blocks every merge in the fleet.
5. **Capacity band, 300s, debounced across two consecutive samples.** The band must HOLD before it is believed: measured over, within, over, within inside fifteen minutes with no change in real work. Edge-triggering on this input is the opposite of debouncing. Print `sustained_cpu_cores` beside the verdict and flag when the two disagree.
6. **Arm staleness, 600s.** Gated on the arms table landing in `fno agents status`. Any red row is the "dispatching arm is down" trigger, which gives the dispatch exception below a mechanical source instead of a hunch.

Every arm emits on **probe failure** as well as on the watched condition. A monitor that is silent when its instrument breaks reports "nothing happened" and "the reader is dead" with the same silence. Gate on a positive marker in the output, never on the exit code alone: `fno backlog show` does not exist and the failure exits 0, so an exit-code caller reads a missing verb as a healthy empty node.

Not monitored, because each has an owner: individual worker transcripts (court-mode watching, the machinery's job), per-PR CI (the merge arm and the heal driver), and the raw load average (item 5 names why).

Then inject the two native commands, typing them as the operator would:

```
fno agents mail send "/loop ${king.checkin_interval} ${king.checkin_text}" --to-self --raw
fno agents mail send "/goal ${king.goal_text}" --to-self --raw
```

Read both texts with `fno config get`. The defaults, verbatim, so a fresh install runs with no config:

```
king.checkin_interval = 30m
king.checkin_text = reign check-in: run the check-in body of the reign skill (skills/reign/SKILL.md); journal reign_checkin; if nothing changed since the last check-in, print 'no change' and stop
king.goal_text = reign goal: fno inbox board --json reports no actionable rows for the crown scope, fno agents court --json shows no split, and the operator has not ordered a stand-down; until then keep reigning and never /goal clear on NoProgress
```

Confirm the goal with `/hooks` and the loop with its receipt. Journal `reign_armed` (`fno doctor event emit`) with every receipt.

## The check-in body

What the loop prompt runs every interval and what you run by hand at any time.

Read `fno inbox board --json`, `fno agents court --json`, `fno agents status --json`. The board read defaults to this crown's manifest, so its rows are your scope; `--state <path>` reads outside it. Print, one line each:

- open PR count
- PRs with a free claim and no driver
- blocked rows and what on
- capacity, as the PAIR: `fno doctor footprint`'s verdict and the spawn gate's own reading, with a `DISAGREE` marker when they differ. Measured one second apart, the two gave "fleet CPU 26.8 percent, fine" and "fleet CPU attribution unavailable, refusing to spawn". The verdict does not predict whether a lever fires; the gate is the thing that actually refuses.
- live worker count
- the oldest worker last-seen stamp
- crown liveness including `split`

Then the levers, in this order, stopping at the first that applies per row: mail the stalled worker; `fno backlog rank <node> --top` so the drain dispatches it next tick; `fno backlog undefer` or `supersede` when the row is the problem; `fno inbox outstanding ask` when a lever needs the operator.

Journal `reign_checkin`. If nothing changed since the last check-in, print `no change` and stop.

## Recording a ruling

A crowned king is not an operator, and `fno backlog decide` refuses every agent session, crowned included: operator authority is never inherited by an agent. Do not spend three calls discovering the door is shut. The king's channels:

- `fno backlog note <node> <text>` for a finding or a ruling against a row.
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
