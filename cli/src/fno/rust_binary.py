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
from typing import Any, Optional, Sequence

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
    """Dev fallback: a ``cargo build`` artifact under the repo tree.

    ``__file__`` is ``cli/src/fno/rust_binary.py`` so the repo root is
    ``parents[3]``. Checks both a crate-local ``target/`` and a workspace
    ``target/`` so it works whether or not a workspace is introduced later.
    Release outranks debug so a dev's optimized build wins, but a debug build
    counts too: the CI smoke lanes build debug and strip ``FNO_*`` env, so
    this finder is the only reader left for the footprint door there.
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
        repo_root / "crates" / "fno-agents" / "target" / "debug" / BINARY_NAME,
        repo_root / "target" / "debug" / BINARY_NAME,
    )
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def _front_binary() -> Optional[Path]:
    """The checkout-scoped front binary CI and the doctor tooling export
    (``$FNO_AGENTS_FRONT``, from the smoke-setup action). Sits behind PATH so
    a stale export never outranks a fresh install, but ahead of the cargo dev
    walk so a lane that built the binary and exported only the FRONT name
    stays readable - the footprint door now resolves through this finder, and
    a resolver blind to FRONT refuses every dispatch on those lanes.
    """
    raw = (os.environ.get("FNO_AGENTS_FRONT") or "").strip()
    if not raw:
        return None
    candidate = Path(raw).expanduser()
    return candidate if candidate.is_file() and os.access(candidate, os.X_OK) else None


def resolve_binary() -> Optional[Path]:
    """Locate ``fno-agents``: env override -> bundled -> sibling -> PATH -> FRONT -> cargo dev.

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
        _front_binary,
        _cargo_dev_binary,
    ):
        found = finder()
        if found is not None:
            return found
    return None


def call_binary_json(
    verb: str, args: Sequence[str] = (), *, timeout: Optional[float] = 60
) -> tuple[Optional[str], Any]:
    """Run one direct ``fno-agents`` client verb and parse its JSON stdout.

    Returns ``(error, parsed)``: ``error`` is None on success; a missing
    binary, non-zero exit, timeout, or unparseable stdout yields a short error
    text and a None payload. Callers keep the failure shape theirs (refuse
    closed, raise, or exit) - this seam only standardizes the door.
    """
    import json
    import subprocess

    binary = resolve_binary()
    if binary is None:
        return ("fno-agents binary not found", None)
    try:
        proc = subprocess.run(
            [str(binary), verb, *args], capture_output=True, text=True, timeout=timeout
        )
    except subprocess.TimeoutExpired:
        bound = f"{timeout:.1f}s" if timeout is not None else "the caller's bound"
        return (f"timed out after {bound}", None)
    except OSError as exc:
        return (str(exc)[:200], None)
    if proc.returncode != 0:
        return ((proc.stderr or "verb failed").strip()[:200], None)
    try:
        return (None, json.loads(proc.stdout or "null"))
    except ValueError:
        return ("unreadable JSON receipt", None)


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


def find_dev_binary() -> Optional[Path]:
    """The ``@requires_rust`` marker's detector: a binary built in THIS checkout.

    Debug or release under ``crates/fno-agents/target/``, and nothing else. No
    installed, wheel-bundled, or ``$FNO_AGENTS_BIN`` copy may answer for it:
    the tests behind the marker assert against this checkout's build, and the
    smoke-pytest CI shard deletes exactly these two files so they skip there
    instead of running against whatever else happens to be installed. The
    single source for a fact three test files used to copy by hand.
    """
    here = Path(__file__).resolve()
    try:
        repo_root = here.parents[3]
    except IndexError:
        return None
    if not (repo_root / "crates" / "fno-agents").is_dir():
        return None
    for profile in ("release", "debug"):
        candidate = repo_root / "crates" / "fno-agents" / "target" / profile / BINARY_NAME
        if candidate.is_file():
            return candidate
    return None


class VerbUnavailable(RuntimeError):
    """The fno-agents binary is missing, failed, or answered malformed JSON."""

    # Structured exit code for the non-zero-exit failure; None on the other
    # paths (binary missing, OSError/timeout, bad JSON). The gh budget's admit
    # gate reads it: a negative code or 137 is a signalled reader, which is a
    # machine in distress, not a ledger that cannot answer.
    returncode: Optional[int] = None


def verb_call(
    verb: str,
    payload: dict,
    unavailable: type = VerbUnavailable,
    *,
    timeout: float = 30,
    passthrough_stderr: bool = False,
) -> dict:
    """One subprocess round-trip with the fno-agents binary: JSON payload in,
    parsed JSON answer out. The dev checkout's own build outranks any stale
    installed copy. Raises the caller's ``unavailable`` exception - a named
    refusal, never a silent fallback.

    ``timeout`` defaults to the 30s a pure local resolver needs. A verb that
    makes its own network round trips must raise it, or the door reports the
    owner unreachable for a decision that was merely still running - and an
    unread authorization refuses the merge.

    ``passthrough_stderr`` leaves the child's stderr attached to this process
    instead of capturing it: a gate that queues for minutes streams its
    ``spawn queued: ...`` prose live, so an operator watching the spawn sees
    the wait instead of silence.
    """
    import json
    import os
    import subprocess

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        raise unavailable(
            "the fno-agents binary was not found; reinstall fno,"
            " run `fno doctor update --rust`, or set FNO_AGENTS_BIN"
        )
    try:
        proc = subprocess.run(
            [str(binary), verb],
            input=json.dumps(payload),
            stdout=subprocess.PIPE,
            stderr=None if passthrough_stderr else subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise unavailable(f"fno-agents {verb} failed: {exc}") from exc
    if proc.returncode != 0:
        # With passthrough_stderr the child owns the real stderr (None here),
        # so name where it went instead of crashing on strip().
        detail = f"fno-agents {verb} exited {proc.returncode}"
        if passthrough_stderr:
            detail += " (its stderr went to your terminal)"
        else:
            detail += f": {proc.stderr.strip()[:200]}"
        raised = unavailable(detail)
        raised.returncode = proc.returncode
        raise raised
    try:
        return json.loads(proc.stdout)
    except ValueError as exc:
        raise unavailable(f"fno-agents {verb} bad output: {exc}") from exc
    finally:
        if os.environ.get("FNO_ROUTE_SLOT_DEBUG"):
            print(json.dumps({"payload": payload}), flush=True)
