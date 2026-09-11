"""Plan rulings: ``consolidation.rejected`` entries that name a foreign node.

A plan can rule another node's proposal out, and the ruling used to stay
where the planner wrote it, in plan frontmatter - invisible to every reader
the ruled-out node consults. :func:`plan_rulings` is the one scan; the
decisions verb, the think inspect receipt, and the reversal verbs all call it
rather than re-walking the plans directory.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any


def plan_rulings(node_id: str, plans_dir: Path) -> dict[str, Any]:
    """Find plans whose ``consolidation.rejected`` names *node_id*.

    Returns a status-carrying result and never raises. A missing or
    unreadable directory is ``unavailable``, because a caller that folded it
    into ``ok`` with zero rows would read a broken scan as "no ruling
    exists" - the exact false zero this module exists to prevent.

    The precheck keeps the scan cheap: YAML parses only for the rare file
    whose frontmatter text mentions the id (0.3s over 1752 plans, against 14s
    for one graph read). The graph is never read.
    """
    plans_dir = Path(plans_dir)
    result: dict[str, Any] = {
        "status": "ok",
        "dir": str(plans_dir),
        "scanned": 0,
        "skipped": [],
        "detail": None,
        "rulings": [],
    }
    if not plans_dir.is_dir():
        result["status"] = "unavailable"
        result["detail"] = (
            "not a directory" if plans_dir.exists() else "directory does not exist"
        )
        return result

    from fno.plan._doc import _split_frontmatter

    want = node_id.strip()
    if not want:
        return result
    try:
        paths = sorted(plans_dir.glob("*.md"))
    except OSError as exc:  # noqa: PERF203 - one degraded read, named
        result["status"] = "unavailable"
        result["detail"] = str(exc)
        return result

    for path in paths:
        result["scanned"] += 1
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        fm_text, _body = _split_frontmatter(text)
        if want not in fm_text:
            continue
        try:
            import yaml

            frontmatter = yaml.safe_load(fm_text)
        except Exception:  # noqa: BLE001 - one malformed file costs its path, not the scan
            result["skipped"].append(str(path))
            continue
        if not isinstance(frontmatter, dict):
            continue
        block = frontmatter.get("consolidation")
        if not isinstance(block, dict):
            continue
        rejected = block.get("rejected")
        if not isinstance(rejected, list):
            continue
        hits = [
            entry
            for entry in rejected
            if isinstance(entry, dict)
            and str(entry.get("id") or "").strip() == want
        ]
        if not hits:
            continue
        from fno.graph._intake import plan_claims

        claims = plan_claims(str(path))
        for entry in hits:
            result["rulings"].append(
                {
                    "node": want,
                    "by": sorted(claims),
                    "plan_path": str(path),
                    "reason": str(entry.get("reason") or ""),
                }
            )
    return result


def ruling_lines(result: dict, prefix: str, node_id: str) -> list[str]:
    """The stderr lines a reversal verb prints for one node's plan rulings.

    Both ``undefer`` and ``unsupersede`` print from here, so the two verbs
    say the same words about the same ruling.
    """
    if result.get("status") == "unavailable":
        return [
            f"{prefix}: plan rulings for {node_id} not read "
            f"({result.get('dir')}: {result.get('detail')})"
        ]
    lines: list[str] = []
    for ruling in result.get("rulings") or []:
        by = ", ".join(ruling.get("by") or []) or "(unclaimed)"
        lines.append(
            f"{prefix}: {node_id} is rejected by {by} in "
            f"{ruling.get('plan_path')}: {ruling.get('reason')}"
        )
    return lines
