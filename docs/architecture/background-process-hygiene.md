# Background process hygiene

The contract for anything that keeps running after the command that started it. One session left 73 `yes > /dev/null` processes at PPID 1, 797% CPU combined, 71 core-hours over 7.5 hours. That load starved a lock holder to 0.31 seconds of CPU in 52 minutes. It wedged the preflight gate the same session was waiting on.

## The one line to copy

```bash
timeout 300 bash -c 'exec -a fno-load-<node-or-session> yes > /dev/null'
```

`timeout` is the death path. `exec -a fno-...` is the name. Use `bash` explicitly: zsh has no `exec -a`.

Both halves earn their place. Without the bound the process outlives you. `yes` writing to `/dev/null` never even receives SIGPIPE, because no reader goes away and the sink always accepts. Without the name the survivor is anonymous in `top`. The only way to find its owner is `lsof` archaeology against a session task directory.

## Two classes, two mechanisms

A process that can never end on its own is a different defect from an ordinary command that outlived its parent. One is refusable at creation time. The other is not. A `grep` is a legitimate command, and nothing in its text says its parent is about to die.

| Specimen, measured 2026-08-13 | Prevented by the guard? |
|---|---|
| 73 `yes > /dev/null`, PPID 1, 797% CPU | yes, denied at the Bash boundary |
| one `grep -rn --include=*.py`, PPID 1, 64% CPU | no. Reported by the sweep. |
| background Bash tasks that outlived their session | no. Reported by the sweep. |

One of three. That number is the honest coverage of the preventive layer, and the sweep is what covers the rest.

## The guard: `hooks/bg-process-guard.py`

A PreToolUse hook on Bash, wired on both the claude and codex lanes. It denies a command that can never end and carries no time bound in the same segment.

Refused shapes: `yes`, `while true`, `while :`, `until false`, `for ((;;))`, `sleep infinity`. Also `stress` and `stress-ng` with no `-t`, `dd` from an endless device with no `count=`, and a checksum reading one.

Any of these makes it legal: `timeout`, `gtimeout`, `head` in the pipeline, `count=`, `ulimit -t`, `-t <seconds>`. A `break`, `exit`, or `return` in COMMAND position after the loop header also clears it. `while true; do sleep 5; gh pr view && break; done` is the standard poll and it ends. Two things discriminate, and neither is the word. Command position: `while true; do echo break; done` still runs forever, and so does `while true; do rg break src; done`. A text match read both as escapes. Position relative to the header: an escape leaves the loop it is inside, never one it precedes, so `cd /tmp || exit 1; while true; do sleep 60; done &` is still refused.

Heredoc bodies are stripped before parsing. `cat > poll.sh <<'EOF' ... EOF` writes a script. It does not run one. Reading the body as commands refused the write.

It denies whether or not the call sets `run_in_background`. A foreground unbounded `yes` is orphaned just as surely at session exit.

It is parse-only and imports nothing outside the standard library, `psutil` included. A hook runs under whatever bare interpreter the harness hands it. An ImportError here takes the guard down on every Bash call. It fails open on anything unexpected. A guard that breaks a session on its own bug is worse than the orphans it prevents.

It does not reach every harness. opencode and agy have no PreToolUse lane here. A test fixture that spawns a subprocess never passes through the Bash tool at all. Those are the sweep's.

## The sweep: `fno agents orphans`

Enumerates with `psutil`, never `ps ... | awk`. That pipeline returned `count=0` twice during the investigation. The harness truncated 1189 lines, and the pipeline counted the summary line. Redirect to a file and read the file, or use objects.

**Attribution has exactly two arms.** NAME: argv[0] starts with `fno-` and differs from the executable. That means someone relabelled the process on purpose. CWD: the working directory is under a known worktree of this repo. Roots come from `git worktree list`, not only from the canonical checkout. An externally based worktree carries no literal `.claude/worktrees/` in its path, so a path test alone reported a clean machine on those projects. A process must also be at PPID 1, owned by this uid, and absent from the live worker census.

