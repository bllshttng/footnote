"""Public projection selection, Markdown compatibility, and the local board
render handoff.

The local board page is written by the native front binary (`fno
board-render`): the served web backlog page with the rows embedded, so the
file opens from disk with no server and can never drift from the served
board. This module resolves the configured targets and hands them over; it
no longer authors any HTML itself.
"""
from __future__ import annotations

import os
import re
import sys
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING

from fno.graph.render import (
    _project_key,
    make_kanban_classifiers,
)

# The statuses a PUBLIC backlog page shows. The local board shows every row.
PUBLIC_BACKLOG_STATUSES = ("in_progress", "ready", "blocked", "idea")

GROUPS = (
    ("agents / spawn / dispatch", r"spawn|dispatch|agent|worker|roster|registry|retask|handoff|successor"),
    ("review & attestation", r"review|attest|coverage|verdict|finding|sigma|peer"),
    ("PR / merge / CI", r"\bpr\b|merge|\bci\b|check|smoke|pytest|mypy|lint|guard|workflow"),
    ("identity / session / claims", r"session|identity|claim|short.?id|uuid|lock|liveness|crown|king"),
    ("backlog / graph / board", r"backlog|graph|node|kanban|board|rank|triage|carveout|groom"),
    ("mux / panes / tui", r"\bmux\b|pane|tmux|tui|squad|keymap|menu"),
    ("config / paths / install", r"config|path|install|deploy|doctor|update|version|schema"),
    ("mail & messaging", r"mail|envelope|inbox|message|relay|notify|digest"),
    ("providers / models / routing", r"provider|model|route|harness|codex|claude|gemini|zai|glm|account|quota"),
    ("plans / target / loop", r"plan|target|loop|wave|blueprint|execute|phase|stop.?hook|compact"),
    ("worktree / git", r"worktree|git\b|branch|rebase|checkout"),
    ("observability / cost", r"metric|cost|budget|telemetry|event|observab|watchdog|monitor"),
    ("docs / skills / prose", r"doc\b|docs|skill|readme|prose|style"),
)


def group_for(entry: dict) -> str:
    haystack = f"{entry.get('title', '')} {entry.get('slug', '')}".lower()
    for name, pattern in GROUPS:
        if re.search(pattern, haystack):
            return name
    return "uncategorized"


def load_render_entries(entries: list[dict] | None = None) -> list[dict]:
    """Overlay archive on a guarded display read or the canonical graph seam."""
    from fno.graph.store import entries_with_archive, read_graph_with_archive

    return read_graph_with_archive() if entries is None else entries_with_archive(entries)


# The one leak vocabulary. The gate below scans titles with it; a test scans
# a whole rendered public document with the same list, so the probe and the
# gate can never drift into disagreeing about what counts as a leak.
LEAK_PATTERNS: tuple[tuple[str, "re.Pattern[str]"], ...] = (
    ("pr-reference", re.compile(r"(?i)(?:\bPR(?:\s*#?\s*|-)\d+\b|#\d+\b)")),
    # Generic compact prefixes can resemble CSS hex colors; legacy compact x ids cannot.
    ("node-id", re.compile(r"\b(?:[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}|x[0-9a-f]{4,8})\b", re.I)),
    ("home-path", re.compile(r"(?:~/(?:[^\s]+)|/(?:Users|home)/[^\s/]+(?:/[^\s]+)?)")),
    (
        "session-id",
        re.compile(
            r"\b(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|ses-[A-Za-z0-9_-]+)\b",
            re.I,
        ),
    ),
)


def public_title_leaks(entries: list[dict]) -> list[tuple[str, str, tuple[str, ...]]]:
    """Return every public-title offender and every matched leak class."""
    offenders: list[tuple[str, str, tuple[str, ...]]] = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        title = str(entry.get("title") or "").replace("\n", " ").strip()
        classes = tuple(name for name, pattern in LEAK_PATTERNS if pattern.search(title))
        if classes:
            offenders.append((str(entry.get("id") or "?"), title, classes))
    return offenders


