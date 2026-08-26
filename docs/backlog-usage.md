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
| **Column** (Now / Next / Later / Triage / Done) | `_kanban_column` over lifecycle overlays plus live-epic effective priority |
| **Swimlane** (the per-project cluster inside a column) | `project` |
| **Position within a lane** | `rank` (curated), else the shared epic-aware work-order suffix |

`_kanban_column` is the sole column authority, `make_kanban_column(entries)` binds its whole-graph overlays, and `rank` never changes a card's column.

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

`fno backlog intake` is the one creation verb that reads a plan file. The plan's file table is load-bearing downstream: parallel lane fill collision-checks dispatches against it. Intake therefore refuses a plan whose `## Files to Modify` parses empty (exit 2) unless you pass `--allow-no-surface`. Such a node cannot be collision-checked and dispatches fail-open. A multi-path intake refuses the whole batch before any write. The plan-less creation verbs above are unaffected.

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
fno backlog update <id> --no-public            # exclude from public projections
fno backlog update <id> --source-node x-aaaa   # origin edge (see "Node-to-node edges")
fno backlog update <id> --related x-bbbb       # affinity edge, symmetric
```

`<id>` resolves by canonical id (`ab-1a2b3c4d`), title-derived slug
(`dashless-spawn`), or bare hex (`1a2b3c4d`).

When a PR opens outside the Footnote PR path, repair its node with `fno backlog update <id> --locked-by <worker> --pr-number <n>`. This command binds the owner and primary PR together. `--add-pr` records only an additional PR and can leave a ready node offered for dispatch. A bare `--pr-number` removes the node from ready but leaves its owner unknown. The update receipt rereads the stored row and reports its owner, PR, and status.

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
| Replace with a newer node | `fno backlog supersede <new> --replaces <old> --cause "..." --surface <path>` | old stays `blocked` until a merged PR touches every `--surface`, then `superseded` |
| Mark complete | `fno backlog done <id>` | closes only on a MERGED PR; sets `completed_at`, unblocks dependents |
| Reopen it | `fno backlog reopen <id> --reason "..."` | clears `completed_at`; refuses when a referenced PR is MERGED |
| Remove permanently | `fno backlog remove <id>` | hard delete (use for dupes / dead nodes) |
| Sweep old terminal nodes | `fno backlog archive --apply` | moves them to `graph-archive.json`, still readable; runs daily via `fno backlog groom` |
| Bring one back | `fno backlog unarchive <id>` | returns it to the working graph |
| Browse what shipped | `fno backlog album` | pages the archive newest-first; `--project`, `--json` |
| Fix a working/archive id collision | `fno backlog archive-dedupe-ids --apply` | reminds the archived side; old id resolves via `previous_id` |

Blockers: `--blocked-by`, `--add-blocker`, `--remove-blocker` on `update`.
A node with an open blocker derives to `status: blocked` automatically.

**Every transition here has a correction, and that is enforced.**
`cli/tests/unit/test_lifecycle_pairs.py` fails on a verb that changes a node's state or existence and ships without an inverse.
It fails just as hard on an inverse that nothing a caller reads ever names.
That second half is the one that bit us.
`fno backlog remove` existed and worked for a long time while nothing mentioned it.
An agent therefore ruled that no delete verb existed, and another project kept 23 nodes it believed un-file-able.

Reopening is guarded rather than free.
When a referenced PR is MERGED, `reopen` refuses.
That is `done`'s gate inverted: the work is in main, and clearing the completion makes the graph assert that shipped work did not ship.
The usual remedy is to file what remains as its own node.
`--force --reason` records a deliberate reopen of shipped work.
Ancestor epics the cascade auto-closed come back with the child.
An epic closed on its own evidence is left alone and named, because reopening it discards a judgment the verb never made.

## Node-to-node edges

Four edges connect nodes, and only the first two gate anything.

| Edge | Set with | Meaning | Gates? |
|------|----------|---------|--------|
| `blocked_by` | `--blocked-by` / `--add-blocker` / `--remove-blocker` | this cannot start until that lands | yes: derives `_status: blocked` |
| `parent` | `--parent` | this was decomposed into that epic | yes: rollup, epic depth |
| `source_node_id` | `--source-node`, or captured ambiently | this came *out of* working on that | no |
| `related` | `--related` | affinity: two sides of the same coin, or work that co-delivers | no |

`source_node_id` and `related` are deliberately different questions.
Origin is where the decision to think about this was made; affinity is asserted by whoever notices it, in either direction, and neither implies the other.

**Origin capture is ambient first.** Filing a node from a session that already knows its node stamps the origin with no flag, through three branches in precedence order: an explicit `--source-node`, then an owned `.fno/target-state.md`, then the `FNO_NODE` a spawn exported. Pass `--source-node <id>` only when none of those apply, or to override them.

The two paths fail in opposite directions on purpose. Ambient capture that resolves nothing files the node anyway with a null origin, because provenance must never block a filing. An explicit `--source-node` that does not resolve **refuses the command** and writes nothing, because silently dropping an assertion would leave a node looking organically filed.

```bash
fno backlog idea "follow-up" --source-node x-aaaa    # or a slug, or bare hex
fno backlog update <id> --source-node null           # clear it
```

**`related` is symmetric and stored on both endpoints.** Declaring it on one node writes the inverse on each peer, in the same locked write, so the two sides cannot disagree. It replaces the list rather than appending, and a dropped peer loses its inverse edge too.

```bash
fno backlog update x-aaaa --related x-bbbb,x-cccc   # sets both directions
fno backlog idea "co-delivered work" --related x-bbbb
fno backlog update x-aaaa --related null            # clears both sides
```

Not to be confused with `fno backlog relatedness`, which builds a *computed* similarity sidecar from token overlap and shared domain/epic. That sidecar is regenerable and rebuilt from scratch on every `relatedness build`; `related` is an assertion and lives on the node so it survives.

### Reading the edges back

```bash
fno backlog provenance <id>              # origin (with its title), related, birth + spawn sessions
fno backlog provenance <id> --spawned    # invert the origin edge: what did this node produce?
```

`--spawned` walks transitively with traversal-derived depth. A cycle truncates the walk and says so, keeping the descendants it already found.

`fno backlog epic status <epic>` reports **scope growth**: follow-ups the epic accumulated after decomposition (reachable by `source_node_id`, not already children by `parent`). The figure is withheld when origin-capture coverage across the epic's window sits below 50%, since at low capture a small number is indistinguishable from a missed one. Coverage counts only origins that still resolve to a live node, because an origin naming a deleted node joins nothing and would otherwise inflate coverage while contributing no growth; any such danglers are reported separately. Realized node and PR counts print either way, so a withheld figure explains itself.

**done = merged.** `fno backlog done` closes a node only when a referenced PR is
MERGED. An OPEN PR (even with green CI) exits 5 (awaiting merge): the node stays
`in_review` and closes on the actual merge via `reconcile` / merge-triggered
`advance`. A session finishes at PR-up + CI-green + reviewed (it never waits on a
human merge); only the graph close waits for the merge, so the "done" state means
"landed on main" uniformly, whoever closes it. Exit codes: 0 closed, 3 refusal
(CLOSED-unmerged / no evidence), 4 gh outage (retryable), 5 awaiting merge.

`fno backlog update` cannot close a node. It once carried a `--completed` flag that applied completion with no gh evidence check and emitted no event. That silence is what let a ledger append close nodes hours before their PRs merged, so the flag is removed. The only closers are `fno backlog done`, its deprecated one-release `done` spelling, and `reconcile`. `backlog done` is merge-gated with `--force --reason TEXT` for the un-cross-checked case. The deprecated spelling is merge-gated with no bypass. Both surfaces resolve merge evidence through one shared helper and share the exit codes: 0 closed, 3 refusal, 4 gh outage (retryable), 5 awaiting merge.

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

## Board at a glance

```bash
fno backlog board                  # three sections, one line each: just finished, in progress, on deck
fno backlog board --project fno    # scope to one project (default: the current repo's)
fno backlog board --json           # same three sections, machine-readable
```

Reads only the graph and the on-disk pr-status cache under the state root. It never makes a live GitHub call, so this verb can never exhaust the GraphQL quota.

Each section caps at five rows and shows `... and N more` past that. Each row caps at twelve words after the node id, so the whole board fits on one screen.

When a source cannot be read, the section renders an explicit `(unknown: ...)`. It never renders as an empty section: an empty "Just finished" means nothing landed, not that the read failed.

The "In progress" blocking fact comes from the cache's newest row for that PR. That row can be stale by up to the cache TTL. When the PR stays quiet, the row can be older still. The line prints the age beside the verdict, so nobody reads it as the current state.

## Health and hygiene

```bash
fno backlog triage health          # idea pile, stale ready, collisions, dupes
fno backlog maintain --apply       # recurring sweep: re-scope, prune, pr_url backfill, auto-defer
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

