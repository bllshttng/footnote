# The pane keeper: worker panes that outlive the mux server

A worker pane's pty master does not live in the mux server. It lives in a keeper process: `fno-agents-worker --pane`, spawned per pane by the server. When the server dies, the keeper and its child keep running. The next server on the same session re-adopts the same child instead of spawning a replacement. Plain panes are unchanged. The server holds their master directly, and they die with it, exactly as they always have. Worker panes spawned by the resume path are the one exception. `resume_worker_into` hosts them inline, so they die with the server like plain panes. Giving them a keeper means reusing the canonical mesh spawn wrapper, which lives in the Python launcher.

## Ownership rule

**The keeper keeps, the server views.** The same rule that governs threads applies to panes: a viewer never owns the processes it displays. Concretely:

- The keeper `setsid()`s into its own session, opens the pty pair, and spawns the provider argv on the slave. It owns the master for the pane's whole life.
- The server holds one unix socket to the keeper. Attach, detach, restart, or SIGKILL of the server never touches the child's controlling terminal. No SIGHUP reaches the child.
- Admission counts and `pane ls` report the CHILD's pid, never the keeper's. The Identify reply names it. A fleet count and any later kill aim at the process the user sees.

## Why a separate process

**The tier split is not ours.** Claude Code ships the same shape: a daemon that holds the roster and hosts no ptys, plus one PTY host process per background session. While background sessions run, look for `claude bg-pty-host` processes, one per session. fno's keeper is that middle tier. The roster lives in one place. Every live pty lives in its own small process beside it.

**`attach` is a vendor feature, not a capability grade.** Only claude and codex declare an `interactive_attach` resume form in `cli/src/fno/agents/harness_capabilities.toml`. They are the two vendors that built a session multiplexer into their binary. pi, agy and opencode declare `unsupported`. That is not weakness. One binary means one terminal means one session. A row reading `unsupported` records the absence of a vendor multiplexer, and the keeper is fno being that multiplexer for everyone else.

**Why not the daemon.** One process holding N ptys is one failure domain for N sessions. A pty master fd belongs to the process that opened it, so a restarted daemon cannot recover the dead one's masters without `SCM_RIGHTS` fd passing. With a keeper the new server just reconnects to a socket, which is what the re-adoption scan does today.

**It is not a cache argument.** The provider prompt cache is server side, keyed on the prompt-prefix bytes, with a TTL. An idle keeper makes no requests, so its cache expires on the same clock as a dead session's. Holding a process changes no prompt bytes. Keeping a cache warm means resuming the conversation and sending traffic on a schedule, which is what the cache-keepalive skill does. The keeper buys a live session, not a warm cache.

**The cost, honestly.** One pid per live session. A keeper is a pty pair, a socket and a bounded ring. The daemon is the roster, the registry and the HTTP surface. A keeper therefore costs far less memory than the daemon it outlives. Process count binds long before memory does, and [the reaper contract](#the-reaper-contract) is what keeps that count honest.

**What the keeper does not replace.** `fno mux workspace restore` rebuilds cold sessions from registry metadata for any harness with a resume form, at zero idle cost. The keeper is for tabbing between LIVE sessions and for surviving a mid-turn kill. Different failures. Both are wanted.

## Protocol

Frames are `u8 tag | u32 LE length | payload`. The shape is mirrored between `crates/fno-agents/src/pane_keeper.rs` and `crates/fno/src/pty.rs`. Client to keeper: `Input`, `Resize`, `Kill`, `Identify`. Keeper to client: `IdentifyReply(json)`, `Output`, `Exited(i32)`. A protocol version rides the IdentifyReply. A newer keeper meeting an older client refuses loudly instead of decoding garbage.

The **subscriber** is the one client whose frames drive the child and whose connection receives Output. It is the mux server, at spawn or adoption. Later connections are answered Identify on their own connection. They can never drive or steal the stream. Example: `fno mux pane keeper list` probes. When the subscriber dies the keeper keeps running. The next connection to arrive takes the seat. That connection is the re-adopting server.

The keeper retains recent output in a bounded ring (default 1 MiB, `--ring-bytes`). It replays the ring at Identify and names any dropped bytes. That replay is the detached window a re-adopting server can restore.

## Re-adoption

At startup, before serving, the server scans `<state-root>/mux/panes/*.sock`. See [state-root-inventory](../state-root-inventory.md) for the owner + lifetime row.

- A socket whose keeper answers Identify is adopted. The handshake drains the ring and learns the child pid, argv, cwd, and size. The pane is registered under a fresh pane id, its identity rebuilt from argv.
- Worker-name and session-target joins rebind the adopted pane to its squad member. A restore then focuses it instead of spawning a second one.
- A socket with no live listener is unlinked and named in the server log. That is the stale-socket contract.
- `fno mux pane keeper list` also reports leftover sockets whose child is gone (`stale: true`). It answers with the server dead, so a done-probe can grep `keeper_pid` from its JSON.

Re-adoption is not respawn. The proof below pins the SAME child pid across the server's death.

## The reaper contract

`kill_all_panes` (the server shutdown sweep) skips `PtyShell::Keeper` panes. Plain panes die with their server. Keeper-hosted panes survive to re-adoption. A hangup must never become a close. The deliberate path is unchanged. `reap_pane` (explicit pane close) still sends Kill. The keeper SIGKILLs its own child, unlinks its socket, and exits. Surviving a hangup never becomes surviving a close.

## Two traps the implementation had to learn

**portable-pty's `take_writer` is take-once and sends EOT on drop.** The keeper takes the writer once at startup and reuses it for every Input frame. A write-and-drop per frame ships a literal Ctrl-D after the first keystroke. The writer's Drop sends EOT, which ends the child's stdin. A second `take_writer` refuses outright. The local-pane path never meets this because it also takes the writer once in `wire()`.

**A handshake quiet window can cut a frame in half.** The adoption handshake drains the ring replay until 150ms of silence. A frame split by that boundary must seed the reader thread's buffer. Otherwise the tail arrives unanchored and the byte stream desyncs. A payload byte read as a tag decodes as garbage. Observed form: a phantom `Exited` that reaped a live pane. `keeper_handshake` therefore returns its partial-frame leftover. `spawn_keeper_reader` starts from it.

## The proof

`tests/mux-keeper-survives-server-kill.sh` builds both binaries and drives a real server. It asserts survival by named pids, never by exit codes or survivor counts. A passing run reads:

```
[before] worker pane 825 child=57345 keeper=57342; plain pane 826 child=57458
[after kill] worker child 57345 is ALIVE
[after kill] worker child 57345 is still parented by its keeper 57342
[after kill] keeper 57342 is ALIVE and holds the master
[after kill] keeper 57342 reparented to init/launchd (ppid=1)
[after kill] plain pane child 57458 is dead, as it always was
[re-adopt] fresh server bound the SAME child pid 57345 as pane 825 - a re-adoption, not a respawn
[answer] the surviving pane answered through the re-adopted server: GOT:proof-line-274a
PASS: worker child 57345 outlived the killed server fk-57210, was re-adopted by a fresh server, and answered a prompt; the plain pane died with it
```

The server is killed with SIGKILL so no graceful path can spare the child. The answer step reads the whole visible grid, not `--lines`. `read_tail` reads the bottom N display rows. A fresh adopted VT holds this pane's short answer at the top rows with an empty history.