def leak_offender_lines(offenders: list[tuple[str, str, tuple[str, ...]]]) -> list[str]:
    """One refusal line per offender, shared by the manual roadmap verb and
    the auto-render so the two leak-gate reports cannot drift apart."""
    return [
        f"  {node_id}: {','.join(classes)}: {title}"
        for node_id, title, classes in offenders
    ]


def leak_refusal_report(subject: str, offenders: list[tuple[str, str, tuple[str, ...]]]) -> None:
    """The audible refusal, shared by the manual roadmap verb and
    the auto-render so the two leak-gate reports cannot drift apart: one stderr
    line per offender, then a best-effort OS alert. A bare exit under
    launchd is invisible and the live page can sit stale with no reader, so
    the alert rides the same `fno inbox notify` lane the push script fires.
    An alert failure never masks the refusal."""
    print(f"Error: public title leak gate refused {subject}:", file=sys.stderr)
    for line in leak_offender_lines(offenders):
        print(line, file=sys.stderr)
    try:
        from fno.notify._impl import send_notification

        detail = "; ".join(f"{i} {'+'.join(c)}" for i, _, c in offenders[:3])
        code, err = send_notification(
            "roadmap render refused", f"{subject}: leak gate refused ({detail})"
        )
        if err:
            print(f"warning: render alert degraded ({code}): {err}", file=sys.stderr)
    except Exception:  # noqa: BLE001 - an alert must never mask the refusal
        pass


def atomic_write_documents(documents: dict[Path, str]) -> None:
    """Stage every document before replacing any destination."""
    staged: list[tuple[Path, str]] = []
    try:
        for path, content in documents.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            fd, temp = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(content)
            staged.append((path, temp))
        for path, temp in staged:
            os.replace(temp, path)
    except Exception:
        for _path, temp in staged:
            try:
                os.unlink(temp)
            except OSError:
                pass
        raise
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


def _load_obsidian_vault() -> str | None:
    """Read ``config.obsidian.vault`` from the GLOBAL config file directly.

    Walks the config.toml-first global candidates via ``read_global_block``.
    Deliberately bypasses ``load_settings()`` because that loader walks
    project-local-first and stops at the first match: a backlog mutation
    fired from a project whose own ``.fno/settings.yaml`` lacks an obsidian
    block would otherwise render the global board with vault=None, zeroing
    out every Obsidian deep link.
    """
    try:
        from fno.config_io import read_global_block

        obs = read_global_block("obsidian") or {}
        if not obs.get("enabled"):
            return None
        vault = obs.get("vault")
        return str(vault) if vault else None
    except Exception:
        return None


_VAULT_TOPLEVEL_DIRS = ("internal/",)


def canonicalize_plan_path(plan_path: str | None, vault: str | None = None) -> str | None:
    """Normalize a plan_path to a vault-relative form.

    Tolerates the shapes that have shown up in graph.json: canonical
    (``internal/...``), vault-prefixed (the vault name is stripped when
    supplied), and worktree-rooted (the LAST ``/internal/`` occurrence).
    Returns the canonical form or None when the path has no recognizable
    vault-relative segment.
    """
    if not plan_path:
        return None
    p = plan_path.strip()
    if not p:
        return None
    if p.startswith(_VAULT_TOPLEVEL_DIRS):
        return p
    if vault:
        needle = f"/{vault}/"
        idx = p.rfind(needle)
        if idx != -1:
            stripped = p[idx + len(needle):]
            if stripped.startswith(_VAULT_TOPLEVEL_DIRS):
                return stripped
    best_idx = -1
    for marker in _VAULT_TOPLEVEL_DIRS:
        idx = p.rfind(f"/{marker}")
        if idx > best_idx:
            best_idx = idx
    if best_idx != -1:
        return p[best_idx + 1:]
    return None


