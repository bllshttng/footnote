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
import ast
import os
import shutil
import tempfile
import time
import uuid
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Literal, Sequence

import typer

Status = Literal["pass", "fail", "skip"]
PROBE_TIMEOUT_S = 5.0
INSTRUMENT_TIMEOUT_S = 30.0


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
    delay_s: float = 0.0,
) -> LineVerdict:
    """Read a positive marker three times when the result is not positive."""
    last = ""
    for attempt in range(1, 4):
        if attempt > 1 and delay_s:
            time.sleep(delay_s)
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
    *, receipt_output: str, registry_row: object, gate_output: str, attempts: int = 1
) -> LineVerdict:
    if registry_row is not None:
        return LineVerdict("SPAWN", "pass", "registry row", attempts, "row read back")
    detail = "registry row was not found"
    gates = [needle for needle in ("Workspace Trust Required", "consent", "login") if needle.lower() in gate_output.lower()]
    if gates:
        detail += f"; gate={gates[0]}"
    if receipt_output:
        detail += f"; receipt was not used as proof ({receipt_output.strip()[:120]})"
    return LineVerdict("SPAWN", "fail", "registry row", attempts, detail)


def line_identity(
    *,
    session_id: str,
    store_match: bool,
    recalled_nonce: str,
    expected_nonce: str,
    attempts: int = 1,
) -> LineVerdict:
    if store_match and session_id:
        return LineVerdict("IDENTITY", "pass", "local store artifact", attempts, session_id)
    if recalled_nonce and recalled_nonce == expected_nonce:
        return LineVerdict(
            "IDENTITY", "pass", "cross-process recall nonce", attempts, recalled_nonce
        )
    return LineVerdict(
        "IDENTITY",
        "fail",
        "cross-process recall nonce",
        attempts,
        "the planted nonce was not returned by the second process",
    )


def _identity_from_marker(
    *,
    session_id: str,
    observed: str,
    expected_nonce: str,
    attempts: int,
) -> LineVerdict:
    return line_identity(
        session_id=session_id,
        store_match=observed == "local store artifact",
        recalled_nonce=expected_nonce
        if observed == "cross-process recall nonce"
        else "",
        expected_nonce=expected_nonce,
        attempts=attempts,
    )


def _mail_response_marker(before: str, after: str, nonce: str) -> str:
    marker = f"PROBE_REPLY={nonce}"
    return marker if marker in after and marker not in before else ""


def _survive_marker(
    resume_code: int, before: str, after: str, nonce: str
) -> str:
    prior = f"PROBE_SEED={nonce}"
    reply = f"PROBE_SURVIVE={nonce}"
    if resume_code == 0 and prior in before and prior in after and reply in after and reply not in before:
        return reply
    return ""


def _screen_marker(code: int, output: str) -> str:
    return output if code == 0 and output.strip() else ""


def line_claim(*, holder: str | None, attempts: int = 1) -> LineVerdict:
    if holder:
        return LineVerdict("CLAIM", "pass", "live claim holder", attempts, holder)
    return LineVerdict(
        "CLAIM", "fail", "live claim holder", attempts, "claim status named no holder"
    )


def line_mail(*, response_marker: str | None, attempts: int = 1) -> LineVerdict:
    if response_marker:
        return LineVerdict(
            "MAIL BOTH WAYS",
            "pass",
            "worker response to sent message",
            attempts,
            response_marker,
        )
    return LineVerdict(
        "MAIL BOTH WAYS",
        "fail",
        "worker response to sent message",
        attempts,
        "worker output did not change",
    )


def line_view(
    *, screen_marker: str | None, refusal: str | None = None, attempts: int = 1
) -> LineVerdict:
    if screen_marker:
        return LineVerdict("VIEW", "pass", "harness-owned screen", attempts, screen_marker)
    if refusal:
        return LineVerdict("VIEW", "skip", "harness-owned screen", attempts, refusal)
    return LineVerdict(
        "VIEW",
        "fail",
        "harness-owned screen",
        attempts,
        "no harness-owned screen was observed",
    )


def line_survive(*, prior_turn_marker: str | None, attempts: int = 1) -> LineVerdict:
    if prior_turn_marker:
        return LineVerdict(
            "SURVIVE",
            "pass",
            "prior turn after process stop",
            attempts,
            prior_turn_marker,
        )
    return LineVerdict(
        "SURVIVE",
        "fail",
        "prior turn after process stop",
        attempts,
        "prior turn was not visible",
    )


