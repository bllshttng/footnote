"""The registry mint guard: production rows are born through mint_agent_entry.

Replaces the retired check-session-identity-parity.sh and
check-spawn-lineage-parity.sh sweeps. Rust enforces the same invariant at
compile time via RegistryEntry::new; Python enforces it here and at the mint
constructor's signature.
"""

from pathlib import Path

import pytest

from fno.agents.registry import AgentEntry, mint_agent_entry

SRC = Path(__file__).resolve().parents[2] / "src" / "fno"


def test_mint_without_session_identity_raises():
    with pytest.raises(TypeError):
        mint_agent_entry(  # type: ignore[call-arg]
            spawned_by_session=None,
            spawned_by_harness=None,
            spawned_by_cwd=None,
            name="worker-1",
        )


def test_mint_without_parent_edge_raises():
    with pytest.raises(TypeError):
        mint_agent_entry(  # type: ignore[call-arg]
            harness_session_id="ses-1",
            name="worker-1",
        )


def test_mint_stamps_the_identity_and_lineage_fields():
    entry = mint_agent_entry(
        harness_session_id="ses-1",
        spawned_by_session="parent-1",
        spawned_by_harness="claude",
        spawned_by_cwd="/tmp/w",
        name="worker-1",
        harness="claude",
        cwd="/tmp/w",
        log_path="/tmp/w/worker.log",
    )
    assert entry.harness_session_id == "ses-1"
    assert entry.spawned_by_session == "parent-1"
    assert entry.spawned_by_harness == "claude"
    assert entry.spawned_by_cwd == "/tmp/w"


def test_legacy_row_without_the_fields_still_loads():
    row = {
        "name": "old-worker",
        "cwd": "/tmp/w",
        "log_path": "",
        "harness": "codex",
    }
    entry = AgentEntry(**row)
    assert entry.harness_session_id is None
    assert entry.spawned_by_session is None


def test_no_direct_agent_entry_construction_in_production():
    """Every production mint routes through mint_agent_entry; a direct
    AgentEntry( call in src is a mint that can silently omit the identity
    and lineage fields. registry.py holds the dataclass, the mint
    constructor, and the tolerant read path - the allowed exceptions."""
    offenders = []
    for path in sorted(SRC.rglob("*.py")):
        text = path.read_text(encoding="utf-8")
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            if "AgentEntry(" in line and path.name != "registry.py":
                offenders.append(f"{path.relative_to(SRC)}:{lineno}: {stripped}")
            if "AgentEntry(" in line and path.name == "registry.py":
                # Inside registry.py only the mint helper's return and the
                # read path construct directly.
                owner = "mint_agent_entry" if "return AgentEntry(" in line else None
                if owner is None and "**row" not in line:
                    offenders.append(f"{path.relative_to(SRC)}:{lineno}: {stripped}")
    assert not offenders, "direct AgentEntry( mint outside the allowed paths:\n" + "\n".join(offenders)
