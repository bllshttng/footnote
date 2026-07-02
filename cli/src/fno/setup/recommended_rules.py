"""Install footnote's recommended claude-code rules into ~/.claude/rules/.

Opt-in only (wired into ``fno setup wizard``). Symlinks each shipped
``rules/*.md`` into the user's global ``~/.claude/rules/``, falling back to a
copy where symlinks are unavailable. Idempotent (running twice leaves exactly
one link per rule), and it never overwrites a real (non-symlink) file the user
has hand-edited.

Mirrors ``scripts/ensure-global-dir.sh``'s "merge into the user's global claude
config" pattern, but in Python so ``fno setup`` calls it directly instead of
shelling to a repo-root script (shellout-drift-safe). On a bare ``pip install
fno`` no ``rules/`` dir ships, so it degrades to installing nothing (exit clean).
"""
from __future__ import annotations

import os
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import List

# The index is documentation about the pack, not itself a loaded rule, so it is
# not installed into the user's rules dir.
_INDEX_NAME = "RULES.md"


@dataclass
class RuleResult:
    name: str
    action: str  # linked | copied | already | skipped-real
    target: Path


def install_recommended_rules(
    source_dir: Path, target_dir: Path, *, use_symlink: bool = True
) -> List[RuleResult]:
    """Install every ``*.md`` rule from ``source_dir`` into ``target_dir``.

    Returns one ``RuleResult`` per rule. A missing/empty ``source_dir`` installs
    nothing (returns ``[]``). ``target_dir`` is created if absent. A real file
    already at a target is preserved (``skipped-real``); a symlink is repointed
    at the source (idempotent).
    """
    source_dir = Path(source_dir)
    target_dir = Path(target_dir)
    if not source_dir.is_dir():
        return []

    rules = sorted(p for p in source_dir.glob("*.md") if p.name != _INDEX_NAME)
    if not rules:
        return []

    target_dir.mkdir(parents=True, exist_ok=True)
    results: List[RuleResult] = []
    for rule in rules:
        target = target_dir / rule.name
        # A real (non-symlink) file is a user edit: warn-and-skip, never clobber.
        if target.exists() and not target.is_symlink():
            results.append(RuleResult(rule.name, "skipped-real", target))
            continue
        # An existing symlink already points at our source -> nothing to do.
        if target.is_symlink() and _resolves_to(target, rule):
            results.append(RuleResult(rule.name, "already", target))
            continue
        # Stale symlink (points elsewhere / dangling): replace it so a second run
        # converges to exactly one correct link.
        if target.is_symlink():
            target.unlink()
        results.append(_place(rule, target, use_symlink))
    return results


def _resolves_to(link: Path, source: Path) -> bool:
    try:
        return os.path.realpath(link) == os.path.realpath(source)
    except OSError:
        return False


def _place(source: Path, target: Path, use_symlink: bool) -> RuleResult:
    if use_symlink:
        try:
            target.symlink_to(source)
            return RuleResult(source.name, "linked", target)
        except OSError:
            # ponytail: symlinks unavailable (Windows without privilege, some
            # filesystems) -> copy instead. A copy loses live-update, which the
            # RULES.md note documents.
            pass
    shutil.copy2(source, target)
    return RuleResult(source.name, "copied", target)


def summarize(results: List[RuleResult]) -> str:
    if not results:
        return "no recommended rules to install."
    lines = []
    for r in results:
        verb = {
            "linked": "linked",
            "copied": "copied",
            "already": "already installed",
            "skipped-real": "SKIPPED (your edited file kept)",
        }.get(r.action, r.action)
        lines.append(f"  {r.name}: {verb} -> {r.target}")
    return "\n".join(lines)
