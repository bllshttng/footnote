"""Per-repo branch-provenance cache: the pr-watch stranded leg writes it, the Kanban board reads it. File contract: docs/state-root-inventory.md."""

from __future__ import annotations

import json
import logging
import os
import tempfile
from pathlib import Path

from fno.worktree_stranded import CLEAN

CACHE_RELPATH = ".fno/branch-provenance.json"


def write_cache(repo: Path, rows: list) -> bool:
    """Write the non-CLEAN rows when they differ from the cache; True then."""
    out = [
        {
            "branch": row.facts.get("branch"),
            "node": row.node,
            "klass": row.klass,
            "unpushed": row.unpushed,
            "has_remote": row.facts.get("has_remote"),
            "age": row.age,
            "pr_number": row.facts.get("pr_number"),
            "live": row.facts.get("live"),
        }
        for row in rows
        if row.klass != CLEAN
    ]
    if out == read_cache(repo):
        return False
    target = Path(repo) / CACHE_RELPATH
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=target.parent, suffix=".tmp")
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(out, f)
        os.replace(tmp, str(target))
        return True
    except Exception as exc:  # noqa: BLE001 - display cache, never break the tick
        logging.getLogger(__name__).warning("branch_provenance_cache: write failed for %s: %s", repo, exc)
        return False


def read_cache(repo: Path) -> list[dict]:
    try:
        data = json.loads((Path(repo) / CACHE_RELPATH).read_text(encoding="utf-8"))
        return [r for r in data if isinstance(r, dict)] if isinstance(data, list) else []
    except (OSError, ValueError):
        return []


def _provenance_roots() -> list[Path]:
    try:
        from fno.tracker import sidecar as sidecar_store

        cwds = {str(getattr(sc, "cwd", "")) for sc in sidecar_store.load_all().values()}
    except Exception:  # noqa: BLE001 - display signal; never break a mutation
        return []
    return [Path(c) for c in sorted(cwds) if c and Path(c).is_dir()]


def _provenance_line(row: dict) -> str:
    remote = row.get("has_remote")
    parts = [
        "has remote" if remote else ("no remote" if remote is False else "remote unknown"),
        f"{row.get('unpushed') or 0} unpushed",
        f"PR #{row['pr_number']}" if row.get("pr_number") else "no PR",
        *(["LIVE"] if row.get("live") else []),
        f"newest commit {row.get('age') or 'unknown'}",
    ]
    node = row.get("node")
    label = f"**{node}**" if node else "*(unmapped)*"
    return f"- {label} ({row.get('branch') or 'no branch'}): {', '.join(parts)}"


def provenance_lines(roots: list[Path] | None = None) -> list[str]:
    try:
        rows = [r for root in (roots if roots is not None else _provenance_roots()) for r in read_cache(root)]
    except Exception:  # noqa: BLE001 - display signal; never break a mutation
        return []
    return ["## Branch Provenance", "", *(_provenance_line(r) for r in rows), ""] if rows else []
