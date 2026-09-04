"""The hidden ``fno doctor footprint`` machine-cost diagnostic."""

from __future__ import annotations

import json
import math
import os
import subprocess
import tempfile
import time
from functools import lru_cache
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, NoReturn

import typer

from fno.footprint import Footprint, parse_footprint


#: The sustained-CPU threshold derives from measured capacity at this fraction
#: per core, not from the old absolute 1.0 - which on a 12-core M2 Max asked
#: the fleet to idle at 8% utilisation. ``agents.footprint_sustained_cpu_cores``
#: overrides it whole for a small box that wants the pin.
SUSTAINED_CPU_CAPACITY_FRACTION = 0.1
SUSTAINED_CPU_FLOOR_CORES = 0.25
DAEMON_ALLOWANCE = 1
_PS_COLUMNS = "pid,ppid,etime,%cpu,rss,command"
PS_TIMEOUT_SECONDS = 5.0
_NO_LOAD_SNAPSHOT = object()
#: Exit codes. A capacity breach keeps 3 (existing readers depend on it); the
#: leak alarm gets its own code so ``echo $?`` answers WHICH alarm fired. The
#: two verdicts were merged into one exit and a competent reader misread the
#: leak detector as a capacity ceiling repeatedly in one session.
EXIT_CAPACITY_OVER = 3
EXIT_LEAK = 5


@lru_cache(maxsize=8)
def _footprint_cpu_override_at(root: Path) -> float | None:
    """``agents.footprint_sustained_cpu_cores`` read at ``root``, cached per root.

    The settings load resolves config roots, which on a cold cache shells out
    to git - the cause-only path (the spawn gate's hot lane, contractually one
    ps call) must never pay that per call, so the read happens once per root.
    Keyed on the root rather than cached bare: a zero-arg cache is a global
    keyed on nothing, and the first caller would fix the answer for every
    caller after it. Tests substitute the zero-arg wrapper."""
    try:
        from fno.config import load_settings_for_repo

        override = load_settings_for_repo(root).agents.footprint_sustained_cpu_cores
    except Exception:  # noqa: BLE001 - a broken settings value degrades, not dies
        return None
    return float(override) if override is not None and override > 0 else None


def _footprint_cpu_override() -> float | None:
    """The cwd-rooted read; the seam tests and callers substitute."""
    return _footprint_cpu_override_at(Path.cwd())


def sustained_cpu_threshold(capacity_cores: int | None = None) -> float:
    """The sustained-CPU bar, derived from the machine, not hardcoded.

    ``agents.footprint_sustained_cpu_cores`` pins it absolutely; unset, it is a
    fraction of measured CPU capacity with a floor so a one-core box still has
    a meaningful bar."""
    if capacity_cores is None:
        capacity_cores = _cpu_capacity_cores()
    override = _footprint_cpu_override()
    if override is not None:
        return override
    return max(SUSTAINED_CPU_FLOOR_CORES, SUSTAINED_CPU_CAPACITY_FRACTION * capacity_cores)


def _cpu_capacity_cores() -> int:
    candidates: list[int] = []
    process_cpu_count = getattr(os, "process_cpu_count", None)
    if callable(process_cpu_count):
        count = process_cpu_count()
        if count:
            candidates.append(count)
    sched_getaffinity = getattr(os, "sched_getaffinity", None)
    if callable(sched_getaffinity):
        try:
            count = len(sched_getaffinity(0))
            if count:
                candidates.append(count)
        except OSError:
            pass
    host_count = os.cpu_count()
    if host_count:
        candidates.append(host_count)
    quota = _cpu_quota_cores()
    if quota is not None:
        candidates.append(max(1, math.ceil(quota)))
    return min(candidates) if candidates else 1


def _cpu_quota_cores() -> float | None:
    """Read the effective CPU quota for this process's cgroup hierarchy."""
    quotas: list[float] = []
    for mount, relative in _cgroup_mount_paths("cgroup2"):
        for path in _cgroup_ancestors(mount / relative.lstrip("/"), mount):
            try:
                fields = (path / "cpu.max").read_text(encoding="utf-8").split()
                if not fields or fields[0] == "max":
                    continue
                quota, period = int(fields[0]), int(fields[1])
                if quota > 0 and period > 0:
                    quotas.append(quota / period)
            except (OSError, IndexError, TypeError, ValueError):
                continue
    for mount, relative in _cgroup_mount_paths("cgroup"):
        for path in _cgroup_ancestors(mount / relative.lstrip("/"), mount):
            try:
                quota = int(
                    (path / "cpu.cfs_quota_us").read_text(encoding="utf-8").strip()
                )
                period = int(
                    (path / "cpu.cfs_period_us").read_text(encoding="utf-8").strip()
                )
                if quota > 0 and period > 0:
                    quotas.append(quota / period)
            except (OSError, TypeError, ValueError):
                continue
    return min(quotas) if quotas else None


