#!/usr/bin/env python3
"""check-skill-frontmatter.py -- validate a skill's frontmatter declares
a required binary dependency.

Used by ``scripts/lint/no-cross-skill-runtime-calls.sh`` (the
marketplace-readiness lint) to verify each driver skill's SKILL.md has
``requires.binaries`` containing the named binary (default ``fno``).

Exit codes:
    0  binary is declared in requires.binaries
    1  frontmatter parsed OK but binary not declared (or no requires block)
    2  PyYAML missing OR file not found OR frontmatter malformed

The 2-vs-1 split matters: the caller branches on rc to surface the right
diagnostic (PyYAML missing is a substrate problem with its own fix; missing
fno is an authoring problem). Under the lint's ``set -e``, the caller wraps
this script in an if/else to capture the rc.

Usage:
    python3 scripts/lib/check-skill-frontmatter.py <skill.md> [--require BINARY]

Examples:
    python3 scripts/lib/check-skill-frontmatter.py skills/target/SKILL.md
    python3 scripts/lib/check-skill-frontmatter.py skills/target/SKILL.md --require fno
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path


def _load_frontmatter(text: str):
    """Return (frontmatter_dict, error_message). On parse failure returns
    (None, message). Caller maps None → rc=2 (malformed/substrate)."""
    if not text.startswith("---\n"):
        return None, "no frontmatter (file does not begin with '---\\n')"
    fm_end = text.find("\n---\n", 4)
    if fm_end == -1:
        return None, "unterminated frontmatter (no closing '\\n---\\n')"
    try:
        import yaml
    except ImportError:
        return None, "PyYAML not available; install pyyaml"
    try:
        fm = yaml.safe_load(text[4:fm_end])
    except yaml.YAMLError as exc:
        return None, f"YAML parse error: {exc}"
    if not isinstance(fm, dict):
        return None, f"frontmatter must be a mapping, got {type(fm).__name__}"
    return fm, ""


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="check-skill-frontmatter.py")
    parser.add_argument("skill_md", help="path to SKILL.md")
    parser.add_argument(
        "--require",
        default="fno",
        help="binary name that must appear in requires.binaries (default: fno)",
    )
    args = parser.parse_args(argv)

    path = Path(args.skill_md)
    if not path.is_file():
        print(f"ERROR: skill file not found: {path}", file=sys.stderr)
        return 2

    text = path.read_text(encoding="utf-8")
    fm, err = _load_frontmatter(text)
    if fm is None:
        print(f"ERROR: {err}", file=sys.stderr)
        return 2

    reqs = (fm.get("requires") or {}).get("binaries") or []
    if not isinstance(reqs, list):
        # `binaries` declared but not a list — author error. Treat as
        # missing-binary (rc=1) since the structural shape is wrong.
        return 1

    return 0 if any(args.require in str(r) for r in reqs) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
