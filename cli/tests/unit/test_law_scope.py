"""Law scope: the door stamps in Rust, the chokepoint filter, both fail visibly.

The scope stamp and the widening live in the Rust record door behind the
front's `fno inbox law` group (`crates/fno-agents/src/law_match.rs`, the
record-door and scope_tests sections); what runs here are the transport
contracts that stay in Python: `scope-split` filters `list_decisions`, and a
project that cannot be resolved REFUSES at the door but fails OPEN in a reader.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from tests.unit._front_dev import front_dev_binary, worker_dev_binary

pytestmark = pytest.mark.skipif(
    front_dev_binary() is None,
    reason="compiled fno front binary not present (build with `cargo build --manifest-path crates/fno/Cargo.toml --bin fno)`",
)


@pytest.fixture(autouse=True)
def _point_the_front_at_the_dev_worker(monkeypatch: pytest.MonkeyPatch) -> None:
    """The front serves the law door by spawning the runtime worker; a dev
    checkout's front must never answer from an installed worker."""
    worker = worker_dev_binary()
    if worker is not None:
        monkeypatch.setenv("FNO_AGENTS_WORKER", str(worker))


def _work_map(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, slug: str = "demo") -> Path:
    """A hermetic settings work map: <tmp>/proj resolves to <slug>.

    FNO_GLOBAL_SETTINGS_PATH names the global settings FILE; the crate reads
    that file and its config.toml sibling, so the map rides settings.yaml.
    """
    proj = tmp_path / "proj"
    proj.mkdir(exist_ok=True)
    map_file = tmp_path / "settings.yaml"
    map_file.write_text(
        "work:\n"
        "  workspaces:\n"
        "    main:\n      projects:\n"
        f"        - name: {slug}\n"
        f"          path: {proj}\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(map_file))
    return proj


def test_scope_split_keeps_global_and_the_matching_project(tmp_path, monkeypatch):
    _work_map(tmp_path, monkeypatch)
    monkeypatch.chdir(tmp_path / "proj")
    from fno.rust_binary import call_front_json

    rows = [
        {"decision_id": "d-1", "lane": "law", "scope": "global"},
        {"decision_id": "d-2", "lane": "law", "scope": "project:demo"},
        {"decision_id": "d-3", "lane": "coord", "scope": "project:etl"},
        {"decision_id": "d-4", "lane": "coord"},
        {"decision_id": "d-5", "lane": "law", "scope": "project:etl"},
    ]
    answer = call_front_json({"mode": "scope-split", "rows": rows})
    kept = [r["decision_id"] for r in answer["kept"]]
    assert kept == ["d-1", "d-2", "d-3", "d-4"]
    assert answer["hidden"] == 1
    assert "hid 1 out-of-scope" in answer["note"]


def test_scope_split_absent_scope_reads_project_fno(tmp_path, monkeypatch):
    _work_map(tmp_path, monkeypatch, slug="other")
    monkeypatch.chdir(tmp_path / "proj")
    from fno.rust_binary import call_front_json

    rows = [
        {"decision_id": "d-1", "lane": "law"},
        {"decision_id": "d-2", "lane": "law", "scope": "project:fno"},
    ]
    answer = call_front_json({"mode": "scope-split", "rows": rows})
    kept = [r["decision_id"] for r in answer["kept"]]
    assert kept == []
    assert answer["hidden"] == 2


def test_scope_split_fails_open_when_the_project_cannot_resolve(tmp_path, monkeypatch):
    _work_map(tmp_path, monkeypatch)
    nowhere = tmp_path / "nowhere"
    nowhere.mkdir()
    monkeypatch.chdir(nowhere)
    from fno.rust_binary import call_front_json

    rows = [{"decision_id": "d-1", "lane": "law", "scope": "project:demo"}]
    answer = call_front_json({"mode": "scope-split", "rows": rows})
    assert [r["decision_id"] for r in answer["kept"]] == ["d-1"]
    assert answer["note"] == ""


def test_list_decisions_applies_the_filter_and_the_note(tmp_path, monkeypatch):
    captured = {}

    def fake_front(payload):
        captured.update(payload)
        kept = [r for r in payload["rows"] if r.get("scope") != "project:etl"]
        return {"kept": kept, "hidden": 1, "note": " (hid 1 out-of-scope)"}

    import fno.rust_binary

    monkeypatch.setattr(fno.rust_binary, "call_front_json", fake_front)
    from fno.decide import list_decisions

    label, out, damaged = list_decisions(scope="current")
    assert captured["mode"] == "scope-split"
    assert label.endswith("(hid 1 out-of-scope)")


def test_list_decisions_fails_open_when_the_front_is_unavailable(tmp_path, monkeypatch):
    import fno.rust_binary

    def boom(payload):
        raise RuntimeError("front down")

    monkeypatch.setattr(fno.rust_binary, "call_front_json", boom)
    from fno.decide import list_decisions

    label, out, damaged = list_decisions(scope="current")
    assert "scope filter unavailable; nothing hidden" in label
