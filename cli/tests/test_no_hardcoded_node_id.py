"""Guard against silently re-hardcoding the node-ID scheme (ab-bbfccb8f, T3.2).

Two invariants, enforced across cli/src, scripts, hooks, and crates:

1. **Single generation path.** Every node-id mint routes through
   ``fno.graph._constants.mint_node_id``; no inline ``f"{ID_PREFIX}{uuid...hex[:N]}"``
   (or a literal ``ab-`` interpolated with a uuid) survives outside the canonical
   module. A stray inline mint would silently ignore the configured prefix/width.

2. **No stray format matcher.** No hardcoded ``ab-[0-9a-f]{N}`` regex literal
   survives outside the canonical module + the documented legacy allowlist. Such
   a literal would silently fail to recognize a configured-format id (AC4-FR).

A genuinely-new violation fails CI naming ``file:line`` so the author can route
it through ``mint_node_id`` / ``is_wellformed_node_id`` instead.

Filter: `uv run pytest cli/tests/test_no_hardcoded_node_id.py -q`
"""
from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

# Trees that must stay on the canonical scheme. crates/ is included because the
# Rust side treats unit_id as opaque - a format-asserting ab- regex there would
# be a new, hidden gate.
SCAN_DIRS = ["cli/src/fno", "scripts", "hooks", "crates"]

# The canonical module IS the single source of truth (defines the grammar + the
# mint), so it is the one place these patterns are allowed to live.
CANONICAL_MODULE = "cli/src/fno/graph/_constants.py"

# Allowlist for an INTENTIONAL legacy ``ab-[0-9a-f]{8}`` literal. The target
# bootstrap keeps an exact legacy fast-path (``^ab-[0-9a-f]{8}$`` OR'd with the
# configured grammar + graph-existence check) so legacy ``/target ab-<8hex>``
# stays byte-identical (AC3-FR) without a graph read. This is a deliberate
# optimization, NOT a missed gate: configured ids are handled by the grammar
# branch alongside it. New entries here require an equivalent justification.
LEGACY_FASTPATH_FILES = {
    "hooks/helpers/init-target-state.sh",
}

# A hardcoded format matcher: ``ab-`` glued to a hex character class.
_FORMAT_LITERAL_RE = re.compile(r"ab-\[(?:0-9a-f|a-f0-9|0-9A-Fa-f)\]")

# An inline node-id mint: a node prefix interpolated with a uuid hex slice.
_INLINE_MINT_RES = [
    re.compile(r"\{(?:ID_PREFIX|LEGACY_PREFIX)\}.*hex\["),
    re.compile(r"""f["']ab-\{.*uuid"""),
    re.compile(r"""["']ab-["']\s*\+.*uuid"""),
]


def _rel(path: Path) -> str:
    return path.relative_to(REPO_ROOT).as_posix()


def _is_test_or_historical(rel: str) -> bool:
    return (
        "/tests/" in rel
        or "/test/" in rel
        or Path(rel).name.startswith("test_")
        or rel.endswith(".jsonl")
        or "/__pycache__/" in rel
        or "/benchmarks/" in rel
        or "/internal/" in rel
        or rel.endswith(".pyc")
    )


def _iter_source_lines():
    exts = {".py", ".sh", ".rs", ".bash"}
    for d in SCAN_DIRS:
        base = REPO_ROOT / d
        if not base.exists():
            continue
        for path in base.rglob("*"):
            if not path.is_file() or path.suffix not in exts:
                continue
            rel = _rel(path)
            if _is_test_or_historical(rel):
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            for n, line in enumerate(text.splitlines(), start=1):
                yield rel, n, line


def test_no_inline_node_id_mint_outside_canonical():
    """Invariant 1: mints route through mint_node_id."""
    violations = []
    for rel, n, line in _iter_source_lines():
        if rel == CANONICAL_MODULE:
            continue
        if any(rx.search(line) for rx in _INLINE_MINT_RES):
            violations.append(f"{rel}:{n}: {line.strip()}")
    assert not violations, (
        "Inline node-id mint(s) found outside the canonical module. Route "
        "through fno.graph._constants.mint_node_id:\n" + "\n".join(violations)
    )


def test_no_stray_ab_hex_format_literal():
    """Invariant 2: no hardcoded ab-[0-9a-f]{N} outside canonical + allowlist."""
    violations = []
    for rel, n, line in _iter_source_lines():
        if rel == CANONICAL_MODULE or rel in LEGACY_FASTPATH_FILES:
            continue
        if _FORMAT_LITERAL_RE.search(line):
            violations.append(f"{rel}:{n}: {line.strip()}")
    assert not violations, (
        "Hardcoded ab-[0-9a-f] format literal(s) found. Use "
        "fno.graph._constants.is_wellformed_node_id / has_node_id_prefix "
        "(or add a justified entry to LEGACY_FASTPATH_FILES):\n"
        + "\n".join(violations)
    )


def test_guard_actually_scans_something():
    """Sanity: the scanner sees real source (guards against a silently-empty
    sweep that would let both invariants pass vacuously)."""
    seen = sum(1 for _ in _iter_source_lines())
    assert seen > 500, f"scanner only saw {seen} lines; SCAN_DIRS likely wrong"
