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
from fno.graph.statuses import derived_status

if TYPE_CHECKING:
    from fno.config import RenderTargetConfig

# Public-facing column set + labels. Active work is folded into Now and the
# internal Triage column is folded into Later; Done is relabeled "Shipped".
_PUBLIC_COLUMNS = (("Now", "Now"), ("Next", "Next"), ("Later", "Later"), ("Done", "Shipped"))
ALL_PROJECTS = "all"


def _scope_matches(entry: dict, scope: str, *, all_projects: bool = False) -> bool:
    return all_projects or _project_key(entry) == scope


def _target_scope(target: "RenderTargetConfig") -> tuple[str, bool]:
    """Resolve new ``scope`` without changing legacy ``project`` semantics."""
    if target.scope is not None:
        return target.scope, target.scope == ALL_PROJECTS
    if target.project is not None:
        return target.project, False
    return ALL_PROJECTS, True


def _public_entries(
    entries: list[dict], project: str, *, all_projects: bool = False
) -> list[dict]:
    return [
        e for e in entries
        if isinstance(e, dict)
        and e.get("public") is not False
        and _scope_matches(e, project, all_projects=all_projects)
    ]


def _columns(
    entries: list[dict], project: str, *, all_projects: bool = False
) -> dict[str, list[dict]]:
    cols: dict[str, list[dict]] = {col: [] for col, _ in _PUBLIC_COLUMNS}
    board_order, column_for = make_kanban_classifiers(entries)
    for e in _public_entries(entries, project, all_projects=all_projects):
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


def public_backlog_entries(
    entries: list[dict], project: str, *, all_projects: bool = False
) -> list[dict]:
    return [
        entry
        for entry in _public_entries(entries, project, all_projects=all_projects)
        if derived_status(entry) in PUBLIC_BACKLOG_STATUSES
    ]


def _backlog_sections_for(items: list[dict]) -> list[tuple[str, list[dict]]]:
    groups: dict[str, list[dict]] = {}
    status_order = {status: index for index, status in enumerate(PUBLIC_BACKLOG_STATUSES)}
    for entry in items:
        groups.setdefault(group_for(entry), []).append(entry)
    for sorted_items in groups.values():
        sorted_items.sort(
            key=lambda entry: (
                status_order.get(derived_status(entry), 99),
                str(entry.get("priority") or "p2"),
                str(entry.get("title") or "").lower(),
            )
        )
    return [(name, groups[name]) for name in sorted(groups)]


def _backlog_sections(entries: list[dict], project: str) -> list[tuple[str, list[dict]]]:
    return _backlog_sections_for(public_backlog_entries(entries, project))


def render_public_roadmap_html(
    entries: list[dict],
    project: str,
    cols: dict[str, list[dict]] | None = None,
    *,
    all_projects: bool = False,
) -> str:
    from fno.graph.render_html import render_public_sections_html
    if cols is None:
        cols = _columns(entries, project, all_projects=all_projects)
    sections = [(label, cols[column]) for column, label in _PUBLIC_COLUMNS]
    return render_public_sections_html(
        sections, title=f"{project} roadmap", projection="roadmap"
    )


def render_public_backlog_html(
    entries: list[dict],
    project: str,
    backlog_entries: list[dict] | None = None,
    *,
    all_projects: bool = False,
) -> str:
    from fno.graph.render_html import render_public_sections_html

    if backlog_entries is None:
        backlog_entries = public_backlog_entries(
            entries, project, all_projects=all_projects
        )
    return render_public_sections_html(
        _backlog_sections_for(backlog_entries),
        title=f"{project} backlog",
        projection="backlog",
    )


def _state_file_collisions(path: Path) -> list[str]:
    """Graph state files ``path`` resolves onto (empty list = no clash).

    Checked HERE, in the graph layer, not in the pydantic validator: the
    constants resolve through load_settings(), and resolving them from
    inside settings validation re-enters the loader recursively. Post-load
    there is no cycle.
    """
    try:
        from fno.graph import _constants as gc

        resolved = path.resolve()
        hits = []
        for state_path in (
            gc.GRAPH_JSON,
            gc.GRAPH_MD,
            # GRAPH_HTML is deliberately absent: it is a render target now,
            # not a state file, and an operator row for it must win over the
            # default row rather than be refused and then overwritten anyway.
            gc.GRAPH_ARCHIVE_JSON,
            gc.LEDGER_JSON,
            # the sha256 sidecar, the corruption-recovery backup, and the
            # flock whose inode an os.replace would swap out from under the
            # mutation mutex
            Path(str(gc.GRAPH_JSON) + ".sha256"),
            Path(str(gc.GRAPH_JSON) + ".bak"),
            Path(str(gc.GRAPH_JSON) + ".lock"),
        ):
            if resolved == Path(state_path).resolve():
                hits.append(str(state_path))
        return hits
    except Exception:
        return []


