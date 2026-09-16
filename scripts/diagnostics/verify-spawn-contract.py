#!/usr/bin/env python3
"""Read-only verifier for the spawn-door runtime proof.

The integration runner writes correlated evidence to the plan's
`.artifacts/spawn-contract-proof.json`; this verifier reads that file and the
journal/rows it references, and prints the successful scenario ids. It never
launches anything and never writes.

A refusal names exactly what is missing:
- no evidence file, or older than --max-age-seconds (default 24h);
- the implementation SHA recorded in the evidence differs from the running
  tree's HEAD;
- a required scenario id absent from the evidence;
- an entry whose proof is a self-asserted ``passed: true`` without the
  correlated journal/row references;
- a referenced journal file that cannot be read;
- a resolved-identity scenario whose full session id is missing;
- spawn ids uncorrelated across accepted/birth records, or reused across
  scenarios.

Usage:
  python3 scripts/diagnostics/verify-spawn-contract.py \
      --plan /path/to/plan.md [--max-age-seconds 86400]
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path

REQUIRED_SCENARIOS = (
    "warm-daemon-session-a",
    "warm-daemon-session-b",
    "mission-drain-autonomous",
    "blueprinter-autonomous",
    "shell-tty-operator",
    "standalone-test",
    "session-launched-test",
    "pane",
    "keeper-thread",
    "thread",
    "headless",
    "late-identity-binding",
    "failure-after-launch",
)

REQUIRED_FIELDS = ("spawn_id", "journal", "rows")


def _head_sha() -> str:
    try:
        out = subprocess.run(
            ["/usr/bin/git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        if out.returncode == 0:
            return out.stdout.strip()
    except (OSError, subprocess.SubprocessError):
        pass
    try:
        out = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        if out.returncode == 0:
            return out.stdout.strip()
    except (OSError, subprocess.SubprocessError):
        pass
    return ""


def _plan_artifacts_dir(plan_path: Path) -> Path:
    return plan_path.parent / (plan_path.name + ".artifacts")


def _load_evidence(path: Path) -> dict:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise SystemExit(
            f"REFUSED: no evidence file at {path}; the real-path proof matrix "
            "has not run. Run the integration runner, then re-verify."
        )
    except (OSError, UnicodeDecodeError) as exc:
        raise SystemExit(f"REFUSED: evidence at {path} is unreadable: {exc}")
    except json.JSONDecodeError as exc:
        raise SystemExit(f"REFUSED: evidence at {path} is malformed JSON: {exc}")
    if not isinstance(raw, dict):
        raise SystemExit("REFUSED: evidence top level must be an object")
    return raw


def _check_age(evidence_path: Path, max_age: float) -> None:
    try:
        age = time.time() - evidence_path.stat().st_mtime
    except FileNotFoundError:
        raise SystemExit(
            f"REFUSED: no evidence file at {evidence_path}; the real-path proof "
            "matrix has not run. Run the integration runner, then re-verify."
        )
    if age > max_age:
        raise SystemExit(
            f"REFUSED: evidence is {age / 3600:.1f}h old (bound {max_age / 3600:.1f}h); "
            "stale proof is not proof. Re-run the integration runner."
        )


def _check_sha(evidence: dict) -> str:
    recorded = str(evidence.get("implementation_sha") or "")
    actual = _head_sha()
    if not recorded:
        raise SystemExit(
            "REFUSED: evidence carries no implementation_sha; a proof without "
            "the tree it ran on verifies nothing."
        )
    if actual and recorded != actual:
        raise SystemExit(
            f"REFUSED: evidence sha {recorded[:12]} does not match the running "
            f"tree's HEAD {actual[:12]}; the proof names a different build."
        )
    return recorded


def _check_scenarios(evidence: dict) -> list[str]:
    scenarios = evidence.get("scenarios")
    if not isinstance(scenarios, dict) or not scenarios:
        raise SystemExit("REFUSED: evidence carries no scenarios object")
    missing = [s for s in REQUIRED_SCENARIOS if s not in scenarios]
    if missing:
        raise SystemExit(
            "REFUSED: required scenarios absent from the evidence: "
            + ", ".join(missing)
        )
    return sorted(scenarios)


def _check_entry(scenario: str, entry: dict, plan_dir: Path) -> str:
    for field in REQUIRED_FIELDS:
        if not entry.get(field):
            raise SystemExit(
                f"REFUSED: scenario {scenario} is missing {field}; a "
                "self-asserted pass is never evidence."
            )
    spawn_id = str(entry["spawn_id"])
    if not spawn_id.startswith("sp-"):
        raise SystemExit(
            f"REFUSED: scenario {scenario} names spawn_id {spawn_id!r}, which "
            "is not a coordinator id (sp-<hex>)"
        )
    journal_ref = Path(str(entry["journal"]))
    if not journal_ref.is_absolute():
        journal_ref = plan_dir / journal_ref
    try:
        text = journal_ref.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        raise SystemExit(
            f"REFUSED: scenario {scenario} journal {journal_ref} unreadable: {exc}"
        )
    accepted = birth = 0
    for line in text.splitlines():
        if spawn_id not in line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = record.get("type") or record.get("kind")
        if kind == "agent_spawn_accepted":
            accepted += 1
        elif kind == "agent_spawned":
            birth += 1
    if accepted != 1 or birth != 1:
        raise SystemExit(
            f"REFUSED: scenario {scenario} spawn_id {spawn_id} correlates to "
            f"{accepted} accepted / {birth} birth journal records (need 1 and 1)"
        )
    # A resolved-identity scenario must name the full child session id.
    if str(entry.get("identity_status") or "") == "resolved":
        if not entry.get("child_session_id"):
            raise SystemExit(
                f"REFUSED: scenario {scenario} claims identity_status=resolved "
                "without a full child session id"
            )
    return spawn_id


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--plan",
        required=True,
        help="Path to the plan doc; evidence lives beside it in "
        "<plan>.artifacts/spawn-contract-proof.json",
    )
    parser.add_argument(
        "--max-age-seconds",
        type=float,
        default=86400,
        help="Reject evidence older than this (default 24h)",
    )
    args = parser.parse_args()

    plan_path = Path(args.plan).resolve()
    if not plan_path.exists():
        raise SystemExit(f"REFUSED: plan not found at {plan_path}")
    plan_dir = _plan_artifacts_dir(plan_path)
    evidence_path = plan_dir / "spawn-contract-proof.json"

    _check_age(evidence_path, args.max_age_seconds)
    evidence = _load_evidence(evidence_path)
    _check_sha(evidence)
    scenarios = _check_scenarios(evidence)

    seen_ids: dict[str, str] = {}
    for scenario in scenarios:
        entry = evidence["scenarios"][scenario]
        if not isinstance(entry, dict):
            raise SystemExit(
                f"REFUSED: scenario {scenario} entry is not an object; a bare "
                '"passed": true is never evidence'
            )
        spawn_id = _check_entry(scenario, entry, plan_dir)
        if spawn_id in seen_ids:
            raise SystemExit(
                f"REFUSED: scenarios {seen_ids[spawn_id]} and {scenario} share "
                f"spawn_id {spawn_id}; uncorrelated duplicates are not proof"
            )
        seen_ids[spawn_id] = scenario

    print(f"spawn-contract: verified {len(scenarios)} scenarios (sha {evidence['implementation_sha'][:12]})")
    for scenario in scenarios:
        print(f"  ok: {scenario} ({seen_ids[evidence['scenarios'][scenario]['spawn_id']]})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
