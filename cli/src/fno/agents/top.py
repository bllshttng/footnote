"""``fno agents top`` (x-c5cc): every live worker process with RSS.

One table over the SAME union the spawn gate counts (imported from
``spawn_gate.census`` — never duplicated), so the debugging surface and the
enforcement surface can never disagree. Python-only by design (LD8): RSS via
psutil, no daemon involvement, kept out of the Rust client verb list.
"""
from __future__ import annotations

import json
from typing import Optional

from fno.agents.spawn_gate import LiveWorker, census


def _rss_mb(pid: Optional[int]) -> Optional[int]:
    if not pid:
        return None
    try:
        import psutil

        return int(psutil.Process(pid).memory_info().rss / (1024 * 1024))
    except Exception:
        return None


def _crown_map() -> dict[str, str]:
    """name -> crown_label for crowned registry rows (US9). Best-effort: a read
    failure degrades to no crowns rather than breaking the process view."""
    try:
        from fno.agents.registry import load_registry

        return {e.name: e.crown_label for e in load_registry() if e.crown_label}
    except Exception:  # noqa: BLE001 — top is a debug view, never fail on it
        return {}


def _rows(workers: list[LiveWorker], crowns: dict[str, str]) -> list[dict]:
    rows = []
    for w in workers:
        rows.append(
            {
                "source": w.source,
                "name": w.name,
                "provider": w.provider,
                "substrate": w.substrate,
                "pid": w.pid,
                "rss_mb": _rss_mb(w.pid),
                "status": w.status,
                "crown": crowns.get(w.name),  # US9: null when uncrowned
            }
        )
    # Heaviest first: the row the operator is looking for when RAM is tight.
    rows.sort(key=lambda r: -float(r["rss_mb"] or 0))
    return rows


def render_top(as_json: bool = False) -> str:
    """Render the union table (or its JSON mirror — same rows, LD: parity)."""
    c = census()
    rows = _rows(c.workers, _crown_map())
    if as_json:
        return json.dumps(
            {"workers": rows, "slot_claims": c.slot_claims, "warnings": c.warnings},
            indent=2,
        )

    out: list[str] = []
    out.extend(c.warnings)
    header = f"{'SOURCE':<7} {'NAME':<24} {'PROVIDER':<9} {'SUBSTRATE':<10} {'PID':>7} {'RSS_MB':>7} STATUS"
    out.append(header)
    if not rows:
        out.append("no live workers")
    for r in rows:
        # US9: mark a crowned worker in the name cell (ASCII, alignment-safe).
        name_cell = r["name"] + (f" [{r['crown']}]" if r["crown"] else "")
        out.append(
            f"{r['source']:<7} {name_cell:<24} {r['provider']:<9} "
            f"{r['substrate']:<10} {r['pid'] or '-':>7} "
            f"{r['rss_mb'] if r['rss_mb'] is not None else '-':>7} {r['status']}"
        )
    if c.slot_claims:
        out.append(f"(+{c.slot_claims} queued headless slot claim(s))")
    return "\n".join(out)
