"""Pre-PR stale-base guard.

A worktree branched from a stale local HEAD ships a PR full of phantom
deletions (changes you never made appear as reverts), costing a manual
rebase+repush cycle. ``fno worktree ensure`` bases new worktrees off
``origin/main``, but the EnterWorktree and manual-worktree paths bypass it, so
guard at PR-creation time where every path converges.

One implementation (:func:`check_stale_base`), two call sites: the ``/pr create``
router shells ``fno pr base-check``; ``fno worker ship`` imports the function
directly. The bypass and the staleness rule live only here so both sites behave
identically.

Exit-code contract (callers branch on codes, not text):
    0  pass       (fresh base / behind-count 0 / bypass / fail-open)
    3  stale       (merge-base > max_hours of missing main history behind tip)
    4  unrelated    (no merge-base with the base ref)
"""
from __future__ import annotations

import datetime as dt
import json
import os
import re
import stat
import sys
from pathlib import Path
from typing import Mapping, Optional, Tuple

from fno.pr._proc import ToolMissing, run

BASE_DEFAULT = "origin/main"
MAX_HOURS = 24
BYPASS_ENV = "FNO_PR_BASE_OK"
BYPASS_VALUE = "stale-acknowledged"

OK = 0
REFUSED_STALE = 3
UNRELATED = 4

_PREFLIGHT_GATE_SCOPE = frozenset(
    {
        "smoke",
        "rustfmt:fno-agents",
        "rustfmt:fno",
        "cargo-test:fno-agents",
        "cargo-test:fno",
        "squads-leak-guard:fno",
    }
)


def _event_timestamp(value: object) -> Optional[dt.datetime]:
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.astimezone(dt.timezone.utc)


def hosted_ci_decision(
    *,
    declared_none: bool,
    workflow_state: str,
    candidate_sha: str,
    observed_sha: Optional[str],
    checks: Optional[list[dict]],
) -> dict:
    """Reduce hosted-CI policy and observations without collapsing uncertainty."""
    counts = {"total": 0, "passed": 0, "failed": 0, "pending": 0}
    base = {
        "declared_none": declared_none,
        "workflow_state": workflow_state,
        "counts": counts,
    }
    if re.fullmatch(r"[0-9a-f]{40}", candidate_sha, re.IGNORECASE) is None:
        return {**base, "satisfied": False, "result": "unavailable"}
    if workflow_state not in {"present", "absent", "unavailable"}:
        return {**base, "satisfied": False, "result": "unavailable"}
    if workflow_state == "unavailable" or checks is None:
        return {**base, "satisfied": False, "result": "unavailable"}
    if checks and observed_sha is None:
        return {**base, "satisfied": False, "result": "unavailable"}
    if observed_sha is not None:
        if re.fullmatch(r"[0-9a-f]{40}", observed_sha, re.IGNORECASE) is None:
            return {**base, "satisfied": False, "result": "unavailable"}
        if observed_sha.lower() != candidate_sha.lower():
            return {**base, "satisfied": False, "result": "stale"}
    for check in checks:
        if not isinstance(check, dict):
            return {**base, "satisfied": False, "result": "unavailable"}
        counts["total"] += 1
        bucket = str(check.get("bucket") or "").lower()
        status = str(check.get("status") or "").upper()
        conclusion = str(check.get("conclusion") or check.get("state") or "").upper()
        if bucket in {"fail", "cancel"} or conclusion in {
            "FAILURE",
            "TIMED_OUT",
            "CANCELLED",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
            "STALE",
            "ERROR",
        }:
            counts["failed"] += 1
        elif (
            bucket in {"pass", "skipping"}
            or (status in {"", "COMPLETED"} and conclusion in {"SUCCESS", "NEUTRAL", "SKIPPED"})
        ):
            counts["passed"] += 1
        else:
            counts["pending"] += 1
    if counts["failed"]:
        return {**base, "satisfied": False, "result": "failed"}
    if counts["pending"]:
        return {**base, "satisfied": False, "result": "pending"}
    if counts["total"]:
        return {**base, "satisfied": True, "result": "passed"}
    if declared_none and workflow_state == "absent":
        return {**base, "satisfied": True, "result": "not_configured"}
    return {**base, "satisfied": False, "result": "pending"}


