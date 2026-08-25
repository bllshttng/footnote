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
#
# The marker carries our PID, and that is what makes it safe. A bare flag rides
# the environment into the real gh's whole process tree - git, git hooks,
# $GH_EDITOR, extensions, credential helpers - so any descendant that reaches a
# proxy again refuses its own FIRST entry. Stripping the flag back out at each
# such boundary is a guard on one of N paths, and the Nth path is the one that
# breaks. execve PRESERVES the pid and a child never shares it, so comparing the
# marker to our own pid separates "I am my own successor" from "an unrelated
# descendant inherited a stale value" without trusting any caller to scrub it.
_REENTRY_ENV = _quota.PROXY_DEPTH_ENV

# Exit 2 for both self-protection refusals, matching `ProxyIdentityError`
# below. Exit 1 is indistinguishable from the real gh's own ordinary failure,
# so a caller cannot tell "gh said no" from "the proxy refused to run gh".
_REFUSE_EXIT = 2


class Action(str, Enum):
    DELEGATE = "delegate"
    BROKER = "broker"


# Shared with the broker's argv guard so both judge the same command word.
command_args = _quota.command_args


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
    env[_REENTRY_ENV] = str(os.getpid())
    os.execve(real, [real, *args], env)


def main() -> None:
    if os.environ.get(_REENTRY_ENV) == str(os.getpid()):
        print(
            "gh proxy: re-entered itself. resolve_real_gh() gave back the proxy "
            "shim instead of a real gh binary. Refusing to loop.",
            file=sys.stderr,
        )
        raise SystemExit(_REFUSE_EXIT)
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
        raise SystemExit(_REFUSE_EXIT) from exc
    if result.stdout:
        sys.stdout.write(result.stdout)
    if result.stderr:
        sys.stderr.write(result.stderr)
    raise SystemExit(result.returncode)
