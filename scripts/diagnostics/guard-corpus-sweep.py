#!/usr/bin/env python3
"""Run every real Bash command in the local transcripts through the bg guard.

This is the check that found what eight rounds of hand-written cases could not.
The first run over the corpus found 3 false refusals in 23 denials, and both
later regressions in `hooks/bg-process-guard.py` were introduced by the round
that fixed the previous ones. So run it after ANY change to that parser.

Two modes:

    guard-corpus-sweep.py
        Print every DENIAL for reading. Read them, never just the count: a
        count cannot tell a genuine refusal from a false one.

    guard-corpus-sweep.py --against <ref>
        Verdict differential against the guard at a git ref. Prints every
        command whose verdict FLIPPED, both directions. A count staying at 20
        says nothing about whether the same 20 commands are in it.

Exit status is 0 whenever the sweep ran. This reports; it does not judge.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile

REPO = pathlib.Path(__file__).resolve().parents[2]
GUARD = REPO / "hooks" / "bg-process-guard.py"


def _load(path: pathlib.Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec and spec.loader
    spec.loader.exec_module(module)
    return module


def _commands(root: pathlib.Path) -> set[str]:
    """Every unique Bash `command` in the transcripts under `root`.

    Walks the whole JSON rather than matching a fixed shape, because the tool
    block sits at different depths across harness versions and a fixed path
    silently returned zero.
    """
    found: set[str] = set()
    for path in root.rglob("*.jsonl"):
        try:
            with path.open(encoding="utf-8", errors="replace") as handle:
                for line in handle:
                    if '"Bash"' not in line:
                        continue
                    try:
                        record = json.loads(line)
                    except ValueError:
                        continue
                    stack = [record]
                    while stack:
                        node = stack.pop()
                        if isinstance(node, dict):
                            if node.get("name") == "Bash":
                                cmd = (node.get("input") or {}).get("command")
                                if isinstance(cmd, str) and cmd.strip():
                                    found.add(cmd)
                            stack.extend(node.values())
                        elif isinstance(node, list):
                            stack.extend(node)
        except OSError:
            continue
    return found


def _denies(guard, command: str):
    """True, False, or None when the guard itself raised.

    A crash is its own finding: the hook fails open, so an exception here is a
    silent bypass rather than a refusal.
    """
    try:
        return guard.decide(command) is not None
    except Exception:  # noqa: BLE001 -- a crash is a result, not an error
        return None


def _guard_at(ref: str, tmp: pathlib.Path):
    blob = subprocess.run(
        ["git", "-C", str(REPO), "show", f"{ref}:hooks/bg-process-guard.py"],
        capture_output=True, text=True, check=True,
    ).stdout
    path = tmp / "guard_old.py"
    path.write_text(blob, encoding="utf-8")
    return _load(path, "guard_old")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--against", metavar="REF",
        help="git ref to diff verdicts against (e.g. origin/main)",
    )
    parser.add_argument(
        "--transcripts", type=pathlib.Path,
        default=pathlib.Path.home() / ".claude" / "projects",
        help="transcript root (default ~/.claude/projects)",
    )
    args = parser.parse_args()

    if not args.transcripts.is_dir():
        print(f"no transcripts at {args.transcripts}", file=sys.stderr)
        return 1

    guard = _load(GUARD, "guard_new")
    commands = _commands(args.transcripts)
    print(f"unique commands: {len(commands)}")

    if not args.against:
        denied = [c for c in commands if _denies(guard, c)]
        crashed = [c for c in commands if _denies(guard, c) is None]
        print(f"denials: {len(denied)}")
        print(f"crashes: {len(crashed)}")
        print("-" * 60)
        for cmd in sorted(denied):
            print(repr(cmd[:220]))
        return 0

    with tempfile.TemporaryDirectory() as td:
        old = _guard_at(args.against, pathlib.Path(td))
        newly_denied, newly_allowed, crashed = [], [], []
        old_n = new_n = 0
        for cmd in commands:
            before, after = _denies(old, cmd), _denies(guard, cmd)
            old_n += bool(before)
            new_n += bool(after)
            if after is None and before is not None:
                crashed.append(cmd)
            elif before is False and after is True:
                newly_denied.append(cmd)
            elif before is True and after is False:
                newly_allowed.append(cmd)

    print(f"denials {args.against}={old_n} working-tree={new_n}")
    print(f"NEWLY DENIED (false-positive risk): {len(newly_denied)}")
    for cmd in sorted(newly_denied):
        print("  +", repr(cmd[:200]))
    print(f"NEWLY ALLOWED (bypass risk): {len(newly_allowed)}")
    for cmd in sorted(newly_allowed):
        print("  -", repr(cmd[:200]))
    print(f"NEW CRASHES: {len(crashed)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
