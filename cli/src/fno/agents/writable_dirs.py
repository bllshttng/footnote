"""The one answer to "which directories must a spawned worker be able to write".

fno owns state directories outside the worker's cwd - the claim store, the
graph, the ledger - and every harness sandboxes writes to the cwd by default.
So a worker on a bounded posture silently holds no claim, and
``fno claim status node:<id>`` answers ``free`` while that worker is live. That
is a duplicate-dispatch trap the standing "check the claim first" rule cannot
catch, because the check returns free.

The grant already has a channel: ``--add-dir`` is mapped natively for claude,
codex and agy (:func:`fno.agents.mux_spawn.tier3_pane_tokens`). Nothing computed
a value for it, so every spawn passed nothing. This module computes it.

Operator ruling ``d-926a2b90`` on subject ``worker-writable-dirs``: fno computes
the set per spawn and passes it through the existing cell. It does NOT write
into any harness settings file. A per-spawn grant cannot reach a hand-started
session either way, so ``fno doctor`` carries the advisory half.

The set is by need rather than blanket: ``--add-dir`` is a WRITE grant, and a
blanket list hands a code worker the operator's notes.

**What the state-root grant actually covers, stated rather than implied.**
``--add-dir`` is recursive, so granting the state root grants everything under
it. On the default layout that includes ``worktrees_base``
(``~/.fno/worktrees``), which is every sibling worker's checkout. That is wider
than "the claim store" sounds.

It cannot be narrowed to subdirectories. ``graph.json`` and ``ledger.json`` sit
directly at the state root, and both are written with
``tempfile.mkstemp(dir=path.parent)`` followed by ``os.replace``
(``fno/graph/store.py``). An atomic replace needs write access to the
DIRECTORY, not the file, so a worker that mutates the graph needs the root
itself. Granting the children instead would leave every graph write refused.

A project that does not want the sibling-worktree reach moves them out with
``config.paths.worktrees_base``; the harness-native default already places them
at ``<repo>/.claude/worktrees`` rather than under the state root.
"""

from __future__ import annotations

from pathlib import Path
import sys
from typing import Callable, Iterable, Optional, Sequence

__all__ = ["add_dir_tokens", "worker_writable_dirs"]


ADD_DIR_PROVIDERS = ("claude", "codex", "agy")


def add_dir_tokens(
    provider: str,
    add_dir: Optional[str],
    computed_dirs: Sequence[str] = (),
    *,
    unsupported: Callable[[str], object],
) -> list[str]:
    """``--add-dir`` tokens for one spawn: the caller's explicit grant, then
    fno's computed set (:func:`fno.agents.writable_dirs.worker_writable_dirs`).

    The two halves fail DIFFERENTLY on a provider with no additive grant, and
    that asymmetry is the point. An explicit operator flag keeps the hard
    refusal ``unsupported`` raises - fail-closed is correct for something a
    human typed. The computed set is skipped with one named line on stderr,
    because raising on it would refuse every opencode spawn for a default the
    caller never asked for.

    The explicit grant comes first: it composes with the computed set rather
    than being replaced by it, and ``--add-dir`` is repeatable and additive on
    every provider that maps it.
    """
    supported = provider in ADD_DIR_PROVIDERS
    out: list[str] = []
    if add_dir:
        if supported:
            out += ["--add-dir", add_dir]
        else:
            unsupported("--add-dir")
    if computed_dirs:
        if supported:
            for d in computed_dirs:
                out += ["--add-dir", d]
        else:
            print(
                f"note: no state-root grant on {provider} (its --add-dir cell is "
                "not additive); this worker's claim writes may fail, so "
                "`fno claim status` can report free while it runs",
                file=sys.stderr,
            )
    return out


def _state_roots() -> list[Path]:
    """The fno state directories a worker cannot function without.

    Normally one path (``~/.fno``). Two when they diverge: ``locks_dir`` and the
    global claims root are deliberately config-free ($HOME / ``$FNO_CLAIMS_ROOT``)
    while ``state_dir`` honors ``config.paths.state_dir``, so an override moves one
    and not the other. Granting the root rather than three subdirectories keeps
    this from drifting the moment ``config.paths.*`` moves again.
    """
    out: list[Path] = []
    try:
        from fno.paths import state_dir

        out.append(state_dir())
    except Exception:
        pass
    try:
        from fno.claims.io import claims_dir, global_claims_root

        # The claim store's own root, not the store: a worker creates the
        # per-key lockfiles, and on a fresh machine the store itself.
        out.append(claims_dir(global_claims_root()).parent)
    except Exception:
        pass
    return out


def _vault_root_if_plan_inside(cwd: Path, plan_path: Optional[Path]) -> Optional[Path]:
    """The vault root, but only when this spawn's plan actually lives under it.

    A code worker whose plan sits in the repo gets no vault grant. ``plan_path``
    unset falls back to the configured plan directory for ``cwd`` - the same
    resolution ``plan_writable_args`` already uses on the codex lane - because a
    spawn does not know the node's bound plan yet.
    """
    try:
        from fno.paths import vault_root

        vault = vault_root()
    except Exception:
        return None
    if not vault:
        return None
    probe = plan_path
    if probe is None:
        try:
            from fno.paths import plans_content_dir

            probe = Path(plans_content_dir(project_root=cwd))
        except Exception:
            return None
    try:
        probe.resolve().relative_to(Path(vault).resolve())
    except (ValueError, OSError):
        return None
    return Path(vault)


def worker_writable_dirs(
    cwd: Path,
    *,
    plan_path: Optional[Path] = None,
    foreign_roots: Sequence[Path | str] = (),
) -> list[str]:
    """Absolute, deduplicated, existing-only write grants for a worker spawned at ``cwd``.

    Stable order: state root(s), the vault root when the plan reaches it, then any
    caller-supplied sibling project roots (a multi-repo wave; ``/do`` spawns
    foreign waves with ``--cwd <root>`` and grants nothing for that root today).

    Existing-only: a grant naming a directory that is not there is refused by
    some harnesses and buys nothing on any of them.
    """
    candidates: Iterable[Optional[Path]] = (
        *_state_roots(),
        _vault_root_if_plan_inside(cwd, plan_path),
        *(Path(r) for r in foreign_roots),
    )
    out: list[str] = []
    seen: set[str] = set()
    for cand in candidates:
        if cand is None:
            continue
        try:
            resolved = cand.expanduser().resolve()
        except OSError:
            continue
        if not resolved.is_dir():
            continue
        token = str(resolved)
        if token in seen:
            continue
        seen.add(token)
        out.append(token)
    return out
