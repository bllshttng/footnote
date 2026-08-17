"""footnote: autonomous delivery loop for Claude Code.

The ``run_loop`` / ``target`` Python API has been removed. Drive work via
``/target`` in a Claude Code session instead.

This module also owns the reinstall-window import guard (background:
``docs/architecture/cli-lazy-imports.md``). It lives here, and not next to the
lazy group that first needed it, because importing any ``fno.*`` module imports
this one first: it is the single site that covers every caller at once -- the
console script, ``python -m fno.cli``, a spawned worker, and the ~2000
function-level ``from fno. ...`` imports written inside command bodies, which no
per-callsite edit could ever keep up with.
"""

import sys

# No `from __future__ import annotations` here on purpose: it costs a measured
# ~154us of `__future__` import on EVERY `fno` process, and nothing below needs
# postponed evaluation. This module is on the startup path of every caller.

# Keep in lockstep with crates/fno and crates/fno-agents (Rust).
__version__ = "0.3.1"

__all__ = ["__version__"]


def _is_fno_module(name: str) -> bool:
    """True for our own package, false for a third-party dependency.

    The discriminator for every reinstall-window behavior below: a missing
    third-party dependency is a genuinely broken install and must neither be
    retried nor collect reinstall speculation.  Written as an exact-or-dotted
    match so a package merely BEGINNING with those three letters (``fnord``)
    is not mistaken for ours.
    """
    return name == "fno" or name.startswith("fno.")


def _module_is_now_on_disk(name: str) -> bool:
    """True when ``name`` resolves RIGHT NOW, after dropping the finder caches.

    The import that just failed proves nothing about the present: a reinstall
    replaces the package tree between two statements, so a module absent one
    moment is present the next.  Re-checking is what keeps the retry
    falsifiable rather than a hopeful sleep -- a genuinely missing module
    answers False here and fails exactly as it does today.

    ``invalidate_caches()`` is belt-and-braces, and the honest scope is small:
    ``FileFinder`` memoizes a directory listing but re-lists when the directory
    mtime changes, which covers a reinstall on any filesystem with fine mtime
    granularity (measured: APFS self-invalidates, so this call is not what makes
    the check work there).  It is kept for the cases that granularity does not
    cover -- a coarse-mtime filesystem where the rewrite lands inside one mtime
    tick -- and because it is the documented thing to do when files change
    underneath a running process.  It costs microseconds, on an error path only.
    """
    import importlib.util

    importlib.invalidate_caches()
    try:
        return importlib.util.find_spec(name) is not None
    except (ImportError, AttributeError, ValueError):
        # A parent package that is itself mid-replacement cannot answer the
        # question; treat "cannot tell" as "no" so we never retry on a guess.
        return False


def _reinstall_hint(name: str) -> str:
    """Suffix explaining a missing ``fno`` submodule, or "" for anything else.

    Because imports here happen at INVOCATION time, a subcommand's module is
    read off disk long after startup -- so ``uv tool install --reinstall``
    (which ``fno update`` runs) replaces the package underneath a running
    process and every not-yet-imported subcommand fails for the length of the
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
        f" ({name} is part of fno itself: either this package was being "
        "reinstalled underneath the running process, in which case retry, or "
        "the install is stale, in which case run `fno update` then `fno doctor`)"
    )


class _ReinstallWindowFinder:
    """Last-resort meta-path finder: re-check the disk before conceding.

    ``uv tool install --reinstall`` deletes and rewrites this package under
    running processes, so an ``fno.*`` import can fail against a tree that is
    whole again microseconds later.  ``_LazyStub._load_real`` already retries
    the lazy command-group import that way, but that is one of two import paths:
    the other is the deferred ``from fno. ...`` written inside a command body,
    and there are ~2000 of those.  ``fno agents truth`` reaches
    ``fno.agents.session_truth`` that way, and the fno-agents daemon runs it as
    a continuous per-session liveness probe, which makes it the highest-frequency
    reader of the window.  A guard on only one of the two paths is decorative.

    Appended to the END of ``sys.meta_path``, so it is consulted only once every
    normal finder has already said "no such module".  At that point it drops the
    finder caches and asks the path finder ONE more time:

    - present now -> hand back the spec and the import proceeds;
    - still absent -> a stale or broken install, which is neither waited on nor
      masked.  It raises the same dual-cause message the lazy group raises, in
      place of the bare ``ModuleNotFoundError`` those ~2000 sites produce today.

    The disk re-check is the whole difference between this and a hopeful
    sleep-retry, and it is why an absent module is still a hard, legible failure.
    """

    # How `_install_reinstall_window_finder` recognizes an already-installed
    # guard. Not `isinstance`: a module reload rebinds this class, so identity
    # would see a stranger and stack a second guard onto the same meta path.
    _fno_reinstall_window_guard = True

    # ponytail: a plain flag, not thread-local state. It exists only to stop the
    # re-check below from recursing into this finder. Two threads racing it lose
    # one retry and fall back to today's behavior; neither can get a wrong answer.
    _rechecking = False

    def find_spec(self, fullname: str, path=None, target=None):  # noqa: ANN001
        cls = type(self)
        if cls._rechecking or not _is_fno_module(fullname):
            return None
        cls._rechecking = True
        try:
            if not _module_is_now_on_disk(fullname):
                raise ModuleNotFoundError(
                    f"No module named {fullname!r}{_reinstall_hint(fullname)}",
                    name=fullname,
                )
            from importlib.machinery import PathFinder

            return PathFinder.find_spec(fullname, path, target)
        finally:
            cls._rechecking = False


def _install_reinstall_window_finder() -> None:
    """Append the guard once, behind every other finder.

    Idempotent because ``fno`` can be imported more than once in a process (a
    reload, a test that reaches in): stacking finders would multiply the
    re-check per failed import for no gain.
    """
    if any(getattr(finder, "_fno_reinstall_window_guard", False) for finder in sys.meta_path):
        return
    sys.meta_path.append(_ReinstallWindowFinder())


_install_reinstall_window_finder()


def __getattr__(name: str):
    if name in ("run_loop", "target"):
        raise AttributeError(
            f"fno.{name} has been removed: drive work via /target in a Claude Code session instead"
        )
    raise AttributeError(f"module 'fno' has no attribute {name!r}")
