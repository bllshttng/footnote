"""Tests for cost.update() ledger extension - provider attribution.

Phase 04 of provider rotation substrate (ab-256f6b6e).

Covers:
  AC04.1-HP  Tagged write round-trip
  AC04.2-FR  Untagged write does not pollute the entry
  AC04.3-FR  Mixed-schema ledger reads cleanly
  AC04.6-EDGE Atomic write under provider tag
  Task 04.4 bullets: both keys, no keys, mixed-schema read, concurrent writes
"""
from __future__ import annotations

import json
import concurrent.futures
from pathlib import Path

import pytest

from fno.cost import update, _append_to_ledger


# ---------------------------------------------------------------------------
# AC04.1-HP: Tagged write round-trip
# ---------------------------------------------------------------------------

class TestUpdateWithProviderTag:
    def test_AC04_1_HP_tagged_write_contains_provider_fields(self, tmp_path: Path):
        """Given provider_id and account_id are passed, the entry in ledger.json
        must contain exactly those values under those keys."""
        ledger = tmp_path / "ledger.json"

        result = update(
            "session-abc",
            100,
            0.5,
            ledger_path=ledger,
            provider_id="claude-max-secondary",
            account_id="account-secondary",
        )

        assert result["ok"] is True
        assert ledger.exists()

        data = json.loads(ledger.read_text())
        entries = data if isinstance(data, list) else data.get("entries", [])
        assert len(entries) == 1
        entry = entries[0]
        assert entry["provider_id"] == "claude-max-secondary"
        assert entry["account_id"] == "account-secondary"
        # Core fields still present
        assert entry["session_id"] == "session-abc"
        assert entry["tokens"] == 100
        assert entry["cost_usd"] == 0.5
        assert "timestamp" in entry

    def test_AC04_1_HP_result_entry_has_provider_fields(self, tmp_path: Path):
        """The returned entry dict should also carry provider_id / account_id."""
        ledger = tmp_path / "ledger.json"
        result = update(
            "session-xyz",
            50,
            0.25,
            ledger_path=ledger,
            provider_id="claude-max-primary",
            account_id="account-primary",
        )
        assert result["entry"]["provider_id"] == "claude-max-primary"
        assert result["entry"]["account_id"] == "account-primary"


# ---------------------------------------------------------------------------
# AC04.2-FR: Untagged write does not pollute the entry (no None / null keys)
# ---------------------------------------------------------------------------

class TestUpdateWithoutProviderTag:
    def test_AC04_2_FR_untagged_write_has_no_provider_key(self, tmp_path: Path):
        """When provider_id / account_id are omitted, the entry dict must NOT
        contain those keys at all (not even as None / null)."""
        ledger = tmp_path / "ledger.json"
        update("session-old", 200, 1.0, ledger_path=ledger)

        data = json.loads(ledger.read_text())
        entries = data if isinstance(data, list) else data.get("entries", [])
        assert len(entries) == 1
        entry = entries[0]
        assert "provider_id" not in entry, (
            f"provider_id must be absent from untagged entry, got {entry}"
        )
        assert "account_id" not in entry, (
            f"account_id must be absent from untagged entry, got {entry}"
        )

    def test_AC04_2_FR_none_values_not_in_ledger(self, tmp_path: Path):
        """Explicitly passing None for both should still produce a clean entry."""
        ledger = tmp_path / "ledger.json"
        update("session-none", 10, 0.01, ledger_path=ledger,
               provider_id=None, account_id=None)

        data = json.loads(ledger.read_text())
        entries = data if isinstance(data, list) else data.get("entries", [])
        entry = entries[0]
        assert "provider_id" not in entry
        assert "account_id" not in entry


# ---------------------------------------------------------------------------
# AC04.3-FR: Mixed-schema ledger reads cleanly
# ---------------------------------------------------------------------------

