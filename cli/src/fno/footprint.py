"""Pure parsing for the fleet's machine-footprint reading.

The parser receives one file-backed ``ps`` snapshot and never runs ``ps``
itself. A row is fleet-attributable when its command is a directly attributable
fno process or when its parent chain reaches one. Worker sessions such as
``claude`` remain outside this overhead measurement unless they are descendants
of an attributable fno process.
"""

from __future__ import annotations

import re
import shlex
from pathlib import Path
from typing import NamedTuple


SUSTAINED_FLOOR_SECONDS = 30
_FNO_BINARIES = frozenset(
    {"fno", "fno-py", "fno-agents", "fno-agents-daemon", "fno-agents-worker"}
)


class Footprint(NamedTuple):
    """One snapshot split into sustained and startup-cost buckets."""

    sustained_cpu_cores: float
    descendant_cpu_cores: float
    fleet_cpu_cores: float
    descendant_process_count: int
    direct_process_count: int
    transient_call_count: int
    process_count: int
    rss_gb: float
    measured_cpu_cores: float
    top: list[tuple[float, str]]
    unparsed_lines: int
    # None on a complete reading. Set when live worker rows could not be
    # attributed to processes: the fleet share above is then an UNDERCOUNT,
    # which a spawn gate must read as unknown, never as headroom (x-e040).
    attribution_gap: str | None = None
    # Whole-machine count of processes whose OWN program is a test runner.
    # Whole-machine on purpose: a test a person started competes for the same
    # box as a lane.
    test_process_count: int = 0
    # The Claude Code background daemon's idle pre-warm pool. Not fleet CPU:
    # these are not fno processes and fno never bounds or sweeps them. Measured
    # because they can hold most of the machine, and a load refusal that cannot
    # name them sends its reader after the wrong cause.
    spare_pool_process_count: int = 0
    spare_pool_cpu_cores: float = 0.0
    spare_pool_rss_gb: float = 0.0
    # Whole-machine census (x-d6ad AC10), beside the roster-scoped counts.
    machine_process_count: int = 0
    runnable_count: int = 0


class Admission(NamedTuple):
    """The CPU axis's decision, computed once and read by both gates (x-7783 LD1).

    ``verdict`` is ``admit`` | ``hold`` | ``undecidable`` | ``refuse``;
    ``axis`` names what decided: the fleet's CPU share, the fifteen-minute
    backstop, or an unreadable instrument. ``reason`` is the full sentence
    the gates print verbatim. An attribution gap widens the share to an
    interval: ``share_low`` is the attributed share, ``share_high`` the
    whole machine's, and ``bound`` records that the verdict read the upper
    bound. ``backstop`` is ``hard_max_load_per_cpu x cpus``.
    """

    verdict: str
    axis: str
    reason: str
    share_low: float
    share_high: float
    bound: str
    fleet_cores: float
    machine_cores: float
    capacity_cores: float
    ceiling: float
    gap: str | None = None
    load_15m: float | None = None
    backstop: float = 0.0


#: argv[0] basenames that are a test runner on their own.
_TEST_RUNNER_NAMES = frozenset({"pytest", "py.test"})


def is_test_runner(command: str) -> bool:
    """True when a process's OWN program is a test runner.

    Matched on argv[0] plus the first non-flag arguments, never the whole
    command line: a substring scan counts a leaked keeper whose socket path
    sits under ``pytest-of-<user>``. Decoys pinned in test_footprint.py.
    """
    argv = command.split()
    if not argv:
        return False
    program = argv[0].rsplit("/", 1)[-1]
    rest = argv[1:]
    if program in _TEST_RUNNER_NAMES:
        return True
    if program.startswith("python") and rest[:2] == ["-m", "pytest"]:
        return True
    positional = [arg for arg in rest if not arg.startswith("-")]
    if program == "cargo":
        return positional[:1] in (["test"], ["nextest"])
    if program in {"fno", "fno-py"}:
        return positional[:1] == ["test"] or positional[:2] == ["doctor", "test"]
    return False


#: argv[1] tokens the Claude Code background daemon gives its pre-warm pool.
#: Both spellings are live: a pool child runs `claude bg-spare ...` while the
#: app-bundle path runs `claude --bg-pty-host ...`.
_CLAUDE_SPARE_POOL_ARGS = frozenset(
    {"bg-spare", "bg-pty-host", "--bg-spare", "--bg-pty-host"}
)


def is_claude_spare_pool(command: str) -> bool:
    """True for a Claude Code background-daemon pre-warm process.

    These are not fno processes. fno never bounds or sweeps them: a spare is
    what a new bg session is claimed from, so culling the pool disarms the
    dispatch path it serves. They are counted only so a refusal can name them.

    Matched on argv[0] plus argv[1], never a substring scan of the whole
    command line: a claude session whose PROMPT says bg-spare is a session.
    """
    argv = command.split()
    if len(argv) < 2:
        return False
    return argv[0].rsplit("/", 1)[-1] == "claude" and argv[1] in _CLAUDE_SPARE_POOL_ARGS


