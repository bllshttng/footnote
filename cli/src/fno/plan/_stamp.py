from __future__ import annotations

import os
import tempfile
from pathlib import Path


def _atomic_write(target: Path, content: str) -> None:
    """Write content to target atomically via tmp + os.replace.

    Guards against truncation if the process is interrupted mid-write.
    Preserves the target's existing file mode so os.replace does not
    downgrade a 0644 plan file to the mkstemp default of 0600.
    """
    target.parent.mkdir(parents=True, exist_ok=True)
    # Snapshot the target's mode BEFORE creating the tmp so we can restore it.
    # If the target does not exist yet (first write), fall back to the process
    # umask-driven default that a plain open() would have produced.
    original_mode: int | None = None
    if target.exists():
        try:
            original_mode = target.stat().st_mode & 0o777
        except OSError:
            original_mode = None
    fd, tmp_name = tempfile.mkstemp(
        prefix=f".{target.name}.", suffix=".tmp", dir=str(target.parent)
    )
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(content)
        if original_mode is not None:
            try:
                os.chmod(tmp_name, original_mode)
            except OSError:
                pass  # best-effort; atomicity matters more than permissions
        os.replace(tmp_name, target)
    except Exception:
        # Best-effort cleanup of the tmp file; re-raise the original error.
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise
