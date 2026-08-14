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

These make it legal. `timeout` or `gtimeout` as the command. `count=` on `dd`. `-t` or `--timeout` on `stress`, with an optional unit suffix. And `ulimit -t` in an earlier command. So does piping INTO a reader that exits, as in `yes | head -c 1M`. Each is read in command position, so an argument spelling `timeout` is not a bound, and neither is a trailing comment mentioning one. `head` bounds only the stage that feeds it, so `head -c 1M f | yes` is still refused. A `break`, `exit`, or `return` in COMMAND position inside the loop also clears it. `while true; do sleep 5; gh pr view && break; done` is the standard poll and it ends.

The escape is charged to the loop it sits INSIDE, which needs a stack of open loops. Three cheaper rules were each tried and each was wrong. Reading the whole command let `cd /tmp || exit 1; while true; do sleep 60; done &` clear its own loop from a line that had already run. Stopping at the first `done` broke on a nested loop. `while true; do for n in 1 2; do echo $n; done; gh pr view && break; done` was refused, because the inner `done` truncated the scan before the real `break`. One shared boolean let an escape in an earlier loop license every later one, so `while true; do break; done; while true; do sleep 60; done &` was allowed with specimen 1 in the second half.

Command position is the other half, and it is not the word. `while true; do echo break; done` still runs forever, and so does `while true; do rg break src; done`. A text match read both as escapes.

`for ((...))` is read the same way. Bash takes three `;`-separated expressions and the MIDDLE one is the condition. An empty condition is what never ends, so `for ((i=0;;))` is refused and `for ((i=0;i<10;i++))` counts to ten and passes. The header is rebuilt from tokens in command position. Matched on the raw command text instead, the guard denied `echo "for ((;;))"`, `rg 'for ((;;))' hooks/`, and a commit message naming the shape. The third one blocked writing about this guard in the repo that ships it.

Reading tokens is not enough on its own. A quoted argument is ONE token whose text still lands in the rebuilt header, so `for f in "for ((;;))"` was denied even after the move off raw text. Two rules close that. The arithmetic must start immediately after `for`, and a token carrying whitespace ends the header.

Each loop reads its OWN header, at the token index the walk already resolved. An earlier design built a list of headers in a second walk and paired them to loops by counting. That pairing produced three separate bugs, because the two walks disagreed about which `for`s were in command position. A subshell `(`, an assignment prefix, and a `case` pattern word `for` each shifted the count. A header then described one loop while being charged to another. The third let `case $x in a|for) echo hi;; esac; for ((;;)); do :; done` through. Two of the three were introduced by the fix for the one before it. Positional pairing was the defect, so it is gone rather than corrected a fourth time.

A `|` between `case` patterns is alternation, not a pipe. Split as a pipe, the last alternative became a pipeline stage with no reader, so `case $a in y|yes) echo go;; esac` was refused. That refusal was also order-dependent: `yes|y)` passed. Inside a `case` region the guard does not split on `|` at all, which loses a real pipeline in an arm body. That is the cheaper error.

A wrapper's flags are skipped so the real command surfaces, and the list of value-taking flags is hand-written. Any flag missing from it handed command position to its own value: `caffeinate -t 3600 yes > /dev/null` resolved to a command called `3600`. Measured, not reasoned: under `timeout 3` it exits 124, so the `yes` really does run forever. Each segment is now read BOTH ways, because neither is safe alone. Reading every unknown flag as value-taking loses `sudo -E yes > /dev/null`. There `-E` is a real boolean, and the skip eats the generator behind it.

**Test a change to this guard against real history, not against invented cases.** Every false refusal above was found the same way. Extract the unique Bash commands from the local transcripts under `~/.claude/projects/`. Run each one through the hook. Read every DENIAL. On a corpus of 366,478 commands the guard denied 23, and 3 of those were wrong. A hand-written case list missed all three across four rounds. The shapes that break a parser are the ones nobody thinks to write. A poll loop wearing a nested `for`. A `case` arm listing `y|yes`. A quoted argument holding the very syntax being matched.

The denial set is small enough to read line by line. That is the point. A guard whose refusals you cannot enumerate is a guard nobody can judge.

