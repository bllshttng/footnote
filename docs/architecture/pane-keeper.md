# The pane keeper: worker panes that outlive the mux server

A worker pane's pty master does not live in the mux server. It lives in a dedicated **keeper** process (`fno-agents-worker --pane`, spawned per pane by the server). When the server dies - crash, `kill -9`, reboot of its parent - the keeper and its child keep running, and the next server on the same session **re-adopts the same child** instead of spawning a replacement. Plain (non-worker) panes are unchanged: the server holds their master directly, and they die with it, exactly as they always have.

## Ownership rule

**The keeper keeps, the server views.** The same rule that governs threads applies to panes: a viewer never owns the processes it displays. Concretely:

- The keeper `setsid()`s into its own session, opens the pty pair, spawns the provider argv on the slave, and owns the master for the pane's whole life.
- The server holds one unix socket to the keeper. Attach, detach, restart, or SIGKILL of the server never touches the child's controlling terminal, so no SIGHUP reaches it.
- Admission counts and `pane ls` report the CHILD's pid (answered by the keeper's Identify), never the keeper's: a fleet count and any later kill aim at the process the user sees.

## Protocol

Frames are `u8 tag | u32 LE length | payload`, mirrored between `crates/fno-agents/src/pane_keeper.rs` (keeper side) and `crates/fno/src/pty.rs` (client side). Client to keeper: `Input`, `Resize`, `Kill`, `Identify`. Keeper to client: `IdentifyReply(json)`, `Output`, `Exited(i32)`. A protocol version rides the IdentifyReply; a newer keeper meeting an older client (or the reverse, across an upgrade) refuses loudly instead of decoding garbage.

The **subscriber** is the one client whose Input/Resize/Kill frames drive the child and whose connection receives Output - the mux server, at spawn or adoption. Later connections (`fno mux pane keeper list` probes) are answered Identify on their own connection but can never drive or steal the stream. When the subscriber dies the keeper keeps running; the next connection to arrive (the re-adopting server) takes the seat.

The keeper retains recent output in a bounded ring (default 1 MiB, `--ring-bytes`) and replays it at Identify, stating dropped bytes rather than losing them silently. That is the detached window a re-adopting server can restore.

## Re-adoption

At startup, before serving, the server scans `<state-root>/mux/panes/*.sock` (see [state-root-inventory](../state-root-inventory.md) for the owner + lifetime row):

1. A socket whose keeper answers Identify is adopted: the handshake drains the ring, learns the child pid, argv, cwd and size, and the pane is registered under a fresh pane id with its identity rebuilt from argv (`fno mux pane ls` shows it; worker-name and session-target joins rebind it to its squad member, so a restore focuses the adopted pane instead of spawning a second one).
2. A socket with no live listener is unlinked and named in the server log: the stale-socket contract. Leftover sockets whose child is gone are also reported by `fno mux pane keeper list` (`stale: true`), which answers with the server dead - a done-probe can grep `keeper_pid` from its JSON without any server running.

Re-adoption is not respawn: the proof below pins the SAME child pid across the server's death.

## The reaper contract

`kill_all_panes` (server shutdown sweep) **skips `PtyShell::Keeper` panes**: plain panes die with their server, keeper-hosted panes survive to re-adoption. A hangup must never become a close. The deliberate path is unchanged: `reap_pane` (explicit pane close) still sends Kill, and the keeper SIGKILLs its own child, unlinks its socket, and exits. Surviving a hangup never becomes surviving a close.

## Two traps the implementation had to learn

**portable-pty's `take_writer` is take-once, and its writer sends EOT on drop.** The keeper takes the pty writer once at startup and reuses it for every Input frame. A write-and-drop per frame ships a literal Ctrl-D after the first keystroke (the writer's Drop sends EOT, ending the child's stdin), and a second `take_writer` refuses outright. The local-pane path never meets this because it takes the writer once in `wire()` too.

**A handshake quiet window can cut a frame in half.** The adoption handshake drains the ring replay until 150ms of silence; a frame split by that boundary must seed the socket reader thread's buffer, or the tail arrives unanchored and the byte stream desyncs - a payload byte read as a tag decodes as garbage, observed as a phantom `Exited` that reaped a live pane. `keeper_handshake` therefore returns its partial-frame leftover and `spawn_keeper_reader` starts from it.

## The proof

`tests/mux-keeper-survives-server-kill.sh` builds both binaries, drives a real server, and asserts survival by named pids - never by exit codes or survivor counts. A passing run reads:

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

The server is killed with SIGKILL so no graceful path can spare the child through a route that proves nothing. The answer step reads the whole visible grid, not `--lines`: `read_tail` reads the bottom N display rows, and a fresh adopted VT holds this pane's short answer at the top rows with an empty history.