def _cgroup_mount_paths(fs_type: str) -> list[tuple[Path, str]]:
    """Return cgroup mounts paired with this process's relative cgroup path."""
    try:
        groups = Path("/proc/self/cgroup").read_text(encoding="utf-8").splitlines()
        relative = None
        for line in groups:
            parts = line.split(":", 2)
            if len(parts) != 3:
                continue
            hierarchy, controllers, path = parts
            if fs_type == "cgroup2" and hierarchy == "0":
                relative = path
            elif fs_type == "cgroup" and "cpu" in controllers.split(","):
                relative = path
        if relative is None:
            return []
        mounts: list[tuple[Path, str]] = []
        for line in Path("/proc/self/mountinfo").read_text(
            encoding="utf-8"
        ).splitlines():
            before, separator, after = line.partition(" - ")
            if not separator or not after.startswith(f"{fs_type} "):
                continue
            fields = before.split()
            if len(fields) > 4:
                root = fields[3].replace("\\040", " ").replace("\\011", "\t")
                mount = fields[4].replace("\\040", " ").replace("\\011", "\t")
                if fs_type == "cgroup2" or "cpu" in after.split()[2].split(","):
                    if root == "/":
                        relative_mount_path = relative
                    elif relative == root:
                        relative_mount_path = "/"
                    elif relative.startswith(root.rstrip("/") + "/"):
                        relative_mount_path = relative[len(root) :]
                    else:
                        continue
                    mounts.append((Path(mount), relative_mount_path))
        if not mounts:
            mounts.append((Path("/sys/fs/cgroup"), relative))
        return mounts
    except (OSError, ValueError):
        return []


def _cgroup_ancestors(path: Path, mount: Path) -> list[Path]:
    """Return a cgroup directory and its parents up to the mount root."""
    ancestors: list[Path] = []
    current = path
    try:
        current.relative_to(mount)
    except ValueError:
        return ancestors
    while True:
        ancestors.append(current)
        if current == mount or current.parent == current:
            return ancestors
        current = current.parent


def _root_pid_is_live(pid: int, pid_start: int | None) -> bool | None:
    from fno.agents.spawn_gate import _pid_alive, _process_start_time

    if pid == 1:
        # Local to this reader on purpose: _pid_alive rejects pid <= 1 for
        # every caller (signal-safety guard), and widening that shared guard
        # would change liveness semantics fleet-wide. A pid-1 root is proven
        # by incarnation token alone.
        current_start = _process_start_time(pid)
        if current_start is None:
            return None
        return pid_start is None or current_start == pid_start
    return _pid_alive(pid, pid_start)


class AttributionGap:
    """Live worker rows this reading could not attribute to processes.

    Not an error: the machine-level measurement stands, and the gap text
    names what is missing so a reader can tell an undercount from a clean
    reading. A spawn gate must treat a gapped fleet share as unknown, never
    as headroom (x-e040).
    """

    def __init__(self, text: str) -> None:
        self.text = text

    def __repr__(self) -> str:  # pragma: no cover - debug aid
        return f"AttributionGap({self.text!r})"


def _pidless_route(row: Any) -> str | None:
    """Name the route that can resolve this pidless row, or None.

    A route names the MECHANISM it drives, and may legitimately mention the
    harness whose machinery that is. What it must never do is gate a
    capability on a harness name: the predicate callers care about is the
    PROPERTY - "this row carries an identity handle some route accepts" -
    and a new harness gets resolution by adding a route entry here, never by
    editing a hardcoded-name list somewhere else (x-e040, the seventh
    specimen of that family).
    """
    # The claude bg socket farm: an 8-hex jobId resolves through the rv
    # socket map. short_id is that handle.
    if getattr(row, "short_id", None):
        return "bg-socket"
    return None


