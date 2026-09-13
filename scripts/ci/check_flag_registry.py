#!/usr/bin/env python3
"""Flag-surface ratchet: a change may not add ``typer.Option`` calls to
cli/src/fno.

Operator ruling 2026-09-12 (node x-72fc): all new code is Rust and the flag
registry is structural there (clap, one declaration per flag). A new Python
flag means a new verb, and a new verb belongs in crates. The gate measures
the change against its own base - the merge base of PR_BASE_REF on a PR, the
previous tip (FLAG_BASE_SHA = github.event.before) on a push - over only the
files the change touched, and refuses any growth. There is no stored count:
a checked-in total made every count-changing PR edit one shared
line, and on 2026-09-13 two pairs of PRs merged green on stale bases and
left main red, once growing and once shrinking (node x-2986). Removals bank
no credit either: the base is always the live tree on main, so an earlier
removal never leaves spare count for a later PR.

Lives in scripts/ci, not cli/src/fno: the Python tree is the compatibility
shell and is itself shrink-only (net +100), so the gate that enforces that
cannot be part of the tree it guards.

Usage:
  python3 scripts/ci/check_flag_registry.py            # check
  python3 scripts/ci/check_flag_registry.py --selftest # git-repo selftest

Env: FLAG_BASE_SHA (pin the base; never falls back to the merge base),
PR_BASE_REF (default main), PR_REMOTE (default origin).

Exit: 0 pass, 1 refused grow, 2 selftest failure or unresolvable base.
"""

from __future__ import annotations

import argparse
import ast
import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
SCAN_REL = Path("cli/src/fno")


def count_source(text: str) -> int:
    """Count typer.Option(...) call sites in one Python source text via AST."""
    tree = ast.parse(text)
    return sum(
        1
        for node in ast.walk(tree)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Attribute)
        and node.func.attr == "Option"
    )


def git(root: Path, *args: str) -> bytes | None:
    """Run git in root; stdout on success, None on any failure."""
    proc = subprocess.run(["git", "-C", str(root), *args], capture_output=True)
    return proc.stdout if proc.returncode == 0 else None


def resolve_base(
    root: Path, explicit_sha: str, base_ref: str, remote: str
) -> str | None:
    """The one base to measure against, or None (caller refuses, exit 2).

    An explicit sha never falls back to the merge base: on main the merge
    base IS HEAD, and that silent empty diff would read as a pass.
    """
    if explicit_sha and explicit_sha != "0" * 40:
        base = git(root, "rev-parse", "--verify", "--quiet", explicit_sha + "^{commit}")
        if base is None:
            print(
                f"flag-registry: FLAG_BASE_SHA {explicit_sha} does not resolve; "
                "refusing rather than passing on an empty diff (the checkout "
                "must hold that commit)",
                file=sys.stderr,
            )
            return None
        return base.decode().strip()
    if git(
        root,
        "fetch",
        "--quiet",
        remote,
        f"+refs/heads/{base_ref}:refs/remotes/{remote}/{base_ref}",
    ) is None:
        print(
            f"flag-registry: cannot fetch {remote}/{base_ref} - unable to "
            "establish the merge base (set PR_BASE_REF/PR_REMOTE)",
            file=sys.stderr,
        )
        return None
    tip = git(root, "rev-parse", "--verify", "--quiet", f"{remote}/{base_ref}")
    base = git(root, "merge-base", tip.decode().strip(), "HEAD") if tip else None
    if base is None:
        print(
            f"flag-registry: cannot establish a merge base between "
            f"{remote}/{base_ref} and HEAD - refusing (shallow checkout? "
            "fetch full history)",
            file=sys.stderr,
        )
        return None
    return base.decode().strip()


def changed_files(root: Path, base: str) -> list[str]:
    out = git(
        root,
        "-c",
        "core.quotepath=off",
        "diff",
        "--name-only",
        "-z",
        "--no-renames",
        base,
        "HEAD",
        "--",
        "cli/src/fno/*.py",
    )
    if out is None:
        return []
    return [p for p in out.decode("utf-8", errors="replace").split("\0") if p]


def count_at(root: Path, rev: str, path: str) -> int:
    """Option count of one file at a rev; a missing blob (added/deleted) is 0."""
    out = git(root, "show", f"{rev}:{path}")
    if out is None:
        return 0
    return count_source(out.decode("utf-8", errors="replace"))


