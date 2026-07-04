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
from pathlib import Path
from typing import Optional

VERDICTS = ("pass", "concerns", "fail", "error", "not_applicable")
_VERDICT_RE = re.compile(r"VERDICT:\s*(pass|concerns|fail)\b", re.IGNORECASE)
# ponytail: synchronous headless verifier at ship, 90s timeout is the ceiling.
# upgrade path if it adds real stop-hook latency: detach the spawn and record
# the verdict via a follow-up event keyed on (node, pr) instead of inline.
SPAWN_TIMEOUT_S = 90


def read_plan_acs(plan_path: Path) -> Optional[str]:
    """Extract the acceptance-criteria text from a plan (dir -> 00-INDEX.md,
    else the file itself - same resolution finalize uses). None = no ACs."""
    doc = plan_path / "00-INDEX.md" if plan_path.is_dir() else plan_path
    try:
        content = doc.read_text(encoding="utf-8")
    except OSError:
        return None
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
    for p in events_paths:
        append_event(event, p)


def stamp_ledger(session_id: str, verdict: str, ledger_path: Path) -> bool:
    """Denormalize the verdict onto this session's ledger row (same flock as
    fno.cost._register). Returns True when a row was updated."""
    if not session_id or not ledger_path.exists():
        return False
    lock_fd = os.open("/tmp/abilities-ledger.lock", os.O_CREAT | os.O_RDWR)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        try:
            data = json.loads(ledger_path.read_text())
        except (OSError, json.JSONDecodeError):
            return False
        entries = data.get("entries") if isinstance(data, dict) else data
        if not isinstance(entries, list):
            return False
        hit = False
        for entry in entries:
            if isinstance(entry, dict) and entry.get("session_id") == session_id:
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


def already_recorded(session_id: str, events_path: Path) -> bool:
    """True when this session already has a verifier_verdict event (AC6-HP
    exactly-one): a retried finalize fire (e.g. after a partial-failure
    session_finalize_failed) must not double-emit or re-spend on a spawn."""
    if not session_id or not events_path.exists():
        return False
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
                return True
    except OSError:
        return False
    return False


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
    acs = read_plan_acs(cwd / plan_path if not Path(plan_path).is_absolute() else Path(plan_path))
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

    if already_recorded(args.session_id, project_events):
        print("verify_advise: verdict already recorded for this session; skipping")
        return 0

    verdict = decide_verdict(
        reason=args.reason, plan_path=args.plan_path, cwd=cwd, session_id=args.session_id
    )

    # Record: event first (canonical), ledger field second (denormalized).
    # Each step best-effort; a recording failure still exits 0.
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
        emit_verdict_event(
            verdict=verdict,
            node_id=args.node_id or None,
            pr_number=_pr_number(cwd),
            session_id=args.session_id,
            events_paths=events_paths,
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
