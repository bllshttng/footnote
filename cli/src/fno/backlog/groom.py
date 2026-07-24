"""Daily backlog grooming pass.

ONE entry point owns the whole pass: the mechanical legs (archive, reconcile,
maintain, relatedness) run here under the daily claim, then ONE Sonnet worker is
dispatched for the judgment calls with a fixed allowlist of reversible levers and
mails a one-screen report. This module owns sequencing and daily dedup only -
every judgement call lives in the groom skill brief, so the levers stay auditable
in one place.
"""
from __future__ import annotations

import json
import os
import subprocess
import time
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any, Literal, Optional

GroomStatus = Literal["dispatched", "degraded", "already-ran", "dry-run", "failed"]

GROOM_MODEL_DEFAULT = "claude-sonnet-5"

GROOM_AGE_DEFAULT = 14

# The marker must outlive the dispatching process, which cannot hold it by PID.
# A TTL claim whose pid is dead but whose clock has not lapsed classifies
# SUSPECT, which `acquire_claim` treats as held - that is what keeps "once a day"
# true after this process is gone.
_GROOM_TTL_MS = 24 * 60 * 60 * 1000

# `--substrate headless` is a synchronous `claude -p`, so this bounds the whole
# grooming pass, not just its launch. Generous on purpose: a kill lands mid-pass,
# after some levers have already been pulled.
#
# The budget is passed to `spawn --timeout` so the RUNNER bounds the worker.
# Killing our own subprocess would only reap the `fno agents spawn` wrapper - the
# `claude -p` grandchild would survive and keep mutating the graph unattended.
# The outer bound is deliberately larger so the inner one always fires first.
_WORKER_TIMEOUT_S = 1800
_SPAWN_TIMEOUT_S = _WORKER_TIMEOUT_S + 120


# Shared by the key writer and the marker scanner, so the freshness read cannot
# drift from the key it is looking for.
_GROOM_KEY_PREFIX = "groom"


def groom_day_key(today: Optional[date] = None) -> str:
    """The daily dedup claim key, UTC-bucketed like the repo's other day keys."""
    day = today or datetime.now(timezone.utc).date()
    return f"{_GROOM_KEY_PREFIX}:{day.isoformat()}"


# Past this, the daily cadence is presumed dead: `fno doctor` reddens and the
# SessionStart fallback fires. Deliberately > 24h so a healthy LaunchAgent that
# merely slipped a night is not treated as broken.
GROOM_STALE_HOURS = 48.0

# ("ran", hours) | ("never", None) | ("unknown", None). The third state is
# load-bearing: folding an unreadable claims root into "never" would make every
# session look like a machine that has never groomed, and fire the fallback on
# every one of them.
GroomFreshness = tuple[Literal["ran", "never", "unknown"], Optional[float]]


def groom_staleness(*, now: Optional[float] = None) -> GroomFreshness:
    """Hours since the last grooming pass, read from its own claim marker.

    The daily claim IS the run receipt, so there is no second source of truth to
    drift from - and none to add. Live markers sit in the claims dir; recovered
    ones are archived under ``.expired/`` with the same encoded key, so both are
    scanned. Age comes from the file's mtime (an archive is a rename, which
    preserves it), never from a shelled ``stat`` whose flags differ BSD vs GNU.
    """
    from fno.claims.io import EXPIRED_SUBDIR, claims_dir, claims_root_for, encode_key

    prefix = encode_key(f"{_GROOM_KEY_PREFIX}:")
    base = claims_dir(claims_root_for(f"{_GROOM_KEY_PREFIX}:probe"))

    newest: Optional[float] = None
    for directory in (base, base / EXPIRED_SUBDIR):
        try:
            names = os.listdir(directory)
        except FileNotFoundError:
            # A machine that never groomed has no claims dir at all; that is
            # "never", not "unreadable".
            continue
        except OSError:
            return ("unknown", None)
        for name in names:
            if not name.startswith(prefix):
                continue
            try:
                mtime = (directory / name).stat().st_mtime
            except OSError:
                continue
            if newest is None or mtime > newest:
                newest = mtime

    if newest is None:
        return ("never", None)
    hours = ((now if now is not None else time.time()) - newest) / 3600.0

    return ("ran", max(0.0, hours))  # clock skew reads as fresh, never negative


