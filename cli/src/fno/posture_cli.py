"""``fno posture``: write the attended/autonomous stance as ordinary config keys.

A GENERATOR, never a layer. ``apply`` writes real keys through the shared
atomic ``fno config set`` writer and exits, so afterwards there is exactly one
source of truth: the same file you would have hand-edited. It mirrors ``fno
route set`` (atomic, schema-validated, refuse-before-write).

It is deliberately NOT a config layer read at resolve time. A ``posture`` key
read during resolution would reproduce the deprecated ``use_conductor_canonical``
precedence problem this repo already had to explain in its worktrees rule: a
stance that overrides effective config from a second source, silently and
without a line in the file an operator would edit.

The applied-posture stamp (``posture.json`` under the state dir) is ADVISORY
provenance that ``fno doctor`` may display; no resolver reads it. Keeping it in
a side file rather than a config key is exactly what prevents it from becoming a
layer. Scope is posture keys only: the autonomy/irreversibility levers. Worktree
policy, paths, and the reviewer registry are per-project facts, not stances, and
stay out.
"""
from __future__ import annotations

import json
from datetime import datetime, timezone
from typing import Any

import typer

posture_app = typer.Typer(
    name="posture",
    help="Apply the attended/autonomous stance as ordinary config keys (generator).",
    no_args_is_help=True,
)


@posture_app.callback()
def _posture_root() -> None:
    """Apply the attended/autonomous stance as ordinary config keys.

    A generator, never a layer: ``apply`` writes ordinary config keys via the
    shared atomic writer and exits, leaving one source of truth. See ``apply --help``.
    """

# The stance levers: keys whose effective value decides whether the fleet acts
# on its own (merges, drains) or waits for the operator. Deliberately small and
# named: a posture is a stance over irreversible-action authority, not a
# grab-bag for every knob. A key belongs here only if "silently doing X
# unattended" is the behavior it gates, which is also the rule ``fno doctor``
# reports under (a default-on switch that can silently take an irreversible
# action owes a doctor line).
POSTURE_KEYS: dict[str, dict[str, str]] = {
    # attended: a human is adjudicating. No autonomous merge, no autonomous drain.
    "attended": {
        "auto_merge.enabled": "false",
        "dispatch.auto_merge": "false",
        "active_backlog.enabled": "false",
    },
    # autonomous: the fleet runs unattended. Merge authority armed, drain on.
    "autonomous": {
        "auto_merge.enabled": "true",
        "dispatch.auto_merge": "true",
        "active_backlog.enabled": "true",
    },
}


def _stamp_path() -> Any:
    from fno import paths

    return paths.state_dir() / "posture.json"


def _write_stamp(posture: str, scope: str, keys: dict[str, str]) -> None:
    """Advisory provenance only. Best-effort: a failed write warns, never aborts.

    NOT read by config resolution; it exists so ``fno doctor`` can report
    'posture: autonomous (applied ...)' beside the switch lines. A side file
    (not a config key) is what keeps it from becoming a resolve-time layer.
    """
    stamp = {
        "posture": posture,
        "scope": scope,
        "applied_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "keys": sorted(keys),
    }
    try:
        path = _stamp_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(stamp, indent=2, sort_keys=True) + "\n")
    except OSError as exc:
        typer.echo(f"posture: warning: could not write provenance stamp: {exc}", err=True)


def _project_shadowed_keys(keys: dict[str, str]) -> list[str]:
    """Posture keys the project-local config already defines.

    A global write would be silently overridden at resolve time: project-local
    config outranks the per-user global (config precedence, highest first, has
    the worktree then canonical project files above ``~/.fno``). The merged
    Settings collapse the source scope and so cannot tell a global value from a
    project one, so this reads the raw project file instead. Returns the dotted
    key names that are present; empty when there is no project config or it sets
    none of the stance levers.
    """
    try:
        from fno.config_io import _load_raw, _unwrap_config_dict
        from fno.paths import resolve_repo_root

        root = resolve_repo_root()
        for name in ("config.toml", "settings.yaml"):
            path = root / ".fno" / name
            if path.exists():
                raw, ok = _load_raw(path)
                if ok and raw:
                    data = _unwrap_config_dict(raw)
                    break
        else:
            return []
    except Exception:  # noqa: BLE001 - unreadable/absent project config is not posture's alarm
        return []

    shadowed: list[str] = []
    for dotted in keys:
        parts = dotted.split(".")
        node: object = data
        for p in parts[:-1]:
            if not isinstance(node, dict) or p not in node:
                node = None
                break
            node = node[p]
        if isinstance(node, dict) and parts[-1] in node:
            shadowed.append(dotted)
    return shadowed


@posture_app.command("apply")
def apply_cmd(
    posture: str = typer.Argument(
        ...,
        help="attended (human adjudicates; merge/drain off) or autonomous "
        "(fleet runs unattended; merge/drain on).",
    ),
    local: bool = typer.Option(
        False,
        "--local/--global",
        "-l/-g",
        help="Write the project-local config.toml instead of the per-user "
        "global one (default global; a stance is operator-level).",
    ),
) -> None:
    """Write the stance's keys atomically and exit (one source of truth).

    Mirrors ``fno route set``: refuse an unknown posture BEFORE any write, then
    delegate to the shared atomic ``fno config set`` writer. There is no
    posture-specific resolution path afterwards; the keys are ordinary config.
    """
    name = posture.strip().lower()
    keys = POSTURE_KEYS.get(name)
    if keys is None:
        typer.echo(
            f"error: unknown posture {posture!r}; choose attended or autonomous. "
            "Config unchanged.",
            err=True,
        )
        raise typer.Exit(2)

    from fno.config.writer import ConfigSetError, set_config_values

    scope = "project" if local else "global"
    # A global write is silently shadowed when the project config already pins a
    # stance lever: project-local outranks global at resolve time, so the printed
    # success would describe a value the running fleet never sees. Warn (do not
    # refuse) naming the keys the operator must move to --local to actually flip.
    if not local:
        shadowed = _project_shadowed_keys(keys)
        if shadowed:
            typer.echo(
                "warning: project-local config already defines "
                + ", ".join(sorted(shadowed))
                + ", which override this global write at resolve time. "
                "Re-run with --local to change the effective value.",
                err=True,
            )
    try:
        set_config_values(list(keys.items()), scope=scope)
    except ConfigSetError as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(exc.exit_code) from exc

    _write_stamp(name, scope, keys)
    pairs = ", ".join(f"{k}={v}" for k, v in keys.items())
    typer.echo(f"posture apply {name} ({scope}): {pairs}")
