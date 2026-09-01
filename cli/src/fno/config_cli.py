"""CLI surface for config commands (`fno config ...`).

Lives next to paths_cli.py and setup_cli.py for consistency; implementation
lives in setup/doctor.py.
"""
from __future__ import annotations

from dataclasses import asdict, dataclass
import os
from pathlib import Path
from importlib import import_module
from typing import TYPE_CHECKING, Any, Literal, Optional, Union

import typer

from fno._lazy_group import make_lazy_group_cls

if TYPE_CHECKING:
    from fno.paths import StateFile

# Mounted as ``fno config accounts``: accounts ARE config
# (``config.accounts.records``), so the verb path mirrors the config path. This
# replaced the top-level ``fno providers``, which was hidden and therefore
# discoverable by nobody; as a `fno config` subcommand it shows up in
# `fno config --help` and gets its own page.
#
# Lazy for the same reason the top-level map is: the sub-app drags in the
# provider loader (~68ms), and `fno config get` is called from shell dozens of
# times per phase. `providers` used to be a top-level LAZY_SUBCOMMANDS entry, so
# mounting it eagerly here would have moved that cost onto every `fno config`.
_LAZY_SUBCOMMANDS: dict[str, tuple[str, str] | tuple[str, str, dict[str, Any]]] = {
    "accounts": ("fno.adapters.providers.cli:cli", "Manage account records."),
    # Folded under config (unit 6, x-9d6c). The old top-level spellings stay
    # one-release shims (fno.verb_moves); these mounts are the canonical
    # homes. Lazy for the same reason `accounts` is: `fno config get` runs
    # from shell dozens of times per phase and must not pay for their imports.
    # Hidden: the menu cap is 12 and config already curates nine visible
    # verbs; `fno help config --all` lists these.
    "paths": ("fno.paths_cli:app", "Path resolution helpers", {"hidden": True}),
    "plugins": (
        "fno.plugins.cli:plugins_app",
        "Install, verify, activate, and inspect function packs.",
        {"hidden": True},
    ),
    "route": (
        "fno.route_cli:route_app",
        "Provider route lanes: ls / set / unset / env (GLM build lane).",
        {"hidden": True},
    ),
    # The routing-inventory surface under its plan-named spelling: same app as
    # `route`, so `fno config routing init` and `fno config route init` both
    # resolve. Hidden like its sibling.
    "routing": (
        "fno.route_cli:route_app",
        "Declared model-routing inventory: init (sample) / inventory (reach).",
        {"hidden": True},
    ),
    # x-6233 (d-344fe242): `project init` gives a checkout its own fno state
    # root, which is state-root configuration - config's territory. Same
    # lazy+hidden treatment as its siblings above.
    "project": (
        "fno.project:project_app",
        "Isolated per-project fno environments.",
        {"hidden": True},
    ),
    "setup": ("fno.setup_cli:app", "Interactive settings.yaml wizard", {"hidden": True}),
}

app = typer.Typer(
    help="Config inspection and diagnostics",
    cls=make_lazy_group_cls(_LAZY_SUBCOMMANDS),
)


# ---------------------------------------------------------------------------
# Post-merge config readiness oracle (ab-dba85fcc)
#
# One pure, read-only verdict consumed by three callers: `fno config doctor
# --post-merge` (this surface), the /target preflight check, and the deferred
# launchd watcher. The rule lives here once so the three never disagree.
# ---------------------------------------------------------------------------

PostMergeStatus = Literal["ready", "unconfigured", "opted_out", "dormant", "error"]


@dataclass(frozen=True)
class PostMergeVerdict:
    """Whether the /fno:pr merged ritual can run for a repo, and why."""

    status: PostMergeStatus
    enabled: bool
    activity: bool
    parking_lot_path: Optional[str] = None
    project_id: Optional[str] = None
    cause: Optional[str] = None  # populated only on status == "error"
    note: Optional[str] = None  # soft advisory (e.g. project.id unset on ready)

    @property
    def is_ready(self) -> bool:
        """The pr-watch tick reads this to decide a merge dispatch (the seam
        `_noop_readiness` already fakes)."""
        return self.status == "ready"

    def to_dict(self) -> dict:
        return dict(asdict(self).items())

    def summary_line(self) -> str:
        """One human-readable line (also appended to bare `fno config doctor`)."""
        if self.status == "ready":
            line = (
                "[doctor] post-merge: ready "
                f"(parking_lot_path={self.parking_lot_path})"
            )
            return line + (f"; note: {self.note}" if self.note else "")
        if self.status == "unconfigured":
            return (
                "[doctor] post-merge: unconfigured - "
                "config.post_merge.parking_lot_path is unset; the /fno:pr merged "
                "prose+triage will be skipped. Set it with: fno config setup post-merge"
            )
        if self.status == "opted_out":
            return "[doctor] post-merge: opted_out (config.post_merge.enabled=false)"
        if self.status == "dormant":
            return "[doctor] post-merge: dormant (no fno activity in this repo)"
        if self.status == "error":
            return f"[doctor] post-merge: error - {self.cause}"
        return f"[doctor] post-merge: {self.status}"