The sweep is also the only check that confirms the guard does its job. On the current corpus it denies 20 commands and refuses nothing else, and one of the 20 is `for i in $(seq 1 24); do yes > /dev/null & done`. That is the command that burned 71 core-hours, taken verbatim from the transcript that ran it. Both it and the subshell spinner `for j in 1 2 3 4 5 6; do ( while :; do :; done ) & done` are rows in the test table now.

Two rules keep the sweep honest. Read every denial, never a count of them. And run it after any change to the parser, because both regressions found this way were introduced by the round that fixed the previous ones.

`scripts/diagnostics/guard-corpus-sweep.py` runs it. Bare, it prints every denial. With `--against <ref>` it prints every command whose verdict FLIPPED against the guard at that ref, in both directions. Use the differential for a parser change. A denial count that stays at 20 says nothing about whether the same 20 commands are in it. That distinction separates an intended fix from a regression.

Heredoc bodies are stripped before parsing. `cat > poll.sh <<'EOF' ... EOF` writes a script. It does not run one. Reading the body as commands refused the write.

It denies whether or not the call sets `run_in_background`. A foreground unbounded `yes` is orphaned just as surely at session exit.

It is parse-only and imports nothing outside the standard library, `psutil` included. A hook runs under whatever bare interpreter the harness hands it. An ImportError here takes the guard down on every Bash call. It fails open on anything unexpected. A guard that breaks a session on its own bug is worse than the orphans it prevents.

It does not reach every harness. opencode and agy have no PreToolUse lane here. A test fixture that spawns a subprocess never passes through the Bash tool at all. Those are the sweep's.

It reads one level into `bash -c` and no further. A payload handed to `eval`, or detached by `screen -dmS` or `tmux new -d`, is never parsed. So `eval 'yes > /dev/null'` passes, and so does a command substitution. A shell FUNCTION body is invisible the same way: `myloop() { while true; do sleep 1; done; }; myloop &` passes, while the bare-brace `{ while true; ...; } &` is refused. That is the fail-open direction on purpose. A guard that guesses at nested quoting refuses real work, and the sweep backstops what the guard misses. Adding `eval` to the recursion is the one cheap extension left.

## The sweep: `fno agents orphans`

Enumerates with `psutil`, never `ps ... | awk`. That pipeline returned `count=0` twice during the investigation. The harness truncated 1189 lines, and the pipeline counted the summary line. Redirect to a file and read the file, or use objects.

**Attribution has exactly two arms.** NAME: argv[0] starts with `fno-` and differs from the executable. That means someone relabelled the process on purpose. CWD: the working directory is under a known worktree of this repo. Roots come from `git worktree list`, not only from the canonical checkout. An externally based worktree carries no literal `.claude/worktrees/` in its path, so a path test alone reported a clean machine on those projects. A process must also be at PPID 1, owned by this uid, and absent from the live worker census.

**There is no CPU floor.** CPU is printed on every finding and gates none of them. One specimen burned no CPU at all.

**The reaper is measured, not assumed to be PID 1.** That assumption is macOS-shaped. On a Linux host where `systemd --user` is the reaper, or under anything that called `PR_SET_CHILD_SUBREAPER`, an orphan reparents to that manager instead. A literal `== 1` made both probes fail there, so every hourly sweep printed `verdict withheld (scan-broken)` forever with no way to quiet it. The NAME probe answers the question instead: whatever it reparents to IS the reaper on this host. No probe settling means no reaper, and the sweep says the reaper is unknown rather than counting against a guess.

**Measuring ONE reaper is still wrong.** A subreaper claims only its own descendants. PID 1 stays the reaper of last resort for everything else. So both ppids are live at once. An orphan whose ancestors exited above the subreaper sits at PID 1. Filtering on the single measured reaper dropped every one of those. The sweep still printed a green control and a positive `orphans:` count. A count that is wrong is worse than a withheld one. This is the module's own thesis failing in its own terms. The scan matches either ppid.

The scan line says `reparented`, not `ppid=1`. The label was hardcoded inside a design whose whole point is the measured reaper. On a subreaper host it printed `ppid=1` over a count of something else.