def _repo_root(repo_root: Path | None) -> Path:
    return (repo_root or Path.cwd()).resolve()


def run_instrument(
    command: Sequence[str],
    *,
    cwd: Path,
    timeout: float = INSTRUMENT_TIMEOUT_S,
    env: dict[str, str] | None = None,
) -> tuple[int, str]:
    """Run one instrument directly so its return code remains observable."""
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
        )
    except subprocess.TimeoutExpired:
        return 124, f"command timed out after {timeout:g}s: {command[0]}"
    except OSError as exc:
        return 127, f"could not run {command[0]}: {exc}"
    return result.returncode, (result.stdout + result.stderr).strip()


def _python_declaration(path: Path, symbol: str) -> set[str]:
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"))
    except (OSError, SyntaxError, UnicodeDecodeError):
        return set()
    for node in ast.walk(tree):
        value = None
        targets: list[ast.expr] = []
        if isinstance(node, ast.Assign):
            value = node.value
            targets = node.targets
        elif isinstance(node, ast.AnnAssign):
            value = node.value
            targets = [node.target]
        if value is None or not any(
            isinstance(target, ast.Name) and target.id == symbol for target in targets
        ):
            continue
        if not isinstance(value, (ast.Tuple, ast.List, ast.Set)):
            return set()
        return {
            item.value
            for item in value.elts
            if isinstance(item, ast.Constant) and isinstance(item.value, str)
        }
    return set()


def _declared_membership(path: Path, symbol: str, harness: str) -> bool:
    if path.suffix == ".py":
        return harness in _python_declaration(path, symbol)
    try:
        source = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return False
    match = re.search(
        rf"(?s)\b{re.escape(symbol)}\b\s*:[^=]+?=\s*&?\[([^]]*)\]",
        source,
    )
    return bool(match and re.search(rf'"{re.escape(harness)}"', match.group(1)))


def _table_has_row(path: Path, harness: str) -> bool:
    try:
        source = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return False
    return bool(re.search(rf"(?m)^\[harness\.{re.escape(harness)}\]\s*$", source))


def _registration_membership(harness: str, root: Path) -> tuple[list[str], list[str]]:
    locations = {
        "CAPABILITY_TABLE": (
            root / "crates/fno-agents/src/harness_capabilities.toml",
            "",
        ),
        "KNOWN_HARNESSES": (root / "cli/src/fno/harness_names.py", "KNOWN_HARNESSES"),
        "READABLE_PROVIDERS": (
            root / "cli/src/fno/agents/harnesses/__init__.py",
            "READABLE_PROVIDERS",
        ),
        "PANE_HOSTABLE_PROVIDERS": (
            root / "cli/src/fno/agents/mux_spawn.py",
            "PANE_HOSTABLE_PROVIDERS",
        ),
        "_SESSION_BINDING_HARNESSES": (
            root / "cli/src/fno/agents/mux_spawn.py",
            "_SESSION_BINDING_HARNESSES",
        ),
        "KNOWN_PROVIDERS": (
            root / "crates/fno-agents/src/provider.rs",
            "KNOWN_PROVIDERS",
        ),
        "_AMBIENT_NAMES": (root / "cli/src/fno/hermetic.py", "_AMBIENT_NAMES"),
    }
    present: list[str] = []
    absent: list[str] = []
    for name, (path, symbol) in locations.items():
        found = (
            _table_has_row(path, harness)
            if name == "CAPABILITY_TABLE"
            else _declared_membership(path, symbol, harness)
        )
        (present if found else absent).append(name)
    return present, absent


def _sweep_has_finding(output: str, harness: str) -> bool:
    try:
        section = output.split("=== 3.", 1)[1].split("=== 4.", 1)[0]
    except IndexError:
        return True
    return bool(re.search(rf"(?m)^\s*{re.escape(harness)}\.", section))