def _load_repo_post_merge(repo_root: Path):
    """Parse this repo's `.fno/settings.yaml` and validate ONLY the post_merge
    (+ project) block. Returns ``(PostMergeBlock, project_id)``.

    Reads only the repo-local file (post_merge is a per-repo opt-in; a global
    parking_lot_path must not make every repo look ready). Validating just the
    post_merge block - not the whole SettingsModel - keeps the normal layered
    semantics for UNRELATED keys: a repo that sets e.g. ``config.obsidian.enabled``
    locally while supplying ``config.obsidian.vault`` globally must not be
    reported as a post-merge ``error`` (codex review on PR #511). A missing file
    is defaults; unparseable YAML or an invalid post_merge value RAISES so the
    caller maps it to ``error`` carrying the real cause. project.id is
    scaffold-and-note only, so a bad project block degrades to ``None`` rather
    than erroring the verdict.
    """
    import tomllib

    import yaml

    from fno.config import (
        PostMergeBlock,
        ProjectBlock,
        _deep_merge,
        _unwrap_config_dict,
        _worktree_local_override,
    )

    fno_dir = repo_root / ".fno"

    def _read_flat(toml_path: Path, yaml_path: Path) -> dict:
        """Read config as a FLAT dict, config.toml-first with a READ-ONLY legacy
        settings.yaml fallback. The oracle must never migrate (it is read-only,
        may run against an unmigrated repo, and test_oracle_is_read_only forbids
        writes), so it tolerates both formats without converting. A malformed
        file RAISES so the caller maps it to ``error``, never a false verdict.
        """
        if toml_path.is_file():
            return _unwrap_config_dict(
                tomllib.loads(toml_path.read_text(encoding="utf-8"))
            )
        if yaml_path.is_file():
            parsed = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
            if parsed is None:
                return {}
            if not isinstance(parsed, dict):
                raise ValueError(f"config is not a mapping: {yaml_path}")
            return _unwrap_config_dict(parsed)
        return {}

    raw: dict = _read_flat(fno_dir / "config.toml", fno_dir / "settings.yaml")

    # Per-worktree local override (x-cbce): layer the allowlisted key (project.id
    # since x-071c narrowed the allowlist) so this oracle agrees with
    # load_settings() and `fno config get`. Repo-local only (the local file is
    # never symlinked to canonical), which preserves the "a global
    # parking_lot_path must not make every repo look ready" guard above.
    local_toml = fno_dir / "config.local.toml"
    local_yaml = fno_dir / "settings.local.yaml"
    local_path = local_toml if local_toml.is_file() else local_yaml
    if local_path.is_file() and not local_path.is_symlink():
        try:
            if local_path.suffix == ".toml":
                local_parsed = tomllib.loads(local_path.read_text(encoding="utf-8"))
            else:
                local_parsed = yaml.safe_load(local_path.read_text(encoding="utf-8")) or {}
        except (tomllib.TOMLDecodeError, yaml.YAMLError) as exc:
            # Deliberately non-fatal, UNLIKE `_read_flat` above, which raises so
            # the caller can map it to `error`. This file only supplies the
            # allowlisted worktree-local override (project.id), so a corrupt one
            # must not take down a verdict the repo config can answer on its own.
            # It is logged rather than dropped: silently ignoring it made the
            # oracle look confident about a value it never read.
            import logging

            logging.getLogger(__name__).warning(
                "ignoring unparseable worktree-local override %s: %s", local_path, exc
            )
            local_parsed = {}
        if isinstance(local_parsed, dict):
            override = _worktree_local_override(_unwrap_config_dict(local_parsed))
            if override:
                raw = _deep_merge(raw, override)

    if not raw:
        return PostMergeBlock(), None

    pm_raw = raw.get("post_merge")
    pm_raw = pm_raw if isinstance(pm_raw, dict) else {}
    pm = PostMergeBlock.model_validate(pm_raw)  # raises on an invalid post_merge value

    project_id = None
    proj = raw.get("project")
    if isinstance(proj, dict):
        try:
            project_id = ProjectBlock.model_validate(proj).id or None
        except Exception:  # noqa: BLE001 - project.id is non-blocking; degrade to None
            project_id = None
    return pm, project_id


def _repo_has_fno_activity(repo_root: Path, project_id: Optional[str]) -> bool:
    """True if this repo ships or plans through fno, so the post-merge gap is
    reachable here. Bounded, short-circuits on the first hit, and biases to
    False (dormant) on any unreadable state - a false negative degrades to
    today's silent behavior; a false positive is the nag we are removing.
    """
    import json

    # 1. In-flight target session (cheapest: a stat). An imminent merge counts.
    try:
        if (repo_root / ".fno" / "target-state.md").is_file():
            return True
    except OSError:
        pass

    # 2. Repo-local ledger holds a session that shipped a PR.
    try:
        ledger = repo_root / ".fno" / "ledger.json"
        if ledger.is_file():
            data = json.loads(ledger.read_text(encoding="utf-8"))
            entries = data.get("entries") if isinstance(data, dict) else data
            if isinstance(entries, list):
                for entry in entries:
                    if isinstance(entry, dict) and (
                        entry.get("pr_number") or entry.get("pr_url")
                    ):
                        return True
    except (OSError, ValueError):
        pass  # bias dormant

    # 3. Global graph holds a node mapping this repo (by project.id or cwd).
    try:
        import fno.paths as paths

        graph = paths.graph_json()
        if graph.is_file():
            data = json.loads(graph.read_text(encoding="utf-8"))
            entries = data.get("entries") if isinstance(data, dict) else data
            if isinstance(entries, list):
                root_str = str(repo_root.resolve())
                for node in entries:
                    if not isinstance(node, dict):
                        continue
                    if project_id and node.get("project") == project_id:
                        return True
                    for key in ("_resolved_cwd", "cwd"):
                        cwd = node.get(key)
                        if isinstance(cwd, str) and cwd and (
                            cwd == root_str or cwd.startswith(root_str + "/")
                        ):
                            return True
    except (OSError, ValueError):
        pass  # bias dormant

    return False


def post_merge_readiness(repo_root: Union[str, Path]) -> PostMergeVerdict:
    """Read-only verdict on post-merge config readiness for ``repo_root``.

    Never writes. Order: settings load -> enabled -> activity -> parking_lot_path.
    A settings-load failure is ``error`` (distinct from ``unconfigured``).
    """
    repo_root = Path(repo_root)
    try:
        pm, project_id = _load_repo_post_merge(repo_root)
    except Exception as exc:  # noqa: BLE001 - surface the real cause, never crash
        return PostMergeVerdict(
            status="error",
            enabled=True,
            activity=False,
            cause=f"{type(exc).__name__}: {exc}",
        )

    enabled = bool(pm.enabled)
    parking = pm.parking_lot_path or None

    if not enabled:
        return PostMergeVerdict(
            status="opted_out",
            enabled=False,
            activity=False,
            parking_lot_path=parking,
            project_id=project_id,
        )

    if not _repo_has_fno_activity(repo_root, project_id):
        return PostMergeVerdict(
            status="dormant",
            enabled=True,
            activity=False,
            parking_lot_path=parking,
            project_id=project_id,
        )

    if not parking:
        return PostMergeVerdict(
            status="unconfigured",
            enabled=True,
            activity=True,
            parking_lot_path=None,
            project_id=project_id,
        )

    note = (
        None
        if project_id
        else "project.id unset - ritual auto-detects; set for clean provenance"
    )
    return PostMergeVerdict(
        status="ready",
        enabled=True,
        activity=True,
        parking_lot_path=parking,
        project_id=project_id,
        note=note,
    )


