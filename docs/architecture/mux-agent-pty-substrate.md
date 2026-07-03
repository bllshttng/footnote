# Agent PTY substrate: daemon → mux

## Scope

How interactive agent panes are hosted after the mux replaced the daemon as the agent-PTY substrate. Covers what moved, what the daemon still owns, and the accepted crash-isolation cost.

## Principle

The `fno` mux hosts agent PTYs. The `fno-agents` daemon keeps the registry (the single agent index), the inside-leg report store, spawn orchestration (the front half: provider/argv/env/dedup/registry/billing-guard), and the crash-isolated substrates (`bg`, `headless`). The daemon no longer runs a PTY of its own.

## What moved

An interactive agent used to be a daemon-owned PTY worker (`fno-agents-worker`), observed and driven over a WebSocket (`fno agents grid` / `fno agents drive`). That whole surface was deleted:

- **Grid + drive** — the TUI compositor and the WebSocket drive/watch surface. Agent panes now live in the mux; observe them in the sideline and script them with `fno mux pane ls|read|run|send|wait|kill`. `fno mux block pipe --from <pane> --to <pane> [--block last|<seq>]` composes two of those into cross-pane block piping: read a completed, typed block from the source pane and land its text in the target pane's input (trailing newlines stripped, so it fills the input line and never submits). An open or byte-cap-truncated source block refuses with exit 14 (partial text never pipes). A receive-side idle guard mirrors the block-rerun guard's policy on the target: an agent badged working/blocked refuses (busy), an agent row with no fresh report refuses fail-closed (not provably idle), while done agents, plain shell panes (no registry row), and exited agents receive; session-scoped, since pane ids collide across sessions. `--force` overrides the idle guard only, and a guard refusal exits 15 so scripts can tell it from an error.
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

## Reconcile at startup

Because the migration removes PTY workers, a registry row that still carries a pre-migration worker ref would otherwise look live forever. The daemon settles these at startup: recovery scans for a live worker socket and, finding none, falls back to a PID-liveness probe (`kill(0)` plus a start-time match to defeat PID reuse) and marks the row exited. No stranded agents, no phantoms — the first `list` after a restart reads truthful liveness.