class TestMixedSchemaLedger:
    def test_AC04_3_FR_old_entries_readable(self, tmp_path: Path):
        """A ledger with 5 old-format entries and 5 new-format entries must
        be appended to without error, and both kinds survive round-trip."""
        ledger = tmp_path / "ledger.json"

        # Pre-populate with 5 old-format entries (no provider fields)
        old_entries = [
            {
                "session_id": f"old-session-{i}",
                "tokens": 100 * i,
                "cost_usd": 0.1 * i,
                "timestamp": "2026-01-01T00:00:00Z",
            }
            for i in range(1, 6)
        ]
        ledger.write_text(json.dumps(old_entries))

        # Write 5 new-format entries
        for i in range(1, 6):
            update(
                f"new-session-{i}",
                200 * i,
                0.2 * i,
                ledger_path=ledger,
                provider_id=f"provider-{i}",
                account_id=f"account-{i}",
            )

        data = json.loads(ledger.read_text())
        entries = data if isinstance(data, list) else data.get("entries", [])
        assert len(entries) == 10, f"Expected 10 entries, got {len(entries)}"

        # Old entries still intact and without provider keys
        for e in entries[:5]:
            assert "provider_id" not in e

        # New entries have provider keys
        for i, e in enumerate(entries[5:], 1):
            assert e["provider_id"] == f"provider-{i}"

    def test_mixed_schema_no_crash_on_append(self, tmp_path: Path):
        """Appending to a ledger with mixed-schema entries must not raise."""
        ledger = tmp_path / "ledger.json"
        mixed = [
            {"session_id": "old", "tokens": 10, "cost_usd": 0.01, "timestamp": "2026-01-01T00:00:00Z"},
            {"session_id": "new", "tokens": 20, "cost_usd": 0.02, "timestamp": "2026-01-01T00:00:00Z",
             "provider_id": "some-provider", "account_id": "some-account"},
        ]
        ledger.write_text(json.dumps(mixed))

        # Should not raise
        update("another", 30, 0.03, ledger_path=ledger)
        data = json.loads(ledger.read_text())
        entries = data if isinstance(data, list) else data.get("entries", [])
        assert len(entries) == 3


# ---------------------------------------------------------------------------
# AC04.6-EDGE: Atomic write under concurrent provider tag writes
# ---------------------------------------------------------------------------

class TestConcurrentAtomicWrite:
    def test_AC04_6_EDGE_concurrent_writes_both_land(self, tmp_path: Path):
        """Two concurrent update() calls with different provider_ids must both
        complete, leaving exactly 2 entries in the ledger with correct provider_ids."""
        ledger = tmp_path / "ledger.json"

        def write_entry(session_id: str, provider: str) -> dict:
            return update(
                session_id,
                100,
                0.1,
                ledger_path=ledger,
                provider_id=provider,
                account_id=f"account-for-{provider}",
            )

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            fut1 = pool.submit(write_entry, "sess-provider-a", "provider-a")
            fut2 = pool.submit(write_entry, "sess-provider-b", "provider-b")
            r1 = fut1.result(timeout=10)
            r2 = fut2.result(timeout=10)

        assert r1["ok"] is True
        assert r2["ok"] is True

        data = json.loads(ledger.read_text())
        entries = data if isinstance(data, list) else data.get("entries", [])
        assert len(entries) == 2, (
            f"Expected 2 entries after concurrent writes, got {len(entries)}: {entries}"
        )

        provider_ids = {e["provider_id"] for e in entries}
        assert provider_ids == {"provider-a", "provider-b"}, (
            f"Both provider_ids should land, got: {provider_ids}"
        )

    def test_AC04_6_EDGE_file_never_partial(self, tmp_path: Path):
        """After concurrent writes the ledger.json must be valid JSON (never half-written)."""
        ledger = tmp_path / "ledger.json"
        errors: list[str] = []

        def write_and_check(session_id: str, provider: str) -> None:
            update(session_id, 100, 0.1, ledger_path=ledger, provider_id=provider)
            try:
                json.loads(ledger.read_text())
            except json.JSONDecodeError as exc:
                errors.append(f"{session_id}: {exc}")

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futs = [
                pool.submit(write_and_check, f"sess-{i}", f"provider-{i}")
                for i in range(4)
            ]
            for f in concurrent.futures.as_completed(futs):
                f.result()

        assert not errors, f"Partial writes detected: {errors}"