def _repo_root() -> Path:
    """Git toplevel of the cwd (the repo the oracle reports on), else cwd."""
    import subprocess

    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if out.returncode == 0 and out.stdout.strip():
            return Path(out.stdout.strip())
    except (OSError, subprocess.SubprocessError):
        pass
    return Path.cwd()


def _jsonable_container(node: object) -> object:
    """Reduce records nested inside a container the way a leaf is reduced.

    A block read (`fno config get agents.provider_limits`) returns a dict whose
    VALUES can be models, and `json.dumps` renders one as its repr. A machine
    consumer of `--json` then gets a string it cannot parse for exactly the
    key that grew a record.
    """
    from pydantic import BaseModel

    if isinstance(node, dict):
        return {k: _jsonable_container(v) for k, v in node.items()}
    if isinstance(node, list):
        return [_jsonable_container(v) for v in node]
    if isinstance(node, BaseModel):
        return node.model_dump(mode="json")
    return node


def _report_review_capability(json_out: bool = False) -> int:
    """`fno config doctor --review`: the diagnostic twin of the init refusal.

    Reports each configured reviewer as satisfiable, needs-operator, or
    unavailable. Those three stay distinct on purpose: a `human`-kind reviewer
    in an unattended run is correctly unsatisfiable, NOT a misconfiguration, and
    collapsing the two would teach an operator to "fix" a valid config.

    Exit 0 when every reviewer can run here, 1 otherwise. Diagnostic only - it
    never writes, and it never proposes `declare` as the remedy.
    """
    import json as _json

    from fno.config import load_settings
    from fno.review_capability import (
        detect_session,
        local_peers_refusal_message,
        resolve_local_peers,
        resolve_reviewers,
    )

    session = detect_session()
    review = load_settings().review
    reviewers = list(review.reviewers or [])
    verdicts = resolve_reviewers(reviewers, session, review.reviewer_registry)
    peer_verdicts = resolve_local_peers(review.peers, review.peer_identity, session)
    blocked = [v for v in verdicts if v.blocks_autonomy]
    peer_blocked = local_peers_refusal_message(peer_verdicts, session) is not None

    # Tier staleness: a band no configured provider can serve, and a tier id
    # no reachability row maps, are both routing holes an operator must see
    # here rather than at dispatch. This table drifted a full model release
    # with nothing detecting it; this line is the tripwire.
    from fno.adapters.providers.benchmarks import (
        empty_bands_for_harness,
        unreachable_tier_ids,
    )

    dead_ids = unreachable_tier_ids()
    empty_bands = empty_bands_for_harness()

    if json_out:
        typer.echo(
            _json.dumps(
                {
                    "harness": session.harness,
                    "substrate": session.substrate,
                    "attended": session.attended,
                    "provider": session.provider,
                    "reviewers": [
                        {
                            "name": v.name,
                            "status": v.status,
                            "kind": v.descriptor.kind if v.descriptor else None,
                            "requires": v.descriptor.requires if v.descriptor else None,
                            "asserts": v.descriptor.asserts if v.descriptor else None,
                            "invocation": (
                                v.descriptor.invocation if v.descriptor else None
                            ),
                            "reason": v.reason,
                            "resolved_route": v.name,
                        }
                        for v in verdicts
                    ],
                    "local_peers": [
                        {"name": v.name, "status": v.status, "reason": v.reason}
                        for v in peer_verdicts
                    ],
                    "local_peer_gate": (
                        "unavailable"
                        if peer_blocked
                        else "satisfiable"
                        if peer_verdicts
                        else "not-configured"
                    ),
                    "tier_staleness": {
                        "unreachable_tier_ids": dead_ids,
                        "empty_bands": empty_bands,
                    },
                }
            )
        )
        return 1 if blocked or peer_blocked else 0

    typer.echo(f"review gate: {session.describe()} attended={session.attended}")
    if not reviewers:
        typer.echo("  config.review.reviewers is empty - no local reviewers gate.")
    else:
        for reviewer_verdict in verdicts:
            typer.echo(f"  {reviewer_verdict.line()}")
    if dead_ids:
        typer.echo(
            "  WARN tier staleness: ids no reachability row serves: "
            + ", ".join(dead_ids)
        )
    for harness, bands in empty_bands.items():
        typer.echo(
            f"  WARN tier staleness: {harness} cannot serve band(s): {', '.join(bands)}"
        )
    if peer_verdicts:
        state = "unavailable" if peer_blocked else "satisfiable"
        typer.echo(f"  local peer gate: {state}")
        for peer_verdict in peer_verdicts:
            typer.echo(f"    {peer_verdict.line()}")
    if blocked or peer_blocked:
        typer.echo("")
        typer.echo(
            "This gate is fail-closed. Change config.review.reviewers or "
            "config.review.peers; `declare` is never substituted for you."
        )
    return 1 if blocked or peer_blocked else 0


def _report_gates() -> None:
    """Print the resolved ship gates: each probe with its source, each reviewer
    with the rung it asserts.

    This is where an operator should discover that their guardrail is only
    WITNESSED - a skill gate proves the skill ran at the reviewed commit and
    claims nothing about its verdict - rather than at the stop gate after the
    work is done. Read-only: doctor reports the probes, it never runs them.

    Only the project source is visible here; a plan's own `done_probes` is
    resolved per session against its bound plan doc, which doctor has no
    business guessing at.
    """
    from fno.config import load_settings
    from fno.review_capability import detect_session, resolve_reviewers

    settings = load_settings()
    probes = list(settings.done_probes or [])
    reviewers = list(settings.review.reviewers or [])
    if not probes and not reviewers:
        return

    typer.echo("")
    typer.echo("ship gates:")
    for cmd in probes:
        typer.echo(f"  probe (project): {cmd}")
    if not probes:
        typer.echo("  probe: none declared in config (a plan may still declare its own)")
    for v in resolve_reviewers(
        reviewers, detect_session(), settings.review.reviewer_registry
    ):
        rung = v.descriptor.asserts if v.descriptor else "unknown"
        typer.echo(f"  reviewer: {v.name} - asserts {rung}{_RUNG_GLOSS.get(rung, '')}")


_RUNG_GLOSS = {
    "review-evidence": " (a reviewer ran and returned a verdict)",
    "invocation": " (the reviewer ran at the reviewed commit; no claim about its verdict)",
    "self-cert": " (nothing; satisfies the gate on its own say-so)",
}


