"""Pure parsing for the fleet's machine-footprint reading.

The parser receives one file-backed ``ps`` snapshot and never runs ``ps``
itself. A row is fleet-attributable only when its command's executable is
``fno``, ``fno-py``, ``fno-agents``, ``fno-agents-daemon`` or
``fno-agents-worker``. Python-launched ``fno-py`` is also attributable. Worker
sessions such as ``claude`` are deliberately outside this overhead measurement.
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
    transient_call_count: int
    process_count: int
    rss_gb: float
    top: list[tuple[float, str]]
    unparsed_lines: int


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
) -> Footprint:
    """Parse a file-backed ``ps -Ao pid,etime,%cpu,rss,command`` snapshot."""
    sustained_cpu_percent = 0.0
    transient_call_count = 0
    process_count = 0
    rss_kb = 0
    sustained: list[tuple[float, str]] = []
    unparsed_lines = 0

    for raw_line in ps_output.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("PID "):
            continue
        fields = line.split(None, 4)
        if len(fields) != 5:
            unparsed_lines += 1
            continue
        try:
            int(fields[0])
            elapsed = _elapsed_seconds(fields[1])
            cpu_percent = float(fields[2])
            rss = int(fields[3])
            command = fields[4].strip()
            if not command or cpu_percent < 0 or rss < 0:
                raise ValueError("invalid process fields")
        except (TypeError, ValueError):
            unparsed_lines += 1
            continue

        if not _attributed_command(command):
            continue

        process_count += 1
        rss_kb += rss
        if elapsed < sustained_floor_seconds:
            transient_call_count += 1
        else:
            sustained_cpu_percent += cpu_percent
            sustained.append((cpu_percent, command))

    sustained.sort(key=lambda item: (-item[0], item[1]))
    return Footprint(
        sustained_cpu_cores=sustained_cpu_percent / 100,
        transient_call_count=transient_call_count,
        process_count=process_count,
        rss_gb=rss_kb / (1024 * 1024),
        top=sustained,
        unparsed_lines=unparsed_lines,
    )
