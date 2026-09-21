# Reign: the tenured king

A pass encodes a wave and abdicates (`/fno:reign <scope> --once`). A reign without `--once` stays. The operator asked for a king that keeps working over a territory for days. It checks in on a schedule, drives with levers, and parks rather than dies. `/fno:reign <scope>` is that skill.

## What keeps a king active

Three facts fix the design, all measured against the harness internals:

- fno already has a better goal than the native `/goal` for kings. The in-session king arm reads BOARD truth. It blocks exit while actionable rows exist, exits `NoWork` on a clean board, and escalates every `NoProgress`. The native `/goal` evaluator reads only the transcript.
- The king can inject native commands itself. `fno agents mail send '<command>' --to-self --raw` types the command verbatim, as from the operator. So the reign arms its own `/loop` and `/goal` at start, without waiting for a ritual.
- `/loop 4h` fires as a heartbeat and keeps the process from idling out. A Monitor fires only on change, which is what the one watcher needs.

## The one arm, and the demand reads

The 2026-09-10 measurement covered one 12-hour reign. The stop hook drove all four real dispatches. The six monitor arms surfaced nothing the king acted on. Court costs are charged per wake, not per hour. So the skill arms ONE monitor, a harness-tracked Monitor running a shell until-loop. No tokens while waiting. When the condition changes, the session wakes.

1. **Fleet settled-PR wake, 600s.** The stop hook fires on an agent stop, and on nothing else. A session can stop while its PR is pending. Its CI can settle an hour later. Nothing watches for that. This arm does. The until-loop exits on one condition: a quiet or parked roster row whose node's `pr_number` reads settled. The reads are `fno agents list --json` and `fno do pr status <n>`. Green alone is not settled work. The `fno do pr status` payload carries `pr_state`. A row whose `pr_state` reads `MERGED` or `CLOSED` is skipped, because a merged PR still reads `verdict: green` and `settled: true`. A row that passes that skip reads its node with `fno backlog get <node>`. If the node `status` reads `done` or `superseded`, the row is skipped. Without them, every crown merge whose worker has parked wakes the crown to poke a finished worker. The wake pokes the stopped session with `fno agents resume <id>`, never a fresh dispatch. A by-hand run on 2026-09-10 covered PRs 1650, 1694 and 1649. Three pokes, zero slot cost. One query run centrally beats six timers run per king.

The deleted arms are demand reads. Each is read on demand. Mail arrives as a conversation turn and cannot be missed. The board and crown liveness are check-in body reads. A red row in `fno agents status` stays the mechanical trigger for the one dispatch exception. Main CI is read as one verdict token (`red`, `green`, `pending`), never a check-run count. Several of the most productive reign wakes began with "main flipped green". Capacity is the spawn gate's job. The gate refused twice in the measured reign, correctly. The band's five readings changed no decision.

Every arm emits on probe failure as well as on the watched condition. When its instrument breaks, a silent monitor reports "nothing happened" and "the reader is dead" with the same silence.

Two things stay unmonitored because each has an owner. Worker transcripts belong to court-mode watching. Per-PR CI belongs to the merge arm and the heal driver.

## The shape field and which hook reads it

A reign that spawns workers is not a pure pass. Until the shape field existed, saying so had no machine-visible act. The Stop nudge offered three options and detected two. The cheapest way to silence it, the carveout, downgraded a live teammate to advisory self-review. The fix:

- The king manifest carries `shape` from birth. The default is `pass`. A reign declares `court` the moment it spawns its first worker, via `fno agents king shape <pass|court>`.
- `hooks/context-nudge.sh` resolves the manifest in its orphan branch. When the shape is `court` and the spawned workers are live, it goes silent. It stays loud for an unshaped reign walking away from live workers.
- `hooks/king-postcompact-reinject.sh` appends the reign operating rules after a compaction. The manifest must name the compacting session.

## Term