@app.command("doctor")
def doctor_cmd(
    post_merge: bool = typer.Option(
        False,
        "--post-merge",
        help="Report this repo's post-merge config readiness (read-only).",
    ),
    review: bool = typer.Option(
        False,
        "--review",
        help="Report whether every config.review.reviewers entry can actually "
        "run in this session (read-only). Diagnostic twin of the refusal "
        "`fno do target init` performs.",
    ),
    json_out: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="With --post-merge, emit the verdict as a single JSON object.",
    ),
) -> None:
    """Inspect resolved paths; flag suspicious values. Read-only.

    With ``--post-merge`` (or ``--json``), instead report whether
    ``config.post_merge.parking_lot_path`` is set for this repo - the gate the
    /fno:pr merged ritual needs. With ``--review``, report whether every
    configured local reviewer can run in this session. Bare ``fno config
    doctor`` runs the path diagnostic and appends a one-line post-merge
    readiness summary plus the resolved ship gates (each probe with its source,
    each reviewer with the rung it asserts).
    """
    import json as _json

    if review:
        raise typer.Exit(_report_review_capability(json_out=json_out))

    if post_merge or json_out:
        verdict = post_merge_readiness(_repo_root())
        if json_out:
            typer.echo(_json.dumps(verdict.to_dict()))
        else:
            typer.echo(verdict.summary_line())
        raise typer.Exit(0)

    from fno.setup.doctor import run_doctor

    rc = run_doctor()
    # Open Question 1: bare doctor carries a one-line post-merge summary so the
    # gap is visible without remembering the flag. Best-effort; never crashes
    # the diagnostic.
    try:
        typer.echo(post_merge_readiness(_repo_root()).summary_line())
    except Exception:  # noqa: BLE001 - the summary is advisory, not the command
        pass
    try:
        _report_gates()
    except Exception:  # noqa: BLE001 - a report, not the diagnostic itself
        pass
    try:
        _report_state_roots()
    except Exception:  # noqa: BLE001 - a report, not the diagnostic itself
        pass
    try:
        _report_deprecated_auto_merge()
    except Exception:  # noqa: BLE001 - advisory, same wrap as the three above
        pass
    raise typer.Exit(rc)



def _state_root_selector(row: "StateFile") -> str:
    """The key that actually decided this row's root, not the one that could have.

    The table's ``selector`` names the candidates in precedence order. When a
    ``config.paths.*`` override is really in play, ``resolve_source`` says
    WHICH file set it, so the receipt cannot drift from the loader's own merge
    semantics - it is reading the loader's answer rather than re-deriving one.
    """
    from fno.config import resolve_source

    for token in row.selector.split(","):
        key = token.strip().removeprefix("else ").strip()
        # Drop a trailing parenthetical note. Without this the registry row -
        # the only one that carries one - hands resolve_source a key ending in
        # "(Rust runtime home: FNO_AGENTS_HOME)", gets None, and silently
        # never names its deciding file. A receipt whose whole job is naming
        # the decider must not have one row that structurally cannot.
        key = key.split("(", 1)[0].strip()
        if not key.startswith("config."):
            continue
        try:
            decided = resolve_source(key.removeprefix("config."))
        except Exception:  # noqa: BLE001 - a receipt, not the loader
            decided = None
        if decided is not None:
            return f"{key} (set in {decided[0]})"
    return row.selector


def _report_state_roots() -> None:
    """Name the root each state class actually resolved, and the key that chose it.

    `fno config get review.max_rounds` answered from the canonical root while
    the gate resolver answered from the worktree. The diagnostic tool and the
    runtime disagreed BY CONSTRUCTION and nothing reported the disagreement,
    so the wrong answer stayed a returned value for as long as anyone looked
    at it. Asserting that a resolver RETURNS something proves nothing; this
    prints WHICH root it returned.

    The warning line is the cheapest half and the whole point: whenever the
    cwd-derived project root differs from the ``--git-common-dir`` canonical
    one, the two are named side by side. Read-only.
    """
    from fno.paths import STATE_FILES, resolve_canonical_repo_root, resolve_repo_root

    typer.echo("")
    typer.echo("state roots:")
    typer.echo(f"  {'class':<9}{'file':<17}{'root':<48}selector")
    for row in STATE_FILES:
        if row.resolver is None:
            resolved = "NO RESOLVER - built by hand at every call site"
        else:
            module_name, _, attr = row.resolver.rpartition(".")
            try:
                resolved = str(getattr(import_module(module_name), attr)())
            except Exception as exc:  # noqa: BLE001 - a receipt, not the resolver
                resolved = f"unresolvable ({type(exc).__name__})"
        # One space minimum: a resolved path longer than the column must not
        # run into the selector and read as one token.
        typer.echo(
            f"  {row.root_class:<9}{row.filename:<17}{resolved:<48} "
            f"{_state_root_selector(row)}"
        )

    worktree_root = resolve_repo_root().resolve()
    canonical_root = resolve_canonical_repo_root().resolve()
    if worktree_root != canonical_root:
        typer.echo(
            f"  WARNING: the project root resolved from cwd ({worktree_root}) differs\n"
            f"           from the canonical root ({canonical_root}). Project config is\n"
            "           read from the canonical root, so a key set only in this\n"
            "           worktree's .fno/settings.yaml is NOT what the loader served,\n"
            "           and a gate key set only in canonical reads its shipped default\n"
            "           for any resolver that stays on the worktree root."
        )


