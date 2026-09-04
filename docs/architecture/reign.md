# Reign: the tenured king

A pass encodes a wave and abdicates (`/fno:king-for-a-day`). A reign stays. The operator asked for a king that keeps working over a territory for days. It checks in on a schedule, drives with levers, and parks rather than dies. `/fno:reign <scope>` is that skill.

## What keeps a king active

Three facts fix the design, all measured against the harness internals:

- fno already has a better goal than the native `/goal` for kings. The in-session king arm reads BOARD truth. It blocks exit while actionable rows exist, exits `NoWork` on a clean board, and escalates every `NoProgress`. The native `/goal` evaluator reads only the transcript.
- The king can inject native commands itself. `fno agents mail send '<command>' --to-self --raw` types the command verbatim, as from the operator. So the reign arms its own `/loop` and `/goal` at start, without waiting for a ritual.
- `/loop 30m` fires unconditionally and keeps the process from idling out. That is what a tenured king needs. A Monitor fires only on change, which is what the six watchers need.

## The six arms and why each exists

Each arm is a harness-tracked Monitor running a shell until-loop. No tokens while waiting. When a condition changes, the session wakes.

1. **Unread mail, 60s.** Worker reports, peer facts, and operator answers all land here.
2. **Board change, 120s.** The board read is expensive at load. The arm watches the cheap proxy: the project events journal (`pr_opened`, `loop_terminated`, `claim_released`) and the `fno backlog ready` count.
3. **Crown liveness, 300s.** A crown can be lost silently. A king cannot re-crown itself, but it can escalate the minute it is crownless. It reads `reign_state(scope)`. Unreadable instruments report `CROWN-UNKNOWN`. `split` and court `conflicts` report separately from the agree counts.
4. **Main branch CI, 300s.** A red main blocks every merge in the fleet. Several of the most productive reign wakes began with "main flipped green".
5. **Capacity band, 300s, debounced across two samples.** The load-derived verdict flaps. Measured over, within, over, within inside fifteen minutes, with no change in real work. The band must HOLD before it is believed. The arm prints `sustained_cpu_cores` beside the verdict and flags disagreement.
6. **Arm staleness, 600s.** Any red row in `fno agents status` is the mechanical trigger for the one dispatch exception.

Every arm emits on probe failure as well as on the watched condition. When its instrument breaks, a silent monitor reports "nothing happened" and "the reader is dead" with the same silence.

Three things stay unmonitored because each has an owner. Worker transcripts belong to court-mode watching. Per-PR CI belongs to the merge arm and the heal driver. The raw load average is wrong for the reason arm 5 names.

## The shape field and which hook reads it

A reign that spawns workers is not a pure pass. Until the shape field existed, saying so had no machine-visible act. The Stop nudge offered three options and detected two. The cheapest way to silence it, the carveout, downgraded a live teammate to advisory self-review. The fix:

- The king manifest carries `shape` from birth. The default is `pass`. A reign declares `court` the moment it spawns its first worker, via `fno agents king shape <pass|court>`.
- `hooks/context-nudge.sh` resolves the manifest in its orphan branch. When the shape is `court` and the spawned workers are live, it goes silent. It stays loud for an unshaped reign walking away from live workers.
- `hooks/king-postcompact-reinject.sh` appends the reign operating rules after a compaction. The manifest must name the compacting session.

## Stop semantics

Exit is blocked while actionable rows exist. The stop hook reads board truth. A clean board exits `NoWork`, and the loop re-enters on the next beat. `NoProgress` after three unshrinking fires escalates automatically and the session parks. The operator's answer wakes it through the wake arm. A reign never fights the hook. A reign never `/goal clear` on NoProgress.

## The dispatch exception and its journal row

A reign does not dispatch. The single exception is a provably dead dispatching arm: a red row in `fno agents status`. That spawn is journaled `reign_dispatch_exception`, naming the arm and the node, BEFORE it fires. A spawn without that row is a defect.

## The codex limit

Codex exposes none of `/goal`, `/loop`, or Monitor. A codex reign has no self-injected beat. The wake arm's backstop is its only pulse. The skill names this in its first line.

## Config keys

`config.king` carries the injected texts, so an OSS user edits one place. `king.checkin_interval` defaults to `30m`. `king.checkin_text` and `king.goal_text` carry the prompts. The skill prints the defaults verbatim. A fresh install runs with no config.
