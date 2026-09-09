"""The local board's court section.

The section's data is the agents runtime's own (registry, claims, crown
verdicts), so the graph renderer never imports this module's world: the
contract between the two layers is the fragment file, and the board reads
it at render time. `update_board` refreshes that fragment from one court
read, then splices it into graph.html between the render's markers.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

_SECTION_BEGIN = "<!-- court:begin -->"
_SECTION_END = "<!-- court:end -->"


def _section_html(crowns: list[dict]) -> str:
    """The section markup from the native fold (the same read the CLI's
    ``--nodes`` serves). A stale or missing binary returns an empty section:
    the board renders without one rather than wedging the update."""
    from fno.paths import graph_json
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        return ""
    payload = [
        {k: c.get(k) for k in ("scope", "level", "holder", "agree", "reason")}
        for c in crowns
    ]
    try:
        proc = subprocess.run(
            [str(binary), "court-fold", "--graph", str(graph_json()),
             "--crowns-json", json.dumps(payload), "--format", "html-section"],
            capture_output=True, text=True, check=False, timeout=30,
        )
        if proc.returncode != 0:
            return ""
        return json.loads(proc.stdout)["section"]
    except (OSError, ValueError, subprocess.SubprocessError, KeyError):
        return ""


def splice_board(graph_html: Path, section: str) -> bool:
    """Replace the board's court markers' content with ``section``. A board
    without markers (older render, public snapshot) is left untouched:
    splicing into a document that never asked for the section would publish
    holder names."""
    try:
        text = graph_html.read_text(encoding="utf-8")
    except OSError:
        return False
    begin, end = text.find(_SECTION_BEGIN), text.find(_SECTION_END)
    if begin == -1 or end == -1 or end < begin:
        return False
    updated = text[: begin + len(_SECTION_BEGIN)] + section + text[end:]
    tmp = graph_html.with_name(graph_html.name + ".court-tmp")
    try:
        tmp.write_text(updated, encoding="utf-8")
        os.replace(tmp, graph_html)
    except OSError:
        tmp.unlink(missing_ok=True)
        return False
    return True


def update_board() -> None:
    """`fno agents court --update-board`: one court read, fresh fragment,
    spliced into the local board. Prints what happened; never raises."""
    from fno.agents.court import gather_court
    from fno.graph._constants import COURT_SECTION_HTML, GRAPH_HTML

    court = gather_court()
    crowns = court.get("crowns") or []
    section = _section_html(crowns) if crowns else ""
    try:
        COURT_SECTION_HTML.write_text(section, encoding="utf-8")
        spliced = splice_board(GRAPH_HTML, section) if GRAPH_HTML.is_file() else False
    except OSError as exc:
        print(f"court: board update failed: {exc}")
        return
    print(
        f"court: section {len(section)} bytes, "
        + ("spliced into the board" if spliced else "fragment written; board not spliced")
    )