def GRAPH_HTML_PATH() -> Path:
    from fno.graph._constants import GRAPH_HTML

    return Path(GRAPH_HTML)


def render_one_target(target: "RenderTargetConfig", entries: list[dict]) -> None:
    """Render exactly one configured target. Never raises."""
    render_configured_targets(entries, _only=target)


def _default_targets() -> "list[RenderTargetConfig]":
    """The canonical local board, as an ordinary render-target row.

    Named once and returned from both the success path and the degraded path.
    store.py stopped rendering GRAPH_HTML unconditionally when the board became
    a configurable row, so a config-read failure that returned no rows at all
    would freeze the operator's board for as long as the config stayed broken.
    """
    from fno.config import RenderTargetConfig
    from fno.graph._constants import GRAPH_HTML

    return [
        RenderTargetConfig(path=str(GRAPH_HTML), scope=ALL_PROJECTS, projection="local")
    ]


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
        block = read_global_block("backlog", unreadable=unreadable)
        rows = None if block is None else block.get("render_targets")
        if rows is None:
            rows = []
        elif not isinstance(rows, list):
            # Same text the BacklogBlock coercion logs at settings load.
            print(
                "Warning: " + RENDER_TARGETS_TABLE_TYPO_MSG % type(rows).__name__,
                file=sys.stderr,
            )
            rows = []
        if unreadable and rows == [] and block is not None:
            # A global config that exists but fails to parse must not silently
            # disable configured targets behind the generic parse warning
            # config_io already logged. Fires only where a disability is
            # possible: a readable [backlog] block exists, no readable file
            # defines render_targets, and some candidate is unreadable.
            print(
                "Warning: backlog.render_targets may be disabled: global "
                f"config unreadable: {', '.join(str(p) for p in unreadable)}",
                file=sys.stderr,
            )
        # Per-row validation, not one atomic model_validate: a single bad row
        # (e.g. a relative path) must not stop every OTHER target rendering.
        out: list[RenderTargetConfig] = []
        seen_paths: set[Path] = set()
        for row in rows:
            # An unknown key is refused HERE, not in the settings validator: a
            # raise there bricks every fno command over one typo. `scope`
            # defaults to `all`, so a misspelled scope key that is merely
            # ignored publishes every project on a page meant to name one.
            # Skipping the row costs that one board.
            unknown = (
                [key for key in row if key not in RenderTargetConfig.model_fields]
                if isinstance(row, dict)
                else []
            )
            if unknown:
                print(
                    "Warning: skipping backlog.render_targets row "
                    f"{row.get('path')!r}: unknown key(s) "
                    f"{', '.join(repr(k) for k in unknown)}; a misspelled scope "
                    "would otherwise publish every project",
                    file=sys.stderr,
                )
                continue
            try:
                target = RenderTargetConfig.model_validate(row)
            except Exception as exc:
                print(
                    f"Warning: skipping malformed backlog.render_targets row: {exc}",
                    file=sys.stderr,
                )
                continue
            clashes = _state_file_collisions(Path(os.path.expanduser(target.path)))
            if clashes:
                print(
                    f"Warning: skipping render target {target.path}: collides "
                    f"with graph state file {', '.join(clashes)}; refusing to "
                    "overwrite it",
                    file=sys.stderr,
                )
                continue
            resolved = Path(os.path.expanduser(target.path)).resolve()
            if resolved in seen_paths:
                print(
                    f"Warning: skipping duplicate render target path {target.path}: "
                    "duplicate render target path; first target kept",
                    file=sys.stderr,
                )
                continue
            seen_paths.add(resolved)
            out.append(target)
        _warn_shadowed_local_rows(out)
        from fno.graph._constants import GRAPH_HTML

        if not any(
            Path(os.path.expanduser(target.path)).resolve() == Path(GRAPH_HTML).resolve()
            for target in out
        ):
            # The legacy global board is now an ordinary local/all target. Keep
            # it as the default row for installs that have no explicit replacement.
            out[0:0] = _default_targets()
        return out
    except Exception as exc:
        # Every other degradation in this module warns; a silent [] here would
        # read as "no targets configured" while the board rots.
        print(
            f"Warning: backlog.render_targets read failed: "
            f"{type(exc).__name__}: {exc}",
            file=sys.stderr,
        )
        return _default_targets()


