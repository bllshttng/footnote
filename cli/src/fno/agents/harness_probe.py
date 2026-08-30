"""Executable support rubric for a real harness lane.

The first six lines observe the worker and its harness. They do not consult the
capability declaration. The seventh line is the only table-facing line: it
checks that declarations and registrations agree. The eighth line proves that
the readiness manifest was captured from a live pane.
"""

from __future__ import annotations

import json
import re
import subprocess
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Literal, Sequence

import typer

Status = Literal["pass", "fail", "skip"]
PROBE_TIMEOUT_S = 5.0


@dataclass(frozen=True)
class LineVerdict:
    line: str
    status: Status
    marker: str
    attempts: int = 1
    detail: str = ""

    def __post_init__(self) -> None:
        if self.status not in {"pass", "fail", "skip"}:
            raise ValueError(f"unknown status {self.status!r}")
        if self.status in {"pass", "fail"} and not self.marker.strip():
            raise ValueError("pass/fail verdict requires a positive marker")
        if self.attempts < 1:
            raise ValueError("attempts must be positive")


def retry_marker(
    *,
    line: str,
    marker_name: str,
    read_marker: Callable[[], str],
    matches: Callable[[str], bool],
) -> LineVerdict:
    """Read a positive marker three times when the result is not positive."""
    last = ""
    for attempt in range(1, 4):
        last = read_marker()
        if matches(last):
            return LineVerdict(line, "pass", marker_name, attempt, last)
    return LineVerdict(
        line,
        "fail",
        marker_name,
        3,
        f"positive marker not observed; last read={last!r}",
    )


def line_spawn(
    *, receipt_output: str, registry_row: object, gate_output: str
) -> LineVerdict:
    if registry_row is not None:
        return LineVerdict("SPAWN", "pass", "registry row", detail="row read back")
    detail = "registry row was not found"
    gates = [needle for needle in ("Workspace Trust Required", "consent", "login") if needle.lower() in gate_output.lower()]
    if gates:
        detail += f"; gate={gates[0]}"
    if receipt_output:
        detail += f"; receipt was not used as proof ({receipt_output.strip()[:120]})"
    return LineVerdict("SPAWN", "fail", "registry row", detail=detail)


def line_identity(
    *, session_id: str, store_match: bool, recalled_nonce: str, expected_nonce: str
) -> LineVerdict:
    if store_match and session_id:
        return LineVerdict("IDENTITY", "pass", "local store artifact", detail=session_id)
    if recalled_nonce and recalled_nonce == expected_nonce:
        return LineVerdict("IDENTITY", "pass", "cross-process recall nonce", detail=recalled_nonce)
    return LineVerdict(
        "IDENTITY",
        "fail",
        "cross-process recall nonce",
        detail="the planted nonce was not returned by the second process",
    )


def line_claim(*, holder: str | None) -> LineVerdict:
    if holder:
        return LineVerdict("CLAIM", "pass", "live claim holder", detail=holder)
    return LineVerdict("CLAIM", "fail", "live claim holder", detail="claim status named no holder")


def line_mail(*, response_marker: str | None) -> LineVerdict:
    if response_marker:
        return LineVerdict("MAIL BOTH WAYS", "pass", "worker response to sent message", detail=response_marker)
    return LineVerdict("MAIL BOTH WAYS", "fail", "worker response to sent message", detail="worker output did not change")


def line_view(*, screen_marker: str | None, refusal: str | None = None) -> LineVerdict:
    if screen_marker:
        return LineVerdict("VIEW", "pass", "harness-owned screen", detail=screen_marker)
    if refusal:
        return LineVerdict("VIEW", "skip", "harness-owned screen", detail=refusal)
    return LineVerdict("VIEW", "fail", "harness-owned screen", detail="no harness-owned screen was observed")


def line_survive(*, prior_turn_marker: str | None) -> LineVerdict:
    if prior_turn_marker:
        return LineVerdict("SURVIVE", "pass", "prior turn after process stop", detail=prior_turn_marker)
    return LineVerdict("SURVIVE", "fail", "prior turn after process stop", detail="prior turn was not visible")


def _repo_root(repo_root: Path | None) -> Path:
    return (repo_root or Path.cwd()).resolve()


def run_instrument(command: Sequence[str], *, cwd: Path) -> tuple[int, str]:
    """Run one instrument directly so its return code remains observable."""
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=PROBE_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        return 124, f"command timed out after {PROBE_TIMEOUT_S:g}s: {command[0]}"
    return result.returncode, (result.stdout + result.stderr).strip()


