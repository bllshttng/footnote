"""Guard against reintroducing references to Group 3's deletion list.

Group 3 (deletions and GC) removed several capture/migration paths that had
zero readers anywhere in the codebase: the convo-signals capture hook, the
tasks.json/tasks.md -> ledger.json migration shim, and the metrics.jsonl
analytics reader (scripts/metrics/analyze.sh). A future edit that quietly
reintroduces a reference to one of these dead basenames or symbols would
resurrect write-only state with no reader; this test fails naming the
offending file:line instead.

Filter: `uv run pytest cli/tests/test_no_stale_deleted_capture_refs.py -q`
"""
from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

SCAN_DIRS = ["cli/src/fno", "scripts", "hooks", "crates"]

# scripts/prune-fno-dir.sh intentionally names these deleted basenames in its
# disposition table (they are its DELETE_TARGETS), and cli/src/fno/doctor.py
# names them in its orphan-file detector - both are the janitor doing its
# job, not a stale reference.
#
# cli/src/fno/cost/_session_cost.py keeps "tasks.json"/"tasks.md" in its own
# docstrings and --help text: those describe the (unrenamed) function family
# render_tasks_md()/backfill_tasks_json(), which already routes through
# paths.ledger_json() internally (ab-58645f63) - renaming the functions
# themselves is a separate refactor, out of scope for this GC pass.
ALLOWLIST_FILES = {
    "scripts/prune-fno-dir.sh",
    "cli/src/fno/doctor.py",
    "cli/src/fno/cost/_session_cost.py",
}

_DELETED_REFS_RE = re.compile(
    r"_OLD_TASKS_PATH\b"
    r"|_OLD_TASKS_MD\b"
    r"|convo-signal-capture\.sh"
    r"|convo-signals\.jsonl"
    r"|metrics/analyze\.sh"
    r"|\bmetrics\.jsonl\b"
    r"|\btasks\.json\b"
    r"|\btasks\.md\b"
)


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
    # .json is included so a hook re-registration (hooks/hooks.json) would be
    # caught, not just the .sh hook script itself.
    exts = {".py", ".sh", ".rs", ".bash", ".json"}
    for d in SCAN_DIRS:
        base = REPO_ROOT / d
        if not base.exists():
            continue
        for path in base.rglob("*"):
            if not path.is_file() or path.suffix not in exts:
                continue
            rel = _rel(path)
            if _is_test_or_historical(rel) or rel in ALLOWLIST_FILES:
                continue
            try:
                text = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            for n, line in enumerate(text.splitlines(), start=1):
                yield rel, n, line


def test_no_stale_reference_to_deleted_capture_paths():
    violations = []
    for rel, n, line in _iter_source_lines():
        if _DELETED_REFS_RE.search(line):
            violations.append(f"{rel}:{n}: {line.strip()}")
    assert not violations, (
        "Reference(s) to a Group 3 deletion-list basename/symbol found. These "
        "had zero readers and were removed; resurrect only with a concrete "
        "reader in hand:\n" + "\n".join(violations)
    )


def test_guard_actually_scans_something():
    """Sanity: the scanner sees real source, so a clean pass isn't vacuous."""
    seen = sum(1 for _ in _iter_source_lines())
    assert seen > 500, f"scanner only saw {seen} lines; SCAN_DIRS likely wrong"