After an update, `fno doctor update` re-renders the agent onto the fresh binary through `--refresh-agent`. This automatic tail step preserves the installed hour and working directory.
Without that, an update replaces the binary the plist points at and a migration that breaks the old entry point leaves the agent wedged with no self-heal.
The verb is a no-op when no agent is installed, so it costs nothing if you schedule grooming another way.

Non-macOS gets a cron line instead; the verb itself is scheduler-agnostic:

```cron
0 2 * * * fno backlog groom
```

This replaces the former `scripts/nightly-groom.sh` compatibility shim.
`~/.fno/groom-digest.md` is retired - nothing writes or reads it, and you can delete it.
The worker re-derives its proposals by running read-only `fno backlog maintain` at pass start, so there is no intermediate file left to go stale.

## Parallel lanes

With `config.parallel.max_lanes >= 2`, the active-backlog daemon dispatches up to that many ready nodes concurrently, each as an isolated bg worktree lane. When file surfaces are disjoint, same-domain nodes co-schedule: the dispatch is collision-gated. Merges stay serialized (`fno do pr merge` takes a repo-wide lock, and holds a stale-base PR for `fno do pr rebase` while lanes run). This covers immediate merges. A queued `--auto` merge lands asynchronously on GitHub's side. If you use `require_checks_pass`, pair lanes with branch protection requiring up-to-date branches.