class _Process(NamedTuple):
    pid: int
    ppid: int | None
    elapsed_seconds: int
    cpu_percent: float
    rss_kb: int
    command: str
    # ps state letter(s); empty when the snapshot has no state column.
    state: str = ""


#: The `worktrees/<name>` tail a cluster of commands shares - the one line
#: that explains a spike ("that tree's test suite is running").
_WORKTREE_RE = re.compile(r"[\w~./-]*worktrees/[\w.-]+")


def top_consumers(sustained: list[tuple[float, str]], n: int = 5) -> list[dict]:
    """Top programs by summed ps ``%cpu``, with the worktree a cluster calls home.

    Aggregates the rows :func:`parse_footprint` already kept - no second ps
    pass. The name is argv[0]'s basename; ``worktree`` names the tree the
    most of that program's rows run from, with how many, so "a worker is
    running its test suite" stays one line instead of a mystery.
    """
    programs: dict[str, dict] = {}
    for cpu_percent, command in sustained:
        argv = command.split()
        name = Path(argv[0]).name if argv else command
        entry = programs.setdefault(
            name, {"name": name, "procs": 0, "cpu": 0.0, "trees": {}}
        )
        entry["procs"] += 1
        entry["cpu"] += cpu_percent
        match = _WORKTREE_RE.search(command)
        if match:
            entry["trees"][match.group(0)] = entry["trees"].get(match.group(0), 0) + 1
    ranked = sorted(
        programs.values(), key=lambda e: (-e["cpu"], -e["procs"], e["name"])
    )[:n]
    consumers: list[dict] = []
    for entry in ranked:
        tree, tree_procs = (
            max(entry["trees"].items(), key=lambda kv: kv[1]) if entry["trees"] else (None, 0)
        )
        consumers.append(
            {
                "name": entry["name"],
                "procs": entry["procs"],
                "cpu_pct": round(entry["cpu"], 1),
                "worktree": tree,
                "worktree_procs": tree_procs,
            }
        )
    return consumers


def _elapsed_seconds(value: str) -> int:
    """Parse the ``ps etime`` forms ``DD-HH:MM:SS``, ``HH:MM:SS`` or ``MM:SS``."""
    days = 0
    remainder = value
    if "-" in remainder:
        day_text, remainder = remainder.split("-", 1)
        days = int(day_text)
    parts = [int(part) for part in remainder.split(":")]
    if len(parts) == 3:
        hours, minutes, seconds = parts
    elif len(parts) == 2:
        hours = 0
        minutes, seconds = parts
    elif len(parts) == 1:
        hours = minutes = 0
        seconds = parts[0]
    else:
        raise ValueError(f"invalid etime: {value!r}")
    if min(days, hours, minutes, seconds) < 0 or minutes >= 60 or seconds >= 60:
        raise ValueError(f"invalid etime: {value!r}")
    return days * 86400 + hours * 3600 + minutes * 60 + seconds


def _attributed_command(command: str) -> bool:
    try:
        tokens = shlex.split(command)
    except (IndexError, ValueError):
        return False
    if not tokens:
        return False
    names = [Path(token).name for token in tokens]
    if names[0] in _FNO_BINARIES:
        return True
    return names[0].startswith("python") and "fno-py" in names[1:]


