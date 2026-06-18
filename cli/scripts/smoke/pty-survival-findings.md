# PTY survival smoke findings (Phase 6, ab-a09e1eaf, Wave 0.0)

Date: 2026-05-23
Platform: darwin-arm64 (macOS), rustc 1.94.1, portable-pty 0.8.1
Harness: `cli/scripts/smoke/pty-survival/` (`run-survival-test.sh`)

## Question

The Phase 6 design asserts a load-bearing requirement: *PTY-managed agents must survive daemon restarts* ("the daemon is a supervisor, not a controller"). Codex's 2026-05-22 review doubted this, expecting POSIX SIGHUP on master-fd close to kill the child. Wave 0 resolves it empirically before any Wave 1 code commits to an architecture.

## Method

A ~70 LOC Rust supervisor (`pty-survival-probe`) opens a PTY via `portable-pty`, spawns a bash heartbeat child on the slave, drops the slave handle, prints both PIDs, and blocks draining the master. The harness lets the child beat ~4 times, `kill -9`s the supervisor (worst case: no graceful close of the master), waits 5s, then checks (a) is the child still alive (`kill -0`), (b) did the heartbeat log keep growing, (c) did SIGHUP arrive. Two child modes:

- `default` — child takes SIGHUP's default action (terminate), logging arrival first.
- `ignore` — child `trap '' SIGHUP` (the mitigation a hardened worker would apply).

## Raw results

| Mode | child_alive @ +5s | heartbeats before / after | SIGHUP observed | Verdict |
|------|-------------------|---------------------------|-----------------|---------|
| `default` | no | 4 / 4 (no growth) | yes (`child_sighup_received`) | **DIED** |
| `ignore`  | yes | 4 / 9 (kept beating) | yes (`child_sighup_ignored`) | **SURVIVED (process), but I/O-detached** |

`default`-mode log tail (cause of death is unambiguous):

```
heartbeat pid=78679 ts=1779566760.484683000
child_sighup_received pid=78679 ts=1779566761.499950000   <- supervisor SIGKILLed; master closed; slave hung up
(no further heartbeats; child terminated)
```

## Interpretation

1. **Direct daemon-owned PTY does not survive (Outcome A refuted).** When the supervisor dies, its master fd closes; the kernel hangs up the slave's controlling terminal and sends SIGHUP to the child's process group. Default action terminates the child. This is core POSIX TTY behavior (consistent across darwin and linux), not a portable-pty quirk.

2. **Ignoring SIGHUP keeps the process alive but does NOT make it reattachable.** In `ignore` mode the child outlived the supervisor and kept beating — but the master fd is gone. You cannot mint a *new* master for an existing slave, so a freshly-started daemon has no I/O path back to that child. Process-liveness without reachability is useless for the supervisor model (we could never stream output or send input again).

3. **Reattach requires a process that holds the master fd open across daemon restarts.** That is, by definition, a per-agent worker. Outcome C (SCM_RIGHTS master-fd passing) does not avoid this — the receiving end of the fd is itself a holder that must outlive the daemon, i.e. a worker by another name. The minimal robust design is therefore a per-agent worker process (Outcome B).

## Scope notes / follow-ups

- Tested on darwin-arm64 only. The mechanism (SIGHUP on controlling-terminal hang-up) is POSIX-standard; a linux-x64 confirmation run is a cheap follow-up but the outcome is not expected to differ.
- `portable-pty 0.8.1` resolved (0.9.0 available). Wave 1 should pin a version deliberately; AgentRelay's pin was unavailable (reference repo absent at `~/code/tools/agentworkforce/relay`).
- The harness and probe are committed as reproducible evidence, mirroring the existing `capture-*.sh` smoke scripts. The probe crate is `publish = false` and excluded from the workspace build.
