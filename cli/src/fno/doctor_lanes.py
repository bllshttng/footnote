"""The hidden ``fno doctor lanes`` advisor: how many more lanes fit.

One question, one number - or an honest refusal. The answer reads the WHOLE
machine, not just the fleet: ``fno doctor footprint`` attributes fno's own
descendants and is correct for its question, but the browser and every other
app compete for the same box, and a lane answer built on fno-only load keeps
saying "room for more" while something else eats the machine.

Two readings this module exists to get right, both measured on the machine
this was specified against (2026-09-01):

**A pinned-at-zero signal is not headroom.** ``swap_usage`` read 0 while RAM
held 81.5 of 103 GB - because the machine had NO swap file at all
(``sysctl vm.swapusage`` confirmed 0.00M total). So the memory arm reads
``swap_usage`` only when ``swap_total > 0``; when there is no swap file the
reading comes from ``memory_pressure``, which is on every Mac, needs no sudo,
and reports a free percentage plus live compressor activity. When neither
answers, the arm is DARK.

**A dark sensor is never headroom.** Every arm reports ``measured`` or
``dark`` with a reason, and the lane number is printed ONLY when both
resource arms (whole-machine CPU and memory) measured. Anything else refuses
by name rather than guessing from the arms that survived.

The per-lane cost is measured, not assumed: the fleet's own attributed CPU
and RSS (footprint's descendant attribution) divided by the live roster, with
the observed resident sizes from this node as the fallback seed when no rows
are live.
"""

from __future__ import annotations

import json
import re
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

import typer

#: One macmon sample, then the pipe is closed - this is a one-shot read, not
#: a subscription. The tool streams forever, so the timeout is the NORMAL
#: exit path here, and a sample that never arrived inside it is a DARK arm.
MACMON_TIMEOUT_S = 5.0
MEMORY_PRESSURE_TIMEOUT_S = 5.0

#: The lane answer's sanity cap. Per-lane costs are small; without a cap a
#: quiet machine advertises thousands of lanes, which is a number nobody can
#: act on and nobody should read as a promise.
LANE_ANSWER_CAP = 64

#: Per-lane seed costs (cores, GB) used when no live roster row exists to
#: measure from. Derived from the observed resident sizes on the specifying
#: node: a keeper about 6.5 MB, fno-agents-daemon about 105 MB, the mux
#: server about 125 MB, a claude bg pty-host about 95 MB.
SEED_PER_LANE_CORES = 0.1
SEED_PER_LANE_GB = 0.35

MEASURED = "measured"
DARK = "dark"


@dataclass
class ArmReading:
    """One arm of the lane answer: what it read, or why it could not."""

    name: str
    state: str  # measured | dark
    value: Any = None
    source: str = ""
    reason: str = ""  # named only when dark

    def to_json(self) -> dict:
        return {
            "name": self.name,
            "state": self.state,
            "value": self.value,
            "source": self.source,
            "reason": self.reason,
        }


@dataclass
class LaneReading:
    arms: list[ArmReading] = field(default_factory=list)
    lane_count: Optional[int] = None
    per_lane_cpu_cores: Optional[float] = None
    per_lane_mem_gb: Optional[float] = None
    cost_source: str = ""
    refusal_reason: str = ""
    census: dict = field(default_factory=dict)

    @property
    def refused(self) -> bool:
        return self.lane_count is None

    def arm(self, name: str) -> Optional[ArmReading]:
        return next((a for a in self.arms if a.name == name), None)

    def to_json(self) -> dict:
        return {
            "lane_count": self.lane_count,
            "per_lane_cpu_cores": self.per_lane_cpu_cores,
            "per_lane_mem_gb": self.per_lane_mem_gb,
            "cost_source": self.cost_source,
            "refused_reason": self.refusal_reason,
            "census": self.census,
            "arms": [a.to_json() for a in self.arms],
        }


def _first_json_line(raw: bytes | str) -> Optional[dict]:
    if isinstance(raw, bytes):
        raw = raw.decode("utf-8", errors="replace")
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            return value
    return None


