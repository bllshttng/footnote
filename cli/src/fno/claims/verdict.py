"""Native batch claim verdicts for Python callers."""

from __future__ import annotations

import json
from datetime import datetime, timezone
from subprocess import run as run_subprocess
from pathlib import Path
from typing import Any, Sequence

import psutil

from fno.rust_binary import resolve_binary
from .io import claim_path, claims_dir


class ClaimVerdictUnavailable(RuntimeError):
    """The native claim decision could not be reached."""


class ClaimVerdictError(RuntimeError):
    """The native claim decision returned an invalid or failed response."""


class ClaimSweepOmission(ClaimVerdictError):
    """The sweep omitted a claim the on-disk state says exists.

    Raised rather than the bare ClaimVerdictError so a racer whose directory
    snapshot straddled another worker's archive-and-recreate can be told
    apart from a genuinely failed sweep and simply re-classify.
    """


def process_create_time_ms(pid: int | None) -> int | None:
    """Read a process create time for the re-anchor fact check."""
    if pid is None:
        return None
    try:
        return int(psutil.Process(pid).create_time() * 1000)
    except (psutil.NoSuchProcess, psutil.AccessDenied):
        return None


def run_op(
    op_args: Sequence[str], claims_dirs: Sequence[Path]
) -> tuple[dict[str, Any] | None, str | None]:
    """Run one ``fno-agents claim <op>`` that takes repeatable
    ``--claims-dir``; answers ``(payload, None)`` or ``(None, error)`` — a
    reporting caller never dies on an op failure."""
    binary = resolve_binary()
    if binary is None:
        return None, "fno-agents binary not found"
    command = [str(binary), "claim", *op_args]
    for cdir in claims_dirs:
        command.extend(("--claims-dir", str(cdir)))
    try:
        result = run_subprocess(command, capture_output=True, text=True, check=False)
    except OSError as exc:
        return None, f"OSError: {exc}"
    if result.returncode != 0:
        return None, result.stderr.strip() or f"exit {result.returncode}"
    try:
        return json.loads(result.stdout), None
    except json.JSONDecodeError as exc:
        return None, f"JSONDecodeError: {exc}"


def claim_verdicts(
    keys: Sequence[str] | None = None,
    *,
    prefix: str | None = None,
    root: Path | None = None,
    claims_dir_path: Path | None = None,
) -> dict[str, dict[str, Any]]:
    """Return native verdict rows for many keys in one subprocess.

    ``claims_dir_path`` is passed verbatim as ``--claims-dir``; ``root``
    would be re-resolved one level down, so a caller holding a resolved
    claims directory must use it.
    """
    binary = resolve_binary()
    if binary is None:
        raise ClaimVerdictUnavailable(
            "fno-agents claim verdict unavailable: the fno-agents binary was not found; "
            "set FNO_AGENTS_BIN or reinstall fno."
        )

    requested = tuple(keys or ())
    command = [str(binary), "claim", "sweep", "--json"]
    if requested:
        for key in requested:
            command.extend(("--key", key))
    elif prefix is not None:
        command.extend(("--prefix", prefix))
    else:
        command.append("--all")
    # The door must read the SAME directory Python resolves: root as given,
    # else the claims_dir(None) contract (env override, else the repo's space).
    # Verbatim --claims-dir, because --root spells a repo checkout (it appends
    # .fno/claims) and no root reaches the space layout.
    command.extend(("--claims-dir", str(claims_dir_path or claims_dir(root))))

    try:
        result = run_subprocess(command, capture_output=True, text=True, check=False)
    except OSError as exc:
        raise ClaimVerdictUnavailable(
            "fno-agents claim verdict unavailable: could not run the native binary; "
            "set FNO_AGENTS_BIN or reinstall fno."
        ) from exc
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no diagnostic"
        raise ClaimVerdictError(f"fno-agents claim sweep failed with exit {result.returncode}: {detail}")

    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise ClaimVerdictError(f"fno-agents claim sweep returned invalid JSON: {exc}") from exc
    rows = payload.get("claims") if isinstance(payload, dict) else None
    if not isinstance(rows, list):
        raise ClaimVerdictError("fno-agents claim sweep returned no claims array")
    verdicts: dict[str, dict[str, Any]] = {}
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("key"), str):
            raise ClaimVerdictError("fno-agents claim sweep returned a malformed claim row")
        verdicts[row["key"]] = row
    for key in requested:
        if key in verdicts:
            continue
        if claim_path(key, root=root).exists():
            raise ClaimSweepOmission(f"native claim sweep omitted existing claim {key!r}; refusing to assume free")
        verdicts[key] = {"key": key, "state": "free"}
    return verdicts


def reclaimable_stamp(row: dict[str, Any]) -> str | None:
    """UTC ISO instant an unresolved claim's bounded grace ends, or None.

    Only a row carrying an integer ``reclaimable_at`` (epoch ms, from the
    native sweep) has a clock exit; a live claim or a pid-suspect claim
    invents no time.
    """
    reclaimable_at = row.get("reclaimable_at")
    if not isinstance(reclaimable_at, int) or isinstance(reclaimable_at, bool):
        return None
    return datetime.fromtimestamp(reclaimable_at / 1000, tz=timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


def reclaimable_note(row: dict[str, Any]) -> str | None:
    """Human note for when the claim turns reclaimable, or None.

    ``reclaimable at <UTC> (in N min)``, or ``reclaimable now`` once the
    instant passed between classification and print.
    """
    stamp = reclaimable_stamp(row)
    if stamp is None:
        return None
    remaining_s = (row["reclaimable_at"] - datetime.now(timezone.utc).timestamp() * 1000) / 1000
    if remaining_s <= 0:
        return "reclaimable now"
    minutes = max(1, round(remaining_s / 60))
    return f"reclaimable at {stamp} (in {minutes} min)"
