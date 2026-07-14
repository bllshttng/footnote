"""Registry + routing migration to harness_session_id (x-ec59).

Covers the shared sync rule, the Python/Rust round-trip via the registry loader,
canonical-first discovery resolution, and harness-generic whoami. The reconcile
heal (AC1-FR / AC1-UI) lives with the other reconcile tests in
test_dispatch_lifecycle.py.
"""
import json
from pathlib import Path

from fno.agents.registry import (
    REGISTRY_LEGACY_SESSION_KEYS,
    SCHEMA_VERSION,
    AgentEntry,
    load_registry,
)
from fno.harness_identity import sync_harness_aliases

M = REGISTRY_LEGACY_SESSION_KEYS


# ---------------------------------------------------------------------------
# sync_harness_aliases — the one shared rule
# ---------------------------------------------------------------------------


def test_sync_canonical_wins_on_conflict():
    """AC2-EDGE: a set canonical id overwrites a conflicting legacy value."""
    d = {"harness": "claude", "harness_session_id": "CANON", "claude_session_uuid": "STALE"}
    sync_harness_aliases(d, M)
    assert d["claude_session_uuid"] == "CANON"


def test_sync_legacy_backfills_canonical():
    d = {"harness": "claude", "claude_session_uuid": "U"}
    sync_harness_aliases(d, M)
    assert d["harness_session_id"] == "U"


def test_sync_codex_canonical_only_syncs_its_legacy_key():
    d = {"harness": "codex", "harness_session_id": "T"}
    sync_harness_aliases(d, M)
    assert d["codex_session_id"] == "T"


def test_sync_skips_null_string_legacy():
    """A 'null'-string legacy value for the row's own harness is not adopted."""
    d = {"harness": "claude", "claude_session_uuid": "null"}
    sync_harness_aliases(d, M)
    assert d.get("harness_session_id") is None


def test_sync_does_not_cross_contaminate_from_another_harness():
    """A claude row must NOT adopt a codex id: only its own harness key is read."""
    d = {"harness": "claude", "claude_session_uuid": "null", "codex_session_id": "CODEX-ID"}
    sync_harness_aliases(d, M)
    assert d.get("harness_session_id") is None


def test_sync_unknown_harness_backfills_from_first_present_legacy():
    """A pre-migration row whose harness is unresolved falls back to a scan."""
    d = {"codex_session_id": "T"}  # no harness set
    sync_harness_aliases(d, M)
    assert d["harness_session_id"] == "T"


def test_sync_unknown_harness_never_crashes():
    """A harness with no legacy key in the map is a no-op, not a KeyError."""
    d = {"harness": "opencode", "harness_session_id": "X"}
    sync_harness_aliases(d, M)
    assert d["harness_session_id"] == "X"
    assert "opencode" not in d


# ---------------------------------------------------------------------------
# Registry round-trip (Python side of the cross-language contract)
# ---------------------------------------------------------------------------


def _write_registry_json(tmp: Path, rows: list[dict]) -> Path:
    p = tmp / "registry.json"
    p.write_text(json.dumps({"schema_version": SCHEMA_VERSION, "agents": rows}))
    return p


def _row(**over) -> dict:
    base = dict(
        name="w1",
        provider="claude",
        cwd="/x",
        log_path="/x/l",
        status="live",
        created_at="2026-01-01T00:00:00Z",
        host_mode="exec",
    )
    base.update(over)
    return base


def test_reads_rust_minted_canonical_row(tmp_path):
    """A canonical-only row (the Rust mint shape) resolves, and the legacy alias
    is synced so an old reader still resolves it."""
    p = _write_registry_json(tmp_path, [_row(harness="claude", harness_session_id="H")])
    e = load_registry(p)[0]
    assert e.harness_session_id == "H"
    assert e.claude_session_uuid == "H"


def test_legacy_row_backfills_canonical(tmp_path):
    """AC1-EDGE: a pre-migration row (provider + claude_session_uuid) gains the
    canonical fields on load."""
    p = _write_registry_json(tmp_path, [_row(claude_session_uuid="U")])
    e = load_registry(p)[0]
    assert e.harness == "claude"
    assert e.harness_session_id == "U"


def test_conflict_resolves_canonical_wins(tmp_path):
    p = _write_registry_json(
        tmp_path, [_row(harness="claude", harness_session_id="CANON", claude_session_uuid="STALE")]
    )
    e = load_registry(p)[0]
    assert e.harness_session_id == "CANON"
    assert e.claude_session_uuid == "CANON"


def test_alien_harness_row_loads_without_bricking(tmp_path):
    """An alien harness string must degrade to a plain row, never a load error
    (harness is identity-only, not validated against KNOWN_PROVIDERS)."""
    p = _write_registry_json(
        tmp_path, [_row(provider="claude", harness="opencode", harness_session_id="X")]
    )
    e = load_registry(p)[0]
    assert e.harness == "opencode"
    assert e.harness_session_id == "X"


# ---------------------------------------------------------------------------
# Discovery resolves canonical-first
# ---------------------------------------------------------------------------


def test_discover_resolves_claude_canonical_only_row(tmp_path):
    """The class the bg-routing bug lives in: a row whose only identity is the
    canonical field is resolvable, not durable-only forever."""
    from fno.agents import discover

    p = _write_registry_json(
        tmp_path, [_row(harness="claude", harness_session_id="FULLUUID")]
    )
    rows = discover._discover_from_registry(p)
    assert any(r["session_id"] == "FULLUUID" for r in rows)


def test_discover_resolves_codex_canonical_field(tmp_path):
    """AC2-HP: a codex row with harness_session_id and no legacy codex_session_id
    still feeds the codex lane the thread id from the canonical field."""
    from fno.agents import discover

    p = _write_registry_json(
        tmp_path, [_row(provider="codex", harness="codex", harness_session_id="THREAD")]
    )
    rows = discover._discover_from_registry(p)
    assert any(r["session_id"] == "THREAD" and r["agent"] == "codex" for r in rows)


# ---------------------------------------------------------------------------
# whoami on any harness (AC2-FR)
# ---------------------------------------------------------------------------


def test_whoami_resolves_codex_via_harness_session_id():
    """A codex worker (no FNO_AGENT_SELF) resolves its own name by matching its
    ambient session id against the canonical harness_session_id."""
    from fno.agents import whoami

    row = AgentEntry(
        name="c1",
        provider="codex",
        cwd="/x",
        log_path="/x/l",
        harness="codex",
        harness_session_id="THREAD-1",
        codex_session_id="THREAD-1",
    )
    res = whoami.resolve_self(env={}, registry=[row], session_uuid="THREAD-1")
    assert res.registered
    assert res.name == "c1"