# ---------------------------------------------------------------------------
# Schema unification: cost.py must read/write the {"entries": [...]} dict shape
# that register-task.py also uses (Gemini Code Assist CRITICAL finding PR #199)
# ---------------------------------------------------------------------------

class TestLedgerSchemaUnification:
    def test_append_to_ledger_writes_dict_shape(self, tmp_path: Path):
        """_append_to_ledger must write {"entries": [...]} so register-task.py can read it."""
        ledger = tmp_path / "ledger.json"
        update("s1", 1, 0.01, ledger_path=ledger)
        data = json.loads(ledger.read_text())
        assert isinstance(data, dict), (
            f"must write dict shape so register-task.py can read it, got {type(data)}"
        )
        assert "entries" in data, f"missing 'entries' key, got keys: {list(data.keys())}"
        assert len(data["entries"]) == 1

    def test_append_to_ledger_reads_register_task_dict_shape(self, tmp_path: Path):
        """cost.update must read and preserve a dict-shape ledger written by register-task.py."""
        ledger = tmp_path / "ledger.json"
        # Simulate register-task.py having written first
        existing = {"entries": [{"session_id": "old", "tokens": 10, "cost_usd": 0.1, "timestamp": "2026-01-01T00:00:00Z"}]}
        ledger.write_text(json.dumps(existing))

        update("new", 20, 0.2, ledger_path=ledger)

        data = json.loads(ledger.read_text())
        assert isinstance(data, dict), "must write dict shape after reading dict-shape ledger"
        assert "entries" in data
        assert len(data["entries"]) == 2, f"old + new must both be preserved, got {len(data['entries'])}"
        ids = [e["session_id"] for e in data["entries"]]
        assert "old" in ids and "new" in ids

    def test_append_to_ledger_tolerates_bare_list_back_compat(self, tmp_path: Path):
        """Reading a bare-list ledger (written by pre-fix cost.py) must not wipe entries."""
        ledger = tmp_path / "ledger.json"
        # Pre-fix cost.py wrote bare JSON lists
        old_entries = [
            {"session_id": "legacy-1", "tokens": 5, "cost_usd": 0.05, "timestamp": "2026-01-01T00:00:00Z"},
            {"session_id": "legacy-2", "tokens": 7, "cost_usd": 0.07, "timestamp": "2026-01-01T01:00:00Z"},
        ]
        ledger.write_text(json.dumps(old_entries))

        update("new-after-migration", 10, 0.1, ledger_path=ledger)

        data = json.loads(ledger.read_text())
        assert isinstance(data, dict), "must upgrade bare-list to dict shape"
        assert len(data["entries"]) == 3, "both legacy entries + new must be preserved"
        ids = [e["session_id"] for e in data["entries"]]
        assert "legacy-1" in ids and "legacy-2" in ids and "new-after-migration" in ids


# ---------------------------------------------------------------------------
# Phase 02 of provider rotation failover (ab-9728b70b): cost.py wires through
# the per-turn attribution sidecar so cost-aware callers don't need to import
# the lower-level turn_attribution module directly.
# ---------------------------------------------------------------------------

