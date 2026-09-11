"""``fno agents king history`` - read one crown's recorded reign_checkin rows back.

Selection is deterministic read-back, never generated summary; the contract
and the why live in docs/architecture/reign.md.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any

REIGN_CHECKIN = "reign_checkin"

#: Pre-contract key spellings; a row carrying any is legacy evidence.
FORBIDDEN_ALIASES = ("crown", "crown_scope", "result")


class HistoryUnreadable(Exception):
    """No resolvable crown, or a corrupt journal line."""


def canonicalize_scope(scope: str) -> str:
    """The stored form of a crown scope, through the same path king init uses."""
    from fno.agents.crown import _canonical_members, canonical_scope

    return canonical_scope(list(_canonical_members(scope)))


def resolve_scope(explicit: str) -> str:
    """Explicit ``--scope`` wins; else the caller's crown, positively resolved.

    An unreadable registry, an unregistered identity, or an uncrowned row is
    a refusal, never an empty history.
    """
    if explicit.strip():
        canonical = canonicalize_scope(explicit)
        if not canonical:
            raise HistoryUnreadable(
                "--scope names no crown territory: name an epic or a project."
            )
        return canonical

    from fno.agents.crown import (
        AGENT_UNREGISTERED,
        REGISTRY_UNREADABLE,
        calling_agent_row,
    )

    try:
        row = calling_agent_row()
    except Exception as exc:  # noqa: BLE001 - a resolution failure refuses, never crashes
        raise HistoryUnreadable(
            f"cannot resolve the caller's crown: {exc}. Pass --scope explicitly."
        ) from exc
    if row is REGISTRY_UNREADABLE or row is AGENT_UNREGISTERED:
        raise HistoryUnreadable(
            "cannot resolve the caller's crown: this session carries an agent "
            "identity the registry does not resolve to a crowned row. Pass "
            "--scope explicitly."
        )
    own = getattr(row, "crown_scope", None)
    if not own:
        raise HistoryUnreadable(
            "this session holds no crown, so there is no reign history to "
            "read. Pass --scope <territory>."
        )
    return canonicalize_scope(own)


def _legacy_entry(lineno: int, aliases: list[str], data: dict) -> dict:
    missing = sorted(k for k in ("scope", "change") if k not in data)
    return {"line": lineno, "forbidden": aliases, "missing": missing}


def read_history(events_path: Path, scope: str) -> dict[str, Any]:
    """One journal scan; the canonical reign record for ``scope``, newest first.

    ``rejected`` counts every non-canonical ``reign_checkin`` row so a
    journal of pre-contract check-ins never reads as an empty reign;
    ``rejected_legacy`` names (by raw line number) those attributing to
    this crown.
    """
    result: dict[str, Any] = {
        "scope": scope,
        "events_path": str(events_path),
        "scanned": 0,
        "matched": 0,
        "events": [],
        "rejected": 0,
        "rejected_legacy": [],
    }
    if not events_path.exists():
        return result

    with events_path.open("r", encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, start=1):
            line = raw.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError as exc:
                raise HistoryUnreadable(
                    f"{events_path}:{lineno}: corrupt JSON line: {exc}"
                ) from exc
            if not isinstance(event, dict):
                raise HistoryUnreadable(
                    f"{events_path}:{lineno}: line is not a JSON object"
                )
            result["scanned"] += 1
            if event.get("type") != REIGN_CHECKIN:
                continue
            data = event.get("data")
            data = data if isinstance(data, dict) else {}
            aliases = sorted(k for k in FORBIDDEN_ALIASES if k in data)
            row_scope = data.get("scope")
            scope_ok = isinstance(row_scope, str) and bool(row_scope.strip())
            if scope_ok and "change" in data and not aliases:
                if canonicalize_scope(row_scope) == scope:
                    result["events"].append(event)
                continue
            result["rejected"] += 1
            spellings = ([row_scope] if scope_ok else []) + [
                data[k] for k in aliases if isinstance(data.get(k), str)
            ]
            if any(isinstance(s, str) and canonicalize_scope(s) == scope for s in spellings):
                result["rejected_legacy"].append(
                    _legacy_entry(lineno, aliases, data)
                )

    result["events"].reverse()
    result["matched"] = len(result["events"])
    return result


def render(result: dict[str, Any]) -> str:
    """Human output: each recorded check-in verbatim, newest first."""
    lines: list[str] = []
    for event in result["events"]:
        data = event.get("data") or {}
        lines.append(f"{event.get('ts', '')}  {data.get('scope', '')}")
        lines.append(f"  change: {data.get('change', '')}")
        rest = {k: v for k, v in data.items() if k not in ("scope", "change")}
        if rest:
            lines.append(
                "  evidence: " + json.dumps(rest, sort_keys=True, ensure_ascii=False)
            )
    lines.append(
        f"history: {result['matched']} canonical check-in(s) for "
        f"{result['scope']}, scanned {result['scanned']} rows, "
        f"{result['rejected']} legacy-invalid reign row(s)"
    )
    for entry in result["rejected_legacy"]:
        lines.append(
            f"  rejected legacy row at line {entry['line']}: "
            f"forbidden={entry['forbidden']} missing={entry['missing']}"
        )
    return "\n".join(lines)