def hosted_workflow_state(cwd: Path) -> str:
    """Return present, absent, or unavailable for hosted workflow discovery."""
    workflows = cwd / ".github" / "workflows"
    try:
        workflows.stat()
    except FileNotFoundError:
        return "absent"
    except OSError:
        return "unavailable"
    try:
        if not workflows.is_dir():
            return "unavailable"
        for child in workflows.iterdir():
            if child.suffix.lower() not in {".yml", ".yaml"}:
                continue
            try:
                if stat.S_ISREG(child.stat().st_mode):
                    return "present"
            except OSError:
                return "unavailable"
        return "absent"
    except FileNotFoundError:
        return "unavailable"
    except OSError:
        return "unavailable"


def verification_decision(candidate_sha: str, event_paths: list[Path]) -> dict:
    """Select the newest valid receipt across deduped, unordered journals."""
    from fno.events import ValidationError, validate

    if re.fullmatch(r"[0-9a-f]{40}", candidate_sha, re.IGNORECASE) is None:
        raise ValueError("candidate_sha must be a full 40-hex commit")
    seen_paths: set[str] = set()
    seen_events: set[str] = set()
    receipts: list[tuple[dt.datetime, str, dict]] = []
    malformed = 0
    unreadable = 0
    for raw_path in event_paths:
        path = Path(raw_path).expanduser()
        try:
            path_key = str(path.resolve())
        except OSError:
            path_key = os.path.abspath(path)
        if path_key in seen_paths:
            continue
        seen_paths.add(path_key)
        if not path.exists():
            continue
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeError):
            unreadable += 1
            continue
        for line in lines:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                continue
            if not isinstance(event, dict) or event.get("type") != "verification_receipt":
                continue
            try:
                validate(event)
            except ValidationError:
                malformed += 1
                continue
            parsed_ts = _event_timestamp(event.get("ts"))
            if parsed_ts is None:
                malformed += 1
                continue
            signature = json.dumps(event, sort_keys=True, separators=(",", ":"))
            if signature in seen_events:
                continue
            seen_events.add(signature)
            receipts.append((parsed_ts, signature, event))
    coverage = {
        "complete": malformed == 0 and unreadable == 0,
        "malformed_lines": malformed,
        "unreadable_paths": unreadable,
        "deduped_events": len(receipts),
    }
    receipts.sort(key=lambda item: (item[0], item[1]))
    exact = [item for item in receipts if item[2]["data"]["candidate_sha"].lower() == candidate_sha.lower()]
    if exact:
        event = exact[-1][2]
        data = event["data"]
        command = data["command"]
        environment = data["environment"]
        producer = data["producer"]
        command_path = Path(command[0]).as_posix()
        trusted_producer = (
            event["source"] == "target"
            and producer["kind"] == "preflight"
            and producer["id"].startswith(f"{environment['host']}:")
            and environment["runner"] == "scripts/ci/preflight.sh"
            and (
                command_path == "scripts/ci/preflight.sh"
                or command_path.endswith("/scripts/ci/preflight.sh")
            )
            and frozenset(data["scope"]) == _PREFLIGHT_GATE_SCOPE
            and len(data["scope"]) == len(_PREFLIGHT_GATE_SCOPE)
        )
        return {
            "satisfied": (
                coverage["complete"]
                and trusted_producer
                and data["mode"] == "full"
                and data["result"] == "passed"
            ),
            "mode": data["mode"],
            "result": data["result"],
            "receipt": event,
            "coverage": coverage,
        }
    if receipts:
        event = receipts[-1][2]
        return {
            "satisfied": False,
            "mode": event["data"]["mode"],
            "result": "stale",
            "receipt": event,
            "coverage": coverage,
        }
    return {
        "satisfied": False,
        "mode": None,
        "result": "unavailable",
        "receipt": None,
        "coverage": coverage,
    }


