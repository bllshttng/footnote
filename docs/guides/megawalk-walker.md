# Megawalk Walker (`fno megawalk`)

> **Updated:** the Python walker is deleted. Walking is now
> handled by `fno-agents loop run --driver megawalk`. This guide covers the new launch-and-watch
> front door. See [docs/architecture/unified-loop.md](../architecture/unified-loop.md) for
> the runtime design.

## When to use this

You have multiple features queued in `~/.fno/graph.json` (intake via
`fno backlog intake <plan>` or `/blueprint`-then-adopt) and want them shipped autonomously.

## Two ways to invoke

### From Claude Code (interactive)

```
/megawalk
```

The skill launches `fno-agents loop run --driver megawalk` in the background and tails
`.fno/events.jsonl` to show progress.

### From a shell (direct)

```bash
fno-agents loop run --driver megawalk \
  --driver-lib-dir /path/to/footnote/scripts/lib \
  --cwd "$(pwd)"

# Once mode (stop after 1 node):
fno-agents loop run --driver megawalk --max-units 1 ...

# Allow PR auto-merge:
fno-agents loop run --driver megawalk --allow-merge ...

# Cross-project walk:
fno-agents loop run --driver megawalk --all ...

# Scoped to a specific project:
fno-agents loop run --driver megawalk --project myproject ...
```

## Watch the walk

```bash
# Live TUI (in a second terminal):
fno megawalk watch

# Or tail events manually:
tail -f .fno/events.jsonl | jq .
```

Events of interest:

| Event kind | Meaning |
|---|---|
| `loop_unit_dispatched` | Walker started a node |
| `node_closed{close:closed}` | Node shipped successfully |
| `node_closed{close:parked}` | Node parked (claim held; see park docs) |
| `walk_paused{policy:...}` | Walk paused by policy (consecutive_failures or p0_failed) |
| `loop_terminated{reason:NoWork}` | Backlog drained - walk complete |
| `loop_terminated{reason:Budget}` | Iteration ceiling reached |

## Cancel

```bash
touch .fno/.target-cancelled   # loop checks this sentinel between iterations
# or Ctrl-C a foreground run
```

## Parked nodes

A parked node holds its `node:<id>` claim and appears as `node_closed{close:parked}`.

```bash
fno claim list --prefix node:                    # see held claims
fno claim force-release node:<id> --reason "..." # release for re-dispatch
```

## Selection scope and precedence

Selection is **project-scoped by default**: the walker only considers
nodes belonging to the current project (derived from cwd, matching `fno backlog next`).
Pass `--all` to include all projects.

Within scope, candidates are ordered **epics-first, then flat priority**:
a child of a ready epic ranks above a loose node of the same priority. This drains epics
before loose work.

## State commands

| Command | What it does |
|---|---|
| `fno megawalk watch` | Live Rich TUI at ~1Hz; reads `.fno/events.jsonl`. Ctrl-C to exit. |
| `fno backlog next` | Show what the walker would pick next. |
| `fno backlog status` | Overall backlog health. |
| `fno claim list --prefix node:` | Active node claims (in-flight or parked). |

The old `fno megawalk status / pause / resume / bootstrap / reset` subcommands were
removed with the Python walker. Use `fno backlog` + `fno claim` instead.