def _terminal_row_changed_after_snapshot(row: Any, snapshot_at: float) -> bool:
    # Only the exit-transition stamp (`exited_at`) counts: `last_reconciled_at`
    # rotates on every probe, so a CHECKED bump inside the measurement window
    # is indistinguishable from a transition there. Transition stamps are
    # whole-second (`now_rfc3339_like`), so a stamp of T covers [T, T+1); a
    # snapshot inside that window cannot rule out a later transition.
    stamp = getattr(row, "exited_at", None)
    if stamp is None:
        return False
    try:
        timestamp = datetime.fromisoformat(str(stamp).replace("Z", "+00:00"))
        if timestamp.tzinfo is None:
            timestamp = timestamp.replace(tzinfo=timezone.utc)
    except (TypeError, ValueError, OverflowError):
        return True
    resolution = 0.0 if timestamp.microsecond else 1.0
    return timestamp.timestamp() + resolution > snapshot_at


def _live_root_pids(
    *,
    deadline: float | None = None,
    snapshot_pids: set[int] | None = None,
    snapshot_at: float | None = None,
) -> tuple[set[int], str | AttributionGap | None]:
    """Return positively live worker PIDs that may have detached children."""
    roots: set[int] = set()
    try:
        from fno.agents.registry import load_registry
        from fno.agents.session_procs import bg_socket_pid_map
        from fno.agents.spawn_gate import LIVE_STATUSES

        rows = load_registry()
        if not getattr(rows, "complete", True):
            return roots, "worker registry incomplete"
        # A broken import below (a renamed spawn_gate private) is a programming
        # error, not an environment state: swallowing it here would report
        # "worker root discovery unavailable" and misdiagnose every reading.
        for row in rows:
            if row.status not in LIVE_STATUSES:
                if (
                    snapshot_at is not None
                    and row.pid is None
                    and _terminal_row_changed_after_snapshot(row, snapshot_at)
                ):
                    return roots, "worker root liveness unavailable"
                if (
                    snapshot_pids is None
                    or row.pid is None
                    or row.pid not in snapshot_pids
                ):
                    continue
                if row.pid_start_time is None:
                    return roots, "worker root liveness unavailable"
                root_live = _root_pid_is_live(row.pid, row.pid_start_time)
                if root_live is not True:
                    return roots, "worker root liveness unavailable"
                roots.add(row.pid)
                continue
            if row.pid is None:
                continue
            if row.pid_start_time is None:
                return roots, "worker root liveness unavailable"
            root_live = _root_pid_is_live(row.pid, row.pid_start_time)
            if root_live is None:
                return roots, "worker root liveness unavailable"
            if root_live:
                roots.add(row.pid)
            elif snapshot_pids is not None and row.pid in snapshot_pids:
                return roots, "worker root liveness unavailable"
        pidless_rows = [
            row for row in rows if row.status in LIVE_STATUSES and row.pid is None
        ]
        unrouted_rows = [row for row in pidless_rows if _pidless_route(row) is None]
        routed_rows = [row for row in pidless_rows if _pidless_route(row) is not None]
        # A pidless row NO route accepts is an attribution gap, not a dead
        # reading: one such row used to suppress the entire report (x-e040,
        # measured: six pidless codex rows kept the sensor returning
        # "worker root discovery unavailable" at rest). The machine-level
        # measurement stands; the gap names what is unattributed.
        gap_rows: list[str] = [
            f"{len(unrouted_rows)} pidless row(s) with no identity route "
            f"({', '.join(sorted({str(row.harness) for row in unrouted_rows}))})"
        ] if unrouted_rows else []
        if not routed_rows:
            return roots, AttributionGap("; ".join(gap_rows)) if gap_rows else None
        if deadline is not None and time.monotonic() >= deadline:
            gap_rows.append("bg-socket resolution timed out")
            return roots, AttributionGap("; ".join(gap_rows))
        socket_timeout = (
            15.0
            if deadline is None
            else max(0.01, deadline - time.monotonic())
        )
        socket_pids = bg_socket_pid_map(timeout=socket_timeout)
        missing = [row for row in routed_rows if row.short_id not in socket_pids]
        if missing:
            gap_rows.append(
                f"{len(missing)} bg-socket row(s) missing from the socket map"
            )
        for row in routed_rows:
            pid = socket_pids.get(row.short_id)
            if pid is None:
                continue
            root_live = _root_pid_is_live(pid, None)
            if root_live is None:
                return roots, "worker root liveness unavailable"
            if root_live:
                roots.add(pid)
            else:
                return roots, "worker root liveness unavailable"
        return roots, AttributionGap("; ".join(gap_rows)) if gap_rows else None
    except ImportError:
        raise
    except Exception:
        return roots, "worker root discovery unavailable"


