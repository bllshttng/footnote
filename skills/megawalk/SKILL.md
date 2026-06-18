---
name: megawalk
description: "Continuous backlog delivery. /megawalk launches the Rust loop walker (fno-agents loop run --driver megawalk) and watches progress until the walk terminates or you Ctrl-C. Supports roadmap generation, adopt, retro, defer, and status flows."
argument-hint: "[ab-xxxxxxxx | roadmap <vision.md> | adopt <paths...> | status | defer ID | cancel | retro | once | auto-merge | auto-continue | no-merge | parallel | combo <name>]"
metadata:
  internal: true
requires:
  binaries:
    - "fno >= 0.1"
    - "fno-agents >= 0.1"
    - "gh >= 2.0"
    - "git >= 2.30"
---

# Megawalk

**From vision to shipped product - now driven by the Rust loop runtime.**

Megawalk is target's loop. It picks tasks from the backlog and executes them via
`fno-agents loop run --driver megawalk`. The Python walker machinery was deleted in
control-plane step 5 task 2.4 (ab-7303e5d7). The new architecture is launch-and-watch:
you start the Rust binary in the background, then tail its events.

<HARD-GATE>
NEVER edit ~/.fno/graph.json directly via Edit/Write tools or `jq -i`/`sed -i`.
ALWAYS use `fno backlog` commands or call `locked_mutate_graph()` from Python.
Direct edits are blocked by the PreToolUse hook AND detected via hash sidecar.
</HARD-GATE>

<HARD-GATE>
Megawalk NEVER writes code, creates implementation files, or does task work directly.
Megawalk NEVER spawns Agent tools for task execution.
Tasks execute by launching `fno-agents loop run --driver megawalk` as a background
process. The /target skill handles individual nodes dispatched by the walker.
If you find yourself writing code, stop.
</HARD-GATE>

## Usage

### Bare /megawalk - launch and watch

```bash
/megawalk                   # picks ready nodes from current project, loops until done
/megawalk --all             # include all projects (default is current project only)
/megawalk once              # stop after one node (--max-units 1)
/megawalk auto-merge        # pass --allow-merge to the walker
/megawalk auto-continue     # arm merge-triggered auto-continue for this project's chain
/megawalk parallel          # accepted; logged; execution is sequential (group 2 default)
```

Single node by ID - invoke /target directly:

```bash
/megawalk ab-ff6f96e0       # delegates to: /target ab-ff6f96e0
```

Epic-scoped walks and roadmap / adopt / retro / status flows remain unchanged:

```bash
/megawalk roadmap vision.md # generate roadmap then start walk
/megawalk adopt plans/foo.md
/megawalk status
/megawalk defer 5
/megawalk cancel
/megawalk retro
```

### Preflight

Before launching the walker:

1. Run `fno doctor` and print the verdict. If the result is `stale` with missing verbs,
   emit a warning and refuse to start an unattended walk. Instruct the user to run
   `fno update` first.
2. Resolve the `fno-agents` binary using this priority order:
   a. `$FNO_AGENTS_BIN` env var
   b. `$REPO_ROOT/crates/fno-agents/target/release/fno-agents`
   c. `$REPO_ROOT/crates/fno-agents/target/debug/fno-agents`
   d. `fno-agents` on PATH
   If none found, fail with a message naming the four search paths.

### Launch the walk

Use Bash with `run_in_background: true` (a walk runs for hours; a foreground Bash call
would hit the 10-minute tool timeout).

**Flag mapping from /megawalk argument-hint to `fno-agents loop run --driver megawalk`:**

| /megawalk modifier | fno-agents flag |
|---|---|
| `once` | `--max-units 1` |
| `parallel` | `--parallel-cap 4` (note: group 2 serializes; cap is logged) |
| `auto-merge` | `--allow-merge` |
| `no-merge` | (default; workers get no-merge prompts via dispatcher) |
| `--all` | `--all` |
| `--project NAME` | `--project NAME` |

Always pass:
- `--driver-lib-dir "${CLAUDE_PLUGIN_ROOT}/scripts/lib"` (the dir holding `driver-claude-code.sh`, the bash driver lib sourced by the loop; this is **not** where `dispatch-node.sh` lives - that script is the `bg`-dispatch primitive at `skills/target/scripts/dispatch-node.sh` and is unrelated to `--driver-lib-dir`)
- `--cwd "$(pwd)"`

Example launch:

