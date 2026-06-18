# How to: watch a fleet with `fno agents grid`

`fno agents grid` tiles multiple PTY-managed agents side by side as live panes, with the focused pane promotable to a full driver session. This guide is the practical usage + troubleshooting reference. For the internals and the design rationale, see [docs/architecture/fno-agents-grid.md](../architecture/fno-agents-grid.md).

## Launch

```bash
fno agents grid <name1> <name2> ...   # tile specific agents
fno agents grid --all                 # tile every live PTY-managed agent (codex / gemini)
```

Inside the grid:

| Key | WATCH mode | DRIVE mode |
|---|---|---|
| Tab / arrows | move focus (heavy border tracks it); auto-flips the page when focus crosses a page boundary | forwarded to the agent |
| `[` / `]` (or PgUp / PgDn) | previous / next page (clamped at the ends, no wrap) | forwarded to the agent |
| Enter | take over the focused pane (DRIVE) | forwarded to the agent |
| Esc | (inert) | release back to WATCH |
| `q` | quit | forwarded to the agent |
| Ctrl-C | quit | quit (operator escape hatch) |

Every pane is read-only by default. Enter on a focused, drivable pane opens a take-over: your keystrokes reach that agent's PTY until you press Esc.

## Grouping panes into a navigation rail (`--rail`)

When you are watching agents across several repos or sessions, a wall of equal tiles is hard to scan. The `--rail` flag adds a left navigation rail that groups the panes by a switchable key and shows one focused agent at full width in the main area:

```bash
fno agents grid --all --rail                      # rail grouped by cwd (default)
fno agents grid --all --rail --group-by session   # start grouped by session
```

The rail is opt-in. Without `--rail` the grid is exactly the tiled grid above; nothing changes for existing usage. Inside the grid, `t` toggles the rail on and off live.

