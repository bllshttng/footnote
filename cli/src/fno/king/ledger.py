"""``fno agents king ledger`` - render the reign ledger page.

Contract: docs/architecture/reign.md. The crown-to-nodes join stays in the
native court-fold read (``fno.agents.court.fold_scope_nodes``); this module
renders that answer and never re-derives it. Membership detail (titles for
omitted nodes, uncrowned epics) comes from the SAME compiler the court and
the wake read, ``fno.king.scope.compile_scope_ids``, forced to each crown
row's own level.
"""
from __future__ import annotations

import html
import os
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

#: Lifecycle order for a fold's counts line; a status outside the vocabulary
#: keeps its place at the end rather than vanishing (same order court-fold
#: renders from).
_COUNT_ORDER = (
    "in_progress",
    "in_review",
    "ready",
    "blocked",
    "design",
    "idea",
    "deferred",
    "done",
    "superseded",
)

_CSS = """\
body{font-family:-apple-system,'Segoe UI',sans-serif;margin:24px auto;max-width:900px;color:#1a1a2e}
h1{font-size:20px;margin:0 0 4px}.meta{color:#6b7280;font-size:12px;margin:2px 0}
.note{color:#b91c1c;font-size:12.5px}
section.crown{border:1px solid #e5e7eb;border-radius:8px;padding:10px 14px;margin:14px 0}
section.crown h2{font-size:14px;margin:0 0 4px;font-family:ui-monospace,monospace}
table{width:100%;border-collapse:collapse;font-size:12px;font-family:ui-monospace,monospace}
th{text-align:left;color:#6b7280;font-weight:500;padding:3px 8px 3px 0;border-bottom:1px solid #e5e7eb}
td{padding:3px 8px 3px 0;border-bottom:1px solid #f3f4f6;word-break:break-all}
"""


def _esc(value: Any) -> str:
    return html.escape(str(value if value is not None else "-"))


def build_ledger_data(
    rows: Optional[list] = None,
    *,
    fold_fn: Optional[Callable] = None,
    entries: Optional[list[dict]] = None,
) -> dict[str, Any]:
    """gather_court plus the native scope fold and the graph read; the
    ledger's whole input."""
    from fno.agents.court import fold_scope_nodes, gather_court

    court = gather_court(rows)
    crowns = court.get("crowns")
    if crowns:
        (fold_fn or fold_scope_nodes)(crowns)
    if entries is None:
        from fno.agents.crown import _graph_index

        entries = list((_graph_index() or {}).values())
    court["entries"] = entries
    return court


def default_ledger_path() -> Path:
    """``<state_dir>/reign.html``, the sibling of graph.html."""
    try:
        from fno import paths as _paths

        return _paths.state_dir() / "reign.html"
    except Exception:
        return Path.home() / ".fno" / "reign.html"


def counts_line(fold: dict[str, Any]) -> str:
    counts = fold.get("counts") or {}
    ordered = [s for s in _COUNT_ORDER if s in counts]
    ordered += sorted(k for k in counts if k not in _COUNT_ORDER)
    return ", ".join(f"{s} {counts[s]}" for s in ordered)


def _scope_members(
    crown: dict[str, Any], entries: list[dict]
) -> Optional[set[str]]:
    """The crown's member ids at its own level; None when it cannot compile."""
    scope = crown.get("scope")
    level = crown.get("level")
    if not (isinstance(scope, str) and scope.strip()) or level is None:
        return None
    from fno.king.scope import compile_scope_ids

    try:
        return compile_scope_ids(scope, entries, level=level)
    except Exception:  # noqa: BLE001 - an uncompilable scope is the fold's verdict already
        return None


def _row_tr(n: dict[str, Any], titles: dict[str, dict]) -> str:
    entry = titles.get(str(n.get("id") or ""), {})
    return "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>".format(
        _esc(n.get("id")),
        _esc(entry.get("title") or "-"),
        _esc(n.get("status")),
        _esc(n.get("worker")) if n.get("worker") is not None else "",
        f"#{_esc(n['pr_number'])}" if n.get("pr_number") else "",
    )


_TABLE_HEAD = (
    "<table><thead><tr><th>node</th><th>title</th><th>status</th>"
    "<th>worker</th><th>pr</th></tr></thead><tbody>"
)