def _live_shared_serve_root_pids(
    *, snapshot_pids: set[int] | None = None
) -> tuple[set[int], str | None]:
    """Return the confirmed PID of the detached shared opencode serve."""
    roots: set[int] = set()
    try:
        from fno import paths

        record = json.loads(
            (paths.agents_home_dir() / "opencode-serve.json").read_text(encoding="utf-8")
        )
        if not isinstance(record, dict):
            return roots, "shared serve root discovery unavailable"
        pid = record.get("pid")
        pid_start = record.get("pid_start")
        if (
            not isinstance(pid, int)
            or isinstance(pid, bool)
            or pid <= 0
            or not isinstance(pid_start, int)
            or isinstance(pid_start, bool)
            or pid_start <= 0
        ):
            return roots, "shared serve root liveness unavailable"
        root_live = _root_pid_is_live(pid, pid_start)
        if root_live is None:
            return roots, "shared serve root liveness unavailable"
        if root_live:
            roots.add(pid)
        elif snapshot_pids is not None and pid in snapshot_pids:
            return roots, "shared serve root liveness unavailable"
    except FileNotFoundError:
        return roots, None
    except Exception:
        return roots, "shared serve root discovery unavailable"
    return roots, None


def cause_reading(*, timeout: float = 5.0) -> tuple[Footprint | None, str | None]:
    """One CPU-only fleet reading shared by `--cause-only` and the spawn gate.

    The single producer for the cause pipeline: both the verb and the gate's
    load-refusal evidence read through here, so a hardening fix lands once and
    the two explanations cannot drift. Returns ``(reading, None)`` on success
    or ``(None, reason)``; the caller decides whether a reason is an exit-4 or
    missing evidence.
    """
    deadline = time.monotonic() + timeout
    snapshot_at = time.time()
    ps_output, error = _read_ps(timeout=timeout)
    if error is not None or ps_output is None:
        return None, error or "footprint unavailable: ps returned no output"
    snapshot_pids = _snapshot_pids(ps_output)
    root_pids, root_error = _live_root_pids(
        deadline=deadline, snapshot_pids=snapshot_pids, snapshot_at=snapshot_at
    )
    attribution_gap = None
    if isinstance(root_error, AttributionGap):
        # The reading stands with a named gap; the fleet share above it is an
        # undercount, which the gates must read as unknown (x-e040).
        attribution_gap = root_error.text
        root_error = None
    if root_error is not None:
        return None, f"footprint unavailable: {root_error}"
    shared_serve_pids, shared_serve_error = _live_shared_serve_root_pids(
        snapshot_pids=snapshot_pids
    )
    if shared_serve_error is not None:
        return None, f"footprint unavailable: {shared_serve_error}"
    if (root_pids | shared_serve_pids) - snapshot_pids:
        return None, "footprint unavailable: discovered worker root missing from ps snapshot"
    reading = parse_footprint(
        ps_output,
        excluded_root_pids={os.getpid()},
        attributed_root_pids=root_pids | shared_serve_pids,
        threshold_excluded_root_pids=shared_serve_pids,
    )
    if reading.unparsed_lines:
        return None, (
            f"footprint unavailable: {reading.unparsed_lines} ps line(s) could not be parsed"
        )
    if attribution_gap is not None:
        reading = reading._replace(attribution_gap=attribution_gap)
    return reading, None


