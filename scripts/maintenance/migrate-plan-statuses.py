#!/usr/bin/env python3
"""One-time migration: normalize legacy plan-doc frontmatter statuses.

The vault carries ~85 distinct historical `status:` values (COMPLETE, draft,
ready-for-implementation, inp-progres, ...). The canonical vocabulary is the
Pydantic ladder in `fno.plan._status`: design -> ready -> in_progress ->
in_review, plus the off-axis terminals done/superseded.
This script maps the legacy variants onto that ladder so the Obsidian Bases
views stop showing a smear of dead statuses.

Two signals, graph wins:
  1. Graph-truth override: if a doc's `node` resolves in graph.json (or the
     archive) as done/superseded, the doc's status becomes done/superseded
     regardless of the mapping. This alone fixes most stale `ready` docs.
  2. Mapping table: otherwise the legacy string maps by family.
  Unmapped values are reported and left untouched.

Dry-run by default (prints a per-file old->new table). `--apply` writes
frontmatter-only, byte-preserving edits (single-line scalar) via `_stamp`.
NOT a loc-ratchet control-plane path.
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

# Run-directly bootstrap: make `fno.plan.*` importable without an install.
_REPO = Path(__file__).resolve().parents[2]
_SRC = _REPO / "cli" / "src"
if _SRC.is_dir() and str(_SRC) not in sys.path:
    sys.path.insert(0, str(_SRC))

from fno.plan._status import KNOWN_STATUSES, canonical_status  # noqa: E402
from fno.plan._stamp import read_plan_file, write_plan_file  # noqa: E402

# Legacy status -> canonical ladder value. Keys are matched case-insensitively
# after strip. A value already in KNOWN_STATUSES is left as-is (never listed).
_MAPPING: dict[str, str] = {}


def _add(canonical: str, *variants: str) -> None:
    for v in variants:
        _MAPPING[v.strip().lower()] = canonical


_add("done", "complete", "completed", "implemented", "merged", "✅",
     "shipped-pr1", "subsystem-shipped", "mostly-complete", "finished", "closed")
_add("design", "draft", "planned", "planning", "not-started", "proposed",
     "idea", "backlog", "todo", "spec", "discovery", "research", "researching",
     "design-doc", "designing")
_add("ready", "ready-for-implementation", "ready-for-impl", "ready-to-build",
     "ready-to-implement", "approved", "confirmed", "accepted")
_add("in_progress", "active", "in-progress", "inprogress", "in-flight",
     "inp-progres", "handoff", "handoff-pending", "wip", "started")
_add("in_review", "reviewing", "in-review", "ready-for-review", "reviewed",
     "waves_complete_awaiting_review", "awaiting-review")
_add("superseded", "abandoned", "cancelled", "canceled", "moved",
     "deferred", "future", "future-work", "obsolete", "wontfix")


def _graph_truth(graph_path: Path, archive_path: Path | None) -> dict[str, str]:
    """node id -> terminal status implied by the graph (done|superseded).

    A node with completed_at is done; superseded_by (or status superseded) is
    superseded. Reads the working graph and, if present, the archive. Best-effort:
    a missing/corrupt file yields no overrides.
    """
    import json

    truth: dict[str, str] = {}
    for p in (graph_path, archive_path):
        if not p or not p.exists():
            continue
        try:
            data = json.loads(p.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        for e in data.get("entries", []) if isinstance(data, dict) else []:
            nid = e.get("id")
            if not isinstance(nid, str):
                continue
            if e.get("completed_at"):
                truth[nid] = "done"
            elif e.get("superseded_by") or e.get("status") == "superseded":
                truth[nid] = "superseded"
    return truth


def _iter_plan_docs(root: Path):
    """Yield .md files under `root` that look like plan/design docs.

    Mirrors the Bases filter: a path segment `plans/` or `design/`. Skips the
    root's own non-plan notes to avoid touching unrelated frontmatter.
    """
    for p in root.rglob("*.md"):
        parts = {seg.lower() for seg in p.parts}
        if "plans" in parts or "design" in parts:
            yield p


def _resolve(value: object) -> str | None:
    if value is None:
        return None
    s = str(value).strip()
    return s or None


def _norm_key(s: str) -> str:
    # Space and hyphen are interchangeable separators in these ad-hoc statuses
    # ("In Progress" == "in-progress", "Ready for Implementation" ==
    # "ready-for-implementation"), so fold them before the table lookup.
    return s.strip().lower().replace(" ", "-")


def _classify(old: str, node_id: str | None, truth: dict[str, str]) -> str | None:
    """Return the new status, or None to leave untouched (already canonical or
    unmapped)."""
    if node_id and node_id in truth:
        new = truth[node_id]
        # A doc already AT the target rung under a retired spelling is not
        # stale, and rewriting it would turn the x-3ad5 rename into a migration
        # pass over the vault. The override still fixes a genuinely stale rung
        # (the `ready` doc whose node is done), which is what it exists for.
        return None if canonical_status(old) == new else new
    if old.strip().lower() in KNOWN_STATUSES:
        return None  # already canonical
    return _MAPPING.get(_norm_key(old))  # None => unmapped


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    default_root = _REPO / "internal"
    ap.add_argument("--root", type=Path, default=default_root,
                    help=f"Vault root to scan (default: {default_root})")
    ap.add_argument("--graph", type=Path, default=Path.home() / ".fno" / "graph.json",
                    help="graph.json for the graph-truth override")
    ap.add_argument("--archive", type=Path,
                    default=Path.home() / ".fno" / "graph-archive.json",
                    help="graph-archive.json (optional)")
    ap.add_argument("--apply", action="store_true",
                    help="Write the changes (default: dry-run table only)")
    ap.add_argument("--receipt", type=Path, default=None,
                    help="Write the change table + counts to this markdown file")
    args = ap.parse_args(argv)

    root = args.root.resolve()
    if not root.exists():
        print(f"error: root does not exist: {root}", file=sys.stderr)
        return 1

    truth = _graph_truth(args.graph, args.archive)

    changes: list[tuple[Path, str, str]] = []  # (path, old, new)
    unmapped: dict[str, int] = {}
    unparseable: list[Path] = []
    scanned = 0

    for doc in _iter_plan_docs(root):
        try:
            _, fields, _ = read_plan_file(doc)
        except (ValueError, OSError, FileNotFoundError):
            unparseable.append(doc)
            continue
        old = _resolve(fields.get("status"))
        if old is None:
            continue
        scanned += 1
        node_id = _resolve(fields.get("node"))
        new = _classify(old, node_id, truth)
        if new is None:
            if old.strip().lower() not in KNOWN_STATUSES:
                unmapped[old] = unmapped.get(old, 0) + 1
            continue
        if new != old:
            changes.append((doc, old, new))

    # Apply.
    written = 0
    if args.apply:
        for doc, _old, new in changes:
            try:
                target, fields, rest = read_plan_file(doc)
                fields["status"] = new
                write_plan_file(target, fields, rest)
                written += 1
            except (ValueError, OSError, FileNotFoundError) as exc:
                print(f"warning: could not write {doc}: {exc}", file=sys.stderr)

    # Remaining violations after apply: re-scan statuses not in the ladder.
    remaining = sum(unmapped.values())

    lines: list[str] = []
    lines.append(f"# Plan status migration ({'apply' if args.apply else 'dry-run'})")
    lines.append("")
    lines.append(f"- root: `{root}`")
    lines.append(f"- docs with a status: {scanned}")
    lines.append(f"- changes: {len(changes)}"
                 + (f" (written: {written})" if args.apply else " (dry-run, none written)"))
    lines.append(f"- unmapped (left untouched): {remaining}")
    lines.append(f"- unparseable frontmatter (skipped): {len(unparseable)}")
    lines.append("")
    if changes:
        lines.append("| file | old | new |")
        lines.append("|------|-----|-----|")
        for doc, old, new in sorted(changes, key=lambda c: str(c[0])):
            rel = doc.relative_to(root) if doc.is_relative_to(root) else doc
            lines.append(f"| {rel} | {old} | {new} |")
        lines.append("")
    if unmapped:
        lines.append("## Unmapped statuses (add to the table if they should map)")
        lines.append("")
        for status, count in sorted(unmapped.items(), key=lambda kv: -kv[1]):
            lines.append(f"- `{status}` x{count}")
        lines.append("")

    report = "\n".join(lines)
    print(report)

    if args.receipt:
        args.receipt.parent.mkdir(parents=True, exist_ok=True)
        args.receipt.write_text(report + "\n", encoding="utf-8")
        print(f"\nreceipt written: {args.receipt}", file=sys.stderr)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