def _crown_section(
    crown: dict[str, Any],
    titles: dict[str, dict],
    entries: list[dict],
) -> str:
    level = crown.get("level")
    level_s = "L?" if level is None else f"L{level}"
    agree = crown.get("agree")
    agree_s = "?" if agree is None else ("yes" if agree else "no")
    parts = [
        "<section class=\"crown\">",
        f"<h2>{_esc(crown.get('scope'))} &middot; {level_s} &middot; {_esc(crown.get('holder'))}</h2>",
        "<p class=\"meta\">grantor {} &middot; status {} &middot; agree {}</p>".format(
            _esc(crown.get("grantor")), _esc(crown.get("status")), agree_s
        ),
    ]
    if crown.get("reason"):
        parts.append(f"<p class=\"meta\">{_esc(crown['reason'])}</p>")
    fold = crown.get("scope_nodes") or {}
    if fold.get("status") == "unresolved":
        parts.append(
            f"<p class=\"note\">scope fold: unresolved - {_esc(fold.get('reason'))}</p>"
        )
    else:
        parts.append(
            "<p class=\"counts\">{} nodes: {} ({} not listed)</p>".format(
                _esc(fold.get("total", 0)),
                _esc(counts_line(fold)),
                _esc(fold.get("omitted", 0)),
            )
        )
        listed = list(fold.get("nodes") or [])
        rows = [_row_tr(n, titles) for n in listed]
        members = _scope_members(crown, entries) if entries else None
        if members is not None:
            listed_ids = {str(n.get("id")) for n in listed}
            for omitted_id in sorted(members - listed_ids):
                entry = titles.get(omitted_id, {})
                rows.append(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td></td><td></td></tr>".format(
                        _esc(omitted_id),
                        _esc(entry.get("title") or "-"),
                        _esc(entry.get("status")),
                    )
                )
        parts.append(_TABLE_HEAD + "".join(rows) + "</tbody></table>")
    parts.append("</section>")
    return "".join(parts)


def _uncrowned_section(crowns: list[dict[str, Any]], entries: list[dict]) -> str:
    """Epics in no crown's territory: absent from every fold, so the page
    names them instead of letting their absence read as zero."""
    covered: set[str] = set()
    for crown in crowns:
        members = _scope_members(crown, entries)
        if members:
            covered |= members
    orphans = [
        entry
        for entry in entries
        if entry.get("type") == "epic"
        and str(entry.get("id") or "") not in covered
    ]
    if not orphans:
        return ""
    p1 = sum(1 for e in orphans if e.get("priority") == "p1")
    orphans.sort(
        key=lambda e: (str(e.get("priority") or "p2"), str(e.get("title") or ""))
    )
    rows = "".join(
        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>".format(
            _esc(e.get("id")),
            _esc(e.get("title") or "-"),
            _esc(e.get("status")),
            _esc(e.get("priority")),
        )
        for e in orphans
    )
    head = (
        f"<section class=\"crown\"><h2>uncrowned epics</h2>"
        f"<p class=\"meta\">{len(orphans)} uncrowned, {p1} at p1</p>"
        "<table><thead><tr><th>node</th><th>title</th><th>status</th>"
        f"<th>priority</th></tr></thead><tbody>{rows}</tbody></table></section>"
    )
    return head


def render_ledger_html(court: dict[str, Any]) -> str:
    summary = court.get("summary") or {}
    entries = court.get("entries") or []
    titles = {
        str(e.get("id")): e for e in entries if isinstance(e.get("id"), str)
    }
    head = (
        "<!doctype html><html><head><meta charset=\"utf-8\">"
        "<title>Reign Ledger</title>"
        f"<style>{_CSS}</style></head><body>"
        "<h1>Reign Ledger</h1>"
        f"<p class=\"meta\">generated {datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}</p>"
    )
    crowns = court.get("crowns")
    if crowns is None:
        head += (
            "<p class=\"note\">court: CANNOT READ - "
            f"{_esc(summary.get('reason'))}. This is not an empty court; "
            "nothing was checked.</p>"
        )
    elif not crowns:
        head += "<p class=\"meta\">no live crowns</p>"
    else:
        s = summary
        head += (
            "<p class=\"meta\">{} crown{} &middot; {} disagreement{} &middot; "
            "{} unknown{} &middot; {} split{}</p>".format(
                _esc(s.get("total", 0)), "s" if s.get("total") != 1 else "",
                _esc(s.get("disagreements", 0)), "s" if s.get("disagreements") == 1 else "",
                _esc(s.get("unknowns", 0)), "s" if s.get("unknowns") == 1 else "",
                _esc(s.get("splits", 0)), "s" if s.get("splits") == 1 else "",
            )
        )
        if s.get("manifest_only"):
            head += f"<p class=\"meta\">{_esc(s['manifest_only'])} manifest-only</p>"
    if crowns and summary.get("sweep_ran") is False:
        head += (
            "<p class=\"note\">orphan sweep did not run (stale or missing binary): "
            "zero manifest-only entries is an absence, not a finding</p>"
        )
    body = "".join(_crown_section(c, titles, entries) for c in crowns or [])
    if crowns and entries:
        body += _uncrowned_section(crowns, entries)
    return head + body + "</body></html>"


def write_ledger(court: dict[str, Any], path: Optional[Path] = None) -> Path:
    """Render and write atomically; returns the path written."""
    path = Path(path) if path is not None else default_ledger_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(render_ledger_html(court))
        os.replace(temp, str(path))
    except Exception:
        try:
            os.unlink(temp)
        except OSError:
            pass
        raise
    return path