```bash
fno-agents loop run \
  --driver megawalk \
  --driver-lib-dir "${CLAUDE_PLUGIN_ROOT}/scripts/lib" \
  --cwd "$(pwd)" \
  [--max-units N] [--allow-merge] [--all] [--project NAME]
```

### Watch progress

After launching in the background, poll `.fno/events.jsonl` (tail it) and surface
progress lines per unit:

- `loop_unit_dispatched` event: "Dispatching unit: {unit_id}"
- `node_closed{close:closed}`: "Closed (done): {unit_id}"
- `node_closed{close:parked}`: "Parked (see below): {unit_id}"
- `walk_paused{policy:...}`: "Walk paused ({policy}): {detail}"
- `loop_terminated{reason:...}`: "Walk finished: {reason}"

For a live TUI in a second terminal: `fno megawalk watch`

Stop watching when `loop_terminated` or a significant pause appears. Let the user decide
whether to re-run.

### Explicit flags (ac-compat)

Explicitly document these modifiers (no silent drops):

- `auto-continue` - arms **merge-triggered auto-continue** (ab-3cd195b6) for this
  project's chain. Unlike `auto-merge`, this is NOT a flag to the Rust walker:
  the walker correctly dies on `NoWork` once a no-merge PR ships, and the next
  group is dispatched later by the merge event (`fno backlog reconcile` /
  `/pr merged` -> `fno backlog advance`), not by keeping the walk alive. So the
  arm must PERSIST across the merge->next-session boundary - write a per-project
  campaign marker, then enter the loop normally:

  ```bash
  mkdir -p .fno && touch .fno/.auto-continue-armed
  echo "auto-continue: armed for this project - after each merge, the next now-unblocked node auto-dispatches (disarm: rm .fno/.auto-continue-armed)"
  ```

  Echo the arming so it is acknowledged (AC2-UI). The marker is one of the
  inputs `auto_continue_enabled()` reads (env override > marker > settings); a
  persistent opt-in can instead set `config.auto_continue.enabled: true` in
  `.fno/settings.yaml`. After arming, dispatch the bare loop as usual.
- `council` - not supported by the Rust walker; rejected with: "council is not supported
  by the Rust loop walker. Use /megawalk roadmap council <vision.md> for the think-tank
  phase, then bare /megawalk."
- `combo <name>` - not supported in group 2; rejected with: "combo routing arrives with
  group 3 (megatron integration). For single nodes: /target combo <name> ab-<id>."

## When a node is parked

Parking is now claim-held exclusion: the walker holds the `node:<id>` claim and emits
`node_closed{close:parked}` followed by `walk_paused` if a policy limit is hit.

**To see parked nodes:**
```bash
fno backlog status         # shows deferred/parked nodes
fno claim list --prefix node:   # shows held claims
```

**To release a parked node so the walker picks it up again:**
```bash
fno claim force-release node:<id> --reason "investigation complete"
# then re-run /megawalk (bare)
```

The `fno backlog undefer <id>` flow still applies if the node was explicitly deferred.

## Status command

Read backlog + tail events.jsonl:

```bash
fno backlog next           # what would the walker pick next?
fno backlog status         # overall progress
tail -n 20 .fno/events.jsonl | jq .  # last 20 events
```

## Cancel

```bash
touch .fno/.target-cancelled   # the loop's cancel sentinel
# or Ctrl-C a foreground run
```

## Defer

```bash
/megawalk defer 5          # same as before: fno backlog defer <id> --reason "..."
```

## Roadmap generation

See [references/roadmap-generation.md](references/roadmap-generation.md).

## Adopt

See [references/adopt-protocol.md](references/adopt-protocol.md).

## Retro

See [references/milestone-retro.md](references/milestone-retro.md).

## Auto-merge

See [references/auto-merge.md](references/auto-merge.md).

## References

- [references/argument-parsing.md](references/argument-parsing.md) - graph-ID resolution + removed-form redirects
- [references/roadmap-generation.md](references/roadmap-generation.md) - vision to backlog
- [references/adopt-protocol.md](references/adopt-protocol.md) - multi-path adopt
- [references/size-routing.md](references/size-routing.md) - task-to-size mapping
- [references/milestone-retro.md](references/milestone-retro.md) - think-tank retrospectives
- [references/auto-merge.md](references/auto-merge.md) - cross-skill auto-merge
- [references/megawalk-migration.md](references/megawalk-migration.md) - removed-form migration history
- [references/inbox-handlers.md](references/inbox-handlers.md) - cross-project inbox drain