def verification_event_paths(*, cwd: Optional[str] = None) -> tuple[list[Path], list[str]]:
    """Return event journals plus discovery errors that make coverage uncertain."""
    repo = Path(cwd or os.getcwd()).resolve()
    errors: list[str] = []
    try:
        discovered = _git(["rev-parse", "--show-toplevel"], str(repo))
        root = Path(discovered.stdout.strip()).resolve() if discovered.returncode == 0 else repo
    except (OSError, ValueError) as exc:
        root = repo
        errors.append(f"repository root discovery failed: {exc}")
    paths = [root / ".fno" / "events.jsonl", Path.home() / ".fno" / "events.jsonl"]
    try:
        from fno.paths import ledger_json

        raw = json.loads(ledger_json().read_text(encoding="utf-8"))
        rows = raw.get("entries", raw) if isinstance(raw, dict) else raw
        for row in rows if isinstance(rows, list) else []:
            if not isinstance(row, dict):
                continue
            delivery_root = row.get("root_path")
            if isinstance(delivery_root, str) and delivery_root:
                paths.append(Path(delivery_root).expanduser() / ".fno" / "events.jsonl")
            canonical = row.get("canonical_root_path")
            if isinstance(canonical, str) and canonical:
                salvage = Path(canonical).expanduser() / ".fno" / "salvage"
                if salvage.is_dir():
                    paths.extend(sorted(salvage.glob("*/events.jsonl")))
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        errors.append(f"delivery journal discovery failed: {exc}")
    return paths, errors


def check_verification_evidence(
    *,
    cwd: Optional[str] = None,
    candidate_sha: Optional[str] = None,
    event_paths: Optional[list[Path]] = None,
) -> dict:
    repo = cwd or os.getcwd()
    if candidate_sha is None:
        result = _git(["rev-parse", "HEAD"], repo)
        candidate_sha = result.stdout.strip() if result.returncode == 0 else ""
    if re.fullmatch(r"[0-9a-f]{40}", candidate_sha, re.IGNORECASE) is None:
        return {
            "satisfied": False,
            "mode": None,
            "result": "unavailable",
            "receipt": None,
            "coverage": {"complete": False, "error": "candidate SHA unavailable"},
        }
    discovery_errors: list[str] = []
    if event_paths is None:
        event_paths, discovery_errors = verification_event_paths(cwd=repo)
    decision = verification_decision(candidate_sha, event_paths)
    if discovery_errors:
        decision["coverage"]["complete"] = False
        decision["coverage"]["discovery_errors"] = discovery_errors
        decision["satisfied"] = False
    return decision


def run_evidence_check(*, cwd: Optional[str] = None) -> int:
    decision = check_verification_evidence(cwd=cwd)
    print(json.dumps(decision, separators=(",", ":")))
    return 0 if decision["satisfied"] else 1


def _git(args, cwd: str):
    return run(["git", *args], cwd=cwd)


def _committer_epoch(ref: str, cwd: str) -> Optional[int]:
    res = _git(["show", "-s", "--format=%ct", ref], cwd)
    out = res.stdout.strip()
    if res.returncode != 0 or not out:
        return None
    try:
        return int(out)
    except ValueError:
        return None


def _split_base(base: str, cwd: str) -> Tuple[str, str]:
    """``origin/main`` -> ``("origin", "main")``. A leading segment is treated as
    the remote ONLY when it matches a real git remote; otherwise the whole ref is
    fetched from ``origin``, so a slash-containing branch (``releases/v1``) is not
    mis-split into a phantom remote ``releases`` (which would fail the fetch and
    silently skip the guard)."""
    if "/" in base:
        remote, ref = base.split("/", 1)
        res = _git(["remote"], cwd)
        if res.returncode == 0:
            remotes = {ln.strip() for ln in res.stdout.splitlines() if ln.strip()}
            if remote in remotes:
                return remote, ref
    return "origin", base


def _stale_message(span_hours: float, base: str) -> str:
    days = span_hours / 24.0
    return (
        f"stale base: your branch's merge-base with {base} is {days:.1f} days "
        f"behind {base}.\n"
        "PRs from stale bases show phantom deletions (changes you never made "
        "appear as reverts).\n"
        "fix:    fno pr rebase --base=origin/main   (handles fetch + conflicts "
        "+ repush guidance)\n"
        "bypass: FNO_PR_BASE_OK=stale-acknowledged  (deliberate old-base PR; "
        "emits gate_escape)"
    )