def _required_registrations(harness: str, root: Path) -> set[str]:
    required = {
        "CAPABILITY_TABLE",
        "KNOWN_HARNESSES",
        "READABLE_PROVIDERS",
        "PANE_HOSTABLE_PROVIDERS",
        "KNOWN_PROVIDERS",
    }
    try:
        import tomllib

        table = tomllib.loads(
            (root / "crates/fno-agents/src/harness_capabilities.toml").read_text(
                encoding="utf-8"
            )
        )
        capabilities = table.get("harness", {}).get(harness, {})
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError, AttributeError):
        capabilities = {}
    if isinstance(capabilities, dict):
        binding = capabilities.get("session_binding")
        if isinstance(binding, dict) and binding.get("required") is True:
            required.add("_SESSION_BINDING_HARNESSES")
    return required


def line_row_matches(harness: str, *, repo_root: Path | None = None) -> LineVerdict:
    root = _repo_root(repo_root)
    sweep = run_instrument(
        ["python3", "scripts/diagnostics/capability-honesty-sweep.py"], cwd=root
    )
    fresh = run_instrument(
        ["bash", "scripts/ci/check-harness-capabilities-fresh.sh"], cwd=root
    )
    present, absent = _registration_membership(harness, root)
    clean_sweep = sweep[0] == 0 and not _sweep_has_finding(sweep[1], harness)
    fresh_copies = fresh[0] == 0
    detail = (
        f"sweep={'clean' if clean_sweep else 'finding'}; "
        f"freshness={'fresh' if fresh_copies else 'stale'}; "
        f"registration present={','.join(present) or 'none'} absent={','.join(absent) or 'none'}"
    )
    required = _required_registrations(harness, root)
    if clean_sweep and fresh_copies and required.issubset(present):
        return LineVerdict("ROW MATCHES", "pass", "honesty sweep and three-copy freshness", detail=detail)
    return LineVerdict("ROW MATCHES", "fail", "honesty sweep and three-copy freshness", detail=detail)


def line_manifest_pinned(
    *,
    harness: str,
    result: tuple[int, str] | None = None,
    readiness_marker: str | None = None,
) -> LineVerdict:
    if result is None:
        return LineVerdict("MANIFEST PINNED", "skip", "live readiness-grid capture", detail="live pane not requested")
    code, output = result
    if code == 0 and "READINESS_SMOKE!=1" in output:
        return LineVerdict("MANIFEST PINNED", "skip", "live readiness-grid capture", detail="live capture was not requested")
    if code == 0 and readiness_marker:
        return LineVerdict("MANIFEST PINNED", "pass", "live readiness-grid capture", detail=readiness_marker)
    if code == 0:
        return LineVerdict(
            "MANIFEST PINNED",
            "fail",
            "live readiness-grid capture",
            detail="capture exited 0 without a readiness-specific positive marker",
        )
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


def _run_command(
    command: Sequence[str],
    *,
    cwd: Path,
    timeout: float,
    env: dict[str, str] | None = None,
) -> tuple[int, str]:
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
        )
    except subprocess.TimeoutExpired:
        return 124, f"command timed out after {timeout:g}s: {command[0]}"
    except OSError as exc:
        return 127, f"could not run {command[0]}: {exc}"
    return result.returncode, (result.stdout + result.stderr).strip()


def _registry_row(name: str) -> object | None:
    try:
        from fno.agents.registry import load_registry

        return next((row for row in load_registry() if row.name == name), None)
    except Exception:  # noqa: BLE001 - an unreadable registry is a failed marker
        return None


def _row_value(row: object, field: str) -> object | None:
    if isinstance(row, dict):
        return row.get(field)
    return getattr(row, field, None)


def _claim_holder(
    claim_key: str, *, cwd: Path, timeout: float = PROBE_TIMEOUT_S
) -> str:
    _, output = _run_command(
        ["fno", "agents", "claim", "status", claim_key, "--json"],
        cwd=cwd,
        timeout=timeout,
    )
    try:
        value = json.loads(output)
    except json.JSONDecodeError:
        return ""
    return str(value.get("holder") or "") if isinstance(value, dict) else ""


def _worker_logs(name: str, *, cwd: Path) -> str:
    return _run_command(
        ["fno", "agents", "logs", name],
        cwd=cwd,
        timeout=PROBE_TIMEOUT_S,
    )[1]


def _auth_failure(output: str) -> bool:
    return bool(
        re.search(
            r"(?i)(not authenticated|authentication required|login required|"
            r"no providers configured|credential|unauthenticated)",
            output,
        )
    )


