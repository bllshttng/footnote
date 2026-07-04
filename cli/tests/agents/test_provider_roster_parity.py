"""US1 (opencode buildout): the Rust and Python provider rosters must agree.

Rust's single source is ``KNOWN_PROVIDERS`` in
``crates/fno-agents/src/provider.rs`` (registry acceptance in client_verbs,
the spawn gates in bin/client.rs, and ``for_name`` all ride it); Python's
registry-acceptance mirror is ``READABLE_PROVIDERS``. agy taught the lesson:
it joined the Rust lists but not Python's, and a single agy registry row
hard-failed every Python consumer until READABLE_PROVIDERS grew. This test
pins the two lists together so the next provider add cannot drift.
"""

import re
from pathlib import Path

import pytest

from fno.agents.providers import KNOWN_PROVIDERS, READABLE_PROVIDERS


def _rust_provider_rs() -> Path:
    return (
        Path(__file__).resolve().parents[3]
        / "crates"
        / "fno-agents"
        / "src"
        / "provider.rs"
    )


def test_rust_roster_matches_python_readable() -> None:
    rust_src = _rust_provider_rs()
    if not rust_src.exists():
        pytest.skip("rust source not present (installed package)")
    m = re.search(
        r"pub const KNOWN_PROVIDERS: &\[&str\] = &\[([^\]]*)\]",
        rust_src.read_text(encoding="utf-8"),
    )
    assert m, "KNOWN_PROVIDERS const not found in provider.rs"
    rust_roster = set(re.findall(r'"([a-z0-9_-]+)"', m.group(1)))
    assert rust_roster == set(READABLE_PROVIDERS), (
        "Rust KNOWN_PROVIDERS and Python READABLE_PROVIDERS drifted; "
        "a provider readable on one side but not the other reproduces the "
        "agy split-brain (rows rejected by one language's registry loader)"
    )


def test_python_known_is_subset_of_readable() -> None:
    # KNOWN (Python ask-dispatchable, has a Python adapter) is a strict
    # subset of READABLE (registry-acceptance): every dispatchable provider's
    # rows must load, but Rust-only providers are readable without being
    # Python-dispatchable.
    assert set(KNOWN_PROVIDERS) <= set(READABLE_PROVIDERS)
