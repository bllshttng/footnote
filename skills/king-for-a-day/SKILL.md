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
2. **Level 1 is the current ceiling.** A level-1 king crowns nobody and spawns workers. Nothing in the model forbids level 2; there is simply no evidence yet that a third tier earns its translation layer, and the scope-subset rule means it can be raised later without changing anything else.

State your level and scope in your own opening line, so the transcript records what you believed you were authorized to do.

**Abdicate.**
This is orthogonal to the crown and equally load-bearing.
A king who crowns a subordinate and then stays alive to watch it has made itself a permanent monarch, which is the shape this design exists to prevent.
Fan out, record what you fanned out, exit.
If a crowned session dies, the next one sees it in the graph and re-crowns; that is the recovery path, not a regency.

**Crown kings on a frontier model at high effort.**
A pass makes judgment calls (which wave, what to park, what to supersede) and those are the calls not to cheap out on.
Grooming stays on a small model because it is daily and levers-only; a reign is rare and bounded, so the cost argument does not apply to it.

```bash
fno agents spawn king-<epic> "<brief>" --effort high --model <your frontier model>
```

What a reign actually requires is a frontier-class model at high reasoning effort, in a session that can run many steps.
How you spell that depends on your provider, so take the requirement and not this line's defaults.

- **`--effort high`** is the portable half: it is validated against whichever provider is selected, and unset just takes that provider's default.
- **`--model`** is provider-specific by nature. Name your own provider's frontier model; there is no cross-provider alias for "the good one".
- **Substrate** defaults to `pane`, which works on every provider and is the right answer here. `bg` is a detached claude-only thread and hard-errors elsewhere. `headless` is a one-shot and does **not** fit a multi-step reign, whatever the provider.

**Authority for the worker you crown.**
`--yolo` means "full auto, no gates", and the *skill* surface translates it per provider: through `/fno:agent spawn` it maps to `--permission-mode bypassPermissions` on claude, while codex gets its literal bypass flag.
An explicit `--permission-mode` you pass always wins over the mapping.
The trap is that this translation lives in the skill's normalize step, so `fno agents spawn --yolo` called directly on claude is a genuine no-op with only a stderr note.
Prefer the skill surface, or pass your provider's own posture flag when you go straight to the CLI.

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

The direct form: `fno mail send <harness>-<short-id> "<msg>"` (`claude-<short-id>`, `codex-<short-id>`, `opencode-<short-id>`, ...), or the session's slug if you know it.
Every session prints its own handle in its startup header; find a peer's with `fno agents discovered-json` or `fno agents top`.
Add `--from-self` to stamp your own reply handle so the answer comes back to you, and do not trust a sender's advertised `from-name` as an address - it can be stale.

The fallbacks, and why they rank below: `fno mail send <name>` reaches a registered agent (fine when the name resolves); `--to-project <X>` is anycast that queues durable into what may be a ghost inbox when no live peer resolves - and the receipt still reads like success.
The envelope is always written before delivery is attempted, so a send survives a dead recipient; that durability is recovery, not delivery.
**Treat any receipt that is not `delivered (hosted)` as not delivered: re-resolve the handle and send again, do not re-queue.**

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

## What a pass is not

- **Not a supervisor.** Guards narrow what the daemon may select; they never add a second dispatch path. A king encodes and abdicates.
- **Not self-appointed.** Being handed an epic to work on is not a tag. If nobody granted you orchestrator authority with a level and a scope, you are a worker on that epic, and spawning subordinates is out of bounds.
- **Not a groomer.** Grooming is the daily reversible pass (defer + reason, rank, report). A king promotes and wires. Grooming may quarantine; only humans and grooming supersede.
- **Not a driver.** You may `peek` at anything. Attaching and steering a worker is someone else's job, and doing it means you are burning frontier tokens on work a builder already owns.
- **Not a decider of unknowns.** A question you cannot answer from the track goes to the triage pile (`fno backlog defer <id> -R "<question>"`), not into a guessed edge. A day of latency beats a wrong forced decision.

## Done when

The tail dispatches in the intended order from graph state alone, with no reference to this session's transcript, and the mission shows in the sideline.
If reproducing your plan requires reading what you said, you did not encode it.
