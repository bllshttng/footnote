"""The worktree-removal gate's receipt, read from the fno-agents binary.

The gate has ONE implementation: ``crates/fno-agents/src/worktree_reapable.rs``
(the done-node salvage arm deleted the Python classifier). This module is the
Python library
seam for it: run the binary, parse the one-line receipt, fail closed. Callers
that cannot reach the binary get ``None`` and keep whatever they were about
to remove, which is exactly the pre-port fail-closed default.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from typing import Optional


@dataclass(frozen=True)
class Receipt:
    """The parsed receipt line. `detail` is the remainder after ` detail=`,
    which the grammar keeps last so a path with spaces survives the parse."""

    reapable: bool
    reason: str
    detail: str = ""
    line: str = ""


def _parse(line: str) -> Optional[Receipt]:
    text = line.strip()
    if not text.startswith("reapable="):
        return None
    reapable = text.startswith("reapable=yes")
    reason = text.split("reason=", 1)[1].split(" ", 1)[0] if "reason=" in text else ""
    detail = text.split(" detail=", 1)[1] if " detail=" in text else ""
    return Receipt(reapable=reapable, reason=reason, detail=detail, line=text)


def reapable_receipt(
    path: str,
    allow_unborn: bool = False,
    done_node: bool = False,
) -> Optional[Receipt]:
    """Run the fno-agents gate on ``path``; None when it cannot answer."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        return None
    args = [str(binary), "worktree-reapable", str(path)]
    if allow_unborn:
        args.append("--allow-unborn")
    if done_node:
        args.append("--done-node")
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=90)
    except (OSError, subprocess.SubprocessError):
        return None
    stdout = (r.stdout or "").strip()
    if not stdout:
        return None
    return _parse(stdout.splitlines()[0])
