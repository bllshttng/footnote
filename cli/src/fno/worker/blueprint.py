"""fno agents worker blueprint - signal that LLM work is needed.

The CLI does NOT write feature code. It emits a dispatch action so the
skill layer can invoke the appropriate Agent tool, then resume via
`fno agents workspace register-worker`.

The territory feed half (x-e221) is the blueprinter's machinery side: the
daemon asks one verb per territory who is unfed, whether the standing
worker is alive, and (on --deliver) mails each triaged idea to that worker
as ``/fno:blueprint <id>``. The record store keyed by canonical scope is
the durable half of the worker handle; the registry answers liveness.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Optional
from urllib.parse import quote

from fno import paths

#: A failed delivery re-attempts after this long; a delivered idea still
#: un-ready re-delivers after the longer one, so a blueprint that died
#: mid-flight (or mail the worker never drained) self-heals.
RETRY_AFTER_SECS = 1800
REDO_AFTER_SECS = 86400
REPAIR_CAP = 50

_EXCLUDED_STATUS = frozenset(
    {"done", "blocked", "deferred", "superseded", "in_progress", "queued"}
)


def _feed_rungs() -> frozenset:
    from fno.graph.ladder import Rung

    return frozenset({Rung.IDEA, Rung.DESIGN})


def blueprint(plan_path: str) -> dict[str, Any]:
    """Return an llm_blueprint dispatch action.

    Args:
        plan_path: Path to the plan file or folder.

    Returns:
        {"action": "llm_blueprint", "plan_path": str, "next_step": str}
    """
    return {
        "action": "llm_blueprint",
        "plan_path": plan_path,
        "next_step": "re-enter after skill dispatch",
    }


def _now() -> datetime:
    return datetime.now(timezone.utc)


def _iso(dt: datetime) -> str:
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def _parse_iso(value: str) -> Optional[datetime]:
    try:
        return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=timezone.utc
        )
    except (ValueError, TypeError):
        return None


def worker_name_for_scope(scope: str) -> str:
    """A stable, registry-safe worker label for one territory scope."""
    stem = re.sub(r"[^a-z0-9]+", "-", scope.lower()).strip("-")[:24] or "scope"
    return f"blueprinter-{stem}-{hashlib.sha1(scope.encode()).hexdigest()[:6]}"


def record_path(scope: str) -> Path:
    """The durable scope-keyed record file (``<state>/blueprinters/``)."""
    return paths.state_dir() / "blueprinters" / (quote(scope, safe="") + ".json")


def _read_record(scope: str) -> dict[str, Any]:
    path = record_path(scope)
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(raw, dict):
            raw.setdefault("worker", None)
            raw.setdefault("fed", {})
            raw.setdefault("repairs", [])
            return raw
    except (OSError, ValueError):
        pass
    return {"worker": None, "fed": {}, "repairs": []}


def _write_record(scope: str, record: dict[str, Any]) -> None:
    path = record_path(scope)
    path.parent.mkdir(parents=True, exist_ok=True)
    record["updated_at"] = _iso(_now())
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(record, indent=2), encoding="utf-8")
    os.replace(tmp, path)


def _live_registry_names() -> set[str]:
    from fno.agents.spawn_gate import census

    return census().live_registry_names


def _territory(scope: str) -> Optional[dict]:
    from fno.active_backlog import _territories

    for territory in _territories():
        if territory["scope"] == scope:
            return territory
    return None


def _read_entries() -> list[dict]:
    from fno.graph.store import read_graph

    return read_graph(paths.graph_json())


def _is_terminal(row: dict) -> bool:
    return bool(row.get("completed_at")) or row.get("status") in ("done", "superseded")


def _feed_candidate(row: dict) -> bool:
    if not row.get("id") or row.get("completed_at"):
        return False
    if row.get("status") in _EXCLUDED_STATUS:
        return False
    from fno.graph.ladder import plan_rung

    return plan_rung(row) in _feed_rungs()


def _prune_fed(record: dict, entries: list[dict]) -> None:
    """Drop fed ledger rows for nodes that closed or vanished - the record
    never grows with shipped work."""
    by_id = {r.get("id"): r for r in entries if isinstance(r, dict)}
    record["fed"] = {
        node: stamp
        for node, stamp in record["fed"].items()
        if node in by_id and not _is_terminal(by_id[node])
    }


def _due(stamp: dict, now: datetime, ok_window: int, fail_window: int) -> bool:
    at = _parse_iso(stamp.get("at", ""))
    if at is None:
        return True
    window = ok_window if stamp.get("ok") else fail_window
    return now - at >= timedelta(seconds=window)


def _mail_deliver(worker_name: str, node_id: str) -> tuple[bool, str]:
    from fno._subprocess_util import fno_py_cmd

    cmd = [
        *fno_py_cmd(),
        "agents", "mail", "send", worker_name, f"/fno:blueprint {node_id}",
    ]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    except OSError as exc:
        return False, str(exc)
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()
        return False, detail[:200] or f"mail send exited {proc.returncode}"
    return True, ""


def _idea_receipt(row: dict) -> dict[str, Any]:
    from fno.graph.ladder import plan_rung

    return {"id": row["id"], "rung": plan_rung(row).value}


def blueprint_feed(
    scope: str,
    *,
    deliver: bool = False,
    repair: Optional[str] = None,
    now: Optional[datetime] = None,
) -> dict[str, Any]:
    """One territory's blueprinter-feed read or action.

    status: the unfed triaged ideas plus the standing worker's handle and
    liveness. deliver: mail each due idea to the worker and mark what sent.
    repair: record a refusal/repair reason; the ideas stay preserved.
    """
    now = now or _now()
    territory = _territory(scope)
    if territory is None:
        return {"action": "unknown", "scope": scope, "reason": "no such territory"}

    entries = _read_entries()
    from fno.king.scope import territory_membership

    tm = territory_membership(scope, entries)
    if tm.state != "ok":
        return {"action": "unknown", "scope": scope, "reason": tm.reason}
    key = tm.key or scope
    record = _read_record(key)

    fed_before = len(record["fed"])
    _prune_fed(record, entries)
    pruned = len(record["fed"]) != fed_before
    ideas = [
        _idea_receipt(row)
        for row in entries
        if row.get("id") in tm.ids
        and _feed_candidate(row)
        and _due(
            record["fed"].get(row["id"], {"at": "", "ok": False}),
            now,
            REDO_AFTER_SECS,
            RETRY_AFTER_SECS,
        )
    ]

    if repair is not None:
        record["repairs"] = (record["repairs"] + [{"ts": _iso(now), "reason": repair}])[
            -REPAIR_CAP:
        ]
        _write_record(key, record)
        return {
            "action": "repair",
            "scope": key,
            "rung": territory["rung"],
            "kingless": territory["kingless"],
            "recorded": repair,
            "ideas": len(ideas),
        }

    worker = record.get("worker") or None
    live = bool(worker and worker.get("name") in _live_registry_names())
    worker_view = (
        {"name": worker["name"], "spawned_at": worker.get("spawned_at"), "live": live}
        if worker
        else None
    )

    if deliver:
        if worker is None or not live:
            record["repairs"] = (
                record["repairs"]
                + [{"ts": _iso(now), "reason": f"worker_not_live: {worker}"}]
            )[-REPAIR_CAP:]
            _write_record(key, record)
            return {
                "action": "blocked",
                "scope": key,
                "rung": territory["rung"],
                "kingless": territory["kingless"],
                "reason": "worker_not_live",
                "ideas": len(ideas),
            }
        delivered: list[str] = []
        failed: list[dict[str, str]] = []
        for idea in ideas:
            ok, detail = _mail_deliver(worker["name"], idea["id"])
            record["fed"][idea["id"]] = {"at": _iso(now), "ok": ok}
            if ok:
                delivered.append(idea["id"])
            else:
                failed.append({"id": idea["id"], "reason": detail})
        _write_record(key, record)
        return {
            "action": "deliver",
            "scope": key,
            "rung": territory["rung"],
            "kingless": territory["kingless"],
            "worker": worker_view,
            "delivered": delivered,
            "failed": failed,
            "fed": len(record["fed"]),
        }

    if pruned:
        _write_record(key, record)
    return {
        "action": "status",
        "scope": key,
        "rung": territory["rung"],
        "kingless": territory["kingless"],
        "worker": worker_view,
        "worker_name_next": worker_name_for_scope(key),
        "ideas": ideas,
        "fed": len(record["fed"]),
    }
