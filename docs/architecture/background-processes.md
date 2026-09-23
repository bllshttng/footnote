# Background processes: which fno process or arm is which

## Is this page for you?

You see a process in `ps`, a `sh.fno.*` label in `launchctl list`, or an arm row in `fno agents status`. You need to know what it is, who starts it, and how to stop it. The `via=` column of `fno agents status` names the scheduler that owns each row. This page says what each scheduler is.

Not for: which program removed a session and in what order (see [reaping-faq.md](../reaping-faq.md)). And not for: why the stop hook allowed or refused a stop (see [control-plane-loop.md](control-plane-loop.md)).

## Five kinds of thing

A **launchd agent** is macOS's scheduler firing a command on an interval, a calendar time, or at load. It is either a one-shot tick or a `KeepAlive` server. macOS tracks it by label, and killing the tick pid mid-phase never stops the next fire.

A **daemon** is the one lazy-started `fno-agents-daemon` supervisor. Launchd never touches it: a client starts it on first need, and it exits after an idle window.

A **keeper** is a small process that outlives a different parent: the pty keeper outlives the mux server, and the store keeper outlives the daemon. Neither is launchd-managed and neither is a daemon.

An **arm** is a named unit of scheduled work that writes one `control_plane_tick` row per run. It is never a process, and one launchd agent carries several arms. The `pr_watch_merge` confusion comes from exactly this: it is an arm, a phase inside a launchd agent's tick, and not a process at all.

A **hook** runs inside the stopping agent's own process tree, only at Stop. It is not a background process at all.

Beyond the five kinds, two other long-lived fno processes exist: the mux server and the MCP sidecar. Both are listed under Long-running processes below.

## What is pr_watch_merge?

It is the merge phase of the `sh.fno.pr-watcher` tick: `cli/src/fno/pr_watch/cli.py`, function `tick`, calls `_run_phase("merge", _phase_merge, arm="pr_watch_merge")`. Launchd fires the tick every 600 seconds, and the same invocation also drives the other launchd arms in the table below.

Read it with `fno agents status` (the `via=launchd:sh.fno.pr-watcher` column says which scheduler owns the row) and `fno do pr watch status`. See [pr-watch-merge-phase.md](pr-watch-merge-phase.md) for why the merge phase fails and what each failure means.

## Launchd agents

The gated table: every `sh.fno.*` label an installer in this repo writes. Columns: label, installed by, runs, cadence, arms it hosts, health read, safe stop.

| label | installed by | runs | cadence | arms it hosts | health read | safe stop |
|---|---|---|---|---|---|---|
| `sh.fno.pr-watcher` | `fno do pr watch install` (`cli/src/fno/pr_watch/_install.py`, constant `_LABEL`) | `fno-py do pr watch tick` | `StartInterval` 600 s | every arm stamped `launchd:sh.fno.pr-watcher` | `fno do pr watch status`, `fno agents status`, `~/.fno/pr-watcher.out.log` | `fno do pr watch uninstall`; rebind to a new binary with `fno do pr watch refresh` |
| `sh.fno.groom` | `fno backlog groom --install-agent` (`cli/src/fno/backlog/groom.py`, `install_groom_agent`) | `fno backlog groom` | daily at `--hour` (default 2) | none: it writes no `control_plane_tick` row | `launchctl list sh.fno.groom` last exit, `fno doctor`, `~/.fno/groom.out.log` | `launchctl bootout gui/$(id -u)/sh.fno.groom` (no uninstall verb exists) |

Below the table: other `sh.fno.*` labels can appear in `launchctl list` that footnote does not install. Today those are `sh.fno.autocontinue`, `sh.fno.board-server` and `sh.fno.sync-backlog`, an operator's own agents, and `fno agents loops table` lists every label it folds, including them. The `auto_continue` arm's 1800 s heartbeat needs some scheduler that runs `fno backlog advance` with `FNO_CONTROL_PLANE_SCHEDULER` set (`cli/src/fno/control_plane.py`, `scheduler_from_env`). Without one the arm reads `session`.

## Long-running processes

Not gated by the doc-binding test: these are read from `ps`, not from source. Columns: process as `ps` shows it, started by, lifetime and exit, owns, health read, safe stop.