def read_macmon(
    timeout: float = MACMON_TIMEOUT_S, macmon_path: Optional[str] = None
) -> tuple[Optional[dict], Optional[str]]:
    """One ``macmon pipe -s 1`` sample, bounded, or a named darkness.

    macmon streams one JSON object per interval and never exits on its own,
    so subprocess.run always ends at the timeout; ``TimeoutExpired.stdout``
    carries what the pipe produced, and the first complete JSON object is the
    sample. A missing binary, an empty pipe, and unparseable output are all
    darkness - never an assumption of headroom."""
    binary = macmon_path or shutil.which("macmon")
    if not binary:
        return None, "macmon not on PATH (brew install macmon; Apple Silicon only)"
    try:
        result = subprocess.run(
            [binary, "pipe", "-s", "1"],
            capture_output=True,
            timeout=timeout,
            check=False,
        )
        sample = _first_json_line(result.stdout or b"")
    except subprocess.TimeoutExpired as exc:
        sample = _first_json_line(exc.stdout or b"")
    except OSError as exc:
        return None, f"macmon failed: {exc}"
    if sample is None:
        return None, f"macmon produced no sample within {timeout:.0f}s"
    return sample, None


_FREE_PCT_RE = re.compile(r"System-wide memory free percentage:\s*(\d+)%")


def read_memory_pressure(
    timeout: float = MEMORY_PRESSURE_TIMEOUT_S,
) -> tuple[Optional[float], Optional[str]]:
    """The system free-memory percentage (0..1) from ``memory_pressure``.

    The fallback arm for a machine with no swap file: it is present on every
    Mac, needs no sudo, and is the one reading that noticed the 81.5-of-103 GB
    machine the swap-only rule would have called infinite headroom."""
    try:
        result = subprocess.run(
            ["memory_pressure"], capture_output=True, timeout=timeout, check=False
        )
        raw = result.stdout or b""
    except subprocess.TimeoutExpired as exc:
        raw = exc.stdout or b""
    except (OSError, subprocess.SubprocessError) as exc:
        return None, f"memory_pressure failed: {exc}"
    text = raw.decode("utf-8", errors="replace") if isinstance(raw, bytes) else str(raw)
    match = _FREE_PCT_RE.search(text)
    if match is None:
        return None, "memory_pressure produced no free-percentage line"
    return int(match.group(1)) / 100.0, None


def _spawn_load_arm() -> ArmReading:
    from fno.doctor_footprint import _spawn_load_snapshot

    snapshot = _spawn_load_snapshot()
    load = getattr(snapshot, "load_1m", None)
    ceiling = getattr(snapshot, "load_ceiling", None)
    status = getattr(snapshot, "spawn_load_status", "unavailable")
    if load is None or ceiling is None:
        return ArmReading("spawn load", DARK, reason="load snapshot unavailable")
    load_5m = getattr(snapshot, "load_5m", None)
    load_15m = getattr(snapshot, "load_15m", None)
    return ArmReading(
        "spawn load",
        MEASURED,
        value={
            "load_1m": round(load, 1),
            "load_5m": round(load_5m, 1) if load_5m is not None else None,
            "load_15m": round(load_15m, 1) if load_15m is not None else None,
            "ceiling": round(ceiling, 1),
            "max_load_per_cpu": round(getattr(snapshot, "max_load_per_cpu", 0.0), 1),
            "load_cpu_count": getattr(snapshot, "load_cpu_count", None),
            "status": status,
        },
        source="max_load_per_cpu x ncpu",
    )


def _machine_cpu_arm(sample: Optional[dict], reason: Optional[str]) -> ArmReading:
    if sample is None:
        return ArmReading("whole-machine cpu", DARK, reason=reason or "macmon dark")
    pct = sample.get("cpu_usage_pct")
    if not isinstance(pct, (int, float)):
        return ArmReading("whole-machine cpu", DARK, reason="macmon cpu_usage_pct unreadable")
    from fno.doctor_footprint import _cpu_capacity_cores

    # macmon's measured contract is a 0-1 fraction; a value above 1.0 is the
    # sensor lying, not a percent spelling to rescue.
    busy = float(pct)
    return ArmReading(
        "whole-machine cpu",
        MEASURED,
        value={"busy_fraction": round(busy, 3), "capacity_cores": _cpu_capacity_cores()},
        source="macmon cpu_usage_pct",
    )