def parse_footprint(
    ps_output: str,
    *,
    sustained_floor_seconds: int = SUSTAINED_FLOOR_SECONDS,
    excluded_root_pids: set[int] | frozenset[int] | None = None,
    attributed_root_pids: set[int] | frozenset[int] | None = None,
    threshold_excluded_root_pids: set[int] | frozenset[int] | None = None,
) -> Footprint:
    """Parse a file-backed ``ps -Ao pid,ppid,state,etime,%cpu,rss,command`` snapshot.

    The six-column shape without ``state`` and the legacy five-column shape are
    accepted for old fixtures; without ``state`` the runnable count reads zero.
    """
    processes: dict[int, _Process] = {}
    unparsed_lines = 0
    new_format = False
    new_state_format = False

    for raw_line in ps_output.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        if line.startswith("PID "):
            header = line.split()
            new_format = len(header) >= 2 and header[1] == "PPID"
            # The state header varies by platform: STAT/STATE (BSD, macOS) or S (Linux).
            new_state_format = len(header) > 2 and header[2] in ("STAT", "STATE", "S")
            continue
        try:
            # Shapes, newest first: (maxsplit, has_state, has_ppid). The first
            # that parses wins; an etime's colon never collides with a state.
            if new_format:
                shapes = [(6, True, True)] if new_state_format else [(5, False, True)]
            else:
                shapes = [(6, True, True), (5, False, True), (4, False, False)]
            state = ""
            for maxsplit, has_state, has_ppid in shapes:
                fields = line.split(None, maxsplit)
                if len(fields) != maxsplit + 1:
                    continue
                try:
                    pid = int(fields[0])
                    ppid = int(fields[1]) if has_ppid else None
                    cursor = 2 if has_ppid else 1
                    if has_state:
                        state = fields[cursor]
                        cursor += 1
                    elapsed = _elapsed_seconds(fields[cursor])
                    cpu_percent = float(fields[cursor + 1])
                    rss = int(fields[cursor + 2])
                    command = fields[cursor + 3].strip()
                    break
                except (TypeError, ValueError):
                    state = ""
            else:
                raise ValueError("no shape matched")
            if pid < 0 or (ppid is not None and ppid < 0) or not command or cpu_percent < 0 or rss < 0:
                raise ValueError("invalid process fields")
        except (TypeError, ValueError):
            unparsed_lines += 1
            continue
        processes[pid] = _Process(pid, ppid, elapsed, cpu_percent, rss, command, state)

    excluded = frozenset(excluded_root_pids or ())
    attributed_roots = frozenset(attributed_root_pids or ())
    threshold_excluded_roots = frozenset(threshold_excluded_root_pids or ())
    direct = {
        pid: pid in attributed_roots or _attributed_command(process.command)
        for pid, process in processes.items()
    }
    excluded_cache: dict[int, bool] = {}
    attributed_cache: dict[int, bool] = {}
    attributed_marks = frozenset(p for p, d in direct.items() if d)

    def chain_reaches(pid: int, marks: frozenset[int], cache: dict[int, bool]) -> bool:
        # ONE walker for exclusion and attribution: two hand-rolled copies of
        # the same parent-chain traversal drifted apart on the last change to
        # one of them. cache entries are chain-complete (every stored pid's
        # whole walked path was stored under the same verdict), so consulting
        # the cache mid-walk is sound.
        cached = cache.get(pid)
        if cached is not None:
            return cached
        path: list[int] = []
        seen: set[int] = set()
        current: int | None = pid
        result = False
        while current is not None and current != 0 and current not in seen:
            cached = cache.get(current)
            if cached is not None:
                result = cached
                break
            seen.add(current)
            path.append(current)
            if current in marks:
                result = True
                break
            process = processes.get(current)
            current = process.ppid if process is not None else None
        for path_pid in path:
            cache[path_pid] = result
        return result

    def is_excluded(pid: int) -> bool:
        return chain_reaches(pid, excluded, excluded_cache)

    def is_attributed(pid: int) -> bool:
        if is_excluded(pid):
            return False
        return chain_reaches(pid, attributed_marks, attributed_cache)

    sustained_cpu_percent = 0.0
    descendant_cpu_percent = 0.0
    transient_call_count = 0
    descendant_process_count = 0
    direct_process_count = 0
    process_count = 0
    rss_kb = 0
    measured_cpu_percent = 0.0
    sustained: list[tuple[float, str]] = []
    test_process_count = 0
    spare_pool_count = 0
    spare_pool_cpu_percent = 0.0
    spare_pool_rss_kb = 0
    for pid, process in processes.items():
        if not is_excluded(pid):
            measured_cpu_percent += process.cpu_percent
            if is_test_runner(process.command):
                test_process_count += 1
            if is_claude_spare_pool(process.command):
                spare_pool_count += 1
                spare_pool_cpu_percent += process.cpu_percent
                spare_pool_rss_kb += process.rss_kb
        if not is_attributed(pid):
            continue

        process_count += 1
        rss_kb += process.rss_kb
        if not direct[pid]:
            descendant_process_count += 1
            descendant_cpu_percent += process.cpu_percent
            if process.cpu_percent:
                sustained.append((process.cpu_percent, process.command))
        elif pid in attributed_roots:
            if pid not in threshold_excluded_roots:
                direct_process_count += 1
            sustained_cpu_percent += process.cpu_percent
            sustained.append((process.cpu_percent, process.command))
        elif process.elapsed_seconds < sustained_floor_seconds:
            direct_process_count += 1
            transient_call_count += 1
        else:
            direct_process_count += 1
            sustained_cpu_percent += process.cpu_percent
            sustained.append((process.cpu_percent, process.command))

    sustained.sort(key=lambda item: (-item[0], item[1]))
    return Footprint(
        sustained_cpu_cores=sustained_cpu_percent / 100,
        descendant_cpu_cores=descendant_cpu_percent / 100,
        fleet_cpu_cores=(sustained_cpu_percent + descendant_cpu_percent) / 100,
        descendant_process_count=descendant_process_count,
        direct_process_count=direct_process_count,
        transient_call_count=transient_call_count,
        process_count=process_count,
        rss_gb=rss_kb / (1024 * 1024),
        measured_cpu_cores=measured_cpu_percent / 100,
        top=sustained,
        unparsed_lines=unparsed_lines,
        test_process_count=test_process_count,
        spare_pool_process_count=spare_pool_count,
        spare_pool_cpu_cores=spare_pool_cpu_percent / 100,
        spare_pool_rss_gb=spare_pool_rss_kb / (1024 * 1024),
        machine_process_count=len(processes),
        runnable_count=sum(
            1 for process in processes.values() if process.state.startswith("R")
        ),
    )