def obsidian_url(vault: str, plan_path: str) -> str | None:
    """Build an ``obsidian://open?vault=...&file=...`` deep link.

    Returns None when the plan_path has no recognizable vault-relative
    segment, or does not point at a markdown file.
    """
    import urllib.parse

    canonical = canonicalize_plan_path(plan_path, vault=vault)
    if canonical is None:
        return None
    p = canonical.rstrip("/")
    if not p.endswith(".md"):
        return None
    target = p[:-3]
    return (
        f"obsidian://open?vault={urllib.parse.quote(vault, safe='')}"
        f"&file={urllib.parse.quote(target, safe='/')}"
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
            # the corruption-recovery backup (the pre-relocation sibling and
            # the backups/ copy current keepers write), and the
            # flock whose inode an os.replace would swap out from under the
            # mutation mutex
            Path(str(gc.GRAPH_JSON) + ".bak"),
            Path(str(gc.GRAPH_JSON) + ".lock"),
            Path(gc.GRAPH_JSON.parent / "backups" / (Path(gc.GRAPH_JSON).name + ".bak")),
        ):
            if resolved == Path(state_path).resolve():
                hits.append(str(state_path))
        return hits
    except Exception:
        return []


def GRAPH_HTML_PATH() -> Path:
    from fno.graph._constants import GRAPH_HTML

    return Path(GRAPH_HTML)


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
    same recorded reason as ``_load_obsidian_vault``: this runs
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


def render_local_targets() -> int:
    """Write every configured ``local`` board target through the native front
    binary and return the count of failures.

    The page is the served web backlog with the rows embedded, written by
    ``fno board-render`` (JSON request on stdin, JSON receipt on stdout): one
    gather serves every target, so a four-target config reads the store once.
    The ``roadmap``/``backlog`` HTML projections were the second board this
    surface retired; such rows warn and skip with that reason instead of
    rendering. Like every post-publish render, never raises.
    """
    try:
        targets = _configured_targets()
    except Exception as exc:  # noqa: BLE001 - a render pass never fails the write
        print(f"Warning: local board render skipped: {exc}", file=sys.stderr)
        return 1
    local, retired = [], []
    for target in targets:
        if target.projection == "local":
            local.append({"path": target.path, "scope": _target_scope(target)[0]})
        else:
            retired.append(target)
    for target in retired:
        print(
            f"Warning: render target {target.path}: projection "
            f"{target.projection!r} is retired; the web backlog page replaces "
            "the rendered boards (fno mux serve --web / fno backlog view)",
            file=sys.stderr,
        )
    if not local:
        return 0
    try:
        from fno.rust_binary import VerbUnavailable, resolve_front_binary

        binary = resolve_front_binary()
        if binary is None:
            raise VerbUnavailable("the native fno binary was not found")
        import json
        import subprocess

        request = json.dumps({"targets": local, "vault": _load_obsidian_vault()})
        done = subprocess.run(
            [str(binary), "board-render"],
            input=request,
            capture_output=True,
            text=True,
            timeout=600,
        )
    except Exception as exc:  # noqa: BLE001 - every target failed together
        print(f"Warning: local board render failed: {exc}", file=sys.stderr)
        return len(local)
    if done.returncode != 0:
        err = (done.stderr or "").strip()
        if "No such command 'board-render'" in err or "No such command \"board-render\"" in err:
            # The installed front binary predates the verb: an install-lag
            # degradation, not a failed render. Name the remedy once and let
            # the keeper's next settled tick retry after the update.
            print(
                "Warning: local board render skipped: the installed fno "
                "predates board-render; run `fno doctor update --rust`",
                file=sys.stderr,
            )
            return 0
        print(
            f"Warning: board render exited {done.returncode}: {err[:300]}",
            file=sys.stderr,
        )
    failures = 0
    try:
        receipt = json.loads(done.stdout or "{}")
    except Exception:  # noqa: BLE001 - an unreadable receipt counts every target
        print(f"Warning: board render receipt was unreadable: {(done.stdout or '')[:200]}",
              file=sys.stderr)
        return len(local)
    failures += len(receipt.get("failed", []))
    for row in receipt.get("failed", []):
        print(
            f"Warning: render target {row.get('path')} failed: {row.get('error')}",
            file=sys.stderr,
        )
    return failures


