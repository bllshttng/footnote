"""Locate the bundled ``fno-agents`` binary.

Pure filesystem lookup: stdlib only, no ``fno`` imports at all, which is why it
sits at the platform layer rather than under ``fno.agents``. Its callers span
every layer (``fno do phase kill-check``, ``fno doctor``, ``fno agents restart``, the
post-merge ledger finalizer, the relay, the agents runtime), and the ones below
the runtime were paying an upward import for what is four ``os.access`` checks.

Resolution order, widest first:

1. ``$FNO_AGENTS_BIN`` -- an explicit operator override.
2. The wheel-bundled binary at ``<package>/_bin/``.
3. The binary installed next to the running launcher.
4. ``PATH``.
5. A ``cargo build --release`` artifact (dev checkouts only, and only from
   :func:`resolve_binary`).
"""

from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path
from typing import Optional

BINARY_NAME = "fno-agents.exe" if os.name == "nt" else "fno-agents"

#: Operator override. The shell shims (scripts/run-target-loop.sh, the stop
#: hooks) have always honored this, so the Python resolver honors it too rather
#: than making the same export mean two different things depending on which half
#: of the toolchain reads it. Both "binary not found" messages in
#: ``fno.agents.rust_runtime`` name it, so the remedy is discoverable from the
#: failure itself.
BINARY_ENV = "FNO_AGENTS_BIN"


def _env_binary() -> Optional[Path]:
    """The explicit ``$FNO_AGENTS_BIN`` override, when it names a runnable file.

    An unset, empty, or non-executable value falls through to the search rather
    than failing: the shims treat it the same way, and a stale export must not
    make an otherwise-installed binary unreachable.
    """
    raw = (os.environ.get(BINARY_ENV) or "").strip()
    if not raw:
        return None
    candidate = Path(raw).expanduser()
    return candidate if candidate.is_file() and os.access(candidate, os.X_OK) else None


def _bundled_binary() -> Optional[Path]:
    """The wheel-bundled binary at ``<package>/_bin/fno-agents`` (W6 Wave 3)."""
    bundled = Path(__file__).resolve().parent / "_bin" / BINARY_NAME
    return bundled if bundled.is_file() and os.access(bundled, os.X_OK) else None


def _sibling_binary() -> Optional[Path]:
    """The binary installed next to the running launcher (the wheel scripts dir).

    pip installs both the ``fno`` console script and the bundled ``fno-agents``
    wheel-script into the same bin/ (Scripts/ on Windows). When ``fno`` is invoked
    by absolute path without that dir on ``PATH`` (common in CI / cron wrappers),
    ``shutil.which`` misses the binary even though it sits right beside the
    launcher; this finder catches that case (codex P2 on PR #351).
    """
    launcher = sys.argv[0] if sys.argv else ""
    if not launcher:
        return None
    sibling = Path(launcher).resolve().parent / BINARY_NAME
    return sibling if sibling.is_file() and os.access(sibling, os.X_OK) else None


def _path_binary() -> Optional[Path]:
    """The binary as resolved on ``PATH`` (``cargo install`` / GH release / wheel script)."""
    found = shutil.which(BINARY_NAME)
    return Path(found) if found else None


def _cargo_dev_binary() -> Optional[Path]:
    """Dev fallback: a ``cargo build --release`` artifact under the repo tree.

    ``__file__`` is ``cli/src/fno/rust_binary.py`` so the repo root is
    ``parents[3]``. Checks both a crate-local ``target/`` and a workspace
    ``target/`` so it works whether or not a workspace is introduced later.
    """
    here = Path(__file__).resolve()
    try:
        repo_root = here.parents[3]
    except IndexError:  # installed shallower than a dev checkout
        return None
    # Only meaningful in a development checkout. When the package is installed
    # into site-packages, parents[3] is some unrelated ancestor; refuse to
    # traverse it so we never return a coincidental wrong binary.
    if not (repo_root / "Cargo.toml").exists() and not (repo_root / "crates").is_dir():
        return None
    candidates = (
        repo_root / "crates" / "fno-agents" / "target" / "release" / BINARY_NAME,
        repo_root / "target" / "release" / BINARY_NAME,
    )
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def resolve_binary() -> Optional[Path]:
    """Locate ``fno-agents``: env override -> bundled -> sibling -> PATH -> cargo dev.

    Bundled beats PATH so a ``pip install fno`` wheel is self-contained even when
    a different (older) ``fno-agents`` happens to be on PATH. The launcher-sibling
    lookup sits ahead of PATH so an abs-path ``fno`` invocation still resolves the
    co-installed binary. ``$FNO_AGENTS_BIN`` outranks all of them, because an
    operator who set it meant it.
    """
    for finder in (
        _env_binary,
        _bundled_binary,
        _sibling_binary,
        _path_binary,
        _cargo_dev_binary,
    ):
        found = finder()
        if found is not None:
            return found
    return None


def resolve_installed_binary() -> Optional[Path]:
    """Locate an *installed* ``fno-agents``, deliberately excluding the cargo dev target.

    The ``auto`` (default) runtime uses this narrower set so a *development*
    checkout -- where only ``crates/fno-agents/target/release`` exists -- stays on
    the Python dispatch by default, and the in-process test suite never execs the
    binary. A dev who wants Rust opts in explicitly with ``FNO_AGENTS_RUNTIME=rust``,
    which routes through the full :func:`resolve_binary` (cargo dev included).

    Deliberately does NOT consult ``$FNO_AGENTS_BIN``: this function decides the
    *default* runtime, and a stale export pointing at a dev build must not
    silently flip an install onto the Rust path. ``FNO_AGENTS_RUNTIME=rust`` is
    the opt-in, and it routes through :func:`resolve_binary`, which does.
    """
    for finder in (_bundled_binary, _sibling_binary, _path_binary):
        found = finder()
        if found is not None:
            return found
    return None
