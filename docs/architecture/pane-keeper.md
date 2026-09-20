# The pane keeper: every pane outlives the mux server

A pane's pty master does not live in the mux server. It lives in a keeper process: `fno-agents-worker --pane`, spawned per pane by the server. When the server dies, the keeper and its child keep running. The next server on the same session re-adopts the same child instead of spawning a replacement. Every pane takes this road now: worker panes, resumed panes, portal viewers, ad-hoc `pane run`, and plain shells. A plain shell carries its shell integration into the keeper as an env prefix in its argv. When the keeper cannot start, the inline pty survives as the named fallback. The pane still opens, and the server marks it `unkept`. A later `kill-server` refuses while it is live unless `--end-unkept` is passed.

## Ownership rule

**The keeper keeps, the server views.** The same rule that governs threads applies to panes: a viewer never owns the processes it displays. Concretely:

- The keeper `setsid()`s into its own session, opens the pty pair, and spawns the provider argv on the slave. It owns the master for the pane's whole life.
- The server holds one unix socket to the keeper. Attach, detach, restart, or SIGKILL of the server never touches the child's controlling terminal. No SIGHUP reaches the child.
- Admission counts and `pane ls` report the CHILD's pid, never the keeper's. The Identify reply names it. A fleet count and any later kill aim at the process the user sees.

## Why a separate process

**The tier split is not ours.** Claude Code ships the same shape. A daemon holds the roster and hosts no ptys, plus one PTY host process per background session. While background sessions run, look for `claude bg-pty-host` processes, one per session. fno's keeper is that middle tier. The roster lives in one place. Every live pty lives in its own small process beside it.

**`attach` is a vendor feature, not a capability grade.** Only claude and codex declare an `interactive_attach` resume form in `cli/src/fno/agents/harness_capabilities.toml`. They are the two vendors that built a session multiplexer into their binary. The other four rows (pi, agy, opencode, and the retired gemini) declare `unsupported`. That is not weakness. One binary means one terminal means one session. A row reading `unsupported` records the absence of a vendor multiplexer, and the keeper is fno being that multiplexer for everyone else.

**A bare pty is not enough to drive a TUI.** Measured 2026-09-01 on pi. A python openpty plus Popen left termios at ICANON with ECHO on, so pi never entered raw mode and never answered. Baseline, TERM, 24x80, setsid, ctty and tcsetpgrp changed nothing. The same pi answered over tmux, which submits with bracketed paste plus a separate 0x0d. Hosting the pty is one problem. Getting keystrokes into the TUI is a second one, and the vendors that ship a multiplexer already solved it.

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

`kill_all_panes` (the server shutdown sweep) skips `PtyShell::Keeper` panes. Every pane is keeper-hosted now, so a server death keeps them all to re-adoption. A hangup must never become a close. The deliberate paths are unchanged. `reap_pane` (explicit pane close) still sends Kill. The keeper SIGKILLs its own child, unlinks its socket, and exits. And `fno mux kill-server` measures before it signals. A live pane with no live keeper at its id is unkept. The kill refuses while one is live, and `--end-unkept` is the deliberate override. Surviving a hangup never becomes surviving a close.

## Two traps the implementation had to learn

**portable-pty's `take_writer` is take-once and sends EOT on drop.** The keeper takes the writer once at startup and reuses it for every Input frame. A write-and-drop per frame ships a literal Ctrl-D after the first keystroke. The writer's Drop sends EOT, which ends the child's stdin. A second `take_writer` refuses outright. The local-pane path never meets this because it also takes the writer once in `wire()`.

**A handshake quiet window can cut a frame in half.** The adoption handshake drains the ring replay until 150ms of silence. A frame split by that boundary must seed the reader thread's buffer. Otherwise the tail arrives unanchored and the byte stream desyncs. A payload byte read as a tag decodes as garbage. Observed form: a phantom `Exited` that reaped a live pane. `keeper_handshake` therefore returns its partial-frame leftover. `spawn_keeper_reader` starts from it.

## The proof

`tests/mux-restart-survival-matrix.sh` is the whole-contract proof. It drives a plain shell, an ad-hoc pane, a keeper worker, a portal viewer, and the daemon legs in one private root. Each row crosses three restarts: the server alone, the daemon alone, and both SIGKILLed together. It opens with a dead-canary positive control for its liveness reader. It then asserts survival by named pids and a canary counter that must keep advancing across the gap. It fails on any row whose child pid changes or whose counter stops. `tests/mux-keeper-survives-server-kill.sh` remains the single-pane identity proof: the SAME child pid, re-adopted, still answering a prompt, with the plain pane surviving by the same road.

## The hard limit

A pane keeper cannot be refreshed on demand. It holds a live child process and its pty master. Surviving a restart is the keeper's whole purpose, so cycling it can only destroy the thing it exists to keep. Until then the running-process census reports the keeper stale and kept, and no restart surface promises otherwise. The census row names the split in three words: `stale`, `kept`, `current only when its pane ends`.

## A hand-off moves the socket, not the process

A pane keeper becomes a THREAD keeper by moving its socket. `fno mux pane kill --hand-off-to <path>` renames the socket from `mux/panes/` to `mux/threads/`. It then drops the pane from the layout and the persisted squad, and closes the server's connection without sending a Kill frame. A Kill makes the keeper kill its child and exit. A bare hangup is what the keeper is built to survive, so the child keeps running and keeps its pid. The daemon's keeper sweep then finds it at the new path and rebinds the row.

The rename is safe because a renamed unix socket path still reaches the same listener, and the old path stops answering. That was measured on macOS 25.3 with a positive control on the old path, not assumed from the man page.

`fno mux pane keeper list` walks both lanes. Each row carries a `lane` field of `pane` or `thread`. The listing read only `mux/panes/` at first. `tests/convert-pane-to-thread-journey.sh` caught that gap. A conversion moved its own keeper out of the one directory the listing read. So the verb said the keeper was gone while it was running.

An INLINE pane has no keeper. The server itself holds the master, so releasing that entry kills the child with the pty. The hand-off refuses such a pane by name. The remedy it names is to stop and resume the session, which relaunches it keeper-hosted.

