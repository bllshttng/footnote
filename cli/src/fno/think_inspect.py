"""Read-only discovery receipt for ``fno think inspect``.

The model still judges relevance and chooses a design. This module only gathers
facts whose truth should not depend on prompt compliance: repository state,
graph overlap, PR search availability, schema grounding, and the project lesson
corpus. Every external command is argv-only and every missing source is typed.
"""
from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path
from typing import Any, Callable

Run = Callable[..., subprocess.CompletedProcess[str]]
_UNSET = object()
_DB_ENV_KEYS = re.compile(r"^\s*(DATABASE_URL|SUPABASE_DB_URL|POSTGRES_URL|DIRECT_URL)\s*=", re.MULTILINE)


def _default_run(argv: list[str], *, cwd: Path, timeout: int = 10) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        cwd=cwd,
        capture_output=True,
        text=True,
        check=False,
        timeout=timeout,
    )


def _command(run: Run, argv: list[str], repo: Path) -> tuple[str | None, str | None]:
    try:
        result = run(argv, cwd=repo, timeout=10)
    except FileNotFoundError:
        return None, f"{argv[0]} not found"
    except subprocess.TimeoutExpired:
        return None, f"{argv[0]} timed out"
    except OSError as exc:
        return None, str(exc)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or f"exit {result.returncode}").strip()
        return None, detail
    return result.stdout, None


def _repository(repo: Path, run: Run) -> dict[str, Any]:
    branch, branch_error = _command(run, ["git", "rev-parse", "--abbrev-ref", "HEAD"], repo)
    head, head_error = _command(run, ["git", "rev-parse", "HEAD"], repo)
    status, status_error = _command(run, ["git", "status", "--porcelain"], repo)
    log, log_error = _command(
        run, ["git", "log", "-5", "--pretty=format:%h%x09%s"], repo
    )
    errors = [e for e in (branch_error, head_error, status_error, log_error) if e]
    dirty_paths = []
    for line in (status or "").splitlines():
        path = line[3:].strip() if len(line) >= 4 else line.strip()
        if " -> " in path:
            path = path.split(" -> ", 1)[1]
        if path:
            dirty_paths.append(path)
    commits = []
    for line in (log or "").splitlines():
        sha, _, subject = line.partition("\t")
        if sha:
            commits.append({"sha": sha, "subject": subject})
    return {
        "root": str(repo),
        "status": "ok" if not errors else "error",
        "branch": (branch or "").strip() or None,
        "head": (head or "").strip() or None,
        "dirty_paths": dirty_paths,
        "recent_commits": commits,
        "detail": "; ".join(dict.fromkeys(errors)) or None,
    }


def _load_graph(path: Path) -> tuple[list[dict[str, Any]] | None, str | None]:
    try:
        from fno.graph.store import read_graph_strict

        return read_graph_strict(path), None
    except Exception as exc:  # noqa: BLE001 - the receipt types corrupt/missing evidence
        return None, str(exc)


def _node_summary(entry: dict[str, Any], *, archived: bool = False) -> dict[str, Any]:
    return {
        key: entry.get(key)
        for key in (
            "id",
            "slug",
            "title",
            "status",
            "domain",
            "project",
            "parent",
            "plan_path",
            "pr_number",
            "pr_url",
            "additional_prs",
            "merge_status",
        )
        if entry.get(key) is not None
    } | ({"archived": True} if archived else {})


