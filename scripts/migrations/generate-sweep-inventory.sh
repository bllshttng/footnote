#!/usr/bin/env bash
# Generate a machine-readable inventory of direct scripts/lib/*.sh and
# scripts/<name>.py invocations across the migration surface for the
# canonical-instruction sweep (ab-cf715197).
#
# Output: .fno/sweep-inventory.json (gitignored, transient).
# Read-only: this script never modifies source files.
#
# Migration surface:
#   skills/**/*.md
#   hooks/*.sh
#   scripts/lint/*.sh
#
# Excluded surfaces:
#   cli/tests/**            (tests stub the bash scripts via PATH; intentional)
#   scripts/lib/*.sh        (canonical scripts; substrate stays unchanged)
#   skills/*/scripts/lib/   (bundled skill copies; regenerate from canonical)
#
# Usage:
#   bash scripts/migrations/generate-sweep-inventory.sh
#
# Exit codes:
#   0  inventory written successfully
#   1  unexpected error (jq/python missing, repo root not found, etc.)

set -eu

REPO_ROOT="${FNO_REPO_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null)}"
if [[ -z "${REPO_ROOT}" || ! -d "${REPO_ROOT}" ]]; then
    echo "error: cannot resolve repo root" >&2
    exit 1
fi

cd "${REPO_ROOT}"
mkdir -p .fno
OUTPUT=".fno/sweep-inventory.json"

# Use python for JSON assembly - portable across bash 3/4/5 and macOS/Linux.
python3 - "${REPO_ROOT}" "${OUTPUT}" <<'PYEOF'
import json
import re
import sys
from pathlib import Path

repo_root = Path(sys.argv[1])
output_path = Path(sys.argv[2])

# (regex pattern, fno replacement, kind hint)
# Kind: documentation (markdown/.md), code (.sh/.py), unmapped (no replacement)
MAPPINGS = [
    (r"\bbash (?:\./)?scripts/lib/set-gate\.sh\b", "fno gate set"),
    (r"\bbash (?:\./)?scripts/lib/emit-gate-transition\.sh\b", "fno gate transition"),
    (r"\bbash (?:\./)?scripts/lib/verify-pr-merged\.sh\b", "fno pr verify --kind merged"),
    (r"\bbash (?:\./)?scripts/lib/verify-review-replies\.sh\b", "fno pr verify --kind reviews"),
    (r"\bbash (?:\./)?scripts/lib/verify-event-evidence\.sh\b", "fno event verify-evidence"),
    (r"\bbash (?:\./)?scripts/lib/phase-verifier\.sh\b", "fno phase verify"),
    (r"\bbash (?:\./)?scripts/lib/kill-criteria\.sh\b", "fno phase kill-check"),
    (r"\bbash (?:\./)?scripts/lib/infer-task-executor\.sh\b", "fno executor resolve"),
    (r"\bbash (?:\./)?scripts/lib/parse-locked-executor\.sh\b", "fno executor resolve"),
    (r"\bbash (?:\./)?scripts/lib/rebase-resolve\.sh\b", "fno pr rebase"),
    (r"\bbash (?:\./)?scripts/lib/notify\.sh\b", "fno notify"),
    (r"\bbash (?:\./)?scripts/lib/pr-merge\.sh\b", "fno pr merge"),
    (r"\bpython3? (?:\./)?scripts/roadmap-tasks\.py\b", "fno backlog"),
    (r"\bpython3? (?:\./)?scripts/lib/stamp-plan\.py\b", "fno plan stamp"),
]

# Catch-all for unmapped direct invocations (flag for human review).
# The pattern is idempotent by construction: it only matches the legacy
# `bash scripts/lib/...` / `python3 scripts/...` forms, so already-migrated
# `fno <verb>` lines never appear in the inventory. No line-level
# "already migrated" guard is needed and would be unsound: a line that
# carries both a migrated and an unmigrated call (rare but possible in
# diff hunks or before/after docs) must still flag the unmigrated half.
UNMAPPED_PATTERN = re.compile(
    r"\b(bash (?:\./)?scripts/lib/[A-Za-z0-9_-]+\.sh|python3? (?:\./)?scripts/[A-Za-z0-9_/-]+\.py)\b"
)


def collect_surface_files() -> list[Path]:
    """Return migration-surface files relative to repo_root."""
    files: list[Path] = []
    # skills/**/*.md
    for p in (repo_root / "skills").rglob("*.md"):
        rel = p.relative_to(repo_root)
        # Exclude bundled scripts/lib/ subtrees
        parts = rel.parts
        if "scripts" in parts and "lib" in parts:
            # Bundled copy within a skill - skip
            continue
        files.append(rel)
    # hooks/*.sh
    for p in (repo_root / "hooks").glob("*.sh"):
        files.append(p.relative_to(repo_root))
    # scripts/lint/*.sh
    for p in (repo_root / "scripts" / "lint").glob("*.sh"):
        files.append(p.relative_to(repo_root))
    return sorted(set(files))


def file_kind(rel_path: Path) -> str:
    suffix = rel_path.suffix
    if suffix == ".md":
        return "documentation"
    if suffix in (".sh", ".py"):
        return "code"
    return "unknown"


def scan_file(rel_path: Path) -> list[dict]:
    abs_path = repo_root / rel_path
    try:
        text = abs_path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return []
    entries: list[dict] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        # Record every span MAPPINGS consumed so the UNMAPPED catch-all,
        # which also matches the legacy form, does not double-flag the
        # same callsite as both mapped and unmapped.
        mapped_spans: list[tuple[int, int]] = []
        for pattern, abi_verb in MAPPINGS:
            for m in re.finditer(pattern, line):
                mapped_spans.append(m.span())
                entries.append({
                    "file": str(rel_path),
                    "line": lineno,
                    "current": line.rstrip(),
                    "match": m.group(0),
                    "proposed": abi_verb,
                    "kind": file_kind(rel_path),
                })
        # Catch unmapped direct invocations for human review. Iterate
        # independently of the mapped pass so a line with both shapes
        # surfaces both kinds (e.g. before/after diff docs).
        for um in UNMAPPED_PATTERN.finditer(line):
            span = um.span()
            if any(s <= span[0] and span[1] <= e for s, e in mapped_spans):
                # This span was already accounted for by a MAPPINGS entry.
                continue
            entries.append({
                "file": str(rel_path),
                "line": lineno,
                "current": line.rstrip(),
                "match": um.group(0),
                "proposed": None,
                "kind": "unmapped",
            })
    return entries


def main() -> int:
    all_entries: list[dict] = []
    for rel in collect_surface_files():
        all_entries.extend(scan_file(rel))
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(all_entries, indent=2) + "\n", encoding="utf-8")
    mapped = sum(1 for e in all_entries if e["proposed"] is not None)
    unmapped = sum(1 for e in all_entries if e["proposed"] is None)
    print(f"sweep-inventory: {len(all_entries)} entries ({mapped} mapped, {unmapped} unmapped)")
    print(f"output: {output_path}")
    return 0


sys.exit(main())
PYEOF
