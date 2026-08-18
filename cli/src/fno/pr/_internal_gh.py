"""Fixed-purpose ``gh`` adapter for the Rust PR readers.

Metadata and CI are translated from the REST helpers. The remaining GraphQL
shapes run through the serialized quota broker with a purpose fixed by the
executable name, so an environment variable cannot promote a discretionary
read into the coverage reserve.
"""
from __future__ import annotations

import json
import sys
from typing import Callable, Sequence

from fno.pr import _quota, _rest, _status
from fno.pr._proc import Result, run

_METADATA_FIELDS = {
    "state",
    "number",
    "headRefName",
    "headRefOid",
    "mergeable",
    "baseRefName",
}


def _option(args: Sequence[str], name: str) -> str | None:
    try:
        return args[args.index(name) + 1]
    except (ValueError, IndexError):
        return None


def _pr_number(args: Sequence[str], *, cwd: str | None, runner: Callable) -> tuple[int | None, str]:
    for arg in args[2:]:
        if arg.isdigit():
            return int(arg), ""
    return _rest.resolve_current_pr_number_rest(
        cwd=cwd, runner=runner, repo=_option(args, "--repo")
    )


def _rest_runner(real_gh: str, runner: Callable) -> Callable:
    def invoke(cmd, **kwargs):
        argv = [real_gh, *cmd[1:]] if cmd and cmd[0] == "gh" else list(cmd)
        return runner(argv, **kwargs)

    return invoke


def _metadata(
    args: Sequence[str], *, cwd: str | None, real_gh: str, runner: Callable
) -> Result:
    rest_runner = _rest_runner(real_gh, runner)
    number, reason = _pr_number(args, cwd=cwd, runner=rest_runner)
    if number is None:
        return Result(1, "", reason)
    info, reason = _rest.fetch_pr_info_rest(
        str(number), cwd=cwd, runner=rest_runner, repo=_option(args, "--repo")
    )
    if info is None:
        return Result(1, "", reason)
    payload = {
        "state": info["state"],
        "number": info["pr"],
        "headRefName": info["head_ref"],
        "headRefOid": info["head_sha"],
        "mergeable": info["mergeable"],
        "baseRefName": info["base_ref"],
    }
    return Result(0, json.dumps(payload) + "\n", "")


def _bucket(check: dict) -> str:
    raw = str(check.get("conclusion") or check.get("state") or "").upper()
    if raw == "CANCELLED":
        return "cancel"
    if raw in {"SKIPPED", "NEUTRAL"}:
        return "skipping"
    return _status._classify(check)


def _checks(
    args: Sequence[str], *, cwd: str | None, real_gh: str, runner: Callable
) -> Result:
    rest_runner = _rest_runner(real_gh, runner)
    number, reason = _pr_number(args, cwd=cwd, runner=rest_runner)
    if number is None:
        return Result(1, "", reason)
    payload, reason = _rest.fetch_pr_rest(str(number), cwd=cwd, runner=rest_runner)
    if payload is None:
        return Result(1, "", reason)
    rows = []
    for check in payload.get("statusCheckRollup") or []:
        rows.append(
            {
                "name": check.get("name") or check.get("context") or "",
                "state": check.get("conclusion") or check.get("state") or "",
                "bucket": _bucket(check),
                "startedAt": check.get("startedAt") or check.get("createdAt") or "",
                "workflow": "",
            }
        )
    return Result(0, json.dumps(rows) + "\n", "")


def execute(
    purpose: str,
    args: Sequence[str],
    *,
    cwd: str | None = None,
    runner: Callable = run,
    real_gh: str | None = None,
) -> Result:
    gh = real_gh or _quota.resolve_real_gh()
    if not gh:
        return Result(127, "", "gh not found on PATH")
    if len(args) >= 2 and args[:2] == ["pr", "checks"]:
        return _checks(args, cwd=cwd, real_gh=gh, runner=runner)
    if len(args) >= 2 and args[:2] == ["pr", "view"]:
        fields = set((_option(args, "--json") or "").split(","))
        if fields and fields <= _METADATA_FIELDS:
            return _metadata(args, cwd=cwd, real_gh=gh, runner=runner)
        return _quota.execute_graphql(
            purpose, args, runner=runner, real_gh=gh
        )
    if len(args) >= 2 and args[:2] == ["api", "graphql"]:
        return _quota.execute_graphql(
            purpose, args, runner=runner, real_gh=gh
        )
    return runner([gh, *args], cwd=cwd)


def _main(purpose: str) -> None:
    result = execute(purpose, sys.argv[1:])
    if result.stdout:
        sys.stdout.write(result.stdout)
    if result.stderr:
        sys.stderr.write(result.stderr + ("" if result.stderr.endswith("\n") else "\n"))
    raise SystemExit(result.returncode)


def discretionary_main() -> None:
    _main("discretionary")


def coverage_main() -> None:
    _main("coverage")
