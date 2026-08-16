"""The reinstall-window import guard, shared by every path that imports ``fno``.

``fno update`` runs ``uv tool install --reinstall``, which deletes and rewrites
the very ``site-packages`` tree a running ``fno`` process executes from, so a
first-time import landing in that window fails.  ``docs/architecture/
cli-lazy-imports.md`` ("The reinstall-window hazard") has the measurement, the
hand repro, and the three fixes rejected with it.

This module holds ONE implementation of the accepted response: re-check the
module on disk right now, and use what the re-check found.  A module that is
genuinely absent still fails, naming both candidate causes, so a stale install
is never masked as a transient one.

Two consumers share it, which is the whole point -- a guard sitting on one of
several reachable import paths is decorative:

* ``fno._lazy_group._load_real`` covers the lazy command-group import.
* ``_ReinstallWindowFinder`` covers the ~2100 deferred ``from fno. ...``
  imports written inside command bodies, none of which route through
  ``_load_real``.  ``fno agents truth`` is one of them, and the daemon runs it
  as a continuous per-session liveness probe.

Stdlib only, and ``importlib.util`` is imported inside the function rather than
at module level: ``fno/__init__.py`` arms the finder, so everything here is paid
by every single ``import fno``.
"""

from __future__ import annotations

import sys
import threading
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from importlib.machinery import ModuleSpec

# Substring that marks a message as already carrying the dual-cause hint.  The
# finder raises with it baked in, so ``_import_failure_hint`` has to recognise
# its own words to avoid stating one diagnosis twice in one line.
_HINT_MARKER = "is part of fno itself"


def _is_fno_module(name: str) -> bool:
    """True for our own package, false for a third-party dependency.

    The discriminator for every reinstall-window behavior here: a missing
    third-party dependency is a genuinely broken install and must neither be
    retried nor collect reinstall speculation.  Written as an exact-or-dotted
    match so a package merely BEGINNING with those three letters (``fnord``)
    is not mistaken for ours.
    """
    return name == "fno" or name.startswith("fno.")


def _find_spec_now(name: str) -> ModuleSpec | None:
    """The spec for ``name`` as the disk reads RIGHT NOW, after dropping caches.

    The import that just failed proves nothing about the present: a reinstall
    replaces the package tree between two statements, so a module absent one
    moment is present the next.  Re-checking is what keeps the retry falsifiable
    rather than a hopeful sleep -- a genuinely missing module answers ``None``
    here and fails exactly as it does today.

    ``invalidate_caches()`` is belt-and-braces, and the honest scope is small:
    ``FileFinder`` memoizes a directory listing but re-lists when the directory
    mtime changes, which covers a reinstall on any filesystem with fine mtime
    granularity (measured: APFS self-invalidates, so this call is not what makes
    the check work there).  It is kept for the cases that granularity does not
    cover -- a coarse-mtime filesystem where the rewrite lands inside one mtime
    tick -- and because it is the documented thing to do when files change
    underneath a running process.  It costs microseconds, on an error path only.
    """
    import importlib
    import importlib.util

    importlib.invalidate_caches()
    try:
        return importlib.util.find_spec(name)
    except (ImportError, AttributeError, ValueError):
        # A parent package that is itself mid-replacement cannot answer the
        # question; treat "cannot tell" as "no" so we never retry on a guess.
        return None


def _module_is_now_on_disk(name: str) -> bool:
    """[`_find_spec_now`] as the yes/no question ``_load_real`` asks."""
    return _find_spec_now(name) is not None


def _hint_for_name(name: str) -> str:
    """Suffix explaining a missing ``fno`` submodule, or "" for anything else.

    Because imports are deferred to INVOCATION time, a subcommand's module is
    read off disk long after startup -- so ``uv tool install --reinstall``
    (which ``fno update`` runs) replaces the package underneath a running
    process and every not-yet-imported module fails for the length of the
    install.  Two very different things produce that same ModuleNotFoundError
    and they need opposite responses: a reinstall in flight (transient, retry)
    or a stale/incomplete install (persistent, reinstall properly).  Naming only
    the flattering transient one would assert a cause we have not established,
    so name both and let the reader discriminate by whether it recurs.

    Gated on the missing module being ours: a missing third-party dependency is
    a genuinely broken install and must not collect reinstall speculation.
    """
    if not _is_fno_module(name):
        return ""
    return (
        f" ({name} {_HINT_MARKER}: either this package was being "
        "reinstalled underneath the running process, in which case retry, or "
        "the install is stale, in which case run `fno update` then `fno doctor`)"
    )


def _import_failure_hint(exc: ImportError) -> str:
    """[`_hint_for_name`] for an exception that has already been raised."""
    if _HINT_MARKER in str(exc):
        return ""
    return _hint_for_name(getattr(exc, "name", None) or "")


class _ReinstallWindowFinder:
    """Last-resort ``sys.meta_path`` finder for ``fno.*`` modules.

    Appended to the tail of ``sys.meta_path``, so it is consulted only after
    every ordinary finder has MISSED.  During a reinstall that miss means "the
    file was not there a microsecond ago", which is a claim about the past, so
    this asks the disk again and hands back whatever the second look found.

    Retry-once is structural rather than a counter: this finder runs exactly
    once per import attempt, and re-checks the disk exactly once per run.  A
    module that is really gone gets the dual-cause message rather than a bare
    ModuleNotFoundError, and is never waited on -- so a stale install still
    fails immediately instead of hiding behind a delay.
    """

    def __init__(self) -> None:
        # ``_find_spec_now`` walks ``sys.meta_path`` itself, so it re-enters
        # this finder for the same name.  Per-thread because two threads can be
        # importing different modules at the same moment.
        self._active = threading.local()

    def find_spec(
        self,
        fullname: str,
        path: object = None,
        target: object = None,
    ) -> ModuleSpec | None:
        if not _is_fno_module(fullname):
            return None
        names = getattr(self._active, "names", None)
        if names is None:
            names = self._active.names = set()  # type: ignore[attr-defined]
        if fullname in names:
            return None
        names.add(fullname)
        try:
            spec = _find_spec_now(fullname)
        finally:
            names.discard(fullname)
        if spec is not None:
            return spec
        # Raising beats returning None: the message is the only thing an
        # operator sees for the ~2100 function-level imports, and the machinery
        # would otherwise replace it with a bare "No module named ...".
        raise ModuleNotFoundError(
            f"No module named {fullname!r}{_hint_for_name(fullname)}",
            name=fullname,
        )


def install_reinstall_window_finder() -> None:
    """Arm the finder for this process.  Idempotent, and safe to call early."""
    for finder in sys.meta_path:
        if isinstance(finder, _ReinstallWindowFinder):
            return
    sys.meta_path.append(_ReinstallWindowFinder())
