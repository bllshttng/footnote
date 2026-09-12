# Reign: the tenured king

A pass encodes a wave and abdicates (`/fno:king-for-a-day`). A reign stays. The operator asked for a king that keeps working over a territory for days. It checks in on a schedule, drives with levers, and parks rather than dies. `/fno:reign <scope>` is that skill.

## What keeps a king active

Three facts fix the design, all measured against the harness internals:

- fno already has a better goal than the native `/goal` for kings. The in-session king arm reads BOARD truth. It blocks exit while actionable rows exist, exits `NoWork` on a clean board, and escalates every `NoProgress`. The native `/goal` evaluator reads only the transcript.
- The king can inject native commands itself. `fno agents mail send '<command>' --to-self --raw` types the command verbatim, as from the operator. So the reign arms its own `/loop` and `/goal` at start, without waiting for a ritual.
- `/loop 30m` fires unconditionally and keeps the process from idling out. That is what a tenured king needs. A Monitor fires only on change, which is what the one watcher needs.

## The one arm, and the demand reads

The 2026-09-10 measurement covered one 12-hour reign. The stop hook drove all four real dispatches. The six monitor arms surfaced nothing the king acted on. Court costs are charged per wake, not per hour. So the skill arms ONE monitor, a harness-tracked Monitor running a shell until-loop. No tokens while waiting. When the condition changes, the session wakes.

1. **Fleet settled-PR wake, 600s.** The stop hook fires on an agent stop, and on nothing else. A session can stop while its PR is pending. Its CI can settle an hour later. Nothing watches for that. This arm does. The until-loop exits on one condition: a quiet or parked roster row whose node's `pr_number` reads settled. The reads are `fno agents list --json` and `fno do pr status <n>`. The wake pokes the stopped session with `fno agents resume <id>`, never a fresh dispatch. A by-hand run on 2026-09-10 covered PRs 1650, 1694 and 1649. Three pokes, zero slot cost. One query run centrally beats six timers run per king.

The deleted arms are demand reads. Each is read on demand. Mail arrives as a conversation turn and cannot be missed. The board and crown liveness are check-in body reads. A red row in `fno agents status` stays the mechanical trigger for the one dispatch exception. Main CI is read as one verdict token (`red`, `green`, `pending`), never a check-run count. Several of the most productive reign wakes began with "main flipped green". Capacity is the spawn gate's job. The gate refused twice in the measured reign, correctly. The band's five readings changed no decision.

Every arm emits on probe failure as well as on the watched condition. When its instrument breaks, a silent monitor reports "nothing happened" and "the reader is dead" with the same silence.

Two things stay unmonitored because each has an owner. Worker transcripts belong to court-mode watching. Per-PR CI belongs to the merge arm and the heal driver.

## The shape field and which hook reads it

A reign that spawns workers is not a pure pass. Until the shape field existed, saying so had no machine-visible act. The Stop nudge offered three options and detected two. The cheapest way to silence it, the carveout, downgraded a live teammate to advisory self-review. The fix:

- The king manifest carries `shape` from birth. The default is `pass`. A reign declares `court` the moment it spawns its first worker, via `fno agents king shape <pass|court>`.
- `hooks/context-nudge.sh` resolves the manifest in its orphan branch. When the shape is `court` and the spawned workers are live, it goes silent. It stays loud for an unshaped reign walking away from live workers.
- `hooks/king-postcompact-reinject.sh` appends the reign operating rules after a compaction. The manifest must name the compacting session.

## Stop semantics

Exit is blocked while actionable rows exist. The stop hook reads board truth. A clean board exits `NoWork`, and the loop re-enters on the next beat. `NoProgress` after three unshrinking fires escalates automatically and the session parks. The operator's answer wakes it through the wake arm. A reign never fights the hook. A reign never `/goal clear` on NoProgress.

## The dispatch exception and its journal row

A reign does not dispatch. The single exception is a provably dead dispatching arm: a red row in `fno agents status`. That spawn is journaled `reign_dispatch_exception`, naming the arm and the node, BEFORE it fires. A spawn without that row is a defect.

## The check-in journal: write canonical, read it back

Every `reign_checkin` row carries the canonical keys `scope` and `change`, plus the beat's measured evidence under distinct extra keys. The validator refuses any other shape. The aliases `crown_scope`, `crown`, and `result` are refused even beside the canonical keys. Before this contract, 153 check-ins from five kings named the scope three different ways. No schema-keyed reader can walk a journal like that.

`fno agents king history` reads one crown's rows back, newest first, complete payloads. It never generates a summary. Legacy alias rows count as rejected evidence. It never silently accepts them. It reads every journal ``paths.event_journals`` resolves: live files, rotations, mirrors. A zero-match answer still names every journal it read, with each file's scanned row count. An empty history is a measurement, not an absence. `fno agents court -n` answers a different question: who holds which crown right now.

## The reign ledger page

`fno agents king ledger` renders `<state_dir>/reign.html` (`--out` overrides). Each crowned territory gets a section: holder, grantor, agree verdict, status counts, and every member node with its title. Two more sections name what the folds omit: uncrowned epics, and orphan leaves (parentless, actionable, contained by no node). The data path is the court's own. `gather_court` and the native court-fold read run in Python, and the page assembly runs in the native `reign-ledger` verb (the king-history split, so the Python-tree ratchet holds). The page renders that answer and never re-derives the scope join, so the ledger, `fno agents court -n`, and the fold cannot disagree about who holds a node. An empty court renders "no live crowns" as a measurement. An unreadable registry renders the named reason. An unresolved fold states why in place. Never a blank or falsely healthy page.

## The codex limit

Codex exposes none of `/goal`, `/loop`, or Monitor. A codex reign has no self-injected beat. The wake arm's backstop is its only pulse. The skill names this in its first line.

## Config keys

`config.king` carries the injected texts, so an OSS user edits one place. `king.checkin_interval` defaults to `30m`. `king.checkin_text` and `king.goal_text` carry the prompts. The skill prints the defaults verbatim. A fresh install runs with no config.