**It plants its own orphans before it counts anything.** A count of zero has two explanations. A clean machine looks exactly like a dead instrument. So each arm gets a probe that cannot satisfy the other arm. The NAME probe is renamed to an `fno-orphan-probe-` name and parked outside any repo. The CWD probe sits inside it under an `orphan-probe-cwd-` name. That name carries no `fno-` prefix, so the NAME arm cannot claim it and `--reap` cannot kill it. Both markers exist so two sweeps inside one 30-second probe lifetime do not report each other's controls. An unmarked CWD probe is a plain `sleep` in the repo root at PPID 1. That is exactly what the CWD arm reports. The kill path is a third arm, and it is measured after the kill, never before. `_kill` swallows every exception, so reading it off "a probe was spawned" asserts an easier thing than a real kill does. Any missing arm prints `verdict withheld (scan-broken)` and exits 2 with no orphan count at all. Break an arm on purpose to watch it work:

```bash
FNO_ORPHANS_SKIP_PROBE=name fno agents orphans; echo "exit=$?"
FNO_ORPHANS_SKIP_PROBE=cwd  fno agents orphans; echo "exit=$?"
```

Read that exit code directly. Never through a `| tail`. A tail reports its own status, and that has hidden a failing gate here before.

Read the printed line too, not only the code. A deployed `fno` older than this verb answers `No such command 'orphans'` and exits 2, which is the same code a failing arm uses. All three commands above then look like the falsifier working. The healthy run is what separates them: it exits 0 on a current binary and 2 on a stale one. `fno doctor` names the staleness, and `fno doctor --fix` updates it.

**What the live control does not prove**, stated so nobody reads it as covering more than it does. Three arms stay unproven: the uid test, the census exclusion, and the reap age gate. The probe is ours, is not a census row, and is seconds old. Unit tests cover those three against a fabricated process table in `cli/tests/agents/test_orphans.py`.

## Reaping, and why naming is the precondition

`--reap` kills only a finding that was deliberately renamed, carries the `fno-` prefix, and is older than ten minutes. Everything else is reported and left alone. Attribution is a heuristic, and a heuristic must not hold a kill signal.

This inverts the intuitive ranking of the four prevention layers. Naming looks like the cosmetic one. It is the precondition for every automatic kill. A signal fired at a process we relabelled ourselves is safe. A signal fired from a guess is not.

The rename requirement is not decoration either. `fno-agents-daemon` runs at PPID 1 in this repo right now. A bare `fno-` prefix test hands the daemon to the killer.

**Expect the sweep to report far more than it kills.** Every orphan that predates the guard carries whatever name it was born with. A name-gated reap can never touch it. Those are exactly the processes nobody has license to kill unattended. Read the gap as the guard's install date, not as a broken reap.

A clean machine here still reports third-party daemons at PPID 1 whose cwd is the repo. `--quiet-unless-new` reports each once and then stays silent. That is what the SessionStart hook uses. The bare verb always prints everything.

The quiet gate keys on pid, name and start time, so it mutes a process, never a kind of process. A harness that starts a fresh `node` or `bash` per session mints a new key each time, and the sweep speaks again. That is correct and it is also the noisiest thing here. Read a repeat as a new process, not as a broken gate.

**Footnote's own daemons are counted, never listed.** `fno-agents-daemon` and `fno-agents-worker` run detached at PPID 1 by design. So PPID 1 says nothing about whether one leaked. Each restart also mints a fresh seen-key that speaks again. An hourly report nobody can act on is how a sweep gets ignored. The scan line names the count (`3 own-daemon`), because a silent exclusion is the same absence trap the rest of this module refuses. The match is on argv[0] exactly, so `fno-agents-daemon-load` is still a finding. A prefix test hands anyone an opt-out.

A broken scan records nothing in the seen-file. It withheld its findings, so marking them reported lets one census failure silence a real orphan on every healthy sweep after it.

## Measured facts this document rests on

- `exec -a fno-load-test sleep 20` gives a process whose `ps -o comm` reads `fno-load-test`. Its `psutil.name()` reads `sleep`. Its `psutil.cmdline()[0]` reads `fno-load-test`. psutil reads the executable on macOS. The rename lands in argv[0] alone. Attributing on `name()` makes the NAME arm unfirable, which the positive control caught on its first live run.
- A shell that backgrounds a child and exits leaves that child at PPID 1. The child's stdout must be redirected, never inherited. A parent capturing inherited output blocks for the child's whole life.
