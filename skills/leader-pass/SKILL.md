---
name: leader-pass
description: "Encode-before-exit checklist for an episodic batch leader: read the track, write the wave plan, encode it into the graph, kick off the mission, exit. Use when: 'lead this epic', 'plan the next wave', 'encode the wave plan', 'leader pass on <epic>'."
argument-hint: "<epic-id>"
---

# Leader pass

A leader pass is a **batch**, not a process.
One fresh-context session reads a track, decides the next wave or two, writes that decision into the graph, kicks the mission off, and exits.
Nothing supervises afterward: the daemon's reflexes are unchanged, and the tail dispatches from graph state alone.

The core loop is **keep-map-true + promote-next-wave**.
It is never dispatch-ordering: you do not hand work to workers, you make the graph say what should run next and let the existing hands do their job.
If you find yourself wanting to watch a worker, the pass is already over.

## Run it in this order

The order is the whole point.
Steps 3a and 3b are separated because a `ready` node with a `plan_path` is dispatched by the active-backlog daemon within about a minute.
Wiring `blocked_by` *after* linking loses that race and stampedes a wave that was supposed to be serialized.

### 1. Read the track

```bash
fno backlog epic status <epic>          # children: status, worker, PR
fno backlog get <id>                    # one node in full
gh pr list --state open --json number,title,headRefName
```

Read the epic's plan doc too.
You are looking for three things: what landed since the last pass, what is running now, and which nodes are lying about their state.
A `ready` node with no plan and a `blocked` node whose blocker merged are both worth a second look.

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

Status is derived on read, never stored, with the precedence `done > superseded > deferred > in_review > blocked > claimed > idea > ready`.
`ready` is simply the fallthrough for "has a `plan_path` and nothing else is true", so linking a plan to an unblocked node *is* the arming action.
That is the right move for the head of a wave and the wrong move for a design doc you are filing for later.
There is no rung between `idea` and `ready` to park a designed-but-unbuilt node at, so a doc you do not want built yet goes on a node that is `blocked` or `deferred`, or stays unlinked.

**3c. Route the nodes that need thinking rather than building.**

```bash
fno backlog update <id> --dispatch-verb /think
fno backlog update <id> --dispatch-brief "<what to decide>"
```

An L-sized node with no design should get a `/think` pass, not a builder.

Note what these two verbs do and do not do.
They change *how* a dispatcher launches a node that it has already selected; they do not make it selectable.
A plan-less node derives to `idea`, and every autonomous path selects from `ready` only, so setting `--dispatch-verb` on a plan-less node arms nothing.
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
- **Not a decider of unknowns.** A question you cannot answer from the track goes to the triage pile (`fno backlog defer <id> --reason "<question>"`), not into a guessed edge. A day of latency beats a wrong forced decision.

## Done when

The tail dispatches in the intended order from graph state alone, with no reference to this session's transcript, and the mission shows in the sideline.
If reproducing your plan requires reading what you said, you did not encode it.