# The shadow warning repeats on every mutation while misconfigured; dedupe
# identical states within one process so a long-lived daemon says it once.
_SHADOW_WARN_STATE: tuple[list[tuple[str, str, str]], list[tuple[str, str, str]]] | None = None


def _warn_shadowed_local_rows(honored: "list[RenderTargetConfig]") -> None:
    """Warn when load_settings() sees render_targets rows this render ignores.

    This key is honored from the GLOBAL config file only (graph.json is a
    global artifact; a project-local list would make the render cwd-dependent).
    load_settings still parses a project-local list, so an operator who puts
    the rows there gets validation and no rendering - a silent no-op unless
    this warning fires. Best-effort: a settings chain that cannot load at all
    stays silent rather than raising into the mutation.
    """
    global _SHADOW_WARN_STATE
    try:
        from fno.config import load_settings

        def _key(rows: "list[RenderTargetConfig]") -> list[tuple[str, str, str]]:
            return sorted(
                (r.path, r.scope or r.project or ALL_PROJECTS, r.projection)
                for r in rows
            )

        local = _key(load_settings().backlog.render_targets)
        state = (local, _key(honored))
        if local and local != state[1] and state != _SHADOW_WARN_STATE:
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
            _SHADOW_WARN_STATE = state
    except Exception:
        pass


def canonical_target() -> "RenderTargetConfig | None":
    """The configured row for the canonical board, or the default row.

    Split out so ``locked_mutate_graph`` can write this one INSIDE the graph
    flock, the way graph.md always was. It is a state-dir path this repo owns,
    so it carries none of the stall risk that keeps operator-chosen paths
    outside the lock. Without this the canonical board is the only artifact
    written after the lock drops, and two concurrent mutations can land their
    renders out of order, leaving the operator's board older than the
    graph.json beside it. A stale board is the complaint this work answers.
    """
    from fno.graph._constants import GRAPH_HTML

    resolved = Path(GRAPH_HTML).resolve()
    for target in _configured_targets():
        if Path(os.path.expanduser(target.path)).resolve() == resolved:
            return target
    return None


def render_configured_targets(
    entries: list[dict],
    *,
    skip_canonical: bool = False,
    _only: "RenderTargetConfig | None" = None,
) -> None:
    """Render every configured backlog projection (x-9415). Called from
    ``locked_mutate_graph`` AFTER graph.json is written, so it must never
    raise: a failing operator target warns and is skipped, never wedging the
    mutation. The leak gate stays fail-closed - a refusal leaves the target
    byte-unchanged and names every offender; it still only skips the target.

    ``skip_canonical`` omits the canonical board, which the caller has already
    written under the flock via ``render_one_target``.
    """
    from fno.graph.render_html import (
        atomic_write_documents,
        leak_offender_lines,
        public_title_leaks,
    )

    from fno.graph.render_html import render_graph_html

    canonical = GRAPH_HTML_PATH().resolve() if skip_canonical else None
    for target in ([_only] if _only is not None else _configured_targets()):
        out = Path(os.path.expanduser(target.path))
        if canonical is not None and out.resolve() == canonical:
            continue
        scope, all_projects = _target_scope(target)
        scoped_entries = [
            e for e in entries if _scope_matches(e, scope, all_projects=all_projects)
        ]
        if not all_projects and not scoped_entries:
            # Zero matching entries is the typo'd-project signature: leave the
            # operator's last good board byte-unchanged rather than replace it
            # with an empty projection. A project whose entries exist but are
            # all done/private still renders its valid empty projection below.
            print(
                f"Warning: render target {out} matches no graph entry with "
                f"project {scope!r}; target left unchanged "
                "(check the project name)",
                file=sys.stderr,
            )
            continue
        try:
            if target.projection == "local":
                # Local is the explicit private projection: it keeps ids,
                # details, plan paths, and Obsidian links and never enters the
                # public title gate.
                render_graph_html(
                    entries,
                    out,
                    project=scope,
                    all_projects=all_projects,
                )
                continue
            if target.projection == "roadmap":
                # One _columns pass feeds both the gate's render set and the
                # renderer (the manual verb derives it internally).
                cols = _columns(entries, scope, all_projects=all_projects)
                render_set = [e for items in cols.values() for e in items]
                html = render_public_roadmap_html(
                    entries, scope, cols=cols, all_projects=all_projects
                )
            else:
                render_set = public_backlog_entries(
                    entries, scope, all_projects=all_projects
                )
                html = render_public_backlog_html(
                    entries,
                    scope,
                    backlog_entries=render_set,
                    all_projects=all_projects,
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
