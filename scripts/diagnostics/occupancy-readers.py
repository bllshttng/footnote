#!/usr/bin/env python3
"""Census the node-claim occupancy readers under cli/src/fno (x-74aa).

Every site that reads a ``node:`` claim answers "is someone working this
node?". The wrong-tree default answered ``free`` from the repo space while
the global root held a live claim, and absence licensed a double dispatch.
A new reader fails the gate until it registers in the allowlist, which
forces its author through the four-way control
(``cli/tests/unit/test_occupancy_four_way_control.py``) to pick a word.

Two patterns, the same ones the plan's surface block was measured with:
  1. an inline ``claim_status(f"node:{...}"`` call
  2. a ``key = f"node:{...}"`` binding followed within 12 lines by a
     ``claim_status(key`` call

The self-test plants a known-bad site and proves the detector detects it
before the real scan may report clean - a guard that cannot prove it
detects is the false zero this node is about.
"""
from __future__ import annotations

import argparse
import re
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SRC = REPO / "cli" / "src" / "fno"
ALLOWLIST = REPO / "cli" / "tests" / "unit" / "occupancy_readers_allowlist.txt"

_INLINE = re.compile(r'claim_status\(f"node:\{')
_BINDING = re.compile(r'^\s*(\w+)\s*=\s*f"node:\{')

# A planted site every detector must see (the self-test's positive control).
BAD_SITE = 'info = claim_status(f"node:{node_id}")\n'


#: Files the census could not read. A file excluded silently reads as clean,
#: which is the false zero this node is about - the gate refuses on any.
UNREADABLE: list[str] = []


def scan(root: Path) -> list[tuple[str, int, str]]:
    """(relpath, lineno, stripped source line) for every occupancy read."""
    hits: list[tuple[str, int, str]] = []
    for py in sorted(root.rglob("*.py")):
        try:
            lines = py.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError) as exc:
            UNREADABLE.append(f"{py}: {exc}")
            continue
        rel = py.relative_to(REPO) if py.is_relative_to(REPO) else py
        for i, line in enumerate(lines):
            if _INLINE.search(line):
                hits.append((str(rel), i + 1, line.strip()))
                continue
            m = _BINDING.match(line)
            if m:
                tail = "\n".join(lines[i + 1 : i + 13])
                if re.search(rf"claim_status\(\s*{re.escape(m.group(1))}\b", tail):
                    hits.append((str(rel), i + 1, line.strip()))
    return hits


def _allowlisted() -> set[str]:
    if not ALLOWLIST.exists():
        return set()
    return {
        line.strip()
        for line in ALLOWLIST.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.startswith("#")
    }


def self_test() -> bool:
    """Plant a known-bad tree, prove detection, prove the clean tree reads 0."""
    with tempfile.TemporaryDirectory() as td:
        bad = Path(td) / "bad"
        (bad / "pkg").mkdir(parents=True)
        (bad / "pkg" / "reader.py").write_text(
            "def is_live(node_id):\n" + BAD_SITE, encoding="utf-8"
        )
        hits = scan(bad)
        if not any("reader.py" in rel for rel, _n, _s in hits):
            print("self-test FAILED: the planted site went undetected", file=sys.stderr)
            return False
        clean = Path(td) / "clean"
        (clean / "pkg").mkdir(parents=True)
        (clean / "pkg" / "empty.py").write_text("x = 1\n", encoding="utf-8")
        if scan(clean):
            print("self-test FAILED: the clean tree read as planted", file=sys.stderr)
            return False
    print("self-test OK: planted site detected, clean tree reads empty")
    return True


def _refuse_unreadable() -> bool:
    if not UNREADABLE:
        return False
    print(
        f"{len(UNREADABLE)} file(s) unreadable, excluded from the census; "
        "a file the census could not read is a hole in the guard:",
        file=sys.stderr,
    )
    for entry in UNREADABLE:
        print(f"  {entry}", file=sys.stderr)
    return True


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--self-test", action="store_true", help="prove the detector detects")
    args = ap.parse_args()
    if args.self_test:
        return 0 if self_test() else 1

    hits = scan(SRC)
    if _refuse_unreadable():
        return 1
    registered = _allowlisted()
    missing = [
        (rel, n, s)
        for rel, n, s in hits
        if f"{rel}:{s}" not in registered
    ]
    if missing:
        print(
            f"{len(missing)} unregistered node-claim occupancy read(s); "
            "register each in cli/tests/unit/occupancy_readers_allowlist.txt "
            "with the control that covers it (see the four-way control test):",
            file=sys.stderr,
        )
        for rel, n, s in sorted(missing):
            print(f"  {rel}:{n}: {s}", file=sys.stderr)
        return 1
    stale = registered - {f"{rel}:{s}" for rel, _n, s in hits}
    if stale:
        print(
            "stale allowlist entries name no live site; remove or update:",
            file=sys.stderr,
        )
        for entry in sorted(stale):
            print(f"  {entry}", file=sys.stderr)
        return 1
    print(f"occupancy readers: OK - {len(hits)} site(s), all registered, self-control green")
    return 0


if __name__ == "__main__":
    sys.exit(main())
