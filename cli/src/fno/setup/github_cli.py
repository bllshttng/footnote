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

_WRAPPER = "#!/bin/sh\nexec fno-gh-proxy \"$@\"\n"


@dataclass(frozen=True)
class InstallResult:
    proxy: Path
    delegate: Path
    changed: bool
    backup: Optional[Path] = None


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
    changed = not proxy.exists() or proxy.read_text() != _WRAPPER
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
    try:
        result = ensure_proxy(
            real_gh=Path(inherited_delegate) if inherited_delegate else None
        )
    except (FileNotFoundError, RuntimeError):
        return env
    old_path = env.get("PATH", "")
    env["PATH"] = str(result.proxy.parent) + (os.pathsep + old_path if old_path else "")
    env.pop("FNO_REAL_GH", None)
    return env
