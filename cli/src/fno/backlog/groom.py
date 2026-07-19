"""Daily backlog grooming pass.

Dispatches ONE Sonnet worker a day to groom the global backlog with a fixed
allowlist of reversible levers and mail a one-screen report. This module owns
dispatch and daily dedup only - every judgement call lives in the groom skill
brief, so the levers stay auditable in one place.
"""
from __future__ import annotations

import json
import os
import subprocess
from datetime import date, datetime, timezone
from typing import Any, Optional

GROOM_MODEL_DEFAULT = "claude-sonnet-5"

# The marker must outlive the dispatching process, which cannot hold it by PID.
# A TTL claim whose pid is dead but whose clock has not lapsed classifies
# SUSPECT, which `acquire_claim` treats as held - that is what keeps "once a day"
# true after this process is gone.
_GROOM_TTL_MS = 24 * 60 * 60 * 1000

# `--substrate headless` is a synchronous `claude -p`, so this bounds the whole
# grooming pass, not just its launch. Generous on purpose: a kill lands mid-pass,
# after some levers have already been pulled.
_SPAWN_TIMEOUT_S = 1800


def groom_day_key(today: Optional[date] = None) -> str:
    """The daily dedup claim key, UTC-bucketed like the repo's other day keys."""
    day = today or datetime.now(timezone.utc).date()
    return f"groom:{day.isoformat()}"


def groom_brief(day: str) -> str:
    """The worker seed. Deliberately thin: the skill carries the contract."""
    return (
        f"Daily backlog grooming pass for {day}.\n\n"
        "Load the `fno:groom` skill and follow it end to end. "
        "Use ONLY the levers it allowlists - never edit graph files directly. "
        "Anything the patterns do not support goes to the triage pile as a "
        "one-line question. Finish by mailing the one-screen report."
    )


def _spawn_groom_worker(brief: str, cwd: str, model: str, day: str) -> str:
    """Launch the one-shot Sonnet groom worker; return its spawn short_id.

    ``--substrate headless`` is explicit and load-bearing: a one-shot pass wants
    no pane and no placement prompt, and `-p` is only ever reachable through the
    headless verb.
    """
    from fno import _subprocess_util

    cmd = [
        *_subprocess_util.fno_py_cmd(), "agents", "spawn",
        "--provider", "claude", "--substrate", "headless",
        "--model", model, "--cwd", cwd,
        f"groom-{day}", brief,
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=_SPAWN_TIMEOUT_S)
    if proc.returncode != 0:
        raise RuntimeError(
            f"fno agents spawn exited {proc.returncode}: "
            f"{(proc.stderr or proc.stdout or '').strip()[:200]}"
        )
    for line in (proc.stdout or "").splitlines():
        if '"short_id"' in line:
            try:
                sid = str(json.loads(line).get("short_id", "") or "")
            except json.JSONDecodeError:
                continue
            if sid:
                return sid
    return "unknown"


def run_groom(
    *,
    cwd: str,
    model: str = GROOM_MODEL_DEFAULT,
    today: Optional[date] = None,
    dry_run: bool = False,
) -> dict[str, Any]:
    """Dispatch today's grooming pass, at most once per UTC day.

    Returns a receipt whose ``status`` is one of ``dispatched`` | ``already-ran``
    | ``dry-run`` | ``failed``. Never raises: a grooming pass is hygiene, so a
    failed dispatch reports and leaves the day retryable.
    """
    from fno.claims.core import (
        ClaimHeldByOther,
        acquire_claim,
        claim_status,
        release_claim,
    )
    from fno.claims.io import claims_root_for

    key = groom_day_key(today)
    day = key.split(":", 1)[1]
    brief = groom_brief(day)

    if dry_run:
        return {"status": "dry-run", "day": day, "key": key, "model": model, "brief": brief}

    holder = f"groom:{os.getpid()}"
    root = claims_root_for(key)

    # Read before acquiring: `acquire_claim` is idempotent for the SAME holder, so
    # a second call from one process would re-acquire and re-dispatch. The status
    # read catches that; the acquire below still catches the cross-process race.
    try:
        if claim_status(key, root=root).get("state") in ("live", "suspect"):
            return {"status": "already-ran", "day": day, "key": key}
    except Exception:  # noqa: BLE001 - an unreadable marker falls through to acquire
        pass

    try:
        acquire_claim(
            key,
            holder,
            ttl_ms=_GROOM_TTL_MS,
            reason=f"daily grooming pass {day}",
            root=root,
        )
    except ClaimHeldByOther:
        return {"status": "already-ran", "day": day, "key": key}
    except Exception as exc:  # noqa: BLE001 - a claims fault must not crash cron
        return {"status": "failed", "day": day, "detail": f"claim: {str(exc)[:200]}"}

    try:
        short_id = _spawn_groom_worker(brief, cwd, model, day)
    except subprocess.TimeoutExpired:
        # The worker RAN before the kill, so levers may already be applied. Hold
        # the marker - re-running today would re-apply them - and report loudly.
        return {
            "status": "failed",
            "day": day,
            "detail": f"groom worker exceeded {_SPAWN_TIMEOUT_S}s and was killed mid-pass",
            "released": False,
        }
    except Exception as exc:  # noqa: BLE001
        # The worker never ran, so hand the day back rather than burn it. Report
        # whether the handback actually happened: a silently leaked marker would
        # make every retry today exit 0 as `already-ran` with nothing amiss.
        receipt: dict[str, Any] = {"status": "failed", "day": day, "detail": str(exc)[:200]}
        try:
            release_claim(key, holder, strict=True, root=root)
            receipt["released"] = True
        except Exception as rexc:  # noqa: BLE001
            receipt["released"] = False
            receipt["release_error"] = str(rexc)[:200]
        return receipt

    return {"status": "dispatched", "day": day, "short_id": short_id, "model": model}
