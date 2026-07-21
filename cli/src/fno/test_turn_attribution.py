"""Tests for turn attribution sidecar.

Run: cd cli && uv run pytest src/fno/test_turn_attribution.py -v

Phase 02 of provider rotation failover (ab-9728b70b). Sidecar lives at
.fno/turn-attribution.jsonl with one line per assistant turn,
written by the dispatch layer and read by cost.py + the stop hook.
"""
from __future__ import annotations

import json
import logging
from pathlib import Path

import pytest

from fno.turn_attribution import (
    SIDECAR_FILENAME,
    TurnAttribution,
    iter_turn_attributions,
    record_turn,
    summarize_per_provider,
)


class TestRecordTurn:
    def test_hp1_successful_turn_writes_line_with_active_provider_no_error(
        self, tmp_path: Path,
    ):
        sidecar = tmp_path / SIDECAR_FILENAME

        record_turn(
            sidecar_path=sidecar,
            turn_index=0,
            ts="2026-05-05T01:23:45Z",
            provider_id="claude-anthropic",
            error_class=None,
        )

        lines = sidecar.read_text().splitlines()
        assert len(lines) == 1
        entry = json.loads(lines[0])
        assert entry["turn_index"] == 0
        assert entry["ts"] == "2026-05-05T01:23:45Z"
        assert entry["provider_id"] == "claude-anthropic"
        assert entry["error_class"] is None

    def test_hp2_failed_turn_writes_line_with_error_class(self, tmp_path: Path):
        sidecar = tmp_path / SIDECAR_FILENAME

        record_turn(
            sidecar_path=sidecar,
            turn_index=2,
            ts="2026-05-05T01:24:48Z",
            provider_id="claude-openrouter",
            error_class="provider_5xx",
        )

        entry = json.loads(sidecar.read_text().splitlines()[0])
        assert entry["error_class"] == "provider_5xx"

    def test_appends_to_existing_sidecar(self, tmp_path: Path):
        sidecar = tmp_path / SIDECAR_FILENAME
        record_turn(sidecar_path=sidecar, turn_index=0, ts="2026-05-05T00:00:00Z",
                    provider_id="a", error_class=None)
        record_turn(sidecar_path=sidecar, turn_index=1, ts="2026-05-05T00:01:00Z",
                    provider_id="a", error_class=None)
        record_turn(sidecar_path=sidecar, turn_index=2, ts="2026-05-05T00:02:00Z",
                    provider_id="b", error_class="provider_5xx")

        lines = sidecar.read_text().splitlines()
        assert len(lines) == 3
        ids = [json.loads(ln)["provider_id"] for ln in lines]
        assert ids == ["a", "a", "b"]

    def test_err1_sidecar_write_failure_is_non_blocking(
        self, tmp_path: Path, caplog: pytest.LogCaptureFixture, monkeypatch,
    ):
        """A turn must NOT propagate an exception from the sidecar writer.
        The stamp is observability, not load-bearing."""
        sidecar = tmp_path / "no" / "perm" / SIDECAR_FILENAME
        # Force an OSError by making the parent unwriteable
        sidecar.parent.mkdir(parents=True, exist_ok=True)
        sidecar.parent.chmod(0o500)

        try:
            with caplog.at_level(logging.WARNING, logger="fno.turn_attribution"):
                # Must not raise
                record_turn(
                    sidecar_path=sidecar,
                    turn_index=0,
                    ts="2026-05-05T00:00:00Z",
                    provider_id="x",
                    error_class=None,
                )
        finally:
            sidecar.parent.chmod(0o755)

        # Should have logged a warning rather than raising
        assert any("turn-attribution" in r.message.lower() or
                   "sidecar" in r.message.lower()
                   for r in caplog.records), caplog.records

    def test_creates_parent_directory_if_missing(self, tmp_path: Path):
        sidecar = tmp_path / "subdir" / SIDECAR_FILENAME
        record_turn(
            sidecar_path=sidecar,
            turn_index=0,
            ts="2026-05-05T00:00:00Z",
            provider_id="x",
            error_class=None,
        )
        assert sidecar.exists()

    def test_concurrent_appends_are_line_atomic(self, tmp_path: Path):
        """fcntl.LOCK_EX serializes appends so the JSONL stays parseable
        even with concurrent writers from multiple processes/threads."""
        import threading

        sidecar = tmp_path / SIDECAR_FILENAME

        def worker(thread_id: int):
            for turn in range(20):
                record_turn(
                    sidecar_path=sidecar,
                    turn_index=turn,
                    ts=f"2026-05-05T00:{thread_id:02d}:{turn:02d}Z",
                    provider_id=f"thread-{thread_id}",
                    error_class=None,
                )

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        # Every line must parse as JSON; total = 8*20 = 160
        lines = sidecar.read_text().splitlines()
        assert len(lines) == 160
        for line in lines:
            entry = json.loads(line)  # raises if any line is partial
            assert "provider_id" in entry


