"""One-release environment-variable back-fill for the abilities -> fno rename.

The rename moved every ``ABILITIES_*`` and ``ABI_*`` environment variable to a
``FNO_*`` equivalent. An operator may still have the old names exported (shell
profile, launchd plists, CI). Rather than ignore those silently (a Failure Mode
the design calls out), this shim back-fills the new name from the old at CLI
startup, for one release.

The mapping is uniform -- strip the legacy prefix, prepend ``FNO_`` -- so no
per-variable enumeration is needed and any future ``ABI_*`` var is covered:

    ABILITIES_HOME        -> FNO_HOME
    ABI_AGENTS_HOME       -> FNO_AGENTS_HOME
    ABI_REPO_ROOT         -> FNO_REPO_ROOT

``setdefault`` semantics: an explicitly-set ``FNO_*`` always wins; the legacy
value is used only when the new name is unset. Idempotent and side-effect-free
to re-run.

This file intentionally references the OLD prefixes; it is exempt from the
rename residual-grep guard (scripts/rename/residual-check.sh KEEP_FILES).
"""
from __future__ import annotations

import os

_LEGACY_PREFIXES = ("ABILITIES_", "ABI_")


def backfill_legacy_env(environ: "os._Environ[str] | dict[str, str] | None" = None) -> list[tuple[str, str]]:
    """Back-fill FNO_* from any set ABILITIES_*/ABI_* vars.

    Returns the list of ``(legacy_name, new_name)`` pairs that were back-filled
    (empty when nothing legacy is set), so callers/tests can assert behavior.
    Mutates ``environ`` (default: ``os.environ``) in place via setdefault.
    """
    env = os.environ if environ is None else environ
    filled: list[tuple[str, str]] = []
    # Snapshot keys first -- we mutate env while iterating.
    for old in list(env.keys()):
        for prefix in _LEGACY_PREFIXES:
            if old.startswith(prefix):
                new = "FNO_" + old[len(prefix):]
                if new not in env:
                    env[new] = env[old]
                    filled.append((old, new))
                break
    return filled
