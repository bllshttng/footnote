"""Diff a directory against docs/state-root-inventory.md, the drift gate.

CLI: python3 -m fno.graph._state_root_inventory --report ~/.fno; exit 0 clean, 1 drift, 2 unreadable.
"""
from __future__ import annotations

import argparse
import fnmatch
import re
import sys
from pathlib import Path

# The caller's repo owns the doc: nearest docs/ upward from the cwd.
_doc_name = "docs/state-root-inventory.md"
DEFAULT_DOC = next((p / _doc_name for p in [Path.cwd(), *Path.cwd().parents] if (p / _doc_name).is_file()), Path(_doc_name))

_BACKTICK = re.compile(r"`([^`]+)`")
_PLACEHOLDER = re.compile(r"<[^>]*>")


def top_level_patterns(doc_path: Path) -> list[str]:
    """Patterns that can name one top-level entry, from each row's Entry cell.

    Writer and lifetime columns never whitelist: a ``doctor.py`` in a Writer
    cell must not admit a root file of that name. Within the Entry cell, a
    bare name matches itself; a subfolder row (``agents/reap-receipts/...``)
    documents its first segment, the directory the doc owns; an extension span
    (``.lock``) covers only the named bases it sits beside (``graph.json``,
    ``.lock`` means ``graph.json.lock``, never every lock file). A ``<ts>``
    placeholder becomes ``*``. A pattern reduced to ``*`` is dropped: it would
    whitelist the entire root and blind the gate.
    """
    seen, out = set(), []

    def add(pattern: str) -> None:
        if pattern and pattern != "*" and pattern not in (".", "..") and pattern not in seen:
            seen.add(pattern)
            out.append(pattern)

    for line in doc_path.read_text(encoding="utf-8").splitlines():
        if not line.lstrip().startswith("|"):
            continue
        cells = line.split("|")
        if len(cells) < 2:
            continue
        spans = [s for s in (_PLACEHOLDER.sub("*", t.strip()) for t in _BACKTICK.findall(cells[1])) if s]
        slashed = any("/" in s for s in spans)
        bases = [s for s in spans if "/" not in s and "*" not in s and not s.startswith(".")]
        for span in spans:
            if "/" in span:
                add(span.split("/", 1)[0])
            elif not slashed:
                add(span)
                if span.startswith(".") and len(span) > 1 and "*" not in span:
                    for base in bases:
                        add(base + span)
    return out


def undocumented(dir_path: Path, doc_path: Path | None = None) -> list[str]:
    """Top-level entries of ``dir_path`` no doc pattern covers. Never recursive."""
    patterns = top_level_patterns(doc_path or DEFAULT_DOC)
    names = sorted(entry.name for entry in dir_path.iterdir())
    return [n for n in names if not any(fnmatch.fnmatchcase(n, p) for p in patterns)]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Report state-root entries the inventory doc does not name.")
    parser.add_argument("--report", required=True, type=Path, metavar="DIR")
    parser.add_argument("--doc", type=Path, default=DEFAULT_DOC)
    args = parser.parse_args(argv)
    try:
        missing = undocumented(args.report, args.doc)
    except OSError as exc:
        print(f"state-root-inventory: cannot read {args.report} or {args.doc}: {exc}", file=sys.stderr)
        return 2
    if not missing:
        print(f"state-root-inventory: {args.report} fully documented by {args.doc}")
        return 0
    print("\n".join(missing))
    print(f"state-root-inventory: {len(missing)} undocumented top-level entr(y|ies) in {args.report}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
