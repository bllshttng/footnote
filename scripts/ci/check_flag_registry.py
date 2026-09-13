#!/usr/bin/env python3
"""Flag-surface ratchet: the inline ``typer.Option`` count in cli/src/fno
never grows.

Operator ruling 2026-09-12 (node x-72fc): all new code is Rust and the flag
registry is structural there (clap, one declaration per flag). A new Python
flag means a new verb, and a new verb belongs in crates. So the gate is not
"one declaration per name" but the stricter, simpler one: ANY new
typer.Option call in cli/src/fno fails, against a checked-in count in
scripts/ci/flag-baseline.txt. Shrink-only, both-directional like
check-file-budget.sh: a removal must lower the baseline in the same PR, so a
deletion can never bank credit for a later addition.

Lives in scripts/ci, not cli/src/fno: the Python tree is the compatibility
shell and is itself shrink-only (net +100), so the gate that enforces that
cannot be part of the tree it guards.

Usage:
  python3 scripts/ci/check_flag_registry.py            # check
  python3 scripts/ci/check_flag_registry.py --update   # rewrite the baseline
  python3 scripts/ci/check_flag_registry.py --selftest # fixture selftest

Exit: 0 pass, 1 refused grow or stale baseline, 2 selftest failure.
"""

from __future__ import annotations

import argparse
import ast
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
SCAN_REL = Path("cli/src/fno")
BASELINE_REL = Path("scripts/ci/flag-baseline.txt")


def count_options(root: Path) -> int:
    """Count typer.Option(...) call sites under cli/src/fno via AST."""
    total = 0
    for path in sorted((root / SCAN_REL).rglob("*.py")):
        tree = ast.parse(path.read_text(encoding="utf-8", errors="replace"))
        for node in ast.walk(tree):
            if (
                isinstance(node, ast.Call)
                and isinstance(node.func, ast.Attribute)
                and node.func.attr == "Option"
            ):
                total += 1
    return total


def read_baseline(path: Path) -> int:
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            return int(line.split("\t")[0].strip())
    raise ValueError(f"no data row in {path}")


def write_baseline(path: Path, count: int) -> None:
    path.write_text(
        "# Flag-surface ratchet (node x-72fc, operator ruling 2026-09-12): the\n"
        "# inline typer.Option call count under cli/src/fno never grows. New\n"
        "# flags are Rust work (clap in crates). --update rewrites this number;\n"
        "# a removal must lower it in the same PR.\n"
        f"{count}\n",
        encoding="utf-8",
    )


def run(root: Path, update: bool = False) -> int:
    baseline_path = root / BASELINE_REL
    live = count_options(root)
    if update:
        write_baseline(baseline_path, live)
        print(f"flag-registry: baseline set to {live}")
        return 0
    baseline = read_baseline(baseline_path)
    if live > baseline:
        print(
            f"flag-registry: FAIL\n"
            f"typer.Option count {live} > baseline {baseline} in {SCAN_REL}. "
            "A new flag is a new verb and a new verb belongs in crates "
            "(clap; operator ruling 2026-09-12, node x-72fc). Shrink the "
            "Python flag surface, never grow it. If this PR only removed "
            "options, lower scripts/ci/flag-baseline.txt to the live count.",
            file=sys.stderr,
        )
        return 1
    if live < baseline:
        print(
            f"flag-registry: FAIL\n"
            f"baseline {baseline} > live count {live}: the surface shrank. "
            f"Lower scripts/ci/flag-baseline.txt to {live} in this PR so a "
            "removal can never bank credit for a later addition.",
            file=sys.stderr,
        )
        return 1
    print(f"flag-registry: ok ({live} inline typer.Option calls, at baseline)")
    return 0


def selftest() -> int:
    """Fixture check: the gate fails a grow, a stale row, and passes a fresh
    baseline. Runs against a synthetic tree, never the real one."""
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / SCAN_REL).mkdir(parents=True)
        (root / BASELINE_REL).parent.mkdir(parents=True)
        src = root / SCAN_REL / "sample.py"
        src.write_text(
            "import typer\n"
            "def cmd(a: bool = typer.Option(False, '--json')):\n"
            "    pass\n",
            encoding="utf-8",
        )

        def baseline(n: int) -> None:
            write_baseline(root / BASELINE_REL, n)

        baseline(1)
        if run(root) != 0:
            print("selftest: at-baseline case failed", file=sys.stderr)
            return 2

        src.write_text(
            src.read_text(encoding="utf-8")
            + "def cmd2(b: bool = typer.Option(False, '--force')):\n    pass\n",
            encoding="utf-8",
        )
        if run(root) != 1:
            print("selftest: grow case did not fail", file=sys.stderr)
            return 2

        baseline(3)
        if run(root) != 1:
            print("selftest: stale-baseline case did not fail", file=sys.stderr)
            return 2

        if run(root, update=True) != 0 or read_baseline(root / BASELINE_REL) != 2:
            print("selftest: --update case failed", file=sys.stderr)
            return 2
    print("selftest: ok")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--update", action="store_true", help="rewrite the baseline")
    parser.add_argument("--selftest", action="store_true", help="fixture selftest")
    args = parser.parse_args()
    if args.selftest:
        return selftest()
    return run(REPO_ROOT, update=args.update)


if __name__ == "__main__":
    sys.exit(main())
