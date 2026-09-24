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
import time

# No `from __future__ import annotations` here on purpose: it costs a measured
# ~154us of `__future__` import on EVERY `fno` process, and nothing below needs
# postponed evaluation. This module is on the startup path of every caller.

# Keep in lockstep with crates/fno and crates/fno-agents (Rust).
__version__ = "0.4.0"

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


# Mirror of the front door's VERIFY_ATTEMPTS / VERIFY_POLL in
# crates/fno/src/bootstrap.rs (15 * 200ms = 3s); the budget is shared as
# numbers because the implementations cannot cross the language boundary.
_VERIFY_ATTEMPTS = 15
_VERIFY_POLL_SECONDS = 0.2

# Once one import has exhausted the wait budget in this process, every later
# absence answers after a single look: all of a stale install's absent modules
# are that install answering again, and re-paying 3s per import would turn one
# legible failure into a process-wide stall.
_recheck_budget_spent = False


def _module_appears_on_disk(name: str) -> bool:
    """True when ``name`` resolves within a bounded re-check budget.

    A reinstall replaces the package tree between two statements, so one
    immediate re-check loses races an installer in flight is about to win.
    This polls instead, and returns the moment the module appears.  Every pass
    re-runs the real predicate, which keeps the wait falsifiable: a genuinely
    missing module stays absent through every pass and fails exactly as it did
    before the wait existed, within a bounded, once-per-process delay.  Full
    mechanics and measured costs: docs/architecture/cli-lazy-imports.md.
    """
    global _recheck_budget_spent

    import importlib.util

    def look():
        importlib.invalidate_caches()
        try:
            spec = importlib.util.find_spec(name)
        except (ImportError, AttributeError, ValueError):
            # A parent package that is itself mid-replacement cannot answer the
            # question; treat "cannot tell" as "no" so we never retry on a guess.
            return None
        if spec is not None and spec.loader is None:
            # A namespace portion: the directory exists but the package's own
            # __init__.py does not. Importing it "succeeds" as an empty module
            # and every submodule lookup after it fails, so answer "absent".
            return False
        return spec is not None

    answer = look()
    if answer is None or answer or _recheck_budget_spent:
        return bool(answer)
    for _ in range(_VERIFY_ATTEMPTS):
        time.sleep(_VERIFY_POLL_SECONDS)
        answer = look()
        if answer:
            return True
        if answer is None:
            return False
    _recheck_budget_spent = True
    return False


