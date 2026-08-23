"""The hidden ``fno doctor footprint`` machine-cost diagnostic."""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any

import typer

from fno.footprint import Footprint, parse_footprint


CPU_THRESHOLD_CORES = 1.0
DAEMON_ALLOWANCE = 1
_PS_COLUMNS = "pid,etime,%cpu,rss,command"


def _fno_binary() -> str:
    return shutil.which("fno") or shutil.which("fno-py") or "fno"


def _read_ps() -> tuple[str | None, str | None]:
    path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w+", encoding="utf-8", prefix=".fno-footprint-", delete=False
        ) as output:
            path = Path(output.name)
            result = subprocess.run(
                ["ps", "-Ao", _PS_COLUMNS],
                stdout=output,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            if result.returncode != 0:
                detail = (result.stderr or "").strip() or f"exit {result.returncode}"
                return None, f"ps unavailable: {detail}"
            output.flush()
            output.seek(0)
            return output.read(), None
    except OSError as exc:
        return None, f"ps unavailable: {exc}"
    finally:
        if path is not None:
            try:
                path.unlink()
            except OSError:
                pass


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
    process_threshold: int,
    exit_code: int,
) -> dict[str, Any]:
    return {
        "sustained_cpu_cores": reading.sustained_cpu_cores,
        "sustained_cpu_threshold_cores": CPU_THRESHOLD_CORES,
        "transient_call_count": reading.transient_call_count,
        "process_count": reading.process_count,
        "process_count_threshold": process_threshold,
        "rss_gb": reading.rss_gb,
        "top": [
            {"cpu_percent": cpu_percent, "command": command}
            for cpu_percent, command in reading.top
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
    process_threshold: int,
    json_output: bool,
) -> None:
    cpu_over = reading.sustained_cpu_cores > CPU_THRESHOLD_CORES
    process_over = reading.process_count > process_threshold
    exit_code = 3 if cpu_over or process_over else 0
    payload = _payload(
        reading,
        process_threshold=process_threshold,
        exit_code=exit_code,
    )
    if json_output:
        typer.echo(json.dumps(payload, sort_keys=True))
    else:
        verdict = "over budget" if exit_code == 3 else "within budget"
        typer.echo(
            f"sustained CPU: {reading.sustained_cpu_cores:.3f} cores "
            f"(threshold {CPU_THRESHOLD_CORES:.3f})"
        )
        typer.echo(
            f"processes: {reading.process_count} "
            f"(threshold {process_threshold})"
        )
        typer.echo(f"transient calls: {reading.transient_call_count}")
        typer.echo(f"verdict: {verdict} (exit {exit_code})")
        if reading.unparsed_lines:
            typer.echo(f"unparsed lines: {reading.unparsed_lines}")
        if exit_code == 3:
            typer.echo("top sustained consumers:")
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
) -> None:
    """Measure fno CPU and process cost without using load average."""
    ps_output, error = _read_ps()
    if error is not None or ps_output is None:
        _emit_failure(error or "footprint unavailable: ps returned no output", json_output=json_output)
        raise typer.Exit(code=4)

    reading = parse_footprint(ps_output)
    if reading.unparsed_lines:
        _emit_failure(
            f"footprint unavailable: {reading.unparsed_lines} ps line(s) could not be parsed",
            json_output=json_output,
        )
        raise typer.Exit(code=4)

    roster_count, error = _roster_count()
    if error is not None or roster_count is None:
        _emit_failure(error or "roster unavailable: no row count", json_output=json_output)
        raise typer.Exit(code=4)

    _emit_result(
        reading,
        process_threshold=roster_count + DAEMON_ALLOWANCE,
        json_output=json_output,
    )
