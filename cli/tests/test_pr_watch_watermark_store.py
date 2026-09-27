"""Watermark store: atomic round-trips and corruption degradation."""

import json

import pytest


class TestWatermarkStore:
    """AC: atomic watermark store round-trips and degrades on corruption."""

    def test_load_missing_returns_empty(self, tmp_path):
        """AC-HP: missing file -> load() returns {} without raising."""
        from fno.pr_watch._state import WatermarkStore

        store = WatermarkStore(path=tmp_path / "pr-watcher-state.json")
        assert store.load() == {}

    def test_set_and_get_round_trip(self, tmp_path):
        """AC-HP: set() persists; get() retrieves the same dict."""
        from fno.pr_watch._state import WatermarkStore

        store = WatermarkStore(path=tmp_path / "pr-watcher-state.json")
        entry = {
            "last_review_ts": None,
            "last_seen_state": "OPEN",
            "merge_dispatched": False,
            "retries": 0,
            "parked": None,
        }
        store.set("owner/repo#1", entry)
        assert store.get("owner/repo#1") == entry

    def test_set_merges_with_an_external_write_landed_behind_its_back(self, tmp_path):
        """A park sweep (Rust) rewriting the store while this tick sat in a
        merge must survive: set() re-reads the disk under the flock instead
        of writing its load-once dict over the sweep's row."""
        from fno.pr_watch._state import WatermarkStore

        store_path = tmp_path / "pr-watcher-state.json"
        store = WatermarkStore(path=store_path)
        store.set("owner/repo#1", {"last_seen_state": "OPEN", "retries": 0, "parked": None})
        # The sweep lands a row the in-memory cache has never seen.
        data = json.loads(store_path.read_text())
        data["owner/repo#2"] = {"last_seen_state": "OPEN", "retries": 0, "parked": "checks-red"}
        store_path.write_text(json.dumps(data))
        # The tick's own write for its key must not clobber the sweep's row.
        store.set("owner/repo#1", {"last_seen_state": "OPEN", "retries": 1, "parked": None})
        final = json.loads(store_path.read_text())
        assert final["owner/repo#2"]["parked"] == "checks-red"
        assert final["owner/repo#1"]["retries"] == 1
        # The cache is left coherent with the disk.
        assert store.get("owner/repo#2")["parked"] == "checks-red"

    def test_atomic_persist_via_os_replace(self, tmp_path):
        """AC-VERIFY: persisted JSON is valid and contains the expected key."""
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "pr-watcher-state.json"
        store = WatermarkStore(path=path)
        store.set("owner/repo#42", {"last_seen_state": "MERGED", "merge_dispatched": True, "retries": 0, "parked": None, "last_review_ts": None})
        raw = json.loads(path.read_text())
        assert "owner/repo#42" in raw
        assert raw["owner/repo#42"]["merge_dispatched"] is True

    def test_corrupt_json_returns_empty_no_raise(self, tmp_path):
        """AC-ERR: corrupt JSON file -> load() returns {} and logs warning."""
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "pr-watcher-state.json"
        path.write_text("NOT VALID JSON {{{")
        store = WatermarkStore(path=path)
        result = store.load()
        assert result == {}

    def test_missing_repo_slug_refuses_ambiguous_key(self, tmp_path):
        """A PR number without a repository can never be persisted safely."""
        from fno.pr_watch._state import make_watermark_key

        with pytest.raises(ValueError, match="repo_slug is required"):
            make_watermark_key(repo_slug=None, pr_number=99)

    def test_slug_key_format(self, tmp_path):
        """AC-HP: normal slug key = 'owner/repo#N'."""
        from fno.pr_watch._state import make_watermark_key

        key = make_watermark_key(repo_slug="owner/repo", pr_number=7)
        assert key == "owner/repo#7"

    def test_ambiguous_bare_key_is_dropped_without_guessing_repo(self, tmp_path):
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "state.json"
        path.write_text(json.dumps({
            "316": {"last_seen_state": "OPEN"},
            "owner/one#316": {"last_seen_state": "OPEN"},
            "owner/two#316": {"last_seen_state": "OPEN"},
        }))
        store = WatermarkStore(path)

        receipt = store.normalize_keys()

        assert sorted(store.load()) == ["owner/one#316", "owner/two#316"]
        assert receipt.dropped == [
            {"key": "316", "reason": "ambiguous-key", "state": "OPEN"}
        ]

    def test_bare_key_never_collapses_into_a_candidate_only_twin(self, tmp_path):
        """A same-numbered current candidate is not identity evidence.

        Collapsing bare 316 into a candidate from another repository would
        transplant its parked record onto that live PR and suppress it
        forever, so the bare key is dropped instead.
        """
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "state.json"
        path.write_text(json.dumps({
            "316": {"last_seen_state": "OPEN", "parked": "retries-exhausted"},
        }))
        store = WatermarkStore(path)

        receipt = store.normalize_keys()

        assert store.load() == {}
        assert receipt.dropped == [
            {"key": "316", "reason": "unresolvable-key", "state": "OPEN"}
        ]

    def test_duplicate_collapse_keeps_terminal_last_seen_state(self, tmp_path):
        """A MERGED twin must not read back OPEN from its duplicate."""
        from fno.pr_watch._state import WatermarkStore

        path = tmp_path / "state.json"
        path.write_text(json.dumps({
            "owner/repo#9": {
                "last_seen_state": "OPEN",
                "merge_dispatched": False,
                "retries": 0,
                "parked": None,
                "last_review_ts": None,
            },
            "Owner/Repo#9": {
                "last_seen_state": "MERGED",
                "merge_dispatched": True,
                "retries": 0,
                "parked": None,
                "last_review_ts": None,
            },
        }))
        store = WatermarkStore(path)

        receipt = store.normalize_keys()

        assert list(store.load()) == ["owner/repo#9"]
        assert store.load()["owner/repo#9"]["last_seen_state"] == "MERGED"
        assert receipt.normalized

    def test_persist_survives_a_removed_tmp_file(self, tmp_path, monkeypatch):
        """A tmp file removed between write and replace aborts the
        whole merge queue walk. Retry the persist once instead."""
        import os as os_mod

        from fno.pr_watch import _state as state_mod
        from fno.pr_watch._state import WatermarkStore

        store = WatermarkStore(path=tmp_path / "pr-watcher-state.json")
        real_replace = os_mod.replace
        calls = {"n": 0}

        def flaky_replace(src, dst):
            calls["n"] += 1
            if calls["n"] == 1:
                raise FileNotFoundError(src)
            return real_replace(src, dst)

        monkeypatch.setattr(state_mod.os, "replace", flaky_replace)
        store.set("owner/repo#1", {"retries": 1})

        assert calls["n"] == 2, "the retry must have run"
        assert store.get("owner/repo#1")["retries"] == 1

