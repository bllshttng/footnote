"""The global-id prefix lists must not diverge between Python and Rust.

`claims_root_for` is implemented twice: `fno.claims.io` owns the canonical
list, and `crates/fno-agents/src/claims.rs` hand-copies it under a comment
saying it mirrors the Python. Nothing held the two equal, so a prefix added
to one and not the other routes the same claim key to two different roots:
the writer and the reader coordinate on different files and both believe
they won.

The parse asserts a known member (`node`) before comparing, so an empty or
broken parse fails as a broken instrument rather than passing as agreement.
"""
from __future__ import annotations

import re
from pathlib import Path

from fno.claims.io import _GLOBAL_ID_PREFIXES

RUST_CLAIMS = (
    Path(__file__).resolve().parents[3] / "crates" / "fno-agents" / "src" / "claims.rs"
)


def _rust_global_prefixes() -> set[str]:
    source = RUST_CLAIMS.read_text()
    block = re.search(
        r"const GLOBAL_ID_PREFIXES: &\[&str\] = &\[(.*?)\];", source, re.DOTALL
    )
    assert block is not None, f"GLOBAL_ID_PREFIXES not found in {RUST_CLAIMS}"
    members = re.findall(r'"([^"]+)"', block.group(1))
    assert "node" in members, (
        f"broken instrument: the parse of {RUST_CLAIMS} found {members}, "
        "which lacks the known member 'node'"
    )
    return set(members)


def test_rust_prefix_list_is_a_positive_copy_of_python() -> None:
    rust = _rust_global_prefixes()
    python = set(_GLOBAL_ID_PREFIXES)
    assert python, "broken instrument: _GLOBAL_ID_PREFIXES parsed empty"
    missing_in_rust = python - rust
    missing_in_python = rust - python
    assert not missing_in_rust, (
        f"prefixes {sorted(missing_in_rust)} are in fno.claims.io but missing "
        f"from {RUST_CLAIMS}: the two route the same key to different roots"
    )
    assert not missing_in_python, (
        f"prefixes {sorted(missing_in_python)} are in {RUST_CLAIMS} but missing "
        "from fno.claims.io"
    )


def test_gate_key_routes_to_the_global_root() -> None:
    from fno.claims.io import claims_root_for, global_claims_root

    assert claims_root_for("gate:spawn") == global_claims_root()
