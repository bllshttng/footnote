"""fno target CLI - discoverable bootstrap for /fno:target sessions.

Exposes:
    fno target init --input <text|ab-id> [--plan-path <path>]

Why this exists (Change 3 of the worktree-binding plan): the canonical
bootstrap lives at ``hooks/helpers/init-target-state.sh`` - outside the
skill dir and named without a path in SKILL.md. Agents looking in the skill
dir fail to find it and substitute the discoverable-but-wrong ``fno state
init``, which writes a stub the stop hook then archives (often in a loop).
A discoverable verb that REFUSES to write a stub closes that substitution at
the source for every CLI (Claude / Codex / Gemini), since ``fno`` is the
shared dependency.

The verb is a thin wrapper: it sets ``TARGET_START=1`` plus the input env the
init script reads (``TARGET_INPUT`` / ``TARGET_PLAN_PATH``) and execs the
canonical script, propagating its exit code. It records nothing itself - the
init script owns the owner_cwd worktree binding and all state.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import socket
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.paths import resolve_plugin_script


target_app = typer.Typer(
    name="target",
    help="Target session bootstrap (records input/plan_path + owner_cwd binding).",
    add_completion=False,
)

_INIT_RELPATH = "hooks/helpers/init-target-state.sh"


def _resolve_init_script() -> Path:
    """Locate the canonical init script from the PLUGIN root, not the cwd repo.

    The init script ships with the fno plugin, not with arbitrary user
    projects. Resolving it via ``resolve_repo_root()`` (the active project)
    means ``fno target init`` would fail in normal plugin usage - running
    ``fno`` inside a target project where ``hooks/helpers/`` does not exist
    (Codex P1 on #337). Since this verb is the mandatory bootstrap path, it
    resolves from the plugin install first.

    Order:
      1. ``CLAUDE_PLUGIN_ROOT`` - set by Claude Code in plugin context.
      2. ``FNO_REPO_ROOT`` - explicit override / test hook.
         Both env hints are authoritative: if set, the script is expected
         there and we do NOT fall through to guessing, so a misconfigured
         root surfaces loudly via the caller's ``is_file()`` check.
      3. Package-relative: the repo root is three parents above this file
         (``cli/src/fno/target_cli.py`` -> repo root) for a
         source/editable install where ``hooks/`` sits beside ``cli/``.
      4. ``resolve_repo_root()`` - last resort (running inside fno repo).
    """
    # Delegates to the shared resolver (env hint -> package-relative ->
    # persisted ~/.fno/plugin-root pointer -> repo) so `fno target init`
    # finds the script from any project without a hand-set FNO_REPO_ROOT.
    return resolve_plugin_script(_INIT_RELPATH)


_SIZE_ORDER = {"S": 0, "M": 1, "L": 2}


def _modulate_size(
    verdict: str,
    *,
    size_explicit: bool,
    operator_size: Optional[str],
    downgrade: bool,
    matched_paths: Optional[list] = None,
) -> tuple[Optional[str], Optional[str]]:
    """Pure blast -> effective-size decision (Locked Decision 1: floor up, cautious down).

    Returns ``(effective_size, announce)``:
      * ``effective_size`` is the token to FORCE into TARGET_SIZE, or ``None``
        meaning "leave the operator/default size untouched".
      * ``announce`` is a one-line human note, or ``None`` when nothing changed.

    Rules:
      high    -> floor at ``M``: max(base, M), even over an explicit ``S``
                 (non-overridable downward). base is the operator size or the
                 implicit ``M`` default. Always pins the floor (so a re-init
                 reads the floored size, never regresses).
      low     -> downgrade to ``S`` ONLY when ``downgrade`` and the size was
                 NOT explicitly pinned. An explicit size is never downgraded.
      unknown -> no change (fail-safe to today's behavior).
    """
    base = operator_size or "M"
    if verdict == "high":
        floor = "M"
        effective = base if _SIZE_ORDER[base] >= _SIZE_ORDER[floor] else floor
        hit = (matched_paths or ["control-plane"])[0]
        if effective != base:
            note = f"blast: high ({hit}) -> floor {effective} (operator size {base} raised)"
        else:
            note = f"blast: high ({hit}); ceremony already at/above floor M (size {base})"
        return effective, note
    if verdict == "low" and downgrade and not size_explicit:
        return "S", "blast: low -> fast path S (no size pinned)"
    return None, None


def _load_blast_cfg():
    """Load config.target.blast, fail-safe to a disabled default block.

    A malformed settings file (or any load error) degrades to a default
    ``BlastConfig`` (enabled=False) so a config typo can never make the blast
    read raise. Mirrors the ``agents_headless_yolo`` try/except idiom.
    """
    from fno.config import BlastConfig

    try:
        from fno.config import load_settings

        return load_settings().target.blast
    except Exception:
        return BlastConfig()


def _repo_root_or_none() -> Optional[str]:
    """Best-effort repo root for path normalization; None on any failure."""
    try:
        from fno.paths import resolve_repo_root

        return str(resolve_repo_root())
    except Exception:
        return None


def _resolve_plan_for_blast(plan_path: Optional[str], input_: Optional[str]) -> Optional[str]:
    """Plan path the blast read should classify, or None to skip.

    Honors Locked Decision 2 (plan AND node inputs covered): an explicit
    ``--plan-path`` wins; otherwise the ``--input`` is tokenized and each token
    is matched (exact, case-insensitive, format-agnostic) against a graph entry
    id. This covers modifier-prefixed node inputs - the auto-continue path
    builds ``/target no-merge <id>`` and passes the original arg to
    ``fno target init`` - while a free-text feature description (no token equals
    an id) simply skips. Exactly one distinct node match is required; zero or
    ambiguous (>=2) -> skip. No fuzzy title guessing, so a description never
    mis-resolves. Fail-safe to None on any error.
    """
    if plan_path:
        return plan_path
    tokens = (input_ or "").split()
    if not tokens:
        return None
    try:
        from fno.graph.load import load_graph
        from fno.paths import graph_json

        graph_data = load_graph(graph_json())
        if not isinstance(graph_data, list):
            return None
        by_id: dict[str, dict] = {}
        for entry in graph_data:
            if isinstance(entry, dict):
                eid = entry.get("id")
                if isinstance(eid, str):
                    by_id[eid.lower()] = entry
        matched: list[dict] = []
        seen: set[str] = set()
        for tok in tokens:
            hit = by_id.get(tok.lower())
            if hit is not None:
                key = hit.get("id", "").lower()
                if key not in seen:
                    seen.add(key)
                    matched.append(hit)
        if len(matched) == 1:
            return matched[0].get("plan_path") or None
    except Exception:
        return None
    return None


_TARGET_NODE_TOKEN_RE = re.compile(r"^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$", re.IGNORECASE)
_SOURCE_PR_URL_RE = re.compile(
    r"https?://github\.com/([^/\s]+)/([^/\s]+)/pull/(\d+)(?:[#?\s)]|$)",
    re.IGNORECASE,
)
_GH_PREFLIGHT_TIMEOUT_S = 30.0


def _resolve_dispatch_node(
    input_: Optional[str], plan_path: Optional[str]
) -> Optional[dict]:
    """Resolve only an exact node/plan reference for the retro gate.

    Free-text target inputs return ``None`` without reading GitHub. Exact
    matching keeps ordinary target init behavior cheap and avoids title/fuzzy
    guesses at the claim boundary.
    """
    tokens = (input_ or "").split()
    node_tokens = {tok.lower() for tok in tokens if _TARGET_NODE_TOKEN_RE.fullmatch(tok)}
    try:
        from fno.graph.load import load_graph
        from fno.paths import graph_json

        graph = load_graph(graph_json())
    except Exception:  # noqa: BLE001 - the dispatch gate is fail-open
        return None
    if not isinstance(graph, list):
        return None
    entries = [entry for entry in graph if isinstance(entry, dict)]

    if node_tokens:
        matches = [
            entry
            for entry in entries
            if str(entry.get("id", "")).lower() in node_tokens
        ]
        return matches[0] if len(matches) == 1 else None

    if not plan_path:
        return None
    try:
        wanted = Path(plan_path).expanduser().resolve()
    except OSError:
        return None
    matches = []
    for entry in entries:
        candidate = entry.get("plan_path")
        if not isinstance(candidate, str) or not candidate:
            continue
        try:
            if Path(candidate).expanduser().resolve() == wanted:
                matches.append(entry)
        except OSError:
            continue
    return matches[0] if len(matches) == 1 else None


def _source_pr_repo(node: dict, source_pr: int) -> Optional[str]:
    """Extract the source PR repo from the retro node's canonical permalink."""
    from fno.graph._reconcile import repo_slug_from_url

    values = [node.get("source_pr_url"), node.get("pr_url"), node.get("details")]
    for value in values:
        match = _SOURCE_PR_URL_RE.search(str(value or ""))
        if match and int(match.group(3)) == source_pr:
            url = match.group(0).rstrip("#? )\t\r\n")
            return repo_slug_from_url(url)
    return None


def _evidence_region_files(node: dict) -> list[str]:
    """Read file paths from an enriched ``git:merged-region:<path>`` packet."""
    paths: list[str] = []

    def visit(value: Any) -> None:
        if isinstance(value, dict):
            items = value.get("items")
            if items is not None:
                visit(items)
            for key, child in value.items():
                if isinstance(key, str) and key.startswith("git:merged-region:"):
                    path = key.removeprefix("git:merged-region:").strip()
                    if path and path not in paths:
                        paths.append(path)
                elif key != "items":
                    visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    for key in ("evidence", "validity_evidence", "evidence_packet"):
        visit(node.get(key))
    return paths


def _fetch_dispatch_region_file(
    node: dict, source_pr: int, finding_hash: str, repo: str
) -> Optional[str]:
    """Best-effort live enrichment for nodes whose packet is not persisted."""
    root = node.get("cwd")
    if not isinstance(root, str) or not os.path.isdir(root):
        return None
    try:
        from fno.graph.cli import _fetch_retro_comment, _read_merged_region

        comment = _fetch_retro_comment(
            source_pr, finding_hash, root, repo=repo
        )
        if not comment:
            return None
        path = comment.get("path")
        if not isinstance(path, str) or not path:
            return None
        line = comment.get("line") or comment.get("original_line")
        if not _read_merged_region(root, path, line):
            return None
        return path
    except Exception:  # noqa: BLE001 - Tier 2 is advisory and fail-open
        return None


GhRunner = Callable[[list[str]], tuple[int, str, str]]


def _default_preflight_gh_runner(args: list[str]) -> tuple[int, str, str]:
    try:
        proc = subprocess.run(
            ["gh", *args],
            capture_output=True,
            text=True,
            check=False,
            timeout=_GH_PREFLIGHT_TIMEOUT_S,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return 127, "", str(exc)
    return proc.returncode, proc.stdout, proc.stderr


def _preflight_gh_json(
    args: list[str], runner: GhRunner
) -> Any:
    rc, out, err = runner(args)
    if rc != 0:
        raise RuntimeError(err.strip()[:160] or f"gh exited {rc}")
    try:
        return json.loads(out) if out.strip() else None
    except (json.JSONDecodeError, ValueError) as exc:
        raise RuntimeError(f"invalid JSON: {exc}") from exc


def _parse_github_time(value: object) -> Optional[datetime]:
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)


def _force_supersede_dispatch_node(node_id: str, reason: str) -> bool:
    proc = subprocess.run(
        ["fno", "backlog", "done", node_id, "--force", "--reason", reason],
        check=False,
    )
    return proc.returncode == 0


def _retro_dispatch_preflight(
    node: Optional[dict],
    *,
    beastmode: bool = False,
    unattended: bool = False,
    scan_fn: Optional[Callable[..., list]] = None,
    gh_runner: Optional[GhRunner] = None,
    supersede_fn: Optional[Callable[[str, str], bool]] = None,
    confirm_fn: Optional[Callable[..., bool]] = None,
) -> None:
    """Run the two-tier retro dedup probe before target init claims a node."""
    if not node:
        return

    from fno.graph.maintain import is_retro_triage_node, parse_retro_trailer

    if not is_retro_triage_node(node):
        return
    parsed = parse_retro_trailer(node.get("details"))
    if parsed is None:
        return
    source_pr, finding_hash = parsed
    if source_pr is None:
        typer.echo(
            "WARN target init: Tier 1 skipped (retro node has source_pr=None); proceeding.",
            err=True,
        )
        typer.echo(
            "WARN target init: Tier 2 skipped (retro node has no source PR); proceeding.",
            err=True,
        )
        return

    repo = _source_pr_repo(node, source_pr)
    if not repo:
        typer.echo(
            f"WARN target init: Tier 1 skipped (source PR #{source_pr} repo is unavailable); proceeding.",
            err=True,
        )
        typer.echo(
            "WARN target init: Tier 2 skipped (source PR repo is unavailable); proceeding.",
            err=True,
        )
        return

    from fno.retro.reconcile_findings import scan_addressed_findings

    scan = scan_fn or scan_addressed_findings
    scan_warnings: list[str] = []
    try:
        addressed = scan(
            [node], include_planned=True, warnings=scan_warnings
        )
    except Exception as exc:  # noqa: BLE001 - never close on uncertainty
        addressed = []
        scan_warnings.append(str(exc))

    if addressed:
        finding = addressed[0]
        reason = (
            f"retro finding addressed on source PR #{source_pr} ({finding.signal}); "
            "target dispatch preflight"
        )
        typer.echo(
            f"WARN target init: refusing {node.get('id')} - finding already addressed "
            f"on source PR #{source_pr} ({finding.signal}); superseding node.",
            err=True,
        )
        close = supersede_fn or _force_supersede_dispatch_node
        if not close(str(node.get("id")), reason):
            typer.echo(
                f"WARN target init: could not supersede {node.get('id')}; refusing dispatch.",
                err=True,
            )
        raise typer.Exit(code=3)
    if scan_warnings:
        typer.echo(
            f"WARN target init: Tier 1 skipped (source PR #{source_pr} state unavailable); proceeding.",
            err=True,
        )

    files = _evidence_region_files(node)
    if not files:
        live_file = _fetch_dispatch_region_file(node, source_pr, finding_hash, repo)
        if live_file:
            files = [live_file]
    if not files:
        typer.echo(
            "WARN target init: Tier 2 skipped (enriched merged-file-region unavailable); proceeding.",
            err=True,
        )
        return

    runner = gh_runner or _default_preflight_gh_runner
    try:
        source = _preflight_gh_json(
            ["pr", "view", str(source_pr), "--repo", repo, "--json", "mergedAt"],
            runner,
        )
        source_merged = _parse_github_time(
            source.get("mergedAt") if isinstance(source, dict) else None
        )
        if source_merged is None:
            raise RuntimeError("source PR has no parseable mergedAt")
        merged = _preflight_gh_json(
            [
                "pr", "list", "--repo", repo, "--state", "merged", "--limit", "100",
                "--json", "number,title,mergedAt,files",
            ],
            runner,
        )
        if not isinstance(merged, list):
            raise RuntimeError("merged PR list was not an array")
    except Exception as exc:  # noqa: BLE001 - Tier 2 is advisory
        typer.echo(
            f"WARN target init: Tier 2 skipped (sibling PR probe failed: {exc}); proceeding.",
            err=True,
        )
        return

    finding_files = {path.lstrip("./") for path in files}
    overlaps: list[tuple[int, str, list[str]]] = []
    for pr in merged:
        if not isinstance(pr, dict):
            continue
        merged_at = _parse_github_time(pr.get("mergedAt"))
        if merged_at is None or merged_at <= source_merged:
            continue
        changed = []
        for item in pr.get("files") or []:
            path = item.get("path") if isinstance(item, dict) else item
            if isinstance(path, str) and path.lstrip("./") in finding_files:
                changed.append(path)
        if changed and isinstance(pr.get("number"), int):
            overlaps.append((pr["number"], str(pr.get("title") or ""), changed))

    if not overlaps:
        return
    for number, title, changed in overlaps:
        typer.echo(
            f"WARN target init: Tier 2 sibling PR #{number} {title!r} overlaps "
            f"finding files: {', '.join(changed)}.",
            err=True,
        )

    if beastmode or unattended:
        typer.echo(
            "WARN target init: Tier 2 overlap is advisory; proceeding with dispatch.",
            err=True,
        )
        return

    confirm = confirm_fn or typer.confirm
    if confirm("Dispatch anyway? [y/N]", default=False):
        return
    reason = (
        f"retro finding overlaps merged sibling PR(s) after source PR #{source_pr}; "
        "operator chose supersede at target dispatch"
    )
    close = supersede_fn or _force_supersede_dispatch_node
    if not close(str(node.get("id")), reason):
        typer.echo(
            f"WARN target init: could not supersede {node.get('id')}; refusing dispatch.",
            err=True,
        )
    raise typer.Exit(code=3)


@target_app.command("blast-check")
def blast_check(
    plan: str = typer.Argument(
        ..., help="Path to the plan whose File Ownership Map to classify"
    ),
    quiet: bool = typer.Option(
        False, "--quiet", help="Print only the bare verdict token (high|low|unknown)"
    ),
) -> None:
    """Classify a plan's touched surface as high / low / unknown blast radius.

    Prints JSON ``{verdict, matched_paths, reason}`` (``--quiet`` prints the
    bare token). Always exits 0 and always returns a verdict, even on an
    unreadable plan or a classifier error (-> ``unknown``): the caller treats a
    non-``high``/``low`` verdict as "leave ceremony unchanged", so this verb can
    never block a target init.
    """
    from fno.target.blast import (
        UNKNOWN,
        classify,
        parse_ownership_map,
        resolve_plan_index,
    )

    try:
        text = resolve_plan_index(plan).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        result = {
            "verdict": UNKNOWN,
            "matched_paths": [],
            "reason": f"cannot read plan {plan!r}: {exc}",
        }
    else:
        cfg = _load_blast_cfg()
        paths = parse_ownership_map(text)
        try:
            result = classify(paths, cfg, repo_root=_repo_root_or_none())
        except Exception as exc:  # noqa: BLE001 - fail-safe to unknown, never raise
            result = {
                "verdict": UNKNOWN,
                "matched_paths": [],
                "reason": f"classifier error: {exc}",
            }

    typer.echo(result["verdict"] if quiet else json.dumps(result))


@target_app.command("status")
def status(
    node: Optional[str] = typer.Argument(
        None, help="Node id to orient on (default: read from the session manifest)."
    ),
    plan_path: Optional[str] = typer.Option(
        None, "--plan-path", help="Plan to reconcile (default: manifest plan_path)."
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the report as a JSON object."
    ),
) -> None:
    """Resolved orientation report: node, attended, worktree, tests, plan, done-when.

    Strictly read-only -- never mutates the graph, manifest, or a claim. Each
    line resolves independently; an unresolvable line prints `unknown` plus the
    one command that resolves it. Re-run bare after compaction to re-orient.
    """
    from fno.target.orient import load_orientation, render

    try:
        from fno.paths import resolve_repo_root

        root = resolve_repo_root()
    except Exception:  # noqa: BLE001 - degrade to cwd when not in a repo
        root = Path.cwd()
    lines = load_orientation(root, node_id=node, plan_path=plan_path)
    if json_output:
        typer.echo(json.dumps({ln.label: ln.value for ln in lines}, indent=2))
    else:
        typer.echo(render(lines))


@target_app.command()
def init(
    input_: Optional[str] = typer.Option(
        None, "--input", help="Original target argument: feature text or ab-XXXX node id"
    ),
    plan_path: Optional[str] = typer.Option(
        None, "--plan-path", help="Path to an existing plan to execute"
    ),
    size: Optional[str] = typer.Option(
        None,
        "--size",
        help="Size profile: S, M, or L. Sets TARGET_SIZE so the init script "
        "resolves the matching skip-flag profile. Without it, target_size is "
        "left blank and the caller must set TARGET_SIZE by hand.",
    ),
    model: Optional[str] = typer.Option(
        None,
        "--model",
        "-m",
        help="Pin a model for this session's dispatched workers (exact "
        "passthrough). Persisted to the manifest so it survives into the do phase.",
    ),
    provider: Optional[str] = typer.Option(
        None,
        "--provider",
        "-p",
        help="Pin a provider for this session's dispatched workers. Absent, the "
        "spawn path infers it from the invoking harness at dispatch time.",
    ),
    beastmode: bool = typer.Option(
        False,
        "--beastmode",
        "--beast",
        help="Grant walk-away authority (writes `authority: full` to the "
        "manifest). Judgment calls that would emit <help> and stall are decided "
        "and recorded to an Autonomous Decisions ledger instead. Never grants "
        "irreversibles - merge stays on the --auto-merge axis.",
    ),
) -> None:
    """Bootstrap a target session via the canonical init script.

    Requires --input or --plan-path. With neither, exits non-zero and writes
    no state file: a stub (empty input + plan) is exactly what the stop hook
    archives, so refusing it here prevents the archive loop at the source.
    """
    if not input_ and not plan_path:
        typer.echo(
            "fno target init: requires --input <text|ab-id> or --plan-path <path>.\n"
            "Refusing to write a stub state file (empty input + plan); the stop "
            "hook would archive it, and a re-running bootstrap loops.\n"
            'Example: fno target init --input "fix the login redirect bug"',
            err=True,
        )
        raise typer.Exit(code=2)

    script_path = _resolve_init_script()
    if not script_path.is_file():
        # Capability-accurate degrade (US3 / AC3-ERR): a bare `pip install fno`
        # ships the CLI + binaries but NOT the footnote plugin (skills + hooks),
        # so there is no pipeline for `target init` to bootstrap. Name the
        # missing capability and the install path - never a 127 or traceback.
        # In a clone the script is present and this branch never fires, so
        # in-clone behavior is byte-for-byte unchanged (AC3-HP). The is_file()
        # check runs before any subprocess, so no partial state is written
        # (AC3-FR / AC3-EDGE).
        typer.echo(
            "fno target init: needs the footnote plugin (skills + hooks), which "
            "a bare `pip install fno` does not ship - the bundled CLI has no "
            "pipeline to bootstrap.\n"
            "Install the plugin and run from its checkout:\n"
            "  clone the footnote repo, then run `claude --plugin-dir "
            "/path/to/footnote`\n"
            "Or set CLAUDE_PLUGIN_ROOT / FNO_REPO_ROOT to an existing plugin "
            f"checkout. (resolved, not on disk: {script_path})",
            err=True,
        )
        raise typer.Exit(code=2)

    normalized_size: Optional[str] = None
    if size is not None:
        normalized_size = size.strip().upper()
        if normalized_size not in {"S", "M", "L"}:
            typer.echo(
                f"fno target init: invalid --size {size!r}; expected S, M, or L.",
                err=True,
            )
            raise typer.Exit(code=2)

    # Validate the dispatch pins before writing any state. Empty --model/--provider
    # is a usage error (never a forwarded empty argv token); provider is resolved
    # only when given, so an absent pin lets the spawn path infer the harness at
    # dispatch time rather than freezing it here.
    from fno.agents.provider_resolve import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_provider,
    )

    try:
        dispatch_model = reject_empty_model(model)
        dispatch_provider = (
            resolve_dispatch_provider(provider)[0] if provider is not None else None
        )
    except DispatchFlagError as exc:
        typer.echo(f"fno target init: {exc}", err=True)
        raise typer.Exit(code=2)

    # Retro-triaged nodes get a dispatch-time dedup check before the shell
    # bootstrap acquires the node claim. Ordinary target inputs return early;
    # probe failures remain fail-open toward dispatch.
    _retro_dispatch_preflight(
        _resolve_dispatch_node(input_, plan_path),
        beastmode=beastmode,
        unattended=bool(
            os.environ.get("TARGET_UNATTENDED")
            or os.environ.get("FNO_AGENT_SELF")
            or os.environ.get("FNO_BG")
        ),
    )

    # Blast-radius modulation (x-518f): a deterministic blast read on the plan's
    # File Ownership Map can raise ceremony to an M floor (high blast) or drop to
    # the S fast path (low blast, unpinned size) BEFORE the immutable manifest is
    # written. Plan AND node inputs are covered (a free-text input has no surface
    # yet -> skipped). Gated on config.target.blast.enabled FIRST, so the disabled
    # path does zero extra work (no graph load, no classify) and is byte-for-byte
    # today's behavior. Every failure degrades to "unknown" -> no change.
    cfg = _load_blast_cfg()
    blast_plan = (
        _resolve_plan_for_blast(plan_path, input_)
        if getattr(cfg, "enabled", False)
        else None
    )
    if blast_plan:
        try:
            from fno.target.blast import (
                classify,
                parse_ownership_map,
                resolve_plan_index,
            )

            plan_text = resolve_plan_index(blast_plan).read_text(encoding="utf-8")
            result = classify(
                parse_ownership_map(plan_text),
                cfg,
                repo_root=_repo_root_or_none(),
            )
        except Exception:  # noqa: BLE001 - fail-safe: any error -> unchanged
            result = {"verdict": "unknown", "matched_paths": []}
        # An existing TARGET_SIZE in the env is an operator pin too (the
        # documented init path lets callers pin the resolved profile via the env
        # var, not only --size). Treat it as explicit so a low-blast plan never
        # strips ceremony the operator already pinned.
        env_size: Optional[str] = os.environ.get("TARGET_SIZE", "").strip().upper()
        env_size = env_size if env_size in {"S", "M", "L"} else None
        effective, announce = _modulate_size(
            result.get("verdict", "unknown"),
            size_explicit=size is not None or env_size is not None,
            operator_size=normalized_size or env_size,
            downgrade=bool(getattr(cfg, "downgrade", True)),
            matched_paths=result.get("matched_paths", []),
        )
        if effective is not None:
            normalized_size = effective
        if announce:
            typer.echo(announce, err=True)

    env = dict(os.environ)
    env["TARGET_START"] = "1"
    # Change D (x-a7be): resolve `attended` from the substrate before the bash
    # manifest writer runs. A spawned/bg worker has no operator at the keyboard;
    # the claude spawn path injects FNO_AGENT_SELF into EVERY spawned worker and
    # a bg thread sets FNO_BG (the spawn_think precedent, codex PR #9). Marking
    # the run unattended makes init stamp `attended: false`, so the skill
    # surfaces offers as non-blocking lines instead of a [Y/n] that hangs a
    # detached session. An explicit TARGET_UNATTENDED always wins.
    if "TARGET_UNATTENDED" not in env and (
        env.get("FNO_AGENT_SELF") or env.get("FNO_BG")
    ):
        env["TARGET_UNATTENDED"] = "1"
    if input_:
        env["TARGET_INPUT"] = input_
    if plan_path:
        env["TARGET_PLAN_PATH"] = plan_path
    if normalized_size:
        env["TARGET_SIZE"] = normalized_size
    if dispatch_model:
        env["TARGET_DISPATCH_MODEL"] = dispatch_model
    if dispatch_provider:
        env["TARGET_DISPATCH_PROVIDER"] = dispatch_provider
    # Sole authority: an inherited TARGET_BEASTMODE must never self-grant (spawns
    # inherit the parent env wholesale, so per-provider scrubbing cannot cover it).
    env["TARGET_BEASTMODE"] = "1" if beastmode else ""

    proc = subprocess.run(["bash", str(script_path)], check=False, env=env)
    if proc.returncode == 0:
        if beastmode:
            _warn_if_authority_not_granted()
        _print_orientation_report()
        _maybe_dispatch_work_start()
        _maybe_reconcile_lane_slot()
    raise typer.Exit(code=propagate_returncode(proc.returncode))


def _warn_if_authority_not_granted(project_root: Optional[Path] = None) -> None:
    """Name a --beastmode that did not take: the write-once manifest and both of
    start's idempotent early returns drop the flag, and an ungranted session
    looks identical to one whose flag was dropped.

    Verdict comes from the orienter's own predicate, never a string match, so
    this warning and the `attended` line can never disagree about whether the
    grant holds - a stamped-but-stale claim denies authority in one place and
    must not read as granted in the other. Callers on a worktree path pass their
    own root; the cwd default is only right for init.
    """
    try:
        from fno.paths import resolve_repo_root
        from fno.target.orient import _authority_granted, _read_manifest

        root = project_root or resolve_repo_root()
        raw = _read_manifest(root)
        if _authority_granted(raw):
            return
        stamped = str((raw or {}).get("authority", "")).strip().lower() == "full"
    except Exception:  # noqa: BLE001 - a warning must never fail a run that worked
        return
    if stamped:
        typer.echo(
            "--beastmode was stamped but has NOTHING LIVE TO ANCHOR IT - this session "
            "will not act with authority.\nAuthority needs a LIVE CLAIM: a "
            "free-text run claims no node, and a stale claim proves nothing, so "
            "an abandoned session would be indistinguishable from a live one. The "
            "grant is refused rather than left to outlive its session.\nRe-run "
            "against a backlog node (`fno target start --beastmode <node>`), or "
            "continue without authority.",
            err=True,
        )
        return
    fix = (
        "The manifest is write-once and one already existed, so the flag was a "
        "no-op. To run with authority, finish or cancel this session first "
        "(`/fno:cancel-target`), then start a fresh one."
        if raw
        else "No manifest was written, so nothing consumed the flag. Run "
        "`fno target init --beastmode --input <node>` to claim this session with a grant."
    )
    typer.echo(f"--beastmode did NOT take - this session has no authority grant.\n{fix}", err=True)


def _maybe_reconcile_lane_slot() -> None:
    """Bind a parallel-mode lane slot to THIS worker's lifecycle (LD#8).

    Runs right after the init script claims ``node:<id>``. A parallel-mode
    dispatcher (``fno backlog dispatch-lanes``) holds a lane slot
    (``parallel-lane:<node>``) across the spawn->init window, TTL-anchored to
    itself; now that this worker owns the node, re-anchor that slot to the
    worker's durable session pid so ``active_lane_count`` tracks the real lane
    and frees the slot when the worker ends. A no-op for every non-parallel run
    (this node holds no lane slot) and for a missing pid. Strictly non-fatal:
    never affects the init exit code the caller propagates.
    """
    try:
        from fno.paths import resolve_repo_root

        repo_root = resolve_repo_root()
        manifest = repo_root / ".fno" / "target-state.md"
        text = manifest.read_text(encoding="utf-8")
        m = re.search(r"^graph_node_id\s*:\s*(.+)$", text, re.MULTILINE)
        if not m:
            return
        node_id = m.group(1).strip().strip("\"'")
        if not node_id or node_id == "null":
            return
        from fno.claims.lanes import reconcile_lane_slot
        from fno.claims.session_pid import resolve_session_pid

        reconcile_lane_slot(node_id, pid=resolve_session_pid())
    except Exception:  # noqa: BLE001 - additive; never affect the init exit code
        pass


def _print_orientation_report() -> None:
    """Change A (x-a7be): print the resolved situation report as init's first
    orientation output. Reads the just-written manifest; strictly read-only and
    fully non-fatal -- a degraded report never affects the init exit code.
    """
    try:
        from fno.paths import resolve_repo_root
        from fno.target.orient import load_orientation, render

        typer.echo(render(load_orientation(resolve_repo_root())))
    except Exception:  # noqa: BLE001 - orientation is additive; never block init
        pass


def _maybe_dispatch_work_start() -> None:
    """A2 (x-122a): fire a ``work-start`` context /think after a node is claimed.

    Runs right after the init script returns success - the authoritative
    ``fno claim acquire node:<id>`` has completed and the manifest is written, so
    this is the "node enters work" moment (Claude's Discretion 4). Reads the
    claimed ``graph_node_id`` back from the manifest (``null`` => no node claimed
    => nothing to dispatch) and routes the durable node through
    ``on_node_work_start``, gated by ``config.think_spawn.on_work_start``
    (default OFF). Strictly non-fatal: any failure here never affects the init
    exit code the caller propagates.
    """
    try:
        from fno.config import load_settings

        # Gate-first: the default-OFF install (every un-opted-in install) pays one
        # settings read and returns - NO git rev-parse, NO graph load, NO manifest
        # read. The wrapper re-checks the sub-flag authoritatively below.
        try:
            if not load_settings().think_spawn.on_work_start:
                return
        except Exception:  # noqa: BLE001 - fail-safe to disabled
            return

        from fno.paths import resolve_repo_root

        repo_root = resolve_repo_root()
        manifest = repo_root / ".fno" / "target-state.md"
        text = manifest.read_text(encoding="utf-8")
        m = re.search(r"^graph_node_id\s*:\s*(.+)$", text, re.MULTILINE)
        if not m:
            return
        node_id = m.group(1).strip().strip("\"'")
        if not node_id or node_id == "null":
            return

        from fno.provenance.spawn_think import on_node_work_start

        node = _find_node(node_id)
        if node is not None:
            # Carry the session's persisted dispatch pins into the work-start
            # /think spawn. maybe_spawn_think reads node["model"]/node["provider"]
            # at the spawn seam, so overlaying the manifest fields here is all it
            # takes for `fno target start --model X` to reach the spawned worker.
            dm = re.search(r"^dispatch_model\s*:\s*(.*)$", text, re.MULTILINE)
            dp = re.search(r"^dispatch_provider\s*:\s*(.*)$", text, re.MULTILINE)
            model_pin = dm.group(1).strip().strip("\"'") if dm else ""
            provider_pin = dp.group(1).strip().strip("\"'") if dp else ""
            if model_pin:
                node["model"] = model_pin
            if provider_pin:
                node["provider"] = provider_pin
            on_node_work_start(node, project_root=repo_root)
    except Exception:  # noqa: BLE001 - additive; never affect the init exit code
        pass


# --------------------------------------------------------------------------- #
# `fno target start` - one-verb cold-start (x-d91b).
# --------------------------------------------------------------------------- #
def _wt_name(node: str) -> str:
    """Filesystem-safe worktree name from a node id/slug or feature text.

    A node id (``x-d91b``) or slug is already clean and round-trips unchanged;
    free-text input is slugified so the dir/branch name never carries spaces or
    shell-hostile characters. Bounded so a long feature string cannot produce a
    pathological path component.
    """
    s = re.sub(r"[^A-Za-z0-9._-]+", "-", node.strip().lower()).strip("-")
    # strip("-") AFTER the slice too: truncation can land on a hyphen and leave
    # a trailing one (gemini PR #114).
    return s[:60].strip("-") or "target"


def _resolve_node_id(node: str) -> str:
    """Resolve a slug / bare-hex input to its canonical backlog node id.

    ``fno target init`` only derives the node id (and thus the node claim) from
    an exact graph id or ``--plan-path``; a documented slug forwarded raw would
    write ``graph_node_id: null`` and skip the claim, so another worker could
    grab the same card (codex PR #114). Resolving here means a slug input still
    claims its node. A non-match (free-text feature input) returns the arg
    unchanged - init accepts text - so this only ever upgrades a resolvable id,
    never blocks one. Best-effort: any load/resolve error falls through to raw.
    """
    try:
        from fno.graph.fuzzy import resolve_node
        from fno.graph.load import load_graph
        from fno.paths import graph_json

        data = load_graph(graph_json())
        entries = data if isinstance(data, list) else []
        match = resolve_node(node, entries)
        node_id = getattr(match, "id", None)
        if getattr(match, "kind", "none") == "exact" and node_id:
            return node_id
    except Exception:  # noqa: BLE001 - best-effort; the raw arg still works
        pass
    return node


def _find_node(node_id: str) -> Optional[dict]:
    """The graph node dict for an exact id, or None (best-effort, never raises)."""
    try:
        from fno.graph.load import load_graph
        from fno.paths import graph_json

        data = load_graph(graph_json())
        return next(
            (
                e
                for e in (data if isinstance(data, list) else [])
                if isinstance(e, dict) and e.get("id") == node_id
            ),
            None,
        )
    except Exception:  # noqa: BLE001 - best-effort; caller degrades to default
        return None


def _resolve_node_model(
    node_id: str, *, explicit: Optional[str] = None, provider: Optional[str] = None
) -> tuple[Optional[str], str]:
    """``(model, decision_source)`` for a node's ``model`` pin / ``model_tier``.

    The single Python projection of ``route_resolve`` at the ``target start`` seam
    (the same precedence ``advance.py`` uses), so tier resolution lives in exactly
    ONE place. An explicit ``-m`` wins without loading the node. ``provider`` scopes
    tier resolution to the spawn harness so a tier never yields a cross-harness
    pick; None defaults to ``claude`` (the bg spawn default -- a bg worker is
    always claude regardless of the invoking harness, so scoping by the ambient
    harness would mis-resolve; Locked 3 intent is the incident bg-default lane).
    ``model`` is None -> the spawn path uses the provider default. Strictly
    non-fatal: any error degrades to the explicit value or the provider default,
    so a dispatch never fails because of the routing layer (inherited Locked 10).
    """
    try:
        from fno import route_resolve

        node = None if explicit else _find_node(node_id)
        model, source, _chain = route_resolve.resolve_dispatch_model(
            explicit=explicit,
            task_model=(node or {}).get("model"),
            task_tier=(node or {}).get("model_tier"),
            provider=provider or "claude",
        )
        return model, source
    except Exception:  # noqa: BLE001 - routing degrades, never blocks a dispatch
        return explicit, "explicit" if explicit else "provider-default"


def _model_reachable_by(model: str, provider: str) -> bool:
    """True if ``model`` maps to the ``provider`` harness (per the benchmark map).

    A tier resolves the cheapest model clearing its floor across ALL harnesses, so
    it can pick a model mapped to a different provider than the spawn lane uses.
    Best-effort: an unknown model or any lookup error is treated as reachable so
    the guard only ever DROPS a confirmed cross-harness pick, never a valid one.
    """
    try:
        from fno.adapters.providers import benchmarks as bm

        reach = bm.reachable(model)
        return reach is None or reach[0] == provider
    except Exception:  # noqa: BLE001 - never block a dispatch on a lookup error
        return True


@target_app.command("resolve-model")
def resolve_model(
    node: str = typer.Argument(..., help="Backlog node id/slug."),
    provider: Optional[str] = typer.Option(
        None,
        "--provider",
        help="Only print the model if it is reachable by this harness. A tier "
        "can resolve to a model mapped to another provider (e.g. a codex gpt-*); "
        "a single-provider spawn lane (bg is claude-only) passes its provider "
        "here so a cross-harness pick degrades to the provider default instead "
        "of an invalid `<provider> --model <foreign-model>` spawn.",
    ),
) -> None:
    """Print the dispatch model a node resolves to (its ``model`` pin / ``model_tier``).

    The one Python projection of ``route_resolve`` for bash dispatchers
    (``dispatch-node.sh``), so a tiered node's worker spawns on the tier model
    without bash ever reimplementing resolution. Prints the resolved model on
    stdout, or nothing when the node has no pin/tier (the caller uses the provider
    default). Never fails a dispatch: any error prints nothing.
    """
    model, _source = _resolve_node_model(_resolve_node_id(node), provider=provider)
    if model and provider and not _model_reachable_by(model, provider):
        return  # a pin resolves unfiltered; the guard drops a cross-harness pin
    if model:
        typer.echo(model)


def _resolve_fno_cmd() -> list[str]:
    """Locate the ``fno`` executable to compose its own subcommands.

    PATH first, then a sibling of the running interpreter (the editable/uv
    install layout). Falls back to the bare name so a misconfigured PATH still
    surfaces a real subprocess error rather than a silent no-op.
    """
    # Resolve `fno-py` (the console script; the Rust mux binary owns `fno`).
    found = shutil.which("fno-py")
    if found:
        return [found]
    sibling = Path(sys.executable).parent / "fno-py"
    if sibling.exists():
        return [str(sibling)]
    return ["fno-py"]


def _git_out(cwd: Path, *args: str) -> Optional[str]:
    proc = subprocess.run(
        ["git", "-C", str(cwd), *args], capture_output=True, text=True
    )
    if proc.returncode != 0 or not proc.stdout.strip():
        return None
    return proc.stdout.strip()


def _is_linked_worktree(cwd: Path) -> bool:
    """True if ``cwd`` is inside a git LINKED worktree (git-dir != common-dir).

    This is the location verdict ``ok`` condition in pure git terms: a linked
    worktree means we are already isolated and ``start`` must no-op rather than
    nest a worktree inside a worktree.
    """
    gdir = _git_out(cwd, "rev-parse", "--git-dir")
    common = _git_out(cwd, "rev-parse", "--git-common-dir")
    if not gdir or not common:
        return False

    def _abs(p: str) -> Path:
        path = Path(p)
        return (path if path.is_absolute() else cwd / path).resolve()

    return _abs(gdir) != _abs(common)


def _manifest_node_id(manifest: Path) -> Optional[str]:
    """The manifest's claimed ``graph_node_id``, or None if absent/null/unreadable."""
    try:
        text = manifest.read_text(encoding="utf-8")
    except OSError:
        return None
    m = re.search(r"^graph_node_id\s*:\s*(.+)$", text, re.MULTILINE)
    if not m:
        return None
    val = m.group(1).strip().strip("\"'")
    return None if (not val or val == "null") else val


def _foreign_live_holder(node_id: str) -> Optional[dict]:
    """Claim info for ``node:<id>`` iff a DIFFERENT live/suspect session holds
    it, else None (free / dead / ours).

    Read-only and never raises: any probe failure degrades to None so ``start``
    behaves exactly as before the guard when the claim system is unreadable.
    Liveness is pid-based (``classify``), so a busy owner whose TTL lapsed still
    reads live - that is what lets the caller park a second session instead of
    telling it the owner went idle.
    """
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for
    from fno.claims.session_pid import resolve_session_pid

    # node: claims live under $HOME, not the default root -- claims_root_for(key)
    # routes there; a bare claim_status(key) would read the wrong tree as free.
    key = f"node:{node_id}"
    try:
        info = claim_status(key, root=claims_root_for(key))
    except Exception:
        return None
    if info.get("state") not in ("live", "suspect"):
        return None
    # Ours by declared identity. A driver-run claude session sets
    # TARGET_SESSION_ID; a codex session has no TARGET_SESSION_ID, so
    # init-target-state.sh makes its raw CODEX_THREAD_ID the claim owner. Match
    # either -- the durable-pid arm below only resolves a claude ancestor, so
    # codex parity (a same-thread re-run is not foreign) depends on this arm.
    holder = info.get("holder")
    for env_var in ("TARGET_SESSION_ID", "CODEX_THREAD_ID"):
        own_id = os.environ.get(env_var)
        if own_id and holder == f"target-session:{own_id}":
            return None
    # Ours by durable session pid + host (a bare interactive re-run with no TSID).
    # An uncapturable own pid on a live foreign-looking claim reads as foreign
    # (park, never share) -- the conservative direction.
    try:
        own_pid = resolve_session_pid(from_pid=os.getpid())
        own_host = socket.gethostname()  # can raise OSError in sandboxes
    except Exception:
        own_pid = own_host = None
    if own_pid and info.get("pid") == own_pid and info.get("host") == own_host:
        return None
    return info  # foreign + live/suspect -> caller refuses


def _print_foreign_holder_park(node_id: str, info: dict, wt_path: Path) -> None:
    """Loud park naming the live holder so a second session does not assume the
    node went idle. Both start exits call this so the message is identical."""
    holder = info.get("holder", "unknown")
    pid = info.get("pid", "?")
    host = info.get("host", "?")
    typer.echo(
        f"fno target start: node {node_id} is held by a live session\n"
        f"  {holder} (pid={pid}, host={host}).\n"
        f"  Refusing to share its worktree at {wt_path} "
        f"(would corrupt a shared git index).\n"
        f"  If this is your session, cd {wt_path} to continue it;\n"
        f"  otherwise wait for it to release the claim, or pick another node.",
        err=True,
    )


@target_app.command()
def start(
    node: str = typer.Argument(
        ..., help="Backlog node id/slug to start (or feature text)."
    ),
    plan_path: Optional[str] = typer.Option(
        None, "--plan-path", help="Path to an existing plan to execute."
    ),
    size: Optional[str] = typer.Option(
        None, "--size", help="Size profile: S, M, or L (forwarded to init)."
    ),
    model: Optional[str] = typer.Option(
        None, "--model", "-m",
        help="Pin a model for this session's dispatched workers (forwarded to init).",
    ),
    provider: Optional[str] = typer.Option(
        None, "--provider", "-p",
        help="Pin a provider for this session's dispatched workers (forwarded to init).",
    ),
    beastmode: bool = typer.Option(
        False, "--beastmode", "--beast",
        help="Grant walk-away authority for this session (forwarded to init).",
    ),
) -> None:
    """Cold-start a worktree-isolated target session in ONE verb.

    Collapses the five-move bootstrap a bg ``/target`` does by hand - whose two
    silent killers (``.fno`` whole-dir symlink -> init refuses; base behind
    origin/main -> phantom-deletion PRs) live only in agent memory - into one
    idempotent verb with a printed receipt, so a memory-less agent succeeds.

    Composes: ``fno worktree ensure`` (create/reuse off origin/main, never local
    HEAD) -> heal ``.fno`` + link shared state -> ``fno target init`` (writes the
    manifest, claims the node exactly once) -> receipt. Run from INSIDE a valid
    worktree it is a no-op.
    """
    cwd = Path.cwd()

    # Boundary: already isolated -> no-op, create nothing (x-45e6 case). But
    # first refuse if a DIFFERENT live session holds this node's claim: this cwd
    # is that session's worktree and sharing its git index corrupts the build.
    if _is_linked_worktree(cwd):
        node_id = _resolve_node_id(node)
        holder = _foreign_live_holder(node_id)
        if holder is not None:
            _print_foreign_holder_park(node_id, holder, cwd)
            raise typer.Exit(code=1)
        typer.echo(f"already isolated at {cwd}; nothing created.")
        if beastmode:
            _warn_if_authority_not_granted()
        return

    repo_root_s = _git_out(cwd, "rev-parse", "--show-toplevel")
    if not repo_root_s:
        typer.echo(
            f"fno target start: {cwd} is not a git repository.", err=True
        )
        raise typer.Exit(code=1)
    repo_root = Path(repo_root_s)
    fno = _resolve_fno_cmd()
    # Resolve a slug/hex input to its canonical id so init claims the node (a raw
    # slug would write graph_node_id: null and skip the claim) - both the
    # worktree name and `init --input` then use the canonical id.
    node = _resolve_node_id(node)
    name = _wt_name(node)

    # 1. Create/reuse the worktree off origin/main (x-73ca). ensure prints the
    #    worktree path on stdout and is idempotent (reuse) + refuses to nest.
    #    Forward the current session's harness so a claude cold-start lands
    #    harness-native at <repo>/.claude/worktrees/<name>; a bare terminal with no
    #    ambient marker omits it and ensure degrades to the external base.
    from fno.harness_identity import resolve_harness_identity

    harness = resolve_harness_identity().harness
    ensure_cmd = fno + ["worktree", "ensure", "--repo", str(repo_root), "--name", name]
    if harness:
        ensure_cmd += ["--harness", harness]
    ens = subprocess.run(ensure_cmd, capture_output=True, text=True)
    wt = ens.stdout.strip()
    if ens.returncode != 0 or not wt:
        typer.echo(
            f"fno target start: worktree ensure failed (step: ensure): "
            f"{ens.stderr.strip() or 'no path on stdout'}",
            err=True,
        )
        raise typer.Exit(code=1)
    wt_path = Path(wt)

    # policy=never: ensure returned the repo main checkout itself (launch in place,
    # no worktree). Skip the worktree-only heal + setup-worktree.sh - both mutate
    # the CANONICAL .fno (unlink a real symlink, re-link shared state), the exact
    # corruption Locked Decision 4 forbids. Init still runs, in place.
    in_place = wt_path.resolve() == repo_root.resolve()

    # 2. Heal .fno when it arrived as a whole-dir symlink (the memory-only fix,
    #    now in code), then link shared state via the canonical setup hook.
    healed = False
    if not in_place:
        fno_dir = wt_path / ".fno"
        if fno_dir.is_symlink():
            fno_dir.unlink()
            fno_dir.mkdir()
            healed = True
        from fno.worktree import _run_setup_worktree_hook

        rc, tail = _run_setup_worktree_hook(repo_root, wt_path)
        if rc not in (0, -1):
            # Non-fatal: the worktree is still usable; name it but do not abort.
            typer.echo(
                f"fno target start: setup-worktree.sh exited {rc} (non-fatal): {tail}",
                err=True,
            )

    base_label = "in-place" if in_place else "origin/main"

    # Idempotent re-run from canonical: a manifest already in the worktree means
    # init has run (write-once) - skip it, never double-claim or error.
    manifest = wt_path / ".fno" / "target-state.md"
    fno_state = "in-place" if in_place else ("healed" if healed else "ok")
    if manifest.exists() and not manifest.is_symlink():
        # A manifest means init ran, so the claim is set - refuse if a DIFFERENT
        # live session owns it rather than presenting its worktree as usable.
        holder = _foreign_live_holder(node)
        if holder is not None:
            _print_foreign_holder_park(node, holder, wt_path)
            raise typer.Exit(code=1)
        # In-place (policy=never) manifests live in the SHARED canonical .fno, so
        # unlike a per-node worktree this one may belong to a DIFFERENT node - the
        # fast-path's "manifest => THIS node's init ran" invariant does not hold.
        # A node mismatch is another node's (stale/foreign) session; refuse rather
        # than report already-claimed and let the caller run under its state.
        if in_place:
            mnode = _manifest_node_id(manifest)
            if mnode is not None and mnode != node:
                typer.echo(
                    f"fno target start: {manifest} belongs to node {mnode}, not "
                    f"{node}; refusing to run in place under another node's session. "
                    f"Cancel it (fno target cancel) or isolate a worktree.",
                    err=True,
                )
                raise typer.Exit(code=1)
        typer.echo(
            f"worktree={wt_path}  .fno={fno_state}  base={base_label}  "
            f"node=already-claimed"
        )
        if beastmode:
            _warn_if_authority_not_granted(wt_path)
        return

    # Project the node's model pin / tier into init's dispatch pin so a bare
    # start on a tiered node carries the resolved model (x-d7a7). An explicit -m
    # wins (precedence, resolved inside the helper); no pin/tier -> None ->
    # nothing forwarded, byte-identical to pre-change. Never blocks (Locked 10).
    model, decision_source = _resolve_node_model(
        node, explicit=model, provider=provider
    )

    # 3. Init the session FROM the worktree (binds owner_cwd, claims the node
    #    exactly once - preserve the existing one-call claim).
    init_cmd = fno + ["target", "init", "--input", node]
    if plan_path:
        init_cmd += ["--plan-path", plan_path]
    if size:
        init_cmd += ["--size", size]
    if model:
        init_cmd += ["--model", model]
    if provider:
        init_cmd += ["--provider", provider]
    if beastmode:
        init_cmd += ["--beastmode"]
    init = subprocess.run(init_cmd, cwd=str(wt_path))
    if init.returncode != 0:
        typer.echo(
            f"fno target start: target init failed (step: init, exit "
            f"{init.returncode}); worktree at {wt_path} is created but unclaimed.",
            err=True,
        )
        raise typer.Exit(code=init.returncode)

    # 4. Receipt - one parse-friendly line a memory-less agent acts on. When a
    #    model was resolved, record it + its decision_source so the dispatch is
    #    auditable (x-d7a7); absent -> today's line, byte-identical.
    model_note = f"  model={model} ({decision_source})" if model else ""
    typer.echo(
        f"worktree={wt_path}  .fno={fno_state}  base={base_label}  node=claimed{model_note}"
    )
    typer.echo(f"cd {wt_path} to continue the pipeline.", err=True)
