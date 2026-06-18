"""Scan hermes-agent responses for <promise>...</promise> tags and write
.fno/target-promise.signal with the inner content of the last match.

The external loop wrapper (scripts/run-target-loop.sh) reads this sentinel
file to decide whether to keep looping. Writes are atomic (write-tmp,
rename) so partial states never surface.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

PROMISE_RE = re.compile(r"<promise>(.*?)</promise>", re.DOTALL)

SENTINEL_DIR_NAME = ".fno"
SENTINEL_FILE_NAME = "target-promise.signal"
SENTINEL_TMP_NAME = "target-promise.signal.tmp"


def on_response(response_text: str, cwd: Path | str) -> Path | None:
    """Write the sentinel if a <promise> tag is present in response_text.

    Args:
        response_text: the final assistant message (stripped of tool frames).
        cwd: the workspace directory hermes is running in.

    Returns:
        Path to the written sentinel file, or None if no tag was found.
    """
    matches = PROMISE_RE.findall(response_text)
    if not matches:
        return None

    inner = matches[-1].strip()
    if not inner:
        return None

    cwd_path = Path(cwd)
    signal_dir = cwd_path / SENTINEL_DIR_NAME
    signal_dir.mkdir(parents=True, exist_ok=True)

    tmp = signal_dir / SENTINEL_TMP_NAME
    dst = signal_dir / SENTINEL_FILE_NAME
    payload = inner + "\n"

    tmp.write_text(payload, encoding="utf-8")
    try:
        tmp.replace(dst)
    except OSError as err:
        # Cross-device link or other rename failure (e.g. .fno/ is a
        # symlink to a different mount). Fall back to a direct write so the
        # loop wrapper still sees the sentinel; this loses the atomic
        # guarantee, but the wrapper's stdout-scan fallback covers the
        # narrow window where a reader sees a partial write.
        print(
            f"[fno-promise-tag-reader] atomic rename failed ({err}); "
            "falling back to direct write",
            file=sys.stderr,
        )
        dst.write_text(payload, encoding="utf-8")
        try:
            tmp.unlink()
        except OSError as unlink_err:
            # Leaving a stale .tmp behind is cosmetic; next write overwrites
            # it. Log so accumulated clutter is at least noticeable.
            print(
                f"[fno-promise-tag-reader] tmp cleanup failed: {unlink_err}",
                file=sys.stderr,
            )
    return dst


# Hermes plugin contract - exposes a recognized hook name so the plugin
# loader picks it up without extra glue.
def hermes_on_response(response_text: str, cwd: str | Path) -> None:
    on_response(response_text, cwd)