def run(
    root: Path, base_sha: str = "", base_ref: str = "main", remote: str = "origin"
) -> int:
    base = resolve_base(root, base_sha, base_ref, remote)
    if base is None:
        return 2
    deltas: dict[str, int] = {}
    paths = changed_files(root, base)
    for path in paths:
        delta = count_at(root, "HEAD", path) - count_at(root, base, path)
        if delta:
            deltas[path] = delta
    total = sum(deltas.values())
    if total > 0:
        print("flag-registry: FAIL", file=sys.stderr)
        print(
            f"typer.Option count grew by +{total} against base {base[:7]} "
            f"in {SCAN_REL}:",
            file=sys.stderr,
        )
        for path in sorted(deltas):
            if deltas[path] > 0:
                print(f"  {path} +{deltas[path]}", file=sys.stderr)
        print(
            "A new flag is a new verb and a new verb belongs in crates "
            "(clap; operator ruling 2026-09-12, node x-72fc). Shrink the "
            "Python flag surface, never grow it.",
            file=sys.stderr,
        )
        return 1
    print(
        f"flag-registry: ok (base {base[:7]}, {total:+d} typer.Option calls "
        f"across {len(paths)} changed files)"
    )
    return 0


def selftest() -> int:
    """Fixture check in throwaway git repos: grow refused, the two-PR race
    composes, a merged removal banks no credit, an unresolvable base refuses.
    Always passes run() an explicit base sha, never env."""
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)

        def sh(*args: str) -> str:
            out = git(root, *args)
            assert out is not None, args
            return out.decode().strip()

        def commit(msg: str) -> str:
            sh("add", "-A")
            sh(
                "-c",
                "user.name=selftest",
                "-c",
                "user.email=selftest@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                msg,
            )
            return sh("rev-parse", "HEAD")

        def write_opts(name: str, count: int) -> None:
            path = root / SCAN_REL / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(
                "import typer\n"
                + "".join(
                    f"X{i} = typer.Option(False, '--f{i}')\n" for i in range(count)
                ),
                encoding="utf-8",
            )

        sh("init", "-q", "-b", "main")

        # Base B: one Option in a.py, one in b.py.
        write_opts("a.py", 1)
        write_opts("b.py", 1)
        base_b = commit("base")

        # Grow: a branch adds one in a new file - refused against B.
        sh("checkout", "-q", "-b", "grow")
        write_opts("c.py", 1)
        commit("grow")
        if run(root, base_b) != 1:
            print("selftest: grow case did not fail", file=sys.stderr)
            return 2
        sh("checkout", "-q", "main")

        # The race (the 2026-09-13 specimen): x and y each remove a different
        # Option from B. Each passes against B on its own branch.
        sh("checkout", "-q", "-b", "x")
        (root / SCAN_REL / "a.py").unlink()
        commit("x removes a.py")
        tip_x = sh("rev-parse", "HEAD")
        if run(root, base_b) != 0:
            print("selftest: shrink case did not pass", file=sys.stderr)
            return 2
        sh("checkout", "-q", "main")
        sh("checkout", "-q", "-b", "y")
        (root / SCAN_REL / "b.py").unlink()
        commit("y removes b.py")
        if run(root, base_b) != 0:
            print("selftest: shrink case did not pass", file=sys.stderr)
            return 2
        sh("checkout", "-q", "main")
        sh("-c", "user.name=selftest", "-c", "user.email=selftest@example.invalid",
           "-c", "commit.gpgsign=false", "merge", "-q", "--no-edit", "x")
        sh("-c", "user.name=selftest", "-c", "user.email=selftest@example.invalid",
           "-c", "commit.gpgsign=false", "merge", "-q", "--no-edit", "y")
        # The merged tip against x's tip (the push-alarm base) is clean: the
        # two removals composed, and no stored count existed to race on.
        if run(root, tip_x) != 0:
            print("selftest: race case did not compose clean", file=sys.stderr)
            return 2

        # No banking: from the merged tip, one added Option is still refused.
        merged_tip = sh("rev-parse", "main")
        sh("checkout", "-q", "-b", "z")
        write_opts("c.py", 1)
        commit("z grows again")
        if run(root, merged_tip) != 1:
            print("selftest: no-banking case did not fail", file=sys.stderr)
            return 2

        # An unresolvable base refuses instead of passing on an empty diff.
        if run(root, "d" * 40) != 2:
            print("selftest: unresolvable-base case did not refuse", file=sys.stderr)
            return 2
    print("selftest: ok")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--selftest", action="store_true", help="git-repo selftest")
    args = parser.parse_args()
    if args.selftest:
        return selftest()
    return run(
        REPO_ROOT,
        base_sha=os.environ.get("FLAG_BASE_SHA", ""),
        base_ref=os.environ.get("PR_BASE_REF", "main"),
        remote=os.environ.get("PR_REMOTE", "origin"),
    )


if __name__ == "__main__":
    sys.exit(main())
