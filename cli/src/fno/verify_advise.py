"""Ship-gate verifier advisory (W6, x-f063): run the plan-AC check against the
ship diff, record the verdict, NEVER block.

Invoked by ``fno-agents finalize``'s ship branch as::

    python3 -m fno.verify_advise --node-id <id> --plan-path <p> \
        --session-id <sid> --reason <TerminationReason> [--cwd <dir>]

Contract (AC6-ERR): every exit is 0. A spawn failure, timeout, or parse miss
records verdict ``error``; a doc ship (``DoneAdvisory``) or a plan with no
acceptance criteria records ``not_applicable``. The ``verifier_verdict`` event
(project + global events.jsonl) is canonical; the ledger row's
``verifier_verdict`` field is a denormalized convenience.
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Optional

VERDICTS = ("pass", "concerns", "fail", "error", "not_applicable")
_VERDICT_RE = re.compile(r"VERDICT:\s*(pass|concerns|fail)\b", re.IGNORECASE)
# ponytail: synchronous headless verifier at ship, 90s timeout is the ceiling.
# upgrade path if it adds real stop-hook latency: detach the spawn and record
# the verdict via a follow-up event keyed on (node, pr) instead of inline.
SPAWN_TIMEOUT_S = 90


def read_plan_acs(plan_path: Path) -> Optional[str]:
    """Extract the acceptance-criteria text from a plan file. None = no plan
    / no ACs. An exists-but-unreadable doc raises OSError so the caller
    records verdict `error`, not a false `not_applicable` (sigma P3)."""
    if not plan_path.exists():
        return None
    content = plan_path.read_text(encoding="utf-8")
    # Prefer the dedicated section; fall back to any AC-labelled lines.
    m = re.search(
        r"^#{2,4}\s*Acceptance [Cc]riteria.*?$(.*?)(?=^#{1,2}\s|\Z)",
        content,
        re.MULTILINE | re.DOTALL,
    )
    if m and m.group(1).strip():
        return m.group(1).strip()
    ac_lines = [ln for ln in content.splitlines() if re.search(r"\bAC\d+[A-Za-z-]*\b", ln)]
    return "\n".join(ac_lines) if ac_lines else None


def _git(cwd: Path, args: list[str]) -> str:
    try:
        out = subprocess.run(
            ["git", *args], cwd=cwd, capture_output=True, text=True, timeout=30
        )
        return out.stdout.strip() if out.returncode == 0 else ""
    except (OSError, subprocess.TimeoutExpired):
        return ""


def build_prompt(acs: str, diffstat: str, commits: str) -> str:
    """Compact rubric: the agents/verifier.md protocol retargeted at the SHIPPED
    plan's ACs + the origin/main...HEAD diff (not .fno/current-PLAN.md)."""
    return (
        "You are an independent verification agent. Objectively judge whether "
        "the shipped diff plausibly satisfies the plan's acceptance criteria. "
        "Be factual; do not trust claims. You may Read files and run read-only "
        "commands to check.\n\n"
        f"## Acceptance criteria\n{acs}\n\n"
        f"## Ship diffstat (origin/main...HEAD)\n{diffstat}\n\n"
        f"## Commits\n{commits}\n\n"
        "Reply with your reasoning, then a final line exactly of the form\n"
        "VERDICT: pass|concerns|fail\n"
        "(pass = criteria met; concerns = partially met or unverifiable; "
        "fail = a criterion is clearly not met)."
    )


def parse_verdict(text: str) -> Optional[str]:
    m = _VERDICT_RE.search(text or "")
    return m.group(1).lower() if m else None


def run_verifier(prompt: str, cwd: Path, session_id: str, timeout: int = SPAWN_TIMEOUT_S) -> str:
    """One-shot headless verifier via the sanctioned spawn surface (never a
    bare ``claude -p``). Returns the reply text; raises on any failure."""
    name = f"verifier-advise-{session_id[:12] or 'adhoc'}"
    out = subprocess.run(
        [
            "fno", "agents", "spawn", name, prompt,
            "--provider", "claude",
            "--substrate", "headless",
            "--model", "haiku",
            "--cwd", str(cwd),
            "--timeout", str(timeout),
        ],
        capture_output=True,
        text=True,
        timeout=timeout + 30,  # hard backstop over the spawn's own timeout
    )
    if out.returncode != 0:
        raise RuntimeError(
            f"verifier spawn exit {out.returncode}: {out.stderr.strip()[:500]}"
        )
    return out.stdout


