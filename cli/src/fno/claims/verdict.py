"""Native batch claim verdicts for Python callers."""

from __future__ import annotations

import json
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


def process_create_time_ms(pid: int | None) -> int | None:
    """Read a process create time for the re-anchor fact check."""
    if pid is None:
        return None
    try:
        return int(psutil.Process(pid).create_time() * 1000)
    except (psutil.NoSuchProcess, psutil.AccessDenied):
        return None


def claim_verdicts(
    keys: Sequence[str] | None = None,
    *,
    prefix: str | None = None,
    root: Path | None = None,
) -> dict[str, dict[str, Any]]:
    """Return native verdict rows for many keys in one subprocess."""
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
    command.extend(("--claims-dir", str(claims_dir(root))))

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
            raise ClaimVerdictError(f"native claim sweep omitted existing claim {key!r}; refusing to assume free")
        verdicts[key] = {"key": key, "state": "free"}
    return verdicts
