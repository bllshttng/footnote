#!/usr/bin/env python3
"""Measure how much of the real mail corpus the style rules refuse.

Reads ~/.fno/bus/messages.jsonl and its rotated segments, strips the delivered
envelope (the <fno_mail> open/close lines and every appended "-- " trailer),
drops the bodies that already bypass the gate (control: lane, style-exception
marker), then runs the checker over the authored bodies that remain.

The strip carries a positive control: "your crown" and "peer mail:" exist only
in the trailers, so near-zero occurrences after the strip prove the strip
worked. Keeping the envelope reads ~31.3% because the trailers themselves
break rules 1 and 2.

Lives in scripts/, never in cli/src/fno: the Python tree there is shrink-only
against PY_TREE_ALLOWANCE.
"""
from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))

from fno import style  # noqa: E402

CONTROL_POSITIVE_MARKERS = ("your crown", "peer mail:")


def corpus_paths() -> list[Path]:
    bus = Path.home() / ".fno" / "bus"
    return sorted(bus.glob("messages.jsonl*"))


def strip_envelope(body: str) -> str:
    """Remove the delivered envelope: open/close tag lines and "-- " trailers."""
    kept = [
        line
        for line in body.splitlines()
        if not line.startswith("<fno_mail")
        and line.strip() != "</fno_mail>"
        and not line.startswith("-- ")
    ]
    return "\n".join(kept).strip()


def load_bodies(sender: str | None) -> tuple[list[str], str, str, int]:
    """Return (authored bodies, first ts, last ts, total send rows)."""
    bodies: list[str] = []
    timestamps: list[str] = []
    total = 0
    for path in corpus_paths():
        for line in path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if row.get("kind") != "send" or not row.get("body"):
                continue
            if sender is not None and row.get("from") != sender:
                continue
            total += 1
            timestamps.append(row.get("ts", ""))
            body = strip_envelope(row["body"])
            if not body:
                continue
            first = body.splitlines()[0].strip().lower()
            if first.startswith("control:") or style.has_exception(body):
                continue
            bodies.append(body)
    return bodies, min(timestamps, default=""), max(timestamps, default=""), total


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--sender", help="only measure mail from this bus handle")
    args = parser.parse_args()

    bodies, first_ts, last_ts, total = load_bodies(args.sender)
    text = "\n".join(bodies)
    residue = {marker: text.count(marker) for marker in CONTROL_POSITIVE_MARKERS}

    per_rule: Counter[int] = Counter()
    refused_1_to_6 = refused_8 = refused_any = 0
    for body in bodies:
        rules = {v.rule for v in style.check(body, surface="mail")}
        if not rules:
            continue
        refused_any += 1
        refused_1_to_6 += bool(rules & {1, 2, 3, 4, 5, 6})
        refused_8 += 8 in rules
        per_rule.update(rules)

    n = len(bodies)
    pct = lambda count: f"{100 * count / n:.1f}%" if n else "n/a"  # noqa: E731
    print(f"corpus send rows: {total}; authored bodies checked: {n}")
    print(f"date range: {first_ts} .. {last_ts}")
    print(f"strip positive control (must be near zero): {residue}")
    print(f"refused by rules 1-6: {refused_1_to_6} ({pct(refused_1_to_6)})")
    print(f"refused by rule 8:    {refused_8} ({pct(refused_8)})")
    print(f"refused by any rule:  {refused_any} ({pct(refused_any)})")
    for rule in sorted(per_rule):
        name = style.RULE_NAMES.get(rule, "?")
        print(f"  rule {rule} ({name}): {per_rule[rule]} messages")
    return 0


if __name__ == "__main__":
    sys.exit(main())
