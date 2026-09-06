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


class OrphanTestBinary(NamedTuple):
    """One orphaned cargo test binary: alive at ppid 1, a deps/ binary argv[0].

    A test binary whose parent runner died without reaping it reparents to
    init and, wedged, holds its dead children as zombies forever. Confirmation
    (a CACHEDIR.TAG in the owning target dir) is the caller's job: this parser
    never touches the filesystem.
    """

    pid: int
    command: str
    zombies: int
    elapsed_seconds: int


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
    # Orphaned cargo test binaries, worst zombie count first. ppid == 1 plus a
    # deps-binary argv[0] is the candidate shape; always wrong when confirmed.
    orphan_test_binaries: tuple[OrphanTestBinary, ...] = ()


#: argv[0] basenames that are a test runner on their own.
_TEST_RUNNER_NAMES = frozenset({"pytest", "py.test"})

#: argv[0] of a compiled cargo test binary: `<...>/target/<profile>/deps/<crate>-<16 hex>`.
#: This PATH SHAPE alone is a name match - the confirmer in doctor_footprint
#: demands a CACHEDIR.TAG in the owning target dir before anything is named.
_DEPS_BINARY_RE = re.compile(r"/target/(?:debug|release)/deps/[A-Za-z0-9_]+-[0-9a-f]{16}$")

#: macOS renders a dead-unreaped child as exactly this command.
_DEFUNCT_COMMAND = "<defunct>"


def _is_deps_test_binary(command: str) -> bool:
    """True when argv[0] is a compiled cargo test binary path."""
    argv = command.split()
    return bool(argv) and _DEPS_BINARY_RE.search(argv[0]) is not None


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


class _Process(NamedTuple):
    pid: int
    ppid: int | None
    elapsed_seconds: int
    cpu_percent: float
    rss_kb: int
    command: str


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
    """Parse a file-backed ``ps -Ao pid,ppid,etime,%cpu,rss,command`` snapshot.

    The legacy five-column shape is accepted for callers with old fixtures. It
    has no parentage, so only directly attributable rows can be counted.
    """
    processes: dict[int, _Process] = {}
    unparsed_lines = 0
    new_format = False

    for raw_line in ps_output.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        if line.startswith("PID "):
            new_format = len(line.split()) >= 2 and line.split()[1] == "PPID"
            continue
        try:
            fields = line.split(None, 5)
            if new_format:
                if len(fields) != 6:
                    raise ValueError("wrong new-format field count")
                pid = int(fields[0])
                ppid = int(fields[1])
                elapsed = _elapsed_seconds(fields[2])
                cpu_percent = float(fields[3])
                rss = int(fields[4])
                command = fields[5].strip()
            else:
                new_shape = False
                if len(fields) == 6:
                    try:
                        pid = int(fields[0])
                        ppid = int(fields[1])
                        elapsed = _elapsed_seconds(fields[2])
                        cpu_percent = float(fields[3])
                        rss = int(fields[4])
                        command = fields[5].strip()
                        new_shape = True
                    except (TypeError, ValueError):
                        new_shape = False
                if not new_shape:
                    fields = line.split(None, 4)
                    if len(fields) != 5:
                        raise ValueError("wrong field count")
                    pid = int(fields[0])
                    ppid = None
                    elapsed = _elapsed_seconds(fields[1])
                    cpu_percent = float(fields[2])
                    rss = int(fields[3])
                    command = fields[4].strip()
            if pid < 0 or (ppid is not None and ppid < 0) or not command or cpu_percent < 0 or rss < 0:
                raise ValueError("invalid process fields")
        except (TypeError, ValueError):
            unparsed_lines += 1
            continue
        processes[pid] = _Process(pid, ppid, elapsed, cpu_percent, rss, command)

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
    for pid, process in processes.items():
        if not is_excluded(pid):
            measured_cpu_percent += process.cpu_percent
            if is_test_runner(process.command):
                test_process_count += 1
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
    # Orphan candidates, from the same snapshot: a live row at ppid 1 whose
    # argv[0] is a deps test binary, with its dead children counted.
    defunct_by_ppid: dict[int, int] = {}
    for process in processes.values():
        if process.command == _DEFUNCT_COMMAND and process.ppid is not None:
            defunct_by_ppid[process.ppid] = defunct_by_ppid.get(process.ppid, 0) + 1
    orphans = sorted(
        (
            OrphanTestBinary(
                pid=process.pid,
                command=process.command,
                zombies=defunct_by_ppid.get(process.pid, 0),
                elapsed_seconds=process.elapsed_seconds,
            )
            for process in processes.values()
            if process.ppid == 1 and _is_deps_test_binary(process.command)
        ),
        key=lambda orphan: (-orphan.zombies, orphan.pid),
    )
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
        orphan_test_binaries=tuple(orphans),
    )