def _read_ps(*, timeout: float = PS_TIMEOUT_SECONDS) -> tuple[str | None, str | None]:
    path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            # errors="replace": ps command lines legally carry non-UTF-8 bytes
            # (any process's argv may); one undecodable byte must degrade a
            # command string, not kill the reading with a traceback.
            mode="w+",
            encoding="utf-8",
            errors="replace",
            prefix=".fno-footprint-",
            delete=False,
        ) as output:
            path = Path(output.name)
            run_kwargs: dict[str, Any] = {
                "stdout": output,
                "stderr": subprocess.PIPE,
                "text": True,
                "check": False,
                "timeout": timeout,
            }
            result = subprocess.run(["ps", "-Ao", _PS_COLUMNS], **run_kwargs)
            if result.returncode != 0:
                detail = (result.stderr or "").strip() or f"exit {result.returncode}"
                return None, f"ps unavailable: {detail}"
            output.flush()
            output.seek(0)
            return output.read(), None
    except subprocess.TimeoutExpired:
        return None, f"ps unavailable: timed out after {timeout:.1f}s"
    except OSError as exc:
        return None, f"ps unavailable: {exc}"
    finally:
        if path is not None:
            try:
                path.unlink()
            except OSError:
                pass


def _snapshot_pids(ps_output: str) -> set[int]:
    pids: set[int] = set()
    for line in ps_output.splitlines():
        fields = line.split(None, 1)
        if fields and fields[0].isdigit():
            pids.add(int(fields[0]))
    return pids


def _bounded_json_command(command: str, byte_limit: int) -> str:
    low, high = 0, len(command)
    while low < high:
        middle = (low + high + 1) // 2
        encoded_size = len(json.dumps(command[:middle]).encode("utf-8"))
        if encoded_size <= byte_limit:
            low = middle
        else:
            high = middle - 1
    return command[:low]


def _spawn_load_snapshot():
    from fno.agents.spawn_gate import _load_snapshot
    from fno.config import load_settings

    try:
        max_load_per_cpu = float(load_settings().agents.max_load_per_cpu)
    except Exception:
        # footprint is a reading, not an enforcer: a broken settings value
        # degrades the load fields instead of killing the diagnostic.
        return _load_snapshot(0.0)
    return _load_snapshot(max_load_per_cpu)


def live_registry_rows() -> tuple[list | None, str | None]:
    """The live registry rows, read in process. ONE reader, two consumers.

    ``None`` is a distinct answer from ``[]``: an unreadable registry must not
    read as an empty fleet. Why not a shell-out, and how the count changed:
    docs/architecture/resource-meter.md.
    """
    try:
        from fno.agents.registry import load_registry
        from fno.agents.spawn_gate import LIVE_STATUSES

        rows = load_registry()
    except Exception as exc:
        return None, f"roster unavailable: registry unreadable ({exc})"
    if not getattr(rows, "complete", True):
        # Incomplete is not unreadable: the rows present are real, but a count
        # over them undercounts. Name the registry, not a dead timeout.
        return None, "roster unavailable: worker registry incomplete"
    return [row for row in rows if row.status in LIVE_STATUSES], None


def _roster_count() -> tuple[int | None, str | None]:
    rows, error = live_registry_rows()
    return (None if rows is None else len(rows)), error


def capacity_verdict(load_snapshot: Any) -> str:
    """``within`` | ``near`` | ``over`` | ``unknown`` from the spawn-load
    ceiling - the only reading here derived from hardware.

    ``load_ceiling`` is ``max_load_per_cpu x ncpu``; 1-minute load against it
    is a real ceiling in a way the roster-derived process arithmetic is not.
    An unavailable snapshot is ``unknown``, never a verdict."""
    status = getattr(load_snapshot, "spawn_load_status", "unavailable")
    ceiling = getattr(load_snapshot, "load_ceiling", None)
    load = getattr(load_snapshot, "load_1m", None)
    if status not in ("within", "exceeded") or ceiling is None or load is None:
        return "unknown"
    if status == "exceeded":
        return "over"
    ratio = load / ceiling if ceiling > 0 else 0.0
    if ratio > 1.0:
        return "over"
    if ratio >= 0.9:
        return "near"
    return "within"


def leak_verdict(direct_processes: int, threshold: int | None) -> str:
    """``clean`` | ``unexplained`` | ``unknown`` from the roster arithmetic.

    This is a LEAK detector: processes the roster cannot explain. It is not a
    capacity number and never gates the capacity exit."""
    if threshold is None:
        return "unknown"
    return "unexplained" if direct_processes > threshold else "clean"