| process as `ps` shows it | started by | lifetime and exit | owns | health read | safe stop |
|---|---|---|---|---|---|
| `fno-agents-daemon --home <dir>` | `ensure_daemon` in `crates/fno-agents/src/client.rs`, on the first client call | exits after 1800 s idle unless an active-backlog project is live; binary resolves from `FNO_AGENTS_DAEMON_BIN` or a sibling of the client | the roster, the registry, and every daemon-scheduled arm | `fno agents status` (`daemon: serving pid=...`) | `fno agents restart` (graceful; `--force` is break-glass) |
| `fno --server <sock>` (the mux server) | `spawn_server` in `crates/fno/src/client.rs`, detached with setsid, on the first `fno` or `fno mux` attach | lives until killed | plain panes and the views over keeper panes | `fno mux pane list` | `fno agents restart --mux` is destructive: it ends plain panes and shells |
| `fno-agents-worker --keeper` (alias `--pane`, the pty keeper) | spawned per pane or thread | lives as long as the hosted child | the child's pty master | `fno mux pane keeper list` | never killed by hand while the child lives; see [pane-keeper.md](pane-keeper.md) |
| `fno-agents-worker --store-keeper --sock <graph>.store.sock` (the store keeper) | spawned by the graph store client when nothing listens on the socket | exits after 600 s idle; `FNO_STORE_KEEPER_IDLE_SECS` overrides, `0` disables | one graph file behind the socket | `lsof <graph>.store.sock` | ends by idle exit, or `fno agents restart` cycles a stale build and the next read respawns it |
| the MCP sidecar (`cli/src/fno/mcp/sidecar.py`) | lazy-started by a session's channel server | idle-exits when no connection, no registered channel and no pending poke outlasts the idle window (default 1800 s) | MCP tool dispatch for its session | see [fno-agents-mcp-channel.md](fno-agents-mcp-channel.md) | ends by idle exit |
| `hooks/target-stop-hook.sh` | the harness, on every Stop | one invocation per stop | the `fno-agents loop-check` decision | `fno agents status`, arm `stop_hook` | nothing to stop: it is not a background process |

## Two keepers, one word

The pty keeper and the store keeper share a binary (`fno-agents-worker`), a frame shape, and one rule: the keeper keeps, the server views. Nothing else. The other two worker lanes are not keepers. `--stream` streams a child's output. `--store-exec` serves one request and exits, so a client leaves no resident process behind.

## Arms

The gated table: one row per arm the readout can show. The scheduler cell is the exact stamp `KNOWN_ARMS` and the tick rows carry (`crates/fno-agents/src/tick_ledger.rs`). Columns: arm, scheduler, hosted by, what it does, default interval.

| arm | scheduler | hosted by | what it does | default interval |
|---|---|---|---|---|
| `king_wake` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | wakes crowned kings on the 900 s beat | 900 s |
| `watchdog` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | the fleet watchdog classifier over lanes | 600 s |
| `pr_watch_sweep` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | scans open-PR backlog nodes and fires `/pr check` | 600 s |
| `pr_watch_merge` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | the merge phase: fires the merge queue for ready PRs | 600 s |
| `active_backlog` | `daemon` | `fno-agents-daemon` | the daemon's mission drain | 300 s |
| `auto_continue` | `session` | backlog advance + the 1800 s heartbeat | reconciles web-merged PRs and dispatches the next node; upstream `pr_watch_merge` | 1800 s |
| `notify_watch` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | the state-change signals arm | 300 s |
| `stop_hook` | `hook:target-stop-hook` | the target stop hook | one row per stop-hook fire | event-driven |
| `reap` | `daemon` | `fno-agents-daemon` | reaps rows, processes, claims and worktrees of merged PRs | 60 s |
| `retire` | `daemon` | `fno-agents-daemon` | retires finished work | 300 s |
| `machine_watch` | `daemon` | `fno-agents-daemon` | watches sustained machine footprint | 300 s |
| `arm_watch` | `daemon` | `fno-agents-daemon` | pages the operator when arms stay broken past the threshold | 300 s |
| `provider_cap` | `daemon` | `fno-agents-daemon` | provider cap accounting | 120 s |
| `merge_close` | `daemon` | `fno-agents-daemon` | merge-close sweeps | 900 s |
| `crown_ledger` | `daemon` | `fno-agents-daemon` | renders reign.html | 300 s |
| `fleet_page` | `daemon` | `fno-agents-daemon` | renders fleet.html | 1800 s |
| `attention` | `daemon` | `fno-agents-daemon` | selects attention rows into the status payload | 30 s |
| `heal` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | the PR auto-heal drive, gated on `auto_heal.enabled` | 600 s |
| `evals` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | the eval bank's demand leg | per `evals.schedule_days` |
| `stranded` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | the stranded-session sweep | one tick in three |
| `recovery` | `launchd:sh.fno.pr-watcher` | the pr-watch tick | the recovery sweep | one tick in three |

Under the table, four notes. First, `evals`, `stranded` and `recovery` have no `KNOWN_ARMS` row, so the readout shows them only after they tick and never reads them `UNOBSERVED`. Second, `stranded`, `recovery` and `watchdog` run one tick in three on staggered slots of the 600 s tick. Their effective cadence is one run per 1800 s. Third, the daemon runs work that writes no arm row: the worktree sweep, the merge reaper, orphan sweeps, the terminal-stop sweep, and liveness. [reaping-faq.md](../reaping-faq.md) holds that list. Fourth, [loops.md](../loops.md) is generated from this same arm list and carries each loop's arming key and live state. When a row reads red, run `fno agents loops table`.

## Stopping things safely

A launchd agent is stopped by its uninstall verb or `launchctl bootout`, never by killing the tick pid mid-phase. The next fire is already scheduled. The daemon is restarted, never killed, because workers survive a graceful restart. A keeper is never killed by hand while its child lives. A pty keeper kill ends the session. A store keeper ends by idle exit or `fno agents restart`.