def _memory_arm(sample: Optional[dict], macmon_reason: Optional[str]) -> ArmReading:
    mem = (sample or {}).get("memory") or {}
    total = mem.get("ram_total")
    usage = mem.get("ram_usage")
    swap_total = mem.get("swap_total")
    swap_usage = mem.get("swap_usage")
    if (
        isinstance(total, (int, float))
        and isinstance(usage, (int, float))
        and isinstance(swap_total, (int, float))
        and isinstance(swap_usage, (int, float))
    ):
        if swap_total > 0:
            available = (total - usage) + (swap_total - swap_usage)
            return ArmReading(
                "memory",
                MEASURED,
                value={
                    "available_gb": round(max(0.0, available) / 1e9, 1),
                    "swap_used_gb": round(swap_usage / 1e9, 1),
                },
                source="macmon swap_usage (swap_total > 0)",
            )
    # No swap file (or an unreadable memory block): swap_usage is pinned at
    # zero by CONFIGURATION there, which proves nothing. The reading falls to
    # memory_pressure, and zero swap is never read as headroom.
    free_pct, reason = read_memory_pressure()
    if free_pct is not None:
        available_gb = None
        if isinstance(total, (int, float)) and total > 0:
            available_gb = round(free_pct * total / 1e9, 1)
        return ArmReading(
            "memory",
            MEASURED,
            value={"free_fraction": round(free_pct, 2), "available_gb": available_gb},
            source="memory_pressure (swap_total is 0 - no swap file)"
            if swap_total == 0
            else "memory_pressure (macmon memory block unreadable)",
        )
    return ArmReading(
        "memory",
        DARK,
        reason=(
            f"no swap file and memory_pressure unreadable: {reason}"
            if swap_total == 0
            else f"macmon memory unreadable ({macmon_reason or 'memory block unreadable'})"
                 f" and memory_pressure unreadable: {reason}"
        ),
    )


def _power_arm(sample: Optional[dict], reason: Optional[str]) -> ArmReading:
    if sample is None:
        return ArmReading("power and thermals", DARK, reason=reason or "macmon dark")
    power = sample.get("sys_power")
    temps = (sample.get("temp") or {}).get("cpu_temp_avg")
    if not isinstance(power, (int, float)) and not isinstance(temps, (int, float)):
        return ArmReading("power and thermals", DARK, reason="macmon temp/power unreadable")
    return ArmReading(
        "power and thermals",
        MEASURED,
        value={
            "sys_power_w": round(power, 1) if isinstance(power, (int, float)) else None,
            "cpu_temp_c": round(temps, 1) if isinstance(temps, (int, float)) else None,
        },
        source="macmon sys_power + temp.cpu_temp_avg",
    )


def _fleet_snapshot() -> tuple[Any, Optional[list], Optional[str], int]:
    """One footprint reading, the live rows, why they are missing, and how long
    both took. The rows come from the reader the leak threshold uses, so the
    two can never describe different fleets."""
    from fno.doctor_footprint import cause_reading, live_registry_rows

    started = time.monotonic()
    reading, error = cause_reading()
    rows, rows_error = live_registry_rows()
    read_ms = int((time.monotonic() - started) * 1000)
    return (None if error is not None else reading), rows, rows_error, read_ms


def _fleet_cost(reading: Any, rows: Optional[list]) -> tuple[float, float, int, str]:
    """Per-lane CPU cores and GB, measured from the fleet's own attributed
    footprint over the live roster; the documented seed when no row is live."""
    count = 0 if reading is None else len(rows or ())
    if not count:
        why = "footprint unavailable" if reading is None else "no live roster rows to measure"
        return SEED_PER_LANE_CORES, SEED_PER_LANE_GB, 0, f"seed ({why})"
    per_cpu = max(0.01, reading.fleet_cpu_cores / count)
    per_gb = max(0.01, reading.rss_gb / count)
    return per_cpu, per_gb, count, "measured from the live roster's attributed footprint"


