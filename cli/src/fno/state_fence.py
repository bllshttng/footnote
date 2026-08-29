"""The source-ahead write fence, as a named mechanism instead of a private helper.

Epic x-3d21 R1: a process running from a source checkout whose schema is ahead
of the deployed binary's must write the PROCESS root, never the operator root.
The failure it exists to stop is fleet-wide and silent: a worktree whose branch
raised a schema constant writes that number into the shared file on its next
ordinary mail send, and every deployed process on the machine degrades until
the branch merges.

WHICH STATE FILES THIS REACHES, and it is fewer than "generalize" suggests.
The fence works by comparing a writer's version against the one already on
disk, so it needs a monotone BUILD version on both sides. Measured on main:

  registry.json    SCHEMA_VERSION, a build version. ARMED (x-665d).
  claims/*.lock    carries a `schema_version` KEY, but it is a per-claim SHAPE
                   discriminator, not a build version: 1 means a pid claim and
                   2 means a pid_unavailable claim, bound to the `pid_unavailable`
                   field by a model validator in fno.claims.types. The two
                   coexist by design and neither supersedes the other, so
                   "on-disk is below mine" is not a raise and comparing them the
                   registry's way would refuse an ordinary refresh of a version-1
                   claim from any source checkout. NOT ARMABLE by this mechanism.
                   `test_claims_schema_version_is_a_shape_discriminator` pins
                   that, so the inference is not made a second time.
  graph.json       bare {"entries": [...]}, no version key at all.
  ledger.json      the same.
  bus messages     a raw messages.jsonl, no version key at all.

For the last three there is nothing to compare. Whether they need some other
staleness guard is a separate question with a different mechanism, and folding
it in here under the word "generalize" would hide an unscoped design decision
inside an enforcement change.

The escape hatch is unchanged and there is deliberately NO bypass flag: point
the checkout at its own store. That works by moving the TARGET, not by
silencing the check, which is the property a blocked worker cannot undo by
reaching for an env var.
"""
from __future__ import annotations

from functools import lru_cache
from pathlib import Path
from typing import Optional


@lru_cache(maxsize=1)
def running_from_source() -> Optional[Path]:
    """The repo root when this fno runs from a checkout, else ``None``.

    Keyed on the MODULE's own path, never on the cwd: a deployed fno invoked
    from inside a worktree is still deployed and must keep its normal write.
    A deployed wheel lives under ``.../site-packages/fno/`` and reaches no
    ``.git`` at any ancestor; source lives at ``<checkout>/cli/src/fno/`` and
    reaches one four levels up.

    A linked worktree's ``.git`` is a FILE, not a directory, so this tests
    existence. ``is_dir()`` would miss every worktree, which is the only place
    a source-ahead process ever runs.

    Zero-argument and cached, and that is correct here rather than the R5
    offence the state-roots lint refuses: the key is ``Path(__file__)``, which
    cannot change within a process, so the cache is keyed on the only input
    there is.
    """
    here = Path(__file__).resolve()
    for parent in list(here.parents)[:6]:
        if (parent / ".git").exists():
            return parent
    return None


def refuse_source_ahead_write(
    *,
    target: Path,
    shared: Path,
    on_disk_version: Optional[int],
    code_version: int,
    source_root: Optional[Path],
    error: type[Exception],
    what: str,
    remedy: str,
) -> None:
    """Refuse to RAISE a shared state file's schema from a source checkout.

    Three conditions gate it, each load-bearing, and they are x-665d's WORKING
    key rather than the one its plan first proposed. That plan's first
    condition (``explicit_path = path is not None``) shipped INERT, because the
    only caller resolves the default itself; the condition that actually works
    is the resolved TARGET compared against the shared resolver's answer. The
    caller passes ``shared`` from ``fno.paths.STATE_FILES``, which is the point
    of that table.

    - the target IS the process-global file (or lies inside the process-global
      directory). A named store is nobody's shared state, so every test and
      every deliberate non-default store is untouched, and so is a checkout
      pointed at its own store, which is the escape hatch.
    - ``source_root`` was found and the target lies OUTSIDE it. A store inside
      the checkout is worktree-local by construction. The root arrives as an
      ARGUMENT so a caller keeps its own hook rather than reaching into this
      module's globals.
    - the on-disk version is a readable int strictly BELOW ``code_version``. An
      absent or unparseable version is NOT a raise: refusing there would leave
      a torn file unrepairable by the command meant to rewrite it.

    Known sharp edge, carried over deliberately: this refuses ANY source-run
    raise of the shared file, not only one that exceeds the deployment. A
    checkout at the deployed version writing a shared file left at an older one
    is refused too. Reading the deployed constant to tell those apart needs a
    machine-specific interpreter path, and in practice the case self-heals in
    seconds because every deployed process stamps this file on its next write.
    The cost of guessing wrong in the other direction is a fleet-wide outage,
    so the comparison stays local.
    """
    if not isinstance(on_disk_version, int) or on_disk_version >= code_version:
        return
    if source_root is None:
        return
    resolved = target.resolve()
    try:
        resolved.relative_to(source_root)
    except ValueError:
        pass
    else:
        return
    try:
        shared_resolved = shared.resolve()
    except OSError:  # unreadable config is not a bump
        return
    if resolved != shared_resolved and shared_resolved not in resolved.parents:
        return
    raise error(
        f"refusing to raise the shared {what} at {target} from "
        f"schema_version={on_disk_version} to schema_version={code_version}: this fno "
        f"is running from source at {source_root}, not from the deployed install, so "
        "the bump exists only on this branch and every deployed reader on the "
        f"machine would degrade until it merges. Either deploy this schema "
        f"(fno doctor update), or {remedy}."
    )
