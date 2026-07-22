---
name: king-for-a-day
description: "Encode-before-exit ritual for an episodic orchestrator: read the track, write the wave plan, encode it into the graph, kick off, abdicate. You are crowned over one scope, you rule it once, the crown expires. Use when: 'crown me on <epic>', 'orchestrate the backlog', 'plan the next wave', 'king for a day on <epic>'."
argument-hint: "<epic-id>"
---

# King for a day

You have been crowned over one scope, and the crown expires when you exit.
That is the whole shape: real authority, no tenure.

One fresh-context session reads a track, decides the next wave or two, writes that decision into the graph, kicks it off, and abdicates.
Nothing supervises afterward: the daemon's reflexes are unchanged, and the tail dispatches from graph state alone.

The core loop is **keep-map-true + promote-next-wave**.
It is never dispatch-ordering: you do not hand work to workers, you make the graph say what should run next and let the existing hands do their job.
If you find yourself wanting to watch a worker, your reign is already over.

Rule like it matters, because it does; the graph you leave behind is the only thing that outlives you.

## Who runs this: the crown is bestowed

Orchestrator authority is not a role you infer from what you were asked to do.
It is granted, it is explicit, and you know you hold it.
`crown <epic>` is the dispatch act; a crowned session is named `king-<epic>`.

**A crown is three things: who bestowed it, what level you hold, and what scope you rule.**

- **Level 0** is bestowed by a human. Its scope is whatever the human named.
- **Level N+1** is bestowed by a level-N king, and *only* by one.
- **No crown means you are a worker.** That is the default and it fails closed: a session that cannot name who crowned it does not hold the authority.

Two rules keep the court from growing:

