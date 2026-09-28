"""Keep the init shell's graph-node token guard aligned with supported IDs."""
from __future__ import annotations

import re
import subprocess
from pathlib import Path

import pytest


INIT_SCRIPT = Path(__file__).resolve().parents[3] / "hooks" / "helpers" / "init-target-state.sh"


@pytest.mark.parametrize("node_id", ["ab-deadbeef", "xd863"])
def test_init_node_token_guard_accepts_supported_node_ids(node_id: str) -> None:
    source = INIT_SCRIPT.read_text(encoding="utf-8")
    match = re.search(
        r'^\s*\[\[ "\$_tok" =~ (.+?) \]\] \|\| continue$', source, re.MULTILINE
    )
    assert match, "init target-state helper must keep its ID-shaped token guard"

    result = subprocess.run(
        ["bash", "-c", f'node_id="{node_id}"; [[ "$node_id" =~ {match.group(1)} ]]'],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, f"guard rejected supported node id {node_id!r}"
