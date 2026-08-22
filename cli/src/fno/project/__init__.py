"""``fno project`` - mint an isolated fno environment for one project.

Isolation is not invented here. A project-local ``<repo>/.fno/config.toml``
already wins per key and deep-merges over the per-user global, and every
``paths.*`` derives from one ``state_dir``, so writing that one key moves the
graph, the ledger, the briefs, the agent registry and the mail bus together.
``fno project init`` writes that file through the same writer ``fno config set``
uses; it adds no second resolution path.

``FNO_CONFIG`` is deliberately not the lever: it is the ONLY candidate when set,
so it discards the operator's global defaults rather than overriding one key.

The receipt is the other half of the deliverable. There are two layers and only
one is fno's. Layer 1 is fno's own data, which ``state_dir`` moves. Layer 2 is
the harness substrate - one claude daemon namespace per box, plus codex
app-servers owned by ChatGPT.app and a VS Code extension - which fno never
spawned and cannot move. A receipt that says "isolated environment" is true
about data and false about identity, so this one says which half it delivered.

See ``docs/architecture/fno-project-environments.md``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Optional

import typer

project_app = typer.Typer(
    name="project",
    help="Isolated per-project fno environments.",
    no_args_is_help=True,
    add_completion=False,
    rich_markup_mode=None,
)

# Where an isolated state root lives. Absolute, already inside the state root
# `fno config doctor` reports, and needs no new rule. In-repo placement would
# need a gitignore entry and can be committed by accident, which puts a demo
# graph in a public history; a sibling of the repo has no discovery path.
PROJECTS_ROOT = "~/.fno/projects"


@project_app.callback()
def _group() -> None:
    """Keep `project` a GROUP, so the verb is `fno project init <id>`.

    Typer collapses a single-command app into that command, which would make
    the spelling `fno project <id>` and leave no room for a second action.
    """


def _state_dir_for(project_id: str) -> str:
    return f"{PROJECTS_ROOT}/{project_id}"


def _read_project_toml(repo_root: Path) -> dict:
    """The repo's own ``.fno/config.toml``, flat, or ``{}``.

    Read directly rather than through the merged settings: the refusal below is
    about what THIS file already pins, and a merged read cannot tell a
    project-local ``state_dir`` from the global default.
    """
    import tomllib

    path = repo_root / ".fno" / "config.toml"
    try:
        with path.open("rb") as fh:
            data = tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError):
        return {}
    return data if isinstance(data, dict) else {}


@project_app.command("init")
def init(
    project_id: str = typer.Argument(
        ...,
        metavar="ID",
        help="Project id: a lowercase, letter-led token of 1-7 letters/digits. "
        "Names the state root and prefixes every node id minted here.",
    ),
    repo_root: Optional[Path] = typer.Option(
        None,
        "--repo-root",
        help="Write into this checkout instead of the current one.",
    ),
) -> None:
    """Give this checkout its own fno state root, and say which half is isolated.

    Writes ``<repo>/.fno/config.toml`` with ``state_dir`` and
    ``backlog.id_prefix``, creates the root, and prints what moved with it and
    what did not.
    """
    from fno.config.writer import ConfigSetError, set_config_values
    from fno.config import load_settings

    if repo_root is None:
        from fno.paths import resolve_repo_root

        repo_root = resolve_repo_root()
    repo_root = Path(repo_root)

    # Normalize BEFORE anything derives from it. `backlog.id_prefix` is
    # lowercased by its own validator at read time, so an id typed `WEB` mints
    # `web-` nodes while this file says `WEB` and the root sits at
    # `.../projects/WEB`. Worse, `fno project init web` afterwards trips the
    # already-pinned refusal and dead-ends someone who only changed the case.
    project_id = project_id.strip().lower()

    target = _state_dir_for(project_id)

    # Refusal 1: an explicit agents-registry override outlives `state_dir`.
    # `paths.agents_registry_path` is honored BEFORE the state_dir fallback, so
    # an environment minted under it gets an isolated graph and a SHARED
    # roster - a half-isolation nobody would notice until two fleets collided.
    override = load_settings().paths.agents_registry_path
    if override:
        typer.echo(
            f"refusing: config.paths.agents_registry_path is set ({override}).\n"
            "That override is honored ahead of state_dir, so this environment "
            "would get its own graph and share your agent roster.\n"
            "Unset it (`fno config unset paths.agents_registry_path`) and re-run.",
            err=True,
        )
        raise typer.Exit(code=1)

    # Refusal 2: a state_dir already pinned to somewhere else. Overwriting it
    # would silently move a live environment's graph, ledger and mail bus.
    existing = _read_project_toml(repo_root).get("state_dir")
    if isinstance(existing, str) and existing.strip() and existing.strip() != target:
        typer.echo(
            f"refusing: {repo_root / '.fno' / 'config.toml'} already sets\n"
            f"  state_dir = {existing.strip()!r}\n"
            f"and this would change it to {target!r}. Nothing was written.\n"
            "Edit that key yourself if the move is what you want.",
            err=True,
        )
        raise typer.Exit(code=1)

    try:
        set_config_values(
            [("state_dir", target), ("backlog.id_prefix", project_id)],
            scope="project",
            repo_root=repo_root,
        )
    except ConfigSetError as exc:
        typer.echo(f"refusing: {exc}", err=True)
        raise typer.Exit(code=exc.exit_code) from exc

    root = Path(target).expanduser()
    root.mkdir(parents=True, exist_ok=True)

    typer.echo(receipt(project_id, target))


def receipt(project_id: str, state_dir: str) -> str:
    """The success receipt.

    The wording is the deliverable, not decoration. Every layer-2 failure seen
    so far is an environment variable outliving the process that set it: a
    session carrying a harness marker nothing in the fleet spawned, a claim
    holding a pid that belongs to a desktop app, a model override inherited
    from a long-lived daemon. A receipt claiming a clean slate would be read as
    ruling those out, and it cannot.

    See ``docs/architecture/fno-project-environments.md``.
    """
    return (
        f"project {project_id}: isolated\n"
        f"  state_dir  {state_dir}   "
        "(graph, ledger, briefs, agent registry, mail bus)\n"
        "  NOT isolated: the claude daemon namespace and any codex app-server.\n"
        "  Those are machine-global and fno does not own them. A session started here\n"
        "  shares them with your real fleet, so an ambient identity marker crosses this\n"
        "  boundary as if it were not there.\n"
        "  mail: this environment has its OWN bus. Your real sessions cannot reach a\n"
        "  worker here, and a worker here cannot reach them."
    )