class TestPerTurnAttribution:
    def test_compute_per_turn_attribution_returns_rollup(self, tmp_path):
        from fno.cost import compute_per_turn_attribution
        from fno.turn_attribution import SIDECAR_FILENAME, record_turn

        sidecar = tmp_path / SIDECAR_FILENAME
        for i in range(3):
            record_turn(sidecar_path=sidecar, turn_index=i, ts=f"a{i}",
                        provider_id="A", error_class=None)
        record_turn(sidecar_path=sidecar, turn_index=3, ts="b0",
                    provider_id="B", error_class="provider_5xx")

        summary = compute_per_turn_attribution(sidecar_path=sidecar)
        assert summary == {
            "A": {"turns": 3, "errors": 0},
            "B": {"turns": 1, "errors": 1},
        }

    def test_compute_per_turn_attribution_empty_when_legacy(self, tmp_path):
        """Legacy sessions predating the sidecar return an empty dict so
        callers can fall back to the active-at-compute attribution that
        cost.update already supports."""
        from fno.cost import compute_per_turn_attribution

        sidecar = tmp_path / "absent" / "turn-attribution.jsonl"
        assert compute_per_turn_attribution(sidecar_path=sidecar) == {}


# ---------------------------------------------------------------------------
# Phase 03 task 3.2: per-provider sub-cap on cost_cap_usd.
# Phase 03 of provider rotation failover (ab-9728b70b).
# ---------------------------------------------------------------------------

