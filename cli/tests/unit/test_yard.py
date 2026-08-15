"""`fno yard`: the identity fold behind the Neko Atsume yard (x-b2bf).

Species stability (full-id key, unsalted hash), the rarity rank's exact
60/25/10/4/1 shape, and the first-sighting read over the album.
"""
from __future__ import annotations

import json
from types import SimpleNamespace

from typer.testing import CliRunner

from fno.cli import app
from fno.yard import citizen_id, fold, rarity_tiers, seen_species, species_for

runner = CliRunner()


def _row(name="w", harness="claude", sid=None, crown=None, created="2026-08-15T00:00:00Z"):
    return SimpleNamespace(
        name=name,
        harness=harness,
        harness_session_id=sid,
        crown_level=crown,
        created_at=created,
    )


# -- species identity --------------------------------------------------------


def test_species_is_stable_per_full_id():
    assert species_for("session-abc") == species_for("session-abc")
    # 24 distinct full ids must not collapse onto one species.
    assert len({species_for(f"session-{i}") for i in range(24)}) > 1


def test_citizen_id_is_the_full_id_never_the_name():
    # Same display name, distinct session ids: identity follows the id.
    a, b = _row(name="worker", sid="11111111-aaaa"), _row(name="worker", sid="22222222-bbbb")
    assert citizen_id(a) == "11111111-aaaa"
    assert citizen_id(b) == "22222222-bbbb"
    # Legacy row with no session id degrades to a per-row stable key.
    assert citizen_id(_row(name="old", created="2026-01-01T00:00:00Z")) == "old@2026-01-01T00:00:00Z"


# -- rarity ------------------------------------------------------------------


def test_rarity_matches_the_weight_shape_exactly():
    pop = ["a"] * 60 + ["b"] * 25 + ["c"] * 10 + ["d"] * 4 + ["e"]
    tiers = rarity_tiers(pop)
    assert tiers == {
        "a": "common",
        "b": "uncommon",
        "c": "rare",
        "d": "epic",
        "e": "legendary",
    }


def test_rarity_majority_common_single_occurrence_legendary():
    tiers = rarity_tiers(["claude"] * 100 + ["codex"])
    assert tiers["claude"] == "common"
    assert tiers["codex"] == "legendary"


def test_rarity_single_value_population_reads_common():
    assert rarity_tiers(["claude"] * 7) == {"claude": "common"}


def test_rarity_empty_population_is_empty():
    assert rarity_tiers([]) == {}


# -- first sighting ----------------------------------------------------------


def test_first_sighting_is_species_dimension_only():
    archive = [
        {
            "id": "x-1",
            "status": "done",
            "source_session_id": "seen-id",
            "sessions": [{"session_id": "nested-seen"}],
        }
    ]
    seen = seen_species(archive)
    assert species_for("seen-id") in seen
    assert species_for("nested-seen") in seen
    assert species_for("never-recorded") not in seen


def test_fold_marks_first_sighting_against_the_album():
    archive = [{"id": "x-1", "status": "done", "source_session_id": "shared-id"}]
    rows = [
        _row(name="veteran", sid="shared-id"),  # same species as the album's cat
        _row(name="newcomer", sid="brand-new-id"),
    ]
    citizens = fold(rows, archive)
    by_name = {c["name"]: c for c in citizens}
    assert by_name["veteran"]["first_sighting"] is False
    assert by_name["newcomer"]["first_sighting"] is True


# -- fold shape --------------------------------------------------------------


def test_fold_shape_order_and_crown_default():
    rows = [_row(name="zed", sid="s1"), _row(name="alpha", sid="s2", crown=2)]
    citizens = fold(rows, [])
    assert [c["name"] for c in citizens] == ["alpha", "zed"]  # sorted by name
    alpha = citizens[0]
    assert set(alpha) == {
        "id",
        "name",
        "harness",
        "species",
        "rarity",
        "crown_level",
        "first_sighting",
    }
    assert alpha["crown_level"] == 2
    assert citizens[1]["crown_level"] == 0  # None reads as 0, never renders a hat


def test_fold_empty_registry_is_empty():
    assert fold([], []) == []


# -- CLI surface -------------------------------------------------------------


def test_yard_cli_json_emits_citizens(tmp_path, monkeypatch):
    import fno.agents.registry as registry_mod
    import fno.paths as paths

    monkeypatch.setattr(
        registry_mod,
        "load_registry",
        lambda: [_row(name="cli-cat", sid="cli-id", crown=1)],
    )
    archive = tmp_path / "graph-archive.json"
    monkeypatch.setattr(paths, "graph_archive_json", lambda: archive)
    r = runner.invoke(app, ["yard", "--json"])
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    (c,) = payload["citizens"]
    assert c["name"] == "cli-cat"
    assert c["rarity"] == "common"  # single-harness population
    assert c["crown_level"] == 1


def test_yard_cli_text_lists_citizens(tmp_path, monkeypatch):
    import fno.agents.registry as registry_mod
    import fno.paths as paths

    monkeypatch.setattr(
        registry_mod,
        "load_registry",
        lambda: [_row(name="cli-cat", sid="cli-id")],
    )
    monkeypatch.setattr(paths, "graph_archive_json", lambda: tmp_path / "missing.json")
    r = runner.invoke(app, ["yard"])
    assert r.exit_code == 0, r.output
    assert "cli-cat" in r.output
    assert "1 citizens" in r.output
