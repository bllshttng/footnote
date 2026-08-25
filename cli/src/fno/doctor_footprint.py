"""The hidden ``fno doctor footprint`` machine-cost diagnostic."""

from __future__ import annotations

import json
import math
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

import typer

from fno.footprint import Footprint, parse_footprint


CPU_THRESHOLD_CORES = 1.0
DAEMON_ALLOWANCE = 1
_PS_COLUMNS = "pid,ppid,etime,%cpu,rss,command"
PS_TIMEOUT_SECONDS = 5.0


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


def _fno_binary() -> str:
    return shutil.which("fno") or shutil.which("fno-py") or "fno"


def _root_pid_is_live(pid: int, pid_start: int | None) -> bool | None:
    from fno.agents.spawn_gate import _pid_alive, _process_start_time

    if pid == 1:
        current_start = _process_start_time(pid)
        if current_start is None:
            return None
        return pid_start is None or current_start == pid_start
    return _pid_alive(pid, pid_start)


def _live_root_pids(
    *, deadline: float | None = None, snapshot_pids: set[int] | None = None
) -> tuple[set[int], str | None]:
    """Return positively live worker PIDs that may have detached children."""
    roots: set[int] = set()
    try:
        from fno.agents.registry import load_registry
        from fno.agents.session_procs import bg_socket_pid_map
        from fno.agents.spawn_gate import LIVE_STATUSES

        rows = load_registry()
        if not getattr(rows, "complete", True):
            return roots, "worker registry incomplete"
        for row in rows:
            if row.status not in LIVE_STATUSES or row.pid is None:
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
        unresolved_rows = [
            row
            for row in rows
            if (
                row.status in LIVE_STATUSES
                and row.pid is None
                and not (row.harness == "claude" and row.short_id)
            )
        ]
        if unresolved_rows:
            return roots, "worker root discovery unavailable"
        pidless_claude_rows = [
            row
            for row in rows
            if (
                row.status in LIVE_STATUSES
                and row.pid is None
                and row.harness == "claude"
                and row.short_id
            )
        ]
        if not pidless_claude_rows:
            return roots, None
        if deadline is not None and time.monotonic() >= deadline:
            return roots, "worker root discovery timed out"
        socket_timeout = (
            15.0
            if deadline is None
            else max(0.01, deadline - time.monotonic())
        )
        socket_pids = bg_socket_pid_map(timeout=socket_timeout)
        missing = [row.short_id for row in pidless_claude_rows if row.short_id not in socket_pids]
        if missing:
            return roots, "worker root discovery unavailable"
        for row in pidless_claude_rows:
            pid = socket_pids[row.short_id]
            root_live = _root_pid_is_live(pid, None)
            if root_live is None:
                return roots, "worker root liveness unavailable"
            if root_live:
                roots.add(pid)
            else:
                return roots, "worker root liveness unavailable"
    except Exception:
        return roots, "worker root discovery unavailable"
    return roots, None


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


def _read_ps(*, timeout: float = PS_TIMEOUT_SECONDS) -> tuple[str | None, str | None]:
    path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w+", encoding="utf-8", prefix=".fno-footprint-", delete=False
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