def _registration_membership(harness: str, root: Path) -> tuple[list[str], list[str]]:
    locations = {
        "CAPABILITY_TABLE": root / "crates/fno-agents/src/harness_capabilities.toml",
        "KNOWN_HARNESSES": root / "cli/src/fno/harness_names.py",
        "READABLE_PROVIDERS": root / "cli/src/fno/agents/harnesses/__init__.py",
        "PANE_HOSTABLE_PROVIDERS": root / "cli/src/fno/agents/mux_spawn.py",
        "_SESSION_BINDING_HARNESSES": root / "cli/src/fno/agents/mux_spawn.py",
        "KNOWN_PROVIDERS": root / "crates/fno-agents/src/provider.rs",
        "_AMBIENT_NAMES": root / "cli/src/fno/hermetic.py",
    }
    present: list[str] = []
    absent: list[str] = []
    for name, path in locations.items():
        text = path.read_text(encoding="utf-8") if path.is_file() else ""
        (present if re.search(rf"['\"]{re.escape(harness)}['\"]", text) else absent).append(name)
    return present, absent


def line_row_matches(harness: str, *, repo_root: Path | None = None) -> LineVerdict:
    root = _repo_root(repo_root)
    sweep = run_instrument(["python3", "scripts/diagnostics/capability-honesty-sweep.py"], cwd=root)
    fresh = run_instrument(["bash", "scripts/ci/check-harness-capabilities-fresh.sh"], cwd=root)
    present, absent = _registration_membership(harness, root)
    clean_sweep = sweep[0] == 0 and harness not in sweep[1]
    fresh_copies = fresh[0] == 0
    detail = (
        f"sweep={'clean' if clean_sweep else 'finding'}; "
        f"freshness={'fresh' if fresh_copies else 'stale'}; "
        f"registration present={','.join(present) or 'none'} absent={','.join(absent) or 'none'}"
    )
    table_present = "CAPABILITY_TABLE" in present
    if clean_sweep and fresh_copies and table_present:
        return LineVerdict("ROW MATCHES", "pass", "honesty sweep and three-copy freshness", detail=detail)
    return LineVerdict("ROW MATCHES", "fail", "honesty sweep and three-copy freshness", detail=detail)


def line_manifest_pinned(*, harness: str, result: tuple[int, str] | None = None) -> LineVerdict:
    if result is None:
        return LineVerdict("MANIFEST PINNED", "skip", "live readiness-grid capture", detail="live pane not requested")
    code, output = result
    if code == 0 and "READINESS_SMOKE!=1" in output:
        return LineVerdict("MANIFEST PINNED", "skip", "live readiness-grid capture", detail="live capture was not requested")
    if code == 0:
        return LineVerdict("MANIFEST PINNED", "pass", "live readiness-grid capture", detail=output)
    return LineVerdict("MANIFEST PINNED", "fail", "live readiness-grid capture", detail=output or f"capture exited {code}")


def _dry_run_lines(harness: str) -> list[LineVerdict]:
    markers = [
        "registry row",
        "local store artifact or cross-process recall nonce",
        "live claim holder",
        "worker response to sent message",
        "harness-owned screen",
        "prior turn after process stop",
        "honesty sweep and three-copy freshness",
        "live readiness-grid capture",
    ]
    names = ["SPAWN", "IDENTITY", "CLAIM", "MAIL BOTH WAYS", "VIEW", "SURVIVE", "ROW MATCHES", "MANIFEST PINNED"]
    return [LineVerdict(name, "skip", marker, detail=f"dry run for {harness}; no process spawned") for name, marker in zip(names, markers)]


def _probe_argv(harness: str) -> list[str]:
    from fno.agents.mux_spawn import build_pane_argv

    return build_pane_argv(harness, "", Path.cwd(), False, None)


def _dry_run_report(harness: str) -> dict[str, object]:
    try:
        argv = _probe_argv(harness)
    except Exception as exc:  # noqa: BLE001 - dry run must name unsupported lanes
        detail = f"unsupported: cannot compose pane argv ({type(exc).__name__}: {exc})"
        lines = [
            LineVerdict(item.line, "skip", item.marker, item.attempts, detail)
            for item in _dry_run_lines(harness)
        ]
        return {
            "harness": harness,
            "live": False,
            "argv": [],
            "argv_detail": detail,
            "lines": [asdict(line) for line in lines],
        }
    return {
        "harness": harness,
        "live": False,
        "argv": argv,
        "argv_detail": "",
        "lines": [asdict(line) for line in _dry_run_lines(harness)],
    }