def _reinstall_hint(name: str) -> str:
    """Suffix explaining a missing ``fno`` submodule, or "" for anything else.

    Because imports here happen at INVOCATION time, a subcommand's module is
    read off disk long after startup -- so the ``uv tool install`` that
    ``fno doctor update`` runs replaces the package underneath a running
    process and every not-yet-imported subcommand can fail for the length of
    the swap.  Two very different things produce that same ModuleNotFoundError
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
        "the install is stale, in which case run `fno doctor update` then `fno doctor`)"
    )


class _ReinstallWindowFinder:
    """Last-resort meta-path finder: re-check the disk before conceding.

    ``uv tool install`` deletes and rewrites this package under
    running processes, so an ``fno.*`` import can fail against a tree that is
    whole again microseconds later.  ``_LazyStub._load_real`` already retries
    the lazy command-group import that way, but that is one of two import paths:
    the other is the deferred ``from fno. ...`` written inside a command body,
    and there are ~2000 of those.  ``fno agents truth`` reaches
    ``fno.agents.session_truth`` that way, and the fno-agents daemon runs it as
    a continuous per-session liveness probe, which makes it the highest-frequency
    reader of the window.  A guard on only one of the two paths is decorative.

    Appended to the END of ``sys.meta_path``, so it is consulted only once every
    normal finder has already said "no such module".  At that point it re-asks
    the disk on a bounded poll (``_VERIFY_ATTEMPTS`` x ``_VERIFY_POLL_SECONDS``,
    the ``install_verified_within`` shape from ``crates/fno/src/bootstrap.rs``):

    - present within the budget -> hand back the spec and the import proceeds;
    - still absent after it -> a stale or broken install, which is not masked.
      It raises the same dual-cause message the lazy group raises, in place of
      the bare ``ModuleNotFoundError`` those ~2000 sites produce today.

    Every pass re-runs the real on-disk predicate, which is the whole
    difference between this and a hopeful sleep-retry, and why an absent
    module is still a hard, legible failure -- now by at most one budget
    later.

    Two limits, both deliberate.

    The RETRY reaches every import shape, but the MESSAGE does not reach one of
    them. For ``from fno.pkg import submodule`` CPython's ``_handle_fromlist``
    swallows a ModuleNotFoundError whose name matches the fromlist entry and
    raises ``cannot import name ... from ...`` in its place, so the dual-cause
    text is dropped there. Nothing at this layer can reach that decision. The
    retry is unaffected because it happens inside ``find_spec``, before the
    exception exists. ``from fno.pkg.submodule import name``, which is how the
    in-body imports are written, keeps the message.

    Raising here inverts one stdlib contract: ``importlib.util.find_spec`` on an
    absent ``fno.*`` module raises instead of returning None. That is the price
    of carrying the message to call sites we do not edit, and it is priced
    knowingly: no caller in this repo probes for an fno module that way, and the
    exception is still an ImportError subclass, so an existing
    ``try/except ImportError`` guard behaves as before.
    """

    # How `_install_reinstall_window_finder` recognizes an already-installed
    # guard. Not `isinstance`: a module reload rebinds this class, so identity
    # would see a stranger and stack a second guard onto the same meta path.
    _fno_reinstall_window_guard = True

    # Thread-local, not a plain class flag. The flag's only job is to stop
    # THIS thread's re-check from recursing back into this finder: it goes
    # through `importlib.util.find_spec`, which DOES walk `sys.meta_path`, and
    # recursion is always same-thread, so per-thread state guards it fully. A
    # plain flag also silenced every OTHER thread's import for the whole hold,
    # which used to be one lookup and is now a bounded wait -- a plain flag
    # would trade one thread's reinstall wait for another thread's bare
    # failure. Created lazily because `import threading` on the startup path
    # costs more than anything else in this module; the first FAILED import
    # pays it instead. Concurrent first use is safe: each thread captures the
    # object it set and clears that one, and a stranger's overwrite only means
    # the other thread's recursion stop rides its own fresh local.
    _rechecking = None

    def find_spec(self, fullname: str, path=None, target=None):  # noqa: ANN001
        cls = type(self)
        tl = cls._rechecking
        if tl is None:
            import threading

            tl = cls._rechecking = threading.local()
        if getattr(tl, "active", False) or not _is_fno_module(fullname):
            return None
        tl.active = True
        try:
            # `_module_appears_on_disk` rather than an inlined PathFinder
            # probe, even though inlining would save this second lookup:
            # `_load_real` asks the same question, and two implementations of
            # "is it on disk now" is the one-of-N-paths trap this guard exists
            # to close. The duplicate lookup costs microseconds and only ever
            # runs on an import that has already failed; its bounded wait is
            # the helper's, not a second one here.
            if not _module_appears_on_disk(fullname):
                raise ModuleNotFoundError(
                    f"No module named {fullname!r}{_reinstall_hint(fullname)}",
                    name=fullname,
                )
            from importlib.machinery import PathFinder

            return PathFinder.find_spec(fullname, path, target)
        finally:
            tl.active = False


class _FnoNamespaceRefuser:
    """Sits BEFORE ``PathFinder`` and refuses namespace portions of ``fno.*``.

    A mid-swap directory without its ``__init__.py`` is something PathFinder
    SAYS YES to: the namespace spec caches an empty module and the last-resort
    guard below is never consulted, and that poison breaks every
    ``from fno.pkg import name`` for the process lifetime. Refusing it makes
    the import the dual-cause ModuleNotFoundError instead; a retry after the
    swap lands on the real package. Cost: one prefix check per import walk.
    Ships no namespace subpackage under ``fno``, so refusing the shape is safe
    here. Full story: docs/architecture/cli-lazy-imports.md.
    """

    # How the installer recognizes an already-installed refuser.
    _fno_namespace_refuser = True

    def find_spec(self, fullname: str, path=None, target=None):  # noqa: ANN001
        if not _is_fno_module(fullname):
            return None
        from importlib.machinery import PathFinder

        spec = PathFinder.find_spec(fullname, path, target)
        if spec is not None and spec.loader is None:
            raise ModuleNotFoundError(
                f"No module named {fullname!r}{_reinstall_hint(fullname)}",
                name=fullname,
            )
        return spec


def _install_reinstall_window_finder() -> None:
    """Install both guards once: the namespace refuser before ``PathFinder``,
    the wait-and-hint guard at the very end.

    Idempotent because ``fno`` can be imported more than once in a process (a
    reload, a test that reaches in): stacking finders would multiply the
    re-check per failed import for no gain.
    """
    if not any(getattr(finder, "_fno_reinstall_window_guard", False) for finder in sys.meta_path):
        sys.meta_path.append(_ReinstallWindowFinder())
    if any(getattr(finder, "_fno_namespace_refuser", False) for finder in sys.meta_path):
        return
    path_finder_at = next(
        (
            i
            for i, finder in enumerate(sys.meta_path)
            if getattr(finder, "__name__", "") == "PathFinder"
        ),
        len(sys.meta_path),
    )
    sys.meta_path.insert(path_finder_at, _FnoNamespaceRefuser())


_install_reinstall_window_finder()


def __getattr__(name: str):
    if name in ("run_loop", "target"):
        raise AttributeError(
            f"fno.{name} has been removed: drive work via /target in a Claude Code session instead"
        )
    raise AttributeError(f"module 'fno' has no attribute {name!r}")
