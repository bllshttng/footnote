"""Install the dedicated `gh` proxy used by Footnote worker environments."""
from __future__ import annotations

import os
import shutil
import stat
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Mapping, Optional

from fno.paths import github_cli_proxy_dir

PROXY_EXEC_LINE = 'exec fno-gh-proxy "$@"'
_WRAPPER = f"#!/bin/sh\n{PROXY_EXEC_LINE}\n"
_PROXY_DIR_ENV = "FNO_GH_PROXY_DIR"
# The proxy stamps this into the env it hands `os.execve`, so its own successor
# can recognize itself. It is good for that one hop only. Anything else building
# a gh environment drops it, or a descendant inherits a marker it never earned
# and the proxy refuses a first-and-only entry.
PROXY_DEPTH_ENV = "FNO_GH_PROXY_DEPTH"
_WHICH = shutil.which


@dataclass(frozen=True)
class InstallResult:
    proxy: Path
    delegate: Path
    changed: bool
    backup: Optional[Path] = None


def fallback_proxy_dir() -> Path:
    """Config-free proxy home for commands whose config cannot be loaded."""
    uid = os.getuid() if hasattr(os, "getuid") else os.getpid()
    return Path(tempfile.gettempdir()) / f"fno-{uid}" / "github-cli"


def _is_wrapper(path: Path) -> bool:
    # A stock CI runner's gh is a compiled binary, not utf-8 text.
    try:
        return path.read_text() == _WRAPPER
    except (OSError, UnicodeDecodeError):
        return False


def ensure_proxy(
    *,
    directory: Optional[Path] = None,
    real_gh: Optional[Path] = None,
    which: Callable[[str], Optional[str]] = shutil.which,
) -> InstallResult:
    resolved = real_gh or (Path(found) if (found := which("gh")) else None)
    if resolved is None:
        raise FileNotFoundError("real gh executable not found on PATH")
    root = directory or github_cli_proxy_dir()
    root.mkdir(parents=True, exist_ok=True)
    proxy = root / "gh"
    resolved = resolved.resolve()
    if resolved == proxy.resolve():
        raise RuntimeError("resolved gh delegate points back to the Footnote proxy")

    backup = None
    changed = not proxy.exists() or not _is_wrapper(proxy)
    if changed:
        if proxy.exists():
            backup = proxy.with_name("gh.pre-fno")
            if not backup.exists():
                shutil.copy2(proxy, backup)
        fd, raw_tmp = tempfile.mkstemp(prefix=".gh.", dir=root)
        try:
            with os.fdopen(fd, "w") as handle:
                handle.write(_WRAPPER)
                handle.flush()
                os.fsync(handle.fileno())
            tmp = Path(raw_tmp)
            tmp.chmod(tmp.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
            os.replace(tmp, proxy)
        finally:
            Path(raw_tmp).unlink(missing_ok=True)
    return InstallResult(proxy=proxy, delegate=resolved, changed=changed, backup=backup)


def worker_environment(base: Mapping[str, str]) -> dict[str, str]:
    inherited_delegate = base.get("FNO_REAL_GH")
    env = dict(base)
    # The identity scrub lives HERE, at the floor every adapter's child env
    # already crosses, not in each adapter: a per-adapter scrub is one the
    # next adapter declines to call (x-b57a - claude's adapter scrubbed,
    # codex's never did, so a codex worker inherited its claude parent's
    # CLAUDE_CODE_SESSION_ID and stamped harness="claude-code" on every mail).
    # Before the no-gh early return: identity is not conditional on gh
    # presence. A spawned child inherits its parent's ROUTE but never its
    # parent's IDENTITY; each harness re-mints its own, so the scrub is
    # lossless. The pane (argv) substrate cannot cross this floor and strips
    # via ambient_identity_env_unset_args() - the same
    # AMBIENT_IDENTITY_ENV, so the shapes cannot drift.
    from fno.harness_identity import scrub_ambient_identity

    scrub_ambient_identity(env)
    # Seed provenance is inherited the same way and for the same reason it must
    # not be (node x-3a64): these fields name WHO SENT THIS CHILD ITS SEED, so a
    # child that inherits them attributes its own first message to whoever
    # seeded its parent - an envelope naming the wrong peer, which is worse than
    # none. Scrubbed at the floor rather than per adapter, so a launcher that
    # knows the seed SETS them after crossing it, and one that does not (a
    # headless one-shot) emits no sidecar instead of a wrong one.
    from fno.mail.seed_provenance import SEED_PROVENANCE_KEYS

    for _seed_key in SEED_PROVENANCE_KEYS:
        env.pop(_seed_key, None)
    # Dropped once, before any return: a worker inherits the marker from a
    # delegated gh, never earns it, and would have its first gh call refused.
    env.pop(PROXY_DEPTH_ENV, None)
    requested_dir = env.get(_PROXY_DIR_ENV)
    found = inherited_delegate or _WHICH("gh", path=env.get("PATH"))
    if not found:
        return env
    delegate = Path(found)
    try:
        if delegate.is_file() and _is_wrapper(delegate):
            env[_PROXY_DIR_ENV] = str(delegate.parent.resolve())
            env.pop("FNO_REAL_GH", None)
            return env
    except OSError:
        pass
    try:
        result = ensure_proxy(
            directory=Path(requested_dir) if requested_dir else None,
            real_gh=delegate,
        )
    except Exception:
        if requested_dir:
            raise
        result = ensure_proxy(directory=fallback_proxy_dir(), real_gh=delegate)
    old_path = env.get("PATH", "")
    env["PATH"] = str(result.proxy.parent) + (os.pathsep + old_path if old_path else "")
    env[_PROXY_DIR_ENV] = str(result.proxy.parent.resolve())
    env.pop("FNO_REAL_GH", None)
    return env
