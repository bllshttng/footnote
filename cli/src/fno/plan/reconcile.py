"""Plan-vs-reality reconciliation delta (x-a7be, change C).

Cheap, advisory heuristics that answer "is this plan stale?" for the target
orientation report. NOT a gate -- printed for the agent's judgment, never an
auto-skip (guidelines-not-gates).

Start cheap: a plan's named file paths are the strongest cheap signal. A path
the plan says it will modify that no longer exists is a ``stale-reference``; one
that exists is ``present``. Deeper classes (shipped-by-PR, superseded-by-node)
are added only if this cheap pass misses a real dead-code case -- per the plan's
"start cheap; go deeper only if the dead-code class is missed."

ponytail: path-existence is the whole heuristic. Add PR/node cross-checks only
when a real stale plan slips through as "present".
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Optional, Union

# Backticked token that looks like a repo path: contains a slash and ends in a
# dotted extension, no whitespace. The slash requirement excludes dotted config
# keys (`config.target.blast`) and bare verbs (`fno target status`), which are
# the common false positives in a plan's prose.
_PATH_RE = re.compile(r"`([^`\s]+/[^`\s]+\.[A-Za-z0-9_]+)`")


@dataclass(frozen=True)
class PathStatus:
    path: str
    status: str  # "present" | "stale-reference"


@dataclass(frozen=True)
class ReconcileDelta:
    present: int
    stale: int
    paths: tuple[PathStatus, ...]
    note: Optional[str] = None  # set when the plan could not be read/parsed

    def summary(self) -> str:
        """One advisory line for the orientation report's ``plan:`` field."""
        if self.note:
            return self.note
        total = self.present + self.stale
        if total == 0:
            return "none (no file paths found in plan)"
        parts = [f"{self.present} present"]
        if self.stale:
            parts.append(f"{self.stale} stale-reference")
        return f"{total} paths: " + ", ".join(parts)


def _extract_paths(text: str) -> list[str]:
    """Ordered, de-duplicated repo-path tokens mentioned in the plan."""
    seen: dict[str, None] = {}
    for m in _PATH_RE.finditer(text):
        seen.setdefault(m.group(1), None)
    return list(seen)


def reconcile_plan(
    plan_path: Union[str, Path], repo_root: Path
) -> ReconcileDelta:
    """Classify each file path a plan names as present or stale-reference.

    ``plan_path`` may carry a ``#fragment`` index pointer; it is stripped. Any
    read error degrades to a ``note`` rather than raising -- the report must
    print ``unknown`` + the resolving command, never abort init.
    """
    raw = str(plan_path)
    file_part = raw.split("#", 1)[0]
    try:
        text = Path(file_part).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        return ReconcileDelta(0, 0, (), note=f"unknown (plan unreadable: {exc})")

    statuses: list[PathStatus] = []
    present = stale = 0
    for path in _extract_paths(text):
        if (repo_root / path).exists():
            statuses.append(PathStatus(path, "present"))
            present += 1
        else:
            statuses.append(PathStatus(path, "stale-reference"))
            stale += 1
    return ReconcileDelta(present, stale, tuple(statuses))


def _self_check() -> None:
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        root = Path(d)
        (root / "src").mkdir()
        (root / "src" / "here.py").write_text("x = 1\n", encoding="utf-8")
        plan = root / "p.md"
        plan.write_text(
            "edits `src/here.py` and `src/gone.py`; gate `config.a.b`\n",
            encoding="utf-8",
        )
        delta = reconcile_plan(plan, root)
        assert delta.present == 1, delta
        assert delta.stale == 1, delta
        assert len(delta.paths) == 2, delta  # config.a.b excluded (no slash)
        miss = reconcile_plan(root / "nope.md", root)
        assert miss.note and "unknown" in miss.summary(), miss
    print("reconcile self-check OK")


if __name__ == "__main__":
    _self_check()