A reign that never ends is not a bound. It is a reign nobody bothered to bound. One crown ran 11 days, 37 context windows and 36 compactions with every bound looking correctly configured. The open-question branch returned `block` unconditionally. It never called the ceiling function the other branches share. The fix: the manifest carries `term`, a spec of `span:<N>[smhd]` or `compactions:<N>`. Undeclared reads a Rust constant default, `span:96h` (the eval's measured ideal handoff, about 100 hours in). `fno agents king term <spec> [--reason TEXT]` declares or extends it. A declared or reached term refuses a bare re-declaration without `--reason` - the extension is the receipt.

The Stop-hook gate (`loopcheck::king_decide`) reads the term before the board read. It sits ahead of every early return, including the open-question branch that used to wire completion shut. Reached or unreadable, it blocks through the same bounded spine every other block uses (`blind_block`). A king that ignores the message still ends on `Budget` and escalates. The message names the handoff: `fno agents spawn --crown <scope> --succeed`, or an extension with a reason. A `compactions:` term only measures a claude transcript. Declaring one on another harness is refused at declaration time, not discovered later as an always-`Unreadable` gate.

A same-session re-crown (`fno agents king init --scope --force`) starts a fresh term: the re-crown is itself a grantor's receipt. `fno agents king verdict` prints the term as evidence (`term: span:96h (default) 101h of 96h, reached`). A reached term does not change the verdict word itself. Only the Stop-hook gate forces the handoff.

## Compaction

The crown survives a compact. Its evidence does not. After a compaction the session context holds a summary, and nothing forces that summary back against the store before the king acts on it. Two machines close the gap. The post-compact hook (`king-postcompact-reinject.sh`) reads the newest `isCompactSummary` entry from the transcript. It orders by line position, never by timestamp. It extracts the node ids the published grammar matches and resolves every candidate through one `fno backlog get` batch. The answer rides back as `unresolved:` and `resolved:` rows with one instruction: act on these rows, not on the summary. The stop gate adds a check beside the term gate. Past `king.compaction_ceiling` (default 3) compactions since the manifest's `created_at`, a crown handoff doc that is missing or more than 24 hours old blocks exit. The block names the refresh command, `precompact-canon-doc.sh`. The doc gate counts boundaries only on claude transcripts. It fails open on anything it cannot measure, because it blocks exit and a false block traps a session. One reader-authority rule covers both machines: a node's state comes from `fno backlog get`, never from a `graph.json` read. With the sqlite backend the file is the relational store's stale twin.

## Stop semantics

Exit is blocked while actionable rows exist. The stop hook reads board truth. A clean board exits `NoWork`, and the loop re-enters on the next beat. `NoProgress` after three unshrinking fires escalates automatically and the session parks. The operator's answer wakes it through the wake arm. A reign never fights the hook. A reign never `/goal clear` on NoProgress.

## The dispatch exception and its journal row

A reign does not dispatch. The single exception is a provably dead dispatching arm: a red row in `fno agents status`. That spawn is journaled `reign_dispatch_exception`, naming the arm and the node, BEFORE it fires. A spawn without that row is a defect. The journal row is emitted with:

`fno doctor event emit -t reign_dispatch_exception -s loop -d '{"scope":"<scope>","arm":"<arm>","node":"<id>"}'`

## The check-in journal: write canonical, read it back

Every `reign_checkin` row carries the canonical keys `scope` and `change`, plus the beat's measured evidence under distinct extra keys. The validator refuses any other shape. The aliases `crown_scope`, `crown`, and `result` are refused even beside the canonical keys. Before this contract, 153 check-ins from five kings named the scope three different ways. No schema-keyed reader can walk a journal like that.

`fno agents king checkin` runs the check-in body as one verb. It gathers the readings the skill names and prints them in a fixed order. It diffs the previous canonical row for the scope. It emits the `reign_checkin` row from the same dict it printed, so the row and the lines cannot disagree. Coverage is per reading. A reader that fails prints `READER FAILED <name>: <reason>` on its own line. The beat continues without it. The row names the failure under `readers_failed`. `coverage` counts the readers that answered. The printed total is `coverage` plus the failed count, so it always equals the readers the beat ran. `--change` carries the king's sentence, and the derived diff moves to `diff`. When a scope's newest check-in is older than two check-in intervals, the next stop journals one mechanical row with source `hook`. The row carries the previous fire's measured facts. A cancelled crown writes none. `no change` is refused while any reader failed, because an unread axis cannot be known unchanged. If journalling was requested but no row was written, the verb exits 3 after printing the beat. The verb never decides. It holds no graph write, no spawn and no reap. The levers stay the king's judgment. The skill's check-in body is this verb's documented output contract.

`fno agents king history` reads one crown's rows back, newest first, complete payloads. It never generates a summary. Legacy alias rows count as rejected evidence. It never silently accepts them. It reads the `events.db` store beside every journal ``paths.event_journals`` resolves, and it ingests before it reads. The store holds every durable row that the files' single rotation generation cannot keep on disk. The read is an index seek on `(scope, type, ts_ms)`, not a scan of raw rows. A zero-match answer still names every store it read, with each journal's ingested and reign-row counts. An empty history is a measurement, not an absence. `fno agents court -n` answers a different question: who holds which crown right now.

## The three crown numbers

Three different crown counts travel under similar words, and collapsing them is how a board reads clean while two rows hold one scope. A manifest split is the crown manifest naming a different session than its registry row (`manifest_session` != `registry_session`). The check-in renders it as `manifest-splits` and the ledger page shows the `splits` tile. A double rule is more than one non-terminal registry row on one normalized territory key. The check-in renders `double-ruled` plus a line naming the scope and both live holders. The ledger page shows the `double ruled` tile and flips the verdict card to its disagrees state. When a registry row flips terminal, nothing clears its crown fields. A stale crown is that terminal row still carrying crown fields. Stale crowns accumulate until `fno agents rm <row>` or the next crowned spawn over that scope clears them. The check-in renders `stale-crowned` with the remedy command, and the ledger page shows the `stale crowns` tile without flipping the verdict. The court and the crown-split reader read the STORED registry status. `fno agents list` renders served activity computed from a live probe instead. The two surfaces can disagree about the same row without either lying.

## The verdict: bounds read as a set

`fno agents king verdict` reads the reign's tenure bounds as one set and answers `converging`, `stalled`, `degraded`, or `unknown`. This reader exists because a crown ran seven days with every bound looking correctly configured while the reign sat outside all of them. 15 Budget ceiling hits told nobody, because a Budget terminal ended a turn and never escalated. `respawn_count` never moved on a reign that does not dispatch. The block-cap variable was injected only into spawned workers. No reader compared any of the numbers.

Four bounds, each with a state of `exceeded`, `within`, or `absent`. `iterations` reads loop fires against the manifest's `budget_max_iterations`. When `respawn_ceiling` is 0, `respawns` reads absent. Otherwise it reads `respawn_count` against `respawn_ceiling`. For Claude, `compactions` counts the transcript's own `compact_boundary` lines at or after the crown start. It falls back to post-compact `context_snapshot` rows in the journal. Other harnesses keep the journal reading. `block_cap` reads the `loop_check_config` recording, absent with no row. An absent bound never prints as satisfied. An unset bound and a satisfied bound are different readings. A court crown older than two check-in intervals with no recent loop check-in reads `unknown`, not `converging`.

The delivery trend splits the crown scope at the manifest `created_at`. Rows created before it are INHERITED. Rows created after are FILED by this reign. A king that files real work raises the raw undelivered count by working well. So `stalled` keys only on the inherited set. The last fire read a quiet board. Inherited nodes sit undelivered. None closed inside the window of three check-in intervals. Filing never makes a reign read stalled.

One owner assembles the verdict's inputs. The native `king-verdict` verb resolves the caller crown, the canonical scope, the crown manifest, the config values, the graph scope, and the window. It reads the inherited/filed delivery split. It scans the journals, decides the verdict, and renders the JSON payload and the human page. The config keys are `king.checkin_interval` and `king.compaction_ceiling`. Every refusal names the failed reading, and none degrade to zero. An explicit scope reads that scope's own manifest at `<space>/kings/<scope>.md`, not the caller's. Python keeps only the Typer transport, the event-path enumeration, the binary resolution, and the durable operator-channel writes. The escalation question's words render natively beside the same inputs.

`degraded`, `stalled`, and `unknown` escalate through `fno agents king escalate <scope>`. It records one deduplicated operator question naming the verdict, the exceeded and absent bounds, and the handoff offer. The offer names the succession primitive: `fno agents spawn --crown <scope> --succeed`. The operator pulls the trigger. A Budget terminal now escalates exactly like NoProgress. A failed verdict read records `verdict unreadable` rather than going silent. The king never spawns its own successor.

## The reign ledger page

`fno agents king ledger` renders `<state_dir>/reign.html` (`--out` overrides) as the Crown Ledger page. The masthead carries the generated stamp. The verdict card has four states: the court agrees with itself, holds no live crowns, cannot be read, or disagrees with itself. It shows count tiles and the readers line. Crowns group into rungs. Each crown renders a card: rung badge, status dot, agree and source tags, stats, status bar, legend, and an Active territory table. Two more sections name uncrowned epics and orphan leaves (parentless, actionable, contained by no node). Only active nodes render as table rows. Settled members show as counts in the bar and legend. The data path is the court's own. `gather_court` and the native court-fold read run in Python. The page assembly runs in the native `reign-ledger` verb (the king-history split, so the Python-tree ratchet holds). The page renders that answer and never re-derives the scope join. The ledger, `fno agents court -n`, and the fold cannot disagree about who holds a node. When the served file is older than five minutes, the `/crown` bridge route starts one background `fno agents king ledger` render. The render is single-flight, so concurrent reads start no second child. The route always serves the current file at once. The daemon's `crown_ledger` arm runs `fno agents king ledger` every 300 seconds and writes one `control_plane_tick` row per run, so the file on disk is at most one beat old while the daemon runs. The page carries the shared reload script, `crates/fno-agents/src/page_reload.js`. A tab that stays visible and untouched for `backlog.page_reload_s` seconds (default 60, 0 is off) reloads itself. It keeps its scroll position, form values, pressed chips and open sections. The page prints its generated stamp and a relative age, never its own stale verdict. An empty court renders "no live crowns" as a measurement. An unreadable registry renders the named reason. An unresolved fold states why in place. Never a blank or falsely healthy page.

## Manifest-only crowns and the dead-crown reaper

When no live row holds its scope, the court lists its manifest. Only the newest manifest naming a session counts, so a re-scope leftover is not a crown. Liveness keys on the session id, and new manifests carry no `owner_pid`.

Three facts name a manifest-only crown a DEAD CROWN. No live registry row carries its territory key. The manifest's holder session is proven dead. The proof is one of three witnesses. The claude roster lists the session in a terminal state. Its roster pid is gone. Or the roster is a clean full read that does not list the session while its transcript has been quiet past the window. The manifest itself is older than the window. The window is three check-in intervals, twelve hours at the default. Every other case is unknown and keeps the crown. The keep names its reason: an unreadable or partial roster, a missing transcript, a non-claude harness, a holder on the roster in any non-terminal state. A crown older than the window keeps unless the holder is proven dead. A holder dead for an hour keeps until the crown itself passes the window.

The daemon retire arm owns the reaper. Each tick runs the dead-crown sweep before the registry sweep. The manual `fno agents reap` verb runs it beside its own report, under the `crowns` key in its JSON and as `would vacate` lines under a dry run. The vacate re-checks both facts under their locks. A manifest rewritten to a new session mid-sweep refuses the write. A live row that took the scope refuses it too. Stale crown fields on terminal rows over the vacated scope clear in the same registry write. The receipt names who inherits: the live crown one rung up whose scope covers the dead one, else the operator. The sweep grants no crown. A vacated epic falls back to its project's territory by construction.

`fno agents king done --scope <scope>` stays the attended override: it needs no death proof and vacates whatever holds the scope. One case the reaper does not cover: a crown held by a live-status row whose session is dead. That case belongs to the king wake's successor path under its respawn ceiling.

## The codex limit

Codex exposes none of `/goal`, `/loop`, or Monitor. A codex reign has no self-injected beat. The wake arm's backstop is its only pulse. The skill names this in its first line.

## Config keys

`config.king` carries the injected texts, so an OSS user edits one place. `king.checkin_interval` defaults to `4h`. The verdict window is three intervals and the hook missed-beat row is two intervals, so the defaults read 12h and 8h. `king.checkin_text` and `king.goal_text` carry the prompts. The skill prints the defaults verbatim. A fresh install runs with no config.
