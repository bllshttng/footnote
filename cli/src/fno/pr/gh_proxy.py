"""`gh` proxy policy for Footnote-launched processes."""
from __future__ import annotations

import subprocess
import sys
from enum import Enum
from typing import Sequence

from fno.pr import _quota


class Action(str, Enum):
    DELEGATE = "delegate"
    REFUSE_INFO = "refuse-info"
    REFUSE_STATUS = "refuse-status"
    BROKER = "broker"


def classify(args: Sequence[str]) -> Action:
    if len(args) >= 2 and args[:2] == ["pr", "checks"]:
        return Action.REFUSE_STATUS
    if len(args) >= 2 and args[:2] == ["pr", "view"] and "--json" in args:
        return Action.REFUSE_INFO
    if len(args) >= 2 and args[:2] == ["api", "graphql"]:
        return Action.BROKER
    return Action.DELEGATE


def _pr(args: Sequence[str]) -> str:
    for value in args[2:]:
        if value.isdigit():
            return value
    return "<n>"


def main() -> None:
    args = sys.argv[1:]
    action = classify(args)
    real = _quota.resolve_real_gh()
    if not real:
        print("gh proxy: real gh executable not found", file=sys.stderr)
        raise SystemExit(127)
    if action is Action.REFUSE_INFO:
        pr = _pr(args)
        print(
            f"GraphQL PR metadata read refused. Use `fno pr info {pr}` for "
            "state/head/mergeability; stop retrying `gh pr view --json` this quota window.",
            file=sys.stderr,
        )
        raise SystemExit(_quota.REFUSED)
    if action is Action.REFUSE_STATUS:
        pr = _pr(args)
        print(
            f"GraphQL CI read refused. Use `fno pr status {pr}`; its CI read is REST, "
            "while optional review-thread and coverage reads inside it remain GraphQL.",
            file=sys.stderr,
        )
        raise SystemExit(_quota.REFUSED)
    if action is Action.BROKER:
        result = _quota.execute_graphql(
            "discretionary", args, real_gh=real
        )
    else:
        result = subprocess.run([real, *args], text=True, capture_output=True, check=False)
    if result.stdout:
        sys.stdout.write(result.stdout)
    if result.stderr:
        sys.stderr.write(result.stderr)
    raise SystemExit(result.returncode)
