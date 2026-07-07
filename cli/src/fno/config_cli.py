"""CLI surface for config commands (`fno config ...`).

Lives next to paths_cli.py and setup_cli.py for consistency; implementation
lives in setup/doctor.py.
"""
from __future__ import annotations

from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Literal, Optional, Union

import typer

app = typer.Typer(help="Config inspection and diagnostics")


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
                "prose+triage will be skipped. Set it with: fno setup post-merge"
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
    import yaml

    from fno.config import PostMergeBlock, ProjectBlock

    settings_path = repo_root / ".fno" / "settings.yaml"
    raw: dict = {}
    if settings_path.is_file():
        parsed = yaml.safe_load(settings_path.read_text(encoding="utf-8"))
        if parsed is not None:
            if not isinstance(parsed, dict):
                raise ValueError(f"settings.yaml is not a mapping: {settings_path}")
            raw = parsed

    # Per-worktree local override (x-cbce): layer the allowlisted keys
    # (parking_lot_path, project.id) so this oracle agrees with load_settings()
    # and `fno config get`. Repo-local only (the local file is never symlinked
    # to canonical), which preserves the "a global parking_lot_path must not make
    # every repo look ready" guard above - the override is still repo-scoped.
    local_path = repo_root / ".fno" / "settings.local.yaml"
    if local_path.is_file() and not local_path.is_symlink():
        from fno.config import _deep_merge, _load_raw, _worktree_local_override

        local_parsed, ok = _load_raw(local_path)
        if ok:
            override = _worktree_local_override(local_parsed)
            if override:
                raw = _deep_merge(raw, override)

    if not raw:
        return PostMergeBlock(), None

    config = raw.get("config")
    config = config if isinstance(config, dict) else {}
    pm_raw = config.get("post_merge")
    pm_raw = pm_raw if isinstance(pm_raw, dict) else {}
    pm = PostMergeBlock.model_validate(pm_raw)  # raises on an invalid post_merge value

    project_id = None
    for candidate in (config.get("project"), raw.get("project")):
        if candidate is None:
            continue
        try:
            pid = ProjectBlock.model_validate(candidate).id
        except Exception:  # noqa: BLE001 - project.id is non-blocking; degrade to None
            pid = None
        if pid:
            project_id = pid
            break
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


@app.command("doctor")
def doctor_cmd(
    post_merge: bool = typer.Option(
        False,
        "--post-merge",
        help="Report this repo's post-merge config readiness (read-only).",
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
    /fno:pr merged ritual needs. Bare ``fno config doctor`` runs the path
    diagnostic and appends a one-line post-merge readiness summary.
    """
    import json as _json

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
    raise typer.Exit(rc)


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
        typer.echo("active-backlog: disabled (no enabled projects)")
        return
    for t in targets:
        mission = f" mission={t['mission']}" if t["mission"] else ""
        typer.echo(
            f"{t['project']}\t{t['cwd']}\tinterval={t['interval_seconds']}s\t"
            f"failure_limit={t['failure_limit']}{mission}"
        )


@app.command("get")
def get_cmd(
    key: str = typer.Argument(
        ...,
        help="Dotted config key, e.g. config.blueprint.max_prs_per_epic",
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
    """
    import json
    import sys

    from fno.config import load_settings
    from pydantic import BaseModel

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

    ok, node = _traverse(key)
    if not ok and not key.startswith("config."):
        ok, node = _traverse(f"config.{key}")
    if not ok:
        typer.echo(f"error: unknown config key '{key}'", file=sys.stderr)
        raise typer.Exit(code=1)

    if isinstance(node, BaseModel):
        typer.echo(node.model_dump_json())
    elif isinstance(node, (dict, list)):
        typer.echo(json.dumps(node, default=str))
    else:
        typer.echo("" if node is None else str(node))


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
        help="Write the project-local .fno/settings.yaml instead of the "
        "per-user global ~/.fno/settings.yaml (default global).",
    ),
) -> None:
    """Set one or more config keys in settings.yaml (atomic, schema-validated).

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

    from fno.config.writer import ConfigSetError, set_config_values

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

    # x-e106: setting pr_watch.enabled couples to the launchd agent so enabled
    # means running. Loud on failure, never reverts config (doctor is the guard).
    for r in results:
        if r.key.endswith("pr_watch.enabled"):
            _couple_pr_watch(bool(r.value))
            break


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
                "config stays enabled. Run `fno pr-watch install` or check `fno doctor`.",
                file=sys.stderr,
            )
    else:
        outcome = deactivate_watcher()
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
        help="Remove from the project-local .fno/settings.yaml instead of the "
        "per-user global ~/.fno/settings.yaml (default global).",
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

    from fno.config.writer import ConfigSetError, unset_config_value

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
    yaml: bool = typer.Option(
        False, "--yaml", help="Emit a commented example settings.yaml (every key at its default)."
    ),
    write: bool = typer.Option(
        False, "--write", help="With --markdown/--yaml: regenerate the committed reference file."
    ),
    check: bool = typer.Option(
        False, "--check", help="With --markdown/--yaml: exit non-zero if the committed file differs."
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

    selected = sum([json_schema, markdown, wizard_plan, yaml])
    if selected > 1:
        typer.echo(
            "error: pick at most one of --json-schema / --markdown / --wizard-plan / --yaml",
            file=sys.stderr,
        )
        raise typer.Exit(code=2)

    if json_schema:
        typer.echo(schema_gen.json_schema())
        return
    if wizard_plan:
        typer.echo(schema_gen.wizard_plan())
        return

    # --yaml selects the example file; --markdown (default) selects the guide.
    # Both share the write/check/echo plumbing below.
    if yaml:
        rendered = schema_gen.render_example_yaml()
        target = _repo_root() / "docs" / "settings.example.yaml"
        regen = "fno config schema --yaml --write"
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