def _roster_count() -> tuple[int | None, str | None]:
    result = subprocess.run(
        [_fno_binary(), "agents", "list", "--status", "live", "--json"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or "").strip() or f"exit {result.returncode}"
        return None, f"roster unavailable: {detail}"
    try:
        payload = json.loads(result.stdout)
    except (TypeError, json.JSONDecodeError) as exc:
        return None, f"roster unavailable: invalid JSON ({exc})"
    if isinstance(payload, list):
        return len(payload), None
    if isinstance(payload, dict):
        for key in ("agents", "rows", "workers", "items"):
            rows = payload.get(key)
            if isinstance(rows, list):
                return len(rows), None
    return None, "roster unavailable: JSON did not contain a row list"


def _payload(
    reading: Footprint,
    *,
    process_threshold: int | None,
    exit_code: int,
    top_limit: int | None = None,
    command_limit: int | None = None,
) -> dict[str, Any]:
    cpu_capacity = _cpu_capacity_cores()
    measured_share = (
        reading.fleet_cpu_cores / reading.measured_cpu_cores * 100
        if reading.measured_cpu_cores > 0
        else 0.0
    )
    return {
        "sustained_cpu_cores": reading.sustained_cpu_cores,
        "descendant_cpu_cores": reading.descendant_cpu_cores,
        "fleet_cpu_cores": reading.fleet_cpu_cores,
        "sustained_cpu_threshold_cores": CPU_THRESHOLD_CORES,
        "fleet_cpu_threshold_cores": CPU_THRESHOLD_CORES,
        "transient_call_count": reading.transient_call_count,
        "process_count": reading.process_count,
        "process_count_threshold": process_threshold,
        "descendant_process_count": reading.descendant_process_count,
        "direct_process_count": reading.direct_process_count,
        "rss_gb": reading.rss_gb,
        "cpu_capacity_cores": cpu_capacity,
        "fleet_percent_capacity": reading.fleet_cpu_cores / cpu_capacity * 100,
        "fleet_percent_measured_cpu": measured_share,
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
) -> None:
    cpu_over = reading.fleet_cpu_cores > CPU_THRESHOLD_CORES
    process_over = (
        process_threshold is not None
        and reading.direct_process_count > process_threshold
    )
    exit_code = 0 if cause_only else (3 if cpu_over or process_over else 0)
    payload = _payload(
        reading,
        process_threshold=process_threshold,
        exit_code=exit_code,
        top_limit=5 if cause_only else None,
        command_limit=1024 if cause_only else None,
    )
    if json_output:
        typer.echo(json.dumps(payload, sort_keys=True))
    else:
        verdict = "over budget" if exit_code == 3 else "within budget"
        typer.echo(
            f"fleet CPU: {reading.fleet_cpu_cores:.3f} cores "
            f"({payload['fleet_percent_capacity']:.1f}% capacity, "
            f"{payload['fleet_percent_measured_cpu']:.1f}% of measured CPU)"
        )
        typer.echo(
            f"sustained CPU: {reading.sustained_cpu_cores:.3f} cores "
            f"(threshold {CPU_THRESHOLD_CORES:.3f})"
        )
        typer.echo(
            f"descendant CPU: {reading.descendant_cpu_cores:.3f} cores "
            f"({reading.descendant_process_count} processes)"
        )
        typer.echo(
            f"processes: {reading.process_count}"
        )
        typer.echo(
            f"direct processes: {reading.direct_process_count} "
            f"(threshold {process_threshold if process_threshold is not None else 'n/a'})"
        )
        typer.echo(f"transient calls: {reading.transient_call_count}")
        typer.echo(f"verdict: {verdict} (exit {exit_code})")
        if reading.unparsed_lines:
            typer.echo(f"unparsed lines: {reading.unparsed_lines}")
        if exit_code == 3 or cause_only:
            typer.echo("top fleet consumers:")
            for cpu_percent, command in reading.top[:5]:
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
    ps_output, error = _read_ps()
    if error is not None or ps_output is None:
        _emit_failure(error or "footprint unavailable: ps returned no output", json_output=json_output)
        raise typer.Exit(code=4)

    snapshot_pids = _snapshot_pids(ps_output)
    root_pids, root_error = _live_root_pids(snapshot_pids=snapshot_pids)
    if root_error is not None:
        _emit_failure(f"footprint unavailable: {root_error}", json_output=json_output)
        raise typer.Exit(code=4)
    shared_serve_pids, shared_serve_error = _live_shared_serve_root_pids(
        snapshot_pids=snapshot_pids
    )
    if shared_serve_error is not None:
        _emit_failure(
            f"footprint unavailable: {shared_serve_error}",
            json_output=json_output,
        )
        raise typer.Exit(code=4)
    if (root_pids | shared_serve_pids) - snapshot_pids:
        _emit_failure(
            "footprint unavailable: discovered worker root missing from ps snapshot",
            json_output=json_output,
        )
        raise typer.Exit(code=4)
    reading = parse_footprint(
        ps_output,
        excluded_root_pids={os.getpid()},
        attributed_root_pids=root_pids | shared_serve_pids,
        threshold_excluded_root_pids=shared_serve_pids,
    )
    if reading.unparsed_lines:
        _emit_failure(
            f"footprint unavailable: {reading.unparsed_lines} ps line(s) could not be parsed",
            json_output=json_output,
        )
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
        _emit_failure(error or "roster unavailable: no row count", json_output=json_output)
        raise typer.Exit(code=4)

    _emit_result(
        reading,
        process_threshold=roster_count + DAEMON_ALLOWANCE,
        json_output=json_output,
    )
