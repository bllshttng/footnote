"""Installer for the fno-agents state-path stub used by init-driving tests.

Copies tests/helpers/fno-agents-state-path-stub.sh into a test-owned bin dir
and returns the env (PATH entry + FNO_TEST_SPACE) the test must merge before
invoking init-target-state.sh.
"""

from __future__ import annotations

import shutil
from pathlib import Path

STUB = Path(__file__).resolve().parents[2] / "tests" / "helpers" / "fno-agents-state-path-stub.sh"


def install_state_path_stub(bin_dir: Path, space: Path) -> dict[str, str]:
    bin_dir.mkdir(parents=True, exist_ok=True)
    space.mkdir(parents=True, exist_ok=True)
    dest = bin_dir / "fno-agents"
    shutil.copyfile(STUB, dest)
    dest.chmod(0o755)
    return {"FNO_TEST_SPACE": str(space)}
