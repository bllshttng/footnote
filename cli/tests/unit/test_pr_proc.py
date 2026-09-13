"""``_proc.run`` bounds: a timeout kills the child's whole process group (x-626f).

A plain child kill orphans the helpers the child spawned in turn; the child
runs in its own session so the group we kill is never ours.
"""

from __future__ import annotations

import subprocess
import time
from pathlib import Path

import pytest

from fno.pr import _proc


def _pattern_alive(match: str) -> bool:
    out = subprocess.run(["pgrep", "-f", match], capture_output=True, text=True)
    return bool(out.stdout.strip())


def test_run_timeout_kills_the_process_group(tmp_path):
    script = tmp_path / "tree.sh"
    script.write_text("#!/bin/bash\nsleep 98765 &\nwait\n")
    with pytest.raises(subprocess.TimeoutExpired):
        _proc.run(["bash", str(script)], timeout=1)
    time.sleep(0.3)
    assert not _pattern_alive("sleep 98765"), "the group kill must take the child's subtree"


def test_run_timeout_without_children_still_raises():
    with pytest.raises(subprocess.TimeoutExpired):
        _proc.run(["sleep", "98764"], timeout=1)