def _graph_section(
    seed: str,
    entries: list[dict[str, Any]] | None,
    archive: list[dict[str, Any]],
    error: str | None,
    archive_error: str | None,
) -> dict[str, Any]:
    if entries is None:
        return {
            "status": "error",
            "resolved": None,
            "parent": None,
            "closure": None,
            "duplicates": [],
            "epic_candidates": [],
            "archive_status": "error" if archive_error else "ok",
            "detail": error or "graph unavailable",
        }

    from fno.graph.fuzzy import resolve_node
    from fno.graph.relatedness import _MIN_SCORE, epic_candidates, similar_nodes

    active_by_id = {
        row["id"]: row for row in entries if isinstance(row, dict) and isinstance(row.get("id"), str)
    }
    archive_by_id = {
        row["id"]: row for row in archive if isinstance(row, dict) and isinstance(row.get("id"), str)
    }
    combined = list(active_by_id.values()) + [
        row for node_id, row in archive_by_id.items() if node_id not in active_by_id
    ]
    match = resolve_node(seed, combined)
    resolved = match.candidates[0] if match.kind == "exact" and match.candidates else None
    probe = resolved or {
        "id": "__think_seed__",
        "title": seed,
        "slug": seed,
        "details": seed,
    }
    # floor=_MIN_SCORE: blueprint's consolidation gate reads this list, and the
    # real lock family behind that gate sits at 0.26-0.27 under a 0.30 dedup
    # floor. Ranked recall is the point; a full-context reader makes the call.
    scored = similar_nodes(probe, combined, k=5, floor=_MIN_SCORE)
    duplicates = []
    for node_id, score, reason in scored:
        row = active_by_id.get(node_id) or archive_by_id.get(node_id)
        if row is None:
            continue
        duplicates.append(
            _node_summary(row, archived=node_id in archive_by_id and node_id not in active_by_id)
            | {"score": score, "reason": reason}
        )
    rollups = []
    for node_id, score, reason in epic_candidates(probe, combined, k=3):
        row = active_by_id.get(node_id) or archive_by_id.get(node_id)
        if row is None:
            continue
        rollups.append(
            _node_summary(row, archived=node_id in archive_by_id and node_id not in active_by_id)
            | {"score": score, "reason": reason}
        )
    parent = None
    if resolved and isinstance(resolved.get("parent"), str):
        parent_row = active_by_id.get(resolved["parent"]) or archive_by_id.get(resolved["parent"])
        if parent_row:
            parent = _node_summary(
                parent_row,
                archived=resolved["parent"] in archive_by_id and resolved["parent"] not in active_by_id,
            )
    # Graph re-read of the resolved node's closure, not classify_closure: that
    # helper probes a named behavior against current main and needs inputs a
    # blueprint does not have. A done/superseded row here is a halt signal for
    # the consolidation gate, so the fields are read verbatim from the row.
    closure = None
    if resolved:
        closure = {
            "status": resolved.get("status"),
            "pr_number": resolved.get("pr_number"),
            "superseded_by": resolved.get("superseded_by"),
        }
    return {
        "status": "partial" if archive_error else "ok",
        "resolved": (
            _node_summary(
                resolved,
                archived=(
                    resolved.get("id") in archive_by_id
                    and resolved.get("id") not in active_by_id
                ),
            )
            if resolved
            else None
        ),
        "parent": parent,
        "closure": closure,
        "duplicates": duplicates,
        "epic_candidates": rollups,
        "archive_status": "error" if archive_error else "ok",
        "detail": f"archive unreadable: {archive_error}" if archive_error else None,
    }


def _pull_requests(seed: str, repo: Path, run: Run) -> dict[str, Any]:
    stdout, error = _command(
        run,
        [
            "gh",
            "pr",
            "list",
            "--state",
            "all",
            "--search",
            seed,
            "--limit",
            "20",
            "--json",
            "number,title,state,url",
        ],
        repo,
    )
    if error:
        return {"status": "unavailable", "matches": [], "detail": error}
    try:
        payload = json.loads(stdout or "[]")
    except json.JSONDecodeError as exc:
        return {"status": "error", "matches": [], "detail": f"invalid gh JSON: {exc}"}
    if not isinstance(payload, list):
        return {"status": "error", "matches": [], "detail": "gh returned a non-list payload"}
    return {"status": "ok", "matches": payload, "detail": None}


def _database(repo: Path) -> dict[str, Any]:
    signals: list[str] = []
    signal_paths: list[Path] = []
    fixed = (
        "prisma/schema.prisma",
        "drizzle.config.ts",
        "drizzle.config.js",
        "drizzle.config.mjs",
        "drizzle.config.cjs",
        "supabase/migrations",
        "drizzle",
    )
    for relative in fixed:
        path = repo / relative
        if path.exists():
            signals.append(relative)
            signal_paths.append(path)
    for name in (".env", ".env.local", ".env.development", ".env.test"):
        path = repo / name
        try:
            text = path.read_text(encoding="utf-8")
        except (FileNotFoundError, OSError, UnicodeDecodeError):
            continue
        signals.extend(f"{name}:{match}" for match in _DB_ENV_KEYS.findall(text))
        if _DB_ENV_KEYS.search(text):
            signal_paths.append(path)
    signals = sorted(dict.fromkeys(signals))
    artifact = repo / ".fno" / "codemap.md"
    grounded = False
    try:
        grounded = "## Database Schema" in artifact.read_text(encoding="utf-8")
    except (FileNotFoundError, OSError, UnicodeDecodeError):
        pass
    stale = False
    if grounded:
        try:
            artifact_mtime = artifact.stat().st_mtime_ns
            source_mtimes: list[int] = []
            for path in signal_paths:
                if path.is_dir():
                    source_mtimes.extend(
                        child.stat().st_mtime_ns
                        for child in path.rglob("*")
                        if child.is_file()
                    )
                elif path.is_file():
                    source_mtimes.append(path.stat().st_mtime_ns)
            stale = bool(source_mtimes and max(source_mtimes) > artifact_mtime)
        except OSError:
            stale = True
    status = "stale" if stale else ("grounded" if grounded else ("missing" if signals else "not-applicable"))
    return {
        "detected": bool(signals) or grounded,
        "signals": signals,
        "schema_artifact": ".fno/codemap.md" if artifact.exists() else None,
        "schema_status": status,
    }


