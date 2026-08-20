#!/usr/bin/env python3
"""Enumerate shell-shaped and argv-shaped GitHub GraphQL callers."""
from __future__ import annotations

import ast
import re
import subprocess
from pathlib import Path

# The whole crates tree, not just fno-agents: a raw poller written in the mux
# crate was invisible to this instrument, and a guard that covers some of the
# reachable paths reads as protection while the rest stay open. Files outside
# the two named adapters fall through _disposition to "unclassified", which is
# the fail-closed answer.
ROOTS = ("cli/src", "crates", "hooks", "skills", "scripts")
SKIP_PARTS = {"tests", "target", ".git"}
SKIP_NAMES = {
    "check-no-direct-graphql-pr-read.sh",
    "graphql-caller-inventory.py",
}
TEXT_SUFFIXES = {".md", ".py", ".rs", ".sh", ".toml", ".yaml", ".yml"}
PATTERNS = {
    "argv-pr": re.compile(
        r'(?:["\']gh["\']\s*,\s*)?["\']pr["\']\s*,\s*["\'](?:view|checks|list|status)["\']'
    ),
    "argv-api": re.compile(
        r'(?:["\']gh["\']\s*,\s*)?["\']api["\']\s*,\s*["\']graphql["\']'
    ),
    "shell-pr": re.compile(
        r"\bgh\s+(?:(?:-R|--repo|--hostname)(?:=|\s+)?\S+\s+)*"
        r"pr\s+(?:view|checks|list|status)\b"
    ),
    "shell-api": re.compile(r"\bgh\s+api\b[^\n]*\bgraphql\b"),
}


_DOCSTRING_SPANS: dict[str, list[tuple[int, int]]] = {}


def _docstring_spans(relative: str, source: str) -> list[tuple[int, int]]:
    """Byte-offset spans of every DOCSTRING in a Python source file.

    Deliberately docstrings only, NOT every string literal. A real caller in
    Python is a string handed to a subprocess (`run("gh pr view ...",
    shell=True)`), so excluding all literals would hide exactly the callers this
    inventory exists to find. A docstring is a bare statement expression that
    nothing can execute, which is the same argument the `.md` exclusion makes.
    An unparseable file yields no spans, so it stays fully counted.
    """
    cached = _DOCSTRING_SPANS.get(relative)
    if cached is not None:
        return cached
    spans: list[tuple[int, int]] = []
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError):
        _DOCSTRING_SPANS[relative] = spans
        return spans
    starts = [0]
    for line in source.splitlines(keepends=True):
        starts.append(starts[-1] + len(line))

    def _offset(lineno: int, col: int) -> int:
        return starts[lineno - 1] + col if 0 < lineno <= len(starts) else 0

    for node in ast.walk(tree):
        body = getattr(node, "body", None)
        if not isinstance(body, list) or not body:
            continue
        if not isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        first = body[0]
        if (
            isinstance(first, ast.Expr)
            and isinstance(first.value, ast.Constant)
            and isinstance(first.value.value, str)
            and first.end_lineno is not None
            and first.end_col_offset is not None
        ):
            spans.append(
                (
                    _offset(first.lineno, first.col_offset),
                    _offset(first.end_lineno, first.end_col_offset),
                )
            )
    _DOCSTRING_SPANS[relative] = spans
    return spans


def _disposition(relative: str, source: str, offset: int) -> str:
    line = source[source.rfind("\n", 0, offset) + 1 : source.find("\n", offset)]
    stripped = line.lstrip()
    suffix = Path(relative).suffix
    # A markdown file cannot execute, so it can never be a caller. Saying so
    # here rather than only at the count ceiling matters: everything under
    # crates/ that is not one of the two named adapters falls through to
    # "unclassified", which is a hard failure, so widening ROOTS to the whole
    # crates tree turned every README naming `gh pr view` into a red gate.
    if suffix == ".md":
        return "documentation"
    if (suffix in {".py", ".sh", ".toml", ".yaml", ".yml"} and stripped.startswith("#")) or (
        suffix == ".rs" and stripped.startswith("//")
    ):
        return "documentation"
    # Prose in a Python docstring is not a caller either. Without this, writing
    # `gh pr view` into any docstring under cli/src/ raised that bucket's count
    # and reded a per-disposition ceiling pinned at its exact current value -
    # the same prose-edit false red the sha256 pin was retired for.
    if suffix == ".py" and any(
        start <= offset < end for start, end in _docstring_spans(relative, source)
    ):
        return "documentation"
    if relative.startswith("cli/src/"):
        return "fno-process-proxy"
    if relative in {
        "crates/fno-agents/src/loopcheck.rs",
        "crates/fno-agents/src/finalize.rs",
    }:
        return "fixed-purpose-adapter"
    if relative == "hooks/git-protection.py":
        return "operator-guard-definition"
    if relative == "scripts/diagnostics/graphql-quota-soak.py":
        return "quota-soak-broker"
    if relative.startswith("skills/"):
        return "worker-proxy-or-hook"
    return "unclassified"


def main() -> int:
    root = Path(__file__).resolve().parents[2]
    rows: list[str] = []
    tracked = subprocess.run(
        ["git", "ls-files", "-co", "--exclude-standard", "--", *ROOTS],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    for relative in tracked:
        path = root / relative
        if (
            path.name in SKIP_NAMES
            or path.suffix not in TEXT_SUFFIXES
            or any(part in SKIP_PARTS for part in path.parts)
        ):
            continue
        try:
            source = path.read_text()
        except (OSError, UnicodeDecodeError):
            continue
        for kind, pattern in PATTERNS.items():
            for match in pattern.finditer(source):
                normalized = " ".join(match.group(0).split())
                relative_path = str(path.relative_to(root))
                disposition = _disposition(relative_path, source, match.start())
                rows.append(f"{disposition}|{kind}|{relative_path}|{normalized}")
    print("\n".join(sorted(rows)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
