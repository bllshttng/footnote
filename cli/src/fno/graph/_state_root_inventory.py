"""Diff a directory against docs/state-root-inventory.md.

The doc names every writer to the top level of the state root. This module is
the enforcement half (x-a469): it parses the doc's table rows and reports
top-level entries nothing in the doc covers, so the doc cannot drift from the
real root unnoticed.

CLI:
    python3 -m fno.graph._state_root_inventory --report ~/.fno

Exit 0 when fully documented, 1 when drift is found, 2 on an unreadable doc
or directory.
"""
from __future__ import annotations

import argparse
import fnmatch
import re
import sys
from pathlib import Path

DEFAULT_DOC = Path("docs/state-root-inventory.md")

_BACKTICK = re.compile(r"`([^`]+)`")
_PLACEHOLDER = re.compile(r"<[^>]*>")


def doc_patterns(doc_path: Path) -> list[str]:
    """Every backtick span in any Markdown table row, as a match pattern.

    A comma-separated cell such as `` `graph.json`, `.lock` `` contributes one
    pattern per name. A ``<ts>``-style placeholder translates to ``*`` so a
    written row keeps matching the file family it names.
    """
    patterns: list[str] = []
    for line in doc_path.read_text(encoding="utf-8").splitlines():
        if not line.lstrip().startswith("|"):
            continue
        for span in _BACKTICK.findall(line):
            pattern = _PLACEHOLDER.sub("*", span.strip())
            if pattern:
                patterns.append(pattern)
    return patterns


def _is_extension(pattern: str) -> bool:
    return pattern.startswith(".") and len(pattern) > 1 and "*" not in pattern


def top_level_patterns(doc_path: Path) -> list[str]:
    """Patterns that can name one top-level entry of the state root.

    Per table cell: a bare name matches itself; a subfolder row
    (``agents/reap-receipts/...``) documents its first segment, the directory
    the doc owns; an extension span (``.lock``) documents the suffix sidecar
    of a plain base in the same cell (``config.toml``, ``.lock`` covers
    ``config.toml.lock``). Extensions inside a subfolder cell (the mux row's
    ``.log``, ``.pid``) document that subfolder's sidecars, never the root. A
    pattern reduced to ``*`` is dropped: it would whitelist the entire root
    and blind the gate.
    """
    seen, out = set(), []

    def add(pattern: str) -> None:
        if pattern and pattern != "*" and pattern not in (".", "..") and pattern not in seen:
            seen.add(pattern)
            out.append(pattern)

    for line in doc_path.read_text(encoding="utf-8").splitlines():
        if not line.lstrip().startswith("|"):
            continue
        for cell in line.split("|"):
            spans = [_PLACEHOLDER.sub("*", s.strip()) for s in _BACKTICK.findall(cell)]
            spans = [s for s in spans if s]
            slashed = any("/" in s for s in spans)
            bases = [s for s in spans if "/" not in s and "*" not in s and not s.startswith(".")]
            for span in spans:
                if "/" in span:
                    add(span.split("/", 1)[0])
                elif not slashed:
                    add(span)
                    if _is_extension(span):
                        for base in bases:
                            add(f"*{span}")
    return out


def undocumented(dir_path: Path, doc_path: Path | None = None) -> list[str]:
    """Top-level entries of ``dir_path`` no doc pattern covers.

    Top level only, never recursive, matching the doc's own scope.
    """
    patterns = top_level_patterns(doc_path or DEFAULT_DOC)
    names = sorted(entry.name for entry in dir_path.iterdir())
    return [n for n in names if not any(fnmatch.fnmatchcase(n, p) for p in patterns)]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Report state-root entries the inventory doc does not name.")
    parser.add_argument("--report", required=True, type=Path, metavar="DIR", help="directory to check")
    parser.add_argument("--doc", type=Path, default=DEFAULT_DOC, help="inventory doc (default: docs/state-root-inventory.md)")
    args = parser.parse_args(argv)
    try:
        missing = undocumented(args.report, args.doc)
    except OSError as exc:
        print(f"state-root-inventory: cannot read {args.report} or {args.doc}: {exc}", file=sys.stderr)
        return 2
    if not missing:
        print(f"state-root-inventory: {args.report} fully documented by {args.doc}")
        return 0
    for name in missing:
        print(name)
    print(f"state-root-inventory: {len(missing)} undocumented top-level entr(y|ies) in {args.report}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