| Key | RailNav (the rail owns the keyboard) | PaneDrive (driving the focused agent) |
|---|---|---|
| Up / Down | move the selection; the selected agent fills the main area | forwarded to the agent |
| Enter | in **GroupTile**: drill into the selected tile (drop to Single, focused on it); in **Single**: drive the selected agent (if drivable) | forwarded to the agent |
| `d` | drive the selected agent (if drivable), from either Single or GroupTile | forwarded to the agent |
| `a` | toggle the **attention filter**: show only agents waiting for input (idle + exited hidden) | forwarded to the agent |
| Esc | (inert) | release the driver claim, back to RailNav |
| `g` | cycle the group-by key: cwd → session → provider → status | forwarded to the agent |
| `Tab` | toggle the main area between **Single** (one focused pane) and **GroupTile** (the selected agent's whole group tiled side by side) | forwarded to the agent |
| `]` / `[` | in GroupTile, page through a group too large to tile at once (`PageDn` / `PageUp` also work) | forwarded to the agent |
| `t` | toggle the rail off (back to the tiled grid) | forwarded to the agent |
| `q` | quit | forwarded to the agent |
| Ctrl-C | quit | quit |

The footer always names the active mode, focus axis, and grouping, e.g. `WATCH · single | group-by: cwd  ·  ↑↓ select · Enter/d drive · Tab tile · g regroup · a attn · t railless · q quit`. In GroupTile it reads `WATCH · tile | group-by: cwd  ·  group <name> page 1/k  ·  …`. Each group header shows its member count `(N)` and an attention badge distinct from the count: `!k` for agents waiting on input, `xk` for agents that have exited. The footer also rolls those badges up across **all** groups into a global attention summary - `!2 x1` means two agents fleet-wide are waiting and one has exited; it is omitted when nothing needs attention. With the attention filter on, the footer adds a ` · filter: attention` token after the group-by key so you always know the rail is showing a waiting-only subset rather than the whole fleet. When you press `g`, the selection follows the same agent into its new group rather than jumping to a slot.

**Single vs GroupTile, and drilling in.** Single mode (the default) fills the main area with one focused agent. Press `Tab` to switch to GroupTile, which tiles every member of the *selected agent's* group side by side, so you can watch a whole project's fleet at once. The page you see always holds the selected agent, so the accented tile, and whatever `d` will drive, is never off screen: moving the selection with Up/Down or jumping a page with `]`/`[` keeps the driven pane visible. From GroupTile, `Enter` *drills* into the selected tile - it drops to Single focused on that agent (rather than driving it), so the progression reads Enter-deeper: GroupTile → Single → DRIVE, with `Tab`/`Esc` backing you out. `d` drives the selection directly from either mode. (If the selected tile is on a member that has already exited, drilling re-anchors onto a live survivor first.) A group too large to tile at minimum pane size paginates inside its own area (footer `group <name> page p/k`), independent of the railless grid's paging. A single-member group fills the area as one pane. `Tab` again returns to Single.

**Attention filter (`a`).** When you only care about who needs you, press `a` to filter the rail down to agents waiting for input; idle and exited agents are hidden and the selection re-anchors onto a still-visible agent (or surfaces an empty-state hint when nothing is waiting). The `· filter: attention` footer token and the global `!N` summary stay visible so the filtered view never reads as a missing fleet. Press `a` again to show everyone. The filter persists across a `g` regroup.

If the focused agent exits while you are driving it, the grid drops you back to RailNav with an "exited - released drive" note instead of stranding your keystrokes against a dead pane. A drive attempt on an exec (one-shot, watch-only) agent or one that has exited is refused with a one-line hint, not a silent no-op.

On a terminal too narrow for the rail plus a legible pane, the grid degrades to the railless tiled grid rather than blanking; the rail returns automatically when you widen the window.

> **Scope.** `--rail` covers grouping, navigation, single-pane focus + drive (Phase 1), and the GroupTile side-by-side view (Phase 2, the `Tab` toggle above). The rail watches the fixed pane set chosen at launch. When a member **exits** mid-session, GroupTile now re-tiles to the survivors (the exited pane drops out of the live tile layout, and paging/drill skip it). A brand-new agent that **spawns** mid-session does not yet join the rail without a relaunch - live spawn reflow needs a dynamic PTY-watcher lifecycle and is deferred.

## Paging a fleet that doesn't fit

When you give the grid more agents than fit on screen at a legible tile size, it splits them into **pages**. The footer shows `Page n/P`; press `]` (or PgDn) to advance and `[` (or PgUp) to go back. Page keys work in WATCH only - in DRIVE they forward to the agent, so you press Esc to release before paging. If you'd rather navigate by focus, Tab/arrows move the highlighted pane and the page follows automatically when focus crosses to the next page.

Off-screen agents are not out of sight: each tick the grid scans **every** pane (visible or not) for a waiting prompt and flags pages that need you in the footer, e.g. `▸p2●1` means one agent on page 2 is waiting for input. Flip to that page and the badge clears. Agents that have exited or disconnected never raise the flag.

Very large fleets are soft-capped at 32 panes (set `FNO_GRID_MAX_PANES` to raise it). Above the cap the grid tiles the first 32 and prints a `32/40 shown` note in the footer rather than opening an unbounded number of connections.

## Getting an agent the grid can actually watch

The grid watches **daemon-managed PTY workers**. The way you start an agent decides whether the grid can see it:

- **`fno agents spawn <name> --provider codex "<task>"`** creates a daemon PTY worker. The grid **can** watch it, while it works.
- **`fno agents ask <name> ...`** runs codex as a detached subprocess writing JSONL to a file. The grid **cannot** watch it (there is no PTY worker behind it), even though codex is actively running.

So to see live output in a pane:

```bash
# Tab 1: spawn a daemon PTY worker WITH a task (so it stays alive while working):
fno agents spawn pr-review --provider codex "explore this repo and write a detailed review"

# Tab 2: watch it live:
fno agents grid pr-review
```

The pane streams codex's raw `--json` output while the task runs, then shows `exited` when it finishes. A bare `fno agents spawn x --provider codex` with no task runs `codex exec ""`, has nothing to do, and exits immediately, so always give a spawned codex agent a task if you want to watch it.

## Reading the pane labels

| Label | Meaning |
|---|---|
| `connecting…` | watcher socket open, no bytes yet (distinct from blank `watching` so a hung stream is visible) |
| `watching` | live, read-only |
| `driving` | you hold the driver claim; keystrokes forward here |
| `tailing (busy: driven by <holder>)` | someone else holds the driver; you stay read-only |
| `disconnected - … (r to retry)` | WS dropped or never connected; `r` retries |
| `exited (code N)` | the agent's process exited; last frame frozen |

## Troubleshooting

**"agent X not found"** - no registry row named X. Two common causes:
- Name vs short_id: `fno agents spawn codex-bot` registers the *name* `codex-bot`; `codexbot` is its short_id. The grid resolves by **name**, so use `fno agents grid codex-bot`. Check names with `fno agents list`.
- You started it with `fno agents ask`, which has no PTY worker. Use `spawn` instead (see above).

**"agent X worker is not reachable; cannot drive"** - a registry row exists but its worker PTY is dead. Either a stale row from a prior session, or a `codex exec` that already exited. Clear stale rows:

```bash
fno agents rm <name> --force
fno-agents reconcile          # sync registry status with reality
```

**`fno agents grid` says "No such command 'grid'"** - your installed Python `fno` predates the `grid` route. Call the Rust binary directly, or reinstall:

```bash
fno-agents grid --all                                    # the Rust binary always has grid
# or refresh the Python wrapper:
uv tool install --reinstall footnote                    # from canonical, after the PR merges
```

**Every command fails with "registry read failed: row N has status=exited"** - an older `fno` whose registry reader predates the status-vocabulary fix. Reinstall both binaries:

```bash
cargo install --path <footnote-checkout>/crates/fno-agents --bins
uv tool install --reinstall footnote
```

**Panes render but a keypress does nothing / the grid seems frozen** - if your terminal is paused (you hit Ctrl-S), output flow-control blocks the render write and stalls input; press Ctrl-Q to resume.

## Why codex agents are short-lived

`codex exec` is one-shot: it runs the task and exits. So a codex agent is watchable only during the window it is actively working, and resumes only via its rollout UUID (`codex resume <uuid>`), not as a persistent fno session. The grid's watch model fits interactive agents (an idle prompt waiting for input) more naturally than a one-shot exec; see the architecture doc's "watch-model boundary" for the full picture.