class TestPerProviderCap:
    def test_compute_per_provider_cost_attributes_by_turn_share(self, tmp_path):
        """v0 math: total session cost × (turns on provider / total turns).
        The exact per-segment math (rate × tokens per segment) is Spec 2.5.
        v0's job is to bound damage with a "cheap and approximate" sub-cap."""
        from fno.cost import compute_per_provider_cost
        from fno.turn_attribution import SIDECAR_FILENAME, record_turn

        sidecar = tmp_path / SIDECAR_FILENAME
        # 30 turns on A, 60 turns on B = 1:2 ratio
        for i in range(30):
            record_turn(sidecar_path=sidecar, turn_index=i, ts=f"a{i}",
                        provider_id="A", error_class=None)
        for i in range(60):
            record_turn(sidecar_path=sidecar, turn_index=30 + i, ts=f"b{i}",
                        provider_id="B", error_class=None)

        result = compute_per_provider_cost(
            total_session_cost_usd=90.0,
            sidecar_path=sidecar,
        )
        # 30/90 of $90 = $30 to A, 60/90 of $90 = $60 to B.
        assert result["A"] == pytest.approx(30.0)
        assert result["B"] == pytest.approx(60.0)

    def test_hp1_single_provider_under_cap_no_block(self, tmp_path):
        from fno.cost import (
            compute_per_provider_cost,
            check_per_provider_caps,
        )
        from fno.turn_attribution import SIDECAR_FILENAME, record_turn

        sidecar = tmp_path / SIDECAR_FILENAME
        for i in range(10):
            record_turn(sidecar_path=sidecar, turn_index=i, ts=f"a{i}",
                        provider_id="claude-anthropic", error_class=None)

        per_provider = compute_per_provider_cost(
            total_session_cost_usd=20.0, sidecar_path=sidecar,
        )
        # Cap = $30, spend = $20: under cap
        result = check_per_provider_caps(
            per_provider_cost=per_provider,
            caps_by_provider={"claude-anthropic": 30.0},
        )
        assert result.tripped is False
        assert result.tripped_provider is None

    def test_hp2_single_provider_over_cap_blocks(self, tmp_path):
        from fno.cost import (
            compute_per_provider_cost,
            check_per_provider_caps,
        )
        from fno.turn_attribution import SIDECAR_FILENAME, record_turn

        sidecar = tmp_path / SIDECAR_FILENAME
        for i in range(10):
            record_turn(sidecar_path=sidecar, turn_index=i, ts=f"a{i}",
                        provider_id="claude-anthropic", error_class=None)

        per_provider = compute_per_provider_cost(
            total_session_cost_usd=32.0, sidecar_path=sidecar,
        )
        result = check_per_provider_caps(
            per_provider_cost=per_provider,
            caps_by_provider={"claude-anthropic": 30.0},
        )
        assert result.tripped is True
        assert result.tripped_provider == "claude-anthropic"
        assert result.tripped_amount_usd == pytest.approx(32.0)

    def test_err1_mixed_providers_one_over(self, tmp_path):
        from fno.cost import (
            compute_per_provider_cost,
            check_per_provider_caps,
        )
        from fno.turn_attribution import SIDECAR_FILENAME, record_turn

        sidecar = tmp_path / SIDECAR_FILENAME
        # 25 on A, 32 on B (in dollar-share terms)
        for i in range(25):
            record_turn(sidecar_path=sidecar, turn_index=i, ts=f"a{i}",
                        provider_id="A", error_class=None)
        for i in range(32):
            record_turn(sidecar_path=sidecar, turn_index=25 + i, ts=f"b{i}",
                        provider_id="B", error_class=None)

        per_provider = compute_per_provider_cost(
            total_session_cost_usd=57.0, sidecar_path=sidecar,
        )
        # A gets 25/57 of 57 = 25, B gets 32/57 of 57 = 32
        assert per_provider["A"] == pytest.approx(25.0)
        assert per_provider["B"] == pytest.approx(32.0)

        result = check_per_provider_caps(
            per_provider_cost=per_provider,
            caps_by_provider={"A": 30.0, "B": 30.0},
        )
        assert result.tripped is True
        assert result.tripped_provider == "B"

    def test_provider_without_cap_is_ignored(self, tmp_path):
        """When a provider has no cost_cap_usd_per_session set, no per-
        provider check fires for it."""
        from fno.cost import (
            compute_per_provider_cost,
            check_per_provider_caps,
        )
        from fno.turn_attribution import SIDECAR_FILENAME, record_turn

        sidecar = tmp_path / SIDECAR_FILENAME
        for i in range(5):
            record_turn(sidecar_path=sidecar, turn_index=i, ts=f"x{i}",
                        provider_id="X", error_class=None)

        per_provider = compute_per_provider_cost(
            total_session_cost_usd=999.0, sidecar_path=sidecar,
        )
        # X has no cap entry: should not trip even though spend is huge
        result = check_per_provider_caps(
            per_provider_cost=per_provider, caps_by_provider={},
        )
        assert result.tripped is False

    def test_empty_sidecar_returns_empty_per_provider(self, tmp_path):
        from fno.cost import compute_per_provider_cost

        sidecar = tmp_path / "no" / "such" / "sidecar.jsonl"
        result = compute_per_provider_cost(
            total_session_cost_usd=10.0, sidecar_path=sidecar,
        )
        # Legacy session: empty sidecar means no per-provider attribution.
        assert result == {}

    def test_edge2_storm_bounds_damage_at_subcap(self, tmp_path):
        """Cites what-if finding #12: an unattended overnight run with a
        thrashing provider eating $0.50/cycle has its damage bounded at
        the per-provider sub-cap rather than running unbounded."""
        from fno.cost import (
            compute_per_provider_cost,
            check_per_provider_caps,
        )
        from fno.turn_attribution import SIDECAR_FILENAME, record_turn

        sidecar = tmp_path / SIDECAR_FILENAME
        # 20 thrash cycles attributed entirely to bad-provider.
        for i in range(20):
            record_turn(sidecar_path=sidecar, turn_index=i, ts=f"x{i}",
                        provider_id="bad-provider",
                        error_class="provider_5xx")

        # session_cost includes the $10 spent on the thrash.
        per_provider = compute_per_provider_cost(
            total_session_cost_usd=10.0, sidecar_path=sidecar,
        )
        result = check_per_provider_caps(
            per_provider_cost=per_provider,
            caps_by_provider={"bad-provider": 10.0},
        )
        # At-or-above cap: trips.
        assert result.tripped is True
        assert result.tripped_provider == "bad-provider"
