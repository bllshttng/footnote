# PTY survival architecture decision (Phase 6, ab-a09e1eaf, Wave 0.1)

Date: 2026-05-23
Inputs: `pty-survival-findings.md` (Wave 0.0 empirical run)

## Decision: Outcome B — per-agent worker process owns the PTY master

The Wave 0 smoke prototype refuted the design's "direct daemon-owned PTY survives daemon restarts" assertion. A child spawned on a PTY whose master the daemon owns is SIGHUP'd and dies the moment the daemon's master fd closes (default action). Ignoring SIGHUP keeps the process alive but leaves it I/O-unreachable (no way to reattach to a slave whose master is gone). Reattach across daemon restarts therefore requires a long-lived holder of the master fd: a per-agent worker.

This decision **replaces** the design doc's Architecture-section claim that "PTY processes survive daemon restarts" under the direct model. The empirically-verified statement is:

> PTY children survive daemon restarts **because a per-agent worker process owns the PTY master and outlives the daemon.** The daemon spawns workers, never owns PTY masters directly; on restart it reconnects to surviving workers via their per-agent sockets.

## Architecture implications for Wave 1+

1. **New module `src/bin/worker.rs`** (per the design's Outcome B branch, ~200 LOC). One worker process per agent. The worker:
   - opens the PTY and spawns the provider child on the slave,
   - owns the master fd for the agent's whole lifetime,
   - exposes a small control/data socket at `~/.fno/agents/<short_id>/worker.sock`,
   - survives daemon death (separate process; daemon death does not close the worker's master).

2. **The daemon becomes a true supervisor.** It spawns/monitors workers and proxies `spawn`/`ask`/`drive` traffic to the right `worker.sock`. It holds no PTY masters. Daemon restart = re-scan `~/.fno/agents/*/worker.sock`, reconnect, resume.

3. **Recovery procedure updates.** The startup recovery sweep (design Architecture §Recovery) discovers workers via socket scan and reattaches, rather than reaping orphan PTYs the daemon used to own. Orphan-PID sweep still applies to workers whose socket is dead.

4. **Wave 1 foundation lands the worker seam now.** `pty.rs` is written as the worker-side PTY owner (spawn + drain + bounded ring on the worker), not daemon-side. The daemon↔worker protocol is stubbed in Wave 1 and fleshed out in Wave 3 (Daemon + IPC). This avoids a costly re-layering later.

## What does NOT change

- The Provider trait split, ReadinessDetector trait, RestartPolicy, write_queue backpressure, and anti-injection envelope are unaffected by A-vs-B; they live on the worker side under Outcome B but their shapes are identical.
- Wire formats, state-file schemas, and the cross-language flock invariant are independent of this branch.

## Residual risk / follow-ups

- Worker lifecycle adds its own failure modes (worker dies but agent child lingers; worker.sock stale). These are addressable with the same orphan-PID + heartbeat machinery already in the design; tracked for Wave 3.
- Linux confirmation run of the Wave 0 probe is a cheap follow-up; mechanism is POSIX-standard so the branch is not expected to change.
