"""The Rust pending-session vocabulary mirrors Python's SESSION_PHASES
(pending_session_row.rs doc comment owns the mirror contract); one test
fails when either leg drifts."""

import re
from pathlib import Path

from fno.graph.types import SESSION_PHASES


def test_rust_phase_vocabulary_mirrors_session_phases():
    root = Path(__file__).resolve().parents[3]
    src = (root / "crates/fno-agents/src/pending_session_row.rs").read_text(
        encoding="utf-8"
    )
    match = re.search(r"const PHASES: &\[&str\] = &\[(?P<items>[^\]]+)\]", src)
    assert match, "the PHASES const moved; update this parity test"
    rust_phases = set(re.findall(r'"([a-z]+)"', match.group("items")))
    assert rust_phases == set(SESSION_PHASES)
    assert "execute" in rust_phases
    assert "do" not in rust_phases
