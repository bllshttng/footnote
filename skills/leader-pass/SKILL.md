---
name: leader-pass
description: "Encode-before-exit ritual for an episodic orchestrator: read the track, write the wave plan, encode it into the graph, kick off, exit. Covers both the cross-epic mega pass and the single-epic leader pass. Use when: 'lead this epic', 'orchestrate the backlog', 'plan the next wave', 'leader pass on <epic>'."
argument-hint: "<epic-id> | --mega"
---

# Leader pass

A leader pass is a **batch**, not a process.
One fresh-context session reads a track, decides the next wave or two, writes that decision into the graph, kicks it off, and exits.
Nothing supervises afterward: the daemon's reflexes are unchanged, and the tail dispatches from graph state alone.

The core loop is **keep-map-true + promote-next-wave**.
It is never dispatch-ordering: you do not hand work to workers, you make the graph say what should run next and let the existing hands do their job.
If you find yourself wanting to watch a worker, the pass is already over.

## Who runs this

Two roles, one ritual.
The steps below are identical for both; only the scope changes.

- **Mega pass** (`--mega`): cross-epic. Decides which epics are live, spawns one leader per live epic, owns nothing inside any epic.
- **Epic leader** (`<epic-id>`): exactly one epic. Runs the ritual on that epic's children. Spawns workers.

**Lane discipline is the invariant, not a depth counter.**
An orchestrator may spawn a leader only for an epic it does not itself own, and an epic leader spawns workers only.
That single rule caps the hierarchy at two levels on its own: depth three would require an epic nested inside an epic, which is the supervision tree this design exists to avoid.

**Spawn and exit.**
A mega pass that stays alive to watch its epic leaders is an always-on supervisor wearing a different word, and that shape is already rejected.
Fan out, record what you fanned out, exit.
If a leader dies, the next mega pass sees it in the graph and re-spawns; that is the recovery path, not a babysitter.

**Run leaders on a frontier model at high effort.**
A pass makes judgment calls (which wave, what to park, what to supersede) and those are the calls not to cheap out on.
Grooming stays on a small model because it is daily and levers-only; leaders are rare and bounded, so the cost argument does not apply to them.

```bash
fno agents spawn leader-<epic> "<brief>" --model fable --substrate bg
```

Note for claude workers: `--yolo` is a no-op (it only maps to a real bypass on codex).
Permission posture on claude comes from `--permission-mode`, and bypass is already the default.

## Your hands

You are not limited to the backlog verbs.
Reach for these by need, not by reflex; most passes touch only the first group.

**Encode (the graph is the deliverable).**
`fno backlog epic status <epic>` · `get` · `update --add-blocker/--blocked-by/--plan-path/--dispatch-verb/--dispatch-brief` · `rank` · `defer -R` / `undefer` · `advance --epic`

**Dispatch.**
`fno agents spawn <name> "<msg>" --model <m> --substrate pane|bg|headless` starts a worker.
`fno agents ask <name> "<msg>"` follows up on one already running.
`fno backlog advance --epic <id>` is the graph-driven fan-out and needs `config.auto_continue.enabled`.

**Message.**
`fno mail send <name> "<msg>"` reaches a registered agent; `--to-project <X>` reaches a project (live peer delivers, none queues durable).
`--from-self` stamps your own reply handle so the answer comes back to you.
The envelope is written before delivery is attempted, so a send survives a dead recipient.

**Observe (read-only, never drive).**
`fno agents list` · `status` (daemon liveness + per-agent state) · `top` (every live worker process, fno-spawned and foreign alike) · `logs <name>` · `peek <handle>` (read-only observation of any peer you could message) · `needs` (the needs-me queue) · `digest --session <s>` (catch-up fold) · `trace <name>` (dispatch lifecycle).

**Take over.**
`fno agents attach <name>` joins a running claude session; `resume` restarts one in its recorded cwd; `stop` ends it.
Prefer `peek` first: attaching is a drive action and a leader that starts driving has stopped leading.

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

### 2. Write the wave plan

Add or refresh an `## Orchestration status` section in the epic's plan doc.
Keep it short: the wave strata, one line of why, and the receipts from step 3.
This is the half a human reads; the graph carries the machine-readable half.
A pass that only mutates the graph leaves no trace of its reasoning, and the next leader re-derives it from nothing.

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

No leader outlives its batch.
Do not stay to watch, and do not re-plan mid-batch.
Re-planning is a *new* pass with fresh context reading the map, which is the point: a leader that persists accrues drift, and drift is what the graph exists to prevent.

## What a pass is not

- **Not a supervisor.** Guards narrow what the daemon may select; they never add a second dispatch path. A leader encodes and leaves.
- **Not a groomer.** Grooming is the daily reversible pass (defer + reason, rank, report). A leader promotes and wires. Grooming may quarantine; only humans and grooming supersede.
- **Not a driver.** You may `peek` at anything. Attaching and steering a worker is someone else's job, and doing it means you are burning frontier tokens on work a builder already owns.
- **Not a decider of unknowns.** A question you cannot answer from the track goes to the triage pile (`fno backlog defer <id> -R "<question>"`), not into a guessed edge. A day of latency beats a wrong forced decision.

## Done when

The tail dispatches in the intended order from graph state alone, with no reference to this session's transcript, and the mission shows in the sideline.
If reproducing your plan requires reading what you said, you did not encode it.
