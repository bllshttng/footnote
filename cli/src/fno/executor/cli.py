"""Internal Python plan-executor resolver.

The former top-level ``fno executor`` command remains tombstoned. This module
is an internal parity surface for the flat-plan Bash resolver and can be run as
``python -m fno.executor.cli resolve``.
"""

from __future__ import annotations

import re
import subprocess
import sys
from argparse import ArgumentParser
from pathlib import Path
from typing import List

_EXECUTOR_RE = re.compile(r"^[ \t]*executor:[ \t]*(.*)$")
_FILES_RE = re.compile(r"^[ \t]*\*{0,2}files?:?\*{0,2}[ \t]+(.*)$", re.IGNORECASE)
_RANGE_RE = re.compile(r"[ \t]*\([^)]*\)[ \t]*$")


def _normalize(value: str) -> str:
    value = value.strip().replace('"', "").replace("'", "").replace(" ", "")
    return "tdd" if value == "do" else value


def _read_declared(plan_path: Path) -> str:
    for line in plan_path.read_text(encoding="utf-8").splitlines():
        match = _EXECUTOR_RE.match(line)
        if match:
            return _normalize(match.group(1))
    return ""


def _read_files(plan_path: Path) -> List[str]:
    files: List[str] = []
    for line in plan_path.read_text(encoding="utf-8").splitlines():
        match = _FILES_RE.match(line)
        if not match:
            continue
        for value in match.group(1).split(","):
            value = value.translate(str.maketrans("", "", '`"[]')).strip()
            value = _RANGE_RE.sub("", value).strip()
            if value:
                files.append(value)
    return files


def _read_inferred(task_files: List[str]) -> str:
    result = subprocess.run(
        [sys.executable, "-m", "fno.executor._surface"],
        input="\n".join(task_files) + "\n",
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        print(
            f"fno executor: fno.executor._surface exited "
            f"rc={result.returncode}: {result.stderr.strip()}",
            file=sys.stderr,
        )
        raise SystemExit(2)
    return _normalize(result.stdout.strip())


def resolve(
    plan_path: Path | None = None,
    task_files: str | None = None,
    explain: bool = False,
) -> str:
    if plan_path is not None and not plan_path.is_file():
        print(f"fno executor: plan file not found: {plan_path}", file=sys.stderr)
        raise SystemExit(2)

    if plan_path is not None:
        declared = _read_declared(plan_path)
        if declared in ("tdd", "impeccable"):
            return f"tier: plan-frontmatter\nvalue: {declared}" if explain else declared
        if declared:
            return "tier: plan-frontmatter\nvalue: tdd" if explain else "tdd"

    files = [part.strip() for part in (task_files or "").split(",") if part.strip()]
    if not files and plan_path is not None:
        files = _read_files(plan_path)
    if files:
        inferred = _read_inferred(files)
        if inferred in ("tdd", "impeccable"):
            return f"tier: inference\nvalue: {inferred}" if explain else inferred

    return "tier: default\nvalue: tdd" if explain else "tdd"


def _main(argv: list[str]) -> int:
    parser = ArgumentParser(prog="python -m fno.executor.cli")
    subcommands = parser.add_subparsers(dest="command", required=True)
    resolve_parser = subcommands.add_parser("resolve")
    resolve_parser.add_argument("--plan-path", type=Path)
    resolve_parser.add_argument("--task-files")
    resolve_parser.add_argument("--explain", action="store_true")
    args = parser.parse_args(argv)
    print(resolve(args.plan_path, args.task_files, args.explain))
    return 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
