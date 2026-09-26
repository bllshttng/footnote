"""Law scope: the door stamps in Rust, the chokepoint filter, both fail visibly.

The scope stamp and the widening moved to the Rust record door
(`crates/fno-agents/src/law_match.rs`, the record-door and scope_tests
sections); what stays here are the transport contracts that still run in
Python: `scope-split` filters `list_decisions`, and a project that cannot be
resolved REFUSES at the door but fails OPEN in a reader.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.rust_binary import find_dev_binary

pytestmark = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents)`",
)


def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
    monkeypatch.setenv("FNO_HOME", str(tmp_path / "state"))
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    index = tmp_path / "state" / "decisions.jsonl"
    index.parent.mkdir(exist_ok=True)
    index.touch()
    import fno.decide

    monkeypatch.setattr(fno.decide, "_decisions_index_path", lambda: index)
    return index


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


def _run(args: list[str]):
    import typer

    from fno.law import law_app

    parent = typer.Typer()
    parent.add_typer(law_app, name="law")
    return CliRunner().invoke(parent, ["law", *args])


def test_the_shim_forwards_global_so_the_door_can_widen(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Widening is the crate door's flag; the shim carries it verbatim."""
    index = _isolate(tmp_path, monkeypatch)
    proj = _work_map(tmp_path, monkeypatch)
    monkeypatch.chdir(proj)
    from types import SimpleNamespace

    seen: dict = {}

    import fno.rust_binary

    monkeypatch.setattr(fno.rust_binary, "resolve_binary", lambda: Path("/stub/fno-agents"))
    monkeypatch.setattr(
        "subprocess.run",
        lambda args, **k: seen.update(args=args, **k) or SimpleNamespace(returncode=0),
    )

    result = _run(
        [
            "set",
            "scope",
            "Every project answers only what needs the user.",
            "--rationale",
            "General.",
            "--global",
        ]
    )

    assert result.exit_code == 0, result.output
    import json

    request = json.loads(seen["input"])
    assert request["mode"] == "record"
    assert "--global" in request["argv"]
    assert index.exists()


def test_scope_split_keeps_global_and_the_matching_project(tmp_path, monkeypatch):
    _work_map(tmp_path, monkeypatch)
    monkeypatch.chdir(tmp_path / "proj")
    from fno.rust_binary import verb_call

    rows = [
        {"decision_id": "d-1", "lane": "law", "scope": "global"},
        {"decision_id": "d-2", "lane": "law", "scope": "project:demo"},
        {"decision_id": "d-3", "lane": "coord", "scope": "project:etl"},
        {"decision_id": "d-4", "lane": "coord"},
        {"decision_id": "d-5", "lane": "law", "scope": "project:etl"},
    ]
    answer = verb_call("law-match", {"mode": "scope-split", "rows": rows})
    kept = [r["decision_id"] for r in answer["kept"]]
    assert kept == ["d-1", "d-2", "d-3", "d-4"]
    assert answer["hidden"] == 1
    assert "hid 1 out-of-scope" in answer["note"]


def test_scope_split_absent_scope_reads_project_fno(tmp_path, monkeypatch):
    _work_map(tmp_path, monkeypatch, slug="other")
    monkeypatch.chdir(tmp_path / "proj")
    from fno.rust_binary import verb_call

    rows = [
        {"decision_id": "d-1", "lane": "law"},
        {"decision_id": "d-2", "lane": "law", "scope": "project:fno"},
    ]
    answer = verb_call("law-match", {"mode": "scope-split", "rows": rows})
    kept = [r["decision_id"] for r in answer["kept"]]
    assert kept == []
    assert answer["hidden"] == 2


def test_scope_split_fails_open_when_the_project_cannot_resolve(tmp_path, monkeypatch):
    _work_map(tmp_path, monkeypatch)
    nowhere = tmp_path / "nowhere"
    nowhere.mkdir()
    monkeypatch.chdir(nowhere)
    from fno.rust_binary import verb_call

    rows = [{"decision_id": "d-1", "lane": "law", "scope": "project:demo"}]
    answer = verb_call("law-match", {"mode": "scope-split", "rows": rows})
    assert [r["decision_id"] for r in answer["kept"]] == ["d-1"]
    assert answer["note"] == ""


def test_list_decisions_applies_the_filter_and_the_note(tmp_path, monkeypatch):
    captured = {}

    def fake_verb(verb, params):
        captured.update(params)
        kept = [r for r in params["rows"] if r.get("scope") != "project:etl"]
        return {"kept": kept, "hidden": 1, "note": " (hid 1 out-of-scope)"}

    import fno.rust_binary

    monkeypatch.setattr(fno.rust_binary, "verb_call", fake_verb)
    from fno.decide import list_decisions

    label, out, damaged = list_decisions(scope="current")
    assert captured["mode"] == "scope-split"
    assert label.endswith("(hid 1 out-of-scope)")


def test_list_decisions_fails_open_when_the_crate_is_unavailable(tmp_path, monkeypatch):
    import fno.rust_binary

    def boom(verb, params):
        raise RuntimeError("crate down")

    monkeypatch.setattr(fno.rust_binary, "verb_call", boom)
    from fno.decide import list_decisions

    label, out, damaged = list_decisions(scope="current")
    assert "scope filter unavailable; nothing hidden" in label