def groom_is_due(freshness: Optional[GroomFreshness] = None) -> bool:
    """Whether an unattended trigger should run a pass now.

    ``unknown`` stays dormant on purpose: the fallback's failure mode is firing
    on every session, so an unreadable root must not be an invitation to run.
    """
    state, hours = freshness if freshness is not None else groom_staleness()
    if state == "never":
        return True
    return state == "ran" and hours is not None and hours > GROOM_STALE_HOURS


def groom_brief(day: str, mechanical: Optional[dict[str, str]] = None) -> str:
    """The worker seed. Deliberately thin: the skill carries the contract.

    The mechanical outcomes are interpolated because the worker is the only
    thing that reaches a human. The receipt goes to a launchd log nobody reads,
    so a leg that fails silently every night is invisible unless the mailed
    report names it - which the worker cannot do without being told.
    """
    legs = ""
    if mechanical:
        itemized = ", ".join(f"{name} {outcome}" for name, outcome in mechanical.items())
        legs = (
            f"\n\nToday's mechanical pass already ran: {itemized}.\n"
            "Report these verbatim as the leading Mechanical line, naming every "
            "leg. Any leg that did not come back `ok` is an anomaly: say so "
            "plainly in the report - that line is the only signal an operator gets."
        )
    return (
        f"Daily backlog grooming pass for {day}."
        f"{legs}\n\n"
        "Load the `fno:groom` skill and follow it end to end. "
        "Use ONLY the levers it allowlists - never edit graph files directly. "
        "Anything the patterns do not support goes to the triage pile as a "
        "one-line question. Finish by mailing the one-screen report."
    )


# Exit 4 is a PARTIAL result, never "nothing to do" - every site that raises it
# in this CLI is a degraded outcome (unresolved PR queries in reconcile, a
# retryable gh outage, nothing intaked). The quiet paths all exit 0.
_PARTIAL_EXIT = 4
_LEG_TIMEOUT_S = 600


def _mechanical_legs(age: int) -> list[tuple[str, list[str]]]:
    """The mechanical pass, in dependency order.

    ``relatedness build`` runs LAST so the map reflects the post-groom graph:
    nodes archived this pass are gone from the corpus rather than left as
    dangling edges until the next build.
    """
    return [
        ("archive", ["archive", "--apply", "--older-than-days", str(age)]),
        ("reconcile", ["reconcile"]),
        # --no-validity is load-bearing: the validity sweep watermarks the pile
        # it reviews, so running it here would leave the worker's own read-only
        # `maintain` with zero eligible candidates and its proposals would never
        # reach the report. The sweep belongs to the pass that can report it.
        ("maintain", ["maintain", "--apply", "--no-validity"]),
        ("relatedness", ["relatedness", "build"]),
    ]


def _run_mechanical(age: int) -> dict[str, str]:
    """Run every mechanical leg best-effort; return a per-leg outcome map.

    Best-effort is the point: one failing leg must not cost the night the other
    three. Each leg is idempotent, so a retry after a released claim is safe.
    """
    from fno import _subprocess_util

    results: dict[str, str] = {}
    for name, args in _mechanical_legs(age):
        cmd = [*_subprocess_util.fno_py_cmd(), "backlog", *args]
        try:
            proc = subprocess.run(
                cmd, capture_output=True, text=True, timeout=_LEG_TIMEOUT_S
            )
        except Exception as exc:  # noqa: BLE001 - a wedged leg must not abort the pass
            results[name] = f"failed: {type(exc).__name__}: {str(exc)[:120]}"
            continue
        if proc.returncode == 0:
            results[name] = "ok"
            continue
        detail = (proc.stderr or proc.stdout or "").strip().replace("\n", " ")
        label = "partial" if proc.returncode == _PARTIAL_EXIT else "failed"
        results[name] = f"{label}: {proc.returncode}: {detail[:120]}"
    return results