def _payload(
    reading: Footprint,
    *,
    process_threshold: int | None,
    exit_code: int,
    top_limit: int | None = None,
    command_limit: int | None = None,
    load_snapshot: Any = _NO_LOAD_SNAPSHOT,
) -> dict[str, Any]:
    cpu_capacity = _cpu_capacity_cores()
    if load_snapshot is _NO_LOAD_SNAPSHOT:
        load_snapshot = _spawn_load_snapshot()
    threshold_cores = sustained_cpu_threshold(cpu_capacity)
    measured_share = (
        reading.fleet_cpu_cores / reading.measured_cpu_cores * 100
        if reading.measured_cpu_cores > 0
        else 0.0
    )
    payload: dict[str, object] = {
        "sustained_cpu_cores": reading.sustained_cpu_cores,
        "descendant_cpu_cores": reading.descendant_cpu_cores,
        "fleet_cpu_cores": reading.fleet_cpu_cores,
        "sustained_cpu_threshold_cores": threshold_cores,
        "fleet_cpu_threshold_cores": threshold_cores,
        "transient_call_count": reading.transient_call_count,
        "process_count": reading.process_count,
        "direct_process_count_threshold": process_threshold,
        "descendant_process_count": reading.descendant_process_count,
        "direct_process_count": reading.direct_process_count,
        "rss_gb": reading.rss_gb,
        "cpu_capacity_cores": cpu_capacity,
        "fleet_percent_capacity": reading.fleet_cpu_cores / cpu_capacity * 100,
        "fleet_percent_measured_cpu": measured_share,
        "leak_verdict": leak_verdict(reading.direct_process_count, process_threshold),
        "capacity_verdict": capacity_verdict(load_snapshot),
        "load_1m": getattr(load_snapshot, "load_1m", None),
        "max_load_per_cpu": getattr(load_snapshot, "max_load_per_cpu", None),
        "load_ceiling": getattr(load_snapshot, "load_ceiling", None),
        "load_cpu_count": getattr(load_snapshot, "load_cpu_count", None),
        "spawn_load_status": getattr(load_snapshot, "spawn_load_status", "unavailable"),
        "top": [
            {
                "cpu_percent": cpu_percent,
                "command": (
                    _bounded_json_command(command, command_limit)
                    if command_limit is not None
                    else command
                ),
            }
            for cpu_percent, command in (
                reading.top[:top_limit] if top_limit is not None else reading.top
            )
        ],
        "unparsed_lines": reading.unparsed_lines,
        "exit_code": exit_code,
    }
    if reading.attribution_gap is not None:
        payload["attribution_gap"] = reading.attribution_gap
    return payload


def _emit_failure(message: str, *, json_output: bool) -> None:
    if json_output:
        typer.echo(json.dumps({"error": message, "exit_code": 4}))
    else:
        typer.echo(message)