def emit_verdict_event(
    *,
    verdict: str,
    node_id: Optional[str],
    pr_number: Optional[int],
    session_id: str,
    events_paths: list[Path],
) -> None:
    """Emit one canonical verifier_verdict event to each events log."""
    from fno.events import _build, append_event

    event = _build(
        "verifier_verdict",
        "target",
        {
            "graph_node_id": node_id,
            "pr_number": pr_number,
            "verdict": verdict,
            "source": "ship-gate",
            "session_id": session_id,
        },
    )
    # Per-path append: a project-log write failure must not starve the global
    # log calibration reads (source-independence, sigma P2).
    for p in events_paths:
        try:
            append_event(event, p)
        except Exception as exc:
            print(f"verify_advise: event emit to {p} failed: {exc}", file=sys.stderr)


def stamp_ledger(session_id: str, verdict: str, ledger_path: Path) -> bool:
    """Denormalize the verdict onto this session's ledger row (same flock as
    fno.cost._register). Returns True when a row was updated."""
    if not session_id or not ledger_path.exists():
        return False
    lock_fd = os.open("/tmp/abilities-ledger.lock", os.O_CREAT | os.O_RDWR)
    try:
        # Bounded, non-blocking acquisition: this runs synchronously inside the
        # stop hook's finalize, so an orphaned lock holder must cost seconds,
        # never wedge every session's exit (sigma P3). The event is canonical;
        # a skipped stamp is a lost convenience field, not lost data.
        deadline = time.monotonic() + 5.0
        while True:
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    print(
                        "verify_advise: ledger lock busy; skipping stamp "
                        "(event is canonical)",
                        file=sys.stderr,
                    )
                    return False
                time.sleep(0.1)
        try:
            data = json.loads(ledger_path.read_text())
        except (OSError, json.JSONDecodeError):
            return False
        entries = data.get("entries") if isinstance(data, dict) else data
        if not isinstance(entries, list):
            return False
        hit = False
        for entry in entries:
            # Match either key: new rows carry fno_id, pre-rename rows only
            # session_id (one-release dual-key window).
            if isinstance(entry, dict) and session_id in (
                entry.get("fno_id"),
                entry.get("session_id"),
            ):
                entry["verifier_verdict"] = verdict
                hit = True
        if not hit:
            return False
        tmp_fd, tmp_path = tempfile.mkstemp(dir=ledger_path.parent, suffix=".tmp")
        try:
            with os.fdopen(tmp_fd, "w") as f:
                f.write(json.dumps(data, indent=2) + "\n")
            os.replace(tmp_path, ledger_path)
        except Exception:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass
            raise
        return True
    finally:
        fcntl.flock(lock_fd, fcntl.LOCK_UN)
        os.close(lock_fd)


def recorded_verdict(session_id: str, events_path: Path) -> Optional[str]:
    """The verdict this session already recorded in an events log, else None
    (AC6-HP exactly-one): a retried finalize fire (e.g. after a partial-failure
    session_finalize_failed) must not re-spend on a spawn, but it DOES reuse
    the recorded verdict to backfill any sink the prior run failed to write
    (codex P2: skipping outright left a missing global event / ledger field
    unrepaired forever)."""
    if not session_id or not events_path.exists():
        return None
    verdict = None
    try:
        for line in events_path.read_text(encoding="utf-8").splitlines():
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (
                isinstance(e, dict)
                and e.get("type") == "verifier_verdict"
                and isinstance(e.get("data"), dict)
                and e["data"].get("session_id") == session_id
            ):
                verdict = e["data"].get("verdict") or verdict
    except OSError:
        return None
    return verdict


def already_recorded(session_id: str, events_path: Path) -> bool:
    return recorded_verdict(session_id, events_path) is not None