class TestIterAndSummarize:
    def test_iter_yields_parsed_records(self, tmp_path: Path):
        sidecar = tmp_path / SIDECAR_FILENAME
        record_turn(sidecar_path=sidecar, turn_index=0, ts="t0",
                    provider_id="a", error_class=None)
        record_turn(sidecar_path=sidecar, turn_index=1, ts="t1",
                    provider_id="b", error_class="provider_5xx")

        records = list(iter_turn_attributions(sidecar_path=sidecar))
        assert len(records) == 2
        assert records[0].provider_id == "a"
        assert records[0].error_class is None
        assert records[1].provider_id == "b"
        assert records[1].error_class == "provider_5xx"

    def test_iter_returns_empty_when_sidecar_missing(self, tmp_path: Path):
        sidecar = tmp_path / "absent" / SIDECAR_FILENAME
        assert list(iter_turn_attributions(sidecar_path=sidecar)) == []

    def test_iter_skips_malformed_lines(self, tmp_path: Path):
        sidecar = tmp_path / SIDECAR_FILENAME
        sidecar.write_text(
            '{"turn_index":0,"ts":"t0","provider_id":"a","error_class":null}\n'
            'not json\n'
            '{"turn_index":2,"ts":"t2","provider_id":"b","error_class":null}\n'
        )
        records = list(iter_turn_attributions(sidecar_path=sidecar))
        assert len(records) == 2
        assert records[0].provider_id == "a"
        assert records[1].provider_id == "b"

    def test_edge1_summarize_per_provider_breakdown(self, tmp_path: Path):
        """Cites what-if finding #9. Sidecar yields a per-provider breakdown
        (count of turns + count of errors) so cost.py and the stop hook can
        attribute work even though the math (rate × tokens) is Spec 2.5."""
        sidecar = tmp_path / SIDECAR_FILENAME
        # 30 turns on A, 60 turns on B (one of A's was an error)
        for i in range(30):
            record_turn(sidecar_path=sidecar, turn_index=i, ts=f"a{i}",
                        provider_id="A",
                        error_class="provider_5xx" if i == 29 else None)
        for i in range(60):
            record_turn(sidecar_path=sidecar, turn_index=30 + i, ts=f"b{i}",
                        provider_id="B", error_class=None)

        summary = summarize_per_provider(sidecar_path=sidecar)
        assert summary["A"]["turns"] == 30
        assert summary["A"]["errors"] == 1
        assert summary["B"]["turns"] == 60
        assert summary["B"]["errors"] == 0

    def test_edge2_timestamps_disambiguate_swap_boundary(self, tmp_path: Path):
        """Cites what-if finding #8. A phase_transition under provider A
        followed 200ms later by a gate artifact must be disambiguatable: the
        sidecar's ts field tells which side of the swap each turn falls on."""
        sidecar = tmp_path / SIDECAR_FILENAME
        record_turn(sidecar_path=sidecar, turn_index=10, ts="2026-05-05T01:00:00.000Z",
                    provider_id="A", error_class=None)
        record_turn(sidecar_path=sidecar, turn_index=11, ts="2026-05-05T01:00:00.200Z",
                    provider_id="B", error_class=None)

        records = list(iter_turn_attributions(sidecar_path=sidecar))
        assert records[0].ts == "2026-05-05T01:00:00.000Z"
        assert records[0].provider_id == "A"
        assert records[1].ts == "2026-05-05T01:00:00.200Z"
        assert records[1].provider_id == "B"

    def test_summarize_empty_when_sidecar_missing(self, tmp_path: Path):
        sidecar = tmp_path / "absent" / SIDECAR_FILENAME
        assert summarize_per_provider(sidecar_path=sidecar) == {}


class TestTurnAttributionShape:
    def test_dataclass_is_frozen(self):
        ta = TurnAttribution(turn_index=0, ts="t", provider_id="a", error_class=None)
        with pytest.raises((AttributeError, Exception)):
            ta.provider_id = "tampered"  # type: ignore[misc]