def _report_deprecated_auto_merge() -> None:
    """Name every config file still setting the deprecated ``dispatch.auto_merge``.

    The migration arm of x-4be1: the alias keeps old files working for one
    release, and this line tells the operator WHICH file to move. Reads the
    raw candidate chain (not the merged model) so each file is named
    individually - the merged model cannot tell them apart, which is exactly
    the home-vs-project confusion the node exists to end.
    """
    from fno.config import _candidate_paths, _global_settings_path, _load_raw

    global_dir = _global_settings_path().parent
    for candidate in _candidate_paths():
        if not candidate.is_file():
            continue
        parsed, ok = _load_raw(candidate)
        if not ok:
            continue
        # Either shape: flat config.toml (top-level dispatch) or a pre-migration
        # settings.yaml (config-wrapped dispatch); the canonical grant lives in
        # the same scope so the masked check uses the file's real shape.
        scope = parsed if isinstance(parsed.get("dispatch"), dict) else parsed.get("config")
        legacy = parsed.get("dispatch")
        if not isinstance(legacy, dict):
            wrapped = parsed.get("config")
            legacy = wrapped.get("dispatch") if isinstance(wrapped, dict) else None
        if not (isinstance(legacy, dict) and "auto_merge" in legacy):
            continue
        # Migration commands must target THE SAME FILE this warning names:
        # `fno config set/unset` defaults to the global scope, so a project
        # file needs --local or the operator edits the wrong file. The removal
        # must also drop the legacy key, or the warning recurs forever.
        scope_flag = "" if candidate.parent == global_dir else " --local"
        canonical = scope.get("auto_merge") if isinstance(scope, dict) else None
        if isinstance(canonical, dict) and "grant" in canonical:
            # Canonical wins in this file (`_alias_am_grant` refuses to fold), so
            # the legacy line is INERT. Never print its fold value as a migration
            # target: telling the operator to `config set auto_merge.grant
            # dispatch` on a file whose canonical grant is "none" would arm
            # unattended merge on a project they just disarmed.
            typer.echo(
                f"warn: {candidate} still sets the deprecated `dispatch.auto_merge`, "
                "but a canonical `auto_merge.grant` in the same file masks it "
                "(the file reads as the canonical value). Remove the legacy line.\n"
                f"      Migrate: fno config unset dispatch.auto_merge{scope_flag}"
            )
            continue
        reads_as = "dispatch" if legacy.get("auto_merge") is True else "none"
        typer.echo(
            f"warn: {candidate} sets the deprecated `dispatch.auto_merge`.\n"
            f"      It reads as `auto_merge.grant = \"{reads_as}\"` for one release.\n"
            f"      Migrate: fno config set auto_merge.grant {reads_as}{scope_flag} && "
            f"fno config unset dispatch.auto_merge{scope_flag}"
        )


@app.command("active-backlog")
def active_backlog_cmd(
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit a JSON list of drain targets for the daemon."
    ),
) -> None:
    """Resolve which projects the active-backlog daemon should drain.

    Reads config.active_backlog + the workspace project->path map and prints the
    enabled drain targets (project, cwd, interval, failure_limit, mission). The
    daemon shells this on entering Serving to discover its targets. Read-only and
    best-effort: a malformed config yields an empty list, never an error.
    """
    import json as _json

    from fno.active_backlog import drain_targets_as_dicts

    targets = drain_targets_as_dicts()
    if json_out:
        typer.echo(_json.dumps(targets))
        return
    if not targets:
        typer.echo("active-backlog: no active missions to drain")
        return
    for t in targets:
        mission = f" mission={t['mission']}" if t["mission"] else ""
        typer.echo(
            f"{t['project']}\t{t['cwd']}\tinterval={t['interval_seconds']}s\t"
            f"failure_limit={t['failure_limit']}{mission}"
        )


@app.command("status-sinks")
def status_sinks_cmd(
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit a JSON list of fanout targets for the daemon."
    ),
) -> None:
    """Resolve which projects the status-fanout supervisor should tick (x-2057).

    A project is a target when it has >=1 enabled ``status_sinks`` entry -
    INDEPENDENT of ``config.active_backlog``. The daemon shells this to discover
    its fanout loops. Read-only and best-effort: a malformed config yields an
    empty list, never an error.
    """
    import json as _json

    from fno.active_backlog import fanout_targets_as_dicts

    targets = fanout_targets_as_dicts()
    if json_out:
        typer.echo(_json.dumps(targets))
        return
    if not targets:
        typer.echo("status-sinks: no projects with enabled sinks")
        return
    for t in targets:
        typer.echo(f"{t['project']}\t{t['cwd']}\tinterval={t['interval_seconds']}s")


@app.command("assert-subagent-budget", hidden=True)
def assert_subagent_budget_cmd(
    width: int = typer.Option(..., "--width", "-w", help="The fan-out width the caller declares."),
    provider: Optional[str] = typer.Option(
        None,
        "--provider",
        "-P",
        help="Provider whose budget applies; defaults to the FNO_ROUTE_PROVIDER stamp.",
    ),
) -> None:
    """Refuse a declared fan-out wider than the provider's subagent budget.

    Prints the verdict's reason and exits 0 on a permit, 1 on a refusal.
    Fails open: no stamp, no budget entry or an unreadable config permits,
    with the reason saying so. This is the seam skill text calls before
    declaring a panel width (x-25a7 Locked Decision 7).
    """
    from fno.config import assert_subagent_budget

    check = assert_subagent_budget(width, provider)
    prefix = "permit" if check.permitted else "refused"
    typer.echo(f"{prefix}: {check.reason}")
    raise typer.Exit(0 if check.permitted else 1)


