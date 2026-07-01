---
created: 2026-06-26T00:00
title: Using the backlog
---

# Using the backlog

A practical guide to driving `fno backlog` day to day. For the internals
(lane key, rank model, WIP caps, board rendering) see
[architecture/backlog-board-ordering.md](architecture/backlog-board-ordering.md).

## Mental model: the board is derived

You never drag a card. Both boards (`graph.md` Obsidian Kanban and the
`fno backlog view` HTML) render from node fields, so you "move" a card by
changing the field that drives its placement:

| What you see | What controls it |
|--------------|------------------|
| **Column** (Now / Next / Later / Triage / Done) | priority + lifecycle status (claimed, queued, done, deferred) |
| **Swimlane** (the per-project cluster inside a column) | `project` |
| **Position within a lane** | `rank` (curated), else `(priority, created_at)` |

`_kanban_column` is the sole column authority; `rank` never changes a card's
column.

## Creating nodes

There are three plan-less creation verbs. They overlap; pick by how much
ceremony you need:

| Goal | Command |
|------|---------|
| Capture an idea, minimal ceremony | `fno backlog idea "title" --details "why"` |
| Add a fuller node (type/size/blockers/parent) | `fno backlog add "title" --priority p2 --details "..."` |
| Create one auto-scoped to the current git repo (carries source provenance) | `fno backlog new "title"` |
| Pull an existing plan file in as a node | `fno backlog intake path/to/plan.md` |

Differences:

- **`idea`** signals "skip the spec/plan ceremony for now"; the lightest verb.
- **`add`** is the fullest manual form: also takes `--type`, `--size`,
  `--blocked-by`, `--parent`, `--roadmap-id`.
- **`new`** auto-scopes `project`/`cwd` from the current git repo (pass
  `--unscoped` to opt out) and records `--source-*` provenance; built for
  agent/automated creation.

All three accept `--details`/`--description`. A node with no `plan_path`
derives to `_status: idea` until a plan is associated.

## Editing a node

`fno backlog update <id>` edits a node in place. Use this instead of
recreating via `idea` (which produces duplicates). Editable fields:

```bash
fno backlog update <id> --title "..."          # rename
fno backlog update <id> --details "..."        # rationale ('null' clears)
fno backlog update <id> --priority p1          # see "Moving cards"
fno backlog update <id> --domain code          # domain
fno backlog update <id> --size L               # S | M | L
fno backlog update <id> --type epic            # feature | epic | bug
fno backlog update <id> --project fno --cwd /path   # reproject (move swimlane)
fno backlog update <id> --plan-path path/to/plan.md
fno backlog update <id> --public               # show on the public roadmap
```

`<id>` resolves by canonical id (`ab-1a2b3c4d`), title-derived slug
(`dashless-spawn`), or bare hex (`1a2b3c4d`).

## Moving cards

### Between columns

Columns are derived, so change the driving field:

| Target column | How |
|---------------|-----|
| **Now** | `fno backlog update <id> --priority p1` (p0/p1 -> Now), or it auto-moves when a live session claims it |
| **Next** | `fno backlog update <id> --priority p2` |
| **Later** | `fno backlog update <id> --priority p3` |
| **Triage** | the queued / pick flow (queued = awaiting human ack); not a priority |
| **Done** | `fno backlog done <id>` |
| **Off-board** | `fno backlog defer <id> --reason "..."` (deferred and superseded leave the board) |

### To a different swimlane

The swimlane is the project cluster, so reproject the node:

```bash
fno backlog update <id> --project <name> --cwd <path>
```

### Reorder within a lane

`rank` floats a card inside its `(column, project)` lane without changing its
column. Board order == work order, so `--top` also makes it run next.

```bash
fno backlog rank <id> --top            # front of the lane (and runs next)
fno backlog rank <id> --bottom
fno backlog rank <id> --before <id>    # anchor must already be ranked
fno backlog rank <id> --after <id>
fno backlog rank <id> --clear          # rejoin the priority fallback
```

## Lifecycle

`intake -> triage -> ready/next -> done`, with two reversible side states:

| Action | Command | Effect |
|--------|---------|--------|
| Pause a node | `fno backlog defer <id> --reason "..."` | leaves the board; `_status: deferred` |
| Resume it | `fno backlog undefer <id>` | returns to `ready`/`idea` |
| Replace with a newer node | `fno backlog supersede <new> --replaces <old> --reason "..."` | auto-defers old; `_status: superseded` |
| Mark complete | `fno backlog done <id>` | sets `completed_at`, unblocks dependents |
| Remove permanently | `fno backlog remove <id>` | hard delete (use for dupes / dead nodes) |

Blockers: `--blocked-by`, `--add-blocker`, `--remove-blocker` on `update`.
A node with an open blocker derives to `_status: blocked` automatically.

## Priority tiers

Lower N is more urgent. Priority and size are orthogonal.

- **p0** drop everything (incidents, blocking bugs, hotfixes) -> Now
- **p1** next-up, typically small -> Now
- **p2** normal (default) -> Next
- **p3** long-tail / experimental -> Later

## Finding work

```bash
fno backlog next                  # highest-priority unblocked node
fno backlog ready                 # the ready queue
fno backlog get <id|slug|hex>     # resolve and inspect one node
fno backlog find "query"          # high-recall search over title/slug/details
```

## Health and hygiene

```bash
fno backlog triage health          # idea pile, stale ready, collisions, dupes
fno backlog maintain --apply       # recurring sweep: re-scope, prune, auto-defer
fno backlog reconcile              # close nodes whose PR merged outside the gate
```

## Parallel lanes

With `config.parallel.max_lanes >= 2`, the active-backlog daemon dispatches up
to that many ready nodes from distinct domains concurrently, each as an
isolated bg worktree lane. Merges stay serialized (`fno pr merge` takes a
repo-wide lock, and holds a stale-base PR for `fno pr rebase` while lanes run).

```bash
fno backlog lane-fill --max 3      # preview which nodes would dispatch as lanes
fno backlog dispatch-lanes         # manually fire one lane-fill round
fno backlog lanes                  # rollup: live lanes vs the cap, per-node status
```

## Public roadmap

A curated, leak-free view for advertising an OSS project's roadmap. Opt in
per node; nothing is published unless flagged.

```bash
fno backlog update <id> --public                              # flag a node
fno backlog roadmap --project fno --out ROADMAP.md --html roadmap.html
```

The roadmap emits only title / priority / size grouped Now / Next / Later /
Shipped (Triage folds into Later). It never exposes IDs, plan paths, or cwd,
and excludes deferred / superseded nodes.