def _missing_binary_lines(harness: str, detail: str) -> list[LineVerdict]:
    markers = [
        ("IDENTITY", "local store artifact or cross-process recall nonce"),
        ("CLAIM", "live claim holder"),
        ("MAIL BOTH WAYS", "worker response to sent message"),
        ("VIEW", "harness-owned screen"),
        ("SURVIVE", "prior turn after process stop"),
        ("ROW MATCHES", "honesty sweep and three-copy freshness"),
        ("MANIFEST PINNED", "live readiness-grid capture"),
    ]
    return [
        LineVerdict("SPAWN", "fail", "harness binary", detail=detail),
        *[
            LineVerdict(line, "skip", marker, detail=f"blocked by {harness} binary")
            for line, marker in markers
        ],
    ]


def run_probe(harness: str, *, live: bool, repo_root: Path | None = None) -> dict[str, object]:
    if not live:
        return _dry_run_report(harness)
    # Live execution is opt-in. Credential-gated harnesses are skipped with an
    # operator action instead of being misreported as failed support.
    try:
        credential = subprocess.run(
            [harness, "status", "--format", "json"],
            capture_output=True,
            text=True,
            timeout=PROBE_TIMEOUT_S,
        )
        credential_detail = credential.stdout or credential.stderr
    except subprocess.TimeoutExpired:
        credential = None
        credential_detail = f"status probe timed out after {PROBE_TIMEOUT_S:g}s"
    except OSError as exc:
        return {
            "harness": harness,
            "live": True,
            "argv": [harness],
            "lines": [
                asdict(line)
                for line in _missing_binary_lines(
                    harness, f"{harness} binary unavailable: {exc}"
                )
            ],
        }
    try:
        authenticated = bool(json.loads(credential.stdout).get("isAuthenticated")) if credential else False
    except (json.JSONDecodeError, AttributeError):
        authenticated = bool(credential and credential.returncode == 0)
    if not authenticated:
        action = f"credential required: run {harness} login"
        if credential_detail and "timed out" in credential_detail:
            action = f"{credential_detail}; verify credentials before retrying"
        lines = [LineVerdict(name, "skip", marker, detail=action) for name, marker in (("SPAWN", "registry row"), ("IDENTITY", "cross-process recall nonce"), ("CLAIM", "live claim holder"), ("MAIL BOTH WAYS", "worker response to sent message"), ("VIEW", "harness-owned screen"), ("SURVIVE", "prior turn after process stop"), ("ROW MATCHES", "honesty sweep and three-copy freshness"), ("MANIFEST PINNED", "live readiness-grid capture"))]
    else:
        root = _repo_root(repo_root)
        lines = _dry_run_lines(harness)
        lines[6] = line_row_matches(harness, repo_root=root)
        capture = run_instrument(
            ["bash", "cli/scripts/smoke/capture-readiness-grid.sh", harness],
            cwd=root,
        )
        lines[7] = line_manifest_pinned(harness=harness, result=capture)
    return {"harness": harness, "live": True, "lines": [asdict(line) for line in lines]}


def _report_lines(report: dict[str, object]) -> list[dict[str, object]]:
    raw = report.get("lines")
    if not isinstance(raw, list) or not all(isinstance(item, dict) for item in raw):
        raise RuntimeError("harness probe returned an invalid line report")
    return raw


app = typer.Typer(add_completion=False, no_args_is_help=False, invoke_without_command=True)


@app.callback()
def harness_probe_command(
    harness: str = typer.Argument(..., help="Harness name to probe."),
    live: bool = typer.Option(False, "--live", help="Run real pane and state probes."),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit machine-readable JSON."),
) -> None:
    report = run_probe(harness, live=live)
    lines = _report_lines(report)
    if json_out:
        typer.echo(json.dumps(report))
        if any(item["status"] == "fail" for item in lines):
            raise typer.Exit(1)
        return
    typer.echo(f"fno doctor harness {harness} ({'live' if live else 'dry run'})")
    for item in lines:
        status = item["status"]
        if not isinstance(status, str):
            raise RuntimeError("harness probe returned a line without a status")
        typer.echo(f"{status.upper():4} {item['line']}: marker={item['marker']} ({item['detail']})")
    if not live:
        argv = report.get("argv")
        rendered = " ".join(str(token) for token in argv) if isinstance(argv, list) else ""
        argv_detail = report.get("argv_detail")
        suffix = f" ({argv_detail})" if isinstance(argv_detail, str) and argv_detail else ""
        typer.echo(f"argv would run: {rendered or 'unavailable'}{suffix}")
    if any(item["status"] == "fail" for item in lines):
        raise typer.Exit(1)