@app.command("get")
def get_cmd(
    key: str = typer.Argument(
        ...,
        help="Dotted config key, e.g. config.blueprint.max_prs_per_epic",
    ),
    json_out: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit {key, value, source, overrides} as one JSON object on stdout.",
    ),
) -> None:
    """Print a single resolved config value. Read-only.

    Traverses the loaded settings model by dotted path so a skill / LLM
    caller can read one value (e.g. the decomposition ceiling fallback)
    without re-implementing settings lookup. Scalars print bare; nested
    objects print as JSON. Unknown keys exit non-zero.

    The leading ``config.`` is optional: a bare ``review.required_bots`` is
    retried as ``config.review.required_bots`` so a caller need not remember
    the redundant prefix (x-8b64 E: the review gate defaults to
    ``config.review.required_bots`` but the shorthand used to error).

    Which FILE decided the value prints on STDERR (x-4be1): the silent
    home-vs-project override is the defect this fixes, so the resolved value
    alone on stdout would keep the confusion. stdout stays value-only because
    callers pipe it (normalize.sh compares the whole stream); the source line,
    including an ``overrides`` clause exactly when a lower-precedence file
    also sets the key, is stderr-only. ``--json`` carries both streams' facts
    as one object.
    """
    import json
    import os
    import sys

    from fno.config import describe_settings_for_repo, load_settings, resolve_source
    from fno.paths import resolve_repo_root
    from pydantic import BaseModel

    root = load_settings()
    # The receipt must describe the chain that PRODUCED the value.
    # load_settings() reads a pinned FNO_CONFIG through the env branch, which
    # short-circuits the repo chain, so provenance seeded at the repo root
    # would name files that did not decide the key.
    pinned_config = os.environ.get("FNO_CONFIG")
    settings_root = (
        Path(pinned_config).resolve().parent
        if pinned_config
        else resolve_repo_root().resolve()
    )
    provenance_root: Optional[Path] = None if pinned_config else settings_root
    searched_candidates = describe_settings_for_repo(provenance_root)

    def _traverse(dotted: str) -> tuple[bool, object]:
        node: object = root
        for part in dotted.split("."):
            if isinstance(node, BaseModel) and part in type(node).model_fields:
                node = getattr(node, part)
            elif isinstance(node, dict) and part in node:
                node = node[part]
            else:
                return (False, None)
        return (True, node)

    ok, node = _traverse(key)
    if not ok and key.startswith("config."):
        # The model is flat now (config fields at the top level); a legacy
        # `config.`-prefixed key resolves once the prefix is dropped.
        ok, node = _traverse(key[len("config.") :])
    if not ok and not key.startswith("config."):
        ok, node = _traverse(f"config.{key}")
    if not ok and (
        key.startswith("providers.")
        or key.startswith("config.providers.")
        or key == "providers"
        or key == "config.providers"
    ):
        aliased = key.replace("providers", "accounts", 1)
        ok, node = _traverse(aliased)
        if not ok and aliased.startswith("config."):
            ok, node = _traverse(aliased[len("config.") :])
        if not ok and not aliased.startswith("config."):
            ok, node = _traverse(f"config.{aliased}")
    if not ok:
        typer.echo(f"error: unknown config key '{key}'", file=sys.stderr)
        raise typer.Exit(code=1)

    # Provenance is per LEAF: a block get merges leaves from several files, so
    # no single file "decided" it (project auto_merge.enabled + global
    # auto_merge.merge_strategy both live in the one resolved block). Attributing
    # the block to one file would re-create the home-vs-project confusion this
    # command exists to end, just with the block as the lie.
    is_leaf = not isinstance(node, (BaseModel, dict))
    if is_leaf:
        source = resolve_source(key, root=provenance_root)
        if source is None and (
            key.startswith("providers.")
            or key.startswith("config.providers.")
            or key == "providers"
            or key == "config.providers"
        ):
            # The value resolved through the rename; provenance must too, or a
            # file that DOES set the key reports "source: default".
            source = resolve_source(
                key.replace("providers", "accounts", 1), root=provenance_root
            )
        if source is not None:
            decider, overridden = source
        else:
            decider, overridden = None, []
    else:
        decider, overridden = None, []

    if json_out:
        # model_dump(mode="json") is the JSON-ready form in one step; scalars
        # and containers pass through unchanged.
        value: object = (
            node.model_dump(mode="json")
            if isinstance(node, BaseModel)
            else _jsonable_container(node)
        )
        typer.echo(
            json.dumps(
                {
                    "key": key,
                    "value": value,
                    "source": str(decider) if decider else None,
                    "overrides": [str(p) for p in overridden],
                    "root": str(settings_root),
                    "searched": [str(p) for p in searched_candidates],
                },
                default=str,
            )
        )
        return

    if is_leaf:
        source_line = f"source: {decider}" if decider else "source: default (no config file sets this key)"
        if overridden:
            source_line += " (overrides " + ", ".join(str(p) for p in overridden) + ")"
    else:
        source_line = "source: mixed - a block merges leaves from several files; query a leaf (e.g. auto_merge.enabled) for its decider"
    searched = ", ".join(str(path) for path in searched_candidates) or "<none>"
    source_line += f" (root: {settings_root}; searched: {searched})"

    if isinstance(node, BaseModel):
        typer.echo(node.model_dump_json())
    elif isinstance(node, (dict, list)):
        # A container can now hold RECORDS (`agents.provider_limits` maps a provider
        # to a ProviderBudget), and `default=str` renders one as its repr. A
        # reader cannot paste that back into config.toml, so dump the record.
        typer.echo(
            json.dumps(
                node,
                default=lambda v: v.model_dump() if isinstance(v, BaseModel) else str(v),
            )
        )
    else:
        typer.echo("" if node is None else str(node))
    typer.echo(source_line, file=sys.stderr)


@app.command("set")
def set_cmd(
    tokens: list[str] = typer.Argument(
        ...,
        help="Either `<key> <value>` (single set; value may contain '=') or "
        "one-or-more `key=value` pairs (atomic multi-key set).",
    ),
    local: bool = typer.Option(
        False,
        "--local/--global",
        "-l/-g",
        help="Write the project-local .fno/config.toml instead of the "
        "per-user global ~/.fno/config.toml (default global).",
    ),
) -> None:
    """Set one or more config keys in config.toml (atomic, schema-validated).

    Two forms:

      fno config set <key> <value>        # single key (value may contain '=')
      fno config set a.b=1 c.d=2 ...       # atomic multi-key set

    Each value is coerced to the field's type and validated against the schema
    (e.g. ``config.agents.a2a.turn_ceiling`` must be >= 1), then written
    atomically under a single file lock. In the multi-key form the batch is
    all-or-nothing: if ANY value is invalid the file is left unchanged and the
    command exits non-zero (AC2-ERR / AC2-FR). A key repeated in one call uses
    the last value (AC2-EDGE).
    """
    import sys

    from fno.claims.optout_lease import ConfigSetError, set_config_values
    from fno.config.optouts import optout_release_command

    scope = "project" if local else "global"

    # Disambiguate the single-key `<key> <value>` form (so a value may itself
    # contain '=') from the multi-key `key=value` form: exactly two tokens whose
    # first carries no '=' is the legacy single set; otherwise every token must
    # be a key=value pair.
    if len(tokens) == 2 and "=" not in tokens[0]:
        items = [(tokens[0], tokens[1])]
    else:
        items = []
        for tok in tokens:
            if "=" not in tok:
                typer.echo(
                    f"error: expected key=value, got {tok!r}. Use "
                    "`fno config set <key> <value>` for a single key.",
                    file=sys.stderr,
                )
                raise typer.Exit(code=2)
            k, _, v = tok.partition("=")
            items.append((k, v))

    # A dangling accounts.active bricks every loader call (and every accounts
    # verb loads first), so the pointer is validated against declared record
    # ids here rather than repaired after the fact. Scope-aware: a GLOBAL
    # pointer must name a globally declared record (a local-only id would
    # brick every other project), while a project pointer may name anything
    # the merged view resolves.
    for k, v in items:
        normalized = k[len("config."):] if k.startswith("config.") else k
        if normalized in ("accounts.active", "providers.active"):
            from fno.adapters.providers.loader import (
                known_account_ids,
                load_scope_config,
            )

            if scope == "global":
                ids = {r.id for r in load_scope_config("global").records}
            else:
                ids = known_account_ids()
            if v not in ids:
                known = ", ".join(sorted(ids)) if ids else "(no records configured)"
                typer.echo(
                    f"error: account {v!r} is not a configured record id "
                    f"(known: {known}); add the record first",
                    file=sys.stderr,
                )
                raise typer.Exit(code=1)

    try:
        results = set_config_values(items, scope=scope)
    except ConfigSetError as exc:
        typer.echo(f"error: {exc}", file=sys.stderr)
        raise typer.Exit(code=exc.exit_code) from exc

    if len(results) == 1:
        r = results[0]
        typer.echo(f"set {r.key} = {r.value} ({r.scope}: {r.path})")
    else:
        for r in results:
            typer.echo(f"set {r.key} = {r.value}")
        # Scope + path printed once (AC2-UI).
        typer.echo(f"({results[0].scope}: {results[0].path})")

    for r in results:
        if r.lease:
            scope_flag = " --local" if r.scope == "project" else ""
            release = optout_release_command(r.key, scope_flag)
            typer.echo(
                f"lease: {r.key} held by {r.lease['holder']}, expires at "
                f"{r.lease['expires_at']}; release: {release}"
            )

    _check_overridden_writes(results)

    # x-e106: setting pr_watch.enabled couples to the launchd agent so enabled
    # means running. Loud on failure, never reverts config (doctor is the guard).
    for r in results:
        if r.key.endswith("pr_watch.enabled"):
            _couple_pr_watch(bool(r.value))
            break


