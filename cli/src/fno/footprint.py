"""Pure parsing for the fleet's machine-footprint reading.

The parser receives one file-backed ``ps`` snapshot and never runs ``ps``
itself. A row is fleet-attributable when its command is a directly attributable
fno process or when its parent chain reaches one. Worker sessions such as
``claude`` remain outside this overhead measurement unless they are descendants
of an attributable fno process.
"""

from __future__ import annotations

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


class _Process(NamedTuple):
    pid: int
    ppid: int | None
    elapsed_seconds: int
    cpu_percent: float
    rss_kb: int
    command: str


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
    direct = {
        pid: pid in attributed_roots or _attributed_command(process.command)
        for pid, process in processes.items()
    }
    excluded_cache: dict[int, bool] = {}

    def is_excluded(pid: int) -> bool:
        cached = excluded_cache.get(pid)
        if cached is not None:
            return cached
        path: list[int] = []
        seen: set[int] = set()
        current: int | None = pid
        result = False
        while current is not None and current not in (0, 1) and current not in seen:
            if current in excluded:
                result = True
                break
            seen.add(current)
            path.append(current)
            process = processes.get(current)
            current = process.ppid if process is not None else None
        for path_pid in path:
            excluded_cache[path_pid] = result
        return result

    attributed_cache: dict[int, bool] = {}

    def is_attributed(pid: int) -> bool:
        if is_excluded(pid):
            return False
        cached = attributed_cache.get(pid)
        if cached is not None:
            return cached
        path: list[int] = []
        seen: set[int] = set()
        current: int | None = pid
        result = False
        while current is not None and current not in (0, 1) and current not in seen:
            cached = attributed_cache.get(current)
            if cached is not None:
                result = cached
                break
            seen.add(current)
            path.append(current)
            if direct.get(current, False):
                result = True
                break
            process = processes.get(current)
            current = process.ppid if process is not None else None
        for path_pid in path:
            attributed_cache[path_pid] = result
        return result

    sustained_cpu_percent = 0.0
    descendant_cpu_percent = 0.0
    transient_call_count = 0
    descendant_process_count = 0
    direct_process_count = 0
    process_count = 0
    rss_kb = 0
    measured_cpu_percent = 0.0
    sustained: list[tuple[float, str]] = []
    for pid, process in processes.items():
        if not is_excluded(pid):
            measured_cpu_percent += process.cpu_percent
        if not is_attributed(pid):
            continue

        process_count += 1
        rss_kb += process.rss_kb
        if not direct[pid]:
            descendant_process_count += 1
            descendant_cpu_percent += process.cpu_percent
            if process.cpu_percent:
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
    )