def _pitfall_headings(repo: Path) -> tuple[str, list[str]]:
    for name in ("AGENTS.md", "CLAUDE.md"):
        path = repo / name
        try:
            text = path.read_text(encoding="utf-8")
        except (FileNotFoundError, OSError, UnicodeDecodeError):
            continue
        marker = "## Pitfalls corpus (capped)"
        start = text.find(marker)
        if start < 0:
            continue
        tail = text[start + len(marker):]
        next_section = re.search(r"^## (?!#)", tail, re.MULTILINE)
        section = tail[: next_section.start()] if next_section else tail
        headings = re.findall(r"^###\s+(.+?)\s*$", section, re.MULTILINE)
        return name, headings
    return "missing", []


def _pitfalls(repo: Path, plans_path: Path, home: Path) -> dict[str, Any]:
    source, entries = _pitfall_headings(repo)
    retros = []
    if plans_path.is_dir():
        retros = [str(path) for path in sorted(plans_path.glob("*retro-synthesis*.md"), reverse=True)[:5]]
    candidates = home / ".fno" / "lesson-candidates.jsonl"
    try:
        count = sum(1 for line in candidates.read_text(encoding="utf-8").splitlines() if line.strip())
    except (FileNotFoundError, OSError, UnicodeDecodeError):
        count = 0
    return {
        "source": source,
        "entries": entries,
        "retro_syntheses": retros,
        "lesson_candidates": count,
    }


def build_receipt(
    seed: str,
    repo: Path,
    *,
    graph_entries: list[dict[str, Any]] | None | object = _UNSET,
    archive_entries: list[dict[str, Any]] | object = _UNSET,
    graph_error: str | None = None,
    plans_path: Path | None = None,
    home: Path | None = None,
    run: Run = _default_run,
) -> dict[str, Any]:
    """Collect a deterministic, read-only design-discovery receipt."""
    from fno.config import load_settings_for_repo
    from fno.paths import resolve_configured_path

    repo = Path(repo).resolve()
    settings = load_settings_for_repo(repo)
    graph_override = settings.paths.graph_json
    graph_path = (
        resolve_configured_path(graph_override, project_root=repo, settings=settings)
        if graph_override is not None
        else resolve_configured_path(settings.state_dir, project_root=repo, settings=settings)
        / "graph.json"
    )
    archive_path = graph_path.parent / "graph-archive.json"
    configured_plans_path = resolve_configured_path(
        settings.plans_dir,
        project_root=repo,
        settings=settings,
    )
    if graph_entries is _UNSET:
        graph_entries, graph_error = _load_graph(graph_path)
    archive_error = None
    if archive_entries is _UNSET:
        if archive_path.exists():
            archive_entries, archive_error = _load_graph(archive_path)
        else:
            archive_entries = []
    archive = archive_entries if isinstance(archive_entries, list) else []
    graph = _graph_section(
        seed,
        graph_entries if isinstance(graph_entries, list) else None,
        archive,
        graph_error,
        archive_error,
    )
    repository = _repository(repo, run)
    resolved = graph.get("resolved") or {}
    pr_seed: str = resolved["title"] if isinstance(resolved.get("title"), str) else seed
    pull_requests = _pull_requests(pr_seed, repo, run)
    database = _database(repo)
    pitfalls = _pitfalls(repo, plans_path or configured_plans_path, home or Path.home())
    warnings: list[str] = []
    if repository["status"] != "ok":
        warnings.append("repository evidence is incomplete")
    if graph["status"] != "ok":
        warnings.append("backlog graph evidence is unavailable")
    if pull_requests["status"] != "ok":
        warnings.append("pull request evidence is unavailable")
    if database["schema_status"] == "missing":
        warnings.append("database schema evidence is missing")
    if database["schema_status"] == "stale":
        warnings.append("database schema evidence is stale")
    return {
        "version": 1,
        "seed": seed,
        "complete": not warnings,
        "repository": repository,
        "graph": graph,
        "pull_requests": pull_requests,
        "database": database,
        "pitfalls": pitfalls,
        "warnings": warnings,
    }


def render_receipt(receipt: dict[str, Any]) -> str:
    """Render the receipt for a human without hiding typed source status."""
    repo = receipt["repository"]
    graph = receipt["graph"]
    prs = receipt["pull_requests"]
    database = receipt["database"]
    pitfalls = receipt["pitfalls"]
    resolved = graph.get("resolved") or {}
    lines = [
        f"think inspection: {'complete' if receipt['complete'] else 'incomplete'}",
        f"repository: {repo['status']} branch={repo.get('branch') or '-'} head={repo.get('head') or '-'} dirty={len(repo['dirty_paths'])}",
        f"graph: {graph['status']} resolved={resolved.get('id', '-')} duplicates={len(graph['duplicates'])} epics={len(graph['epic_candidates'])}",
        f"pull requests: {prs['status']} matches={len(prs['matches'])}",
        f"database: detected={str(database['detected']).lower()} schema={database['schema_status']}",
        f"pitfalls: source={pitfalls['source']} entries={len(pitfalls['entries'])} syntheses={len(pitfalls['retro_syntheses'])} candidates={pitfalls['lesson_candidates']}",
    ]
    lines.extend(f"warning: {warning}" for warning in receipt["warnings"])
    return "\n".join(lines)