def _unrelated_message(base: str) -> str:
    return (
        f"unrelated histories: your branch has no common ancestor with {base}. "
        "That is a bigger problem than a stale base - check you branched from the "
        "right place before opening a PR."
    )


def _emit_bypass(cwd: Optional[str], env: Mapping[str, str], events_path=None) -> None:
    """Fire-and-forget gate_escape on a deliberate bypass. Never blocks (AC2-FR)."""
    try:
        from fno.events.gate_escape import default_dedup_key, emit_gate_escape

        emit_gate_escape(
            "stale-base",
            detail="pre-pr base-check bypass",
            dedup_key=default_dedup_key("stale-base", env),
            events_path=events_path,
            cwd=cwd,
        )
    except Exception:
        pass  # ponytail: telemetry must never block a deliberate ship (AC2-FR)


def check_stale_base(
    base: str = BASE_DEFAULT,
    max_hours: int = MAX_HOURS,
    *,
    cwd: Optional[str] = None,
    env: Optional[Mapping[str, str]] = None,
    events_path=None,
) -> Tuple[int, Optional[str]]:
    """Return ``(exit_code, message)`` without printing or exiting.

    Message is a human string for stderr (a warning on fail-open, a refusal on
    stale/unrelated) or ``None`` on a clean pass.
    """
    e = os.environ if env is None else env
    repo = cwd or os.getcwd()

    # Bypass honored here so both call sites are identical. Only the literal
    # string bypasses; any other value falls through and refuses (AC1-FR).
    if e.get(BYPASS_ENV, "") == BYPASS_VALUE:
        _emit_bypass(repo, e, events_path)
        return OK, None

    try:
        # _split_base shells `git remote`, so it shares the fail-open ToolMissing
        # guard with the fetch. Compare against the remote-tracking ref we
        # actually fetch: `git fetch` advances refs/remotes/<remote>/<ref>, never
        # a local branch, so a bare `--base main` must still read origin/main -
        # comparing local `main` would read a ref the fetch never touched and
        # pass exactly when it should refuse.
        remote, ref = _split_base(base, repo)
        compare = f"{remote}/{ref}"
        fetch = _git(["fetch", remote, ref, "--quiet"], repo)
    except ToolMissing:
        return OK, "could not run git; stale-base check skipped"
    if fetch.returncode != 0:
        # Fail-open: a network flake must not block a ship, and gh pr create is
        # about to make the real network test anyway.
        return OK, f"could not refresh {compare}; stale-base check skipped"

    mb = _git(["merge-base", "HEAD", compare], repo)
    mb_sha = mb.stdout.strip()
    if mb.returncode != 0 or not mb_sha:
        return UNRELATED, _unrelated_message(compare)

    behind = _git(["rev-list", "--count", f"{mb_sha}..{compare}"], repo)
    if behind.returncode != 0:
        # Fail-open on an unexpected rev-list error rather than misreading empty
        # stdout as behind-count 0 (a false "up to date" pass).
        return OK, "could not determine behind count; stale-base check skipped"
    if behind.stdout.strip() == "0":
        return OK, None  # up to date; merge-base age is irrelevant

    tip_ts = _committer_epoch(compare, repo)
    mb_ts = _committer_epoch(mb_sha, repo)
    if tip_ts is None or mb_ts is None:
        return OK, "could not read commit dates; stale-base check skipped"

    span_hours = (tip_ts - mb_ts) / 3600.0
    if span_hours > max_hours:
        return REFUSED_STALE, _stale_message(span_hours, compare)
    return OK, None


def run_base_check(base: str = BASE_DEFAULT, *, cwd: Optional[str] = None) -> int:
    """CLI entry: run the check, print any message to stderr, return the code."""
    code, msg = check_stale_base(base=base, cwd=cwd)
    if msg:
        sys.stderr.write(msg.rstrip("\n") + "\n")
    return code