def _readiness_marker(harness: str, root: Path) -> str | None:
    fixture = root / "cli/tests/agents/fixtures" / f"readiness-grid-{harness}.txt"
    try:
        screen = fixture.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None
    try:
        from fno.agents.mux_spawn import _evaluate_manifest_screen

        verdict = _evaluate_manifest_screen(harness, screen, allow_dev_binary=True)
    except Exception:  # noqa: BLE001 - unreadable detector is no proof
        return None
    if verdict.get("matched") and verdict.get("rule_id"):
        return f"readiness rule {verdict['rule_id']}"
    return None


def _credential_skip_lines(action: str) -> list[LineVerdict]:
    return [
        LineVerdict(line, "skip", marker, detail=action)
        for line, marker in (
            ("SPAWN", "registry row"),
            ("IDENTITY", "local store artifact or cross-process recall nonce"),
            ("CLAIM", "live claim holder"),
            ("MAIL BOTH WAYS", "worker response to sent message"),
            ("VIEW", "harness-owned screen"),
            ("SURVIVE", "prior turn after process stop"),
            ("MANIFEST PINNED", "live readiness-grid capture"),
        )
    ]


def _run_live_probe(harness: str, root: Path) -> list[LineVerdict]:
    probe_id = uuid.uuid4().hex[:8]
    name = f"harness-probe-{harness}-{probe_id}"
    claim_key = f"probe:{harness}:{probe_id}"
    nonce = uuid.uuid4().hex
    seed = (
        f"Support probe {nonce}. Run fno agents claim acquire {claim_key} "
        f"--holder {name} --ttl 2m, print PROBE_SEED={nonce}, then remain idle."
    )
    with tempfile.TemporaryDirectory(prefix=f"fno-harness-{harness}-") as scratch_name:
        scratch = Path(scratch_name)
        spawn_code, spawn_output = _run_command(
            [
                "fno",
                "agents",
                "spawn",
                seed,
                "--name",
                name,
                "--harness",
                harness,
                "--cwd",
                str(scratch),
                "--timeout",
                "60",
            ],
            cwd=root,
            timeout=75.0,
        )
        try:
            row_probe = retry_marker(
                line="SPAWN",
                marker_name="registry row",
                read_marker=lambda: (
                    "row" if _registry_row(name) is not None else ""
                ),
                matches=bool,
                delay_s=0.5,
            )
            row = _registry_row(name)
            if spawn_code != 0 and _auth_failure(spawn_output):
                row_lines = _credential_skip_lines(
                    f"credential required: run {harness} login"
                )
                row_lines.insert(6, line_row_matches(harness, repo_root=root))
                return row_lines

            spawn_line = line_spawn(
                receipt_output=spawn_output,
                registry_row=row,
                gate_output=spawn_output,
                attempts=row_probe.attempts,
            )
            row_line = line_row_matches(harness, repo_root=root)
            if row is None:
                return [
                    spawn_line,
                    line_identity(
                        session_id="",
                        store_match=False,
                        recalled_nonce="",
                        expected_nonce=nonce,
                        attempts=3,
                    ),
                    line_claim(holder=None, attempts=3),
                    line_mail(response_marker=None, attempts=3),
                    line_view(screen_marker=None, attempts=3),
                    line_survive(prior_turn_marker=None, attempts=3),
                    row_line,
                    line_manifest_pinned(
                        harness=harness,
                        result=(spawn_code, spawn_output),
                    ),
                ]

            session_id = str(_row_value(row, "harness_session_id") or "")
            logs_before = _worker_logs(name, cwd=root)
            _, recall_output = _run_command(
                [
                    "fno",
                    "agents",
                    "resume",
                    name,
                    "--message",
                    f"print PROBE_RECALL={nonce}",
                ],
                cwd=root,
                timeout=PROBE_TIMEOUT_S,
            )
            identity = retry_marker(
                line="IDENTITY",
                marker_name="local store artifact",
                read_marker=lambda: (
                    "local store artifact"
                    if session_id and session_id in _worker_logs(name, cwd=root)
                    else (
                        "cross-process recall nonce"
                        if nonce in recall_output or nonce in _worker_logs(name, cwd=root)
                        else ""
                    )
                ),
                matches=bool,
                delay_s=0.5,
            )
            identity_line = _identity_from_marker(
                session_id=session_id,
                observed=identity.detail,
                expected_nonce=nonce,
                attempts=identity.attempts,
            )

            claim = retry_marker(
                line="CLAIM",
                marker_name="live claim holder",
                read_marker=lambda: _claim_holder(claim_key, cwd=root),
                matches=bool,
                delay_s=0.5,
            )
            claim_line = line_claim(
                holder=claim.detail if claim.status == "pass" else None,
                attempts=claim.attempts,
            )

            mail_nonce = uuid.uuid4().hex
            logs_before_mail = _worker_logs(name, cwd=root)
            _run_command(
                [
                    "fno",
                    "agents",
                    "mail",
                    "send",
                    name,
                    f"PROBE_MAIL={mail_nonce}",
                ],
                cwd=root,
                timeout=PROBE_TIMEOUT_S,
            )
            mail = retry_marker(
                line="MAIL BOTH WAYS",
                marker_name="worker response to sent message",
                read_marker=lambda: _mail_response_marker(
                    logs_before_mail, _worker_logs(name, cwd=root), mail_nonce
                ),
                matches=bool,
                delay_s=0.5,
            )
            mail_line = line_mail(
                response_marker=mail.detail if mail.status == "pass" else None,
                attempts=mail.attempts,
            )

            mux = _row_value(row, "mux")
            if isinstance(mux, dict) and mux.get("session") and mux.get("pane_id"):
                pane_ref = f"{mux['session']}:{mux['pane_id']}"
                view = retry_marker(
                    line="VIEW",
                    marker_name="harness-owned screen",
                    read_marker=lambda: _screen_marker(
                        *_run_command(
                            ["fno", "mux", "pane", "read", pane_ref],
                            cwd=root,
                            timeout=PROBE_TIMEOUT_S,
                        )
                    ),
                    matches=bool,
                    delay_s=0.5,
                )
                view_line = line_view(
                    screen_marker=view.detail if view.status == "pass" else None,
                    attempts=view.attempts,
                )
            else:
                view_line = line_view(
                    screen_marker=None,
                    refusal="harness supplied no native pane reference",
            )

            if isinstance(mux, dict) and mux.get("session") and mux.get("pane_id"):
                resume_code, _ = _run_command(
                    [
                        "fno",
                        "mux",
                        "pane",
                        "kill",
                        "--session",
                        str(mux["session"]),
                        str(mux["pane_id"]),
                    ],
                    cwd=root,
                    timeout=PROBE_TIMEOUT_S,
                )
                _run_command(
                    [
                        "fno",
                        "agents",
                        "resume",
                        name,
                        "--message",
                        f"print PROBE_SURVIVE={nonce}",
                    ],
                    cwd=root,
                    timeout=PROBE_TIMEOUT_S,
                )
                survive = retry_marker(
                    line="SURVIVE",
                    marker_name="prior turn after process stop",
                    read_marker=lambda: _survive_marker(
                        resume_code,
                        logs_before,
                        _worker_logs(name, cwd=root),
                        nonce,
                    ),
                matches=bool,
                delay_s=0.5,
            )
            survive_line = line_survive(
                prior_turn_marker=survive.detail if survive.status == "pass" else None,
                attempts=survive.attempts,
            )

            env = dict(os.environ)
            env["READINESS_SMOKE"] = "1"
            capture = run_instrument(
                ["bash", "cli/scripts/smoke/capture-readiness-grid.sh", harness],
                cwd=root,
                timeout=INSTRUMENT_TIMEOUT_S,
                env=env,
            )
            return [
                spawn_line,
                identity_line,
                claim_line,
                mail_line,
                view_line,
                survive_line,
                row_line,
                line_manifest_pinned(
                    harness=harness,
                    result=capture,
                    readiness_marker=_readiness_marker(harness, root),
                ),
            ]
        finally:
            _run_command(
                ["fno", "agents", "rm", name],
                cwd=root,
                timeout=PROBE_TIMEOUT_S,
            )


def run_probe(harness: str, *, live: bool, repo_root: Path | None = None) -> dict[str, object]:
    if not live:
        return _dry_run_report(harness)
    root = _repo_root(repo_root)
    if shutil.which(harness) is None:
        return {
            "harness": harness,
            "live": True,
            "argv": [harness],
            "lines": [
                asdict(line)
                for line in _missing_binary_lines(
                    harness, f"{harness} binary is not on PATH"
                )
            ],
        }
    lines = _run_live_probe(harness, root)
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