def _leg_trouble(mechanical: dict[str, str]) -> list[str]:
    return sorted(name for name, outcome in mechanical.items() if outcome != "ok")


def _spawn_groom_worker(brief: str, cwd: str, model: str, day: str) -> str:
    """Launch the one-shot Sonnet groom worker; return its spawn short_id.

    ``--substrate headless`` is explicit and load-bearing: a one-shot pass wants
    no pane and no placement prompt, and `-p` is only ever reachable through the
    headless verb.
    """
    from fno import _subprocess_util

    cmd = [
        *_subprocess_util.fno_py_cmd(), "agents", "spawn",
        "--harness", "claude", "--substrate", "headless",
        "--model", model, "--cwd", cwd,
        "--timeout", str(_WORKER_TIMEOUT_S),
        "--name", f"groom-{day}", brief,
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
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            # A bare JSON string or list parses fine but has no .get; losing the
            # correlation id must not crash a pass whose worker already launched.
            if isinstance(data, dict) and (sid := str(data.get("short_id", "") or "")):
                return sid
    return "unknown"


def run_groom(
    *,
    cwd: str,
    model: str = GROOM_MODEL_DEFAULT,
    today: Optional[date] = None,
    dry_run: bool = False,
    age: int = GROOM_AGE_DEFAULT,
) -> dict[str, Any]:
    """Run today's grooming pass end to end, at most once per UTC day.

    The mechanical legs run first (under the claim, so a same-day rerun skips
    them too), then the judgment worker is dispatched. Returns a receipt whose
    ``status`` is one of ``dispatched`` | ``degraded`` (dispatched, but a
    mechanical leg did not come back clean) | ``already-ran`` | ``dry-run`` |
    ``failed``, carrying a ``mechanical`` map of per-leg outcomes. Never raises:
    a grooming pass is hygiene, so a failed dispatch reports and leaves the day
    retryable.
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

    if dry_run:
        return {
            "status": "dry-run",
            "day": day,
            "key": key,
            "model": model,
            "brief": groom_brief(day),
            # Same shape as a real pass, so a consumer never has to branch on it.
            "mechanical": {name: "pending" for name, _ in _mechanical_legs(age)},
        }

    holder = f"groom:{os.getpid()}"
    root = claims_root_for(key)

    # Read before acquiring: `acquire_claim` is idempotent for the SAME holder, so
    # a second call from one process would re-acquire and re-dispatch. The status
    # read catches that; the acquire below still catches the cross-process race.
    claim_read_error: Optional[str] = None
    try:
        if claim_status(key, root=root).get("state") in ("live", "suspect"):
            return {"status": "already-ran", "day": day, "key": key}
    except Exception as exc:  # noqa: BLE001 - an unreadable marker falls through to acquire
        # Safe (the acquire below still catches the cross-process race), but a
        # persistently unreadable claims root degrades same-process dedup, so
        # carry the reason instead of dropping it.
        claim_read_error = str(exc)[:200]

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

    # Mechanics run under the claim and BEFORE the worker: the worker's first
    # step re-derives today's proposals from the live graph, so it must see the
    # post-mechanical state.
    mechanical = _run_mechanical(age)
    brief = groom_brief(day, mechanical)

    try:
        short_id = _spawn_groom_worker(brief, cwd, model, day)
    except OSError as exc:
        # The spawn binary could not be executed, so no worker ran and no lever
        # was pulled: hand the day back rather than burn it. Report whether the
        # handback actually happened - a silently leaked marker would make every
        # retry today exit 0 as `already-ran` with nothing visibly amiss.
        receipt: dict[str, Any] = {
            "status": "failed",
            "day": day,
            "detail": str(exc)[:200],
            "mechanical": mechanical,
        }
        try:
            release_claim(key, holder, strict=True, root=root)
            receipt["released"] = True
        except Exception as rexc:  # noqa: BLE001
            receipt["released"] = False
            receipt["release_error"] = str(rexc)[:200]
        return receipt
    except Exception as exc:  # noqa: BLE001
        # A timeout or a non-zero exit both mean the worker may have RUN and
        # already applied levers - re-running today would re-apply them. Hold the
        # marker and report; exit 1 puts it in front of the operator. Burning one
        # day of hygiene is the cheaper mistake than double-mutating the graph.
        return {
            "status": "failed",
            "day": day,
            "detail": str(exc)[:200],
            "released": False,
            "mechanical": mechanical,
        }

    # A leg that fails must reach the exit code: the receipt's only other sink is
    # a launchd log with no reader.
    trouble = _leg_trouble(mechanical)
    receipt = {
        "status": "degraded" if trouble else "dispatched",
        "day": day,
        "short_id": short_id,
        "model": model,
        "mechanical": mechanical,
    }
    if trouble:
        receipt["degraded_legs"] = trouble
    if short_id == "unknown":
        # The worker launched but its correlation id was lost, so `fno agents
        # logs` has no handle. Not worth failing the pass; worth counting.
        receipt["short_id_lost"] = True
    if claim_read_error:
        receipt["claim_read_error"] = claim_read_error
    return receipt


# ── daily cadence (macOS LaunchAgent) ───────────────────────────────────────

GROOM_LABEL = "sh.fno.groom"
GROOM_HOUR_DEFAULT = 2  # local time; the UTC-keyed claim, not the clock, enforces once-a-day

_GROOM_PLIST = """\
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!--
  Daily backlog grooming pass: mechanical legs then one Sonnet judgment worker.
  Firing twice is harmless - the UTC-day claim makes the second run a no-op.
-->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>

  <key>ProgramArguments</key>
  <array>
    <string>{fno_binary}</string>
    <string>backlog</string>
    <string>groom</string>
  </array>

  <!-- launchd launches with a minimal PATH; capture install-time PATH so
       fno / gh / claude resolve without a login shell. -->
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
    <key>HOME</key>
    <string>{home}</string>
  </dict>

  <key>StartCalendarInterval</key>
  <dict>
    <key>Hour</key>
    <integer>{hour}</integer>
    <key>Minute</key>
    <integer>0</integer>
  </dict>

  <key>RunAtLoad</key>
  <false/>

  <key>ProcessType</key>
  <string>Background</string>

  <!-- A repo root, not $HOME: maintain's validity sweep resolves its git
       evidence root from the cwd, and from a non-repo it records every symbol
       "unavailable" while the leg still reports ok. -->
  <key>WorkingDirectory</key>
  <string>{workdir}</string>

  <key>StandardOutPath</key>
  <string>{log_out}</string>

  <key>StandardErrorPath</key>
  <string>{log_err}</string>
</dict>
</plist>
"""


def render_groom_plist(
    *,
    fno_binary: str,
    install_path: str,
    hour: int = GROOM_HOUR_DEFAULT,
    workdir: Optional[str] = None,
) -> str:
    """Render the daily groom LaunchAgent plist. No filesystem writes."""
    from fno.pr_watch._install import _augment_path, _xml_escape

    home = str(Path.home())
    state = Path(home) / ".fno"
    return _GROOM_PLIST.format(
        label=_xml_escape(GROOM_LABEL),
        fno_binary=_xml_escape(fno_binary),
        path=_xml_escape(_augment_path(install_path)),
        home=_xml_escape(home),
        workdir=_xml_escape(workdir or home),
        hour=int(hour),
        log_out=_xml_escape(str(state / "groom.out.log")),
        log_err=_xml_escape(str(state / "groom.err.log")),
    )


def install_groom_agent(
    *,
    launch_agents_dir: Optional[Path] = None,
    fno_binary: Optional[str] = None,
    install_path: Optional[str] = None,
    hour: int = GROOM_HOUR_DEFAULT,
    workdir: Optional[str] = None,
) -> dict[str, Any]:
    """Write the plist and bootstrap it into launchd. Returns a receipt.

    Reuses pr_watch's bounce rather than `launchctl load`, because an `fno
    update` can leave a job wedged in a state only a re-bootstrap clears. But
    WITHOUT its kickstart: grooming's tick mutates the backlog and burns the
    day's claim, so forcing one here would run a full pass at install time and
    silently contradict the plist's own `RunAtLoad=false`.
    """
    import shutil
    import sys

    from fno.pr_watch._install import bounce

    if sys.platform != "darwin":
        return {
            "status": "unsupported",
            "detail": "launchd is macOS-only; see docs/backlog-usage.md for the cron one-liner",
            "cron": f"0 {hour} * * * {shutil.which('fno') or 'fno'} backlog groom",
        }

    launch_agents_dir = launch_agents_dir or (Path.home() / "Library" / "LaunchAgents")
    fno_binary = fno_binary or shutil.which("fno") or "fno"
    install_path = install_path if install_path is not None else os.environ.get("PATH", "")

    # Captured at install time: the scheduled run has no cwd of its own, and
    # maintain's validity sweep needs a real repo to read source evidence from.
    # A refresh passes the installed value so it is not re-resolved against
    # whatever directory `fno update` happened to run from.
    if workdir is None:
        try:
            from fno.paths import resolve_repo_root

            workdir = str(resolve_repo_root())
        except Exception:  # noqa: BLE001 - falls back to $HOME, same as before
            workdir = None

    plist_path = launch_agents_dir / f"{GROOM_LABEL}.plist"
    try:
        launch_agents_dir.mkdir(parents=True, exist_ok=True)
        plist_path.write_text(
            render_groom_plist(
                fno_binary=fno_binary,
                install_path=install_path,
                hour=hour,
                workdir=workdir,
            ),
            encoding="utf-8",
        )
    except OSError as exc:
        return {"status": "failed", "detail": f"write {plist_path}: {exc}"}

    msg, rc = bounce(plist_path=plist_path, label=GROOM_LABEL, kickstart=False)
    return {
        "status": "installed" if rc == 0 else "failed",
        "plist": str(plist_path),
        "hour": hour,
        "workdir": workdir,
        "detail": msg,
    }


def _installed_agent_settings(plist_path: Path) -> tuple[Optional[int], Optional[str]]:
    """Read the installed plist's hour and working directory.

    A refresh re-renders, so without this it would silently reset an operator's
    chosen ``--hour`` back to the default. plistlib parses what launchd itself
    reads, rather than pattern-matching the XML we happened to write.
    """
    import plistlib

    try:
        data = plistlib.loads(plist_path.read_bytes())
    except Exception:  # noqa: BLE001 - a hand-edited or corrupt plist falls back to defaults
        return None, None
    hour = None
    cal = data.get("StartCalendarInterval")
    if isinstance(cal, dict) and isinstance(cal.get("Hour"), int):
        hour = cal["Hour"]
    workdir = data.get("WorkingDirectory")
    return hour, (workdir if isinstance(workdir, str) else None)


def refresh_groom_agent(*, launch_agents_dir: Optional[Path] = None) -> dict[str, Any]:
    """Re-render the installed plist onto the current binary and re-bootstrap it.

    The tail of ``fno update``: an update replaces the binary but never re-renders
    the plist, so a migration that breaks the old entry point leaves the agent
    pointing at it with no self-heal. A no-op when no groom agent is installed,
    so an operator who never ran ``--install-agent`` gets nothing. Never raises:
    the update chain calls this best-effort.
    """
    import sys

    launch_agents_dir = launch_agents_dir or (Path.home() / "Library" / "LaunchAgents")
    plist_path = launch_agents_dir / f"{GROOM_LABEL}.plist"
    if sys.platform != "darwin" or not plist_path.exists():
        return {"status": "skipped", "detail": "no groom agent installed"}

    # Carry the installed schedule and cwd forward; only the binary and PATH
    # are meant to change on a refresh.
    hour, workdir = _installed_agent_settings(plist_path)
    return install_groom_agent(
        launch_agents_dir=launch_agents_dir,
        hour=hour if hour is not None else GROOM_HOUR_DEFAULT,
        workdir=workdir,
    )