def _census(
    reading: Any, rows: Optional[list], rows_error: Optional[str], read_ms: int
) -> dict:
    """The court census: kings, workers, tests.

    Kings and workers are ROW counts, tests is a PROCESS count, never folded
    together. The gap rides as its own field for the same reason (x-e040).
    Full rule: docs/architecture/resource-meter.md.
    """
    census: dict[str, Any] = {
        "kings": None, "king_conflicts": None, "workers": None,
        "tests": None if reading is None else reading.test_process_count,
        "roster_rows": None if rows is None else len(rows),
        "attribution_gap": None if reading is None else reading.attribution_gap,
        # Top fleet consumers by program name, aggregated from the ps read
        # this census already sits on. The panel renders them in its expanded
        # view; a dark footprint leaves them None rather than fabricating a
        # list.
        "top_consumers": None,
        # The calling session's own share reading (x-5283), from the ONE
        # function the spawn gate refuses on - this number and the gate's
        # number cannot drift. None when it cannot be read.
        "share": _share_reading(),
        # An unread count NAMES why, the rule every arm follows: without it
        # "unknown rows" cannot be told from an incomplete registry.
        "roster_error": rows_error,
        "read_ms": read_ms,
    }
    if reading is not None:
        from fno.footprint import top_consumers

        census["top_consumers"] = top_consumers(reading.top)
    try:
        from fno.agents.court import gather_court

        court = gather_court(rows) if rows is not None else {}
    except Exception:
        return census
    kings = (court.get("summary") or {}).get("total")
    if not isinstance(kings, int):
        # An unreadable court nulls the crown counts rather than reporting a
        # kingless fleet from a read that saw nothing.
        return census
    conflicts = court.get("conflicts")
    census["kings"] = kings
    census["king_conflicts"] = len(conflicts) if isinstance(conflicts, list) else None
    census["workers"] = len(rows or ()) - kings
    return census


def _share_reading() -> Optional[dict[str, Any]]:
    """The calling session's share reading, from spawn_gate.share_reading.

    The gate's divisor arithmetic and this panel's number are one function
    (x-5283 AC3). None on any failure: the panel renders unknown, never a
    fabricated zero.
    """
    try:
        from fno.agents.spawn_gate import census as gate_census, share_reading
        from fno.claims.self_identity import resolve_self_identity
        from fno.config import load_settings

        caller = resolve_self_identity().session_id
        cap = int(load_settings().agents.max_live)
        return share_reading(gate_census(), cap, caller)
    except Exception:
        return None


def read_lanes(
    *,
    macmon_fn: Optional[Callable[..., tuple[Optional[dict], Optional[str]]]] = None,
) -> LaneReading:
    """Assemble every arm, then answer or refuse.

    The refusal rule: the lane number needs BOTH resource arms (whole-machine
    CPU and memory) measured. A dark arm is named with its reason; the arms
    that still work are named too, so a reader never guesses which half they
    are looking at. ``macmon_fn`` defaults to :func:`read_macmon` resolved at
    CALL time - a def-time bind would freeze a test's monkeypatch out."""
    macmon_fn = macmon_fn or read_macmon
    sample, macmon_reason = macmon_fn()
    reading = LaneReading()
    load_arm = _spawn_load_arm()
    cpu_arm = _machine_cpu_arm(sample, macmon_reason)
    mem_arm = _memory_arm(sample, macmon_reason)
    power_arm = _power_arm(sample, macmon_reason)
    reading.arms.extend([load_arm, cpu_arm, mem_arm, power_arm])

    # One fleet read serves both the census and the per-lane divisor, taken
    # BEFORE the refusal branch: a refused lane number is exactly when a
    # person most wants to see what the machine is holding.
    footprint, rows, rows_error, read_ms = _fleet_snapshot()
    reading.census = _census(footprint, rows, rows_error, read_ms)

    dark = [a for a in reading.arms if a.state == DARK]
    if cpu_arm.state == DARK or mem_arm.state == DARK:
        working = [a.name for a in reading.arms if a.state == MEASURED]
        reading.refusal_reason = (
            "the machine arms cannot answer the lane question: "
            + "; ".join(f"{a.name} dark ({a.reason})" for a in dark)
            + (f" | still working: {', '.join(working)}" if working else "")
        )
        return reading

    assert cpu_arm.value is not None and mem_arm.value is not None  # measured arms
    per_cpu, per_gb, row_count, cost_source = _fleet_cost(footprint, rows)
    busy = cpu_arm.value["busy_fraction"]
    capacity = cpu_arm.value["capacity_cores"]
    free_cores = capacity * (1.0 - busy)
    mem_value = mem_arm.value
    available_gb = mem_value.get("available_gb")
    if available_gb is None and mem_value.get("free_fraction") is not None:
        # memory_pressure without a macmon total: the seed cost needs a GB
        # figure, and a fraction alone cannot give one. Refusing here would
        # contradict the arm's own "measured" state, so the seed GB total
        # (16 GB) bounds the estimate and the output says so.
        available_gb = round(mem_value["free_fraction"] * 16.0, 1)
        cost_source = "seed 16 GB total x memory_pressure free fraction"
    cpu_fits = free_cores / per_cpu
    mem_fits = available_gb / per_gb
    answer = int(max(0.0, min(cpu_fits, mem_fits, float(LANE_ANSWER_CAP))))

    if (
        isinstance(load_arm.value, dict)
        and load_arm.value.get("status") == "exceeded"
    ):
        # The spawn-load ceiling is already breached: no advisory headroom on
        # top of a breached ceiling.
        answer = 0
        cost_source += "; spawn-load ceiling already breached, answer capped at 0"

    reading.lane_count = answer
    reading.per_lane_cpu_cores = round(per_cpu, 3)
    reading.per_lane_mem_gb = round(per_gb, 3)
    reading.cost_source = (
        f"{cost_source} ({row_count} live row(s))" if row_count else cost_source
    )
    return reading