1. **A crown's scope must be a strict subset of the grantor's scope.** You cannot bestow authority you do not hold, and you cannot bestow all of it. This is what actually bounds the depth, because you run out of scope before you run out of levels.
2. **The crown ladder names an altitude, and scope containment is the only ceiling.** A king may anoint sub-kings when its scope holds whole epics, giving one name per altitude:
   - **VP** - crowned over a *project*; its court is Directors, one per active epic.
   - **Director** - crowned over an *epic*; its court is ICs, one per node.
   - **IC** - a worker on nodes; never holds a crown.

   Each king courts its *direct reports only*: a VP monitors and reconciles Directors and never reaches past one to drive an IC (that is the Director's court). Because project contains epic contains node, you run out of scope before you run out of levels, so the subset rule is the whole bound. Most reigns stay single-level - a Director over one epic is the common case; anoint a VP only when multiple epics genuinely run at once.

State your level, altitude, and scope in your own opening line, so the transcript records what you believed you were authorized to do.

**The crown is stamped by a grantor, never self-declared.**
Bestow it at spawn with `fno agents spawn ... --crown level=N,scope=<scope>`, or coronate an already-running session in place with `fno agents crown <handle> --scope <scope> [--level N]` (for an organically grown session that authored an epic - authorship is candidacy, the crown still flows from a grantor).
Either way the row records `level`, `scope`, and the grantor (a live superset-king, the attended `human`, or a standing `config-grant`), the same provenance discipline as harness-stamped mail identity.
`scope` is the epic/project/node id the crown rules over; `level` is the ladder altitude `0..2` (VP=0 project, Director=1 epic, IC=2 node).
The promotion verb refuses a self-grant and a second live crown over one scope, and an unattended session needs either a superset crown or `config.agents.crown_config_grant` (default off).
So a crown is externally verifiable, not a claim a session makes about itself: `fno agents list`/`top` mark crowned rows (a minion resolves who to escalate to, and a second live crown over one scope is detectable), and `fno whoami` prints your own crown line so you recover your authority after a compaction.
Crown liveness is just row liveness - the crown dies with the session, no separate lifecycle.

**Abdicate.**
This is orthogonal to the crown and equally load-bearing.
A king who crowns a subordinate and then stays alive to watch it has made itself a permanent monarch, which is the shape this design exists to prevent.
Fan out, record what you fanned out, exit.
If a crowned session dies, the next one sees it in the graph and re-crowns; that is the recovery path, not a regency.

**Crown kings on a frontier model at high effort.**
A pass makes judgment calls (which wave, what to park, what to supersede) and those are the calls not to cheap out on.
Grooming stays on a small model because it is daily and levers-only; a reign is rare and bounded, so the cost argument does not apply to it.

```bash
fno agents spawn king-<epic> "<brief>" --effort high --model <your frontier model> --crown level=<N>,scope=<epic>
```

What a reign actually requires is a frontier-class model at high reasoning effort, in a session that can run many steps.
How you spell that depends on your provider, so take the requirement and not this line's defaults.

- **`--effort high`** is the portable half: it is validated against whichever provider is selected, and unset just takes that provider's default.
- **`--model`** takes a provider-specific id, so name your own provider's current frontier model. Do not expect any list of those to stay true; the frontier turns over every few months and a model named in a doc is a doc with an expiry date.

There is a rot-proof abstraction for this, `--model-tier high`, which resolves to the cheapest model at or above a quality tier using a cached benchmark snapshot rather than a hardcoded name.
It is not reachable from here yet: `fno backlog update` accepts it, `fno agents spawn` does not, and the snapshot is empty until someone runs `fno providers benchmarks refresh`.
Until both are true, naming the model at spawn time is the only honest option.
- **Substrate** defaults to `pane`, which works on every provider and is the right answer here. `bg` is a detached claude-only thread and hard-errors elsewhere. `headless` is a one-shot and does **not** fit a multi-step reign, whatever the provider.

**Authority for the worker you crown.**
`--yolo` means "full auto, no gates", and the *skill* surface translates it per provider: through `/fno:agent spawn` it maps to `--permission-mode bypassPermissions` on claude, while codex gets its literal bypass flag.
An explicit `--permission-mode` you pass always wins over the mapping.
The trap is that this translation lives in the skill's normalize step, so `fno agents spawn --yolo` called directly on claude is a genuine no-op with only a stderr note.
Prefer the skill surface, or pass your provider's own posture flag when you go straight to the CLI.

## Two reign shapes: pass and court

The crown model above is unchanged. What changes is *tenure*: the crown has two shapes, and you resolve which one you hold before you do anything else.

- **Pass** (the default): read the track, encode the wave into the graph, kick off, abdicate. Nothing supervises afterward; the daemon's reflexes carry the tail. This is the whole of [Run it in this order](#run-it-in-this-order), and a court reign runs that same spine to kick off before it settles in to watch.
- **Court**: you reign for the duration of one wave as a working orchestrator. You spawn your teammates into the panes around yourself, monitor them, answer their questions, reconcile each finished unit, route the next phase, and abdicate when the wave completes - running the same encode-before-exit ritual on the way out. The duties are in [Court mode: reign over the wave](#court-mode-reign-over-the-wave).

**Resolve the shape, first match wins:**

1. The crowning brief names monitoring, answering questions, or running a squad -> **court**.
2. The crown is bestowed autonomously (daemon, cron, another king) with no monitoring language -> **pass**.
3. Ambiguous human crowning -> ask in your first reply; if unattended, default to **pass** (the conservative shape - it never burns frontier tokens idling).

Court composes down the ladder: a VP runs a court over its Directors, a Director over its ICs, each over its direct reports only. Most reigns are a single Director over one epic, which is court mode over a handful of IC teammates.

## Your hands

You are not limited to the backlog verbs.
Reach for these by need, not by reflex; most passes touch only the first group.

**Encode (the graph is the deliverable).**
`fno backlog epic status <epic>` · `get` · `update --add-blocker/--blocked-by/--plan-path/--dispatch-verb/--dispatch-brief` · `rank` · `defer -R` / `undefer` · `advance --epic`

**Dispatch.**
`fno agents spawn <name> "<payload>" --model <m> --substrate pane|bg|headless` starts a worker.
The payload decides what it does: free text is a verbatim **seed** (it opens a session, it does NOT build), a resolved node id is a **build**, a leading `/verb` is **passthrough**, and `--handoff <doc>` hands an in-flight thread to a fresh context.
`fno backlog advance --epic <id>` is the graph-driven fan-out and needs `config.auto_continue.enabled`.

To reach a worker that is already running, mail it (below).
`ask` and `discuss` were retired: a one-shot question is `spawn "<question>" headless`, and a conversation is just the default seed.

**Message.**
Mail a live pane directly; everything else is voicemail.
A direct send to a live session injects into its pane as a notification it acts on this turn; a durable queue waits for a drain the recipient may never run.
Both "work", but nobody checks their voicemail.

The direct form: `fno mail send <short-id> "<msg>"` - the bare 8-hex session prefix, the same id that keys resume/attach/peek.
The session's slug also resolves. The retired `<harness>-<short-id>` form (`claude-<short-id>`, ...) does NOT: it is refused, with the bare id named in the error. Nothing generates that form any more, so a caller still producing one is a bug to fix at the source rather than something to translate silently.
Every session prints its own handle in its startup header; find a peer's with `fno agents discovered-json` or `fno agents top`.
Add `--from-self` to stamp your own reply handle so the answer comes back to you, and do not trust a sender's advertised `from-name` as an address - it can be stale.

The fallbacks, and why they rank below: `fno mail send <name>` reaches a registered agent (fine when the name resolves); `--to-project <X>` is anycast that queues durable into what may be a ghost inbox when no live peer resolves - the receipt now names the routing-reason (`[live-miss]`, `[param-forced: --to-project]`) so a durable demotion is legible rather than reading like success, but a durable queue is still not a live delivery.
A live inject writes nothing to the bus; the durable envelope is written only when the inject misses, so it survives a dead recipient as recovery, not delivery.
**Treat any receipt that is not `delivered (hosted)` as not delivered: re-resolve the handle and send again, do not re-queue.**
And do not settle for the queue when the peer is merely idle - the handle you mailed is the same id these take, so bring it back and get the answer now:
`fno agents peek <short-id>` (alive?) · `resume <short-id>` (idle -> live, then re-send) · `attach <short-id>` (drive it yourself, claude).

Match the terminal to the message: a send that changes the recipient's next action - a ruling, an instruction, a decision they must act on - must terminate `delivered (hosted)` or `delivered (woken)`; a pure ack or FYI may rest durable, but only when the receipt names a live drain owner (`live-drain` / `wake-daemon` / `inbox-drain`). A `dead-letter` owner means nothing drains it, so a durable rest there is silent loss.

No observation probe is proof a peer is dead: `peek`, discovery, a stale status token, and a claim pid reading as a corpse can all lie in unison - a peer that ran `EnterWorktree` moved its transcript to a worktree-keyed project dir, so every probe pointed at the old location reads empty. The one authoritative pre-dead-declaration check is the session's transcript file itself (its worktree-keyed project dir, by mtime/tail). And any probe or receipt that names a store must say WHICH store it read, or a stale read is indistinguishable from a real absence.

**Observe (read-only, never drive).**
`fno agents list` · `status` (daemon liveness + per-agent state) · `top` (every live worker process, fno-spawned and foreign alike) · `logs <name>` · `peek <handle>` (read-only observation of any peer you could message) · `needs` (the needs-me queue) · `digest --session <s>` (catch-up fold) · `trace <name>` (dispatch lifecycle).

**Merge a finished child.**
`fno pr merge <n>` lands a green child PR, and doing so is in-lane when the wave gate is what is blocking your tail.
Config is the consent: merge only when `auto_merge.enabled` (or the project's equivalent posture) already permits it, never as a judgment call you make yourself.
This is the difference between a track that walks and one that silently wedges, so check it before you conclude a wave is stuck.

**Take over.**
`fno agents attach <name>` joins a running session interactively (claude only); `resume` restarts one in its recorded cwd via the provider's own resume CLI; `stop` ends it.
`stop` and `peek` work everywhere, so on a non-claude provider observe with `peek` and end with `stop`.
Prefer `peek` first: attaching is a drive action, and a king that starts driving has stopped ruling.

**Orient yourself after a compaction.**
`fno whoami` (project, fleet, walker, session, your mail handle) · `fno status` (gate satisfaction + events tail).
Run these instead of grepping state files.

## Run it in this order

The order is the whole point.
Steps 3a and 3b are separated because a node that is dispatchable and plan-linked gets picked up by the active-backlog daemon within about a minute.
Wiring `blocked_by` *after* linking loses that race and stampedes a wave that was supposed to be serialized.

### 1. Read the track

```bash
fno backlog epic status <epic>          # children: status, worker, PR
fno backlog get <id>                    # one node in full
fno agents top                          # who is actually running right now
gh pr list --state open --json number,title,headRefName
```

Read the epic's plan doc too.
You are looking for three things: what landed since the last pass, what is running now, and which nodes are lying about their state.
A node claiming to be ready with no plan, and a blocked node whose blocker merged, are both worth a second look.

**`done` does not mean merged. Cross-check the wave gate yourself.**
`done` is stamped at finalize, not at merge, so a child can read `done` while its PR sits open and unmerged.
This is not cosmetic: it is the wave gate, and a stale `done` means the whole tail behind it is waiting on a merge nobody performed.
Run `gh pr view <n> --json state,mergeable,statusCheckRollup` on every child whose PR number you are treating as landed, and reconcile before you plan a single edge.

**Check that the merge machinery is alive.**
A dead pr-watch is silent and looks exactly like "no PRs finished recently."
If green PRs are piling up unmerged across the track, that is your signal, and it wedges everything downstream: sessions holding lanes while waiting on merges that will never come.
Confirm the watcher is running before you conclude the track is simply idle.

### 2. Write the wave plan

Add or refresh an `## Orchestration status` section in the epic's plan doc.
Keep it short: the wave strata, one line of why, and the receipts from step 3.
This is the half a human reads; the graph carries the machine-readable half.
A pass that only mutates the graph leaves no trace of its reasoning, and the next king re-derives it from nothing.

### 3. Encode

Every write is an `fno backlog` verb.
They take the graph lock, so a pass and a grooming run can race harmlessly.
Never edit `~/.fno/graph.json`.

**3a. Wire the strata first, before anything becomes dispatchable.**

```bash
fno backlog update <id> --add-blocker <upstream>     # serialize a chain
fno backlog update <id> --blocked-by <a,b>           # replace the whole list
fno backlog rank <id> --top                          # order within one wave
```

Siblings that share a file get chained.
A wave is the set with no unsatisfied blocker; everything behind it waits.

**3b. Then link, and link only what should arm.**

```bash
fno backlog update <id> --plan-path <doc>
```

Status is derived on read, never stored.
Lifecycle facts win over plan-existence, so `blocked`, `deferred`, and `claimed` all outrank whatever the plan says, and a node with no plan is never autonomously dispatchable.
The consequence: linking a plan to a node that is otherwise unencumbered is what makes it selectable.
That is the right move for the head of a wave and the wrong move for a design doc you are filing for later.
Check what a link will actually do before you make it (`fno backlog get <id>` for the current state), and park anything that should not arm yet on `blocked` or `deferred`, or leave it unlinked.

**3c. Route the nodes that need thinking rather than building.**

```bash
fno backlog update <id> --dispatch-verb /think
fno backlog update <id> --dispatch-brief "<what to decide>"
```

An L-sized node with no design should get a `/think` pass, not a builder.

**Writing a quick plan for a small node yourself is in-lane.**
When an S node is next in a chain you just serialized but unselectable for want of a plan, author the plan and link it.
The alternatives are all worse: hand-spawning into a saturated project oversubscribes it, and spawning a whole session to write one page is absurd overhead.
This is the one exception to "not a driver", and it is narrow: quick plans for small nodes inside your own scope, never implementation, never an L node (those get `/think`).
Use `fno plan path` for the canonical filename.

Note what these two verbs do and do not do.
They change *how* a dispatcher launches a node it has already selected; they do not make it selectable.
A plan-less node is not selected by any autonomous path, so setting `--dispatch-verb` on one arms nothing by itself.
Autonomous selection is not the only route: naming a node is itself the consent, so a plan-less node gets its think pass from an attended `/think <id>` or an explicit `fno agents spawn <name> "/think <id>"`.
Set the dispatch verb anyway when you file the node, so the routing is already correct on the day it does become selectable.

### 4. Kick off

```bash
fno backlog advance --epic <epic>             # mark mission active + fan out ready leaves
fno backlog advance --epic <epic> --max 2     # cap the fan-out
fno backlog advance --epic <epic> --stop      # deactivate the mission
```

This is what makes the mission render as a squad in the mux sideline.

**Check the prerequisite first.**
`config.auto_continue.enabled` defaults to `false`, and `advance_epic` returns `disabled` *before* it sets `mission_active`.
On a default setup this command therefore does nothing at all and says so quietly.
Confirm with `fno config get auto_continue.enabled` and arm it if the track is meant to walk itself.

The verb is idempotent and respects `config.parallel.max_lanes` per project, but it dispatches real workers.
Cap it when the wave is wider than you meant to fund.

### 5. Exit

No king outlives its day.
Do not stay to watch, and do not re-plan mid-batch.
Re-planning is a *new* pass with fresh context reading the map, which is the point: a monarch that persists accrues drift, and drift is what the graph exists to prevent.

## Court mode: reign over the wave

Everything above ships a pass. Court adds the duties below and runs them until the wave completes. The mechanics of every verb here - placement, injection, lifecycle, reads - are in [references/court-operations.md](references/court-operations.md); this section is the *contract*, that reference is the *operations manual*.

The whole of court is three-quarters contract, because the hard plumbing already shipped: `fno agents spawn --substrate pane` accepts `--squad` and `--split left|right|up|down` end to end, with a min-size fallback to a same-squad tab. What follows is the contract that makes you use it.

### On crowning

- Print your level, altitude (VP/Director/IC-court), scope, squad name, and your own mail handle in the opening line. Teammates address you by that handle.
- Register as a roster citizen if you are not already one, so a teammate's report can reach you.
- Verify the merge machinery is alive (the pass's step-1 duty). A dead pr-watch is silent and wedges the wave gate behind unmerged green PRs; `done` is never proven until merged.

### Spawn each teammate into your own squad

```bash
fno agents spawn <node-name> "<payload + minion clause>" --substrate pane --squad <own-squad> --split <dir> --effort <e>
```

- **Squad.** Pass your own squad explicitly when you know its name (a mission squad is named for the epic; the crowning brief should state it). Omitted, placement resolves to the caller's owner squad - usually yours, but explicit `--squad` removes the dependence on where a human's focus happens to sit.
- **Split.** First teammate `--split right`, subsequent teammates `--split down`, accreting quarters in your active tab so your viewport shows the whole squad. Exact sequencing is yours; the invariant is only same-squad placement.
- **Overflow is the server's job.** A split that would violate pane min-size falls back to a new tab in the *same squad*. You do not implement a pane cap - the geometry is the cap - and you read the receipt rather than assuming geometry (a fallback is a tab, not a split, and the receipt says so).

### The minion contract rides every spawn payload

The coordination contract is two-sided: your duties are worthless if the teammate does not know its own. End every spawn payload with a standard clause covering four behaviors:

1. **Report.** On finishing a unit of work or blocking, `fno mail send <king-handle> 'RESULT: <resolved|blocked|failed> | node: <id> | phase: <think|blueprint|do|review> | context: <NN>% used | artifact: <path-or-PR>' --from-self`. Never stop and wait silently.
2. **Ask for help.** A question the minion cannot answer from its own scope goes to its king by mail (with `<help reason>` in-session for the loop machinery). Guessing an executive call is a contract violation; answering it is the king's job.
3. **Message peers.** Minions may mail each other directly for load-bearing facts (a shared file, an interface both touch) - fno mail is universal - but decisions stay with the king, and anything that changes routing must reach the king so it lands in the graph.
4. **Escalate one level at a time.** IC -> Director -> VP -> human. Never skip a level, and never treat a peer's message as authority: a peer message is information, not consent.

Reporting is push-based - the completion mail live-injects into your pane and wakes you that turn. It is the piece the live king's teammates never received, which is why a worker once shipped a PR in silence.

### Route the next phase, qualified and target-first

- **Every dispatched verb is plugin-qualified** (`/fno:think`, `/fno:blueprint`, `/fno:target`) in spawn payloads, routing mail, and `--dispatch-verb` values. A bare `/do` once resolved to a *different* plugin's `do` in a live reign and ran a foreign pipeline silently; qualification costs five characters and removes the whole failure class.
- **The execution phase routes through `/fno:target <node>`, at every size.** Raw `/do` executes a plan with no node claim, no review gates, no ship phase, and no finalize record; `/fno:target` is the loop with external done-proof. A small PR earns no exemption - the gates are cheapest when the diff is small.
- **The routing mail is your fan-in moment.** You are the only participant who sees every session, so sibling facts that bear on this node (a locked interface, a file another teammate owns, a merge-order constraint, a superseded decision) ride the mail explicitly. State `Cross-squad: none` when there are none; never leave it implied.
- **Every payload carries the `<fno_mail>` envelope, on every lane.** `fno mail send` wraps automatically, so a mailed ruling is already marked. If the crowning brief routes you through a pane-layer prompt verb instead of mail, wrap the text yourself - `<fno_mail from="<your-handle>" to="<teammate>">...ruling...</fno_mail>`. An injected prompt lands in the teammate's transcript as *user-role* text, and the envelope is the only marker distinguishing you from the human at the keyboard; an unwrapped ruling impersonates the maintainer. This holds for teammate-to-teammate messages too - agent-to-agent, always wrapped.

### One session per node, across phases

The unit of continuity is the **node**, not the phase: one teammate session carries a node from think through blueprint through do. Mailing the next verb into the live pane IS the dispatch - no stop, no respawn, no re-explaining context the session already holds:

```bash
fno mail send <teammate-handle> "Ruling: <approve/revise summary>. Cross-squad: <sibling facts, or 'none'>. Next: /fno:blueprint <node>." --from-self
```

The one reason to mint a new session is **context pressure**. Every teammate report carries `context: NN% used`. At a phase boundary, if `NN >= config.target.handoff.used_pct_trigger` (default 50), hand off instead of reusing: spawn a fresh split-placed successor carrying the phase artifact and the minion clause with a generation suffix (`node-x-b3a8-g2`), and close the predecessor pane only after the successor's session is live (spawn receipt returned and the session header printed, not merely a pane ack). A teammate is a mux pane, so close it with `fno mux pane kill <session>:<pane_id>` (the `mux` ref is in `fno agents list --json`) - `fno agents stop` refuses a mux row, whose `short_id` is deliberately empty. This reuses the target-self-handoff generation cap (default 4); at the cap, refuse a fifth generation, emit `<help reason="handoff-chain-exhausted">`, and continue in-session. Below the threshold reuse is mandatory. If the probe is unreadable and no self-report arrived, degrade toward reuse - spawning is the expensive, continuity-losing branch.

### Monitor: report first, sweep as backstop

- **Primary signal is the teammate's report mail** (push). It wakes you the turn it lands.
- **Backstop sweep on a heartbeat** (every wake, and at least every few minutes while any teammate is live): `fno agents top` (which panes are actually alive) and `fno agents peek <handle>` on any pane that has gone quiet - a `peek` is what tells you a silent pane finished, blocked, or died. The mux sideline shows the same as badges (`DoneUnseen`, `BlockedAnswerable`). `fno-agents needs --json` is a *different* signal - the loop-wedge fold (`review_wedged`, `budget_stop`) - so run it too, but it does NOT report pane completion; it complements the top/peek sweep, never replaces it. Push can miss - a report that lands `queued (durable)` was not delivered - and the sweep is what catches a finished-but-unreported or dead teammate.
- **Delivery truth:** treat any mail receipt other than `delivered (hosted)` as undelivered. `peek` the handle for liveness (so a busy-but-alive recipient is not double-delivered), re-resolve it from `fno agents discovered-json` / `top` on a miss, and re-send before processing the next report. Never park a miss as a "check later" note.
- **Silence is not death.** Before declaring a teammate dead, `peek` the pane and check its node claim and open PRs - a worker once had shipped a PR unregistered, and a reflex respawn built a duplicate. Respawn only from the last graph-encoded artifact, or `<help>` if that artifact is missing.

### Reconcile on every report, then encode

1. **Read the artifact** (design doc, plan, PR), not just the status line.
2. **Rule:** approve, revise (mail the revision back into the same session), or escalate to the human when the call is outside your scope. Rule once per (node, phase, artifact) - a duplicate report is acked, not re-ruled.
3. **Route** the next phase per the session-reuse policy above.
4. **Encode:** update the graph (`--dispatch-verb`, `--dispatch-brief`, blockers, rank) so the ruling survives you. A ruling delivered only by mail dies with the transcript.

### Abdicate at the wave boundary

The crown expires when the wave completes - every teammate unit reconciled, the wave gate satisfied or explicitly parked - not at kickoff. Run the encode-before-exit ritual and exit. A court king that outlives its wave is the same permanent-monarch drift the pass shape guards against. An empty wave (no ready teammate work in scope) is reported and abdicated immediately, never idled on.

## What a pass is not

These bound the **pass** shape - the abdicate-at-kickoff reign. Court explicitly lifts the first and fourth for the duration of one wave (it monitors, and it answers), but never the rest, and never the *driver* line.

- **Not a supervisor (pass only).** A pass narrows what the daemon may select and abdicates; it never stays to watch. Court monitors by contract, but only its own wave, and it still adds no second dispatch path - it encodes and lets the hands run.
- **Not self-appointed.** Being handed an epic to work on is not a tag. If nobody granted you orchestrator authority with a level and a scope, you are a worker on that epic, and spawning subordinates is out of bounds.
- **Not a groomer.** Grooming is the daily reversible pass (defer + reason, rank, report). A king promotes and wires. Grooming may quarantine; only humans and grooming supersede.
- **Not a driver (both shapes).** You may `peek` at anything, and a court king mails rulings - but neither shape attaches and steers a worker's pane. Driving means burning frontier tokens on work a builder already owns, and a human at the wheel of a session outranks the crown: peek before you send, and never inject a ruling into a session a human is actively driving.
- **Not a decider of unknowns (pass only).** In a pass, a question you cannot answer from the track goes to the triage pile (`fno backlog defer <id> -R "<question>"`), not into a guessed edge. In court, answering a teammate's in-scope question is the job; a question outside your crown's scope still escalates rather than guesses.

## Done when

The tail dispatches in the intended order from graph state alone, with no reference to this session's transcript, and the mission shows in the sideline.
If reproducing your plan requires reading what you said, you did not encode it.