```bash
fno backlog lane-fill --max 3      # preview which nodes would dispatch as lanes
fno backlog dispatch-lanes         # manually fire one lane-fill round
fno backlog lanes                  # rollup: live lanes vs the cap, per-node status
```

## Worktree isolation policy

Every code payload launched from a repo main checkout is auto-isolated into a worktree by `fno agents workspace worktree ensure`. `config.worktree.policy` opts a project out of that. Values (`never | harness-native | external`):

- `never` - launch in place, no worktree. For a checkout whose working tree IS the product (e.g. an Obsidian vault attached live, committing straight to main). `ensure` prints the repo root and exits 0 (not a failure, so dispatch lanes are never skipped).
- `harness-native` (default) - the harness's own worktree lifecycle. Claude -> `<repo>/.claude/worktrees/<name>`. Codex Desktop -> same-thread `/worktree` or **Hand off -> Worktree** under `$CODEX_HOME/worktrees`. A substrate with no native transition degrades to the Footnote-owned `<state_dir>/worktrees` fallback (normally `~/.fno/worktrees`). It never inherits an external allocator from `paths.worktrees_base`.
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
fno agents workspace worktree policy --repo <path> [--harness claude]
```

## Public roadmap

A curated view for advertising an OSS project's roadmap. Qualifying nodes are public by default. Use `--no-public` for an explicit exclusion. One fail-closed title gate protects both public projections.

```bash
fno backlog update <id> --no-public
fno backlog roadmap --project fno --out ROADMAP.md \
  --html roadmap.html --backlog-html backlog.html
```

The roadmap emits only title / priority / size grouped Now / Next / Later / Shipped (Triage folds into Later). The public backlog groups open `idea`, `ready`, `in_progress`, and `blocked` work by subsystem. Neither projection emits IDs, details, plan paths, cwd, blockers, PR links, or sessions.

Before any public file is replaced, the shared gate scans every emitted title for PR references, graph IDs, home paths, and session IDs. One offender makes the command exit nonzero, prints the complete cleanup queue on private stderr, and leaves every requested output unchanged.