def _check_overridden_writes(results: list) -> None:
    """Warn on stderr when a write succeeded on disk but a higher-precedence
    configuration layer overrides it (x-389d).

    Receipt defect fix: local-over-global precedence is correct and stays, but
    `fno config set` must not silently report success on a write that is inert in
    the current project context. Stays silent when the write takes effect or when
    no higher layer overrides it.
    """
    import sys

    from pydantic import BaseModel

    from fno.config import load_settings, resolve_source

    load_settings.cache_clear()
    root = load_settings()

    def _traverse(dotted: str) -> tuple[bool, object]:
        node: object = root
        for part in dotted.split("."):
            if isinstance(node, BaseModel) and part in type(node).model_fields:
                node = getattr(node, part)
            elif isinstance(node, dict) and part in node:
                node = node[part]
            else:
                return (False, None)
        return (True, node)

    for r in results:
        source = resolve_source(r.key)
        if source is None:
            continue
        decider, _ = source
        try:
            is_same_file = decider.resolve() == r.path.resolve()
        except OSError:
            is_same_file = decider == r.path
        if not is_same_file:
            ok, node = _traverse(r.key)
            if not ok and r.key.startswith("config."):
                ok, node = _traverse(r.key[len("config.") :])
            if not ok and not r.key.startswith("config."):
                ok, node = _traverse(f"config.{r.key}")
            effective_value = node if ok else None
            if effective_value != r.value:
                target_flag = "--local" if r.scope == "global" else "--global"
                typer.echo(
                    f"warn: {r.key} set in {r.path}, but {decider} overrides it "
                    f"(value in effect: {effective_value}). Target the winning file with {target_flag}.",
                    file=sys.stderr,
                )


def _couple_pr_watch(enabled: bool) -> None:
    """Install+load (or unload) the PR-watch agent to match pr_watch.enabled."""
    import sys

    try:
        from fno.pr_watch.cli import deactivate_watcher, ensure_watcher_activated
    except Exception as exc:  # noqa: BLE001 - coupling is best-effort
        typer.echo(f"pr-watch coupling unavailable: {exc}", file=sys.stderr)
        return

    if enabled:
        outcome = ensure_watcher_activated()
        if outcome == "activated":
            typer.echo("pr-watch: agent installed and loaded.")
        elif outcome == "already-running":
            typer.echo("pr-watch: agent already running.")
        else:
            # AC1-ERR: activation failed but enable stuck; surface loudly.
            typer.echo(
                f"pr-watch: WARNING enabled but activation failed ({outcome}); "
                "config stays enabled. Run `fno do pr watch install` or check `fno doctor`.",
                file=sys.stderr,
            )
    else:
        outcome = deactivate_watcher()
        if outcome == "unload-failed":
            # Inverse of the found incident: config says disabled but the agent
            # keeps ticking. Surface as loudly as the enable path - doctor's
            # liveness line stays silent once enabled=false, so this is the only
            # signal that the watcher did not actually stop.
            typer.echo(
                "pr-watch: WARNING disable set but launchctl unload failed; the "
                "agent may still be running. Check whether it is "
                "(`launchctl list | grep sh.fno.pr-watcher`), then retry the "
                "unload: `launchctl unload "
                "~/Library/LaunchAgents/sh.fno.pr-watcher.plist` "
                "(or re-run this config set, which re-fires the unload). "
                "`fno do pr watch uninstall` removes the watcher "
                "entirely - nothing measured is lost (the watermark store "
                "survives; `fno do pr watch install` restores it) - but run it "
                "only when removal is the intent, not to silence this warning.",
                file=sys.stderr,
            )
        else:
            typer.echo(f"pr-watch: agent {outcome} (disabled).")


@app.command("unset")
def unset_cmd(
    key: str = typer.Argument(
        ..., help="Dotted config key to remove, e.g. config.auto_merge.enabled"
    ),
    local: bool = typer.Option(
        False,
        "--local/--global",
        "-l/-g",
        help="Remove from the project-local .fno/config.toml instead of the "
        "per-user global ~/.fno/config.toml (default global).",
    ),
) -> None:
    """Remove a config key, reverting it to the model default.

    The undo of ``set``: deletes the dotted key (and prunes any block the
    removal leaves empty), so the value falls back to its schema default. Since
    the revert is non-destructive there is no confirmation. An unknown key exits
    1 and changes nothing; an absent key is a clean no-op (``not set: <key>``).
    Aliased as ``fno config rm``.
    """
    import sys

    from fno.claims.optout_lease import ConfigSetError, unset_config_value

    scope = "project" if local else "global"
    try:
        result = unset_config_value(key, scope=scope)
    except ConfigSetError as exc:
        typer.echo(f"error: {exc}", file=sys.stderr)
        raise typer.Exit(code=exc.exit_code) from exc

    if not result.present:
        typer.echo(f"not set: {key}")
        raise typer.Exit(0)

    typer.echo(
        f"unset {result.key} (was {result.was}); now defaults to "
        f"{result.default} ({result.scope}: {result.path})"
    )


