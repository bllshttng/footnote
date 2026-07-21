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
derives to `status: idea` until a plan is associated.

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
| Pause a node | `fno backlog defer <id> --reason "..."` | leaves the board; `status: deferred` |
| Resume it | `fno backlog undefer <id>` | returns to `ready`/`idea` |
| Replace with a newer node | `fno backlog supersede <new> --replaces <old> --reason "..."` | auto-defers old; `status: superseded` |
| Mark complete | `fno backlog done <id>` | closes only on a MERGED PR; sets `completed_at`, unblocks dependents |
| Remove permanently | `fno backlog remove <id>` | hard delete (use for dupes / dead nodes) |

Blockers: `--blocked-by`, `--add-blocker`, `--remove-blocker` on `update`.
A node with an open blocker derives to `status: blocked` automatically.

**done = merged.** `fno backlog done` closes a node only when a referenced PR is
MERGED. An OPEN PR (even with green CI) exits 5 (awaiting merge): the node stays
`in_review` and closes on the actual merge via `reconcile` / merge-triggered
`advance`. A session finishes at PR-up + CI-green + reviewed (it never waits on a
human merge); only the graph close waits for the merge, so the "done" state means
"landed on main" uniformly, whoever closes it. Exit codes: 0 closed, 3 refusal
(CLOSED-unmerged / no evidence), 4 gh outage (retryable), 5 awaiting merge.

`fno backlog update <id> --completed` is the un-cross-checked operator bypass
(same authority class as `done --force`, without the reason ceremony): it applies
completion with no gh evidence check. Use it only by hand for a node whose real
state the cross-check cannot see; automation must never call it.

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

### The daily pass

`fno backlog groom` is the single grooming surface, and it runs the whole pass:

1. The mechanical legs, in order - `archive --apply` (age-gated, `--age`, default 14 days), `reconcile`, `maintain --apply`, then `relatedness build` last so the map reflects the post-groom graph.
   Best-effort: one failing leg is named in the receipt and does not cost you the other three.
   A leg is `ok` only on exit 0; exit 4 is recorded as `partial` (in this CLI it always means a degraded result, such as PR queries `reconcile` could not resolve, never "nothing to do").
   If any leg comes back other than `ok` the receipt status is `degraded` and the verb exits non-zero, and the worker names the leg in its report - a scheduler log nobody reads is not a signal.
2. One Sonnet worker for the judgment calls, working from a fixed allowlist of reversible levers, finishing by mailing a one-screen report that leads with the mechanical outcomes.

A UTC-day claim, not the scheduler, enforces once-a-day, so a double-fire or a manual run on a day that already groomed is a no-op (`already-ran`, zero subprocesses).
That makes the cadence boring to install:

```bash
fno backlog groom --install-agent          # daily LaunchAgent at 2am local (macOS)
fno backlog groom --install-agent --hour 3 # pick another hour
```

`fno update` re-renders the agent onto the freshly-installed binary (`--refresh-agent`, run automatically at the tail of an update alongside the pr-watch refresh), preserving the hour and working directory you installed with.
Without that, an update replaces the binary the plist points at and a migration that breaks the old entry point leaves the agent wedged with no self-heal.
The verb is a no-op when no agent is installed, so it costs nothing if you schedule grooming another way.

Non-macOS gets a cron line instead; the verb itself is scheduler-agnostic:

```cron
0 2 * * * fno backlog groom
```

Two notes on what this replaced.
`scripts/nightly-groom.sh` is a deprecation shim that execs this verb and will be deleted next release.
`~/.fno/groom-digest.md` is retired - nothing writes or reads it, and you can delete it.
The worker re-derives its proposals by running read-only `fno backlog maintain` at pass start, so there is no intermediate file left to go stale.

## Parallel lanes

With `config.parallel.max_lanes >= 2`, the active-backlog daemon dispatches up
to that many ready nodes from distinct domains concurrently, each as an
isolated bg worktree lane. Merges stay serialized (`fno pr merge` takes a
repo-wide lock, and holds a stale-base PR for `fno pr rebase` while lanes run).
This covers immediate merges; a queued `--auto` merge lands asynchronously on
GitHub's side, so pair lanes with branch protection requiring up-to-date
branches if you use `require_checks_pass`.

```bash
fno backlog lane-fill --max 3      # preview which nodes would dispatch as lanes
fno backlog dispatch-lanes         # manually fire one lane-fill round
fno backlog lanes                  # rollup: live lanes vs the cap, per-node status
```

## Worktree isolation policy

Every code payload launched from a repo main checkout is auto-isolated into a
worktree by `fno worktree ensure`. `config.worktree.policy` opts a project out
of that. Values (`never | harness-native | external`):

- `never` - launch in place, no worktree. For a checkout whose working tree IS
  the product (e.g. an Obsidian vault attached live, committing straight to
  main). `ensure` prints the repo root and exits 0 (not a failure, so dispatch
  lanes are never skipped).
- `harness-native` (default) - the harness's own worktree location (claude ->
  `<repo>/.claude/worktrees/<name>`). A harness with no native mechanism
  degrades to `external`.
- `external` - `<config.paths.worktrees_base>/<repo>/<name>` (the maintainer's
  `~/conductor/workspaces` when that knob is set).

Precedence (first match wins): a per-project entry's `worktree` key >
`config.worktree.policy` > the `harness-native` default.

```toml
# global default
[worktree]
policy = "harness-native"

# per-project override (matched by realpath of `path`, else `name`)
[[work.workspaces.default.projects]]
name = "c3po"
path = "~/c3po"
worktree = "never"
```

Fail-closed: a config that exists but fails to parse, or an out-of-enum value
(`conductor` is a `worktrees_base`, not a mode), refuses creation rather than
silently auto-isolating. `fno config doctor` flags an out-of-enum value and a
per-project key mistyped within one edit of `worktree` (the `extra="ignore"`
trap: a misspelled key silently means "default policy"). Read the resolved
verdict without creating anything:

```bash
fno worktree policy --repo <path> [--harness claude]
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
