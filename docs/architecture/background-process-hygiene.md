# Background process hygiene

The contract for anything that keeps running after the command that started it. Written after one session left 73 `yes > /dev/null` processes at PPID 1, 797% CPU combined, 71 core-hours over 7.5 hours, which starved a lock holder to 0.31 seconds of CPU in 52 minutes and wedged the preflight gate the same session was waiting on.

## The one line to copy

```bash
timeout 300 bash -c 'exec -a fno-load-<node-or-session> yes > /dev/null'
```

`timeout` is the death path. `exec -a fno-...` is the name. Use `bash` explicitly: zsh has no `exec -a`.

Both halves earn their place. Without the bound the process outlives you, and `yes` writing to `/dev/null` never even receives SIGPIPE, because there is no reader to go away and the sink always accepts. Without the name the survivor is anonymous in `top`, and the only way anyone finds its owner is `lsof` archaeology against a session task directory.

## Two classes, two mechanisms

A process that can never end on its own is a different defect from an ordinary command that outlived its parent. One is refusable at creation time. The other is not, because a `grep` is a legitimate command and nothing in its text says its parent is about to die.

| Specimen, measured 2026-08-13 | Prevented by the guard? |
|---|---|
| 73 `yes > /dev/null`, PPID 1, 797% CPU | yes, denied at the Bash boundary |
| one `grep -rn --include=*.py`, PPID 1, 64% CPU | no. Reported by the sweep. |
| background Bash tasks that outlived their session | no. Reported by the sweep. |

One of three. That number is the honest coverage of the preventive layer, and the sweep is what covers the rest.

## The guard: `hooks/bg-process-guard.py`

A PreToolUse hook on Bash, wired on both the claude and codex lanes. It denies a command that can never end and carries no time bound in the same segment.

Refused shapes: `yes`, `while true`, `while :`, `until false`, `for ((;;))`, `sleep infinity`, `stress`/`stress-ng` with no `-t`, `dd` from `/dev/zero` or `/dev/urandom` with no `count=`, and a checksum reading an endless device.

Any of these makes it legal: `timeout`, `gtimeout`, `head` in the pipeline, `count=`, `ulimit -t`, `-t <seconds>`.

It denies whether or not the call sets `run_in_background`. A foreground unbounded `yes` is orphaned just as surely when the session exits.

It is parse-only and imports nothing outside the standard library, `psutil` included. A hook runs under whatever bare interpreter the harness hands it, and an ImportError here would take the guard down on every Bash call. It fails open on anything unexpected, because a guard that breaks a session on its own bug is worse than the orphans it prevents.

It does not reach every harness. opencode and agy have no PreToolUse lane here, and a test fixture that spawns a subprocess never passes through the Bash tool at all. Those are the sweep's.

## The sweep: `fno agents orphans`

Enumerates with `psutil`, never `ps ... | awk`. That pipeline returned `count=0` twice during the investigation, because the harness truncated 1189 lines and the pipeline counted the summary line. Redirect to a file and read the file, or use objects.

**Attribution has exactly two arms.** NAME: argv[0] starts with `fno-` and differs from the executable, so the process was deliberately relabelled. CWD: the process's working directory is under a repo root or a `.claude/worktrees/` path. A process must also be at PPID 1, owned by this uid, and absent from the live worker census.

**There is no CPU floor.** CPU is printed on every finding and gates none of them. One specimen burned no CPU at all.

**It plants its own orphans before it counts anything.** A count of zero has two explanations, and a clean machine looks exactly like a dead instrument. So each arm gets a probe that cannot satisfy the other arm: the NAME probe is renamed and parked outside any repo, the CWD probe keeps its own name inside it. Either one missing prints `verdict withheld (scan-broken)` and exits 2 with no orphan count at all. Break an arm on purpose to watch it work:

```bash
FNO_ORPHANS_SKIP_PROBE=name fno agents orphans; echo "exit=$?"
FNO_ORPHANS_SKIP_PROBE=cwd  fno agents orphans; echo "exit=$?"
```

Read that exit code directly. Never through a `| tail`, which reports the tail's status and has hidden a failing gate here before.

**What the live control does not prove**, stated so nobody reads it as covering more than it does: the uid arm, the census exclusion, and the reap age gate. The probe is ours, is not a census row, and is seconds old. Those three are covered by unit tests against a fabricated process table in `cli/tests/agents/test_orphans.py`.

## Reaping, and why naming is the precondition

`--reap` kills only a finding that was deliberately renamed, carries the `fno-` prefix, and is older than ten minutes. Everything else is reported and left alone. Attribution is a heuristic, and a heuristic must not hold a kill signal.

This inverts the intuitive ranking of the four prevention layers. Naming looks like the cosmetic one. It is the precondition for every automatic kill: a signal fired at a process we relabelled ourselves is safe, and one fired from a guess is not.

The rename requirement is not decoration either. `fno-agents-daemon` runs at PPID 1 in this repo right now, and a bare `fno-` prefix test would have handed the daemon to the killer.

**Expect the sweep to report far more than it kills.** Every orphan that predates the guard carries whatever name it was born with, so a name-gated reap can never touch it. Those are exactly the processes nobody has license to kill unattended. Read the gap as the guard's install date, not as a broken reap.

A clean machine here still reports long-lived daemons at PPID 1 whose cwd is the repo, including third-party ones. `--quiet-unless-new` reports each once and then stays silent, which is what the SessionStart hook uses; the bare verb always prints everything.

## Measured facts this document rests on

- `exec -a fno-load-test sleep 20` produces a process whose `ps -o comm` reads `fno-load-test`, whose `psutil.name()` reads `sleep`, and whose `psutil.cmdline()[0]` reads `fno-load-test`. psutil reads the executable on macOS; the rename lands in argv[0] alone. Attributing on `name()` makes the NAME arm unfirable, which is what the positive control caught on its first live run.
- A shell that backgrounds a child and exits leaves that child at PPID 1. The child's stdout must be redirected rather than inherited, or a parent capturing output blocks for the child's whole life.
