# Agent PTY substrate: daemon → mux

## Scope

How interactive agent panes are hosted after the mux replaced the daemon as the agent-PTY substrate. Covers what moved, what the daemon still owns, and the accepted crash-isolation cost.

## Principle

The `fno` mux hosts agent PTYs. The `fno-agents` daemon keeps the registry (the single agent index), the inside-leg report store, spawn orchestration (the front half: provider/argv/env/dedup/registry/billing-guard), and the crash-isolated substrates (`bg`, `headless`). The daemon no longer runs a PTY of its own.

## What moved

An interactive agent used to be a daemon-owned PTY worker (`fno-agents-worker`), observed and driven over a WebSocket (`fno agents grid` / `fno agents drive`). That whole surface was deleted:

- **Grid + drive** — the TUI compositor and the WebSocket drive/watch surface. Agent panes now live in the mux; observe them in the sideline and script them with `fno mux pane ls|read|run|send|wait|kill`. `fno mux block pipe --from <pane> --to <pane> [--block last|<seq>] [--json]` composes two of those into cross-pane block piping: read a completed, typed block from the source pane and land its text in the target pane's input (trailing newlines stripped, so it fills the input line and never submits). An open or byte-cap-truncated source block refuses with exit 14 (partial text never pipes). A receive-side idle guard mirrors the block-rerun guard's policy on the target: an agent badged working/blocked refuses (busy), an agent row with no fresh report refuses fail-closed (not provably idle), while done agents, plain shell panes (no registry row), and exited agents receive; session-scoped, since pane ids collide across sessions. `--force` overrides the idle guard only, and a guard refusal exits 15 so scripts can tell it from an error.
- **Daemon PTY hosting** — the `handle_spawn` PTY back-half, the PTY worker (`worker.rs` + `pty.rs`), the `agent.deliver` PTY inject lane, and the injection gate.
- **`host` / `promote` / `grid` / `drive`** — retired verbs. Each prints a one-line pointer to the mux and exits non-zero rather than silently doing nothing.

## Spawn substrates after the migration

`fno agents spawn --substrate <pane|bg|headless>` names where an off-thread agent runs:

- **`pane`** (default) — a mux-hosted PTY pane. The spawn front half is reused; only the hosting call changed. Python owns this back half (`fno.agents.mux_spawn`).
- **`bg`** — a detached `claude --bg` thread. Crash-isolated from the mux (survives a mux server crash). claude-only.
- **`headless`** — a one-shot (`claude -p` / `codex --exec` / `agy -p`).

The daemon's only surviving spawn is the claude stream-json **adoption** lane (`host_mode=interactive` + `mode=stream_json`), which resumes an idle session as a held stream thread for `chat` / switchboard / `ask` to drive. It launches `fno-agents-worker --stream`, which does not open a PTY.

## Accepted cost and the undo

Agent panes die with the mux server (the tmux model): a mux crash takes its panes down with it. This is accepted. `bg` is the crash-isolated alternative when a worker must outlive the server.

The undo, if a real crash ever makes it worth building, is a supervisor that keeps the child PTY file descriptors alive across a mux restart (an fd-keeper / re-parent handoff). It is deliberately **not** built here; file it only when a crash actually bites.

## File-descriptor ceiling on a GUI-launched mux

A mux launched from the macOS GUI (Dock, Spotlight, a login item) inherits the `launchctl limit maxfiles` soft cap of 256 open file descriptors.
A terminal-launched mux does not hit it: zsh raises the soft limit (a shell reports 1048576), so the cap is invisible from a terminal and bites only the GUI-launched process.
With enough live panes, restored squads, and subprocess churn, a GUI mux can exhaust the 256-descriptor table and fail as `attach failed: no spawnable shell: ... Too many open files (os error 24)`.
State that accumulates across restarts lowers the headroom that makes the default livable; a squad store that grew one row per restart was one such accumulator (closed by deriving a squad's durable key from its origin, so one repo holds one row across unbounded restarts).
Raising the ceiling is an operator decision (`launchctl limit maxfiles <soft> <hard>`, or a `SoftResourceLimits` key on a LaunchDaemon), not a code change.

## Badge lattice: the three state producers

The mux pane badge (and every other reader of `inside_leg`) is driven by exactly three Claude Code hook events, each wired in `hooks/hooks.json` to `hooks/inside-leg-report.sh`: `Notification` reports `blocked` (a permission prompt or an idle wait for input — both read as one state, Waiting, with the payload's `message` carried through as the reason), `PreToolUse` and `UserPromptSubmit` report `working` (a tool call or a new turn are each proof the worker is unblocked and running — `PreToolUse` is `blocked`'s natural inverse, since an approved permission prompt fires no `UserPromptSubmit` of its own), and `Stop` reports `done`. A report is a positive marker on a named field (`inside_leg.state`), never an absence, so a reader never has to infer Waiting from silence. The daemon's capability flip clears any stored `screen_state` on a row's first `inside_leg` report, so a hook-reported state can never be shadowed by a stale scrape verdict — this holds for `blocked` exactly as it does for `working`/`done`, by construction rather than a special case.

This lattice is Claude-only. No other harness (codex, agy, opencode) emits a permission-prompt hook event, so a row hosted by one of those keeps the screen-manifest scrape as its sole authority, unchanged by this section.

## Reconcile at startup

Because the migration removes PTY workers, a registry row that still carries a pre-migration worker ref would otherwise look live forever. The daemon settles these at startup: recovery scans for a live worker socket and, finding none, falls back to a PID-liveness probe (`kill(0)` plus a start-time match to defeat PID reuse) and marks the row exited. No stranded agents, no phantoms — the first `list` after a restart reads truthful liveness.
