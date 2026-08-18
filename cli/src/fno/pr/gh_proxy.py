"""`gh` proxy policy for Footnote-launched processes."""
from __future__ import annotations

import os
import sys
from enum import Enum
from typing import Sequence

from fno.pr import _quota


class Action(str, Enum):
    DELEGATE = "delegate"
    BROKER = "broker"


def classify(args: Sequence[str]) -> Action:
    if len(args) >= 2 and args[:2] == ["pr", "checks"]:
        return Action.BROKER
    if len(args) >= 2 and args[:2] == ["pr", "view"] and "--json" in args:
        return Action.BROKER
    if len(args) >= 2 and args[:2] == ["api", "graphql"]:
        return Action.BROKER
    return Action.DELEGATE


def delegate(real: str, args: Sequence[str]) -> None:
    """Replace the proxy so untouched gh commands keep TTY and streaming semantics."""
    os.execv(real, [real, *args])


def main() -> None:
    args = sys.argv[1:]
    action = classify(args)
    real = _quota.resolve_real_gh()
    if not real:
        print("gh proxy: real gh executable not found", file=sys.stderr)
        raise SystemExit(127)
    if action is Action.BROKER:
        result = _quota.execute_graphql(
            "discretionary", args, real_gh=real
        )
    else:
        delegate(real, args)
        raise AssertionError("os.execv returned")
    if result.stdout:
        sys.stdout.write(result.stdout)
    if result.stderr:
        sys.stderr.write(result.stderr)
    raise SystemExit(result.returncode)