def _census_lines(census: dict) -> list[str]:
    """Rendered so an unread count says so: a count the reader can act on and
    a count nobody took must never look the same."""
    if not census:
        return []
    n = {k: ("unknown" if v is None else v) for k, v in census.items()}
    lines = [
        f"  court: {n['kings']} king(s), {n['workers']} worker(s), "
        f"{n['tests']} running test(s) "
        f"({n['roster_rows']} live roster row(s), {n['read_ms']} ms)"
    ]
    if census.get("king_conflicts"):
        lines.append(
            f"  court conflicts: {n['king_conflicts']} scope(s) held by more than "
            "one live crown - a bare king count hides this"
        )
    if census.get("roster_error"):
        lines.append(f"  roster: {n['roster_error']} - the counts above are unread")
    if census.get("attribution_gap"):
        lines.append(
            f"  attribution gap: {n['attribution_gap']} - the fleet CPU share is "
            "an undercount, not headroom"
        )
    return lines


def render(reading: LaneReading) -> str:
    """The answer and every arm's reading, or the refusal naming everything."""
    lines: list[str] = []
    if reading.refused:
        lines.append("lanes: REFUSED - no lane number is printed because a dark "
                     "sensor is not headroom")
        for arm in reading.arms:
            if arm.state == DARK:
                lines.append(f"  {arm.name}: dark - {arm.reason}")
            else:
                lines.append(f"  {arm.name}: measured ({arm.source})")
        lines.extend(_census_lines(reading.census))
        return "\n".join(lines)
    lines.append(f"lanes: {reading.lane_count} more fit")
    for arm in reading.arms:
        lines.append(f"  {arm.name}: {arm.value} ({arm.source})")
    lines.append(
        f"  per-lane cost: {reading.per_lane_cpu_cores} cores, "
        f"{reading.per_lane_mem_gb} GB - {reading.cost_source}"
    )
    lines.extend(_census_lines(reading.census))
    return "\n".join(lines)


def lanes_command(
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the reading as one JSON object."
    ),
) -> None:
    """How many more lanes the machine can take, or an honest refusal.

    Reads whole-machine CPU, memory pressure and power from macmon when it is
    available, falls back to memory_pressure for memory on a machine with no
    swap file, and refuses to print a lane number while a resource arm is
    dark: a missing sensor is named, never treated as headroom."""
    reading = read_lanes()
    if json_output:
        typer.echo(json.dumps(reading.to_json(), sort_keys=True))
    else:
        typer.echo(render(reading))
    if reading.refused:
        raise typer.Exit(code=3)
    raise typer.Exit(code=0)