**There is no CPU floor.** CPU is printed on every finding and gates none of them. One specimen burned no CPU at all.

**It plants its own orphans before it counts anything.** A count of zero has two explanations. A clean machine looks exactly like a dead instrument. So each arm gets a probe that cannot satisfy the other arm. The NAME probe is renamed to an `fno-orphan-probe-` name and parked outside any repo. The CWD probe sits inside it under an `orphan-probe-cwd-` name. That name carries no `fno-` prefix, so the NAME arm cannot claim it and `--reap` cannot kill it. Both markers exist so two sweeps inside one 30-second probe lifetime do not report each other's controls. An unmarked CWD probe is a plain `sleep` in the repo root at PPID 1. That is exactly what the CWD arm reports. The kill path is a third arm, and it is measured after the kill, never before. `_kill` swallows every exception, so reading it off "a probe was spawned" asserts an easier thing than a real kill does. Any missing arm prints `verdict withheld (scan-broken)` and exits 2 with no orphan count at all. Break an arm on purpose to watch it work:

```bash
FNO_ORPHANS_SKIP_PROBE=name fno agents orphans; echo "exit=$?"
FNO_ORPHANS_SKIP_PROBE=cwd  fno agents orphans; echo "exit=$?"
```

Read that exit code directly. Never through a `| tail`. A tail reports its own status, and that has hidden a failing gate here before.

**What the live control does not prove**, stated so nobody reads it as covering more than it does. Three arms stay unproven: the uid test, the census exclusion, and the reap age gate. The probe is ours, is not a census row, and is seconds old. Unit tests cover those three against a fabricated process table in `cli/tests/agents/test_orphans.py`.

## Reaping, and why naming is the precondition

`--reap` kills only a finding that was deliberately renamed, carries the `fno-` prefix, and is older than ten minutes. Everything else is reported and left alone. Attribution is a heuristic, and a heuristic must not hold a kill signal.

This inverts the intuitive ranking of the four prevention layers. Naming looks like the cosmetic one. It is the precondition for every automatic kill. A signal fired at a process we relabelled ourselves is safe. A signal fired from a guess is not.

The rename requirement is not decoration either. `fno-agents-daemon` runs at PPID 1 in this repo right now. A bare `fno-` prefix test hands the daemon to the killer.

**Expect the sweep to report far more than it kills.** Every orphan that predates the guard carries whatever name it was born with. A name-gated reap can never touch it. Those are exactly the processes nobody has license to kill unattended. Read the gap as the guard's install date, not as a broken reap.

A clean machine here still reports third-party daemons at PPID 1 whose cwd is the repo. `--quiet-unless-new` reports each once and then stays silent. That is what the SessionStart hook uses. The bare verb always prints everything.

**Footnote's own daemons are counted, never listed.** `fno-agents-daemon` and `fno-agents-worker` run detached at PPID 1 by design. So PPID 1 says nothing about whether one leaked. Each restart also mints a fresh seen-key that speaks again. An hourly report nobody can act on is how a sweep gets ignored. The scan line names the count (`3 own-daemon`), because a silent exclusion is the same absence trap the rest of this module refuses. The match is on argv[0] exactly, so `fno-agents-daemon-load` is still a finding. A prefix test hands anyone an opt-out.

A broken scan records nothing in the seen-file. It withheld its findings, so marking them reported lets one census failure silence a real orphan on every healthy sweep after it.

## Measured facts this document rests on

- `exec -a fno-load-test sleep 20` gives a process whose `ps -o comm` reads `fno-load-test`. Its `psutil.name()` reads `sleep`. Its `psutil.cmdline()[0]` reads `fno-load-test`. psutil reads the executable on macOS. The rename lands in argv[0] alone. Attributing on `name()` makes the NAME arm unfirable, which the positive control caught on its first live run.
- A shell that backgrounds a child and exits leaves that child at PPID 1. The child's stdout must be redirected, never inherited. A parent capturing inherited output blocks for the child's whole life.
