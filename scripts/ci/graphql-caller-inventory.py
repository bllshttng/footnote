#!/usr/bin/env python3
"""Enumerate shell-shaped and argv-shaped GitHub GraphQL callers."""
from __future__ import annotations

import re
import subprocess
from pathlib import Path

ROOTS = ("cli/src", "crates/fno-agents/src", "hooks", "skills", "scripts")
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


def _disposition(relative: str, source: str, offset: int) -> str:
    line = source[source.rfind("\n", 0, offset) + 1 : source.find("\n", offset)]
    stripped = line.lstrip()
    suffix = Path(relative).suffix
    if (suffix in {".py", ".sh", ".toml", ".yaml", ".yml"} and stripped.startswith("#")) or (
        suffix == ".rs" and stripped.startswith("//")
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
