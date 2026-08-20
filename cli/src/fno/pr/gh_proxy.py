"""`gh` proxy policy for Footnote-launched processes."""
from __future__ import annotations

import os
import sys
from enum import Enum
from typing import Sequence

from fno.pr import _quota

# `os.execve` replaces the process image, so a delegate leaves no child and the
# pid never moves. Nothing outside this program can see the repeat. A marker in
# the environment we hand to execve is the only way the successor can know it is
# us again.
_REENTRY_ENV = "FNO_GH_PROXY_DEPTH"


class Action(str, Enum):
    DELEGATE = "delegate"
    BROKER = "broker"


def command_args(args: Sequence[str]) -> list[str]:
    """Drop gh-wide options so equivalent command spellings share one policy."""
    def strip(tokens: list[str]) -> list[str]:
        while tokens:
            token = tokens[0]
            if token in {"-R", "--repo", "--hostname"}:
                if len(tokens) < 2:
                    return []
                tokens = tokens[2:]
            elif (
                (token.startswith("-R") and token != "-R")
                or token.startswith("--repo=")
                or token.startswith("--hostname=")
            ):
                tokens = tokens[1:]
            else:
                break
        return tokens

    normalized = strip(list(args))
    if normalized[:1] == ["pr"]:
        normalized = ["pr", *strip(normalized[1:])]
    return normalized


def classify(args: Sequence[str]) -> Action:
    command = command_args(args)
    if len(command) >= 2 and command[:2] in (
        ["pr", "checks"],
        ["pr", "list"],
        ["pr", "status"],
        ["pr", "view"],
    ):
        return Action.BROKER
    if command[:1] == ["api"] and "graphql" in command[1:]:
        return Action.BROKER
    return Action.DELEGATE


def delegate(real: str, args: Sequence[str]) -> None:
    """Replace the proxy so untouched gh commands keep TTY and streaming semantics."""
    env = _quota.delegate_environment()
    env[_REENTRY_ENV] = "1"
    os.execve(real, [real, *args], env)


def main() -> None:
    if os.environ.get(_REENTRY_ENV):
        print(
            "gh proxy: re-entered itself. resolve_real_gh() gave back the proxy "
            "shim instead of a real gh binary. Refusing to loop.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    args = sys.argv[1:]
    action = classify(args)
    try:
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
    except _quota.ProxyIdentityError as exc:
        print(_quota.proxy_identity_refusal(exc), file=sys.stderr)
        raise SystemExit(2) from exc
    if result.stdout:
        sys.stdout.write(result.stdout)
    if result.stderr:
        sys.stderr.write(result.stderr)
    raise SystemExit(result.returncode)
