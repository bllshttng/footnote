# fno agents grid: multi-panel compositor

`fno agents grid <name...>` is the multi-pane sibling of [`fno agents drive`](fno-agents-drive.md). Where `drive` steps into one PTY-managed agent full-screen, `grid` tiles N agents side by side as live watcher panes and lets the operator promote any focused pane to a full driver session. It is the "single pane of glass" for a fleet of agents: see who is working and who is waiting at a glance, drop into any one when it needs input, pop back out.

This doc covers the client-side composition model, the render substrate, the two FSMs, the run loop, and most importantly the **watch-model boundary**: exactly which agents the grid can and cannot see, and why. If you have hit "agent X not found" or "worker is not reachable" while an agent is clearly running, the boundary section is the part to read.

## Client-side composition

The grid composes entirely **client-side** (Locked Decision #2): the client opens one watcher WebSocket per agent (`agent.drive` with `mode: "watch"`, the existing surface from [fno-agents-drive.md](fno-agents-drive.md)) and tiles the streams locally. The daemon is untouched. A daemon-side composite is a tracked follow-up justified only by thin-client / remote / headless needs.

The compositor holds **at most one driver claim** at any time: the global mode is single-valued (WATCH or DRIVE), so take-over is serialized through the single focused pane. There is no N-way driver contention to coordinate.

**Interactive claude tiles since E2** (`x-34e2`, superseding the original Locked Decision #6 exclusion). The E1 keystone gave the daemon a `ClaudeInteractiveProvider` that PTY-hosts subscription-billed interactive claude via the generic worker path, so it tiles and drives exactly like a codex pane. Only that lane is admitted: `filter_pty_agents` requires `host_mode == "interactive"`, and `resolve_agent_names` additionally drops any claude row with no live PTY worker socket (the adopted `claude -p` stream-json lane, which also carries `host_mode == "interactive"` to dodge reconcile but cannot be tiled/driven). The headless `claude --bg`/stream lane stays out of the compositor.

## Render substrate

Each pane parses its PTY byte stream with `alacritty_terminal` (headless: `Term<VoidListener>` + `vte::ansi::Processor`), the same emulator the readiness seam ([`crate::screen`](../../crates/fno-agents/src/screen.rs)) uses. An earlier revision used `vt100` on a "dependency weight" rationale that turned out to be wrong for `alacritty_terminal` 0.26: its transitive tree is `vte` (the same VT parser `vt100` wraps) plus `parking_lot` / `polling` / `rustix-openpty` / `regex-automata` and a cfg-gated `windows-sys` (FFI, compiles to nothing off Windows). No winit, no GUI stack. The richer cell model (`Flags`, `NamedColor` / `Rgb`, `Dimensions`) is what per-cell compositing needs, so the whole crate standardized on it. See [`crates/fno-agents/src/grid/pane.rs`](../../crates/fno-agents/src/grid/pane.rs).

## Two state machines

Both live in [`crates/fno-agents/src/grid/state.rs`](../../crates/fno-agents/src/grid/state.rs), pure and unit-tested, so the run loop drives them without re-deriving any rules.

- **Per-pane connection** (`ConnState`): `connecting -> watching -> {driving, busy_elsewhere, disconnected, exited}` plus a `promote_pending` mid-RPC state. `connecting` is distinct from a blank `watching` so a never-arriving stream is visibly "connecting", not silently hung. `can_route_input()` is true only in `Driving`, which is the load-bearing per-pane gate that drops a stray keystroke during the claim-acquire window.
- **Global mode + focus** (`Compositor`): `Mode = WATCH | DRIVE` + a focused index. In WATCH, Tab / arrows cycle focus across drivable panes and no keystroke reaches any agent; Enter promotes the focused pane; `q` quits. In DRIVE, keystrokes forward to the focused pane's driver socket; Esc releases. `observe_pane_states()` snaps the mode back to WATCH when the focused pane leaves `Driving` (claim denied, WS drop, agent exit, or aborted promote), so the global mode can never strand in DRIVE while keystrokes vanish.

## Run loop

[`crates/fno-agents/src/grid/run.rs`](../../crates/fno-agents/src/grid/run.rs) is a `tokio::select!` loop modeled on agentworkforce/relay's `swarm_tui::run_tui`:

- one watcher WS per agent, each drained by a spawned reader task that forwards PTY bytes over an mpsc channel tagged with the pane index (Closed / Exited on WS-close / `child_exited`);
- `crossterm::event::EventStream` for async key input;
- a 30fps render tick that paints **only when state actually changed** (a `dirty` flag set by key / pane-bytes / resize / pane-state transitions). An idle grid never repaints, so it never fills the PTY output buffer. This matters because the client runs on a `current_thread` tokio runtime, where a synchronous blocking `stderr` write (a full buffer, or Ctrl-S flow control) would freeze the executor and starve input; not repainting when idle keeps that window small.

Terminal setup goes through a `TerminalGuard` RAII that uses `crossterm::terminal::enable_raw_mode` (not a hand-rolled libc `cfmakeraw`): crossterm's `EventStream` depends on that call to initialize its internal event subsystem, and bypassing it can panic with "reader source not set" on first poll. The guard restores raw mode + alternate screen + cursor on every exit path including panic unwind.

Take-over (Enter) opens a **second** `mode: "interactive"` connection to the focused agent; the watcher keeps feeding the render while the driver connection carries input. Esc / quit send a detach and close the driver sink, releasing the claim on every exit path.

## Rail-grouped navigation (Phase 1)

The `--rail` flag adds an opt-in left navigation rail that groups the panes by a switchable key and shows one focused agent at full width in the main area. It is additive over the existing grid - no painter or PTY-plumbing rewrite - and the railless tiled grid stays the default, so existing usage is unchanged. `t` toggles the rail on/off live.

- **Grouping** ([`grid/group.rs`](../../crates/fno-agents/src/grid/group.rs), pure + unit-tested). `group_by(rows, key) -> Vec<Group>` partitions the same registry rows `filter_pty_agents` already reads (zero new I/O). `GroupKey = Cwd | Session | Provider | Status`, cycled by `g`. Groups sort by header, members by name; a missing/null field buckets to `"unknown"` rather than panicking. `GroupKey::Session` is the one subtle field: Python-authored rows (the common case) expose a unified `session_id` only as a non-serialized computed `@property`, so it reads the real provider-specific fields (`codex_session_id` / `gemini_session_id` / `cc_session_id` / `claude_short_id`, then `short_id`) - reading the bare `session_id` key would collapse every Python row into one `"unknown"` group.
- **Rail layout** ([`grid/layout.rs`](../../crates/fno-agents/src/grid/layout.rs)). `compute_with_rail(tty, RAIL_COLS, main_pane_count) -> RailLayout { rail, main, footer }` reserves `RAIL_COLS` on the left; the main area is the existing `compute` engine on the narrowed width. Invariant `rail_cols + main_cols == tty.cols` (no gap, no overlap). When the terminal is too narrow for the rail plus a minimum pane, `compute_with_rail` returns `TerminalTooSmall` and `paint` **degrades to the railless tiled grid** (it does not blank to an error); the rail returns when the terminal widens.
- **Focus axis** ([`grid/group.rs`](../../crates/fno-agents/src/grid/group.rs) `RailState`). The one genuinely new concept: `FocusAxis = RailNav | PaneDrive`. In RailNav, Up/Down move the selection and `g` re-groups; in PaneDrive, keystrokes flow to the focused PTY and the rail keymap is suspended. `RailState` tracks the selection as an **agent index** (not a slot), so the selection follows the same agent across a `g` re-partition; if that agent is gone it clamps to the nearest valid row.
- **Reuse, not a parallel path.** The rail does **not** introduce a second drive mechanism. On `d` (either mode) or Enter (in Single) it calls `Compositor::set_focus(idx)` to align `comp.focus` to the rail selection, then routes the promote through the same `handle_action` path the railless Enter uses (which opens the interactive socket and inserts the `driver_sink`); keystroke forwarding in PaneDrive then flows through the compositor's existing DRIVE path. The rail axis flips to PaneDrive only if the claim actually lands, and reverts to RailNav (with a cue) when the driven agent exits or its socket drops - mirroring the compositor's own `observe_pane_states` snap-back, so a keystroke is never stranded against a dead PTY. Enter in **GroupTile** instead *drills* one level deeper: a guarded `KeyCode::Enter` arm (placed before the Enter/`d` drive arm) toggles to Single focused on the selected tile rather than promoting, re-anchoring onto a live survivor first when the selected member has already exited.
- **Render** ([`grid/run.rs`](../../crates/fno-agents/src/grid/run.rs) `raster_rail`). The footer always names the active mode + focus axis + group-by key, plus a ` · filter: attention` token when the `a` attention filter is on and a fleet-wide attention roll-up (`!N` waiting, `xM` exited, e.g. `!2 x1`, omitted when nothing needs attention) that sums the per-group badges. Group headers carry a member count `(N)` plus an attention badge computed from the **live** per-pane signals (`Pane::is_waiting` + `ConnState::Exited`), NOT the frozen registry status string - which never carries needs-input/exited for a running agent. The `a` filter (`RailState::toggle_attention_filter`) narrows the rail to waiting-only agents and re-anchors the selection (mirroring the `g` regroup discipline); it persists across a `g` re-partition. A mode flip / `g` re-partition / filter toggle takes the **full-paint** path (the caller clears `prev_frame`) because the region map changed, which would otherwise tear the diff painter (same discipline as resize). Rail truncation is char-boundary + wide-cell aware, ported from the existing painter.

### GroupTile (Phase 2)

Phase 2 adds the group-tile render and the `Tab` Single ↔ GroupTile toggle (`RailState.main_mode = Single | GroupTile`). The main-area renderer is the same `compute` engine parameterized on pane count, so the toggle reduces to "how many panes the main area shows."

- **Paged layout.** [`compute_with_rail_page(tty, RAIL_COLS, pane_count, page) -> RailPageLayout`](../../crates/fno-agents/src/grid/layout.rs) is to [`compute_page`](../../crates/fno-agents/src/grid/layout.rs) what `compute_with_rail` is to `compute`: it reserves the rail and runs the page-aware engine on the narrowed main area, offsetting tiles by `RAIL_COLS`. A group too large to tile at minimum pane size paginates **inside its own area** (footer `group <name> page p/k`), reusing `PageLayout` and therefore inheriting its clamp-on-shrink; this is independent of the railless grid's top-level pagination (Locked Decision 4), which is inactive in rail mode.
- **The page follows the selection.** There is no stored page index. The rendered page is *derived* from the selected member's position - [`RailState::selected_group_page(group, capacity) = selected_position / capacity`](../../crates/fno-agents/src/grid/group.rs), mirroring `PageLayout`'s focus-anchored `current_page`. So the selected (accented) tile, and whatever `d` drives (or Enter drills into), is mathematically always on the rendered page - a keystroke can never drive an off-screen agent. Up/Down move the selection one member (the page follows); `]`/`[` (`page_jump`) move it by a whole page. `build_frame_rail_group` tiles the page's members, accenting only the selected one; `apply_group_tile_resize` sizes those panes to their tiles on every GroupTile state change (Tab-in, selection move, `g`, page jump), versus `resize_rail_focus`'s full-width sizing in Single mode.
- **Live exit reflow; spawn deferred.** The rail watches the fixed pane set chosen at launch. When a member **exits** mid-session, GroupTile re-tiles to the survivors (AC3-FR): `group::live_members` filters exited panes out of the live tile layout, and selection / paging / drill step over them (including on the async exit path, which re-sizes the remaining tiles), so the group's visible membership shrinks to who is still alive rather than holding a dead `exited` tile. A brand-new agent that **spawns** mid-session does not yet join the rail without a relaunch - live spawn reflow needs a dynamic PTY-watcher lifecycle (registry poll plus a watcher socket per new name), a larger change than the exit path, and is deferred. The derived-page clamp is the render-time safety covering both the reflow and that future spawn path.

## Squads (manual cross-repo teams, x-5b3e)

Where a sideline is a *derived* team (every session is force-grouped under its repo), a **squad** is a *manual* team: the user **recruits** already-spawned agents into a named, cross-repo / cross-provider group. This is the differentiator vs `claude agents` (cwd-only, claude-only, structurally cannot form a cross-repo team). Membership is a **reference (playlist), never a move** - a recruited agent still appears under its repo sideline AND in every squad that recruited it.

- **Store** ([`grid/squads.rs`](../../crates/fno-agents/src/grid/squads.rs), pure + flock-protected). A squad is `{name, members: [agent_name], created_at}` persisted **globally** at `~/.fno/squads.json` (a squad spans repos, so it cannot live in any one project's `.fno`). Membership keys on the agent **`name`**, not a session uuid: the registry has no uuid, and a name is *more* respawn-stable (a respawn reusing the same name auto-rejoins its squads, where a uuid is re-minted every spawn). `update()` mirrors the registry's read-modify-write under an exclusive `.lock` sidecar + atomic tempfile/rename (`state.rs`), so two grid instances never interleave a recruit (last writer re-reads first). `load()` never panics: a corrupt store degrades to empty + a one-line warning, so a malformed `squads.json` can never stop the grid from starting.
- **A squad is a group-by *view*.** `GroupKey::Squad` joins the `g` cycle (`... status -> squad -> union -> cwd`; also `--group-by squad`). [`base_groups`](../../crates/fno-agents/src/grid/run.rs) is the single chokepoint both the nav path (`rail_view_groups`) and the paint path (`rail_groups_and_badges`) route through: for `Squad` it yields [`squads::squad_groups`](../../crates/fno-agents/src/grid/squads.rs) (each squad's member *names* resolved to live row indices) instead of `group::group_by`. So squads render, navigate, and tile through the **existing** rail machinery unchanged - and the sideline views (`Cwd`/`Provider`/...) are untouched. Reference membership falls out for free: the same agent shows under `g`→`Cwd` (its repo sideline) and under `g`→`Squad` (its squad). The store is loaded once into the run loop (reloaded on recruit) so the per-frame paint never reads the file. **Both at once (`GroupKey::Union`, x-fef5).** Rendering sidelines *and* squads simultaneously in one rail is `g`→`union` (also `--group-by union`): `base_groups` concatenates the `Cwd` sidelines with the squad groups, namespacing the two halves' `key_value`s (`cwd:` / `squad:`) so the occurrence cursor (x-8a6a, which replaced the old index-based selection precisely to disambiguate a member appearing in two visible groups) selects each occurrence independently. `group_by` short-circuits `Union` to empty, mirroring `Squad`.
- **Recruit** is plain **`m`** on the focused pane in RailNav: it opens a modal squad-name prompt (reuses the `Launcher` single-line buffer) seeded with the selected agent's name, captured at keypress so a later selection move can't retarget it. Submit = create-if-absent + recruit + persist + reload, with an outcome toast (`recruited` / `already-in` / `failed`); a re-recruit is a visible no-op (dedup on name). The recruit binds to plain `m` because the leader-key model (x-b563) is **tiled-only**; when the rail leader (x-d97d) lands, `m` rebinds to `leader m` - the [`SquadStore::recruit`](../../crates/fno-agents/src/grid/squads.rs) verb both call into is unchanged.
- **Churn / offline.** A recruited member whose session is no longer live (gone from the rows) is **kept** in `squads.json` as a tombstone (composition stays stable across churn) and never tiles - `squad_groups` resolves only live names to indices, so an offline member simply has no tile. It is surfaced, not silently dropped: the squad header reads `*stack +1 off` (offline count baked into the header by `squad_groups`; the rail then appends the live `(count)`). Removal is explicit (`remove_member`).

## Pagination (over-capacity fleets)

v1 rendered only the panes that fit at minimum tile size and showed a `+k more` overflow label. Pagination replaces that label with real navigation: discrete **pages** of the tile grid, plus an attention flag so an off-screen agent waiting for input is never invisible.

- **A page** is one full `rows x cols` tile-set the layout manager computes for the current terminal. Capacity `C = rows * cols`; `page_count = ceil(pane_count / C)`; the visible slice is `panes[current_page*C .. current_page*C + C]`. When `pane_count <= C`, `page_count == 1` and no pagination chrome renders - exactly v1. All page math is single-sourced through [`layout::page_count_for`](../../crates/fno-agents/src/grid/layout.rs) and [`layout::compute_page`](../../crates/fno-agents/src/grid/layout.rs); the `Compositor` owns `current_page` and a `recompute_pagination()` that is the **only** place `current_page` is clamped and re-anchored (Domain Pitfall: do not scatter clamps). Every page in the multi-page case reuses one uniform `C`-tile geometry, so a partial last page renders at normal tile size and a flip needs no resize.
- **Navigation.** `[` / `]` (aliased PgUp / PgDn) flip pages in WATCH, clamped at the bounds (no wrap); they relocate focus to the new page's first pane so the focused index always points into the visible slice. Tab / arrows move focus and **auto-advance** the page when focus crosses the boundary (focus-follow); focus cycling wraps to page 0, which is the one place a wrap happens. Paging is **WATCH-only**: in DRIVE every key (including page keys) forwards to the agent, so a page key can never drop a driver claim or cross a page mid-drive.
- **Eager connections.** Unlike v1, which connected only the visible fit-subset, pagination keeps **every** agent's watcher WS open for the whole session (pane indices are stable globals; off-screen panes keep draining their `alacritty_terminal::Term` so flips are warm and the attention scanner has live state). A lazy "connect-visible-only" policy structurally cannot detect an off-screen waiting agent, which would defeat the feature. The daemon is untouched (grid Locked Decision 2 holds).
- **Off-screen attention.** Each render tick, every live pane's `Term` snapshot is run through the same prompt-glyph readiness check the daemon uses ([`readiness::screen_is_waiting`](../../crates/fno-agents/src/readiness.rs), reused client-side) to produce per-off-screen-page waiting counts. The footer paints `Page n/P` plus a badge per off-screen page holding waiting agents (`▸p2●1`). The scan is gated on connection state (`Watching` / `Driving` only) so an `exited` / `disconnected` pane's frozen last frame cannot false-positive on a trailing glyph.
- **Soft cap.** `DEFAULT_MAX_PANES = 32` (overridable via `FNO_GRID_MAX_PANES`) bounds the eager connection count. Above it the grid renders the first 32 and warns explicitly (warn-and-truncate; the grid still works) rather than opening unbounded connections.

Backpressure note: the plan describes off-screen reads as "drop-to-latest per-pane," but the actual run loop (inherited from grid v1) drains all panes through a **single shared bounded** `mpsc(256)` with blocking sends. It is bounded (no unbounded growth), and the soft cap is the real relief valve for a fleet of chatty agents; a per-pane drop-to-latest ring is a deferred optimization (Domain Pitfall: "measure before optimizing").

## The watch-model boundary (read this when a pane says "not found")

The grid attaches to **daemon-managed PTY workers** over the drive socket. It can only see agents that exist *as a PTY worker the daemon owns*. This is narrower than "any agent that is doing something", and the gap is where the confusing failures come from.

There are two distinct codex/gemini execution substrates in fno-agents, and they do not overlap:

| | `fno agents spawn` (and the Rust daemon path) | `fno agents ask` (Python) |
|---|---|---|
| runs codex as | a **daemon-managed PTY worker** running `codex exec --json` in a PTY the worker owns | a **detached Python subprocess** running `codex exec --json`, tee'd to a JSONL file (`dispatch.py`) |
| registry row | yes, with a live worker the daemon can reach | yes, but **no PTY worker** behind it |
| grid can watch it | **yes**, while the worker's PTY is alive | **no** - there is no daemon PTY to attach to |
| resume | reattach to the worker PTY via `agent.drive` | `codex resume <rollout-uuid>` directly, outside fno |

So an `fno agents ask <name>` run is invisible to the grid even though codex is actively working: the daemon has a registry row for `<name>` but no driveable PTY worker, so `agent.drive` returns "not found". The grid is not broken; it is reporting the truth that there is no PTY to watch.

The other two failure labels mean exactly what they say:

- **"agent X not found"**: no registry row named X (often a name-vs-short_id mix-up: `fno agents spawn codex-bot` registers the *name* `codex-bot`; `codexbot` is its short_id, and the grid resolves by name), or an `ask`-driven subprocess with no PTY worker.
- **"agent X worker is not reachable"**: a registry row exists but its worker PTY is dead (a stale row from a prior session, or an agent whose `codex exec` already exited).

### Why codex agents are short-lived

`codex exec` is codex's one-shot mode: it runs the message to completion and exits. A daemon worker spawned with no task (`fno agents spawn x --provider codex`) runs `codex exec ""`, has nothing to do, and exits immediately. A worker spawned *with* a task stays alive only while it processes that task, then exits. So a codex agent is grid-watchable only during the window it is actively working, and the natural "resume" handle is the rollout UUID (`codex resume <uuid>`), not a persistent fno PTY. The grid's PTY-attach model fits interactive sessions (an idle prompt waiting for input) better than it fits a one-shot `codex exec`.

## How to actually watch a working agent

Use `spawn` (daemon PTY worker), not `ask` (detached subprocess), and give it a task so it stays alive while it runs:

```bash
# Tab 1 - daemon-managed PTY worker, grid-watchable, with a task:
fno agents spawn pr-review --provider codex "explore the repo and write a detailed review"

# Tab 2 - watch it live while it works:
fno agents grid pr-review
```

The pane shows codex's raw `--json` stream live (not pretty, but live), then flips to `exited` when the run completes. `fno agents grid --all` tiles every live PTY-managed agent (codex / gemini, plus interactive PTY-hosted claude since E2; the adopted stream-json claude lane is filtered out by the worker-socket gate).

Note `fno agents grid` (the Python wrapper, space form) only forwards verbs in `rust_runtime.py`'s `AUTO_ROUTE_VERBS`; if your installed `fno` predates the `grid` entry, call the Rust binary directly as `fno-agents grid ...` (hyphen).

## Open design tension

Codex `exec` streams JSONL to a file by nature, so the *natural* way to watch an `ask`-driven run is to **tail that JSONL tee**, not attach a PTY. Three options if `ask`-driven runs should become watchable, none yet chosen:

1. **Grid tails the JSONL tee** for non-PTY agents (a log pane alongside PTY panes).
2. **Route `ask` through a daemon PTY worker** so it is uniform with `spawn` (touches the "ask stays on Python" decision).
3. **Keep the substrates separate** and document the split (this doc): `spawn` for watchable, `ask` for fire-and-collect.

## Related fixes surfaced by grid testing

- **Registry status drift**: both strict registry readers (Python `registry.py`, Rust `client_verbs.rs`) only accepted `{live, orphaned}`, but the daemon writes `exited` on worker exit (retained until rm). Once any agent exited, every registry read hard-errored, bricking all Python `fno agents` commands until the row was rm'd. Both readers now accept the full `AgentStatus` vocabulary. See [cross-language-schema-parity.md](cross-language-schema-parity.md).
- **Flow-control freeze** (p3, residual): a synchronous frame / teardown write can still block the `current_thread` executor if the terminal is paused mid-stream (Ctrl-S). The robust fix is moving frame writes off the executor (a dedicated render thread). The event-driven render above makes the common case safe; this edge is out of scope for the initial landing.

## Tests

- Pure FSM + render + layout: `cargo test -p fno-agents grid::`
- Library-level integration (FSM + Pane + Layout end to end): `cargo test -p fno-agents --test grid_e2e`
- The interactive run loop is exercised live (it needs a daemon + agents); a daemon-driven e2e harness is a tracked follow-up.