def _pr_number(cwd: Path) -> Optional[int]:
    try:
        out = subprocess.run(
            ["gh", "pr", "view", "--json", "number", "--jq", ".number"],
            cwd=cwd, capture_output=True, text=True, timeout=30,
        )
        return int(out.stdout.strip()) if out.returncode == 0 else None
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return None


def decide_verdict(*, reason: str, plan_path: str, cwd: Path, session_id: str) -> str:
    """The advisory core: node-type guard -> AC read -> spawn -> parse.
    Never raises; every failure collapses to 'error' (AC6-ERR)."""
    if reason == "DoneAdvisory":
        return "not_applicable"  # doc ship: the rubric is code-shaped (AC6-EDGE)
    if not plan_path:
        return "not_applicable"  # quick/ad-hoc ship with no plan bound
    try:
        acs = read_plan_acs(
            cwd / plan_path if not Path(plan_path).is_absolute() else Path(plan_path)
        )
    except OSError as exc:  # exists but unreadable: a real fault, not "no ACs"
        print(f"verify_advise: plan unreadable: {exc}", file=sys.stderr)
        return "error"
    if not acs:
        return "not_applicable"
    try:
        diffstat = _git(cwd, ["diff", "--stat", "origin/main...HEAD"]) or "(diff unavailable)"
        commits = _git(cwd, ["log", "--oneline", "origin/main..HEAD"]) or "(log unavailable)"
        reply = run_verifier(build_prompt(acs, diffstat, commits), cwd, session_id)
        return parse_verdict(reply) or "error"
    except Exception as exc:  # spawn died / timeout / anything: advisory never raises
        print(f"verify_advise: verifier failed: {exc}", file=sys.stderr)
        return "error"


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(description="Ship-gate verifier advisory (never blocks)")
    parser.add_argument("--node-id", default="")
    parser.add_argument("--plan-path", default="")
    parser.add_argument("--session-id", default="")
    parser.add_argument("--reason", default="")
    parser.add_argument("--cwd", default=".")
    parser.add_argument("--events", help="project events.jsonl override (tests)")
    parser.add_argument("--global-events", help="global events.jsonl override (tests)")
    parser.add_argument("--ledger", help="ledger.json override (tests)")
    args = parser.parse_args(argv)

    cwd = Path(args.cwd).resolve()
    project_events = Path(args.events) if args.events else cwd / ".fno" / "events.jsonl"

    prior = recorded_verdict(args.session_id, project_events)
    if prior:
        # Exactly-once on the SPAWN; recording stays self-healing. Reuse the
        # recorded verdict and fall through so any sink the prior run failed
        # to write (global log, ledger field) is backfilled below.
        print(f"verify_advise: verdict already recorded ({prior}); backfilling missing sinks")
        verdict = prior
    else:
        verdict = decide_verdict(
            reason=args.reason, plan_path=args.plan_path, cwd=cwd, session_id=args.session_id
        )

    # Record: event first (canonical), ledger field second (denormalized).
    # Each step best-effort and per-sink idempotent; a failure still exits 0.
    try:
        from fno import paths as _paths

        global_events = (
            Path(args.global_events)
            if args.global_events
            else _paths.ledger_json().parent / "events.jsonl"
        )
        events_paths = [project_events]
        if global_events != project_events:
            events_paths.append(global_events)
        missing = [p for p in events_paths if not already_recorded(args.session_id, p)]
        if missing:
            emit_verdict_event(
                verdict=verdict,
                node_id=args.node_id or None,
                pr_number=_pr_number(cwd),
                session_id=args.session_id,
                events_paths=missing,
            )
    except Exception as exc:
        print(f"verify_advise: event emit failed: {exc}", file=sys.stderr)

    try:
        from fno import paths as _paths

        ledger = Path(args.ledger) if args.ledger else _paths.ledger_json()
        stamp_ledger(args.session_id, verdict, ledger)
    except Exception as exc:
        print(f"verify_advise: ledger stamp failed: {exc}", file=sys.stderr)

    print(f"verify_advise: verdict={verdict}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