def _emit_result(
    reading: Footprint,
    *,
    process_threshold: int | None,
    json_output: bool,
    cause_only: bool = False,
    note: str | None = None,
) -> NoReturn:
    capacity = "unknown"
    leak = "unknown"
    exit_code = 0
    # cause_only keeps passing None (never the sentinel) so _payload does not
    # compute a load snapshot the cause-only contract promises not to spend.
    load_snapshot = None if cause_only else _spawn_load_snapshot()
    if not cause_only:
        leak = leak_verdict(reading.direct_process_count, process_threshold)
        capacity = capacity_verdict(load_snapshot)
        # Capacity keeps the historical exit (callers depend on 3); the leak
        # alarm gets its own code. When BOTH fire, capacity wins the exit as
        # the more urgent alarm and the leak still prints.
        if capacity == "over":
            exit_code = EXIT_CAPACITY_OVER
        elif leak == "unexplained":
            exit_code = EXIT_LEAK
    # A gapped reading answers, but it does not clear the gate: --cause-only
    # keeps exit 4 (the Rust spawn gate takes stdout only on exit 0, so a
    # gapped undercount can never be admitted as headroom without a Rust
    # change). The measurement itself is printed either way.
    if reading.attribution_gap is not None and cause_only:
        exit_code = 4
    top_limit = 5 if cause_only else None
    command_limit = 1024 if cause_only else None
    payload = _payload(
        reading,
        process_threshold=process_threshold,
        exit_code=exit_code,
        top_limit=top_limit,
        command_limit=command_limit,
        load_snapshot=load_snapshot,
    )
    if note is not None:
        payload["degraded"] = note
    if json_output:
        typer.echo(json.dumps(payload, sort_keys=True))
    else:
        threshold_cores = payload["sustained_cpu_threshold_cores"]
        typer.echo(
            f"fleet CPU: {reading.fleet_cpu_cores:.3f} cores "
            f"({payload['fleet_percent_capacity']:.1f}% capacity, "
            f"{payload['fleet_percent_measured_cpu']:.1f}% of measured CPU)"
        )
        load_status = payload["spawn_load_status"]
        if load_status in {"within", "exceeded"}:
            typer.echo(
                f"spawn load: {payload['load_1m']:.1f} against "
                f"{payload['load_ceiling']:.1f} (max_load_per_cpu "
                f"{payload['max_load_per_cpu']:g} x {payload['load_cpu_count']} cpus; "
                f"{load_status})"
            )
        else:
            if payload["max_load_per_cpu"] is None:
                typer.echo(f"spawn load: {load_status} (cause-only snapshot)")
            else:
                typer.echo(
                    f"spawn load: {load_status} (max_load_per_cpu "
                    f"{payload['max_load_per_cpu']:g} x {payload['load_cpu_count']} cpus; "
                    f"{load_status})"
                )
        typer.echo(
            f"sustained CPU: {reading.sustained_cpu_cores:.3f} cores "
            f"(threshold {threshold_cores:.3f} from {payload['cpu_capacity_cores']} cpus)"
        )
        typer.echo(
            f"descendant CPU: {reading.descendant_cpu_cores:.3f} cores "
            f"({reading.descendant_process_count} processes)"
        )
        typer.echo(
            f"processes: {reading.process_count}"
        )
        if process_threshold is not None:
            unexplained = max(0, reading.direct_process_count - process_threshold)
            typer.echo(
                f"unexplained processes: {unexplained} "
                f"({reading.direct_process_count} direct, "
                f"roster explains {process_threshold})"
            )
        else:
            typer.echo(
                f"unexplained processes: unknown "
                f"({reading.direct_process_count} direct, roster unavailable)"
            )
        typer.echo(f"transient calls: {reading.transient_call_count}")
        if reading.attribution_gap is not None:
            typer.echo(
                f"attribution gap: {reading.attribution_gap} "
                "(fleet share is an undercount, not headroom)"
            )
        if exit_code == EXIT_CAPACITY_OVER:
            typer.echo(f"verdict: capacity over (exit {exit_code})")
        elif exit_code == EXIT_LEAK:
            typer.echo(f"verdict: leak (exit {exit_code})")
        else:
            typer.echo(f"verdict: {capacity} (exit {exit_code})")
        if reading.unparsed_lines:
            typer.echo(f"unparsed lines: {reading.unparsed_lines}")
        if note is not None:
            typer.echo(f"degraded: {note}")
        if exit_code != 0 or cause_only:
            typer.echo("top fleet consumers:")
            for cpu_percent, command in reading.top[: top_limit or 5]:
                if command_limit is not None and len(command.encode("utf-8")) > command_limit:
                    command = (
                        command.encode("utf-8")[:command_limit].decode(
                            "utf-8", errors="ignore"
                        )
                        + "..."
                    )
                typer.echo(f"  {command} ({cpu_percent:.1f}%)")
    raise typer.Exit(code=exit_code)


def footprint_command(
    json_output: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit the reading as one JSON object.",
    ),
    cause_only: bool = typer.Option(
        False,
        "--cause-only",
        hidden=True,
        help="Emit a bounded CPU-only reading for spawn-gate diagnosis.",
    ),
) -> None:
    """Measure fno CPU and process cost without using load average."""
    reading, cause_error = cause_reading()
    if cause_error is not None or reading is None:
        _emit_failure(cause_error or "footprint unavailable", json_output=json_output)
        raise typer.Exit(code=4)

    if cause_only:
        _emit_result(
            reading,
            process_threshold=None,
            json_output=json_output,
            cause_only=True,
        )

    roster_count, error = _roster_count()
    if error is not None or roster_count is None:
        # The roster is an ENRICHMENT: it sets the process threshold. On
        # roster failure the measurement still prints, with the threshold
        # degraded away and the reason named - a 5s roster timeout under load
        # is exactly when this reading matters (x-e040). _emit_result raises
        # with its own verdict, so the CPU threshold still applies here.
        _emit_result(
            reading,
            process_threshold=None,
            json_output=json_output,
            note=error or "roster unavailable: no row count",
        )

    _emit_result(
        reading,
        process_threshold=roster_count + DAEMON_ALLOWANCE,
        json_output=json_output,
    )
