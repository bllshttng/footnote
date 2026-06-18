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
import subprocess
from pathlib import Path
from typing import Optional

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

        return load_settings().config.target.blast
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
            entry = by_id.get(tok.lower())
            if entry is not None:
                key = entry.get("id", "").lower()
                if key not in seen:
                    seen.add(key)
                    matched.append(entry)
        if len(matched) == 1:
            return matched[0].get("plan_path") or None
    except Exception:
        return None
    return None


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
        env_size = os.environ.get("TARGET_SIZE", "").strip().upper()
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
    if input_:
        env["TARGET_INPUT"] = input_
    if plan_path:
        env["TARGET_PLAN_PATH"] = plan_path
    if normalized_size:
        env["TARGET_SIZE"] = normalized_size

    result = subprocess.run(["bash", str(script_path)], check=False, env=env)
    raise typer.Exit(code=propagate_returncode(result.returncode))
