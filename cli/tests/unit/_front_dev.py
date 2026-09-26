"""The front binary built in THIS checkout.

The same discipline as ``fno.rust_binary.find_dev_binary``: a binary under
``crates/fno/target/{release,debug}`` and nothing else, so the law transport
tests never run against an installed ``fno``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Optional

from fno.rust_binary import newest_runnable


def front_dev_binary() -> Optional[Path]:
    repo_root = Path(__file__).resolve().parents[3]
    base = repo_root / "crates" / "fno" / "target"
    return newest_runnable([base / p / "fno" for p in ("release", "debug")])