# `fno config rm` is an alias for `unset` (Claude's Discretion #3).
app.command("rm")(unset_cmd)


def _reversed_lines(path: Path, chunk: int = 65536):
    """Yield the journal's lines newest-first without loading the whole file.

    events.jsonl has no size bound, so a full read per `config history` call
    would pay for a lifetime of receipts to return `--limit` rows.
    """
    with path.open("rb") as handle:
        handle.seek(0, os.SEEK_END)
        position = handle.tell()
        tail = b""
        while position > 0:
            step = min(chunk, position)
            position -= step
            handle.seek(position)
            lines = (handle.read(step) + tail).split(b"\n")
            tail = lines[0]
            for line in reversed(lines[1:]):
                yield line
        if tail.strip():
            yield tail


@app.command("history", hidden=True)
def history(
    key: Optional[str] = typer.Argument(
        None,
        help="Exact config key or dotted prefix to filter.",
    ),
    limit: int = typer.Option(50, "--limit", min=1, help="Maximum rows to print."),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit matching rows as JSONL."),
    scope: Literal["global", "project", "all"] = typer.Option(
        "all", "--scope", help="Limit rows by the config scope that was written."
    ),
) -> None:
    """Read config-write receipts from the global and project journals."""
    import json

    from fno.paths import global_events_json, project_events_json

    journal_paths = [global_events_json(), project_events_json()]
    rows: list[dict[str, Any]] = []
    for journal_path in journal_paths:
        # Bounded per file: the newest `limit` matching rows of one journal
        # cover every row that file can contribute to the overall top-`limit`,
        # because every filter here is per-row.
        try:
            matched = 0
            for line in _reversed_lines(journal_path):
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(row, dict) or row.get("type") != "config_write":
                    continue
                data = row.get("data")
                if not isinstance(data, dict) or not isinstance(data.get("key"), str):
                    continue
                if scope != "all" and data.get("scope") != scope:
                    continue
                if key and data["key"] != key and not data["key"].startswith(f"{key}."):
                    continue
                rows.append(row)
                matched += 1
                if matched >= limit:
                    break
        except OSError:
            continue

    rows.sort(key=lambda row: str(row.get("ts", "")), reverse=True)
    rows = rows[:limit]
    if not rows:
        typer.echo(
            "no config_write rows; searched journals: "
            + ", ".join(str(path) for path in journal_paths)
        )
        return

    if json_out:
        for row in rows:
            typer.echo(json.dumps(row, ensure_ascii=False, separators=(",", ":")))
        return

    def display_value(data: dict[str, Any], field: str, presence: str) -> str:
        if not data.get(presence):
            return "(unset)"
        value = data.get(field)
        if isinstance(value, str):
            return value
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"), default=str)

    for row in rows:
        data = row["data"]
        old_value = display_value(data, "old_value", "present_before")
        new_value = display_value(data, "new_value", "present_after")
        attester = data.get("attester_session_id") or "(none)"
        witness = data.get("attester_witness") or "(unknown)"
        typer.echo(
            f"{row.get('ts', '')} {data['key']} {old_value} -> {new_value} "
            f"{data.get('scope', '(unknown)')}/{data.get('root_kind', '(unknown)')} "
            f"{data.get('config_path', '(unknown)')} session {attester} ({witness})"
        )


@app.command("schema")
def schema(
    json_schema: bool = typer.Option(
        False, "--json-schema", help="Emit the model's JSON Schema."
    ),
    markdown: bool = typer.Option(
        False, "--markdown", help="Emit the COMPLETE settings reference as Markdown."
    ),
    wizard_plan: bool = typer.Option(
        False, "--wizard-plan", help="Emit the wizard-asked fields as JSON."
    ),
    toml: bool = typer.Option(
        False, "--toml", help="Emit a commented example config.toml (every key at its default)."
    ),
    write: bool = typer.Option(
        False, "--write", help="With --markdown/--toml: regenerate the committed reference file."
    ),
    check: bool = typer.Option(
        False, "--check", help="With --markdown/--toml: exit non-zero if the committed file differs."
    ),
) -> None:
    """Generate config artifacts from the model + registry.

    Exactly one of --json-schema / --markdown / --wizard-plan selects the
    output; --markdown is the default. --write regenerates the docs file
    atomically (temp + replace, never truncating on error); --check compares
    the freshly generated docs against the committed file and exits 2 on drift.
    """
    import os
    import sys
    import tempfile

    from fno.config import schema_gen

    selected = sum([json_schema, markdown, wizard_plan, toml])
    if selected > 1:
        typer.echo(
            "error: pick at most one of --json-schema / --markdown / --wizard-plan / --toml",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    if json_schema:
        typer.echo(schema_gen.json_schema())
        return
    if wizard_plan:
        typer.echo(schema_gen.wizard_plan())
        return

    # --toml selects the example file; --markdown (default) selects the guide.
    # Both share the write/check/echo plumbing below.
    if toml:
        rendered = schema_gen.render_example_toml()
        target = _repo_root() / "docs" / "config.example.toml"
        regen = "fno config schema --toml --write"
    else:
        rendered = schema_gen.render_markdown()
        target = _repo_root() / "docs" / "configuration-guide.md"
        regen = "fno config schema --markdown --write"

    if check:
        try:
            current = target.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            # Unreadable or non-UTF-8 committed file -> treat as stale (differs
            # from the freshly rendered text), prompting a regenerate.
            current = None
        if current != rendered:
            typer.echo(
                f"error: {target} is stale; run `{regen}`",
                file=sys.stderr,
            )
            raise typer.Exit(code=2)
        typer.echo(f"{target} is up to date")
        return

    if write:
        target.parent.mkdir(parents=True, exist_ok=True)
        # Atomic temp + replace so a write error never truncates the committed
        # file (AC5-FR). Write to a temp in the same dir, then os.replace.
        fd, tmp = tempfile.mkstemp(dir=str(target.parent), suffix=".tmp")
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                fh.write(rendered)
            os.replace(tmp, target)
        except Exception:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise
        typer.echo(f"wrote {target}")
        return

    typer.echo(rendered)
