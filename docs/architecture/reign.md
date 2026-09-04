# Reign: the tenured king

A pass encodes a wave and abdicates (`/fno:king-for-a-day`). A reign stays. The operator's ask was a king that keeps working over a territory for days: checks in on a schedule, drives with levers rather than spawns, and parks rather than dies. `/fno:reign <scope>` is that skill.

## What keeps a king active

Three facts fix the design, all measured against the harness internals:

- fno already has a better goal than the native `/goal` for kings. The in-session king arm (`king_decide` in the loop-check verb) reads BOARD truth: it blocks exit while actionable rows exist, exits `NoWork` on a clean board, and escalates every `NoProgress`. The native `/goal` evaluator reads only the transcript.
- The king can inject native commands itself. `fno agents mail send '<command>' --to-self --raw` types the command as the operator would, so the reign arms its own `/loop` and `/goal` at start instead of waiting for a ritual.
- `/loop 30m` fires unconditionally and keeps the process from idling out, which is exactly what a tenured king needs; a Monitor fires only on change, which is what the six watchers need.

## The six arms and why each exists

Each arm is a harness-tracked Monitor running a shell until-loop: no tokens while waiting, wakes the session only when its condition changes.

1. **Unread mail, 60s.** The king's inbox is where worker reports, peer facts, and operator answers land. Everything routes through it.
2. **Board change, 120s.** The board read is expensive at load, so the arm watches the cheap proxy: the project events journal (`pr_opened`, `loop_terminated`, `claim_released`) plus the `fno backlog ready` count.
3. **Crown liveness, 300s.** A crown can be lost silently; a king cannot re-crown itself but it can escalate the minute it is crownless. Reads `reign_state(scope)`, reports unreadable instruments as `CROWN-UNKNOWN`, and reports `split` and court `conflicts` separately from the agree counts.
4. **Main branch CI, 300s.** A red main blocks every merge in the fleet. Several of the most productive reign wakes began with "main flipped green".
5. **Capacity band, 300s, debounced across two consecutive samples.** The load-derived verdict flaps: measured over, within, over, within inside fifteen minutes with no change in real work. The band must HOLD before it is believed. The arm prints `sustained_cpu_cores` beside the verdict and flags disagreement.
6. **Arm staleness, 600s.** Any red row in `fno agents status` is the mechanical trigger for the one dispatch exception.

Every arm emits on probe failure as well as on the watched condition: a monitor that is silent when its instrument breaks reports "nothing happened" and "the reader is dead" with the same silence.

Not monitored, each because it has an owner: individual worker transcripts (court-mode watching), per-PR CI (the merge arm and the heal driver), and the raw load average (arm 5 names why).

## The shape field and which hook reads it

A reign that spawns workers is not a pure pass, and until the shape field existed, saying so had no machine-visible act: the Stop nudge offered three options and could detect two, and the cheapest way to silence it (the carveout) downgraded a live teammate to advisory self-review. The fix:

- The king manifest carries `shape` from birth: `pass` by default, `court` declared the moment the reign spawns its first worker, via `fno agents king shape <pass|court>`.
- `hooks/context-nudge.sh` resolves the manifest in its orphan branch and goes silent when the shape is `court` and the spawned workers are live. It stays loud for the unshaped reign walking away from live workers.
- `hooks/king-postcompact-reinject.sh` appends the reign operating rules after a compaction when the manifest names the compacting session.

## Stop semantics

Exit is blocked while actionable rows exist (the stop hook, reading board truth). A clean board exits `NoWork` and the loop re-enters on the next beat. `NoProgress` after three unshrinking fires escalates automatically and the session parks; the operator's answer wakes it through the wake arm. A reign never fights the hook and never `/goal clear` on NoProgress.

## The dispatch exception and its journal row

A reign does not dispatch. The single exception is a provably dead dispatching arm: a red row in `fno agents status`. That spawn is journaled `reign_dispatch_exception` naming the arm and the node BEFORE it fires. A spawn without that row is a defect.

## The codex limit

Codex exposes none of `/goal`, `/loop`, or Monitor, so a codex reign has no self-injected beat; the wake arm's backstop is its only pulse. The skill names this in its first line.

## Config keys

`config.king` carries the injected texts so an OSS user edits one place: `king.checkin_interval` (default `30m`), `king.checkin_text`, `king.goal_text`. The skill prints the defaults verbatim, so a fresh install runs with no config.
