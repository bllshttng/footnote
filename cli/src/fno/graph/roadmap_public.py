"""Public projection selection and Markdown compatibility.

HTML authoring belongs exclusively to :mod:`fno.graph.render_html`.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import TYPE_CHECKING

from fno.graph.render import (
    _project_key,
    make_kanban_classifiers,
)
from fno.graph.render_html import PUBLIC_BACKLOG_STATUSES, group_for

if TYPE_CHECKING:
    from fno.config import RenderTargetConfig

# Public-facing column set + labels. Active work is folded into Now and the
# internal Triage column is folded into Later; Done is relabeled "Shipped".
_PUBLIC_COLUMNS = (("Now", "Now"), ("Next", "Next"), ("Later", "Later"), ("Done", "Shipped"))


def _public_entries(entries: list[dict], project: str) -> list[dict]:
    return [
        e for e in entries
        if isinstance(e, dict)
        and e.get("public") is not False
        and _project_key(e) == project
    ]


def _columns(entries: list[dict], project: str) -> dict[str, list[dict]]:
    cols: dict[str, list[dict]] = {col: [] for col, _ in _PUBLIC_COLUMNS}
    board_order, column_for = make_kanban_classifiers(entries)
    for e in _public_entries(entries, project):
        col = column_for(e)
        if col == "In Progress":
            col = "Now"
        elif col == "Triage":  # fold the internal triage pile into Later
            col = "Later"
        if col in cols:
            cols[col].append(e)
    for items in cols.values():
        items.sort(key=board_order)
    return cols


def _card_bits(entry: dict) -> tuple[str, str]:
    """Return (title, meta) with only public-safe fields."""
    title = (entry.get("title") or "(untitled)").replace("\n", " ").strip()
    bits = []
    pr = entry.get("priority")
    if pr:
        bits.append(pr)
    size = entry.get("size")
    if size:
        bits.append(str(size))
    return title, " · ".join(bits)


def render_public_roadmap_md(entries: list[dict], project: str) -> str:
    cols = _columns(entries, project)
    out = [f"# {project} roadmap", ""]
    for col, label in _PUBLIC_COLUMNS:
        items = cols[col]
        if not items:
            continue
        out.append(f"## {label}")
        out.append("")
        for e in items:
            title, meta = _card_bits(e)
            out.append(f"- {title}" + (f" _({meta})_" if meta else ""))
        out.append("")
    return "\n".join(out).rstrip() + "\n"


def public_projection_entries(entries: list[dict], project: str) -> list[dict]:
    """The union whose titles must clear the public leak gate."""
    roadmap = [entry for items in _columns(entries, project).values() for entry in items]
    backlog = public_backlog_entries(entries, project)
    by_id: dict[str, dict] = {}
    for entry in [*roadmap, *backlog]:
        key = str(entry.get("id") or id(entry))
        by_id.setdefault(key, entry)
    return list(by_id.values())


def public_backlog_entries(entries: list[dict], project: str) -> list[dict]:
    return [
        entry
        for entry in _public_entries(entries, project)
        if entry.get("status") in PUBLIC_BACKLOG_STATUSES
    ]


def _backlog_sections(entries: list[dict], project: str) -> list[tuple[str, list[dict]]]:
    groups: dict[str, list[dict]] = {}
    status_order = {status: index for index, status in enumerate(PUBLIC_BACKLOG_STATUSES)}
    for entry in public_backlog_entries(entries, project):
        groups.setdefault(group_for(entry), []).append(entry)
    for items in groups.values():
        items.sort(
            key=lambda entry: (
                status_order.get(str(entry.get("status")), 99),
                str(entry.get("priority") or "p2"),
                str(entry.get("title") or "").lower(),
            )
        )
    return [(name, groups[name]) for name in sorted(groups)]


def render_public_roadmap_html(
    entries: list[dict], project: str, cols: dict[str, list[dict]] | None = None
) -> str:
    from fno.graph.render_html import render_public_sections_html
    if cols is None:
        cols = _columns(entries, project)
    sections = [(label, cols[column]) for column, label in _PUBLIC_COLUMNS]
    return render_public_sections_html(
        sections, title=f"{project} roadmap", projection="roadmap"
    )


def render_public_backlog_html(entries: list[dict], project: str) -> str:
    from fno.graph.render_html import render_public_sections_html

    return render_public_sections_html(
        _backlog_sections(entries, project),
        title=f"{project} backlog",
        projection="backlog",
    )


def _configured_targets() -> "list[RenderTargetConfig]":
    """Read ``config.backlog.render_targets`` from the GLOBAL config file.

    Goes through ``read_global_block`` (config.toml-first candidates) for the
    same recorded reason as ``render_html._load_obsidian_vault``: this runs
    right after ``locked_mutate_graph`` commits and ``load_settings()`` stops
    at a project-local file that would shadow the operator's global list.
    Every failure degrades to ``[]`` with a warning instead of raising into
    the mutation.
    """
    try:
        # Function-local: keep graph-module imports free of config's pydantic.
        from fno.config import RENDER_TARGETS_TABLE_TYPO_MSG, RenderTargetConfig
        from fno.config_io import read_global_block

        unreadable: list = []
        backlog = read_global_block("backlog", unreadable=unreadable) or {}
        rows = backlog.get("render_targets")
        if rows is None:
            rows = []
        elif not isinstance(rows, list):
            # Same text the BacklogBlock coercion logs at settings load.
            print(
                "Warning: " + RENDER_TARGETS_TABLE_TYPO_MSG % type(rows).__name__,
                file=sys.stderr,
            )
            rows = []
        if unreadable and not rows:
            # A corrupt global config must not silently disable every target
            # behind the generic parse warning config_io already logged. Only
            # a disability (no usable rows from any readable candidate)
            # warns - a corrupt legacy settings.yaml under a config.toml that
            # fully defines the rows is not the render's problem.
            print(
                "Warning: backlog.render_targets may be disabled: global "
                f"config unreadable: {', '.join(str(p) for p in unreadable)}",
                file=sys.stderr,
            )
        # Per-row validation, not one atomic model_validate: a single bad row
        # (e.g. a relative path) must not stop every OTHER target rendering.
        out: list[RenderTargetConfig] = []
        for row in rows:
            try:
                out.append(RenderTargetConfig.model_validate(row))
            except Exception as exc:
                print(
                    f"Warning: skipping malformed backlog.render_targets row: {exc}",
                    file=sys.stderr,
                )
        _warn_shadowed_local_rows(out)
        return out
    except Exception as exc:
        # Every other degradation in this module warns; a silent [] here would
        # read as "no targets configured" while the board rots.
        print(
            f"Warning: backlog.render_targets read failed: "
            f"{type(exc).__name__}: {exc}",
            file=sys.stderr,
        )
        return []


def _warn_shadowed_local_rows(honored: "list[RenderTargetConfig]") -> None:
    """Warn when load_settings() sees render_targets rows this render ignores.

    This key is honored from the GLOBAL config file only (graph.json is a
    global artifact; a project-local list would make the render cwd-dependent).
    load_settings still parses a project-local list, so an operator who puts
    the rows there gets validation and no rendering - a silent no-op unless
    this warning fires. Best-effort: a settings chain that cannot load at all
    stays silent rather than raising into the mutation.
    """
    try:
        from fno.config import load_settings

        def _key(rows: "list[RenderTargetConfig]") -> list[tuple[str, str, str]]:
            return sorted((r.path, r.project, r.projection) for r in rows)

        local = _key(load_settings().backlog.render_targets)
        if local and local != _key(honored):
            print(
                "Warning: backlog.render_targets is honored from the GLOBAL "
                "config file only; "
                + (
                    f"{len(local)} project-local row(s) ignored"
                    if not honored
                    else "a project-local list shadows the global one and is ignored"
                ),
                file=sys.stderr,
            )
    except Exception:
        pass


def render_configured_targets(entries: list[dict]) -> None:
    """Render every configured public projection (x-9415). Called from
    ``locked_mutate_graph`` AFTER graph.json is written, so it must never
    raise: a failing operator target warns and is skipped, never wedging the
    mutation. The leak gate stays fail-closed - a refusal leaves the target
    byte-unchanged and names every offender; it still only skips the target."""
    from fno.graph.render_html import (
        atomic_write_documents,
        leak_offender_lines,
        public_title_leaks,
    )

    for target in _configured_targets():
        out = Path(os.path.expanduser(target.path))
        try:
            if target.projection == "roadmap":
                # One _columns pass feeds both the gate's render set and the
                # renderer (the manual verb derives it internally).
                cols = _columns(entries, target.project)
                render_set = [e for items in cols.values() for e in items]
                html = render_public_roadmap_html(entries, target.project, cols=cols)
            else:
                render_set = public_backlog_entries(entries, target.project)
                html = render_public_backlog_html(entries, target.project)
            if not any(_project_key(e) == target.project for e in entries):
                # Almost certainly a typo'd project: the empty projection is
                # still written (a drained project must not keep a stale
                # board), but the operator gets a loud signal each mutation.
                print(
                    f"Warning: render target {out} matches no graph entry with "
                    f"project {target.project!r}; wrote an empty projection "
                    "(check the project name)",
                    file=sys.stderr,
                )
            offenders = public_title_leaks(render_set)
            if offenders:
                # Fail closed, before any write: the public file stays
                # byte-unchanged while the already-written graph.json and the
                # remaining targets are untouched by this refusal.
                print(
                    f"Warning: public title leak gate refused {out}: "
                    f"{len(offenders)} offending title(s); target left unchanged",
                    file=sys.stderr,
                )
                for line in leak_offender_lines(offenders):
                    print(line, file=sys.stderr)
                continue
            atomic_write_documents({out: html})
        except Exception as exc:
            # Wide on purpose, unlike store.py's OSError-only render handlers:
            # graph.json is already committed and a bad operator target must
            # never fail `fno backlog update`. The type name keeps bugs visible.
            print(
                f"Warning: render target {out} failed: {type(exc).__name__}: {exc}",
                file=sys.stderr,
            )
