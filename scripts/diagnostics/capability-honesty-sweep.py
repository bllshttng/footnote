#!/usr/bin/env python3
"""Find capability-table fields that were declared rather than measured.

The table has produced two defects of this class, and they do not share a shape,
which is why one sweep is not enough:

  1. UNIFORM. `stop_hook` read "native" on every row, and
     `send_keys_enter_delay_ms` read 0 on four of five, before that. A field
     holding one value across every harness has not been measured for any of
     them. It was declared once and inherited since.

  2. FALSE NEGATIVE. codex declared `interactive_attach.kind = "unsupported"`
     while a working codex attach shipped hardcoded in Rust. That value is not
     uniform, it parses perfectly, and it is wrong. A uniformity sweep cannot
     see it. What it looks like instead is a NEGATIVE claim in the table beside
     a harness-NAMED implementation in the source, which is also the shape the
     capability-mirror law forbids.

So this prints three passes. A finding is a candidate, not a verdict: pair a
negative claim with the identifier that contradicts it, then go read both.

    python3 scripts/diagnostics/capability-honesty-sweep.py
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

CANON = "crates/fno-agents/src/harness_capabilities.toml"
# A value asserting that a capability is ABSENT. Each one is a claim that can be
# contradicted by an implementation somewhere else in the tree.
NEGATIVE = {'"unsupported"', '"refused"', '"none"', '""', "[]", "0", "false", "{}"}
SOURCE_DIRS = ("cli/src", "crates")


def repo_root() -> Path:
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=True,
    )
    return Path(out.stdout.strip())


def flatten(prefix: str, value: object, out: dict[str, str]) -> None:
    if isinstance(value, dict):
        for key, sub in value.items():
            flatten(f"{prefix}.{key}" if prefix else key, sub, out)
    else:
        out[prefix] = json.dumps(value, sort_keys=True)


# Segments that name the SHAPE of a field rather than the capability it is
# about. Pairing on these would match everything and find nothing.
STOPWORDS = {
    "kind", "tokens", "supported", "keys", "rule", "ids", "rule_ids", "strategy",
    "forms", "ms", "required", "timeout", "timeout_ms", "status", "labels",
    "response", "binding", "marker", "grant", "root", "state", "on", "prefix",
    "command", "pattern", "effort", "effort_labels", "allow", "deny", "once",
    "always", "send", "enter", "delay", "keys_enter_delay_ms", "manifest",
    "rules", "permission", "bypass", "switch",
}


def keywords(field: str) -> list[str]:
    """The capability words in a field path, minus the shape words."""
    words = []
    for segment in field.split("."):
        for word in segment.split("_"):
            if len(word) > 3 and word not in STOPWORDS and word not in words:
                words.append(word)
    return words


def iter_sources(root: Path) -> list[tuple[str, str]]:
    out = []
    for directory in SOURCE_DIRS:
        base = root / directory
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*")):
            if path.suffix not in (".rs", ".py") or not path.is_file():
                continue
            if "/tests/" in str(path) or path.name.startswith("test_"):
                continue
            try:
                out.append((str(path.relative_to(root)), path.read_text(encoding="utf-8")))
            except (OSError, UnicodeDecodeError):
                continue
    return out


def named_hits(sources: list[tuple[str, str]], harness: str, words: list[str]) -> list[str]:
    """Definitions named after BOTH the harness and one of its capability words.

    Only EXPORTED definitions. A private helper is an implementation detail and
    a `#[cfg(test)]` function is a fixture; neither is the capability shipping
    behind the table's back, and matching them buries the ones that are.
    """
    pattern = re.compile(
        r"\b(?:pub(?:\([a-z]+\))?\s+(?:fn|const|static)|def)\s+\w*"
        + harness
        + r"\w*(?:" + "|".join(re.escape(w) for w in words) + r")\w*",
        re.IGNORECASE,
    )
    hits = []
    for rel, text in sources:
        for lineno, line in enumerate(text.splitlines(), 1):
            if pattern.search(line):
                hits.append(f"{rel}:{lineno}: {line.strip()[:100]}")
    return hits


def main() -> int:
    root = repo_root()
    table = tomllib.load((root / CANON).open("rb"))
    rows = table["harness"]
    harnesses = sorted(rows)

    flat: dict[str, dict[str, str]] = {}
    for name in harnesses:
        out: dict[str, str] = {}
        flatten("", rows[name], out)
        flat[name] = out
    fields = sorted({key for row in flat.values() for key in row})

    findings = 0

    print(f"table: {CANON}  map_version={table['map_version']}  rows={len(harnesses)}")

    print("\n=== 1. uniform fields (one value across every row) ===")
    uniform = [
        f for f in fields if len({flat[h].get(f, "<absent>") for h in harnesses}) == 1
    ]
    if uniform:
        findings += len(uniform)
        for field in uniform:
            print(f"  UNIFORM  {field} = {flat[harnesses[0]].get(field)}")
        print("\n  One value on every row is a declaration inherited, not a")
        print("  measurement taken. Measure each row or prove the uniformity.")
    else:
        print("  none. Every field carries at least two distinct values.")

    print("\n=== 2. negative claims (a capability declared ABSENT) ===")
    negatives: dict[str, list[str]] = {}
    for name in harnesses:
        negatives[name] = [f for f in fields if flat[name].get(f) in NEGATIVE]
        print(f"  {name}: {len(negatives[name])}")

    print("\n=== 3. a negative claim beside a harness-NAMED implementation ===")
    print("  The pairing is the finding. An identifier named after a harness AND")
    print("  after the capability that harness declares absent is the shape that")
    print("  hid a working codex attach behind an `unsupported` row.")
    sources = list(iter_sources(root))
    paired = 0
    for name in harnesses:
        for field in negatives[name]:
            words = keywords(field)
            if not words:
                continue
            hits = named_hits(sources, name, words)
            if not hits:
                continue
            paired += 1
            findings += 1
            print(f"\n  {name}.{field} = {flat[name][field]}  (declared absent)")
            for hit in hits[:6]:
                print(f"      {hit}")
    if not paired:
        print("\n  none.")

    print("\n=== 4. hardcoded harness lists (a name standing in for a capability) ===")
    print("  The capability-mirror law: never key on a harness NAME for its own")
    print("  sake. A literal list of harness names is a tier the table should own,")
    print("  and a harness that gains the capability will not move without an edit.")
    listed = 0
    literal = re.compile(
        r"[\[(]\s*\"(?:" + "|".join(harnesses) + r")\"\s*,\s*\"(?:"
        + "|".join(harnesses)
        + r")\"[^\]\)]*[\])]"
    )
    for rel, text in sources:
        for lineno, line in enumerate(text.splitlines(), 1):
            if literal.search(line):
                listed += 1
                findings += 1
                print(f"  {rel}:{lineno}: {line.strip()[:110]}")
    if not listed:
        print("  none.")

    print(f"\n{findings} candidate(s). Read each before you believe it.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
